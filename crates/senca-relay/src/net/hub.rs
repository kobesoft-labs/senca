//! 購読の集約を tokio で動かす（[`crate::agg::Aggregator`] ＋ 上流の張り直し）。
//!
//! - [`RelayHub::subscribe`] で、このノードの購読者が remote stream を受ける。同じ stream の購読者が
//!   何人いても上流は 1 つ。最初の購読者で開き、最後の購読者が去って linger 後に閉じる。
//! - 上流はストリームごとのタスク（pump）が持つ。所有するノードは統合側の [`Resolver`]（制御状態の
//!   割り当て。ADR-0013）で調べる。
//! - 上流を失ったら（接続が切れた・断られた）、Resolver で調べ直して購読し直す。購読者には不連続を配る
//!   （続けて新しい上流の設定と GOP が届く）。GoAway のときは、古い上流を流したまま新しい上流を張り、
//!   張れたら切り替える（make-before-break）。
//! - 購読者ごとのキューは有界。遅い購読者は、映像を次のキーフレームまで捨てる（他を待たせない）。

use super::{RelayEndpoint, RelayError, Upstream};
use crate::NodeId;
use crate::agg::{AggAction, AggStats, Aggregator, AggregatorConfig, LocalId};
use crate::event::{StreamEvent, TrackSel};
use senca_core::MediaKind;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};

/// stream を今所有しているノード（無ければ None。少し後に聞き直す）。
pub type Resolver = Arc<dyn Fn(&str) -> Option<NodeId> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct RelayHubConfig {
    pub agg: AggregatorConfig,
    /// 購読者ごとのキューの長さ。
    pub subscriber_queue: usize,
    /// 上流の購読の優先度（DRR の重み）。
    pub priority: u8,
    /// 購読し直すまでの間隔。
    pub retry_backoff: Duration,
    /// これだけ上流が無ければ、購読者に End を配る（購読し直しは続ける）。
    pub give_up_after: Duration,
}

impl Default for RelayHubConfig {
    fn default() -> Self {
        RelayHubConfig {
            agg: AggregatorConfig::default(),
            subscriber_queue: 2048,
            priority: 128,
            retry_backoff: Duration::from_millis(250),
            give_up_after: Duration::from_secs(10),
        }
    }
}

struct SubTx {
    tx: mpsc::Sender<StreamEvent>,
    waiting_key: bool,
}

enum PumpCmd {
    SetTracks(TrackSel),
}

struct State {
    agg: Aggregator,
    subs: HashMap<LocalId, SubTx>,
    pumps: HashMap<String, mpsc::UnboundedSender<PumpCmd>>,
    ids: Vec<LocalId>,
}

struct HubInner {
    ep: RelayEndpoint,
    resolver: Resolver,
    cfg: RelayHubConfig,
    st: Mutex<State>,
    timer: Notify,
}

/// 購読の集約。
#[derive(Clone)]
pub struct RelayHub {
    inner: Arc<HubInner>,
}

impl fmt::Debug for RelayHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayHub")
            .field("stats", &self.stats())
            .finish()
    }
}

impl RelayHub {
    /// 作る（linger のタイマーのタスクを動かす。tokio の実行環境の中で呼ぶ）。
    pub fn new(ep: RelayEndpoint, resolver: Resolver, cfg: RelayHubConfig) -> RelayHub {
        let inner = Arc::new(HubInner {
            ep,
            resolver,
            st: Mutex::new(State {
                agg: Aggregator::new(cfg.agg.clone()),
                subs: HashMap::new(),
                pumps: HashMap::new(),
                ids: Vec::new(),
            }),
            cfg,
            timer: Notify::new(),
        });
        tokio::spawn(timer_loop(Arc::downgrade(&inner)));
        RelayHub { inner }
    }

    pub fn endpoint(&self) -> &RelayEndpoint {
        &self.inner.ep
    }

    pub fn stats(&self) -> AggStats {
        self.inner.st.lock().unwrap().agg.stats()
    }

