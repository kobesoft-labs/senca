//! tokio のアダプター（機能 `tokio`）。
//!
//! - [`RelayConnection`]: 任意の `AsyncRead + AsyncWrite`（ノードは mTLS のストリームを渡す）の上で
//!   [`Connection`] を動かす。読みのタスクは受信バッファを切り出して枠にし（コピーしない）、書きのタスクは
//!   見出しと payload の断片をまとめ書き（`write_vectored`）する。まとめ書きのできないストリーム（tls_io の
//!   `DuplexStream` など）では、断片を 1 つのバッファにつないでから書く。
//! - [`RelayEndpoint`]: 相手のノードごとに 1 本の接続を管理する（両方から同時につないだら、番号の小さい
//!   ノードがつないだ方を残す）。接続が無ければ、統合側が渡した [`Dialer`] でつなぐ。
//! - [`Upstream`]: 自分が購読した物。[`StreamEvent`] を受ける。drop で購読をやめる。
//! - [`IncomingSubscribe`] → [`Downstream`]: 相手に購読された物。hub の出来事を [`Downstream::send`] で積む
//!   （待たない。詰まれば送りの順番の中で優先度の低い物から捨てる）。
//! - [`RelayHub`]: 購読の集約 [`crate::agg`] を動かし、上流を失ったら別のノードから購読し直す。

mod hub;

pub use hub::{RelayHub, RelayHubConfig, RelaySubscription, Resolver};

use crate::NodeId;
use crate::conn::{
    ConnConfig, ConnError, ConnEvent, ConnStats, Connection, DownstreamId, PeerInfo, UpstreamId,
};
use crate::event::{LostReason, StreamEvent, TrackSel};
use crate::sched::ChannelStats;
use crate::wire::ErrorCode;
use bytes::{Bytes, BytesMut};
use senca_core::{AccessUnit, MediaKind, TrackId};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::io::IoSlice;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc, oneshot, watch};

/// 中継の接続に使えるストリーム。
pub trait RelayStream: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> RelayStream for T {}

/// 相手のノードへつなぐ関数（TCP をつなぎ、mTLS を済ませたストリームを返す）。
pub type Dialer = Arc<
    dyn Fn(NodeId) -> Pin<Box<dyn Future<Output = Result<Box<dyn RelayStream>, String>> + Send>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayError {
    /// 相手への接続が無い（Dialer も無い・つなげなかった）。
    NotConnected(NodeId, String),
    /// 購読が断られた・接続が切れた。
    Lost(LostReason),
    Timeout,
    Handshake(String),
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RelayError::NotConnected(n, m) => write!(f, "no relay connection to node {n}: {m}"),
            RelayError::Lost(r) => write!(f, "subscription lost: {r:?}"),
            RelayError::Timeout => f.write_str("timed out"),
            RelayError::Handshake(m) => write!(f, "relay handshake failed: {m}"),
        }
    }
}

impl std::error::Error for RelayError {}

#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub conn: ConnConfig,
    /// 上流 1 つで受けて、まだ読まれていない AU の上限（超えたら映像は次のキーフレームまで捨て、キーフレームを頼む）。
    pub upstream_queue: usize,
    pub subscribe_timeout: Duration,
    pub handshake_timeout: Duration,
    /// まだ答えていない、相手からの購読の上限。
    pub incoming_queue: usize,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        EndpointConfig {
            conn: ConnConfig::default(),
            upstream_queue: 4096,
            subscribe_timeout: Duration::from_secs(5),
            handshake_timeout: Duration::from_secs(10),
            incoming_queue: 1024,
        }
    }
}

/// 下流（相手に購読された物）への知らせ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownEvent {
    /// キーフレームが欲しい（送り手へ伝える）。
    KeyframeRequest(TrackId),
    /// 相手の選ぶトラック（層）が変わった。
    TracksChanged(TrackSel),
}

struct UpSlot {
    tx: mpsc::UnboundedSender<StreamEvent>,
    queued: Arc<AtomicUsize>,
    waiting_key: Vec<TrackId>,
    gap: Vec<TrackId>,
}

