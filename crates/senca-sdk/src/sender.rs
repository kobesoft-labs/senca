//! 送り手（[`Sender`]）。
//!
//! ```no_run
//! use senca_sdk::{Sender, SenderConfig};
//! use senca_sdk::frame::{VideoCodec, VideoFormat, VideoFrame};
//! let cfg = SenderConfig::from_uri("senca://ingest?v=1&proto=srt&host=127.0.0.1&port=9000&streamid=publish:sk_x").unwrap();
//! let s = Sender::new(cfg).unwrap();
//! s.set_event_handler(|ev| eprintln!("{ev:?}")).unwrap();
//! s.start().unwrap();
//! # let annexb: &[u8] = &[];
//! s.push_video(&VideoFrame { codec: VideoCodec::H264, format: VideoFormat::AnnexB, data: annexb,
//!     pts_us: 0, dts_us: 0, keyframe: None }).unwrap();
//! s.stop();
//! ```
//!
//! ## スレッド
//!
//! - すべてのメソッドは `&self` で、どのスレッドから呼んでもよい（映像と音声を別のスレッドから入れてよい）。
//! - 送信は内部のスレッド 1 本（[`start`](Sender::start) で作り、[`stop`](Sender::stop) で終わるまで待つ）。
//! - イベントの通知は送信スレッドから呼ぶ。通知の中で `stop` を呼ばないこと（自分の終わりを待って止まる）。
//!   `push_*`・`stats` は呼んでよい。
//!
//! ## つなぎ直し
//!
//! 回線の失敗では間隔を倍にしながら（上限まで、±20% のゆらぎ付き）つなぎ直す。相手が断った
//! （鍵・パスフレーズ・権限）ときは止まり、状態は [`SenderState::Failed`] になる（理由はイベントで渡す）。
//! つなぎ直した後の映像はキーフレームから送る（キューに無ければ符号化器へキーフレームを頼む）。

use crate::Error;
use crate::frame::{AudioConfig, AudioFrame, Item, Normalizer, VideoCodec, VideoFrame};
use crate::queue::{LiveQueue, QueueLimits};
use crate::transport::rtmp::RtmpConnector;
use crate::transport::srt::SrtConnector;
use crate::transport::{Connector, TlsConnector, Transport, TransportError};
use crate::uri::{Endpoint, IngestUri};
use senca_core::{CodecConfig, MediaKind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// 送り手の設定。
#[derive(Clone)]
pub struct SenderConfig {
    pub endpoint: Endpoint,
    pub queue: QueueLimits,
    pub connect_timeout: Duration,
    pub reconnect_initial: Duration,
    pub reconnect_max: Duration,
    /// つなぎ直しの回数の上限（None は無制限）。
    pub max_reconnect_attempts: Option<u32>,
    /// 停止時に送り残しを送る時間。
    pub close_linger: Duration,
    /// RTMPS 用の TLS。
    pub tls: Option<Arc<dyn TlsConnector>>,
    /// 接続の始めに、映像と音声の両方の設定がそろうまで AU を送らずに待つ上限。
    /// 受け手（HLS など）がトラックの構成を途中で作り直さないようにする。片方しか送らないなら
    /// この時間だけ遅れて始まる。
    pub startup_hold: Duration,
}

impl std::fmt::Debug for SenderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SenderConfig")
            .field("endpoint", &self.endpoint)
            .field("queue", &self.queue)
            .field("tls", &self.tls.is_some())
            .finish_non_exhaustive()
    }
}

impl SenderConfig {
    pub fn new(endpoint: Endpoint) -> Self {
        SenderConfig {
            endpoint,
            queue: QueueLimits::default(),
            connect_timeout: Duration::from_secs(5),
            reconnect_initial: Duration::from_millis(500),
            reconnect_max: Duration::from_secs(10),
            max_reconnect_attempts: None,
            close_linger: Duration::from_secs(2),
            tls: None,
            startup_hold: Duration::from_millis(1000),
        }
    }

    /// 設定文字列（`senca://ingest?...`・`srt://`・`rtmp(s)://`）から作る。
    pub fn from_uri(s: &str) -> Result<Self, Error> {
        Ok(Self::new(IngestUri::parse(s)?.endpoint))
    }

