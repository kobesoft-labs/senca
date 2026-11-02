//! SCTP の association（RFC 9260 の WebRTC で使う部分、RFC 3758 PR-SCTP、RFC 6525 stream reset）。
//!
//! 送るパケットは [`Association::poll_transmit`] で取り出すときに組み立てる（その時刻は
//! 直前に渡された `now`）。受けたパケットは全体を読んで確かめてから状態を変える。

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes};

use crate::chunk::{
    self, Chunk, DataChunk, InitChunk, ReconfigParam, SackChunk, ct, reconfig_result as rr,
};
use crate::cookie::{CookieData, Rng};
use crate::reassembly::{DataOutcome, Receiver, RxMessage, extend, initial_ext};
use crate::{AssociationState, SctpConfig, SctpError, SctpEvent, SendOptions};

/// 遅延 SACK の時間（RFC 9260 6.2、200 ms 以下）。
const DELAYED_SACK: Duration = Duration::from_millis(200);
/// 未確認の DATA チャンクの数の上限。
const MAX_OUTSTANDING_CHUNKS: usize = 16384;
/// 組み立て済みで送り待ちのパケット（INIT-ACK など）の数の上限。
const MAX_RAW_TX: usize = 64;
/// 送り待ちの制御チャンクの数の上限。
const MAX_CTRL: usize = 256;
/// SACK に載せる gap block の数の上限。
const MAX_GAP_BLOCKS: usize = 64;
/// 受けた SACK のうち見る gap block の数の上限。
const MAX_RX_GAP_BLOCKS: usize = 256;
/// FORWARD-TSN に載せるストリームの数の上限。
const MAX_FWD_TSN_STREAMS: usize = 128;
/// 1 つの RE-CONFIG 要求に載せるストリームの数の上限。
const MAX_RECONFIG_STREAMS: usize = 256;
/// 応答する HEARTBEAT の情報の大きさの上限。
const MAX_HEARTBEAT_INFO: usize = 1024;
/// メッセージ以外の出来事の数の上限。
const MAX_EVENTS: usize = 65536;
/// 報告する未知のチャンクの大きさの上限。
const MAX_REPORTED_CHUNK: usize = 256;

fn is_established_like(s: AssociationState) -> bool {
    matches!(
        s,
        AssociationState::Established
            | AssociationState::ShutdownPending
            | AssociationState::ShutdownSent
            | AssociationState::ShutdownReceived
            | AssociationState::ShutdownAckSent
    )
}

/// 診断用の数値。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssociationStats {
    pub cwnd: usize,
    pub ssthresh: usize,
    pub flight_size: usize,
    pub peer_rwnd: usize,
    pub rto: Duration,
    pub srtt: Option<Duration>,
    /// 未確認の DATA チャンクの数。
    pub outstanding_chunks: usize,
    /// まだ TSN を割り当てていないメッセージの数。
    pub queued_messages: usize,
    /// 送信待ち（未送信 + 未確認）の総バイト数。
    pub buffered: usize,
    /// 受信側で持っているバイト数（組み立て中・順番待ち・未取り出し）。
    pub rx_buffered: usize,
    /// 受信側で持っている TSN・断片・順番待ちの数。
    pub rx_items: usize,
    pub error_count: u32,
    /// 送り出せるストリーム数・受けるストリーム数。
    pub outbound_streams: u16,
    pub inbound_streams: u16,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub data_chunks_sent: u64,
    pub retransmitted_chunks: u64,
    pub fast_retransmits: u64,
    pub t3_expirations: u64,
    pub abandoned_messages: u64,
    pub forward_tsn_sent: u64,
    pub forward_tsn_received: u64,
    pub heartbeats_acked: u64,
    pub errors_received: u64,
    pub reconfig_sent: u64,
}

struct OutMessage {
    msg_id: u64,
    stream: u16,
    ppid: u32,
    data: Bytes,
    offset: usize,
    unordered: bool,
    ssn: u16,
    max_rtx: Option<u32>,
    expiry: Option<Instant>,
}

struct OutChunk {
    tsn: u64,
    msg_id: u64,
    stream: u16,
    ssn: u16,
    ppid: u32,
    unordered: bool,
    begin: bool,
    end: bool,
    payload: Bytes,
    /// 送った回数。
    tx_count: u32,
    /// gap ack で確認された。
    acked: bool,
    /// 捨てた（PR-SCTP）。
    abandoned: bool,
    in_flight: bool,
    needs_rtx: bool,
    miss: u32,
    fast_rtx_done: bool,
    max_rtx: Option<u32>,
    expiry: Option<Instant>,
}

impl OutChunk {
    fn to_chunk(&self) -> Chunk {
        Chunk::Data(DataChunk {
            tsn: self.tsn as u32,
            stream: self.stream,
            ssn: self.ssn,
            ppid: self.ppid,
            unordered: self.unordered,
            begin: self.begin,
            end: self.end,
            immediate: false,
            payload: self.payload.clone(),
        })
    }
    fn len(&self) -> usize {
        self.payload.len()
    }
}

#[derive(Default)]
struct StreamOut {
    next_ssn: u16,
    /// この TSN までは前の世代（stream reset の前）の SSN。FORWARD-TSN に載せない。
    epoch_tsn: u64,
    buffered: usize,
    low_threshold: usize,
    resetting: bool,
}

struct ReconfRequest {
    seq: u32,
    streams: Vec<u16>,
    last_tsn: u32,
    /// 相手が「処理中」と答えた（再送しても誤りに数えない）。
    in_progress: bool,
}

struct DeferredReset {
    seq: u32,
    last_tsn: u64,
    streams: Vec<u16>,
}

/// SCTP の association。
pub struct Association {
    cfg: SctpConfig,
    now: Instant,
    epoch: Instant,
    rng: Rng,
    cookie_key: [u8; 32],
    state: AssociationState,
    /// 一度閉じた（以後は何も受けない）。
    dead: bool,
    local_tag: u32,
    peer_tag: u32,
    local_initial_tsn: u32,
    peer_forward_tsn: bool,
    peer_reconfig: bool,
    out_streams: u16,
    in_streams: u16,
    max_send_message_size: usize,

    // 開設
    init_packet: Option<Bytes>,
    cookie: Option<Bytes>,
    cookie_echo_pending: bool,
    cookie_sent_at: Option<Instant>,
    cookie_ack_pending: bool,
    t1: Option<Instant>,
    t1_rto: Duration,
    t1_count: u32,

    // 送信
    next_tsn: u64,
    cum_ack: u64,
    adv_peer_ack: u64,
    send_queue: VecDeque<OutMessage>,
    outstanding: VecDeque<OutChunk>,
    streams_out: HashMap<u16, StreamOut>,
    buffered_total: usize,
    next_msg_id: u64,
    cwnd: usize,
    ssthresh: usize,
    flight_size: usize,
    partial_bytes_acked: usize,
    peer_rwnd: usize,
    fast_recovery: Option<u64>,
    fast_rtx_pending: bool,
    t3: Option<Instant>,
    rto: Duration,
    srtt: Option<Duration>,
    rttvar: Duration,
    rtt_probe: Option<(u64, Instant)>,
    error_count: u32,
    fwd_tsn_pending: bool,

    // 受信
    rx: Receiver,
    sack_now: bool,
    sack_timer: Option<Instant>,
    packets_since_sack: u32,
    last_advertised_rwnd: u32,

    // 制御
    raw_tx: VecDeque<Bytes>,
    ctrl: VecDeque<Chunk>,

    // HEARTBEAT
    hb_timer: Option<Instant>,
    hb_pending: Option<(u64, Instant)>,
    hb_nonce: u64,

    // RE-CONFIG（送る側）
    reconf_next_seq: u32,
    reconf_pending: BTreeSet<u16>,
    reconf_in_flight: Option<ReconfRequest>,
    reconf_timer: Option<Instant>,
    // RE-CONFIG（受ける側）
    peer_req_expected: u32,
    last_peer_result: Option<(u32, u32)>,
    deferred_reset: Option<DeferredReset>,

    // SHUTDOWN
    t2: Option<Instant>,
    t2_rto: Duration,

    events: VecDeque<SctpEvent>,
    stats: AssociationStats,
    /// 試験用: 初期 TSN を決める。
    forced_initial_tsn: Option<u32>,
}

fn validate(cfg: &SctpConfig) -> Result<(), SctpError> {
    if cfg.mtu < 256 || cfg.mtu > chunk::MAX_PACKET_LEN {
        return Err(SctpError::InvalidConfig("mtu must be 256..=65535"));
    }
    if cfg.num_streams == 0 {
        return Err(SctpError::InvalidConfig("num_streams must be > 0"));
    }
    if (cfg.receive_window as usize) < cfg.mtu {
        return Err(SctpError::InvalidConfig("receive_window must be >= mtu"));
    }
    if cfg.max_receive_message_size == 0 {
        return Err(SctpError::InvalidConfig(
            "max_receive_message_size must be > 0",
        ));
    }
    if cfg.max_send_buffer == 0 {
        return Err(SctpError::InvalidConfig("max_send_buffer must be > 0"));
    }
    if cfg.rto_min.is_zero() || cfg.rto_min > cfg.rto_initial || cfg.rto_initial > cfg.rto_max {
        return Err(SctpError::InvalidConfig(
            "rto_min <= rto_initial <= rto_max, rto_min > 0",
        ));
    }
    if cfg.cookie_lifetime.is_zero() {
        return Err(SctpError::InvalidConfig("cookie_lifetime must be > 0"));
    }
    if cfg.heartbeat_interval.is_some_and(|d| d.is_zero()) {
        return Err(SctpError::InvalidConfig("heartbeat_interval must be > 0"));
    }
    Ok(())
}