struct ConnShared {
    conn: Mutex<Connection>,
    wake: Notify,
    drained: Notify,
    peer: OnceLock<PeerInfo>,
    hello: Mutex<Option<oneshot::Sender<Result<PeerInfo, ConnError>>>>,
    ups: Mutex<HashMap<UpstreamId, UpSlot>>,
    pending: Mutex<HashMap<UpstreamId, oneshot::Sender<Result<(), LostReason>>>>,
    downs: Mutex<HashMap<DownstreamId, mpsc::UnboundedSender<DownEvent>>>,
    closed: watch::Sender<bool>,
    incoming: Option<mpsc::Sender<IncomingSubscribe>>,
    dialed: bool,
    upstream_queue: usize,
}

/// 1 本の接続（clone は同じ接続を指す）。
#[derive(Clone)]
pub struct RelayConnection {
    sh: Arc<ConnShared>,
}

impl fmt::Debug for RelayConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayConnection")
            .field("peer", &self.peer_id())
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// [`RelayConnection::establish`] の設定。
pub struct ConnOptions {
    pub conn: ConnConfig,
    pub upstream_queue: usize,
    pub handshake_timeout: Duration,
    /// 相手からの購読を渡す先（無ければ全部断る）。
    pub incoming: Option<mpsc::Sender<IncomingSubscribe>>,
    /// 自分からつないだか（重複した接続のどちらを残すかに使う）。
    pub dialed: bool,
}

impl RelayConnection {
    /// ストリームの上で接続を始め、相手の Hello を待つ。
    pub async fn establish<S: RelayStream>(
        stream: S,
        opts: ConnOptions,
    ) -> Result<RelayConnection, RelayError> {
        let (tx, rx) = oneshot::channel();
        let (closed, _) = watch::channel(false);
        let sh = Arc::new(ConnShared {
            conn: Mutex::new(Connection::new(opts.conn, Instant::now())),
            wake: Notify::new(),
            drained: Notify::new(),
            peer: OnceLock::new(),
            hello: Mutex::new(Some(tx)),
            ups: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            downs: Mutex::new(HashMap::new()),
            closed,
            incoming: opts.incoming,
            dialed: opts.dialed,
            upstream_queue: opts.upstream_queue,
        });
        let (rd, wr) = tokio::io::split(stream);
        tokio::spawn(read_loop(sh.clone(), rd));
        tokio::spawn(write_loop(sh.clone(), wr));
        let c = RelayConnection { sh };
        match tokio::time::timeout(opts.handshake_timeout, rx).await {
            Ok(Ok(Ok(_))) => Ok(c),
            Ok(Ok(Err(e))) => Err(RelayError::Handshake(e.to_string())),
            Ok(Err(_)) => Err(RelayError::Handshake("connection closed".into())),
            Err(_) => {
                c.close("handshake timeout");
                Err(RelayError::Timeout)
            }
        }
    }

    pub fn peer(&self) -> Option<PeerInfo> {
        self.sh.peer.get().cloned()
    }

    /// 同じ接続を指しているか。
    pub fn same_as(&self, other: &RelayConnection) -> bool {
        Arc::ptr_eq(&self.sh, &other.sh)
    }

    pub fn peer_id(&self) -> Option<NodeId> {
        self.sh.peer.get().map(|p| p.node_id)
    }

    pub fn stats(&self) -> ConnStats {
        self.sh.conn.lock().unwrap().stats()
    }

    pub fn is_closed(&self) -> bool {
        *self.sh.closed.borrow()
    }

    pub fn peer_going_away(&self) -> bool {
        self.sh.conn.lock().unwrap().peer_going_away()
    }

    /// 閉じるまで待つ。
    pub async fn closed(&self) {
        let mut rx = self.sh.closed.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }

    fn with<R>(&self, f: impl FnOnce(&mut Connection) -> R) -> R {
        let (r, evs) = {
            let mut c = self.sh.conn.lock().unwrap();
            let r = f(&mut c);
            let mut evs = Vec::new();
            while let Some(e) = c.poll_event() {
                evs.push(e);
            }
            (r, evs)
        };
        if !evs.is_empty() {
            dispatch(&self.sh, evs);
        }
        self.sh.wake.notify_one();
        r
    }

