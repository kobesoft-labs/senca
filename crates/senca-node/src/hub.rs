//! stream タスク（ADR-0006）。送り手 → [hub] → ライブ HLS（packager）・購読者（broadcast）・録画（recorder）。

use crate::live::LiveCache;
use crate::packager::{Packager, PackagerOut};
use crate::recorder::{RecMsg, RecorderTx, SegmentData};
use senca_control::model::{CodecProfile, Stream};
use senca_core::{AccessUnit, Codec, CodecConfig, MediaKind, TrackId};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};

/// hub への入口のキュー長（AU）。送り手は待つ（TCP の背圧）。
const INGEST_QUEUE: usize = 1024;
/// 購読者への broadcast の長さ。
const FANOUT: usize = 2048;
/// GOP キャッシュの上限（AU 数・バイト）。
const GOP_MAX_AUS: usize = 2000;
const GOP_MAX_BYTES: usize = 64 << 20;

/// 購読者に配るもの。
#[derive(Debug, Clone)]
#[allow(dead_code)] // 購読者の種類が増えると使う（M8）
pub enum HubEvent {
    Config(TrackId, CodecConfig),
    Au(AccessUnit),
    /// 送り手が切れた・入れ替わった（epoch が変わる）。
    Discontinuity,
    /// 配信が終わった（再接続の猶予が切れた）。購読者は閉じてよい。
    End,
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum PublishError {
    BadKey,
    Busy,
    Stopped,
    Rejected(String),
}

enum Msg {
    Attach { id: u64, transport: &'static str, peer: String, reply: oneshot::Sender<Result<u64, PublishError>> },
    Config { id: u64, track: TrackId, config: CodecConfig },
    Au { id: u64, au: AccessUnit },
    Detach { id: u64, reason: String },
    UpdateStream(Box<Stream>),
    StartRecording { rec: String, not_before_ms: Option<i64>, reply: oneshot::Sender<Result<(), String>> },
    StopRecording { rec: String, end_at_ms: Option<i64>, reply: Option<oneshot::Sender<()>> },
    Shutdown { reply: oneshot::Sender<()> },
}

/// hub の情報（API の `live` 欄）。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct LiveInfo {
    pub transport: String,
    pub started_at: String,
    pub epoch: u64,
    pub tracks: Vec<serde_json::Value>,
    pub bitrate_bps: u64,
    pub viewers_hint: usize,
}

pub struct HubShared {
    pub live: Mutex<Option<LiveInfo>>,
    pub gop: Mutex<Vec<HubEvent>>,
    pub configs: Mutex<BTreeMap<TrackId, CodecConfig>>,
    pub cache: Arc<LiveCache>,
    pub fanout: broadcast::Sender<HubEvent>,
}

#[derive(Clone)]
pub struct HubHandle {
    tx: mpsc::Sender<Msg>,
    pub shared: Arc<HubShared>,
    pub stream_id: String,
    next_pub: Arc<std::sync::atomic::AtomicU64>,
}

/// 送り手の口。drop で切断扱い（detach を呼ばずに消えても hub が気づく）。
pub struct Publisher {
    id: u64,
    epoch: u64,
    hub: HubHandle,
    detached: bool,
}

impl Publisher {
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn stream_id(&self) -> &str {
        &self.hub.stream_id
    }
    pub async fn config(&self, track: TrackId, config: CodecConfig) -> Result<(), &'static str> {
        self.hub.tx.send(Msg::Config { id: self.id, track, config }).await.map_err(|_| "stream closed")
    }
    pub async fn access_unit(&self, au: AccessUnit) -> Result<(), &'static str> {
        self.hub.tx.send(Msg::Au { id: self.id, au }).await.map_err(|_| "stream closed")
    }
    pub async fn detach(mut self, reason: &str) {
        self.detached = true;
        let _ = self.hub.tx.send(Msg::Detach { id: self.id, reason: reason.into() }).await;
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        if !self.detached {
            let _ = self.hub.tx.try_send(Msg::Detach { id: self.id, reason: "dropped".into() });
        }
    }
}

/// 購読者。最初に GOP キャッシュを受け取り、その後 broadcast を受ける。
pub struct Subscription {
    rx: broadcast::Receiver<HubEvent>,
    initial: Vec<HubEvent>,
    /// 取りこぼした後は次のキーフレームまで映像を落とす。
    waiting_key: bool,
    flv: senca_flv::build::FlvMuxer,
    base_ms: Option<i64>,
}