impl Association {
    pub fn new(cfg: SctpConfig, now: Instant) -> Result<Self, SctpError> {
        validate(&cfg)?;
        let mut rng = Rng::new()?;
        let cookie_key = rng.key32();
        let mtu = cfg.mtu;
        let rto = cfg.rto_initial;
        let max_send_message_size = cfg.max_send_message_size;
        let rx = Receiver::new(0, cfg.receive_window, cfg.max_receive_message_size, 0);
        Ok(Association {
            now,
            epoch: now,
            rng,
            cookie_key,
            state: AssociationState::Closed,
            dead: false,
            local_tag: 0,
            peer_tag: 0,
            local_initial_tsn: 0,
            peer_forward_tsn: false,
            peer_reconfig: false,
            out_streams: 0,
            in_streams: 0,
            max_send_message_size,
            init_packet: None,
            cookie: None,
            cookie_echo_pending: false,
            cookie_sent_at: None,
            cookie_ack_pending: false,
            t1: None,
            t1_rto: rto,
            t1_count: 0,
            next_tsn: initial_ext(0),
            cum_ack: initial_ext(0) - 1,
            adv_peer_ack: initial_ext(0) - 1,
            send_queue: VecDeque::new(),
            outstanding: VecDeque::new(),
            streams_out: HashMap::new(),
            buffered_total: 0,
            next_msg_id: 0,
            cwnd: initial_cwnd(mtu),
            ssthresh: usize::MAX,
            flight_size: 0,
            partial_bytes_acked: 0,
            peer_rwnd: 0,
            fast_recovery: None,
            fast_rtx_pending: false,
            t3: None,
            rto,
            srtt: None,
            rttvar: Duration::ZERO,
            rtt_probe: None,
            error_count: 0,
            fwd_tsn_pending: false,
            rx,
            sack_now: false,
            sack_timer: None,
            packets_since_sack: 0,
            last_advertised_rwnd: cfg.receive_window,
            raw_tx: VecDeque::new(),
            ctrl: VecDeque::new(),
            hb_timer: None,
            hb_pending: None,
            hb_nonce: 0,
            reconf_next_seq: 0,
            reconf_pending: BTreeSet::new(),
            reconf_in_flight: None,
            reconf_timer: None,
            peer_req_expected: 0,
            last_peer_result: None,
            deferred_reset: None,
            t2: None,
            t2_rto: rto,
            events: VecDeque::new(),
            stats: AssociationStats::default(),
            forced_initial_tsn: None,
            cfg,
        })
    }

    /// INIT を送る（能動的に開く）。両側が同時に呼んでもよい（INIT の衝突、RFC 9260 5.2.1）。
    pub fn connect(&mut self, now: Instant) {
        self.now = now;
        if self.dead || self.state != AssociationState::Closed {
            return;
        }
        self.local_tag = self.rng.nonzero_u32();
        self.local_initial_tsn = self.new_initial_tsn();
        self.setup_sender(self.local_initial_tsn);
        let init = InitChunk {
            initiate_tag: self.local_tag,
            a_rwnd: self.cfg.receive_window,
            num_outbound_streams: self.cfg.num_streams,
            num_inbound_streams: self.cfg.num_streams,
            initial_tsn: self.local_initial_tsn,
            forward_tsn_supported: true,
            supported_extensions: vec![ct::FORWARD_TSN, ct::RE_CONFIG],
            state_cookie: None,
            unrecognized: Vec::new(),
        };
        let p = self.packet(0, &[Chunk::Init(init)]);
        self.init_packet = Some(p.clone());
        self.push_raw(p);
        self.state = AssociationState::CookieWait;
        self.t1_rto = self.cfg.rto_initial;
        self.t1_count = 0;
        self.t1 = Some(now + self.t1_rto);
    }

    pub fn state(&self) -> AssociationState {
        self.state
    }

    /// 一度つながって（または開こうとして）から閉じた。以後は使えない。
    pub fn is_terminated(&self) -> bool {
        self.dead
    }

    /// 診断用の数値。
    pub fn stats(&self) -> AssociationStats {
        let mut s = self.stats.clone();
        s.cwnd = self.cwnd;
        s.ssthresh = self.ssthresh;
        s.flight_size = self.flight_size;
        s.peer_rwnd = self.peer_rwnd;
        s.rto = self.rto;
        s.srtt = self.srtt;
        s.outstanding_chunks = self.outstanding.len();
        s.queued_messages = self.send_queue.len();
        s.buffered = self.buffered_total;
        s.rx_buffered = self.rx.buffered;
        s.rx_items = self.rx.held_items();
        s.error_count = self.error_count;
        s.outbound_streams = self.out_streams;
        s.inbound_streams = self.in_streams;
        s
    }

    /// 送り出せるストリームの数（つながる前は 0）。
    pub fn outbound_streams(&self) -> u16 {
        self.out_streams
    }

    /// 受けるストリームの数（つながる前は 0）。
    pub fn inbound_streams(&self) -> u16 {
        self.in_streams
    }

    /// 相手の受信の上限（現在の値）。
    pub fn max_send_message_size(&self) -> usize {
        self.max_send_message_size
    }

    fn new_initial_tsn(&mut self) -> u32 {
        let r = self.rng.next_u32();
        self.forced_initial_tsn.unwrap_or(r)
    }

    fn packet(&self, vtag: u32, chunks: &[Chunk]) -> Bytes {
        chunk::serialize_packet(self.cfg.local_port, self.cfg.remote_port, vtag, chunks)
    }

    fn push_raw(&mut self, p: Bytes) {
        if self.raw_tx.len() < MAX_RAW_TX {
            self.raw_tx.push_back(p);
        }
    }

    fn push_ctrl(&mut self, c: Chunk) {
        if self.ctrl.len() < MAX_CTRL {
            self.ctrl.push_back(c);
        }
    }

    fn push_event(&mut self, e: SctpEvent) {
        let is_msg = matches!(e, SctpEvent::Message { .. } | SctpEvent::Closed { .. });
        if is_msg || self.events.len() < MAX_EVENTS {
            self.events.push_back(e);
        }
    }

    fn setup_sender(&mut self, initial_tsn: u32) {
        self.next_tsn = initial_ext(initial_tsn);
        self.cum_ack = self.next_tsn - 1;
        self.adv_peer_ack = self.cum_ack;
        self.reconf_next_seq = initial_tsn;
    }

    fn setup_peer(&mut self, c: &CookieData) {
        self.peer_tag = c.peer_tag;
        self.peer_forward_tsn = c.peer_forward_tsn;
        self.peer_reconfig = c.peer_reconfig;
        self.out_streams = self.cfg.num_streams.min(c.peer_mis);
        self.in_streams = self.cfg.num_streams.min(c.peer_os);
        self.rx = Receiver::new(
            c.peer_initial_tsn,
            self.cfg.receive_window,
            self.cfg.max_receive_message_size,
            self.in_streams,
        );
        self.last_advertised_rwnd = self.cfg.receive_window;
        self.peer_rwnd = c.peer_a_rwnd as usize;
        self.ssthresh = c.peer_a_rwnd as usize;
        self.cwnd = initial_cwnd(self.cfg.mtu);
        self.peer_req_expected = c.peer_initial_tsn;
        self.last_peer_result = None;
        self.deferred_reset = None;
    }

    fn enter_established(&mut self) {
        let was = self.state;
        self.state = AssociationState::Established;
        self.t1 = None;
        self.cookie = None;
        self.cookie_echo_pending = false;
        self.init_packet = None;
        if let Some(iv) = self.cfg.heartbeat_interval {
            self.hb_timer = Some(self.now + iv);
        }
        if !is_established_like(was) {
            self.push_event(SctpEvent::Connected);
        }
    }