    /// 相手の stream を購読し、受け付けられるまで待つ。
    pub async fn subscribe(
        &self,
        stream_id: &str,
        tracks: TrackSel,
        priority: u8,
        timeout: Duration,
    ) -> Result<Upstream, RelayError> {
        let (tx, rx) = oneshot::channel();
        let (etx, erx) = mpsc::unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let id = {
            let mut c = self.sh.conn.lock().unwrap();
            let id = c
                .subscribe(stream_id, tracks, priority)
                .map_err(RelayError::Lost)?;
            // 読みのタスクが SubscribeOk を見るより先に登録する（conn の鍵の中で）
            self.sh.ups.lock().unwrap().insert(
                id,
                UpSlot {
                    tx: etx,
                    queued: queued.clone(),
                    waiting_key: Vec::new(),
                    gap: Vec::new(),
                },
            );
            self.sh.pending.lock().unwrap().insert(id, tx);
            id
        };
        self.sh.wake.notify_one();
        let up = Upstream {
            conn: self.clone(),
            id,
            rx: erx,
            queued,
            ended: false,
        };
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(()))) => Ok(up),
            Ok(Ok(Err(r))) => {
                let mut up = up;
                up.ended = true;
                Err(RelayError::Lost(r))
            }
            Ok(Err(_)) => Err(RelayError::Lost(LostReason::ConnectionLost(
                "connection closed".into(),
            ))),
            Err(_) => {
                self.sh.pending.lock().unwrap().remove(&id);
                Err(RelayError::Timeout) // up の drop で購読をやめる
            }
        }
    }

    /// 退く（新しい購読を断る。今の購読は続く）。
    pub fn go_away(&self, code: ErrorCode, reason: &str) {
        self.with(|c| c.go_away(code, reason));
    }

    pub fn close(&self, reason: &str) {
        self.with(|c| c.close(reason));
    }

    /// この接続の全体の帯域の上限。
    pub fn set_total_rate(&self, rate_bps: Option<u64>) {
        self.with(|c| c.set_total_rate(rate_bps, Instant::now()));
    }

    /// 送りのキューが `below` バイト以下になるまで待つ（ライブでない一括の送り向けの背圧）。
    pub async fn wait_drained(&self, below: usize) {
        loop {
            let n = self.sh.drained.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if self.is_closed() || self.sh.conn.lock().unwrap().scheduler().queued_bytes() <= below
            {
                return;
            }
            n.await;
        }
    }
}