impl Subscription {
    pub fn initial(&mut self) -> Vec<HubEvent> {
        std::mem::take(&mut self.initial)
    }

    pub async fn recv(&mut self) -> Result<HubEvent, broadcast::error::RecvError> {
        loop {
            let ev = self.rx.recv().await?;
            if self.waiting_key {
                match &ev {
                    HubEvent::Au(au) if au.codec.kind() == MediaKind::Video && !au.keyframe => continue,
                    HubEvent::Au(au) if au.codec.kind() == MediaKind::Video => self.waiting_key = false,
                    _ => {}
                }
            }
            return Ok(ev);
        }
    }

    pub fn lagged(&mut self, _n: u64) {
        self.waiting_key = true;
    }

    /// FLV タグにする（RTMP play 用）。時刻は最初の AU を 0 にそろえる。
    pub fn flv(&mut self, ev: &HubEvent, out: &mut Vec<senca_flv::build::OutTag>) {
        match ev {
            HubEvent::Config(_, c) => {
                let _ = self.flv.set_config(c.clone());
            }
            HubEvent::Au(au) => {
                let ms = senca_core::Rescale::rescale(au.dts, au.timebase, senca_core::Timebase::MILLIS).unwrap_or(0);
                let base = *self.base_ms.get_or_insert(ms);
                let mut a = au.clone();
                a.dts = ms - base;
                a.pts = senca_core::Rescale::rescale(au.pts, au.timebase, senca_core::Timebase::MILLIS).unwrap_or(ms) - base;
                a.timebase = senca_core::Timebase::MILLIS;
                if a.dts < 0 {
                    return;
                }
                let _ = self.flv.access_unit(&a, out);
            }
            HubEvent::Discontinuity => {
                self.flv.resend_configs();
            }
            HubEvent::End => {}
        }
    }
}

pub struct HubDeps {
    pub data_dir: std::path::PathBuf,
    pub record_queue: usize,
    pub reconnect_grace: Duration,
    pub on_recorder_event: Arc<dyn Fn(crate::recorder::RecorderEvent) + Send + Sync>,
    pub on_idle: Arc<dyn Fn(&str) + Send + Sync>,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
}

pub fn spawn(stream: Stream, deps: HubDeps) -> HubHandle {
    let (tx, rx) = mpsc::channel(INGEST_QUEUE);
    let (fanout, _) = broadcast::channel(FANOUT);
    let shared = Arc::new(HubShared {
        live: Mutex::new(None),
        gop: Mutex::new(Vec::new()),
        configs: Mutex::new(BTreeMap::new()),
        cache: Arc::new(LiveCache::new()),
        fanout,
    });
    let handle = HubHandle {
        tx,
        shared: shared.clone(),
        stream_id: stream.id.clone(),
        next_pub: Arc::new(std::sync::atomic::AtomicU64::new(1)),
    };
    let ev = deps.on_recorder_event.clone();
    let recorder = crate::recorder::spawn(
        deps.data_dir.clone(),
        stream.id.clone(),
        deps.record_queue,
        Box::new(move |e| ev(e)),
    );
    tokio::spawn(run(stream, rx, shared, recorder, deps));
    handle
}

impl HubHandle {
    pub async fn attach(&self, transport: &'static str, peer: &str) -> Result<Publisher, PublishError> {
        let id = self.next_pub.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Attach { id, transport, peer: peer.into(), reply })
            .await
            .map_err(|_| PublishError::Rejected("stream closed".into()))?;
        let epoch = rx.await.map_err(|_| PublishError::Rejected("stream closed".into()))??;
        Ok(Publisher { id, epoch, hub: self.clone(), detached: false })
    }

    pub fn subscribe(&self) -> Subscription {
        // 先に receiver を作ってから GOP を写す（隙間を作らない。重複は受け手で無害）
        let rx = self.shared.fanout.subscribe();
        let mut initial: Vec<HubEvent> = self.shared.configs.lock().unwrap().iter().map(|(t, c)| HubEvent::Config(*t, c.clone())).collect();
        initial.extend(self.shared.gop.lock().unwrap().iter().cloned());
        Subscription { rx, initial, waiting_key: false, flv: senca_flv::build::FlvMuxer::new(), base_ms: None }
    }

    pub fn is_live(&self) -> bool {
        self.shared.live.lock().unwrap().is_some()
    }

    pub fn update(&self, s: &Stream) {
        let _ = self.tx.try_send(Msg::UpdateStream(Box::new(s.clone())));
    }

    pub async fn start_recording(&self, rec: &str, not_before_ms: Option<i64>) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::StartRecording { rec: rec.into(), not_before_ms, reply })
            .await
            .map_err(|_| "stream closed".to_string())?;
        rx.await.map_err(|_| "stream closed".to_string())?
    }

    pub fn stop_recording(&self, rec: &str, end_at_ms: Option<i64>) {
        let _ = self.tx.try_send(Msg::StopRecording { rec: rec.into(), end_at_ms, reply: None });
    }

    pub async fn shutdown(&self) {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Msg::Shutdown { reply }).await.is_ok() {
            let _ = rx.await;
        }
    }
}