    /// remote stream を購読する。先に [`RelaySubscription::initial`]（設定と直近の GOP）を使う。
    pub fn subscribe(&self, stream_id: &str, tracks: TrackSel) -> RelaySubscription {
        let (tx, rx) = mpsc::channel(self.inner.cfg.subscriber_queue.max(1));
        let (id, initial) = {
            let mut st = self.inner.st.lock().unwrap();
            let (id, initial) = st.agg.add_subscriber(stream_id, tracks, Instant::now());
            st.subs.insert(
                id,
                SubTx {
                    tx,
                    waiting_key: false,
                },
            );
            apply_actions(&self.inner, &mut st);
            (id, initial)
        };
        self.inner.timer.notify_one();
        RelaySubscription {
            id,
            initial,
            rx,
            hub: Arc::downgrade(&self.inner),
        }
    }
}

/// このノードの中の購読者。drop で外れる。
pub struct RelaySubscription {
    id: LocalId,
    initial: Vec<StreamEvent>,
    rx: mpsc::Receiver<StreamEvent>,
    hub: Weak<HubInner>,
}

impl fmt::Debug for RelaySubscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelaySubscription")
            .field("id", &self.id)
            .finish()
    }
}

impl RelaySubscription {
    pub fn id(&self) -> LocalId {
        self.id
    }

    /// 先に渡す物（設定と直近の GOP）。
    pub fn initial(&mut self) -> Vec<StreamEvent> {
        std::mem::take(&mut self.initial)
    }

    /// 次の出来事。hub が無くなったら None。
    pub async fn recv(&mut self) -> Option<StreamEvent> {
        self.rx.recv().await
    }

    pub fn try_recv(&mut self) -> Option<StreamEvent> {
        self.rx.try_recv().ok()
    }

    /// 受けるトラック（層）を変える。キャッシュにある分はすぐ受け取れる（返り値）。
    pub fn set_tracks(&mut self, tracks: TrackSel) -> Vec<StreamEvent> {
        let Some(h) = self.hub.upgrade() else {
            return Vec::new();
        };
        let mut st = h.st.lock().unwrap();
        let init = st.agg.set_subscriber_tracks(self.id, tracks);
        apply_actions(&h, &mut st);
        init
    }
}

impl Drop for RelaySubscription {
    fn drop(&mut self) {
        if let Some(h) = self.hub.upgrade() {
            let mut st = h.st.lock().unwrap();
            st.subs.remove(&self.id);
            st.agg.remove_subscriber(self.id, Instant::now());
            apply_actions(&h, &mut st);
            drop(st);
            h.timer.notify_one();
        }
    }
}

fn apply_actions(inner: &Arc<HubInner>, st: &mut State) {
    while let Some(a) = st.agg.poll_action() {
        match a {
            AggAction::Open { stream_id, tracks } => {
                let (tx, rx) = mpsc::unbounded_channel();
                st.pumps.insert(stream_id.clone(), tx);
                tokio::spawn(pump(Arc::downgrade(inner), stream_id, tracks, rx));
            }
            AggAction::Update { stream_id, tracks } => {
                if let Some(p) = st.pumps.get(&stream_id) {
                    let _ = p.send(PumpCmd::SetTracks(tracks));
                }
            }
            AggAction::Close { stream_id } => {
                // 送り口を落とせば pump は上流を drop して終わる
                st.pumps.remove(&stream_id);
            }
        }
    }
}

/// 購読者へ配る。
fn deliver(inner: &HubInner, stream_id: &str, ev: StreamEvent) {
    let mut st = inner.st.lock().unwrap();
    let st = &mut *st;
    st.agg.on_upstream(stream_id, &ev, &mut st.ids);
    for id in &st.ids {
        let Some(s) = st.subs.get_mut(id) else {
            continue;
        };
        if let StreamEvent::Au(au) = &ev
            && au.codec.kind() == MediaKind::Video
            && s.waiting_key
        {
            if !au.keyframe {
                continue;
            }
            s.waiting_key = false;
        }
        if let Err(mpsc::error::TrySendError::Full(e)) = s.tx.try_send(ev.clone()) {
            // 遅い購読者: 映像は次のキーフレームまで捨てる
            if matches!(e, StreamEvent::Au(_)) {
                s.waiting_key = true;
            }
        }
    }
}

fn reset(inner: &HubInner, stream_id: &str) {
    let mut st = inner.st.lock().unwrap();
    let st = &mut *st;
    st.agg.upstream_reset(stream_id, &mut st.ids);
    for id in &st.ids {
        if let Some(s) = st.subs.get_mut(id) {
            let _ = s.tx.try_send(StreamEvent::Discontinuity);
            s.waiting_key = true;
        }
    }
}