fn dispatch(sh: &Arc<ConnShared>, evs: Vec<ConnEvent>) {
    let mut kf: Vec<(UpstreamId, TrackId)> = Vec::new();
    for ev in evs {
        match ev {
            ConnEvent::Connected(p) => {
                let _ = sh.peer.set(p.clone());
                if let Some(tx) = sh.hello.lock().unwrap().take() {
                    let _ = tx.send(Ok(p));
                }
            }
            ConnEvent::SubscribeOk(id) => {
                if let Some(tx) = sh.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(Ok(()));
                }
            }
            ConnEvent::Stream(id, StreamEvent::Lost(r)) => {
                if let Some(tx) = sh.pending.lock().unwrap().remove(&id) {
                    if !r.is_graceful() {
                        sh.ups.lock().unwrap().remove(&id);
                        let _ = tx.send(Err(r));
                        continue;
                    }
                    let _ = tx.send(Ok(()));
                }
                let mut ups = sh.ups.lock().unwrap();
                if r.is_graceful() {
                    if let Some(s) = ups.get(&id) {
                        let _ = s.tx.send(StreamEvent::Lost(r));
                    }
                } else if let Some(s) = ups.remove(&id) {
                    let _ = s.tx.send(StreamEvent::Lost(r));
                }
            }
            ConnEvent::Stream(id, StreamEvent::Au(mut au)) => {
                let mut ups = sh.ups.lock().unwrap();
                let Some(s) = ups.get_mut(&id) else { continue };
                let video = au.codec.kind() == MediaKind::Video;
                if video && let Some(i) = s.waiting_key.iter().position(|t| *t == au.track) {
                    if !au.keyframe {
                        continue;
                    }
                    s.waiting_key.swap_remove(i);
                    au.discontinuity = true;
                }
                if s.queued.load(Ordering::Relaxed) >= sh.upstream_queue {
                    // 受け手が遅い: 映像は次のキーフレームまで捨てる
                    if video {
                        if !s.waiting_key.contains(&au.track) {
                            s.waiting_key.push(au.track);
                            kf.push((id, au.track));
                        }
                    } else if !s.gap.contains(&au.track) {
                        s.gap.push(au.track);
                    }
                    continue;
                }
                if let Some(i) = s.gap.iter().position(|t| *t == au.track) {
                    s.gap.swap_remove(i);
                    au.discontinuity = true;
                }
                s.queued.fetch_add(1, Ordering::Relaxed);
                let _ = s.tx.send(StreamEvent::Au(au));
            }
            ConnEvent::Stream(id, ev) => {
                if let Some(s) = sh.ups.lock().unwrap().get(&id) {
                    let _ = s.tx.send(ev);
                }
            }
            ConnEvent::SubscribeRequest {
                id,
                stream_id,
                tracks,
                priority,
            } => {
                let (tx, rx) = mpsc::unbounded_channel();
                sh.downs.lock().unwrap().insert(id, tx);
                let inc = IncomingSubscribe {
                    peer: sh.peer.get().map_or(0, |p| p.node_id),
                    stream_id,
                    tracks,
                    priority,
                    id,
                    conn: RelayConnection { sh: sh.clone() },
                    events: Some(rx),
                };
                let refused = match &sh.incoming {
                    Some(q) => q.try_send(inc).err().map(|e| match e {
                        mpsc::error::TrySendError::Full(i)
                        | mpsc::error::TrySendError::Closed(i) => i,
                    }),
                    None => Some(inc),
                };
                if let Some(mut inc) = refused {
                    inc.answer(Err((ErrorCode::OVERLOADED, "not accepting subscriptions")));
                }
            }
            ConnEvent::Unsubscribed(id) => {
                sh.downs.lock().unwrap().remove(&id);
            }
            ConnEvent::TracksChanged { id, tracks, .. } => {
                if let Some(tx) = sh.downs.lock().unwrap().get(&id) {
                    let _ = tx.send(DownEvent::TracksChanged(tracks));
                }
            }
            ConnEvent::KeyframeRequest { id, track } => {
                if let Some(tx) = sh.downs.lock().unwrap().get(&id) {
                    let _ = tx.send(DownEvent::KeyframeRequest(track));
                }
            }
            ConnEvent::GoAway { .. } => {}
            ConnEvent::Closed(e) => {
                if let Some(tx) = sh.hello.lock().unwrap().take() {
                    let _ = tx.send(Err(e.clone()));
                }
                for (_, tx) in sh.pending.lock().unwrap().drain() {
                    let _ = tx.send(Err(LostReason::ConnectionLost(e.to_string())));
                }
                sh.ups.lock().unwrap().clear();
                sh.downs.lock().unwrap().clear();
                sh.closed.send_replace(true);
                sh.wake.notify_one();
            }
        }
    }
    if !kf.is_empty() {
        let mut c = sh.conn.lock().unwrap();
        for (id, t) in kf {
            c.request_keyframe(id, t);
        }
        drop(c);
        sh.wake.notify_one();
    }
}

fn drain_events(c: &mut Connection) -> Vec<ConnEvent> {
    let mut v = Vec::new();
    while let Some(e) = c.poll_event() {
        v.push(e);
    }
    v
}

