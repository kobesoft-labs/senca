//! 1 本の接続の状態機械（sans-io）。
//!
//! 使い方（アダプターがすること）:
//!
//! 1. [`Connection::new`] で作る（Hello を積む）。
//! 2. 受けたバイトを `BytesMut` に足して [`Connection::handle_input`]（枠は受信バッファの切り出し）。
//! 3. [`Connection::poll_event`] で出来事を取り出して配る。
//! 4. [`Connection::poll_transmit`] で送る断片（`Bytes` の列）を取り出し、まとめ書き（writev）する。
//!    書き終えたら次を取る（書けない間は送りの順番の中で待たせ、詰まれば優先度の低い物から捨てる）。
//! 5. [`Connection::poll_timeout`] の時刻に [`Connection::handle_timeout`]（Ping・時間切れ・統計）。
//!
//! 購読は両方向にできる。自分が購読した物（上流）は [`UpstreamId`]、相手に購読された物（下流）は
//! [`DownstreamId`] で指す。

use crate::NodeId;
use crate::event::{LostReason, StreamEvent, TrackSel};
use crate::sched::{Out, SchedConfig, SchedStats, Scheduler};
use crate::wire::{
    self, BatchBuilder, ChannelEvent, ControlOp, ErrorCode, Frame, Hello, MAX_BATCH_ITEMS,
    MediaItem, PROTOCOL_VERSION, PeerStats, TrackCodec, WireError, Writer, caps,
};
use bytes::{Bytes, BytesMut};
use senca_core::{AccessUnit, CodecConfig, MediaKind, TrackId};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::time::{Duration, Instant};

/// 自分が購読したチャネル。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UpstreamId(pub u32);

/// 相手に購読されたチャネル。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DownstreamId(pub u32);

#[derive(Debug, Clone)]
pub struct ConnConfig {
    pub node_id: NodeId,
    pub capabilities: u64,
    /// 受ける枠の上限（相手に伝える）。
    pub max_frame: usize,
    pub ping_interval: Duration,
    /// これだけ何も届かなければ切れたとみなす。
    pub idle_timeout: Duration,
    pub hello_timeout: Duration,
    pub stats_interval: Duration,
    /// 向きごとのチャネルの上限。
    pub max_channels: usize,
    /// 1 チャネルのトラックの上限。
    pub max_tracks: usize,
    /// 1 回の poll_transmit で出す量の目安。
    pub transmit_max: usize,
    /// 制御の枠のキューの上限（相手が Ping・Subscribe を浴びせたら切る）。
    pub max_control_queue: usize,
    pub sched: SchedConfig,
}