    fn connector(&self) -> Result<Box<dyn Connector>, Error> {
        let unsup = |e: TransportError| match e.kind {
            crate::transport::TransportErrorKind::Unsupported if e.message.contains("TLS") => {
                Error::Unsupported("RTMPS needs a TLS provider")
            }
            _ => Error::Unsupported("endpoint not supported"),
        };
        Ok(match &self.endpoint {
            Endpoint::Rtmp { url, key } => {
                Box::new(RtmpConnector::new(url, key.clone(), self.tls.clone()).map_err(unsup)?)
            }
            Endpoint::Srt {
                host,
                port,
                stream_id,
                passphrase,
                latency_ms,
                pbkeylen,
            } => Box::new(
                SrtConnector::new(
                    host,
                    *port,
                    stream_id.as_ref(),
                    passphrase.as_ref(),
                    *latency_ms,
                    *pbkeylen,
                )
                .map_err(unsup)?,
            ),
            Endpoint::Whip { .. } => {
                return Err(Error::Unsupported(
                    "WHIP is not available in the native SDK yet (needs the WebRTC stack)",
                ));
            }
        })
    }
}

/// 状態。C ABI の値と同じ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum SenderState {
    Idle = 0,
    Connecting = 1,
    Connected = 2,
    Reconnecting = 3,
    Stopped = 4,
    Failed = 5,
}

/// 通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// 状態が変わった。`reason` は失敗・つなぎ直しの理由（秘密を含まない）。
    StateChanged {
        state: SenderState,
        reason: Option<String>,
    },
    /// 符号化器にキーフレーム（IDR）を作ってほしい。
    KeyframeRequested,
}

/// `push_*` の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Queued,
    /// ライブ優先の規則、または設定が無いために捨てた。
    Dropped,
}

/// 統計。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub bytes_sent: u64,
    /// 直近の送信ビットレート（bit/s、約 1 秒ごとに更新）。
    pub bitrate_bps: u64,
    pub video_frames_sent: u64,
    pub audio_frames_sent: u64,
    pub video_frames_dropped: u64,
    pub audio_frames_dropped: u64,
    pub queue_frames: u64,
    pub queue_bytes: u64,
    pub queue_duration_ms: u64,
    /// SRT の往復時間（µs）。RTMP では None。
    pub rtt_us: Option<u64>,
    pub reconnects: u64,
}

type Handler = Arc<dyn Fn(&Event) + Send + Sync>;

struct Inner {
    queue: LiveQueue,
    norm: Normalizer,
    state: SenderState,
    started: bool,
    stop: bool,
    /// 設定が無いうちに来て捨てたフレーム。
    dropped_no_config: u64,
    stats: Stats,
}