async fn read_loop<R: AsyncRead + Unpin>(sh: Arc<ConnShared>, mut rd: R) {
    let mut buf = BytesMut::with_capacity(256 << 10);
    let mut closed = sh.closed.subscribe();
    loop {
        let hint = sh.conn.lock().unwrap().read_hint();
        let want = hint.max(64 << 10);
        if buf.capacity() - buf.len() < want {
            buf.reserve(want.max(256 << 10));
        }
        let r = tokio::select! {
            r = rd.read_buf(&mut buf) => r,
            _ = closed.wait_for(|c| *c) => break,
        };
        let evs = {
            let mut c = sh.conn.lock().unwrap();
            match r {
                Ok(0) => c.transport_closed("peer closed the connection"),
                Err(e) => c.transport_closed(&e.to_string()),
                Ok(_) => c.handle_input(&mut buf, Instant::now()),
            }
            drain_events(&mut c)
        };
        dispatch(&sh, evs);
        sh.wake.notify_one();
        if *sh.closed.borrow() {
            break;
        }
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(sh: Arc<ConnShared>, mut wr: W) {
    let mut chunks: Vec<Bytes> = Vec::new();
    let vectored = wr.is_write_vectored();
    let mut joined = BytesMut::new();
    loop {
        let now = Instant::now();
        let (deadline, finished, evs) = {
            let mut c = sh.conn.lock().unwrap();
            c.handle_timeout(now);
            c.poll_transmit(now, &mut chunks);
            (c.poll_timeout(now), c.is_finished(), drain_events(&mut c))
        };
        if !evs.is_empty() {
            dispatch(&sh, evs);
        }
        if !chunks.is_empty() {
            if let Err(e) = write_chunks(&mut wr, &mut chunks, vectored, &mut joined).await {
                let evs = {
                    let mut c = sh.conn.lock().unwrap();
                    c.transport_closed(&e.to_string());
                    drain_events(&mut c)
                };
                dispatch(&sh, evs);
                break;
            }
            sh.drained.notify_waiters();
            continue;
        }
        if finished {
            let _ = wr.shutdown().await;
            break;
        }
        sh.drained.notify_waiters();
        let sleep = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d.into()).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = sh.wake.notified() => {}
            _ = sleep => {}
        }
    }
    sh.closed.send_replace(true);
    sh.drained.notify_waiters();
}

/// 断片を全部書く。まとめ書きのできるストリームでは 1 回に最大 64 断片。
async fn write_chunks<W: AsyncWrite + Unpin>(
    wr: &mut W,
    chunks: &mut Vec<Bytes>,
    vectored: bool,
    joined: &mut BytesMut,
) -> std::io::Result<()> {
    if !vectored {
        joined.clear();
        for c in chunks.drain(..) {
            joined.extend_from_slice(&c);
        }
        wr.write_all(joined).await?;
        return wr.flush().await;
    }
    let mut i = 0;
    let mut off = 0;
    while i < chunks.len() {
        let mut slices: Vec<IoSlice<'_>> = Vec::with_capacity(64);
        slices.push(IoSlice::new(&chunks[i][off..]));
        slices.extend(chunks[i + 1..].iter().take(63).map(|c| IoSlice::new(c)));
        let mut n = wr.write_vectored(&slices).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        while n > 0 {
            let rem = chunks[i].len() - off;
            if n >= rem {
                n -= rem;
                i += 1;
                off = 0;
            } else {
                off += n;
                n = 0;
            }
        }
    }
    chunks.clear();
    wr.flush().await
}

// ---------------------------------------------------------------------------------------------

/// 自分が購読した物。drop で購読をやめる。
pub struct Upstream {
    conn: RelayConnection,
    id: UpstreamId,
    rx: mpsc::UnboundedReceiver<StreamEvent>,
    queued: Arc<AtomicUsize>,
    ended: bool,
}

impl fmt::Debug for Upstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upstream")
            .field("peer", &self.conn.peer_id())
            .field("id", &self.id)
            .finish()
    }
}

impl Upstream {
    /// 次の出来事。`Lost`（GoAway 以外）の後は None。
    pub async fn recv(&mut self) -> Option<StreamEvent> {
        let ev = self.rx.recv().await;
        self.account(ev)
    }

    pub fn try_recv(&mut self) -> Option<StreamEvent> {
        let ev = self.rx.try_recv().ok();
        self.account(ev)
    }

    fn account(&mut self, ev: Option<StreamEvent>) -> Option<StreamEvent> {
        match &ev {
            Some(StreamEvent::Au(_)) => {
                self.queued.fetch_sub(1, Ordering::Relaxed);
            }
            Some(StreamEvent::Lost(r)) if !r.is_graceful() => self.ended = true,
            None => self.ended = true,
            _ => {}
        }
        ev
    }

    pub fn id(&self) -> UpstreamId {
        self.id
    }

    pub fn peer(&self) -> NodeId {
        self.conn.peer_id().unwrap_or(0)
    }

    pub fn connection(&self) -> &RelayConnection {
        &self.conn
    }

    /// 受けるトラック（層）を変える。
    pub fn set_tracks(&self, tracks: TrackSel) {
        self.conn.with(|c| c.set_tracks(self.id, tracks));
    }