impl Default for ConnConfig {
    fn default() -> Self {
        ConnConfig {
            node_id: 0,
            capabilities: caps::ALL,
            max_frame: wire::DEFAULT_MAX_FRAME,
            ping_interval: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(10),
            hello_timeout: Duration::from_secs(10),
            stats_interval: Duration::from_secs(1),
            max_channels: 4096,
            max_tracks: 64,
            transmit_max: 256 << 10,
            max_control_queue: 16384,
            sched: SchedConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub node_id: NodeId,
    pub version: u32,
    pub capabilities: u64,
    pub max_frame: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnError {
    Wire(WireError),
    Protocol(&'static str),
    Timeout,
    /// 相手の版が違う。
    Version(u32),
    /// 下の接続（TCP・TLS）が切れた。
    Transport(String),
    /// 自分から閉じた。
    Local(String),
}

impl fmt::Display for ConnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnError::Wire(e) => write!(f, "{e}"),
            ConnError::Protocol(m) => write!(f, "protocol violation: {m}"),
            ConnError::Timeout => f.write_str("peer timed out"),
            ConnError::Version(v) => write!(f, "peer protocol version {v} is not supported"),
            ConnError::Transport(m) => write!(f, "transport: {m}"),
            ConnError::Local(m) => write!(f, "closed: {m}"),
        }
    }
}

impl std::error::Error for ConnError {}

impl From<WireError> for ConnError {
    fn from(e: WireError) -> Self {
        match e {
            WireError::Version(v) => ConnError::Version(v),
            e => ConnError::Wire(e),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ConnEvent {
    /// 相手の Hello を受けた。
    Connected(PeerInfo),
    /// 上流の購読が受け付けられた。
    SubscribeOk(UpstreamId),
    /// 上流から届いた物。`Lost` で終わる（`Lost(GoingAway)` の後は流れが続く）。
    Stream(UpstreamId, StreamEvent),
    /// 相手が購読したい。[`Connection::accept`] か [`Connection::reject`] で答える。
    SubscribeRequest {
        id: DownstreamId,
        stream_id: String,
        tracks: TrackSel,
        priority: u8,
    },
    /// 相手が購読をやめた（または接続が切れた）。
    Unsubscribed(DownstreamId),
    /// 相手が受けるトラック（層）を変えた。`added` は新しく足したトラック（キーフレームから送る）。
    TracksChanged {
        id: DownstreamId,
        tracks: TrackSel,
        added: Vec<TrackId>,
    },
    /// キーフレームが欲しい（相手が頼んだ、または詰まって捨てたので次のキーフレームを待っている）。
    /// 統合側は送り手（エンコーダー・WebRTC の PLI など）へ伝えるとよい。
    KeyframeRequest { id: DownstreamId, track: TrackId },
    /// 相手が退く（新しい購読は受けない。今の購読は続く）。
    GoAway { code: ErrorCode, reason: String },
    /// 相手の統合側からの message（[`Connection::send_app`]）。
    App { topic: String, payload: Bytes },
    /// 接続が終わった。以後は何も起きない。
    Closed(ConnError),
}

/// 計測（配置の係が使う。ADR-0013）。
#[derive(Debug, Clone, Default)]
pub struct ConnStats {
    pub peer: Option<PeerInfo>,
    /// RTT（平滑化）。
    pub rtt: Option<Duration>,
    pub rtt_min: Option<Duration>,
    pub rtt_last: Option<Duration>,
    pub tx_bytes: u64,
    pub tx_frames: u64,
    /// poll_transmit が断片を返した回数（＝アダプターの書き込みの回数）。
    pub tx_writes: u64,
    pub tx_aus: u64,
    pub tx_payload_bytes: u64,
    pub rx_bytes: u64,
    pub rx_frames: u64,
    pub rx_aus: u64,
    pub rx_payload_bytes: u64,
    pub rx_gaps: u64,
    /// 直近の統計の間隔での送り・受けの速さ（bit/s）。
    pub tx_bps: u64,
    pub rx_bps: u64,
    /// 相手が数えた量。
    pub peer_stats: PeerStats,
    pub sched: SchedStats,
    pub upstreams: usize,
    pub downstreams: usize,
    pub peer_going_away: bool,
    pub going_away: bool,
}

impl ConnStats {
    /// 送った AU 1 つ当たりの見出しのバイト数（枠の見出し・制御の枠を含む）。
    pub fn tx_overhead_per_au(&self) -> f64 {
        if self.tx_aus == 0 {
            return 0.0;
        }
        (self.tx_bytes.saturating_sub(self.tx_payload_bytes)) as f64 / self.tx_aus as f64
    }
}

#[derive(Debug)]
struct UpChan {
    tracks: TrackSel,
    codecs: Vec<(TrackId, TrackCodec)>,
    last_seq: Vec<(TrackId, u64, u64)>,
}

#[derive(Debug)]
struct DownChan {
    tracks: TrackSel,
    priority: u8,
    accepted: bool,
    configs: Vec<(TrackId, CodecConfig)>,
    codecs: Vec<(TrackId, TrackCodec)>,
}

fn codec_slot(v: &mut Vec<(TrackId, TrackCodec)>, t: TrackId) -> &mut TrackCodec {
    let i = match v.iter().position(|x| x.0 == t) {
        Some(i) => i,
        None => {
            v.push((t, TrackCodec::default()));
            v.len() - 1
        }
    };
    &mut v[i].1
}

/// 1 本の接続。
#[derive(Debug)]
pub struct Connection {
    cfg: ConnConfig,
    t0: Instant,
    peer: Option<PeerInfo>,
    ctrl: VecDeque<Frame>,
    sched: Scheduler,
    writer: Writer,
    batch: BatchBuilder,
    events: VecDeque<ConnEvent>,
    up: HashMap<u32, UpChan>,
    down: HashMap<u32, DownChan>,
    /// 送り終えてから閉じる下流（積んだ End などを捨てない）: (チャネル, 理由, 期限)。
    closing: Vec<(u32, ErrorCode, String, Instant)>,
    next_up: u32,
    closed: Option<ConnError>,
    /// 閉じた後、最後の GoAway を送り終えた。
    flushed: bool,
    going_away: bool,
    peer_going_away: bool,
    last_rx: Instant,
    next_ping: Instant,
    next_stats: Instant,
    stats: ConnStats,
    srtt: Option<f64>,
    win: (Instant, u64, u64),
    want: usize,
}

impl Connection {
    pub fn new(cfg: ConnConfig, now: Instant) -> Connection {
        let mut ctrl = VecDeque::new();
        ctrl.push_back(Frame::Hello(Hello {
            version: PROTOCOL_VERSION,
            node_id: cfg.node_id,
            capabilities: cfg.capabilities,
            max_frame: cfg.max_frame as u64,
        }));
        Connection {
            sched: Scheduler::new(cfg.sched.clone(), now),
            t0: now,
            peer: None,
            ctrl,
            writer: Writer::new(),
            batch: BatchBuilder::default(),
            events: VecDeque::new(),
            up: HashMap::new(),
            down: HashMap::new(),
            closing: Vec::new(),
            next_up: 1,
            closed: None,
            flushed: false,
            going_away: false,
            peer_going_away: false,
            last_rx: now,
            next_ping: now + cfg.ping_interval,
            next_stats: now + cfg.stats_interval,
            stats: ConnStats::default(),
            srtt: None,
            win: (now, 0, 0),
            want: 0,
            cfg,
        }
    }

    pub fn peer(&self) -> Option<&PeerInfo> {
        self.peer.as_ref()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.is_some()
    }

    /// 閉じて、最後の枠も送り終えた（アダプターは下の接続を閉じてよい）。
    pub fn is_finished(&self) -> bool {
        self.closed.is_some() && self.flushed
    }

    pub fn close_reason(&self) -> Option<&ConnError> {
        self.closed.as_ref()
    }

    pub fn peer_going_away(&self) -> bool {
        self.peer_going_away
    }

    pub fn scheduler(&mut self) -> &mut Scheduler {
        &mut self.sched
    }

    /// 次に読むべきバイト数の目安（大きな枠の途中なら、その残り）。
    pub fn read_hint(&self) -> usize {
        self.want
    }

    pub fn stats(&self) -> ConnStats {
        let mut s = self.stats.clone();
        s.peer = self.peer.clone();
        s.sched = self.sched.stats();
        s.upstreams = self.up.len();
        s.downstreams = self.down.len();
        s.going_away = self.going_away;
        s.peer_going_away = self.peer_going_away;
        s
    }

    pub fn poll_event(&mut self) -> Option<ConnEvent> {
        if self.events.is_empty() {
            self.collect_key_wanted();
        }
        self.events.pop_front()
    }

    fn collect_key_wanted(&mut self) {
        for (ch, track) in self.sched.take_key_wanted() {
            if self.down.contains_key(&ch) {
                self.events.push_back(ConnEvent::KeyframeRequest {
                    id: DownstreamId(ch),
                    track,
                });
            }
        }
    }

    fn push_ctrl(&mut self, f: Frame) {
        if self.closed.is_some() {
            return;
        }
        if self.ctrl.len() >= self.cfg.max_control_queue {
            self.fail(ConnError::Protocol("control queue overflow"));
            return;
        }
        self.ctrl.push_back(f);
    }

    /// 統合側の message を送る（チャネルに属さない。制御の枠と同じく、メディアより先に出る）。
    /// 相手が [`caps::APP`] を載せていない・閉じた・大きすぎる・制御のキューが半分を超えて詰まっているときは
    /// 送らずに false（呼び手は捨てるか、後で送り直す）。
    pub fn send_app(&mut self, topic: &str, payload: Bytes) -> bool {
        let Some(p) = &self.peer else { return false };
        let room = (p.max_frame as usize).saturating_sub(16 + wire::MAX_TOPIC);
        if self.closed.is_some()
            || p.capabilities & caps::APP == 0
            || topic.is_empty()
            || topic.len() > wire::MAX_TOPIC
            || payload.len() > wire::MAX_APP_PAYLOAD.min(room)
            || self.ctrl.len() >= self.cfg.max_control_queue / 2
        {
            return false;
        }
        self.ctrl.push_back(Frame::App {
            topic: topic.to_string(),
            payload,
        });
        true
    }

    // -----------------------------------------------------------------------------------------
    // 上流（自分が購読する）

    /// 相手の stream を購読する。相手が退いている・閉じているときは Err。
    pub fn subscribe(
        &mut self,
        stream_id: &str,
        tracks: TrackSel,
        priority: u8,
    ) -> Result<UpstreamId, LostReason> {
        if let Some(e) = &self.closed {
            return Err(LostReason::ConnectionLost(e.to_string()));
        }
        if self.peer_going_away {
            return Err(LostReason::GoingAway);
        }
        if self.up.len() >= self.cfg.max_channels {
            return Err(LostReason::Rejected {
                code: ErrorCode::OVERLOADED.0,
                reason: "too many channels".into(),
            });
        }
        if stream_id.len() > wire::MAX_STREAM_ID {
            return Err(LostReason::Rejected {
                code: ErrorCode::BAD_REQUEST.0,
                reason: "stream id too long".into(),
            });
        }
        let ch = self.next_up;
        self.next_up = self.next_up.checked_add(1).unwrap_or(1);
        self.up.insert(
            ch,
            UpChan {
                tracks: tracks.clone(),
                codecs: Vec::new(),
                last_seq: Vec::new(),
            },
        );
        self.push_ctrl(Frame::Subscribe {
            channel: ch,
            priority,
            stream_id: stream_id.to_string(),
            tracks,
        });
        Ok(UpstreamId(ch))
    }

    pub fn unsubscribe(&mut self, id: UpstreamId) {
        if self.up.remove(&id.0).is_some() {
            self.push_ctrl(Frame::Unsubscribe { channel: id.0 });
        }
    }

    /// 受けるトラック（サイマルキャストの層）を変える。
    pub fn set_tracks(&mut self, id: UpstreamId, tracks: TrackSel) {
        if let Some(u) = self.up.get_mut(&id.0)
            && u.tracks != tracks
        {
            u.tracks = tracks.clone();
            self.push_ctrl(Frame::Control {
                channel: id.0,
                op: ControlOp::SetTracks(tracks),
            });
        }
    }

    pub fn request_keyframe(&mut self, id: UpstreamId, track: TrackId) {
        if self.up.contains_key(&id.0) {
            self.push_ctrl(Frame::Control {
                channel: id.0,
                op: ControlOp::KeyframeRequest(track),
            });
        }
    }

    // -----------------------------------------------------------------------------------------
    // 下流（相手に購読された）

    /// 購読を受け付ける。`rate_bps` はこのチャネルの帯域の上限。
    pub fn accept(&mut self, id: DownstreamId, rate_bps: Option<u64>, now: Instant) {
        let Some(d) = self.down.get_mut(&id.0) else {
            return;
        };
        if d.accepted {
            return;
        }
        d.accepted = true;
        let prio = d.priority;
        self.sched.add_channel(id.0, prio, rate_bps, now);
        self.push_ctrl(Frame::SubscribeOk { channel: id.0 });
    }

    /// 購読を断る、または受け付けた購読を終える（stream が終わった・所有権が移った）。
    /// 積んである物を送り終えてから下流を閉じる（終わりの知らせを捨てない）。長く掛かるなら期限で閉じる。
    pub fn finish_downstream(&mut self, id: DownstreamId, code: ErrorCode, reason: &str, now: Instant) {
        if !self.down.contains_key(&id.0) || self.closing.iter().any(|c| c.0 == id.0) {
            return;
        }
        if self.sched.channel_pending(id.0) {
            self.closing.push((id.0, code, reason.to_string(), now + Duration::from_secs(3)));
        } else {
            self.reject(id, code, reason);
        }
    }

    fn close_drained(&mut self, now: Instant) {
        if self.closing.is_empty() {
            return;
        }
        let ready: Vec<(u32, ErrorCode, String)> = self
            .closing
            .iter()
            .filter(|c| !self.sched.channel_pending(c.0) || now >= c.3)
            .map(|c| (c.0, c.1, c.2.clone()))
            .collect();
        if ready.is_empty() {
            return;
        }
        self.closing.retain(|c| !ready.iter().any(|r| r.0 == c.0));
        for (ch, code, reason) in ready {
            self.flush_batch();
            self.reject(DownstreamId(ch), code, &reason);
        }
        while let Some(f) = self.ctrl.pop_front() {
            self.writer.control(&f);
            self.stats.tx_frames += 1;
        }
    }

    pub fn reject(&mut self, id: DownstreamId, code: ErrorCode, reason: &str) {
        self.closing.retain(|c| c.0 != id.0);
        if self.down.remove(&id.0).is_some() {
            self.sched.remove_channel(id.0);
            self.push_ctrl(Frame::SubscribeErr {
                channel: id.0,
                code,
                reason: reason.to_string(),
            });
        }
    }

    pub fn downstream_tracks(&self, id: DownstreamId) -> Option<&TrackSel> {
        self.down.get(&id.0).map(|d| &d.tracks)
    }

    pub fn set_downstream_rate(&mut self, id: DownstreamId, rate_bps: Option<u64>, now: Instant) {
        self.sched.set_channel_rate(id.0, rate_bps, now);
    }

    pub fn set_total_rate(&mut self, rate_bps: Option<u64>, now: Instant) {
        self.sched.set_total_rate(rate_bps, now);
    }

    /// 設定を送る（選ばれていないトラックは覚えておき、層が足されたときに送る）。
    pub fn send_config(
        &mut self,
        id: DownstreamId,
        track: TrackId,
        config: CodecConfig,
        now: Instant,
    ) -> bool {
        let Some(d) = self.down.get_mut(&id.0) else {
            return false;
        };
        if !d.accepted {
            return false;
        }
        match d.configs.iter_mut().find(|c| c.0 == track) {
            Some(c) => c.1 = config.clone(),
            None => {
                if d.configs.len() >= self.cfg.max_tracks {
                    return false;
                }
                d.configs.push((track, config.clone()));
            }
        }
        if d.tracks.contains(track) {
            self.sched.push_config(id.0, track, config, now);
        }
        true
    }

    /// AU を送る（選ばれていないトラックは捨てる）。積まなかったら false。
    pub fn send_au(&mut self, id: DownstreamId, au: AccessUnit, now: Instant) -> bool {
        let Some(d) = self.down.get(&id.0) else {
            return false;
        };
        if !d.accepted || !d.tracks.contains(au.track) {
            return false;
        }
        self.sched.push_au(id.0, au, now)
    }

    pub fn send_channel_event(
        &mut self,
        id: DownstreamId,
        event: ChannelEvent,
        now: Instant,
    ) -> bool {
        let Some(d) = self.down.get(&id.0) else {
            return false;
        };
        if !d.accepted {
            return false;
        }
        self.sched.push_event(id.0, event, now);
        true
    }

    /// hub の出来事をそのまま送る（`Lost` は送らない）。
    pub fn send(&mut self, id: DownstreamId, ev: &StreamEvent, now: Instant) -> bool {
        match ev {
            StreamEvent::Config(t, c) => self.send_config(id, *t, c.clone(), now),
            StreamEvent::Au(a) => self.send_au(id, a.clone(), now),
            StreamEvent::Discontinuity => {
                self.send_channel_event(id, ChannelEvent::Discontinuity, now)
            }
            StreamEvent::End => self.send_channel_event(id, ChannelEvent::End, now),
            StreamEvent::Segment {
                seq,
                start_us,
                duration_us,
                discontinuity,
            } => self.send_channel_event(
                id,
                ChannelEvent::Segment {
                    seq: *seq,
                    start_us: *start_us,
                    duration_us: *duration_us,
                    discontinuity: *discontinuity,
                },
                now,
            ),
            StreamEvent::Lost(_) => false,
        }
    }

    // -----------------------------------------------------------------------------------------
    // 接続

    /// 退く: 新しい購読を断り、今の購読は続ける（統合側は流れが終わったら閉じる）。
    pub fn go_away(&mut self, code: ErrorCode, reason: &str) {
        if self.going_away || self.closed.is_some() {
            return;
        }
        self.going_away = true;
        self.push_ctrl(Frame::GoAway {
            code,
            reason: reason.to_string(),
        });
    }

    /// 閉じる（GoAway を送ってから）。
    pub fn close(&mut self, reason: &str) {
        self.fail(ConnError::Local(reason.to_string()));
    }

    /// 下の接続が切れた（EOF・I/O の誤り）。
    pub fn transport_closed(&mut self, reason: &str) {
        self.fail(ConnError::Transport(reason.to_string()));
        self.ctrl.clear();
        self.flushed = true;
    }

    fn fail(&mut self, e: ConnError) {
        if self.closed.is_some() {
            return;
        }
        let code = match &e {
            ConnError::Wire(_) | ConnError::Protocol(_) => ErrorCode::PROTOCOL,
            ConnError::Version(_) => ErrorCode::VERSION,
            ConnError::Timeout => ErrorCode::TIMEOUT,
            _ => ErrorCode::NORMAL,
        };
        self.ctrl.clear();
        self.ctrl.push_back(Frame::GoAway {
            code,
            reason: e.to_string(),
        });
        let reason = e.to_string();
        let mut ups: Vec<u32> = self.up.drain().map(|(k, _)| k).collect();
        ups.sort_unstable();
        for ch in ups {
            self.events.push_back(ConnEvent::Stream(
                UpstreamId(ch),
                StreamEvent::Lost(LostReason::ConnectionLost(reason.clone())),
            ));
        }
        let mut downs: Vec<u32> = self.down.drain().map(|(k, _)| k).collect();
        downs.sort_unstable();
        for ch in downs {
            self.sched.remove_channel(ch);
            self.events
                .push_back(ConnEvent::Unsubscribed(DownstreamId(ch)));
        }
        self.events.push_back(ConnEvent::Closed(e.clone()));
        self.closed = Some(e);
    }

    // -----------------------------------------------------------------------------------------
    // 時間

    pub fn poll_timeout(&self, now: Instant) -> Option<Instant> {
        if self.closed.is_some() {
            return None;
        }
        let mut t = self
            .next_ping
            .min(self.next_stats)
            .min(self.last_rx + self.cfg.idle_timeout);
        if self.peer.is_none() {
            t = t.min(self.t0 + self.cfg.hello_timeout);
        }
        if let Some(w) = self.sched.next_wakeup(now) {
            t = t.min(w);
        }
        if let Some(c) = self.closing.iter().map(|c| c.3).min() {
            t = t.min(c);
        }
        Some(t)
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        if self.closed.is_some() {
            return;
        }
        if self.peer.is_none() && now >= self.t0 + self.cfg.hello_timeout {
            self.fail(ConnError::Timeout);
            return;
        }
        if now.saturating_duration_since(self.last_rx) >= self.cfg.idle_timeout {
            self.fail(ConnError::Timeout);
            return;
        }
        if now >= self.next_ping {
            let nonce = now.saturating_duration_since(self.t0).as_micros() as u64;
            self.push_ctrl(Frame::Ping(nonce));
            self.next_ping = now + self.cfg.ping_interval;
        }
        if now >= self.next_stats {
            let dt = now
                .saturating_duration_since(self.win.0)
                .as_secs_f64()
                .max(1e-3);
            self.stats.tx_bps = ((self.stats.tx_bytes - self.win.1) as f64 * 8.0 / dt) as u64;
            self.stats.rx_bps = ((self.stats.rx_bytes - self.win.2) as f64 * 8.0 / dt) as u64;
            self.win = (now, self.stats.tx_bytes, self.stats.rx_bytes);
            self.next_stats = now + self.cfg.stats_interval;
            if self
                .peer
                .as_ref()
                .is_some_and(|p| p.capabilities & caps::STATS != 0)
            {
                self.push_ctrl(Frame::Stats(PeerStats {
                    rx_bytes: self.stats.rx_bytes,
                    rx_aus: self.stats.rx_aus,
                    rx_gaps: self.stats.rx_gaps,
                }));
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // 送り

    /// 送る断片を `out` に足す（全部書いてから次を呼ぶ）。足したバイト数を返す。
    pub fn poll_transmit(&mut self, now: Instant, out: &mut Vec<Bytes>) -> usize {
        if self.flushed {
            return 0;
        }
        while let Some(f) = self.ctrl.pop_front() {
            self.writer.control(&f);
            self.stats.tx_frames += 1;
        }
        if self.closed.is_some() {
            self.flushed = true;
        } else if self.peer.is_some() && self.sched.hold_until(now).is_none() {
            self.pull_media(now);
        }
        if self.closed.is_none() {
            self.close_drained(now);
        }
        let n = self.writer.take(out);
        if n > 0 {
            self.stats.tx_bytes += n as u64;
            self.stats.tx_writes += 1;
        }
        n
    }

    fn pull_media(&mut self, now: Instant) {
        let budget = self.cfg.transmit_max;
        let batching = self
            .peer
            .as_ref()
            .is_some_and(|p| p.capabilities & caps::BATCH != 0);
        let peer_max = self
            .peer
            .as_ref()
            .map_or(wire::DEFAULT_MAX_FRAME, |p| p.max_frame as usize);
        let item_max = self.sched.config().batch_item_max;
        let batch_max = self.sched.config().batch_max_bytes;
        while self.writer.pending_bytes() + self.batch.body_len() < budget {
            let Some(o) = self.sched.pop(now) else { break };
            match o {
                Out::Event { channel, event } => {
                    self.flush_batch();
                    self.writer.control(&Frame::Event { channel, event });
                    self.stats.tx_frames += 1;
                }
                Out::Config {
                    channel,
                    track,
                    config,
                } => {
                    self.flush_batch();
                    self.writer.control(&Frame::Config {
                        channel,
                        track,
                        config,
                    });
                    self.stats.tx_frames += 1;
                }
                Out::Au { channel, au, gap } => {
                    let len = au.payload.len();
                    if !self.down.contains_key(&channel) {
                        continue;
                    }
                    if len + 128 > peer_max {
                        // 相手が受けられない大きさ: 捨てたうえで、映像は次のキーフレームまで送らない（壊れた GOP を送らない）
                        if au.codec.kind() == senca_core::MediaKind::Video {
                            self.sched.skip_to_key(channel, au.track);
                        }
                        continue;
                    }
                    let small = batching && len <= item_max;
                    if !small
                        || self.batch.body_len() + len + 40 > batch_max
                        || self.batch.len() >= MAX_BATCH_ITEMS
                    {
                        self.flush_batch();
                    }
                    let d = self.down.get_mut(&channel).expect("checked");
                    let st = codec_slot(&mut d.codecs, au.track);
                    if small {
                        self.batch.push(channel, au.track, st, &au, gap);
                    } else {
                        self.writer.media(channel, au.track, st, &au, gap);
                        self.stats.tx_frames += 1;
                    }
                    self.stats.tx_aus += 1;
                    self.stats.tx_payload_bytes += len as u64;
                }
            }
        }
        self.flush_batch();
    }

    fn flush_batch(&mut self) {
        if !self.batch.is_empty() {
            self.writer.batch(&mut self.batch);
            self.stats.tx_frames += 1;
        }
    }

    // -----------------------------------------------------------------------------------------
    // 受け

    /// 受けたバイトを処理する。読めた枠は `buf` から切り離す（payload はその切り出し）。
    pub fn handle_input(&mut self, buf: &mut BytesMut, now: Instant) {
        if self.closed.is_some() {
            buf.clear();
            return;
        }
        if !buf.is_empty() {
            self.last_rx = now;
        }
        if self.peer.is_none() && buf.first().is_some_and(|&t| t != wire::kind::HELLO) {
            return self.fail(ConnError::Protocol("first frame must be Hello"));
        }
        loop {
            let (t, h, len) = match wire::parse_frame_header(buf, self.cfg.max_frame) {
                Err(e) => return self.fail(e.into()),
                Ok(None) => {
                    self.want = 0;
                    return;
                }
                Ok(Some(x)) => x,
            };
            if buf.len() < h + len {
                self.want = h + len - buf.len();
                return;
            }
            let frame = buf.split_to(h + len).freeze();
            self.stats.rx_bytes += (h + len) as u64;
            self.stats.rx_frames += 1;
            let body = frame.slice(h..);
            let f = match wire::decode_body(t, &body) {
                Ok(f) => f,
                Err(e) => return self.fail(e.into()),
            };
            if let Err(e) = self.on_frame(f, now) {
                return self.fail(e);
            }
            if self.closed.is_some() {
                buf.clear();
                return;
            }
        }
    }

    fn on_frame(&mut self, f: Frame, now: Instant) -> Result<(), ConnError> {
        if self.peer.is_none() {
            let Frame::Hello(h) = f else {
                return Err(ConnError::Protocol("first frame must be Hello"));
            };
            if h.version != PROTOCOL_VERSION {
                return Err(ConnError::Version(h.version));
            }
            if (h.max_frame as usize) < wire::MIN_MAX_FRAME {
                return Err(ConnError::Protocol("max_frame too small"));
            }
            let p = PeerInfo {
                node_id: h.node_id,
                version: h.version,
                capabilities: h.capabilities,
                max_frame: h.max_frame,
            };
            self.peer = Some(p.clone());
            self.events.push_back(ConnEvent::Connected(p));
            return Ok(());
        }
        match f {
            Frame::Hello(_) => return Err(ConnError::Protocol("duplicate Hello")),
            Frame::GoAway { code, reason } => {
                if !self.peer_going_away {
                    self.peer_going_away = true;
                    let mut ups: Vec<u32> = self.up.keys().copied().collect();
                    ups.sort_unstable();
                    for ch in ups {
                        self.events.push_back(ConnEvent::Stream(
                            UpstreamId(ch),
                            StreamEvent::Lost(LostReason::GoingAway),
                        ));
                    }
                    self.events.push_back(ConnEvent::GoAway { code, reason });
                }
            }
            Frame::Ping(n) => self.push_ctrl(Frame::Pong(n)),
            Frame::Pong(n) => {
                let sent = self.t0 + Duration::from_micros(n);
                if sent <= now {
                    let rtt = now - sent;
                    let s = match self.srtt {
                        None => rtt.as_secs_f64(),
                        Some(s) => s * 0.875 + rtt.as_secs_f64() * 0.125,
                    };
                    self.srtt = Some(s);
                    self.stats.rtt = Some(Duration::from_secs_f64(s));
                    self.stats.rtt_last = Some(rtt);
                    self.stats.rtt_min = Some(self.stats.rtt_min.map_or(rtt, |m| m.min(rtt)));
                }
            }
            Frame::Subscribe {
                channel,
                priority,
                stream_id,
                tracks,
            } => {
                if self.down.contains_key(&channel) {
                    return Err(ConnError::Protocol("channel reused"));
                }
                if self.going_away {
                    self.push_ctrl(Frame::SubscribeErr {
                        channel,
                        code: ErrorCode::GOING_AWAY,
                        reason: "node is draining".into(),
                    });
                } else if self.down.len() >= self.cfg.max_channels {
                    self.push_ctrl(Frame::SubscribeErr {
                        channel,
                        code: ErrorCode::OVERLOADED,
                        reason: "too many channels".into(),
                    });
                } else {
                    self.down.insert(
                        channel,
                        DownChan {
                            tracks: tracks.clone(),
                            priority,
                            accepted: false,
                            configs: Vec::new(),
                            codecs: Vec::new(),
                        },
                    );
                    self.events.push_back(ConnEvent::SubscribeRequest {
                        id: DownstreamId(channel),
                        stream_id,
                        tracks,
                        priority,
                    });
                }
            }
            Frame::SubscribeOk { channel } => {
                if self.up.contains_key(&channel) {
                    self.events
                        .push_back(ConnEvent::SubscribeOk(UpstreamId(channel)));
                }
            }
            Frame::SubscribeErr {
                channel,
                code,
                reason,
            } => {
                if self.up.remove(&channel).is_some() {
                    self.events.push_back(ConnEvent::Stream(
                        UpstreamId(channel),
                        StreamEvent::Lost(LostReason::Rejected {
                            code: code.0,
                            reason,
                        }),
                    ));
                }
            }
            Frame::Unsubscribe { channel } => {
                if self.down.remove(&channel).is_some() {
                    self.sched.remove_channel(channel);
                    self.events
                        .push_back(ConnEvent::Unsubscribed(DownstreamId(channel)));
                }
            }
            Frame::Config {
                channel,
                track,
                config,
            } => {
                if self.up.contains_key(&channel) {
                    self.events.push_back(ConnEvent::Stream(
                        UpstreamId(channel),
                        StreamEvent::Config(track, config),
                    ));
                }
            }
            Frame::Media(m) => self.on_media(m)?,
            Frame::Batch(items) => {
                for m in items {
                    self.on_media(m)?;
                }
            }
            Frame::Control { channel, op } => {
                let Some(d) = self.down.get_mut(&channel) else {
                    return Ok(());
                };
                match op {
                    ControlOp::KeyframeRequest(track) => {
                        self.events.push_back(ConnEvent::KeyframeRequest {
                            id: DownstreamId(channel),
                            track,
                        });
                    }
                    ControlOp::SetTracks(sel) => {
                        let prev = std::mem::replace(&mut d.tracks, sel.clone());
                        // 足したトラック: 覚えた設定を送り、キーフレームから
                        let known: Vec<TrackId> = d.configs.iter().map(|c| c.0).collect();
                        let added: Vec<TrackId> = match sel.added_since(&prev) {
                            Some(a) => a,
                            None => known
                                .iter()
                                .copied()
                                .filter(|t| sel.contains(*t) && !prev.contains(*t))
                                .collect(),
                        };
                        let removed: Vec<TrackId> = known
                            .iter()
                            .copied()
                            .filter(|t| prev.contains(*t) && !sel.contains(*t))
                            .collect();
                        let cfgs: Vec<(TrackId, CodecConfig)> = d
                            .configs
                            .iter()
                            .filter(|c| added.contains(&c.0))
                            .cloned()
                            .collect();
                        let accepted = d.accepted;
                        for t in &removed {
                            self.sched.remove_track(channel, *t);
                        }
                        if accepted {
                            for (t, c) in cfgs {
                                let video = c.codec.kind() == MediaKind::Video;
                                self.sched.push_config(channel, t, c, now);
                                if video {
                                    self.sched.require_key(channel, t);
                                    self.events.push_back(ConnEvent::KeyframeRequest {
                                        id: DownstreamId(channel),
                                        track: t,
                                    });
                                }
                            }
                        }
                        self.events.push_back(ConnEvent::TracksChanged {
                            id: DownstreamId(channel),
                            tracks: sel,
                            added,
                        });
                    }
                }
            }
            Frame::Event { channel, event } => {
                if self.up.contains_key(&channel) {
                    let ev = match event {
                        ChannelEvent::Discontinuity => StreamEvent::Discontinuity,
                        ChannelEvent::End => StreamEvent::End,
                        ChannelEvent::Segment {
                            seq,
                            start_us,
                            duration_us,
                            discontinuity,
                        } => StreamEvent::Segment {
                            seq,
                            start_us,
                            duration_us,
                            discontinuity,
                        },
                    };
                    self.events
                        .push_back(ConnEvent::Stream(UpstreamId(channel), ev));
                }
            }
            Frame::Stats(s) => self.stats.peer_stats = s,
            Frame::App { topic, payload } => {
                self.events.push_back(ConnEvent::App { topic, payload });
            }
            Frame::Unknown(_) => {}
        }
        Ok(())
    }

    fn on_media(&mut self, m: MediaItem) -> Result<(), ConnError> {
        // 購読をやめた後に届いた物は捨てる（チャネル番号は使い回さないので混ざらない）
        let Some(u) = self.up.get_mut(&m.channel) else {
            return Ok(());
        };
        if !u.codecs.iter().any(|c| c.0 == m.track) && u.codecs.len() >= self.cfg.max_tracks {
            return Err(ConnError::Protocol("too many tracks"));
        }
        let len = m.payload.len();
        let au = codec_slot(&mut u.codecs, m.track).decode(m.track, &m.hdr, m.payload)?;
        // sequence の飛び（送り手が捨てた）
        match u.last_seq.iter_mut().find(|x| x.0 == au.track) {
            Some(x) => {
                if x.1 == au.epoch && au.sequence != x.2.wrapping_add(1) {
                    self.stats.rx_gaps += 1;
                }
                x.1 = au.epoch;
                x.2 = au.sequence;
            }
            None => u.last_seq.push((au.track, au.epoch, au.sequence)),
        }
        self.stats.rx_aus += 1;
        self.stats.rx_payload_bytes += len as u64;
        self.events.push_back(ConnEvent::Stream(
            UpstreamId(m.channel),
            StreamEvent::Au(au),
        ));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use senca_core::{Codec, Timebase};

    fn pair(now: Instant) -> (Connection, Connection) {
        // 時計を進めない試験なので、まとめの待ちは 0
        let sched = SchedConfig {
            batch_window: Duration::ZERO,
            ..Default::default()
        };
        let a = Connection::new(
            ConnConfig {
                node_id: 1,
                sched: sched.clone(),
                ..Default::default()
            },
            now,
        );
        let b = Connection::new(
            ConnConfig {
                node_id: 2,
                sched,
                ..Default::default()
            },
            now,
        );
        (a, b)
    }

    /// a → b へ全部流す。
    fn pump(a: &mut Connection, b: &mut Connection, now: Instant) -> usize {
        let mut total = 0;
        loop {
            let mut out = Vec::new();
            let n = a.poll_transmit(now, &mut out);
            if n == 0 {
                return total;
            }
            total += n;
            let mut buf = BytesMut::new();
            for c in out {
                buf.extend_from_slice(&c);
            }
            b.handle_input(&mut buf, now);
            assert!(buf.is_empty());
        }
    }

    fn exchange(a: &mut Connection, b: &mut Connection, now: Instant) {
        for _ in 0..4 {
            pump(a, b, now);
            pump(b, a, now);
        }
    }

    fn events(c: &mut Connection) -> Vec<ConnEvent> {
        let mut v = Vec::new();
        while let Some(e) = c.poll_event() {
            v.push(e);
        }
        v
    }

    fn au(track: u32, codec: Codec, seq: u64, key: bool, len: usize) -> AccessUnit {
        AccessUnit {
            track: TrackId(track),
            codec,
            epoch: 0,
            sequence: seq,
            config_generation: 1,
            pts: seq as i64 * 40,
            dts: seq as i64 * 40,
            duration: 0,
            timebase: Timebase::MILLIS,
            keyframe: key,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from(vec![seq as u8; len]),
        }
    }

    #[test]
    fn subscribe_and_stream() {
        let t0 = Instant::now();
        let (mut a, mut b) = pair(t0);
        let up = a.subscribe("s1", TrackSel::All, 10).unwrap();
        exchange(&mut a, &mut b, t0);
        let ev = events(&mut b);
        assert!(matches!(&ev[0], ConnEvent::Connected(p) if p.node_id == 1));
        let ConnEvent::SubscribeRequest { id, stream_id, .. } = &ev[1] else {
            panic!("{ev:?}")
        };
        assert_eq!(stream_id, "s1");
        let id = *id;
        b.accept(id, None, t0);
        let cfg = CodecConfig {
            codec: Codec::H264,
            generation: 1,
            record: Bytes::from_static(b"avcc"),
            video: None,
            audio: None,
        };
        b.send_config(id, TrackId(0), cfg.clone(), t0);
        for i in 0..20 {
            b.send_au(id, au(0, Codec::H264, i, i.is_multiple_of(10), 3000), t0);
            b.send_au(id, au(1, Codec::Aac, i, true, 200), t0);
        }
        b.send(id, &StreamEvent::End, t0);
        exchange(&mut a, &mut b, t0);
        let ev = events(&mut a);
        assert!(matches!(&ev[0], ConnEvent::Connected(p) if p.node_id == 2));
        assert!(matches!(&ev[1], ConnEvent::SubscribeOk(u) if *u == up));
        let mut n_au = 0;
        let mut saw_end = false;
        for e in &ev[2..] {
            match e {
                ConnEvent::Stream(_, StreamEvent::Config(_, c)) => assert_eq!(c, &cfg),
                ConnEvent::Stream(_, StreamEvent::Au(_)) => n_au += 1,
                ConnEvent::Stream(_, StreamEvent::End) => saw_end = true,
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(n_au, 40);
        assert!(saw_end);
        assert_eq!(a.stats().rx_gaps, 0);
    }

    #[test]
    fn layer_change_sends_config_and_waits_for_key() {
        let t0 = Instant::now();
        let (mut a, mut b) = pair(t0);
        let up = a.subscribe("s", TrackSel::only([TrackId(0)]), 0).unwrap();
        exchange(&mut a, &mut b, t0);
        let id = events(&mut b)
            .iter()
            .find_map(|e| {
                if let ConnEvent::SubscribeRequest { id, .. } = e {
                    Some(*id)
                } else {
                    None
                }
            })
            .unwrap();
        b.accept(id, None, t0);
        let cfg = |g| CodecConfig {
            codec: Codec::H264,
            generation: g,
            record: Bytes::new(),
            video: None,
            audio: None,
        };
        b.send_config(id, TrackId(0), cfg(1), t0);
        b.send_config(id, TrackId(1), cfg(1), t0);
        b.send_au(id, au(1, Codec::H264, 0, true, 10), t0);
        exchange(&mut a, &mut b, t0);
        let ev = events(&mut a);
        assert!(!ev.iter().any(
            |e| matches!(e, ConnEvent::Stream(_, StreamEvent::Au(x)) if x.track == TrackId(1))
        ));
        a.set_tracks(up, TrackSel::only([TrackId(0), TrackId(1)]));
        exchange(&mut a, &mut b, t0);
        let ev = events(&mut b);
        assert!(ev.iter().any(|e| matches!(
            e,
            ConnEvent::KeyframeRequest {
                track: TrackId(1),
                ..
            }
        )));
        assert!(ev.iter().any(
            |e| matches!(e, ConnEvent::TracksChanged { added, .. } if added == &[TrackId(1)])
        ));
        // 差分は捨て、キーフレームから
        assert!(!b.send_au(id, au(1, Codec::H264, 1, false, 10), t0));
        assert!(b.send_au(id, au(1, Codec::H264, 2, true, 10), t0));
        exchange(&mut a, &mut b, t0);
        let ev = events(&mut a);
        assert!(matches!(
            &ev[0],
            ConnEvent::Stream(_, StreamEvent::Config(TrackId(1), _))
        ));
        assert!(
            matches!(&ev[1], ConnEvent::Stream(_, StreamEvent::Au(x)) if x.sequence == 2 && x.keyframe && x.discontinuity)
        );
    }

    #[test]
    fn app_messages() {
        let now = Instant::now();
        let (mut a, mut b) = pair(now);
        // Hello の前は送れない
        assert!(!a.send_app("senca.rooms/1", Bytes::from_static(b"early")));
        exchange(&mut a, &mut b, now);
        events(&mut a);
        events(&mut b);
        assert!(a.send_app("senca.rooms/1", Bytes::from_static(b"{\"v\":1}")));
        assert!(!a.send_app("", Bytes::new()));
        assert!(!a.send_app(&"t".repeat(wire::MAX_TOPIC + 1), Bytes::new()));
        assert!(!a.send_app("t", Bytes::from(vec![0u8; wire::MAX_APP_PAYLOAD + 1])));
        exchange(&mut a, &mut b, now);
        let got: Vec<(String, Bytes)> = events(&mut b)
            .into_iter()
            .filter_map(|e| match e {
                ConnEvent::App { topic, payload } => Some((topic, payload)),
                _ => None,
            })
            .collect();
        assert_eq!(got, vec![("senca.rooms/1".to_string(), Bytes::from_static(b"{\"v\":1}"))]);
        // App を載せない古い相手には送らない
        let (_, mut d) = pair(now);
        let mut c = Connection::new(
            ConnConfig {
                node_id: 1,
                capabilities: caps::BATCH | caps::STATS,
                ..Default::default()
            },
            now,
        );
        exchange(&mut c, &mut d, now);
        assert!(!d.send_app("t", Bytes::from_static(b"x")));
        assert!(c.send_app("t", Bytes::from_static(b"x")));
    }

    #[test]
    fn goaway_and_reject() {
        let t0 = Instant::now();
        let (mut a, mut b) = pair(t0);
        let up = a.subscribe("s", TrackSel::All, 0).unwrap();
        exchange(&mut a, &mut b, t0);
        let id = events(&mut b)
            .iter()
            .find_map(|e| {
                if let ConnEvent::SubscribeRequest { id, .. } = e {
                    Some(*id)
                } else {
                    None
                }
            })
            .unwrap();
        b.accept(id, None, t0);
        b.go_away(ErrorCode::GOING_AWAY, "drain");
        exchange(&mut a, &mut b, t0);
        let ev = events(&mut a);
        assert!(ev.iter().any(|e| matches!(e, ConnEvent::Stream(u, StreamEvent::Lost(LostReason::GoingAway)) if *u == up)));
        assert!(ev.iter().any(|e| matches!(e, ConnEvent::GoAway { .. })));
        assert_eq!(
            a.subscribe("t", TrackSel::All, 0),
            Err(LostReason::GoingAway)
        );
        // 流れは続く
        b.send_au(id, au(1, Codec::Aac, 0, true, 10), t0);
        exchange(&mut a, &mut b, t0);
        assert!(
            events(&mut a)
                .iter()
                .any(|e| matches!(e, ConnEvent::Stream(_, StreamEvent::Au(_))))
        );
        // 所有権が移った: 終える
        b.reject(id, ErrorCode::MOVED, "moved");
        exchange(&mut a, &mut b, t0);
        assert!(events(&mut a).iter().any(|e| matches!(
            e,
            ConnEvent::Stream(_, StreamEvent::Lost(LostReason::Rejected { code: 2, .. }))
        )));
    }

    #[test]
    fn idle_timeout_reports_lost() {
        let t0 = Instant::now();
        let (mut a, mut b) = pair(t0);
        let up = a.subscribe("s", TrackSel::All, 0).unwrap();
        exchange(&mut a, &mut b, t0);
        events(&mut a);
        let t1 = t0 + Duration::from_secs(11);
        assert!(a.poll_timeout(t0).unwrap() <= t1);
        a.handle_timeout(t1);
        let ev = events(&mut a);
        assert!(
            matches!(&ev[0], ConnEvent::Stream(u, StreamEvent::Lost(LostReason::ConnectionLost(_))) if *u == up)
        );
        assert!(matches!(
            ev.last(),
            Some(ConnEvent::Closed(ConnError::Timeout))
        ));
        // GoAway を送ってから終わる
        let mut out = Vec::new();
        assert!(a.poll_transmit(t1, &mut out) > 0);
        assert!(a.is_finished());
    }

    #[test]
    fn rtt_from_ping() {
        let t0 = Instant::now();
        let (mut a, mut b) = pair(t0);
        exchange(&mut a, &mut b, t0);
        let t1 = t0 + Duration::from_secs(1);
        a.handle_timeout(t1);
        pump(&mut a, &mut b, t1);
        let t2 = t1 + Duration::from_millis(30);
        pump(&mut b, &mut a, t2);
        assert_eq!(a.stats().rtt_last, Some(Duration::from_millis(30)));
    }

    #[test]
    fn rejects_garbage_and_wrong_version() {
        let t0 = Instant::now();
        let mut a = Connection::new(ConnConfig::default(), t0);
        let mut buf = BytesMut::from(&b"GET / HTTP/1.1\r\n\r\n"[..]);
        a.handle_input(&mut buf, t0);
        assert!(a.is_closed());
        let mut a = Connection::new(ConnConfig::default(), t0);
        let mut v = Vec::new();
        wire::encode_frame(
            &Frame::Hello(Hello {
                version: 2,
                node_id: 1,
                capabilities: 0,
                max_frame: 1 << 20,
            }),
            &mut v,
        );
        let mut buf = BytesMut::from(&v[..]);
        a.handle_input(&mut buf, t0);
        assert_eq!(a.close_reason(), Some(&ConnError::Version(2)));
    }
}