struct Shared {
    inner: Mutex<Inner>,
    cond: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // 送信スレッドが panic しても状態は壊れていない（値の更新だけ）ので使い続ける
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// 送り手。
pub struct Sender {
    shared: Arc<Shared>,
    config: SenderConfig,
    connector: Mutex<Option<Box<dyn Connector>>>,
    handler: Mutex<Option<Handler>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for Sender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl Sender {
    /// 作る（まだつながない）。WHIP・TLS なしの RTMPS は `Unsupported`。
    pub fn new(config: SenderConfig) -> Result<Sender, Error> {
        let c = config.connector()?;
        Ok(Self::with_connector(config, c))
    }

    /// 送り方を差し替えて作る（試験・独自の transport 用）。`config.endpoint` は使わない。
    pub fn with_connector(config: SenderConfig, connector: Box<dyn Connector>) -> Sender {
        Sender {
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner {
                    queue: LiveQueue::new(config.queue),
                    norm: Normalizer::new(),
                    state: SenderState::Idle,
                    started: false,
                    stop: false,
                    dropped_no_config: 0,
                    stats: Stats::default(),
                }),
                cond: Condvar::new(),
            }),
            config,
            connector: Mutex::new(Some(connector)),
            handler: Mutex::new(None),
            worker: Mutex::new(None),
        }
    }

    /// 通知の受け口（開始前だけ）。
    pub fn set_event_handler(
        &self,
        f: impl Fn(&Event) + Send + Sync + 'static,
    ) -> Result<(), Error> {
        if self.shared.lock().started {
            return Err(Error::InvalidState(
                "event handler must be set before start",
            ));
        }
        *self.handler.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(f));
        Ok(())
    }

    /// 送信スレッドを始める。
    pub fn start(&self) -> Result<(), Error> {
        {
            let mut g = self.shared.lock();
            if g.started {
                return Err(Error::InvalidState("already started"));
            }
            g.started = true;
        }
        let connector = self
            .connector
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .ok_or(Error::InvalidState("already started"))?;
        let handler = self
            .handler
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let shared = self.shared.clone();
        let cfg = self.config.clone();
        let h = std::thread::Builder::new()
            .name("senca-sender".into())
            .spawn(move || {
                Worker {
                    shared,
                    cfg,
                    handler,
                    connector,
                }
                .run()
            })
            .map_err(|_| Error::Internal("failed to spawn sender thread"))?;
        *self.worker.lock().unwrap_or_else(|p| p.into_inner()) = Some(h);
        Ok(())
    }

    fn enqueue(
        &self,
        f: impl FnOnce(&mut Normalizer, &mut Vec<Item>) -> Result<bool, Error>,
    ) -> Result<PushOutcome, Error> {
        let mut items = Vec::with_capacity(2);
        let mut g = self.shared.lock();
        if g.stop {
            return Err(Error::InvalidState("sender is stopped"));
        }
        let had_config = f(&mut g.norm, &mut items)?;
        if !had_config {
            g.dropped_no_config += 1;
        }
        let mut accepted = had_config;
        let mut any = false;
        for it in items {
            any = true;
            let is_au = matches!(it, Item::Au(_));
            if !g.queue.push(it) && is_au {
                accepted = false;
            }
        }
        drop(g);
        if any {
            self.shared.cond.notify_one();
        }
        Ok(if accepted {
            PushOutcome::Queued
        } else {
            PushOutcome::Dropped
        })
    }

    /// 映像の設定レコード（AVCC / HVCC の decoder configuration record）。Annex B で SPS/PPS を AU に入れて
    /// 送るなら不要。
    pub fn set_video_config(&self, codec: VideoCodec, record: &[u8]) -> Result<(), Error> {
        self.enqueue(|n, out| n.set_video_config(codec, record, out).map(|_| true))
            .map(|_| ())
    }

    /// 音声の設定（raw の AAC と Opus で必要。ADTS ならヘッダーから作る）。
    pub fn set_audio_config(&self, config: &AudioConfig) -> Result<(), Error> {
        self.enqueue(|n, out| n.set_audio_config(config, out).map(|_| true))
            .map(|_| ())
    }

    pub fn push_video(&self, frame: &VideoFrame<'_>) -> Result<PushOutcome, Error> {
        self.enqueue(|n, out| n.push_video(frame, out))
    }

    pub fn push_audio(&self, frame: &AudioFrame<'_>) -> Result<PushOutcome, Error> {
        self.enqueue(|n, out| n.push_audio(frame, out))
    }

    pub fn state(&self) -> SenderState {
        self.shared.lock().state
    }

    pub fn stats(&self) -> Stats {
        let g = self.shared.lock();
        let mut s = g.stats.clone();
        s.video_frames_dropped = g.queue.dropped_video;
        s.audio_frames_dropped = g.queue.dropped_audio + g.dropped_no_config;
        s.queue_frames = g.queue.len() as u64;
        s.queue_bytes = g.queue.bytes() as u64;
        s.queue_duration_ms = (g.queue.duration_us() / 1000) as u64;
        s
    }

    /// 止める。キューの残りを送り（つながっていれば）、閉じ、送信スレッドの終わりを待つ。何度呼んでもよい。
    pub fn stop(&self) {
        {
            let mut g = self.shared.lock();
            g.stop = true;
            if !g.started {
                g.state = SenderState::Stopped;
            }
        }
        self.shared.cond.notify_all();
        let h = self.worker.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(h) = h {
            let _ = h.join();
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 揺らぎ（0.8〜1.2 倍）。
fn jitter(d: Duration) -> Duration {
    let mut b = [0u8; 2];
    let r = if getrandom::fill(&mut b).is_ok() {
        u16::from_le_bytes(b)
    } else {
        32768
    };
    d.mul_f64(0.8 + 0.4 * (r as f64 / 65535.0))
}

struct Worker {
    shared: Arc<Shared>,
    cfg: SenderConfig,
    handler: Option<Handler>,
    connector: Box<dyn Connector>,
}

enum PumpEnd {
    Stopped,
    Failed(TransportError),
}

impl Worker {
    fn emit(&self, ev: Event) {
        if let Some(h) = &self.handler {
            h(&ev);
        }
    }

    fn set_state(&self, state: SenderState, reason: Option<String>) {
        {
            let mut g = self.shared.lock();
            if g.state == state && reason.is_none() {
                return;
            }
            g.state = state;
        }
        self.emit(Event::StateChanged { state, reason });
    }

    fn stopping(&self) -> bool {
        self.shared.lock().stop
    }

    fn run(mut self) {
        // 今の設定（つなぎ直したら最初に渡す）
        let mut configs: [Option<CodecConfig>; 2] = [None, None];
        let mut backoff = self.cfg.reconnect_initial;
        let mut attempts = 0u32;
        let mut bytes_base = 0u64;
        loop {
            if self.stopping() {
                break;
            }
            self.set_state(SenderState::Connecting, None);
            let err = match self.connector.connect(self.cfg.connect_timeout) {
                Ok(mut t) => {
                    attempts = 0;
                    backoff = self.cfg.reconnect_initial;
                    let r = self.pump(&mut *t, &mut configs, bytes_base);
                    bytes_base += t.bytes_sent();
                    match r {
                        PumpEnd::Stopped => {
                            t.close(self.cfg.close_linger);
                            break;
                        }
                        PumpEnd::Failed(e) => {
                            t.close(Duration::ZERO);
                            self.shared.lock().stats.rtt_us = None;
                            e
                        }
                    }
                }
                Err(e) => e,
            };
            if !err.retryable() {
                self.set_state(SenderState::Failed, Some(err.message));
                return;
            }
            attempts += 1;
            if self
                .cfg
                .max_reconnect_attempts
                .is_some_and(|m| attempts > m)
            {
                self.set_state(
                    SenderState::Failed,
                    Some(format!(
                        "gave up after {} attempts: {}",
                        attempts - 1,
                        err.message
                    )),
                );
                return;
            }
            self.shared.lock().stats.reconnects += 1;
            self.set_state(SenderState::Reconnecting, Some(err.message));
            let wait = jitter(backoff);
            backoff = (backoff * 2).min(self.cfg.reconnect_max);
            let deadline = Instant::now() + wait;
            let mut g = self.shared.lock();
            while !g.stop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                g = self
                    .shared
                    .cond
                    .wait_timeout(g, left)
                    .map(|r| r.0)
                    .unwrap_or_else(|p| p.into_inner().0);
            }
        }
        self.set_state(SenderState::Stopped, None);
    }

    fn pump(
        &self,
        t: &mut dyn Transport,
        configs: &mut [Option<CodecConfig>; 2],
        bytes_base: u64,
    ) -> PumpEnd {
        for c in configs.iter().flatten() {
            if let Err(e) = t.set_config(c) {
                return PumpEnd::Failed(e);
            }
        }
        self.shared.lock().queue.require_keyframe();
        self.set_state(SenderState::Connected, None);
        let mut last_poll = Instant::now();
        let mut meter_at = Instant::now();
        let mut meter_bytes = t.bytes_sent();
        let started = Instant::now();
        let mut early: Vec<CodecConfig> = Vec::new();
        loop {
            let (item, stop, key_req) = {
                let mut g = self.shared.lock();
                // まだ設定の無い種類の設定は、キューの途中にあっても先に使う（その種類の AU はまだ無い）
                for (slot, kind) in [(0, MediaKind::Video), (1, MediaKind::Audio)] {
                    if configs[slot].is_none() && !early.iter().any(|c| c.codec.kind() == kind) {
                        early.extend(g.queue.take_config(kind));
                    }
                }
                let have = |k: MediaKind| {
                    configs
                        .iter()
                        .flatten()
                        .chain(early.iter())
                        .any(|c| c.codec.kind() == k)
                };
                let hold = !g.stop
                    && !(have(MediaKind::Video) && have(MediaKind::Audio))
                    && started.elapsed() < self.cfg.startup_hold
                    && g.queue.duration_us() < self.cfg.startup_hold.as_micros() as i64 / 2;
                if early.is_empty() && (g.queue.is_empty() || hold) && !g.stop {
                    let wait = t
                        .poll_interval()
                        .saturating_sub(last_poll.elapsed())
                        .max(Duration::from_micros(200));
                    g = self
                        .shared
                        .cond
                        .wait_timeout(g, wait)
                        .map(|r| r.0)
                        .unwrap_or_else(|p| p.into_inner().0);
                }
                let it = if hold || !early.is_empty() {
                    None
                } else {
                    g.queue.pop()
                };
                let stop = g.stop && !hold;
                (it, stop, g.queue.take_keyframe_request())
            };
            for c in early.drain(..) {
                let slot = usize::from(c.codec.kind() != MediaKind::Video);
                if let Err(e) = t.set_config(&c) {
                    return PumpEnd::Failed(e);
                }
                configs[slot] = Some(c);
            }
            if key_req {
                self.emit(Event::KeyframeRequested);
            }
            let r = match &item {
                Some(Item::Config(c)) => {
                    let slot = usize::from(c.codec.kind() != MediaKind::Video);
                    configs[slot] = Some(c.clone());
                    t.set_config(c)
                }
                Some(Item::Au(a)) => t.send(a).map(|_| {
                    let mut g = self.shared.lock();
                    if a.kind() == MediaKind::Video {
                        g.stats.video_frames_sent += 1;
                    } else {
                        g.stats.audio_frames_sent += 1;
                    }
                }),
                None if stop && self.shared.lock().queue.is_empty() => return PumpEnd::Stopped,
                None => Ok(()),
            };
            if let Err(e) = r {
                return PumpEnd::Failed(e);
            }
            if last_poll.elapsed() >= t.poll_interval() || item.is_none() {
                if let Err(e) = t.poll() {
                    return PumpEnd::Failed(e);
                }
                last_poll = Instant::now();
                let el = meter_at.elapsed();
                let mut g = self.shared.lock();
                g.stats.bytes_sent = bytes_base + t.bytes_sent();
                g.stats.rtt_us = t.rtt_us();
                if el >= Duration::from_secs(1) {
                    let b = t.bytes_sent();
                    g.stats.bitrate_bps =
                        ((b - meter_bytes) as f64 * 8.0 / el.as_secs_f64()) as u64;
                    meter_bytes = b;
                    meter_at = Instant::now();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::VideoFormat;
    use crate::frame::tests::{PPS, SPS, annexb};
    use crate::transport::TransportErrorKind;
    use senca_core::AccessUnit;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 記録するだけの transport。`fail_after` 個送ったら回線の失敗を返す。
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<String>>>);

    struct MockTransport {
        log: Log,
        sent: u64,
        fail_after: Option<u64>,
    }

    impl Transport for MockTransport {
        fn set_config(&mut self, c: &CodecConfig) -> Result<(), TransportError> {
            self.log
                .0
                .lock()
                .unwrap()
                .push(format!("config {} {}", c.codec, c.generation));
            Ok(())
        }
        fn send(&mut self, au: &AccessUnit) -> Result<(), TransportError> {
            if self.fail_after == Some(self.sent) {
                return Err(TransportError::network("boom"));
            }
            self.sent += 1;
            self.log
                .0
                .lock()
                .unwrap()
                .push(format!("au {} {} {}", au.codec, au.dts, au.keyframe));
            Ok(())
        }
        fn poll(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
        fn poll_interval(&self) -> Duration {
            Duration::from_millis(5)
        }
        fn bytes_sent(&self) -> u64 {
            self.sent * 100
        }
        fn close(&mut self, _: Duration) {
            self.log.0.lock().unwrap().push("close".into());
        }
    }

    struct MockConnector {
        log: Log,
        connects: Arc<AtomicU32>,
        /// n 回目の接続（0 から）の振る舞い。
        plan: Vec<Result<Option<u64>, TransportErrorKind>>,
    }

    impl Connector for MockConnector {
        fn connect(&mut self, _: Duration) -> Result<Box<dyn Transport>, TransportError> {
            let n = self.connects.fetch_add(1, Ordering::SeqCst) as usize;
            match self.plan.get(n).cloned().unwrap_or(Ok(None)) {
                Ok(fail_after) => {
                    self.log.0.lock().unwrap().push("connect".into());
                    Ok(Box::new(MockTransport {
                        log: self.log.clone(),
                        sent: 0,
                        fail_after,
                    }))
                }
                Err(kind) => Err(TransportError::new(kind, "refused")),
            }
        }
    }

    fn cfg() -> SenderConfig {
        let mut c = SenderConfig::new(Endpoint::Rtmp {
            url: "rtmp://h/live".into(),
            key: crate::Secret::new("k"),
        });
        c.reconnect_initial = Duration::from_millis(10);
        c.startup_hold = Duration::from_millis(20);
        c
    }

    fn frame(data: &[u8], dts: i64) -> VideoFrame<'_> {
        VideoFrame {
            codec: VideoCodec::H264,
            format: VideoFormat::AnnexB,
            data,
            pts_us: dts,
            dts_us: dts,
            keyframe: None,
        }
    }

    fn wait_until(f: impl Fn() -> bool) {
        let t = Instant::now();
        while !f() {
            assert!(t.elapsed() < Duration::from_secs(5), "timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn reconnects_and_restarts_from_keyframe() {
        let log = Log::default();
        let connects = Arc::new(AtomicU32::new(0));
        // 1 回目は拒否ではない失敗、2 回目は 2 つ送って失敗、3 回目は正常
        let conn = MockConnector {
            log: log.clone(),
            connects: connects.clone(),
            plan: vec![Err(TransportErrorKind::Network), Ok(Some(2)), Ok(None)],
        };
        let s = Sender::with_connector(cfg(), Box::new(conn));
        let events = Arc::new(Mutex::new(Vec::new()));
        let ev2 = events.clone();
        s.set_event_handler(move |e| ev2.lock().unwrap().push(e.clone()))
            .unwrap();
        let idr = annexb(&[SPS, PPS, &[0x65, 1]]);
        let p = annexb(&[&[0x41, 2]]);
        s.start().unwrap();
        s.push_video(&frame(&idr, 0)).unwrap();
        s.push_video(&frame(&p, 33)).unwrap();
        s.push_video(&frame(&p, 66)).unwrap(); // ここで 2 回目の接続が切れる
        wait_until(|| connects.load(Ordering::SeqCst) >= 3);
        wait_until(|| s.state() == SenderState::Connected);
        // つなぎ直した後の P フレームはキーフレームまで捨てる
        s.push_video(&frame(&p, 99)).unwrap();
        s.push_video(&frame(&idr, 132)).unwrap();
        s.stop();
        let l = log.0.lock().unwrap().clone();
        let tail: Vec<&str> = l
            .iter()
            .map(|s| s.as_str())
            .skip_while(|x| *x != "connect")
            .skip(1)
            .skip_while(|x| *x != "connect")
            .collect();
        assert_eq!(
            tail,
            vec!["connect", "config h264 1", "au h264 132 true", "close"],
            "{l:?}"
        );
        assert!(s.stats().reconnects >= 2);
        let ev = events.lock().unwrap();
        assert!(ev.contains(&Event::KeyframeRequested));
        assert_eq!(
            ev.last(),
            Some(&Event::StateChanged {
                state: SenderState::Stopped,
                reason: None
            })
        );
        assert!(matches!(
            s.push_video(&frame(&p, 200)),
            Err(Error::InvalidState(_))
        ));
    }

    #[test]
    fn rejection_is_fatal() {
        let conn = MockConnector {
            log: Log::default(),
            connects: Arc::new(AtomicU32::new(0)),
            plan: vec![Err(TransportErrorKind::Rejected)],
        };
        let s = Sender::with_connector(cfg(), Box::new(conn));
        s.start().unwrap();
        wait_until(|| s.state() == SenderState::Failed);
        s.stop();
        assert_eq!(s.state(), SenderState::Failed);
    }

    #[test]
    fn whip_and_rtmps_without_tls_unsupported() {
        let e = Sender::new(
            SenderConfig::from_uri("senca://ingest?v=1&proto=whip&url=https://h/whip/x").unwrap(),
        )
        .unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)));
        let e = Sender::new(SenderConfig::from_uri("rtmps://h/live/k").unwrap()).unwrap_err();
        assert_eq!(e, Error::Unsupported("RTMPS needs a TLS provider"));
    }
}