    pub fn request_keyframe(&self, track: TrackId) {
        self.conn.with(|c| c.request_keyframe(self.id, track));
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.conn.sh.ups.lock().unwrap().remove(&self.id);
        if !self.ended {
            let id = self.id;
            self.conn.with(|c| c.unsubscribe(id));
        }
    }
}

/// 相手からの購読。[`accept`](Self::accept) か [`reject`](Self::reject) で答える（drop は断る）。
pub struct IncomingSubscribe {
    pub peer: NodeId,
    pub stream_id: String,
    pub tracks: TrackSel,
    pub priority: u8,
    id: DownstreamId,
    conn: RelayConnection,
    events: Option<mpsc::UnboundedReceiver<DownEvent>>,
}

impl fmt::Debug for IncomingSubscribe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IncomingSubscribe")
            .field("peer", &self.peer)
            .field("stream_id", &self.stream_id)
            .field("tracks", &self.tracks)
            .finish()
    }
}

impl IncomingSubscribe {
    fn answer(&mut self, r: Result<Option<u64>, (ErrorCode, &str)>) {
        let id = self.id;
        match r {
            Ok(rate) => self.conn.with(|c| c.accept(id, rate, Instant::now())),
            Err((code, reason)) => {
                self.conn.sh.downs.lock().unwrap().remove(&id);
                self.conn.with(|c| c.reject(id, code, reason));
            }
        }
        self.events = None;
    }

    /// 受け付ける。`rate_bps` はこのチャネルの帯域の上限。
    pub fn accept(mut self, rate_bps: Option<u64>) -> Downstream {
        let events = self.events.take().expect("not answered");
        let conn = self.conn.clone();
        let id = self.id;
        conn.with(|c| c.accept(id, rate_bps, Instant::now()));
        Downstream {
            conn,
            id,
            events,
            peer: self.peer,
            done: false,
        }
    }

    /// 断る（stream が無い: [`ErrorCode::NOT_FOUND`]、所有権が移った: [`ErrorCode::MOVED`]）。
    pub fn reject(mut self, code: ErrorCode, reason: &str) {
        self.answer(Err((code, reason)));
    }
}

impl Drop for IncomingSubscribe {
    fn drop(&mut self) {
        if self.events.is_some() {
            self.answer(Err((ErrorCode::NOT_FOUND, "not served")));
        }
    }
}

/// 相手に購読された物。hub の出来事を [`send`](Self::send) で積む。drop で購読を終える。
pub struct Downstream {
    conn: RelayConnection,
    id: DownstreamId,
    events: mpsc::UnboundedReceiver<DownEvent>,
    peer: NodeId,
    done: bool,
}

impl fmt::Debug for Downstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Downstream")
            .field("peer", &self.peer)
            .field("id", &self.id)
            .finish()
    }
}

impl Downstream {
    pub fn peer(&self) -> NodeId {
        self.peer
    }

    /// hub の出来事を積む（待たない）。購読が終わっていれば false。
    pub fn send(&self, ev: &StreamEvent) -> bool {
        self.conn.with(|c| c.send(self.id, ev, Instant::now()))
    }

    /// まとめて積む（鍵を 1 回だけ取る）。積んだ数を返す。
    pub fn send_all(&self, evs: &[StreamEvent]) -> usize {
        let now = Instant::now();
        self.conn
            .with(|c| evs.iter().filter(|e| c.send(self.id, e, now)).count())
    }

    pub fn send_au(&self, au: AccessUnit) -> bool {
        self.conn.with(|c| c.send_au(self.id, au, Instant::now()))
    }

    /// 相手からの知らせ。購読が終わったら None。
    pub async fn next_event(&mut self) -> Option<DownEvent> {
        self.events.recv().await
    }

    pub fn is_closed(&self) -> bool {
        !self.conn.sh.downs.lock().unwrap().contains_key(&self.id)
    }

    pub fn tracks(&self) -> TrackSel {
        self.conn
            .sh
            .conn
            .lock()
            .unwrap()
            .downstream_tracks(self.id)
            .cloned()
            .unwrap_or(TrackSel::All)
    }