struct State {
    stream: Stream,
    publisher: Option<(u64, &'static str)>,
    epoch: u64,
    /// 直近に AU を受けたか（最初の publish の前は false）。
    ever_published: bool,
    configs: BTreeMap<TrackId, CodecConfig>,
    packager: Packager,
    pending_stops: Vec<(String, i64)>,
    recordings: Vec<String>,
    bytes_window: (u64, u64),
}

async fn run(stream: Stream, mut rx: mpsc::Receiver<Msg>, shared: Arc<HubShared>, recorder: RecorderTx, deps: HubDeps) {
    let mut st = State {
        packager: Packager::new(&stream),
        stream,
        publisher: None,
        epoch: 0,
        ever_published: false,
        configs: BTreeMap::new(),
        pending_stops: Vec::new(),
        recordings: Vec::new(),
        bytes_window: (crate::util::mono_ms(), 0),
    };
    let mut grace_deadline: Option<tokio::time::Instant> = None;
    loop {
        let msg = match grace_deadline {
            Some(d) => tokio::select! {
                m = rx.recv() => m,
                _ = tokio::time::sleep_until(d) => {
                    // 猶予が切れた: 配信の終わり
                    grace_deadline = None;
                    end_of_stream(&mut st, &shared, &recorder, &deps).await;
                    continue;
                }
            },
            None => rx.recv().await,
        };
        let Some(msg) = msg else { break };
        match msg {
            Msg::Attach { id, transport, peer, reply } => {
                if st.publisher.is_some() {
                    let _ = reply.send(Err(PublishError::Busy));
                    continue;
                }
                if st.stream.desired_state != "running" {
                    let _ = reply.send(Err(PublishError::Stopped));
                    continue;
                }
                grace_deadline = None;
                if st.ever_published {
                    st.epoch += 1;
                    let _ = shared.fanout.send(HubEvent::Discontinuity);
                    st.packager.discontinuity();
                    if !st.recordings.is_empty() {
                        recorder.control(RecMsg::Gap { at_ms: crate::util::now_ms(), reason: "publisher_reconnect".into() }).await;
                    }
                }
                st.publisher = Some((id, transport));
                *shared.live.lock().unwrap() = Some(LiveInfo {
                    transport: transport.into(),
                    started_at: crate::util::rfc3339(crate::util::now_ms()),
                    epoch: st.epoch,
                    ..Default::default()
                });
                (deps.log)(&format!("stream {}: publisher attached ({transport} {peer}) epoch={}", st.stream.id, st.epoch));
                let _ = reply.send(Ok(st.epoch));
            }
            Msg::Detach { id, reason } => {
                if st.publisher.map(|p| p.0) != Some(id) {
                    continue;
                }
                st.publisher = None;
                (deps.log)(&format!("stream {}: publisher detached ({reason})", st.stream.id));
                grace_deadline = Some(tokio::time::Instant::now() + deps.reconnect_grace);
            }
            Msg::Config { id, track, config } => {
                if st.publisher.map(|p| p.0) != Some(id) {
                    continue;
                }
                if st.stream.codec_profile == CodecProfile::RoyaltyFree && config.codec.is_patent_pooled() {
                    (deps.log)(&format!(
                        "stream {}: {} rejected by royalty_free profile; dropping track",
                        st.stream.id, config.codec
                    ));
                    continue;
                }
                st.configs.insert(track, config.clone());
                shared.configs.lock().unwrap().insert(track, config.clone());
                update_tracks(&st, &shared);
                let outs = st.packager.config(track, &config);
                emit(&mut st, &shared, &recorder, outs);
                let _ = shared.fanout.send(HubEvent::Config(track, config));
            }
            Msg::Au { id, au } => {
                if st.publisher.map(|p| p.0) != Some(id) || !st.configs.contains_key(&au.track) {
                    continue;
                }
                // VP8/VP9 の寸法は最初のキーフレームから補う（fMP4 の初期化セグメントに要る）
                if au.keyframe
                    && matches!(au.codec, Codec::Vp8 | Codec::Vp9)
                    && let Some(cfg) = st.configs.get(&au.track)
                    && cfg.video.is_none_or(|v| v.width == 0)
                    && let Some((w, h)) = crate::media::vpx_keyframe_dims(au.codec, &au.payload)
                {
                    let mut c = cfg.clone();
                    let v = c.video.get_or_insert_with(Default::default);
                    v.width = w;
                    v.height = h;
                    st.configs.insert(au.track, c.clone());
                    shared.configs.lock().unwrap().insert(au.track, c.clone());
                    update_tracks(&st, &shared);
                    let outs = st.packager.config(au.track, &c);
                    emit(&mut st, &shared, &recorder, outs);
                }
                st.ever_published = true;
                st.bytes_window.1 += au.payload.len() as u64;
                let now = crate::util::mono_ms();
                if now - st.bytes_window.0 >= 2000 {
                    let bps = st.bytes_window.1 * 8 * 1000 / (now - st.bytes_window.0).max(1);
                    if let Some(l) = shared.live.lock().unwrap().as_mut() {
                        l.bitrate_bps = bps;
                        l.viewers_hint = shared.fanout.receiver_count();
                    }
                    st.bytes_window = (now, 0);
                }
                gop_push(&shared, &au);
                let outs = st.packager.push(&au);
                let _ = shared.fanout.send(HubEvent::Au(au));
                emit(&mut st, &shared, &recorder, outs);
            }
            Msg::UpdateStream(s) => {
                let stopped = s.desired_state != "running";
                st.stream = *s;
                if stopped && st.publisher.is_some() {
                    // 送り手は接続側が hub の閉鎖で気づく。ここでは受け付けを止める
                    st.publisher = None;
                    grace_deadline = Some(tokio::time::Instant::now());
                }
            }
            Msg::StartRecording { rec, not_before_ms, reply } => {
                let res = start_rec(&mut st, &recorder, &rec, not_before_ms).await;
                let _ = reply.send(res);
            }
            Msg::StopRecording { rec, end_at_ms, reply } => {
                match end_at_ms {
                    // 今作っているセグメントがその時刻を含むなら、それが閉じてから止める
                    Some(t) if st.publisher.is_some() && st.packager.has_open_segment() => st.pending_stops.push((rec, t)),
                    _ => stop_rec(&mut st, &recorder, &rec, end_at_ms).await,
                }
                if let Some(r) = reply {
                    let _ = r.send(());
                }
            }
            Msg::Shutdown { reply } => {
                let outs = st.packager.end();
                emit(&mut st, &shared, &recorder, outs);
                let (r, w) = oneshot::channel();
                recorder.control(RecMsg::Shutdown { reply: r }).await;
                let _ = w.await;
                let _ = reply.send(());
                break;
            }
        }
    }
    *shared.live.lock().unwrap() = None;
    (deps.on_idle)(&st.stream.id);
}

async fn start_rec(st: &mut State, recorder: &RecorderTx, rec: &str, not_before_ms: Option<i64>) -> Result<(), String> {
    if st.recordings.iter().any(|r| r == rec) {
        return Ok(());
    }
    let (reply, rx) = oneshot::channel();
    // fMP4 なら初期化セグメントを先に渡す
    if let Some((bytes, name)) = st.packager.init_segment() {
        recorder.control(RecMsg::Init { bytes, name }).await;
    }
    recorder
        .control(RecMsg::Start {
            id: rec.into(),
            container: st.packager.container().into(),
            not_before_ms,
            started_at_ms: crate::util::now_ms(),
            reply: Some(reply),
        })
        .await;
    rx.await.map_err(|_| "recorder closed".to_string())??;
    st.recordings.push(rec.into());
    Ok(())
}

async fn stop_rec(st: &mut State, recorder: &RecorderTx, rec: &str, not_after_ms: Option<i64>) {
    st.recordings.retain(|r| r != rec);
    let (reply, rx) = oneshot::channel();
    recorder
        .control(RecMsg::Stop {
            id: rec.into(),
            ended_at_ms: crate::util::now_ms(),
            not_after_ms,
            state: "finalized",
            reply: Some(reply),
        })
        .await;
    let _ = rx.await;
}

async fn end_of_stream(st: &mut State, shared: &HubShared, recorder: &RecorderTx, deps: &HubDeps) {
    (deps.log)(&format!("stream {}: input ended", st.stream.id));
    let _ = shared.fanout.send(HubEvent::End);
    *shared.live.lock().unwrap() = None;
    shared.gop.lock().unwrap().clear();
    let outs = st.packager.end();
    emit(st, shared, recorder, outs);
    shared.cache.end();
    st.ever_published = false;
    st.epoch += 1;
    let recs = std::mem::take(&mut st.recordings);
    for r in recs {
        stop_rec(st, recorder, &r, None).await;
    }
    for (r, _) in std::mem::take(&mut st.pending_stops) {
        stop_rec(st, recorder, &r, None).await;
    }
    (deps.on_idle)(&st.stream.id);
}

fn emit(st: &mut State, shared: &HubShared, recorder: &RecorderTx, outs: Vec<PackagerOut>) {
    for o in outs {
        match o {
            PackagerOut::Segment(seg) => {
                shared.cache.push_segment(&seg);
                if !st.recordings.is_empty() || !st.pending_stops.is_empty() {
                    recorder.segment(SegmentData {
                        name: seg.name.clone(),
                        epoch: seg.epoch,
                        sequence: seg.sequence,
                        duration_ms: seg.duration_ms,
                        wall_ms: seg.wall_ms,
                        discontinuity: seg.discontinuity,
                        bytes: seg.bytes.clone(),
                    });
                }
                // このセグメントの終わりを過ぎた停止を実行
                let end_wall = seg.wall_ms + seg.duration_ms as i64;
                let (due, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut st.pending_stops).into_iter().partition(|(_, t)| *t <= end_wall);
                st.pending_stops = keep;
                for (r, t) in due {
                    st.recordings.retain(|x| *x != r);
                    recorder.try_control(RecMsg::Stop {
                        id: r,
                        ended_at_ms: crate::util::now_ms(),
                        not_after_ms: Some(t),
                        state: "finalized",
                        reply: None,
                    });
                }
            }
            PackagerOut::Part(p) => shared.cache.push_part(&p),
            PackagerOut::Init { bytes, name } => {
                shared.cache.set_init(bytes.clone(), &name);
                if !st.recordings.is_empty() {
                    recorder.try_control(RecMsg::Init { bytes, name });
                }
            }
        }
    }
}

fn gop_push(shared: &HubShared, au: &AccessUnit) {
    let mut g = shared.gop.lock().unwrap();
    let is_video = au.codec.kind() == MediaKind::Video;
    let has_video = shared.configs.lock().unwrap().values().any(|c| c.codec.kind() == MediaKind::Video);
    if is_video && au.keyframe {
        g.clear();
    } else if !has_video {
        // 音声だけ: 直近 1 秒ほど
        let n = g.len();
        if n > 50 {
            g.drain(..n - 50);
        }
    }
    let bytes: usize = g.iter().map(|e| if let HubEvent::Au(a) = e { a.payload.len() } else { 0 }).sum();
    if g.len() >= GOP_MAX_AUS || bytes > GOP_MAX_BYTES {
        g.clear(); // 異常に長い GOP は持たない（次のキーフレームから）
        return;
    }
    if has_video && g.is_empty() && !(is_video && au.keyframe) {
        return; // キーフレームの前の AU は持たない
    }
    g.push(HubEvent::Au(au.clone()));
}

fn update_tracks(st: &State, shared: &HubShared) {
    let tracks: Vec<serde_json::Value> = st
        .configs
        .iter()
        .map(|(t, c)| {
            let mut v = serde_json::json!({ "track": t.0, "codec": c.codec.as_str() });
            if let Some(vp) = c.video {
                v["width"] = vp.width.into();
                v["height"] = vp.height.into();
            }
            if let Some(ap) = c.audio {
                v["sample_rate"] = ap.sample_rate.into();
                v["channels"] = ap.channels.into();
            }
            v
        })
        .collect();
    if let Some(l) = shared.live.lock().unwrap().as_mut() {
        l.tracks = tracks;
    }
}