fn end_all(inner: &HubInner, stream_id: &str) {
    let mut st = inner.st.lock().unwrap();
    let st = &mut *st;
    st.agg
        .on_upstream(stream_id, &StreamEvent::End, &mut st.ids);
    for id in &st.ids {
        if let Some(s) = st.subs.get(id) {
            let _ = s.tx.try_send(StreamEvent::End);
        }
    }
}

async fn open(
    inner: &HubInner,
    stream_id: &str,
    tracks: &TrackSel,
) -> Result<Upstream, RelayError> {
    let Some(peer) = (inner.resolver)(stream_id) else {
        return Err(RelayError::NotConnected(0, "owner unknown".into()));
    };
    inner
        .ep
        .subscribe(peer, stream_id, tracks.clone(), inner.cfg.priority)
        .await
}

async fn recv_opt(up: &mut Option<Upstream>) -> Option<StreamEvent> {
    match up {
        Some(u) => u.recv().await,
        None => std::future::pending().await,
    }
}

/// stream ごとの上流（張る・流す・失ったら張り直す）。
async fn pump(
    hub: Weak<HubInner>,
    stream_id: String,
    mut tracks: TrackSel,
    mut cmd: mpsc::UnboundedReceiver<PumpCmd>,
) {
    let mut up: Option<Upstream> = None;
    // GoAway を受けた（流れは続く。張り直す）
    let mut moving = false;
    // 張り替えたら購読者に不連続を配る（失ったときは、その時に配った）
    let mut pending_reset = false;
    let mut down_since: Option<Instant> = Some(Instant::now());
    let mut ended = false;
    let mut next_try = Instant::now();
    loop {
        let Some(inner) = hub.upgrade() else { return };
        let now = Instant::now();
        if (up.is_none() || moving) && now >= next_try {
            match open(&inner, &stream_id, &tracks).await {
                Ok(u) => {
                    if std::mem::take(&mut pending_reset) {
                        reset(&inner, &stream_id);
                    }
                    up = Some(u); // 古い上流（GoAway のもの）はここで drop される
                    moving = false;
                    down_since = None;
                    ended = false;
                }
                Err(_) => {
                    next_try = Instant::now() + inner.cfg.retry_backoff;
                    if let Some(s) = down_since
                        && !ended
                        && s.elapsed() >= inner.cfg.give_up_after
                    {
                        end_all(&inner, &stream_id);
                        ended = true;
                    }
                }
            }
            continue;
        }
        let waiting = up.is_none() || moving;
        drop(inner);
        tokio::select! {
            c = cmd.recv() => match c {
                None => return,
                Some(PumpCmd::SetTracks(t)) => {
                    if let Some(u) = &up {
                        u.set_tracks(t.clone());
                    }
                    tracks = t;
                }
            },
            ev = recv_opt(&mut up) => {
                let Some(inner) = hub.upgrade() else { return };
                match ev {
                    Some(StreamEvent::Lost(r)) if r.is_graceful() => {
                        moving = true;
                        pending_reset = true;
                        next_try = Instant::now();
                    }
                    Some(StreamEvent::Lost(_)) | None => {
                        up = None;
                        moving = false;
                        pending_reset = false;
                        down_since = Some(Instant::now());
                        next_try = Instant::now();
                        reset(&inner, &stream_id);
                    }
                    Some(ev) => deliver(&inner, &stream_id, ev),
                }
            }
            _ = tokio::time::sleep_until(next_try.into()), if waiting => {}
        }
    }
}

async fn timer_loop(hub: Weak<HubInner>) {
    loop {
        let (deadline, notified) = {
            let Some(inner) = hub.upgrade() else { return };
            let d = inner.st.lock().unwrap().agg.poll_timeout();
            // 起こされたか、期限か（hub が無くなったら 1 秒で気づく）
            let w = Arc::clone(&inner);
            drop(inner);
            (d, w)
        };
        let n = notified.timer.notified();
        let until = deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(1));
        tokio::select! {
            _ = n => {}
            _ = tokio::time::sleep_until(until.into()) => {}
        }
        let mut st = notified.st.lock().unwrap();
        st.agg.handle_timeout(Instant::now());
        apply_actions(&notified, &mut st);
        drop(st);
        drop(notified);
    }
}