    pub fn stats(&self) -> Option<ChannelStats> {
        self.conn
            .sh
            .conn
            .lock()
            .unwrap()
            .scheduler()
            .channel_stats(self.id.0)
    }

    pub fn connection(&self) -> &RelayConnection {
        &self.conn
    }

    /// 購読を終える（stream が終わった: NORMAL、所有権が移った: MOVED）。
    pub fn finish(mut self, code: ErrorCode, reason: &str) {
        self.done = true;
        let id = self.id;
        self.conn.sh.downs.lock().unwrap().remove(&id);
        self.conn.with(|c| c.reject(id, code, reason));
    }
}

impl Drop for Downstream {
    fn drop(&mut self) {
        if !self.done {
            let id = self.id;
            self.conn.sh.downs.lock().unwrap().remove(&id);
            self.conn
                .with(|c| c.reject(id, ErrorCode::NORMAL, "publisher closed"));
        }
    }
}

// ---------------------------------------------------------------------------------------------

struct EpInner {
    cfg: EndpointConfig,
    conns: Mutex<HashMap<NodeId, RelayConnection>>,
    incoming: mpsc::Sender<IncomingSubscribe>,
    dialer: Mutex<Option<Dialer>>,
    dialing: Mutex<HashMap<NodeId, Arc<tokio::sync::Mutex<()>>>>,
}

/// 相手のノードごとに 1 本の接続。
#[derive(Clone)]
pub struct RelayEndpoint {
    inner: Arc<EpInner>,
}

impl fmt::Debug for RelayEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayEndpoint")
            .field("node_id", &self.node_id())
            .field("peers", &self.peers())
            .finish()
    }
}

impl RelayEndpoint {
    /// 作る。返り値の受け口に、相手からの購読（[`IncomingSubscribe`]）が届く。
    pub fn new(cfg: EndpointConfig) -> (RelayEndpoint, mpsc::Receiver<IncomingSubscribe>) {
        let (tx, rx) = mpsc::channel(cfg.incoming_queue.max(1));
        let inner = Arc::new(EpInner {
            cfg,
            conns: Mutex::new(HashMap::new()),
            incoming: tx,
            dialer: Mutex::new(None),
            dialing: Mutex::new(HashMap::new()),
        });
        (RelayEndpoint { inner }, rx)
    }

    pub fn node_id(&self) -> NodeId {
        self.inner.cfg.conn.node_id
    }

    pub fn config(&self) -> &EndpointConfig {
        &self.inner.cfg
    }

    /// 接続が無い相手へつなぐ関数を渡す。
    pub fn set_dialer(&self, d: Dialer) {
        *self.inner.dialer.lock().unwrap() = Some(d);
    }

    /// 自分からつないだストリームを足す。
    pub async fn connect<S: RelayStream>(&self, stream: S) -> Result<RelayConnection, RelayError> {
        self.add(stream, true).await
    }

    /// 相手からつながれたストリームを足す。
    pub async fn accept<S: RelayStream>(&self, stream: S) -> Result<RelayConnection, RelayError> {
        self.add(stream, false).await
    }