    fn now_ms(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.epoch).as_millis() as u64
    }

    // ---- 受信 ----

    /// 1 つの SCTP パケットを処理する。
    pub fn handle_packet(&mut self, now: Instant, packet: &[u8]) -> Result<(), SctpError> {
        self.now = now;
        if packet.len() > chunk::MAX_PACKET_LEN {
            return Err(SctpError::Malformed("packet too long"));
        }
        let pkt = chunk::parse_packet(Bytes::copy_from_slice(packet))?;
        if self.dead {
            // 閉じた後の SHUTDOWN-ACK には T ビット付きの SHUTDOWN-COMPLETE を返す（8.4 の 5）。
            // 相手の SHUTDOWN-COMPLETE が失われた場合に相手を閉じさせる。
            if pkt.chunks.iter().any(|c| matches!(c, Chunk::ShutdownAck)) {
                let p = self.packet(pkt.vtag, &[Chunk::ShutdownComplete { t_bit: true }]);
                self.push_raw(p);
            }
            return Ok(());
        }
        if pkt.src_port != self.cfg.remote_port || pkt.dst_port != self.cfg.local_port {
            return Err(SctpError::Malformed("port mismatch"));
        }
        if pkt.chunks.is_empty() {
            return Err(SctpError::Malformed("no chunks"));
        }
        // INIT は単独で、Verification Tag は 0（RFC 9260 8.5.1）。
        if pkt.chunks.iter().any(|c| matches!(c, Chunk::Init(_))) {
            if pkt.chunks.len() != 1 || pkt.vtag != 0 {
                return Err(SctpError::Malformed("INIT must be alone with tag 0"));
            }
            self.stats.packets_received += 1;
            if let Some(Chunk::Init(init)) = pkt.chunks.into_iter().next() {
                self.handle_init(&init)?;
            }
            return Ok(());
        }
        let first_cookie = match pkt.chunks.first() {
            Some(Chunk::CookieEcho { cookie }) => Some(cookie.clone()),
            _ => None,
        };
        let tbit_chunk = pkt.chunks.iter().find_map(|c| match c {
            Chunk::Abort { t_bit, .. } | Chunk::ShutdownComplete { t_bit } => Some(*t_bit),
            _ => None,
        });
        let mut chunks = pkt.chunks.into_iter();
        if let Some(cookie) = first_cookie {
            self.handle_cookie_echo(pkt.vtag, &cookie)?;
            chunks.next();
            if self.local_tag == 0 || pkt.vtag != self.local_tag {
                return Ok(());
            }
        } else {
            let ok = match tbit_chunk {
                Some(true) => self.peer_tag != 0 && pkt.vtag == self.peer_tag,
                _ => self.local_tag != 0 && pkt.vtag == self.local_tag,
            };
            if !ok {
                return Err(SctpError::Malformed("verification tag"));
            }
        }
        self.stats.packets_received += 1;
        let mut got_data = false;
        for c in chunks {
            if self.dead {
                break;
            }
            match c {
                Chunk::Data(d) => {
                    if self.handle_data(&d) {
                        got_data = true;
                    }
                }
                Chunk::Init(_) => {}
                Chunk::InitAck(ia) => self.handle_init_ack(&ia)?,
                Chunk::Sack(s) => {
                    if is_established_like(self.state) {
                        self.handle_sack(&s);
                    }
                }
                Chunk::Heartbeat { info } => {
                    if self.peer_tag != 0 && info.len() <= MAX_HEARTBEAT_INFO {
                        self.push_ctrl(Chunk::HeartbeatAck { info });
                    }
                }
                Chunk::HeartbeatAck { info } => self.handle_heartbeat_ack(&info),
                Chunk::Abort { .. } => {
                    self.close("peer abort".into());
                }
                Chunk::Shutdown { cum_tsn_ack } => self.handle_shutdown(cum_tsn_ack),
                Chunk::ShutdownAck => self.handle_shutdown_ack(),
                Chunk::ShutdownComplete { .. } => {
                    if self.state == AssociationState::ShutdownAckSent {
                        self.close("shutdown".into());
                    }
                }
                Chunk::Error { .. } => self.stats.errors_received += 1,
                Chunk::CookieEcho { .. } => {}
                Chunk::CookieAck => {
                    if self.state == AssociationState::CookieEchoed {
                        if self.t1_count == 0
                            && let Some(t) = self.cookie_sent_at
                        {
                            self.rtt_sample(now.saturating_duration_since(t));
                        }
                        self.enter_established();
                    }
                }
                Chunk::ForwardTsn {
                    new_cum_tsn,
                    streams,
                } => {
                    if is_established_like(self.state) {
                        self.stats.forward_tsn_received += 1;
                        let mut out = Vec::new();
                        self.rx.forward_tsn(new_cum_tsn, &streams, &mut out);
                        self.deliver(out);
                        self.sack_now = true;
                        self.check_deferred_reset();
                    }
                }
                Chunk::Reconfig { params } => {
                    if is_established_like(self.state) {
                        self.handle_reconfig(params);
                    }
                }
                Chunk::Unknown { chunk_type, raw } => {
                    if chunk_type & 0x40 != 0
                        && self.peer_tag != 0
                        && raw.len() <= MAX_REPORTED_CHUNK
                    {
                        self.push_ctrl(Chunk::Error {
                            causes: chunk::unrecognized_chunk_cause(&raw),
                        });
                    }
                }
            }
        }
        if got_data && !self.dead {
            self.check_deferred_reset();
            self.packets_since_sack += 1;
            if self.rx.has_gaps() || self.rx.has_dups() || self.packets_since_sack >= 2 {
                self.sack_now = true;
            } else if self.sack_timer.is_none() {
                self.sack_timer = Some(now + DELAYED_SACK);
            }
            if self.state == AssociationState::ShutdownSent {
                // SHUTDOWN-SENT で DATA を受けたら、すぐに SACK と SHUTDOWN を返す（9.2）。
                self.sack_now = true;
                self.push_ctrl(Chunk::Shutdown {
                    cum_tsn_ack: self.rx.cum as u32,
                });
                self.t2 = Some(now + self.t2_rto);
            }
        }
        Ok(())
    }

    fn make_init_ack(&mut self, init: &InitChunk, my_tag: u32, my_tsn: u32) -> Bytes {
        let cd = CookieData {
            my_tag,
            peer_tag: init.initiate_tag,
            my_initial_tsn: my_tsn,
            peer_initial_tsn: init.initial_tsn,
            peer_a_rwnd: init.a_rwnd,
            peer_os: init.num_outbound_streams,
            peer_mis: init.num_inbound_streams,
            my_os: self.cfg.num_streams,
            my_mis: self.cfg.num_streams,
            peer_forward_tsn: init.supports_forward_tsn(),
            peer_reconfig: init.supports_reconfig(),
            timestamp_ms: self.now_ms(self.now),
        };
        let cookie = cd.encode(&self.cookie_key);
        let ack = InitChunk {
            initiate_tag: my_tag,
            a_rwnd: self.cfg.receive_window,
            num_outbound_streams: self.cfg.num_streams,
            num_inbound_streams: self.cfg.num_streams,
            initial_tsn: my_tsn,
            forward_tsn_supported: true,
            supported_extensions: vec![ct::FORWARD_TSN, ct::RE_CONFIG],
            state_cookie: Some(cookie),
            unrecognized: init.unrecognized.clone(),
        };
        self.packet(init.initiate_tag, &[Chunk::InitAck(ack)])
    }

    fn handle_init(&mut self, init: &InitChunk) -> Result<(), SctpError> {
        if init.initiate_tag == 0 || init.num_outbound_streams == 0 || init.num_inbound_streams == 0
        {
            return Err(SctpError::Malformed("INIT parameters"));
        }
        let (tag, tsn) = match self.state {
            // 状態を持たずに答える（cookie にすべて入れる）。
            AssociationState::Closed => (self.rng.nonzero_u32(), self.new_initial_tsn()),
            // INIT の衝突（5.2.1）・つながった後の INIT（5.2.2 を簡略化）: 今のタグで答える。
            _ => (self.local_tag, self.local_initial_tsn),
        };
        let p = self.make_init_ack(init, tag, tsn);
        self.push_raw(p);
        Ok(())
    }

    fn handle_init_ack(&mut self, ia: &InitChunk) -> Result<(), SctpError> {
        if self.state != AssociationState::CookieWait {
            return Ok(());
        }
        if ia.initiate_tag == 0 || ia.num_outbound_streams == 0 || ia.num_inbound_streams == 0 {
            return Err(SctpError::Malformed("INIT-ACK parameters"));
        }
        let Some(cookie) = ia.state_cookie.clone() else {
            return Err(SctpError::Malformed("INIT-ACK without cookie"));
        };
        let cd = CookieData {
            my_tag: self.local_tag,
            peer_tag: ia.initiate_tag,
            my_initial_tsn: self.local_initial_tsn,
            peer_initial_tsn: ia.initial_tsn,
            peer_a_rwnd: ia.a_rwnd,
            peer_os: ia.num_outbound_streams,
            peer_mis: ia.num_inbound_streams,
            my_os: self.cfg.num_streams,
            my_mis: self.cfg.num_streams,
            peer_forward_tsn: ia.supports_forward_tsn(),
            peer_reconfig: ia.supports_reconfig(),
            timestamp_ms: 0,
        };
        self.setup_peer(&cd);
        self.cookie = Some(cookie);
        self.cookie_echo_pending = true;
        self.cookie_sent_at = None;
        self.state = AssociationState::CookieEchoed;
        self.t1_rto = self.cfg.rto_initial;
        self.t1_count = 0;
        self.t1 = Some(self.now + self.t1_rto);
        if !ia.unrecognized.is_empty() {
            self.push_ctrl(Chunk::Error {
                causes: chunk::unrecognized_params_cause(&ia.unrecognized),
            });
        }
        Ok(())
    }

    fn handle_cookie_echo(&mut self, vtag: u32, cookie: &[u8]) -> Result<(), SctpError> {
        let Some(cd) = CookieData::decode(cookie, &self.cookie_key) else {
            return Err(SctpError::Malformed("invalid cookie"));
        };
        if cd.my_tag != vtag {
            return Err(SctpError::Malformed("cookie tag mismatch"));
        }
        let now_ms = self.now_ms(self.now);
        if cd.timestamp_ms > now_ms
            || now_ms - cd.timestamp_ms > self.cfg.cookie_lifetime.as_millis() as u64
        {
            return Err(SctpError::Malformed("stale cookie"));
        }
        match self.state {
            AssociationState::Closed => {
                // 新しい association。ここで初めて状態を持つ。
                self.local_tag = cd.my_tag;
                self.local_initial_tsn = cd.my_initial_tsn;
                self.setup_sender(cd.my_initial_tsn);
                self.setup_peer(&cd);
                self.enter_established();
                self.cookie_ack_pending = true;
            }
            AssociationState::CookieWait => {
                // INIT の衝突。こちらの INIT への答えより先に相手の COOKIE-ECHO が来た
                // （cookie のタグが違うなら、connect の前に状態を持たずに答えたもの。まだ何も
                // 送っていないので cookie の値に合わせる）。
                if cd.my_tag != self.local_tag {
                    self.local_tag = cd.my_tag;
                    self.local_initial_tsn = cd.my_initial_tsn;
                    self.setup_sender(cd.my_initial_tsn);
                }
                self.setup_peer(&cd);
                self.enter_established();
                self.cookie_ack_pending = true;
            }
            AssociationState::CookieEchoed => {
                // 5.2.4 の B・D: こちらのタグが合えば ESTABLISHED へ。
                if cd.my_tag != self.local_tag {
                    return Ok(());
                }
                self.setup_peer(&cd);
                self.enter_established();
                self.cookie_ack_pending = true;
            }
            _ => {
                // つながった後: COOKIE-ACK が失われた再送（D）か、相手のタグの更新（B）。
                if cd.my_tag != self.local_tag {
                    return Ok(());
                }
                if cd.peer_tag != self.peer_tag {
                    self.peer_tag = cd.peer_tag;
                }
                self.cookie_ack_pending = true;
            }
        }
        Ok(())
    }

    /// DATA を受ける。新しい TSN なら真。
    fn handle_data(&mut self, d: &DataChunk) -> bool {
        match self.state {
            AssociationState::Established
            | AssociationState::ShutdownPending
            | AssociationState::ShutdownSent
            | AssociationState::ShutdownReceived => {}
            _ => return false,
        }
        if d.immediate {
            // I ビット（RFC 7053）: すぐに SACK を返す。
            self.sack_now = true;
        }
        let mut out = Vec::new();
        match self.rx.on_data(d, &mut out) {
            DataOutcome::Accepted => {
                self.deliver(out);
                true
            }
            DataOutcome::Duplicate => true,
            DataOutcome::Dropped => {
                // 窓が足りない: SACK で今の状態を知らせる。
                self.sack_now = true;
                false
            }
            DataOutcome::InvalidStream => {
                let mut v = Vec::new();
                v.put_u16(1);
                v.put_u16(8);
                v.put_u16(d.stream);
                v.put_u16(0);
                self.push_ctrl(Chunk::Error {
                    causes: Bytes::from(v),
                });
                true
            }
            DataOutcome::Violation => {
                self.abort_with("message too large or reassembly overflow");
                false
            }
        }
    }

    fn deliver(&mut self, msgs: Vec<RxMessage>) {
        for m in msgs {
            self.push_event(SctpEvent::Message {
                stream: m.stream,
                ppid: m.ppid,
                data: m.data,
                unordered: m.unordered,
            });
        }
    }

    fn handle_heartbeat_ack(&mut self, info: &[u8]) {
        if info.len() != 12 || u16::from_be_bytes([info[0], info[1]]) != chunk::pt::HEARTBEAT_INFO {
            return;
        }
        let mut n = [0u8; 8];
        n.copy_from_slice(&info[4..12]);
        let nonce = u64::from_be_bytes(n);
        if let Some((want, sent)) = self.hb_pending
            && want == nonce
        {
            self.hb_pending = None;
            self.rtt_sample(self.now.saturating_duration_since(sent));
            self.error_count = 0;
            self.stats.heartbeats_acked += 1;
            if let Some(iv) = self.cfg.heartbeat_interval {
                self.hb_timer = Some(self.now + iv);
            }
        }
    }

    // ---- SACK（送信側） ----

    fn handle_sack(&mut self, s: &SackChunk) {
        let now = self.now;
        let cum = extend(self.cum_ack, s.cum_tsn_ack);
        if cum < self.cum_ack || cum >= self.next_tsn {
            return;
        }
        let flight_before = self.flight_size;
        let was_in_recovery = self.fast_recovery.is_some();
        let cum_advanced = cum > self.cum_ack;
        let mut newly_acked = 0usize;
        let mut htna: Option<u64> = None;
        while self.cum_ack < cum {
            let Some(c) = self.outstanding.pop_front() else {
                break;
            };
            self.cum_ack = c.tsn;
            if c.in_flight {
                self.flight_size -= c.len();
            }
            if !c.acked && !c.abandoned {
                newly_acked += c.len();
                htna = Some(c.tsn);
            }
            if !c.abandoned {
                self.dec_buffered(c.stream, c.len());
            }
        }
        self.cum_ack = cum;
        if let Some((t, sent)) = self.rtt_probe
            && t <= cum
        {
            self.rtt_probe = None;
            self.rtt_sample(now.saturating_duration_since(sent));
        }
        // gap ack block
        let mut ranges: Vec<(u64, u64)> = s
            .gap_blocks
            .iter()
            .take(MAX_RX_GAP_BLOCKS)
            .filter(|(a, b)| *a != 0 && a <= b)
            .map(|&(a, b)| (cum + a as u64, cum + b as u64))
            .collect();
        ranges.sort_unstable();
        let mut ri = 0;
        let mut probe_acked = false;
        for c in self.outstanding.iter_mut() {
            while ri < ranges.len() && ranges[ri].1 < c.tsn {
                ri += 1;
            }
            let in_gap = ri < ranges.len() && ranges[ri].0 <= c.tsn;
            if in_gap {
                if !c.acked {
                    c.acked = true;
                    c.needs_rtx = false;
                    if c.in_flight {
                        c.in_flight = false;
                        self.flight_size -= c.payload.len();
                    }
                    if !c.abandoned {
                        newly_acked += c.payload.len();
                        htna = Some(htna.map_or(c.tsn, |h| h.max(c.tsn)));
                    }
                    if self.rtt_probe.is_some_and(|(t, _)| t == c.tsn) {
                        probe_acked = true;
                    }
                }
            } else if c.acked {
                // 取り消し（renege）。T3 で再送する。
                c.acked = false;
            }
        }
        if probe_acked && let Some((_, sent)) = self.rtt_probe.take() {
            self.rtt_sample(now.saturating_duration_since(sent));
        }
        // 速い再送（7.2.4）
        let mut to_fast = Vec::new();
        if let Some(h) = htna
            && !ranges.is_empty()
        {
            for (i, c) in self.outstanding.iter_mut().enumerate() {
                if c.tsn >= h {
                    break;
                }
                if c.acked || c.abandoned || c.needs_rtx || c.fast_rtx_done {
                    continue;
                }
                c.miss += 1;
                if c.miss >= 3 {
                    c.miss = 0;
                    to_fast.push(i);
                }
            }
        }
        let mut any_fast = false;
        for i in to_fast {
            if i < self.outstanding.len() && self.mark_for_rtx(i) {
                self.outstanding[i].fast_rtx_done = true;
                any_fast = true;
            }
        }
        // 輻輳窓（7.2.1・7.2.2）
        if cum_advanced && !was_in_recovery {
            let mtu = self.cfg.mtu;
            if self.cwnd <= self.ssthresh {
                if flight_before >= self.cwnd {
                    self.cwnd += newly_acked.min(mtu);
                }
            } else {
                self.partial_bytes_acked += newly_acked;
                if self.partial_bytes_acked >= self.cwnd && flight_before >= self.cwnd {
                    self.partial_bytes_acked -= self.cwnd;
                    self.cwnd += mtu;
                }
            }
        }
        if let Some(exit) = self.fast_recovery
            && self.cum_ack >= exit
        {
            self.fast_recovery = None;
        }
        if any_fast {
            self.stats.fast_retransmits += 1;
            if self.fast_recovery.is_none() {
                self.ssthresh = (self.cwnd / 2).max(4 * self.cfg.mtu);
                self.cwnd = self.ssthresh;
                self.partial_bytes_acked = 0;
                self.fast_recovery = Some(self.next_tsn - 1);
            }
            self.fast_rtx_pending = true;
        }
        if self.flight_size == 0 {
            self.partial_bytes_acked = 0;
        }
        self.peer_rwnd = (s.a_rwnd as usize).saturating_sub(self.flight_size);
        if newly_acked > 0 || cum_advanced {
            self.error_count = 0;
        }
        // T3
        let any_unacked = self.outstanding.iter().any(|c| !c.acked);
        if !any_unacked {
            self.t3 = None;
        } else if cum_advanced || self.t3.is_none() {
            self.t3 = Some(now + self.rto);
        }
        self.update_adv_peer_ack();
        self.check_shutdown_progress();
    }

    /// 再送の印を付ける。PR-SCTP の上限を超えたらメッセージごと捨てて偽を返す。
    fn mark_for_rtx(&mut self, i: usize) -> bool {
        let now = self.now;
        let c = &self.outstanding[i];
        if c.abandoned || c.acked {
            return false;
        }
        if c.max_rtx.is_some_and(|m| c.tx_count > m) || c.expiry.is_some_and(|e| now >= e) {
            let id = c.msg_id;
            self.abandon_message(id);
            return false;
        }
        let c = &mut self.outstanding[i];
        c.needs_rtx = true;
        if c.in_flight {
            c.in_flight = false;
            self.flight_size -= c.payload.len();
        }
        true
    }

    /// メッセージを捨てる（RFC 3758 3.5）。送っていない残りも捨てる。
    fn abandon_message(&mut self, id: u64) {
        let mut released: Vec<(u16, usize)> = Vec::new();
        for c in self.outstanding.iter_mut() {
            if c.msg_id != id || c.abandoned {
                continue;
            }
            c.abandoned = true;
            c.needs_rtx = false;
            if c.in_flight {
                c.in_flight = false;
                self.flight_size -= c.payload.len();
            }
            released.push((c.stream, c.payload.len()));
        }
        if self.send_queue.front().is_some_and(|m| m.msg_id == id)
            && let Some(m) = self.send_queue.pop_front()
        {
            released.push((m.stream, m.data.len() - m.offset));
            if m.offset > 0 {
                // 途中まで送ったメッセージ: 送らない最後の断片に TSN を割り当て、捨てたことにする。
                // FORWARD-TSN がメッセージ全体を覆うので、相手は受け取った断片を捨てられる
                // （送った断片がすでに累積 TSN で確認されていても）。
                self.outstanding.push_back(OutChunk {
                    tsn: self.next_tsn,
                    msg_id: m.msg_id,
                    stream: m.stream,
                    ssn: if m.unordered { 0 } else { m.ssn },
                    ppid: m.ppid,
                    unordered: m.unordered,
                    begin: false,
                    end: true,
                    payload: Bytes::new(),
                    tx_count: 0,
                    acked: false,
                    abandoned: true,
                    in_flight: false,
                    needs_rtx: false,
                    miss: 0,
                    fast_rtx_done: false,
                    max_rtx: m.max_rtx,
                    expiry: m.expiry,
                });
                self.next_tsn += 1;
            }
        }
        for (s, n) in released {
            self.dec_buffered(s, n);
        }
        self.stats.abandoned_messages += 1;
        self.update_adv_peer_ack();
        // FORWARD-TSN が失われても再送できるように T3 を動かしておく。
        if self.t3.is_none() && self.outstanding.iter().any(|c| !c.acked) {
            self.t3 = Some(self.now + self.rto);
        }
    }

    fn update_adv_peer_ack(&mut self) {
        if !self.peer_forward_tsn {
            return;
        }
        if self.adv_peer_ack < self.cum_ack {
            self.adv_peer_ack = self.cum_ack;
        }
        let start = (self.adv_peer_ack - self.cum_ack) as usize;
        for c in self.outstanding.iter().skip(start) {
            if c.abandoned {
                self.adv_peer_ack = c.tsn;
            } else {
                break;
            }
        }
        if self.adv_peer_ack > self.cum_ack {
            self.fwd_tsn_pending = true;
        }
    }

    fn make_forward_tsn(&self) -> Chunk {
        let mut per: BTreeMap<u16, u16> = BTreeMap::new();
        let mut new_cum = self.cum_ack;
        // リセットを求めている最中のストリームの、前の世代のメッセージの SSN は載せない
        // （相手がリセットした後に届くと新しい世代の SSN と区別できない。前の世代の
        // 順番待ちは相手がリセットするときに配る）。
        let in_flight = self
            .reconf_in_flight
            .as_ref()
            .map(|r| (r.streams.as_slice(), extend(self.cum_ack, r.last_tsn)));
        for c in self.outstanding.iter() {
            if c.tsn > self.adv_peer_ack {
                break;
            }
            let old_epoch = self
                .streams_out
                .get(&c.stream)
                .is_some_and(|so| c.tsn <= so.epoch_tsn)
                || in_flight.is_some_and(|(ss, last)| c.tsn <= last && ss.contains(&c.stream));
            if !c.unordered && !old_epoch {
                if !per.contains_key(&c.stream) && per.len() >= MAX_FWD_TSN_STREAMS {
                    break;
                }
                per.entry(c.stream)
                    .and_modify(|v| {
                        if (c.ssn.wrapping_sub(*v) as i16) > 0 {
                            *v = c.ssn
                        }
                    })
                    .or_insert(c.ssn);
            }
            new_cum = c.tsn;
        }
        // 途中で切った場合でもメッセージの途中で切らない（切るのは別ストリームの境目だけ）。
        Chunk::ForwardTsn {
            new_cum_tsn: new_cum as u32,
            streams: per.into_iter().collect(),
        }
    }

    fn dec_buffered(&mut self, stream: u16, n: usize) {
        self.buffered_total = self.buffered_total.saturating_sub(n);
        if let Some(so) = self.streams_out.get_mut(&stream) {
            let before = so.buffered;
            so.buffered = so.buffered.saturating_sub(n);
            if before > so.low_threshold && so.buffered <= so.low_threshold && n > 0 {
                self.push_event(SctpEvent::BufferedAmountLow { stream });
            }
        }
    }

    fn rtt_sample(&mut self, r: Duration) {
        match self.srtt {
            None => {
                self.srtt = Some(r);
                self.rttvar = r / 2;
            }
            Some(srtt) => {
                let diff = srtt.abs_diff(r);
                self.rttvar = self.rttvar * 3 / 4 + diff / 4;
                self.srtt = Some(srtt * 7 / 8 + r / 8);
            }
        }
        let rto = self.srtt.unwrap_or(r) + self.rttvar * 4;
        self.rto = rto.clamp(self.cfg.rto_min, self.cfg.rto_max);
    }

    /// 誤りの数を足す。上限を超えたら閉じて真を返す。
    fn bump_error(&mut self) -> bool {
        self.error_count += 1;
        if self.error_count > self.cfg.max_retransmits {
            self.abort_with("retransmission limit");
            true
        } else {
            false
        }
    }

    // ---- SHUTDOWN ----

    fn all_sent_and_acked(&self) -> bool {
        self.send_queue.is_empty() && self.outstanding.is_empty()
    }

    fn check_shutdown_progress(&mut self) {
        if !self.all_sent_and_acked() {
            return;
        }
        match self.state {
            AssociationState::ShutdownPending => {
                self.push_ctrl(Chunk::Shutdown {
                    cum_tsn_ack: self.rx.cum as u32,
                });
                self.state = AssociationState::ShutdownSent;
                self.t2_rto = self.rto;
                self.t2 = Some(self.now + self.t2_rto);
            }
            AssociationState::ShutdownReceived => {
                self.push_ctrl(Chunk::ShutdownAck);
                self.state = AssociationState::ShutdownAckSent;
                self.t2_rto = self.rto;
                self.t2 = Some(self.now + self.t2_rto);
            }
            _ => {}
        }
    }

    fn handle_shutdown(&mut self, cum_tsn_ack: u32) {
        if !is_established_like(self.state) {
            return;
        }
        // SHUTDOWN の累積 TSN は SACK と同じに扱う。
        let s = SackChunk {
            cum_tsn_ack,
            a_rwnd: (self.peer_rwnd + self.flight_size).min(u32::MAX as usize) as u32,
            gap_blocks: Vec::new(),
            dup_tsns: Vec::new(),
        };
        let cum = extend(self.cum_ack, cum_tsn_ack);
        if cum >= self.cum_ack && cum < self.next_tsn {
            self.handle_sack(&s);
        }
        match self.state {
            AssociationState::Established | AssociationState::ShutdownPending => {
                self.state = AssociationState::ShutdownReceived;
                self.check_shutdown_progress();
            }
            AssociationState::ShutdownSent => {
                // 同時に SHUTDOWN を送り合った。
                self.push_ctrl(Chunk::ShutdownAck);
                self.state = AssociationState::ShutdownAckSent;
                self.t2 = Some(self.now + self.t2_rto);
            }
            _ => {}
        }
    }

    fn handle_shutdown_ack(&mut self) {
        match self.state {
            AssociationState::ShutdownSent | AssociationState::ShutdownAckSent => {
                let p = self.packet(self.peer_tag, &[Chunk::ShutdownComplete { t_bit: false }]);
                self.push_raw(p);
                self.close("shutdown".into());
            }
            _ => {}
        }
    }

    fn close(&mut self, reason: String) {
        if self.dead {
            return;
        }
        self.dead = true;
        self.state = AssociationState::Closed;
        self.t1 = None;
        self.t2 = None;
        self.t3 = None;
        self.sack_timer = None;
        self.hb_timer = None;
        self.reconf_timer = None;
        self.ctrl.clear();
        self.send_queue.clear();
        self.outstanding.clear();
        self.flight_size = 0;
        self.buffered_total = 0;
        for so in self.streams_out.values_mut() {
            so.buffered = 0;
        }
        self.push_event(SctpEvent::Closed { reason });
    }

    fn abort_with(&mut self, reason: &str) {
        if self.dead {
            return;
        }
        if self.peer_tag != 0 {
            let p = self.packet(
                self.peer_tag,
                &[Chunk::Abort {
                    t_bit: false,
                    causes: Bytes::new(),
                }],
            );
            self.push_raw(p);
        }
        self.close(reason.into());
    }

    // ---- RE-CONFIG ----

    fn handle_reconfig(&mut self, params: Vec<ReconfigParam>) {
        let mut responses = Vec::new();
        for p in params {
            match p {
                ReconfigParam::OutgoingReset {
                    req_seq,
                    last_tsn,
                    streams,
                    ..
                } => {
                    let result = self.on_peer_outgoing_reset(req_seq, last_tsn, streams);
                    responses.push(ReconfigParam::Response {
                        resp_seq: req_seq,
                        result,
                        next_tsns: None,
                    });
                }
                ReconfigParam::IncomingReset { req_seq, .. }
                | ReconfigParam::SsnTsnReset { req_seq }
                | ReconfigParam::AddOutgoing { req_seq, .. }
                | ReconfigParam::AddIncoming { req_seq, .. } => {
                    // 使わない要求は断る。
                    let result = if req_seq == self.peer_req_expected {
                        self.peer_req_expected = self.peer_req_expected.wrapping_add(1);
                        self.last_peer_result = Some((req_seq, rr::DENIED));
                        rr::DENIED
                    } else {
                        self.replay_result(req_seq)
                    };
                    responses.push(ReconfigParam::Response {
                        resp_seq: req_seq,
                        result,
                        next_tsns: None,
                    });
                }
                ReconfigParam::Response {
                    resp_seq, result, ..
                } => self.on_reconfig_response(resp_seq, result),
                ReconfigParam::Unknown { .. } => {}
            }
        }
        for r in responses {
            self.push_reconfig_param(r);
        }
    }

    fn replay_result(&self, seq: u32) -> u32 {
        match self.last_peer_result {
            Some((s, r)) if s == seq => r,
            _ => rr::ERROR_BAD_SEQUENCE,
        }
    }

    fn push_reconfig_param(&mut self, p: ReconfigParam) {
        // 直前の RE-CONFIG に 1 つしかなければ同じチャンクにまとめる。
        if let Some(Chunk::Reconfig { params }) = self.ctrl.back_mut()
            && params.len() == 1
        {
            params.push(p);
            return;
        }
        self.push_ctrl(Chunk::Reconfig { params: vec![p] });
    }

    fn on_peer_outgoing_reset(&mut self, seq: u32, last_tsn: u32, mut streams: Vec<u16>) -> u32 {
        if seq != self.peer_req_expected {
            return self.replay_result(seq);
        }
        streams.retain(|s| *s < self.in_streams);
        let last = self.rx.extend(last_tsn);
        if last <= self.rx.cum {
            self.perform_incoming_reset(seq, streams, last);
            rr::SUCCESS_PERFORMED
        } else {
            self.deferred_reset = Some(DeferredReset {
                seq,
                last_tsn: last,
                streams,
            });
            rr::IN_PROGRESS
        }
    }

    fn perform_incoming_reset(&mut self, seq: u32, streams: Vec<u16>, last_tsn: u64) {
        let mut out = Vec::new();
        self.rx.reset_streams(&streams, last_tsn, &mut out);
        self.deliver(out);
        self.peer_req_expected = self.peer_req_expected.wrapping_add(1);
        self.last_peer_result = Some((seq, rr::SUCCESS_PERFORMED));
        self.deferred_reset = None;
        self.push_event(SctpEvent::IncomingStreamsReset { streams });
    }

    fn check_deferred_reset(&mut self) {
        let ready = self
            .deferred_reset
            .as_ref()
            .is_some_and(|d| d.last_tsn <= self.rx.cum);
        if ready && let Some(d) = self.deferred_reset.take() {
            let seq = d.seq;
            self.perform_incoming_reset(seq, d.streams, d.last_tsn);
            self.push_reconfig_param(ReconfigParam::Response {
                resp_seq: seq,
                result: rr::SUCCESS_PERFORMED,
                next_tsns: None,
            });
        }
    }

    fn on_reconfig_response(&mut self, resp_seq: u32, result: u32) {
        let Some(req) = &mut self.reconf_in_flight else {
            return;
        };
        if req.seq != resp_seq {
            return;
        }
        // 相手が答えた（生きている）。
        self.error_count = 0;
        match result {
            rr::SUCCESS_NOTHING_TO_DO | rr::SUCCESS_PERFORMED => {
                let streams = std::mem::take(&mut req.streams);
                let epoch = extend(self.cum_ack, req.last_tsn);
                for s in &streams {
                    let so = self.streams_out.entry(*s).or_default();
                    so.next_ssn = 0;
                    so.epoch_tsn = epoch;
                    so.resetting = false;
                }
                self.reconf_in_flight = None;
                self.reconf_next_seq = self.reconf_next_seq.wrapping_add(1);
                self.reconf_timer = None;
                self.push_event(SctpEvent::OutgoingStreamsReset { streams });
            }
            rr::IN_PROGRESS => {
                req.in_progress = true;
                self.reconf_timer = Some(self.now + self.rto);
            }
            _ => {
                // 断られた: あきらめてストリームを使えるように戻す。
                let streams = std::mem::take(&mut req.streams);
                for s in &streams {
                    self.streams_out.entry(*s).or_default().resetting = false;
                }
                self.reconf_in_flight = None;
                self.reconf_next_seq = self.reconf_next_seq.wrapping_add(1);
                self.reconf_timer = None;
            }
        }
    }

    fn reconfig_request_param(&self, r: &ReconfRequest) -> ReconfigParam {
        ReconfigParam::OutgoingReset {
            req_seq: r.seq,
            resp_seq: self.peer_req_expected.wrapping_sub(1),
            last_tsn: r.last_tsn,
            streams: r.streams.clone(),
        }
    }

    fn try_send_reconfig(&mut self) {
        if self.reconf_in_flight.is_some()
            || self.reconf_pending.is_empty()
            || !matches!(
                self.state,
                AssociationState::Established | AssociationState::ShutdownPending
            )
        {
            return;
        }
        // まだ TSN を割り当てていないメッセージが残っていれば待つ。
        let waiting = self
            .send_queue
            .iter()
            .any(|m| self.reconf_pending.contains(&m.stream));
        if waiting {
            return;
        }
        let mut streams = Vec::new();
        while streams.len() < MAX_RECONFIG_STREAMS {
            let Some(s) = self.reconf_pending.pop_first() else {
                break;
            };
            streams.push(s);
        }
        let req = ReconfRequest {
            seq: self.reconf_next_seq,
            streams,
            last_tsn: (self.next_tsn - 1) as u32,
            in_progress: false,
        };
        let p = self.reconfig_request_param(&req);
        self.reconf_in_flight = Some(req);
        self.push_reconfig_param(p);
        self.stats.reconfig_sent += 1;
        self.reconf_timer = Some(self.now + self.rto);
    }

    // ---- 送信 ----

    pub fn send(
        &mut self,
        now: Instant,
        stream: u16,
        ppid: u32,
        data: Bytes,
        opts: SendOptions,
    ) -> Result<(), SctpError> {
        self.now = now;
        match self.state {
            AssociationState::Established => {}
            AssociationState::Closed if self.dead => return Err(SctpError::Closed),
            AssociationState::Closed
            | AssociationState::CookieWait
            | AssociationState::CookieEchoed => return Err(SctpError::NotConnected),
            _ => return Err(SctpError::Closed),
        }
        if stream >= self.out_streams {
            return Err(SctpError::InvalidStream);
        }
        if self.streams_out.get(&stream).is_some_and(|s| s.resetting) {
            return Err(SctpError::InvalidStream);
        }
        if data.is_empty() {
            return Err(SctpError::Malformed("empty message"));
        }
        if self.max_send_message_size != 0 && data.len() > self.max_send_message_size {
            return Err(SctpError::TooLarge);
        }
        if data.len() > self.cfg.max_send_buffer {
            return Err(SctpError::TooLarge);
        }
        if self.buffered_total + data.len() > self.cfg.max_send_buffer {
            return Err(SctpError::BufferFull);
        }
        let (max_rtx, expiry) = if self.peer_forward_tsn {
            (opts.max_retransmits, opts.max_lifetime.map(|d| now + d))
        } else {
            (None, None)
        };
        let len = data.len();
        self.send_queue.push_back(OutMessage {
            msg_id: self.next_msg_id,
            stream,
            ppid,
            data,
            offset: 0,
            unordered: opts.unordered,
            ssn: 0,
            max_rtx,
            expiry,
        });
        self.next_msg_id += 1;
        self.buffered_total += len;
        self.streams_out.entry(stream).or_default().buffered += len;
        Ok(())
    }

    /// 送信ストリームをリセットする（RFC 6525、DataChannel を閉じる）。
    pub fn reset_streams(&mut self, now: Instant, streams: &[u16]) -> Result<(), SctpError> {
        self.now = now;
        match self.state {
            AssociationState::Established => {}
            AssociationState::Closed if self.dead => return Err(SctpError::Closed),
            AssociationState::Closed
            | AssociationState::CookieWait
            | AssociationState::CookieEchoed => return Err(SctpError::NotConnected),
            _ => return Err(SctpError::Closed),
        }
        if !self.peer_reconfig {
            return Err(SctpError::InvalidStream);
        }
        if streams.iter().any(|s| *s >= self.out_streams) {
            return Err(SctpError::InvalidStream);
        }
        for &s in streams {
            let so = self.streams_out.entry(s).or_default();
            if so.resetting {
                continue;
            }
            so.resetting = true;
            self.reconf_pending.insert(s);
        }
        self.try_send_reconfig();
        Ok(())
    }

    /// ストリームの送信待ち（未送信 + 未確認）の byte 数。
    pub fn buffered_amount(&self, stream: u16) -> usize {
        self.streams_out.get(&stream).map_or(0, |s| s.buffered)
    }

    pub fn set_buffered_amount_low_threshold(&mut self, stream: u16, bytes: usize) {
        self.streams_out.entry(stream).or_default().low_threshold = bytes;
    }

    /// 相手の受信の上限を後から変える（SDP の a=max-message-size）。
    pub fn set_max_send_message_size(&mut self, n: usize) {
        self.max_send_message_size = n;
    }

    /// 平滑化した RTT（測れていなければ `None`）。
    pub fn rtt(&self) -> Option<Duration> {
        self.srtt
    }

    /// SHUTDOWN で丁寧に閉じる。
    pub fn shutdown(&mut self, now: Instant) {
        self.now = now;
        match self.state {
            AssociationState::Closed => {
                if !self.dead {
                    self.close("shutdown".into());
                }
            }
            AssociationState::CookieWait => self.close("shutdown".into()),
            AssociationState::CookieEchoed => self.abort_with("shutdown"),
            AssociationState::Established => {
                self.state = AssociationState::ShutdownPending;
                self.check_shutdown_progress();
            }
            _ => {}
        }
    }

    /// ABORT を送ってすぐ閉じる。
    pub fn abort(&mut self, now: Instant) {
        self.now = now;
        self.abort_with("local abort");
    }

    pub fn poll_event(&mut self) -> Option<SctpEvent> {
        let e = self.events.pop_front()?;
        if let SctpEvent::Message { data, .. } = &e {
            self.rx.release(data.len());
            // 窓が大きく開いたら SACK で知らせる。
            let now_rwnd = self.rx.a_rwnd();
            if !self.dead
                && is_established_like(self.state)
                && now_rwnd.saturating_sub(self.last_advertised_rwnd) >= self.cfg.receive_window / 2
            {
                self.sack_now = true;
            }
        }
        Some(e)
    }

    pub fn poll_transmit(&mut self) -> Option<Bytes> {
        let p = self.build_packet()?;
        self.stats.packets_sent += 1;
        Some(p)
    }

    fn make_sack(&mut self) -> Chunk {
        let a_rwnd = self.rx.a_rwnd();
        self.last_advertised_rwnd = a_rwnd;
        self.sack_now = false;
        self.sack_timer = None;
        self.packets_since_sack = 0;
        Chunk::Sack(SackChunk {
            cum_tsn_ack: self.rx.cum as u32,
            a_rwnd,
            gap_blocks: self.rx.gap_blocks(MAX_GAP_BLOCKS),
            dup_tsns: self.rx.take_dups(),
        })
    }

    fn can_send_new_data(&self) -> bool {
        matches!(
            self.state,
            AssociationState::Established
                | AssociationState::ShutdownPending
                | AssociationState::ShutdownReceived
        )
    }

    fn has_sendable_data(&self) -> bool {
        if !self.can_send_new_data() && self.state != AssociationState::ShutdownSent {
            return false;
        }
        let rtx = self
            .outstanding
            .iter()
            .any(|c| c.needs_rtx && !c.acked && !c.abandoned);
        if rtx && (self.fast_rtx_pending || self.flight_size < self.cwnd) {
            return true;
        }
        self.can_send_new_data()
            && !self.send_queue.is_empty()
            && self.flight_size < self.cwnd
            && (self.peer_rwnd > 0 || self.flight_size == 0)
            && self.outstanding.len() < MAX_OUTSTANDING_CHUNKS
    }

    fn max_payload(&self) -> usize {
        (self.cfg.mtu - chunk::COMMON_HEADER_LEN - chunk::DATA_HEADER_LEN) & !3
    }

    fn build_packet(&mut self) -> Option<Bytes> {
        if let Some(p) = self.raw_tx.pop_front() {
            return Some(p);
        }
        if self.dead || self.peer_tag == 0 {
            return None;
        }
        let mtu = self.cfg.mtu;
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut size = chunk::COMMON_HEADER_LEN;
        match self.state {
            AssociationState::Closed | AssociationState::CookieWait => return None,
            AssociationState::CookieEchoed => {
                if self.cookie_echo_pending
                    && let Some(c) = &self.cookie
                {
                    chunks.push(Chunk::CookieEcho { cookie: c.clone() });
                    self.cookie_echo_pending = false;
                    if self.cookie_sent_at.is_none() {
                        self.cookie_sent_at = Some(self.now);
                    }
                    // INIT-ACK の未知のパラメタの報告（COOKIE-ECHO の後ろに付ける）。
                    while let Some(c) = self.ctrl.pop_front() {
                        if let Chunk::Error { .. } = c {
                            let l = c.encoded_len();
                            if size + chunks[0].encoded_len() + l <= mtu {
                                size += l;
                                chunks.push(c);
                            }
                        }
                    }
                }
                if chunks.is_empty() {
                    return None;
                }
                return Some(self.packet(self.peer_tag, &chunks));
            }
            _ => {}
        }
        self.try_send_reconfig();
        if self.cookie_ack_pending {
            self.cookie_ack_pending = false;
            chunks.push(Chunk::CookieAck);
            size += 4;
        }
        let data_ready = self.has_sendable_data();
        if self.sack_now || (self.sack_timer.is_some() && data_ready) {
            let s = self.make_sack();
            size += s.encoded_len();
            chunks.push(s);
        }
        while let Some(c) = self.ctrl.front() {
            let l = c.encoded_len();
            if size + l > mtu {
                if chunks.is_empty() {
                    self.ctrl.pop_front();
                    continue;
                }
                break;
            }
            size += l;
            if let Some(c) = self.ctrl.pop_front() {
                chunks.push(c);
            }
        }
        if self.fwd_tsn_pending {
            if self.adv_peer_ack > self.cum_ack {
                let f = self.make_forward_tsn();
                let l = f.encoded_len();
                if size + l <= mtu {
                    size += l;
                    chunks.push(f);
                    self.fwd_tsn_pending = false;
                    self.stats.forward_tsn_sent += 1;
                    if self.t3.is_none() {
                        self.t3 = Some(self.now + self.rto);
                    }
                }
            } else {
                self.fwd_tsn_pending = false;
            }
        }
        if self.can_send_new_data() || self.state == AssociationState::ShutdownSent {
            let before = chunks.len();
            self.add_retransmissions(&mut chunks, &mut size);
            if self.can_send_new_data() {
                self.add_new_data(&mut chunks, &mut size);
            }
            if chunks.len() > before {
                if self.t3.is_none() {
                    self.t3 = Some(self.now + self.rto);
                }
                if self.hb_pending.is_none()
                    && let Some(iv) = self.cfg.heartbeat_interval
                {
                    self.hb_timer = Some(self.now + iv);
                }
            }
        }
        if chunks.is_empty() {
            return None;
        }
        // SACK を送ったら遅延 SACK は要らない（make_sack で消している）。
        Some(self.packet(self.peer_tag, &chunks))
    }

    fn add_retransmissions(&mut self, chunks: &mut Vec<Chunk>, size: &mut usize) {
        let now = self.now;
        let mtu = self.cfg.mtu;
        let ignore_cwnd = std::mem::take(&mut self.fast_rtx_pending);
        let mut i = 0;
        while i < self.outstanding.len() {
            let c = &self.outstanding[i];
            if !c.needs_rtx || c.acked || c.abandoned {
                i += 1;
                continue;
            }
            if c.expiry.is_some_and(|e| now >= e) {
                let id = c.msg_id;
                self.abandon_message(id);
                i += 1;
                continue;
            }
            if !ignore_cwnd && self.flight_size >= self.cwnd {
                break;
            }
            let l = chunk::data_chunk_len(c.len());
            if *size + l > mtu {
                break;
            }
            let c = &mut self.outstanding[i];
            c.needs_rtx = false;
            c.in_flight = true;
            c.tx_count += 1;
            c.miss = 0;
            self.flight_size += c.payload.len();
            self.peer_rwnd = self.peer_rwnd.saturating_sub(c.payload.len());
            if self.rtt_probe.is_some_and(|(t, _)| t == c.tsn) {
                self.rtt_probe = None;
            }
            chunks.push(c.to_chunk());
            *size += l;
            self.stats.retransmitted_chunks += 1;
            self.stats.data_chunks_sent += 1;
            i += 1;
        }
    }

    fn add_new_data(&mut self, chunks: &mut Vec<Chunk>, size: &mut usize) {
        let now = self.now;
        let mtu = self.cfg.mtu;
        let max_payload = self.max_payload();
        loop {
            if self.outstanding.len() >= MAX_OUTSTANDING_CHUNKS || self.flight_size >= self.cwnd {
                break;
            }
            let Some(m) = self.send_queue.front() else {
                break;
            };
            if m.expiry.is_some_and(|e| now >= e) {
                let id = m.msg_id;
                if m.offset == 0 {
                    if let Some(m) = self.send_queue.pop_front() {
                        self.dec_buffered(m.stream, m.data.len());
                        self.stats.abandoned_messages += 1;
                    }
                } else {
                    self.abandon_message(id);
                }
                continue;
            }
            let remaining = m.data.len() - m.offset;
            let room = mtu.saturating_sub(*size);
            if room < chunk::DATA_HEADER_LEN + 4 {
                break;
            }
            let fit = (room - chunk::DATA_HEADER_LEN) & !3;
            let take = if remaining <= max_payload && chunk::data_chunk_len(remaining) <= room {
                remaining
            } else {
                remaining.min(max_payload).min(fit)
            };
            if take == 0 {
                break;
            }
            // 小さな断片を作らない（残りが入らなければ次のパケットに満杯で入れる）。
            if take < remaining && take < max_payload && !chunks.is_empty() {
                break;
            }
            if take > self.peer_rwnd && self.flight_size > 0 {
                break;
            }
            let Some(m) = self.send_queue.front_mut() else {
                break;
            };
            if m.offset == 0 && !m.unordered {
                let so = self.streams_out.entry(m.stream).or_default();
                m.ssn = so.next_ssn;
                so.next_ssn = so.next_ssn.wrapping_add(1);
            }
            let payload = m.data.slice(m.offset..m.offset + take);
            let begin = m.offset == 0;
            m.offset += take;
            let end = m.offset == m.data.len();
            let oc = OutChunk {
                tsn: self.next_tsn,
                msg_id: m.msg_id,
                stream: m.stream,
                ssn: if m.unordered { 0 } else { m.ssn },
                ppid: m.ppid,
                unordered: m.unordered,
                begin,
                end,
                payload,
                tx_count: 1,
                acked: false,
                abandoned: false,
                in_flight: true,
                needs_rtx: false,
                miss: 0,
                fast_rtx_done: false,
                max_rtx: m.max_rtx,
                expiry: m.expiry,
            };
            if end {
                self.send_queue.pop_front();
            }
            if self.rtt_probe.is_none() {
                self.rtt_probe = Some((oc.tsn, now));
            }
            self.next_tsn += 1;
            self.flight_size += take;
            self.peer_rwnd = self.peer_rwnd.saturating_sub(take);
            chunks.push(oc.to_chunk());
            *size += chunk::data_chunk_len(take);
            self.outstanding.push_back(oc);
            self.stats.data_chunks_sent += 1;
        }
    }

    // ---- 時間 ----

    pub fn poll_timeout(&self) -> Option<Instant> {
        if self.dead {
            return None;
        }
        [
            self.t1,
            self.t2,
            self.t3,
            self.sack_timer,
            self.hb_timer,
            self.reconf_timer,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        self.now = now;
        if self.dead {
            return;
        }
        let due = |t: Option<Instant>| t.is_some_and(|t| t <= now);
        if due(self.t1) {
            self.on_t1();
        }
        if !self.dead && due(self.t3) {
            self.on_t3();
        }
        if !self.dead && due(self.sack_timer) {
            self.sack_timer = None;
            self.sack_now = true;
        }
        if !self.dead && due(self.hb_timer) {
            self.on_heartbeat_timer();
        }
        if !self.dead && due(self.reconf_timer) {
            self.on_reconf_timer();
        }
        if !self.dead && due(self.t2) {
            self.on_t2();
        }
    }

    fn on_t1(&mut self) {
        self.t1_count += 1;
        if self.t1_count > self.cfg.max_init_retransmits {
            self.t1 = None;
            self.close("init timeout".into());
            return;
        }
        self.t1_rto = (self.t1_rto * 2).min(self.cfg.rto_max);
        self.t1 = Some(self.now + self.t1_rto);
        match self.state {
            AssociationState::CookieWait => {
                if let Some(p) = self.init_packet.clone() {
                    self.push_raw(p);
                }
            }
            AssociationState::CookieEchoed => self.cookie_echo_pending = true,
            _ => self.t1 = None,
        }
    }

    fn on_t3(&mut self) {
        self.t3 = None;
        if self.outstanding.iter().all(|c| c.acked) {
            return;
        }
        self.stats.t3_expirations += 1;
        if self.bump_error() {
            return;
        }
        let mtu = self.cfg.mtu;
        self.ssthresh = (self.cwnd / 2).max(4 * mtu);
        self.cwnd = mtu;
        self.partial_bytes_acked = 0;
        self.fast_recovery = None;
        self.rto = (self.rto * 2).min(self.cfg.rto_max);
        self.rtt_probe = None;
        let mut i = 0;
        while i < self.outstanding.len() {
            let c = &self.outstanding[i];
            if !c.acked && !c.abandoned && !c.needs_rtx {
                self.mark_for_rtx(i);
            }
            i += 1;
        }
        self.update_adv_peer_ack();
        if self.adv_peer_ack > self.cum_ack {
            self.fwd_tsn_pending = true;
        }
        self.t3 = Some(self.now + self.rto);
    }

    fn on_heartbeat_timer(&mut self) {
        self.hb_timer = None;
        let Some(iv) = self.cfg.heartbeat_interval else {
            return;
        };
        if self.state != AssociationState::Established {
            return;
        }
        if self.hb_pending.is_some() && self.bump_error() {
            return;
        }
        self.hb_nonce = self.hb_nonce.wrapping_add(1);
        let mut info = Vec::with_capacity(12);
        info.put_u16(chunk::pt::HEARTBEAT_INFO);
        info.put_u16(12);
        info.put_u64(self.hb_nonce);
        self.push_ctrl(Chunk::Heartbeat {
            info: Bytes::from(info),
        });
        self.hb_pending = Some((self.hb_nonce, self.now));
        self.hb_timer = Some(self.now + iv + self.rto);
    }

    fn on_reconf_timer(&mut self) {
        self.reconf_timer = None;
        let Some(in_progress) = self.reconf_in_flight.as_ref().map(|r| r.in_progress) else {
            return;
        };
        if !in_progress && self.bump_error() {
            return;
        }
        let Some(p) = self
            .reconf_in_flight
            .as_ref()
            .map(|r| self.reconfig_request_param(r))
        else {
            return;
        };
        if let Some(req) = &mut self.reconf_in_flight {
            req.in_progress = false;
        }
        self.push_reconfig_param(p);
        self.stats.reconfig_sent += 1;
        self.reconf_timer = Some(self.now + self.rto);
    }

    fn on_t2(&mut self) {
        self.t2 = None;
        if self.bump_error() {
            return;
        }
        self.t2_rto = (self.t2_rto * 2).min(self.cfg.rto_max);
        match self.state {
            AssociationState::ShutdownSent => {
                self.push_ctrl(Chunk::Shutdown {
                    cum_tsn_ack: self.rx.cum as u32,
                });
                self.t2 = Some(self.now + self.t2_rto);
            }
            AssociationState::ShutdownAckSent => {
                self.push_ctrl(Chunk::ShutdownAck);
                self.t2 = Some(self.now + self.t2_rto);
            }
            _ => {}
        }
    }
}

fn initial_cwnd(mtu: usize) -> usize {
    (4 * mtu).min((2 * mtu).max(4380))
}

#[cfg(test)]
impl Association {
    pub(crate) fn force_initial_tsn(&mut self, tsn: u32) {
        self.forced_initial_tsn = Some(tsn);
    }

    /// 試験用: 内部の数の辻褄を確かめる。
    pub(crate) fn check_invariants(&self) {
        let flight: usize = self
            .outstanding
            .iter()
            .filter(|c| c.in_flight)
            .map(|c| c.len())
            .sum();
        assert_eq!(flight, self.flight_size, "flight size");
        for (i, c) in self.outstanding.iter().enumerate() {
            assert_eq!(c.tsn, self.cum_ack + 1 + i as u64, "outstanding contiguous");
            assert!(!(c.in_flight && c.needs_rtx), "in flight and needs rtx");
        }
        assert!(self.adv_peer_ack >= self.cum_ack || !self.peer_forward_tsn);
        if !self.dead && self.outstanding.iter().any(|c| !c.acked) {
            assert!(self.t3.is_some(), "T3 must run while data is outstanding");
        }
    }

    pub(crate) fn debug_rx_stream(&self, s: u16) -> String {
        self.rx.debug_stream(s)
    }
}