    async fn add<S: RelayStream>(
        &self,
        stream: S,
        dialed: bool,
    ) -> Result<RelayConnection, RelayError> {
        let cfg = &self.inner.cfg;
        let c = RelayConnection::establish(
            stream,
            ConnOptions {
                conn: cfg.conn.clone(),
                upstream_queue: cfg.upstream_queue,
                handshake_timeout: cfg.handshake_timeout,
                incoming: Some(self.inner.incoming.clone()),
                dialed,
            },
        )
        .await?;
        let peer = c.peer_id().expect("connected");
        let me = self.node_id();
        if peer == me {
            c.close("connected to self");
            return Err(RelayError::Handshake("connected to self".into()));
        }
        let dialer_of = |x: &RelayConnection| if x.sh.dialed { me } else { peer };
        let kept = {
            let mut m = self.inner.conns.lock().unwrap();
            match m.get(&peer) {
                Some(old) if !old.is_closed() && dialer_of(old) < dialer_of(&c) => {
                    // 両方から同時につないだ: 番号の小さいノードがつないだ方を残す
                    c.go_away(ErrorCode::NORMAL, "duplicate connection");
                    old.clone()
                }
                Some(old) => {
                    let old = old.clone();
                    m.insert(peer, c.clone());
                    if !old.is_closed() {
                        old.go_away(ErrorCode::NORMAL, "duplicate connection");
                    }
                    c.clone()
                }
                None => {
                    m.insert(peer, c.clone());
                    c.clone()
                }
            }
        };
        if Arc::ptr_eq(&kept.sh, &c.sh) {
            let w: Weak<EpInner> = Arc::downgrade(&self.inner);
            let c2 = c.clone();
            tokio::spawn(async move {
                c2.closed().await;
                if let Some(ep) = w.upgrade() {
                    let mut m = ep.conns.lock().unwrap();
                    if m.get(&peer).is_some_and(|x| Arc::ptr_eq(&x.sh, &c2.sh)) {
                        m.remove(&peer);
                    }
                }
            });
        } else {
            // 負けた接続は、流れが無くなったら閉じる
            let c2 = c.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                c2.close("duplicate connection");
            });
        }
        Ok(kept)
    }

    pub fn connection(&self, peer: NodeId) -> Option<RelayConnection> {
        self.inner
            .conns
            .lock()
            .unwrap()
            .get(&peer)
            .filter(|c| !c.is_closed())
            .cloned()
    }

    /// 接続を返す。無ければ Dialer でつなぐ（同じ相手へは同時に 1 回だけ）。
    pub async fn connection_or_dial(&self, peer: NodeId) -> Result<RelayConnection, RelayError> {
        if let Some(c) = self.connection(peer) {
            return Ok(c);
        }
        let lock = self
            .inner
            .dialing
            .lock()
            .unwrap()
            .entry(peer)
            .or_default()
            .clone();
        let _g = lock.lock().await;
        if let Some(c) = self.connection(peer) {
            return Ok(c);
        }
        let Some(d) = self.inner.dialer.lock().unwrap().clone() else {
            return Err(RelayError::NotConnected(peer, "no dialer".into()));
        };
        let s = tokio::time::timeout(self.inner.cfg.handshake_timeout, d(peer))
            .await
            .map_err(|_| RelayError::NotConnected(peer, "dial timed out".into()))?
            .map_err(|e| RelayError::NotConnected(peer, e))?;
        let c = self.connect(s).await?;
        if c.peer_id() != Some(peer) {
            return Err(RelayError::NotConnected(
                peer,
                format!("dialed node {:?} instead", c.peer_id()),
            ));
        }
        Ok(c)
    }

    /// 相手のノードの stream を購読する。
    pub async fn subscribe(
        &self,
        peer: NodeId,
        stream_id: &str,
        tracks: TrackSel,
        priority: u8,
    ) -> Result<Upstream, RelayError> {
        let c = self.connection_or_dial(peer).await?;
        c.subscribe(
            stream_id,
            tracks,
            priority,
            self.inner.cfg.subscribe_timeout,
        )
        .await
    }

    pub fn peers(&self) -> Vec<NodeId> {
        let mut v: Vec<NodeId> = self
            .inner
            .conns
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, c)| !c.is_closed())
            .map(|(k, _)| *k)
            .collect();
        v.sort_unstable();
        v
    }

    /// 相手ごとの計測（RTT・帯域・待ち時間・捨てた数。配置の係が使う）。
    pub fn stats(&self) -> Vec<(NodeId, ConnStats)> {
        let conns: Vec<(NodeId, RelayConnection)> = self
            .inner
            .conns
            .lock()
            .unwrap()
            .iter()
            .map(|(k, c)| (*k, c.clone()))
            .collect();
        let mut v: Vec<(NodeId, ConnStats)> =
            conns.into_iter().map(|(k, c)| (k, c.stats())).collect();
        v.sort_unstable_by_key(|x| x.0);
        v
    }

    /// 全ての接続で退く（ノードを止める前）。
    pub fn go_away(&self, reason: &str) {
        for c in self.inner.conns.lock().unwrap().values() {
            c.go_away(ErrorCode::GOING_AWAY, reason);
        }
    }

    pub fn close_peer(&self, peer: NodeId, reason: &str) {
        if let Some(c) = self.inner.conns.lock().unwrap().remove(&peer) {
            c.close(reason);
        }
    }
}
