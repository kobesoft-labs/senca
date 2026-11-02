//! 1 本の SRT 接続（live mode）の状態機械。
//!
//! I/O と時計を持たない。時刻はすべて引数の `now`（単調増加する µs）で受け取り、
//! 送るべきデータグラムは `poll_transmit`、出来事は `poll_event` で取り出す。

use std::collections::{BTreeSet, VecDeque};
use std::net::SocketAddr;

use bytes::{Bytes, BytesMut};

use crate::config::{CloseReason, Config, ConfigError, ConnectedInfo, Event, MaxBandwidth, RejectReason};
use crate::crypto::{CipherMode, CryptoError, KeyLen, RxCrypto, TxCrypto, TxKmEvent, random_bytes};
use crate::handshake::*;
use crate::packet::*;

/// こちらの SRT 版（1.5.4 相当。AES-GCM は 1.5.4 の形式）。
pub const SRT_VERSION: u32 = 0x0001_0504;
/// HSv5 を話す最小の版。
const MIN_PEER_VERSION: u32 = 0x0001_0300;
/// AES-GCM の 1.5.4 形式を話す最小の版。
const MIN_GCM_VERSION: u32 = 0x0001_0504;
/// 通し番号の起点（31 ビットの巻き戻りを 64 ビットで解くため、十分大きくする）。
const ABS_BASE: u64 = 1 << 40;
const SYN_INTERVAL: u64 = 10_000;
const MIN_NAK_INTERVAL: u64 = 20_000;
const KEEPALIVE_INTERVAL: u64 = 1_000_000;
const HS_RETRY_INTERVAL: u64 = 250_000;
const PACING_SLACK: u64 = 1_000;
const UNLIMITED_BPS: u64 = 125_000_000;
const MAX_CTRL_QUEUE: usize = 1024;
const MAX_ACK_HISTORY: usize = 256;
const MAX_NAK_WORDS: usize = 350;
const MAX_LOSS_RANGES: usize = 1024;
const KM_MAX_RETRY: u32 = 10;
const DRIFT_SAMPLES: u32 = 1000;
const DRIFT_DEADBAND: i64 = 5_000;

/// `send` の失敗。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    /// まだ確立していない、または閉じた。
    NotConnected,
    /// 空、または `payload_size` を超える。
    BadLength,
    /// 送信バッファが一杯。
    BufferFull,
    /// 暗号化に失敗した（乱数源の失敗など）。
    Crypto,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SendError::NotConnected => "not connected",
            SendError::BadLength => "bad payload length",
            SendError::BufferFull => "send buffer full",
            SendError::Crypto => "encryption failed",
        })
    }
}

impl std::error::Error for SendError {}

/// API へ出す統計。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    /// 平滑化した往復時間（µs）とその揺らぎ。
    pub rtt_us: u64,
    pub rtt_var_us: u64,
    pub pkts_sent: u64,
    pub bytes_sent: u64,
    pub pkts_retransmitted: u64,
    /// 相手から NAK で報告された損失（パケット数）。
    pub pkts_snd_loss: u64,
    /// 送信側で遅すぎて捨てたパケット。
    pub pkts_snd_dropped: u64,
    pub pkts_recv: u64,
    pub bytes_recv: u64,
    /// 受信側で検出した損失（パケット数）。
    pub pkts_rcv_loss: u64,
    /// 再送フラグ付きで届いたパケット。
    pub pkts_rcv_retransmitted: u64,
    /// TSBPD に間に合わず捨てた（TLPKTDROP）パケット。
    pub pkts_rcv_dropped: u64,
    /// 重複・遅着（既に渡した番号）のパケット。
    pub pkts_rcv_duplicate: u64,
    /// 復号・認証できなかったパケット。
    pub pkts_rcv_undecrypted: u64,
    pub pkts_delivered: u64,
    /// 直近 1 秒の送信・受信レート（ビット/秒）。
    pub send_rate_bps: u64,
    pub recv_rate_bps: u64,
    pub snd_buf_pkts: usize,
    pub rcv_buf_pkts: usize,
    pub latency_ms: u16,
    pub peer_latency_ms: u16,
    pub flow_window: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Induction,
    Conclusion,
    Connected,
    Closing { deadline: u64 },
    Closed,
}

struct SndPkt {
    wire: Bytes,
    origin: u64,
    last_rexmit: Option<u64>,
}

struct Sender {
    base: u64,
    buf: VecDeque<SndPkt>,
    next_new: u64,
    acked: u64,
    loss: BTreeSet<u64>,
    next_msgno: u32,
    next_tx_time: u64,
    last_ack_progress: u64,
    last_origin: u64,
    last_new_tx: u64,
    peer_avail: u32,
    in_window_start: u64,
    in_window_bytes: u64,
    input_bps: u64,
}

enum Slot {
    Empty,
    Pkt { payload: Bytes, ts: u64 },
    Dropped,
}

struct Receiver {
    base: u64,
    slots: VecDeque<Slot>,
    contig: u64,
    ts_ref: u64,
    tsbpd_base: i64,
    drift_sum: i64,
    drift_n: u32,
    ack_no: u32,
    acks: VecDeque<(u32, u64, u64)>,
    last_ack_time: u64,
    last_ack_sent_time: u64,
    last_ack_abs: u64,
    last_ackack_abs: u64,
    last_nak_time: u64,
    next_delivery: Option<u64>,
}

impl Receiver {
    fn next_expected(&self) -> u64 {
        self.base + self.slots.len() as u64
    }
}

#[derive(Clone, Copy)]
struct RateMeter {
    start: u64,
    bytes: u64,
    pkts: u64,
    bps: u64,
    pps: u64,
}

impl RateMeter {
    fn new(now: u64) -> Self {
        RateMeter { start: now, bytes: 0, pkts: 0, bps: 0, pps: 0 }
    }

    fn add(&mut self, now: u64, bytes: usize) {
        self.roll(now);
        self.bytes += bytes as u64;
        self.pkts += 1;
    }

    fn roll(&mut self, now: u64) {
        let el = now.saturating_sub(self.start);
        if el >= 1_000_000 {
            self.bps = self.bytes * 8 * 1_000_000 / el;
            self.pps = self.pkts * 1_000_000 / el;
            self.bytes = 0;
            self.pkts = 0;
            self.start = now;
        }
    }
}

struct KmPending {
    msg: Vec<u8>,
    last_sent: u64,
    retries: u32,
}

struct CallerHs {
    cookie: u32,
    next_retry: u64,
    deadline: u64,
    km: Option<Vec<u8>>,
}

/// SRT 接続。
pub struct Connection {
    cfg: Config,
    state: State,
    local_id: u32,
    peer_id: u32,
    peer: SocketAddr,
    start: u64,
    now: u64,
    isn: u32,
    ctrl_out: VecDeque<Bytes>,
    events: VecDeque<Event>,
    stream_id: Option<String>,
    rcv_latency_us: u64,
    peer_latency_us: u64,
    latency_ms: u16,
    peer_latency_ms: u16,
    peer_version: u32,
    peer_flow_window: u32,
    caller: Option<CallerHs>,
    hs_response: Option<Bytes>,
    snd: Sender,
    rcv: Receiver,
    tx_crypto: Option<TxCrypto>,
    rx_crypto: Option<RxCrypto>,
    km_pending: Option<KmPending>,
    srtt: u64,
    rttvar: u64,
    rtt_samples: u64,
    last_send_time: u64,
    last_recv_time: u64,
    stats: Stats,
    snd_meter: RateMeter,
    rcv_meter: RateMeter,
}

fn random_u32() -> Result<u32, ConfigError> {
    let mut b = [0u8; 4];
    random_bytes(&mut b).map_err(|_| ConfigError::Rng)?;
    Ok(u32::from_be_bytes(b))
}

fn validate(cfg: &Config) -> Result<(), ConfigError> {
    let max = if cfg.passphrase.is_some() && cfg.cipher == CipherMode::AesGcm {
        MAX_DATAGRAM - HEADER_LEN - crate::crypto::GCM_TAG_LEN
    } else {
        MAX_DATAGRAM - HEADER_LEN
    };
    if cfg.payload_size == 0 || cfg.payload_size > max {
        return Err(ConfigError::PayloadSize);
    }
    if !(32..=65536).contains(&cfg.rcv_buf_pkts) || !(32..=65536).contains(&cfg.snd_buf_pkts) {
        return Err(ConfigError::BufferSize);
    }
    if cfg.stream_id.as_ref().is_some_and(|s| s.len() > MAX_SID_LEN) {
        return Err(ConfigError::StreamIdTooLong);
    }
    Ok(())
}

/// 応答側（Listener）が結論を受け入れるための材料。
pub(crate) struct AcceptInput<'a> {
    pub cfg: Config,
    pub local_id: u32,
    pub peer: SocketAddr,
    pub hs: &'a Handshake,
    pub hs_ts: u32,
    pub now: u64,
    pub stream_id: Option<String>,
    pub require_cipher: Option<CipherMode>,
}

impl Connection {
    fn blank(cfg: Config, local_id: u32, peer: SocketAddr, now: u64, isn: u32, state: State) -> Self {
        let isn_abs = ABS_BASE + isn as u64;
        Connection {
            state,
            local_id,
            peer_id: 0,
            peer,
            start: now,
            now,
            isn,
            ctrl_out: VecDeque::new(),
            events: VecDeque::new(),
            stream_id: None,
            rcv_latency_us: cfg.latency_ms as u64 * 1000,
            peer_latency_us: cfg.peer_latency_ms as u64 * 1000,
            latency_ms: cfg.latency_ms,
            peer_latency_ms: cfg.peer_latency_ms,
            peer_version: 0,
            peer_flow_window: 8192,
            caller: None,
            hs_response: None,
            snd: Sender {
                base: isn_abs,
                buf: VecDeque::new(),
                next_new: isn_abs,
                acked: isn_abs,
                loss: BTreeSet::new(),
                next_msgno: 1,
                next_tx_time: 0,
                last_ack_progress: now,
                last_origin: 0,
                last_new_tx: 0,
                peer_avail: u32::MAX,
                in_window_start: now,
                in_window_bytes: 0,
                input_bps: 0,
            },
            rcv: Receiver {
                base: isn_abs,
                slots: VecDeque::new(),
                contig: isn_abs,
                ts_ref: 0,
                tsbpd_base: now as i64,
                drift_sum: 0,
                drift_n: 0,
                ack_no: 0,
                acks: VecDeque::new(),
                last_ack_time: now,
                last_ack_sent_time: 0,
                last_ack_abs: isn_abs,
                last_ackack_abs: isn_abs,
                last_nak_time: now,
                next_delivery: None,
            },
            tx_crypto: None,
            rx_crypto: None,
            km_pending: None,
            srtt: 100_000,
            rttvar: 50_000,
            rtt_samples: 0,
            last_send_time: now,
            last_recv_time: now,
            stats: Stats::default(),
            snd_meter: RateMeter::new(now),
            rcv_meter: RateMeter::new(now),
            cfg,
        }
    }

    /// 開始側（caller）として接続を始める。最初の誘導要求が `poll_transmit` に積まれる。
    pub fn connect(cfg: Config, peer: SocketAddr, now: u64) -> Result<Self, ConfigError> {
        validate(&cfg)?;
        let local_id = (random_u32()? & 0x3FFF_FFFF).max(1);
        let isn = random_u32()? & SEQ_MASK;
        let tx = match &cfg.passphrase {
            Some(p) => Some(
                TxCrypto::new(p, cfg.key_len, cfg.cipher, cfg.km_refresh_rate, cfg.km_preannounce)
                    .map_err(|_| ConfigError::Rng)?,
            ),
            None => None,
        };
        let mut c = Connection::blank(cfg, local_id, peer, now, isn, State::Induction);
        c.caller = Some(CallerHs {
            cookie: 0,
            next_retry: now + HS_RETRY_INTERVAL,
            deadline: now + c.cfg.connect_timeout_ms * 1000,
            km: tx.as_ref().map(|t| t.km_msg().to_vec()),
        });
        c.tx_crypto = tx;
        c.send_handshake_request(now);
        Ok(c)
    }

    /// 応答側：結論の要求を検査して接続を作る。応答（結論）は `poll_transmit` に積まれる。
    pub(crate) fn accept(input: AcceptInput<'_>) -> Result<Self, RejectReason> {
        let AcceptInput { cfg, local_id, peer, hs, hs_ts, now, stream_id, require_cipher } = input;
        validate(&cfg).map_err(|_| RejectReason::IPE)?;
        let hsreq = hs
            .ext
            .iter()
            .find_map(|e| if let HsExt::HsReq(s) = e { Some(*s) } else { None })
            .ok_or(RejectReason::ROGUE)?;
        check_peer_hs(&hsreq)?;
        if hs.ext.iter().any(|e| matches!(e, HsExt::Filter(_))) {
            return Err(RejectReason::FILTER);
        }
        if hs.ext.iter().any(|e| matches!(e, HsExt::Group(_))) {
            return Err(RejectReason::GROUP);
        }
        if let Some(HsExt::Congestion(c)) = hs.ext.iter().find(|e| matches!(e, HsExt::Congestion(_)))
            && c != "live"
        {
            return Err(RejectReason::CONGESTION);
        }
        let kmreq = hs.ext.iter().find_map(|e| if let HsExt::KmReq(k) = e { Some(k.clone()) } else { None });
        let (rx, tx, km_echo) = match (&cfg.passphrase, kmreq) {
            (None, None) => (None, None, None),
            (None, Some(_)) | (Some(_), None) => return Err(RejectReason::UNSECURE),
            (Some(pass), Some(km)) => {
                let rx = RxCrypto::from_km(pass, &km).map_err(|e| match e {
                    CryptoError::BadSecret => RejectReason::BADSECRET,
                    _ => RejectReason::CRYPTO,
                })?;
                if require_cipher.is_some_and(|m| m != rx.mode()) {
                    return Err(RejectReason::CRYPTO);
                }
                if rx.mode() == CipherMode::AesGcm && hsreq.version < MIN_GCM_VERSION {
                    return Err(RejectReason::CRYPTO);
                }
                let tx = rx.make_tx(&km, cfg.km_refresh_rate, cfg.km_preannounce).ok_or(RejectReason::IPE)?;
                (Some(rx), Some(tx), Some(km))
            }
        };
        if hs.isn > SEQ_MASK {
            return Err(RejectReason::ROGUE);
        }
        let mut c = Connection::blank(cfg, local_id, peer, now, hs.isn, State::Connected);
        c.peer_id = hs.socket_id;
        c.peer_version = hsreq.version;
        c.peer_flow_window = hs.flow_window.clamp(32, 1 << 20);
        c.stream_id = stream_id;
        c.negotiate_as_responder(&hsreq);
        c.set_tsbpd_base(now, hs_ts);
        c.rx_crypto = rx;
        c.tx_crypto = tx;
        let resp = Handshake {
            version: HS_VERSION_SRT1,
            enc_field: c.tx_crypto.as_ref().map(|t| keylen_to_field(t.key_len().bytes())).unwrap_or(0),
            ext_field: HS_EXT_HSREQ | if km_echo.is_some() { HS_EXT_KMREQ } else { 0 },
            isn: hs.isn,
            mtu: hs.mtu.clamp(76, 1500),
            flow_window: c.cfg.rcv_buf_pkts as u32,
            req_type: REQ_CONCLUSION,
            socket_id: local_id,
            cookie: hs.cookie,
            peer_ip: encode_peer_ip(&peer),
            ext: {
                let mut v = vec![HsExt::HsRsp(SrtHs {
                    version: SRT_VERSION,
                    flags: (c.local_flags() & !OPT_TSBPDSND)
                        | if hsreq.flags & OPT_TSBPDRCV != 0 { OPT_TSBPDSND } else { 0 },
                    rcv_latency_ms: c.latency_ms,
                    snd_latency_ms: c.peer_latency_ms,
                })];
                if let Some(km) = km_echo {
                    v.push(HsExt::KmRsp(km));
                }
                v
            },
        };
        let pkt = build_control(ControlType::Handshake, 0, 0, c.ts(now), c.peer_id, &resp.to_bytes());
        c.hs_response = Some(pkt.clone());
        c.push_ctrl(pkt);
        let info = c.connected_info();
        c.events.push_back(Event::Connected(info));
        Ok(c)
    }

    fn local_flags(&self) -> u32 {
        let mut f = OPT_TSBPDSND | OPT_TSBPDRCV | OPT_HAICRYPT | OPT_REXMITFLG;
        if self.cfg.tlpktdrop {
            f |= OPT_TLPKTDROP;
        }
        if self.cfg.nak_report {
            f |= OPT_NAKREPORT;
        }
        f
    }

    fn negotiate_as_responder(&mut self, req: &SrtHs) {
        let mut rcv = self.cfg.latency_ms;
        let mut snd = self.cfg.peer_latency_ms;
        if req.flags & OPT_TSBPDSND != 0 {
            rcv = rcv.max(req.snd_latency_ms);
        }
        if req.flags & OPT_TSBPDRCV != 0 {
            snd = snd.max(req.rcv_latency_ms);
        }
        self.set_latency(rcv, snd);
    }

    fn set_latency(&mut self, rcv: u16, snd: u16) {
        self.latency_ms = rcv;
        self.peer_latency_ms = snd;
        self.rcv_latency_us = rcv as u64 * 1000;
        self.peer_latency_us = snd as u64 * 1000;
    }

    fn set_tsbpd_base(&mut self, now: u64, hs_ts: u32) {
        self.rcv.ts_ref = hs_ts as u64;
        self.rcv.tsbpd_base = now as i64 - hs_ts as i64;
    }

    fn connected_info(&self) -> ConnectedInfo {
        ConnectedInfo {
            peer: self.peer,
            stream_id: self.stream_id.clone(),
            latency_ms: self.latency_ms,
            peer_latency_ms: self.peer_latency_ms,
            encryption: self.tx_crypto.as_ref().map(|t| (t.mode(), t.key_len())),
            peer_version: self.peer_version,
        }
    }

    /// こちらの socket ID。
    pub fn local_id(&self) -> u32 {
        self.local_id
    }

    /// 相手の socket ID（確立前は 0）。
    pub fn peer_id(&self) -> u32 {
        self.peer_id
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    /// 接続時に受け取った（応答側）か送った（開始側）Stream ID。
    pub fn stream_id(&self) -> Option<&str> {
        self.stream_id.as_deref()
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.state, State::Connected | State::Closing { .. })
    }

    pub fn is_closed(&self) -> bool {
        self.state == State::Closed
    }

    /// 暗号の方式と鍵長（平文なら None）。
    pub fn encryption(&self) -> Option<(CipherMode, KeyLen)> {
        self.tx_crypto.as_ref().map(|t| (t.mode(), t.key_len()))
    }

    fn ts(&self, now: u64) -> u32 {
        now.saturating_sub(self.start) as u32
    }

    fn push_ctrl(&mut self, pkt: Bytes) {
        if self.ctrl_out.len() < MAX_CTRL_QUEUE {
            self.ctrl_out.push_back(pkt);
        }
    }

    fn send_ctrl(&mut self, ctype: ControlType, subtype: u16, info: u32, cif: &[u8]) {
        let pkt = build_control(ctype, subtype, info, self.ts(self.now), self.peer_id, cif);
        self.push_ctrl(pkt);
    }

    fn send_handshake_request(&mut self, now: u64) {
        let Some(hs) = &self.caller else { return };
        let mut h = Handshake {
            version: HS_VERSION_UDT4,
            enc_field: 0,
            ext_field: UDT_DGRAM,
            isn: self.isn,
            mtu: 1500,
            flow_window: self.cfg.rcv_buf_pkts as u32,
            req_type: REQ_INDUCTION,
            socket_id: self.local_id,
            cookie: 0,
            peer_ip: encode_peer_ip(&self.peer),
            ext: Vec::new(),
        };
        if self.state == State::Conclusion {
            h.version = HS_VERSION_SRT1;
            h.req_type = REQ_CONCLUSION;
            h.cookie = hs.cookie;
            h.enc_field = self.tx_crypto.as_ref().map(|t| keylen_to_field(t.key_len().bytes())).unwrap_or(0);
            h.ext_field = HS_EXT_HSREQ;
            h.ext.push(HsExt::HsReq(SrtHs {
                version: SRT_VERSION,
                flags: self.local_flags(),
                rcv_latency_ms: self.cfg.latency_ms,
                snd_latency_ms: self.cfg.peer_latency_ms,
            }));
            if let Some(km) = &hs.km {
                h.ext_field |= HS_EXT_KMREQ;
                h.ext.push(HsExt::KmReq(km.clone()));
            }
            if let Some(sid) = &self.cfg.stream_id {
                h.ext_field |= HS_EXT_CONFIG;
                h.ext.push(HsExt::Sid(sid.clone()));
            }
        }
        let pkt = build_control(ControlType::Handshake, 0, 0, self.ts(now), 0, &h.to_bytes());
        self.push_ctrl(pkt);
    }

    /// 受け取ったデータグラムを処理する。
    pub fn handle_datagram(&mut self, now: u64, from: SocketAddr, buf: &[u8]) {
        self.now = self.now.max(now);
        let now = self.now;
        if self.state == State::Closed || from != self.peer {
            return;
        }
        let Ok(pkt) = parse(buf) else { return };
        match pkt {
            Packet::Control(h, cif) => {
                if h.ctype != ControlType::Handshake && h.dest != self.local_id {
                    return;
                }
                self.last_recv_time = now;
                self.handle_control(now, h, cif);
            }
            Packet::Data(h, body) => {
                if h.dest != self.local_id || !self.is_connected() {
                    return;
                }
                self.last_recv_time = now;
                self.handle_data(now, h, body);
            }
        }
        if self.is_connected() {
            self.deliver(now);
        }
    }

    fn handle_control(&mut self, now: u64, h: ControlHeader, cif: &[u8]) {
        match h.ctype {
            ControlType::Handshake => self.handle_handshake(now, h, cif),
            _ if !self.is_connected() => {}
            ControlType::KeepAlive | ControlType::CongestionWarning | ControlType::PeerError => {}
            ControlType::Ack => self.handle_ack(now, h.info, cif),
            ControlType::Nak => self.handle_nak(cif),
            ControlType::AckAck => self.handle_ackack(now, h.info, h.timestamp),
            ControlType::DropReq => self.handle_dropreq(cif),
            ControlType::Shutdown => {
                self.flush_receiver(now);
                self.close_with(CloseReason::PeerShutdown);
            }
            ControlType::UserDefined => self.handle_ext(h.subtype, cif),
            ControlType::Unknown(_) => {}
        }
    }

    fn handle_handshake(&mut self, now: u64, h: ControlHeader, cif: &[u8]) {
        let Ok(hs) = Handshake::parse(cif) else { return };
        match self.state {
            State::Induction => {
                if h.dest != self.local_id || hs.req_type != REQ_INDUCTION {
                    if let Some(r) = RejectReason::from_req_type(hs.req_type) {
                        self.close_with(CloseReason::Rejected(r));
                    }
                    return;
                }
                if hs.version != HS_VERSION_SRT1 || hs.ext_field != SRT_MAGIC {
                    // HSv4 だけを話す相手には対応しない。
                    self.close_with(CloseReason::Rejected(RejectReason::VERSION));
                    return;
                }
                if let Some(c) = &mut self.caller {
                    c.cookie = hs.cookie;
                    c.next_retry = now + HS_RETRY_INTERVAL;
                }
                self.state = State::Conclusion;
                self.send_handshake_request(now);
            }
            State::Conclusion => {
                if h.dest != self.local_id {
                    return;
                }
                if let Some(r) = RejectReason::from_req_type(hs.req_type) {
                    self.close_with(CloseReason::Rejected(r));
                    return;
                }
                if hs.req_type != REQ_CONCLUSION {
                    return;
                }
                if let Err(r) = self.process_conclusion_response(now, &hs, h.timestamp) {
                    self.close_with(CloseReason::Rejected(r));
                }
            }
            State::Connected | State::Closing { .. } => {
                // 応答側：結論の応答が失われた相手が結論を送り直してきたら、同じ応答を返す。
                if hs.req_type == REQ_CONCLUSION
                    && hs.socket_id == self.peer_id
                    && let Some(r) = self.hs_response.clone()
                {
                    self.push_ctrl(r);
                }
            }
            State::Closed => {}
        }
    }

    fn process_conclusion_response(&mut self, now: u64, hs: &Handshake, ts: u32) -> Result<(), RejectReason> {
        if hs.version != HS_VERSION_SRT1 || hs.socket_id == 0 {
            return Err(RejectReason::VERSION);
        }
        let rsp = hs
            .ext
            .iter()
            .find_map(|e| if let HsExt::HsRsp(s) = e { Some(*s) } else { None })
            .ok_or(RejectReason::ROGUE)?;
        check_peer_hs(&rsp)?;
        let kmrsp = hs.ext.iter().find_map(|e| if let HsExt::KmRsp(k) = e { Some(k.clone()) } else { None });
        let sent_km = self.caller.as_ref().and_then(|c| c.km.clone());
        match (sent_km, kmrsp) {
            (None, None) => {}
            (None, Some(_)) => return Err(RejectReason::UNSECURE),
            (Some(_), None) => return Err(RejectReason::UNSECURE),
            (Some(sent), Some(got)) => {
                if got.len() == 4 {
                    let st = u32::from_be_bytes([got[0], got[1], got[2], got[3]]);
                    return Err(match st {
                        4 => RejectReason::BADSECRET,
                        5 => RejectReason::CRYPTO,
                        _ => RejectReason::UNSECURE,
                    });
                }
                if got != sent {
                    return Err(RejectReason::BADSECRET);
                }
                if self.cfg.cipher == CipherMode::AesGcm && rsp.version < MIN_GCM_VERSION {
                    return Err(RejectReason::CRYPTO);
                }
            }
        }
        // libsrt の HSv5 と同じく、受信も自分の SEK で行う。
        if let (Some(tx), Some(pass)) = (&self.tx_crypto, &self.cfg.passphrase) {
            self.rx_crypto = Some(RxCrypto::from_tx(pass, tx));
        }
        let mut rcv = self.cfg.latency_ms;
        let mut snd = self.cfg.peer_latency_ms;
        if rsp.flags & OPT_TSBPDSND != 0 {
            rcv = rsp.snd_latency_ms;
        }
        if rsp.flags & OPT_TSBPDRCV != 0 {
            snd = rsp.rcv_latency_ms;
        }
        self.set_latency(rcv, snd);
        self.peer_id = hs.socket_id;
        self.peer_version = rsp.version;
        self.peer_flow_window = hs.flow_window.clamp(32, 1 << 20);
        self.stream_id = self.cfg.stream_id.clone();
        self.set_tsbpd_base(now, ts);
        self.caller = None;
        self.state = State::Connected;
        self.last_recv_time = now;
        self.snd.last_ack_progress = now;
        self.rcv.last_ack_time = now;
        self.rcv.last_nak_time = now;
        let info = self.connected_info();
        self.events.push_back(Event::Connected(info));
        Ok(())
    }

    fn handle_ext(&mut self, subtype: u16, cif: &[u8]) {
        match subtype {
            EXT_KMREQ => {
                let resp = match (&mut self.rx_crypto, &self.cfg.passphrase) {
                    (Some(rx), _) => match rx.process_km(cif) {
                        Ok(()) => cif.to_vec(),
                        Err(CryptoError::BadSecret) => 4u32.to_be_bytes().to_vec(),
                        Err(_) => 5u32.to_be_bytes().to_vec(),
                    },
                    (None, _) => 3u32.to_be_bytes().to_vec(),
                };
                self.send_ctrl(ControlType::UserDefined, EXT_KMRSP, 0, &resp);
            }
            EXT_KMRSP if self.km_pending.as_ref().is_some_and(|p| p.msg == cif) => {
                self.km_pending = None;
            }
            _ => {}
        }
    }

    fn handle_data(&mut self, now: u64, h: DataHeader, body: &[u8]) {
        let abs = seq_to_abs(self.rcv.base, h.seq);
        self.stats.pkts_recv += 1;
        self.stats.bytes_recv += body.len() as u64;
        self.rcv_meter.add(now, body.len());
        if h.retransmitted {
            self.stats.pkts_rcv_retransmitted += 1;
        }
        if abs < self.rcv.base {
            self.stats.pkts_rcv_duplicate += 1;
            return;
        }
        let off = (abs - self.rcv.base) as usize;
        if off >= self.cfg.rcv_buf_pkts {
            // 受信バッファの外。相手はフロー窓を守っていない。
            self.stats.pkts_rcv_dropped += 1;
            return;
        }
        if let Some(Slot::Pkt { .. } | Slot::Dropped) = self.rcv.slots.get(off) {
            self.stats.pkts_rcv_duplicate += 1;
            return;
        }
        // 暗号を合意したら平文は受け付けない（逆も同じ）。
        let payload = match (&self.rx_crypto, h.key_flags) {
            (None, 0) => Bytes::copy_from_slice(body),
            (Some(rx), kk) if kk != 0 => {
                let mut v = body.to_vec();
                let hdr = h.to_bytes();
                if rx.decrypt(kk, &hdr, &mut v).is_err() {
                    self.stats.pkts_rcv_undecrypted += 1;
                    return;
                }
                Bytes::from(v)
            }
            _ => {
                self.stats.pkts_rcv_undecrypted += 1;
                return;
            }
        };
        let ts = ts_to_abs(self.rcv.ts_ref, h.timestamp);
        if ts > self.rcv.ts_ref {
            self.rcv.ts_ref = ts;
        }
        let next = self.rcv.next_expected();
        if abs >= next {
            let gap = abs - next;
            for _ in 0..gap {
                self.rcv.slots.push_back(Slot::Empty);
            }
            self.rcv.slots.push_back(Slot::Pkt { payload, ts });
            if gap > 0 {
                self.stats.pkts_rcv_loss += gap;
                let cif = encode_loss_list(&[(abs_to_seq(next), abs_to_seq(abs - 1))], MAX_NAK_WORDS);
                self.send_ctrl(ControlType::Nak, 0, 0, &cif);
            }
        } else {
            self.rcv.slots[off] = Slot::Pkt { payload, ts };
        }
        self.advance_contig();
    }

    fn advance_contig(&mut self) {
        let r = &mut self.rcv;
        if r.contig < r.base {
            r.contig = r.base;
        }
        while let Some(s) = r.slots.get((r.contig - r.base) as usize) {
            if matches!(s, Slot::Empty) {
                break;
            }
            r.contig += 1;
        }
    }

    fn play_time(&self, ts: u64) -> u64 {
        (self.rcv.tsbpd_base + ts as i64 + self.rcv_latency_us as i64).max(0) as u64
    }

    /// TSBPD の時刻が来たものを順に渡す。間に合わない欠落は飛ばす（TLPKTDROP）。
    fn deliver(&mut self, now: u64) {
        self.rcv.next_delivery = None;
        loop {
            if self.events.len() >= self.cfg.rcv_buf_pkts {
                // 利用側が出来事を取り出すまで渡さない（受信バッファとフロー窓で相手を抑える）
                self.rcv.next_delivery = Some(now + SYN_INTERVAL);
                break;
            }
            match self.rcv.slots.front() {
                None => break,
                Some(Slot::Dropped) => {
                    self.rcv.slots.pop_front();
                    self.rcv.base += 1;
                    self.stats.pkts_rcv_dropped += 1;
                }
                Some(Slot::Pkt { ts, .. }) => {
                    let at = self.play_time(*ts);
                    if at > now {
                        self.rcv.next_delivery = Some(at);
                        break;
                    }
                    if let Some(Slot::Pkt { payload, ts }) = self.rcv.slots.pop_front() {
                        self.rcv.base += 1;
                        self.stats.pkts_delivered += 1;
                        self.events.push_back(Event::Data { payload, src_time_us: ts, deliver_at: at });
                    }
                }
                Some(Slot::Empty) => {
                    if !self.cfg.tlpktdrop {
                        break;
                    }
                    let Some((idx, ts)) = self.rcv.slots.iter().enumerate().find_map(|(i, s)| match s {
                        Slot::Pkt { ts, .. } => Some((i, *ts)),
                        _ => None,
                    }) else {
                        break;
                    };
                    let at = self.play_time(ts);
                    if at > now {
                        self.rcv.next_delivery = Some(at);
                        break;
                    }
                    for _ in 0..idx {
                        if let Some(Slot::Empty | Slot::Dropped) = self.rcv.slots.pop_front() {
                            self.stats.pkts_rcv_dropped += 1;
                        }
                        self.rcv.base += 1;
                    }
                }
            }
        }
        self.advance_contig();
    }

    /// 閉じる前に、受け取り済みのものを時刻を待たずに順に渡す。
    fn flush_receiver(&mut self, now: u64) {
        while let Some(s) = self.rcv.slots.pop_front() {
            self.rcv.base += 1;
            match s {
                Slot::Pkt { payload, ts } => {
                    self.stats.pkts_delivered += 1;
                    self.events.push_back(Event::Data { payload, src_time_us: ts, deliver_at: now });
                }
                _ => self.stats.pkts_rcv_dropped += 1,
            }
        }
    }

    fn handle_ack(&mut self, now: u64, ack_no: u32, cif: &[u8]) {
        let Ok(seq) = be32(cif, 0) else { return };
        if seq & 0x8000_0000 != 0 {
            return;
        }
        let ack = seq_to_abs(self.snd.base, seq);
        if ack > self.snd.next_new {
            return; // 送っていない番号の ACK は無視する
        }
        if cif.len() >= 16 {
            if ack_no != 0 {
                self.send_ctrl(ControlType::AckAck, 0, ack_no, &[0, 0, 0, 0]);
            }
            let rtt = be32(cif, 4).unwrap_or(0) as u64;
            let var = be32(cif, 8).unwrap_or(0) as u64;
            let avail = be32(cif, 12).unwrap_or(0);
            if rtt > 0 && rtt < 60_000_000 {
                if self.rtt_samples == 0 {
                    self.srtt = rtt;
                    self.rttvar = var.min(60_000_000);
                } else {
                    self.srtt = (self.srtt * 7 + rtt) / 8;
                    self.rttvar = (self.rttvar * 3 + var.min(60_000_000)) / 4;
                }
                self.rtt_samples += 1;
            }
            self.snd.peer_avail = avail.max(2);
        }
        if ack > self.snd.acked {
            self.snd.acked = ack;
            self.snd.last_ack_progress = now;
        }
        while self.snd.base < self.snd.acked && !self.snd.buf.is_empty() {
            self.snd.buf.pop_front();
            self.snd.base += 1;
        }
        if self.snd.base > self.snd.next_new {
            self.snd.next_new = self.snd.base;
        }
        let base = self.snd.base;
        self.snd.loss = self.snd.loss.split_off(&base);
    }

    fn handle_nak(&mut self, cif: &[u8]) {
        let Ok(ranges) = decode_loss_list(cif, MAX_LOSS_RANGES) else { return };
        let mut drops: Vec<(u32, u32)> = Vec::new();
        for (a, b) in ranges {
            let a = seq_to_abs(self.snd.base, a);
            let b = seq_to_abs(a, b);
            if b < a || b >= self.snd.next_new {
                continue;
            }
            let n = (b - a + 1).min(self.cfg.snd_buf_pkts as u64);
            self.stats.pkts_snd_loss += n;
            let b = a + n - 1;
            if a < self.snd.base {
                // 既に捨てた番号：相手に諦めてもらう（確認済みの範囲は除く）。
                let lo = a.max(self.snd.acked);
                let hi = b.min(self.snd.base - 1);
                if lo <= hi {
                    drops.push((abs_to_seq(lo), abs_to_seq(hi)));
                }
            }
            for s in a.max(self.snd.base)..=b {
                if self.snd.loss.len() >= self.cfg.snd_buf_pkts {
                    break;
                }
                self.snd.loss.insert(s);
            }
        }
        for (lo, hi) in drops.into_iter().take(16) {
            self.send_ctrl(ControlType::DropReq, 0, 0, &words(&[lo, hi]));
        }
    }

    fn handle_ackack(&mut self, now: u64, ack_no: u32, ts: u32) {
        let Some(pos) = self.rcv.acks.iter().position(|(n, _, _)| *n == ack_no) else { return };
        let (_, sent, abs) = self.rcv.acks[pos];
        self.rcv.acks.drain(..=pos);
        self.rcv.last_ackack_abs = self.rcv.last_ackack_abs.max(abs);
        let sample = now.saturating_sub(sent).min(60_000_000);
        if self.rtt_samples == 0 {
            // 最初の測定値で初期値（100 ms）を置き換える（libsrt 1.5 と同じ）
            self.srtt = sample;
            self.rttvar = sample / 2;
        } else {
            let diff = self.srtt.abs_diff(sample);
            self.rttvar = (self.rttvar * 3 + diff) / 4;
            self.srtt = (self.srtt * 7 + sample) / 8;
        }
        self.rtt_samples += 1;
        // 時計のずれ（相手の時刻と TSBPD の基準の差）を追う。
        let ts_abs = ts_to_abs(self.rcv.ts_ref, ts);
        let d = now as i64 - (self.rcv.tsbpd_base + ts_abs as i64);
        self.rcv.drift_sum = self.rcv.drift_sum.saturating_add(d);
        self.rcv.drift_n += 1;
        if self.rcv.drift_n >= DRIFT_SAMPLES {
            let avg = self.rcv.drift_sum / self.rcv.drift_n as i64;
            if avg.abs() > DRIFT_DEADBAND {
                self.rcv.tsbpd_base += avg;
            }
            self.rcv.drift_sum = 0;
            self.rcv.drift_n = 0;
        }
    }

    fn handle_dropreq(&mut self, cif: &[u8]) {
        let (Ok(a), Ok(b)) = (be32(cif, 0), be32(cif, 4)) else { return };
        let a = seq_to_abs(self.rcv.base, a & SEQ_MASK);
        let b = seq_to_abs(a, b & SEQ_MASK);
        if b < a || b - a > self.cfg.rcv_buf_pkts as u64 {
            return;
        }
        for s in a.max(self.rcv.base)..=b {
            let off = (s - self.rcv.base) as usize;
            if off >= self.cfg.rcv_buf_pkts {
                break;
            }
            while self.rcv.slots.len() <= off {
                self.rcv.slots.push_back(Slot::Empty);
            }
            if matches!(self.rcv.slots[off], Slot::Empty) {
                self.rcv.slots[off] = Slot::Dropped;
            }
        }
        self.advance_contig();
    }

    /// メッセージを 1 つ送る（live mode：1 パケット = 1 メッセージ）。
    pub fn send(&mut self, data: &[u8], now: u64) -> Result<(), SendError> {
        self.now = self.now.max(now);
        let now = self.now;
        if self.state != State::Connected {
            return Err(SendError::NotConnected);
        }
        if data.is_empty() || data.len() > self.cfg.payload_size {
            return Err(SendError::BadLength);
        }
        self.drop_too_late(now);
        if self.snd.buf.len() >= self.cfg.snd_buf_pkts {
            return Err(SendError::BufferFull);
        }
        let abs = self.snd.base + self.snd.buf.len() as u64;
        let header = DataHeader {
            seq: abs_to_seq(abs),
            position: Position::Solo,
            in_order: false,
            key_flags: 0,
            retransmitted: false,
            msgno: self.snd.next_msgno,
            timestamp: self.ts(now),
            dest: self.peer_id,
        };
        self.snd.next_msgno = if self.snd.next_msgno >= MSGNO_MASK { 1 } else { self.snd.next_msgno + 1 };
        let mut hdr = header.to_bytes();
        let mut body = data.to_vec();
        if let Some(tx) = &mut self.tx_crypto {
            match tx.encrypt(&mut hdr, &mut body) {
                Ok(Some(TxKmEvent::Announce(km))) => {
                    self.km_pending = Some(KmPending { msg: km.clone(), last_sent: now, retries: 0 });
                    self.send_ctrl(ControlType::UserDefined, EXT_KMREQ, 0, &km);
                }
                Ok(None) => {}
                Err(_) => return Err(SendError::Crypto),
            }
        }
        let mut wire = BytesMut::with_capacity(HEADER_LEN + body.len());
        wire.extend_from_slice(&hdr);
        wire.extend_from_slice(&body);
        self.snd.buf.push_back(SndPkt { wire: wire.freeze(), origin: now, last_rexmit: None });
        self.snd.last_origin = now;
        // 入力レートの推定
        self.snd.in_window_bytes += (data.len() + HEADER_LEN) as u64;
        let el = now.saturating_sub(self.snd.in_window_start);
        if el >= 500_000 {
            let r = self.snd.in_window_bytes * 1_000_000 / el;
            self.snd.input_bps = if self.snd.input_bps == 0 { r } else { (self.snd.input_bps * 7 + r) / 8 };
            self.snd.in_window_bytes = 0;
            self.snd.in_window_start = now;
        }
        Ok(())
    }

    fn max_bw(&self) -> u64 {
        match self.cfg.max_bw {
            MaxBandwidth::Unlimited => UNLIMITED_BPS,
            MaxBandwidth::Fixed(b) => b.max(1000),
            MaxBandwidth::Relative { input_bps, overhead_pct } => {
                let base = input_bps.unwrap_or(self.snd.input_bps);
                if base == 0 {
                    UNLIMITED_BPS
                } else {
                    (base.saturating_mul(100 + overhead_pct as u64) / 100).max(1000)
                }
            }
        }
    }

    fn drop_threshold(&self) -> u64 {
        (self.peer_latency_us + self.cfg.snd_drop_delay_ms * 1000).max(1_000_000) + 2 * SYN_INTERVAL
    }

    fn drop_too_late(&mut self, now: u64) {
        if !self.cfg.tlpktdrop {
            return;
        }
        let th = self.drop_threshold();
        while let Some(p) = self.snd.buf.front() {
            if p.origin + th >= now {
                break;
            }
            self.snd.buf.pop_front();
            self.snd.base += 1;
            self.stats.pkts_snd_dropped += 1;
        }
        if self.snd.next_new < self.snd.base {
            self.snd.next_new = self.snd.base;
        }
        let base = self.snd.base;
        self.snd.loss = self.snd.loss.split_off(&base);
    }

    fn next_data_packet(&mut self, now: u64) -> Option<Bytes> {
        if now + PACING_SLACK < self.snd.next_tx_time {
            return None;
        }
        let mut out = None;
        while let Some(abs) = self.snd.loss.pop_first() {
            if abs < self.snd.base || abs >= self.snd.next_new {
                continue;
            }
            let srtt = self.srtt;
            let Some(p) = self.snd.buf.get_mut((abs - self.snd.base) as usize) else { continue };
            if p.last_rexmit.is_some_and(|t| now < t + srtt) {
                continue; // 1 RTT 以内に送り直したものは重ねない
            }
            p.last_rexmit = Some(now);
            let mut w = BytesMut::from(&p.wire[..]);
            w[4] |= 0x04; // 再送フラグ
            self.stats.pkts_retransmitted += 1;
            out = Some(w.freeze());
            break;
        }
        if out.is_none() {
            let idx = (self.snd.next_new - self.snd.base) as usize;
            let window = self.peer_flow_window.min(self.snd.peer_avail) as u64;
            if idx < self.snd.buf.len() && self.snd.next_new - self.snd.acked < window {
                if self.snd.next_new == self.snd.acked {
                    // 送信中のものがなかった：再送タイマーはここから数える
                    self.snd.last_ack_progress = now;
                }
                let p = &self.snd.buf[idx];
                self.snd.next_new += 1;
                self.snd.last_new_tx = now;
                out = Some(p.wire.clone());
            }
        }
        let pkt = out?;
        let cost = (pkt.len() as u64 + 28) * 1_000_000 / self.max_bw();
        self.snd.next_tx_time = self.snd.next_tx_time.max(now.saturating_sub(PACING_SLACK)) + cost;
        self.stats.pkts_sent += 1;
        self.stats.bytes_sent += (pkt.len() - HEADER_LEN) as u64;
        self.snd_meter.add(now, pkt.len() - HEADER_LEN);
        Some(pkt)
    }

    /// 送るべきデータグラムを 1 つ取り出す。
    pub fn poll_transmit(&mut self) -> Option<(SocketAddr, Bytes)> {
        let now = self.now;
        if let Some(p) = self.ctrl_out.pop_front() {
            self.last_send_time = now;
            return Some((self.peer, p));
        }
        if !self.is_connected() {
            return None;
        }
        let p = self.next_data_packet(now)?;
        self.last_send_time = now;
        Some((self.peer, p))
    }

    /// 出来事を 1 つ取り出す。
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    fn has_pending_data(&self) -> bool {
        let idx = (self.snd.next_new - self.snd.base) as usize;
        let window = self.peer_flow_window.min(self.snd.peer_avail) as u64;
        !self.snd.loss.is_empty() || (idx < self.snd.buf.len() && self.snd.next_new - self.snd.acked < window)
    }

    /// 次に `handle_timeout` を呼ぶべき時刻。
    pub fn poll_timeout(&self) -> Option<u64> {
        match self.state {
            State::Closed => None,
            State::Induction | State::Conclusion => {
                let c = self.caller.as_ref()?;
                Some(c.next_retry.min(c.deadline))
            }
            State::Connected | State::Closing { .. } => {
                let mut t = self.rcv.last_ack_time + SYN_INTERVAL;
                t = t.min(self.last_send_time + KEEPALIVE_INTERVAL);
                t = t.min(self.last_recv_time + self.cfg.peer_idle_timeout_ms * 1000);
                if let Some(d) = self.rcv.next_delivery {
                    t = t.min(d);
                }
                if self.has_pending_data() {
                    t = t.min(self.snd.next_tx_time.saturating_sub(PACING_SLACK).max(self.now));
                }
                if self.cfg.nak_report && self.rcv.contig < self.rcv.next_expected() {
                    t = t.min(self.rcv.last_nak_time + self.nak_interval());
                }
                if let Some(k) = &self.km_pending {
                    t = t.min(k.last_sent + self.km_interval());
                }
                if self.snd.acked < self.snd.next_new && self.sender_idle() {
                    t = t.min(self.snd.last_ack_progress.max(self.snd.last_new_tx) + self.rto());
                }
                if let Some(f) = self.snd.buf.front() {
                    t = t.min(f.origin + self.drop_threshold() + 1);
                }
                if let State::Closing { deadline } = self.state {
                    t = t.min(deadline);
                    if self.all_acked() {
                        t = t.min(self.drained_at());
                    }
                }
                Some(t)
            }
        }
    }

    fn sender_idle(&self) -> bool {
        self.snd.next_new >= self.snd.base + self.snd.buf.len() as u64
    }

    fn tail_rexmit_due(&self, now: u64) -> bool {
        self.snd.acked < self.snd.next_new
            && self.sender_idle()
            && now >= self.snd.last_ack_progress.max(self.snd.last_new_tx) + self.rto()
    }

    fn all_acked(&self) -> bool {
        self.snd.acked >= self.snd.base + self.snd.buf.len() as u64
    }

    /// 相手が最後のパケットを TSBPD で渡し終える頃（libsrt は SHUTDOWN で受信バッファを捨てるため、
    /// それまで SHUTDOWN を待つ）。
    fn drained_at(&self) -> u64 {
        self.snd.last_origin + self.peer_latency_us + 4 * SYN_INTERVAL
    }

    fn nak_interval(&self) -> u64 {
        ((self.srtt + 4 * self.rttvar) / 2).max(MIN_NAK_INTERVAL)
    }

    fn km_interval(&self) -> u64 {
        (self.srtt * 3 / 2).max(50_000)
    }

    fn rto(&self) -> u64 {
        self.srtt + 4 * self.rttvar + 2 * SYN_INTERVAL
    }

    /// 時刻を進める（再送・ACK・NAK・keepalive・時間切れ・TSBPD の受け渡し）。
    pub fn handle_timeout(&mut self, now: u64) {
        self.now = self.now.max(now);
        let now = self.now;
        match self.state {
            State::Closed => {}
            State::Induction | State::Conclusion => {
                let Some(c) = &mut self.caller else { return };
                if now >= c.deadline {
                    self.close_with(CloseReason::ConnectTimeout);
                } else if now >= c.next_retry {
                    c.next_retry = now + HS_RETRY_INTERVAL;
                    self.send_handshake_request(now);
                }
            }
            State::Connected | State::Closing { .. } => {
                if now >= self.last_recv_time + self.cfg.peer_idle_timeout_ms * 1000 {
                    self.flush_receiver(now);
                    self.close_with(CloseReason::PeerIdle);
                    return;
                }
                self.deliver(now);
                if now >= self.rcv.last_ack_time + SYN_INTERVAL {
                    self.send_ack(now);
                }
                if self.cfg.nak_report && now >= self.rcv.last_nak_time + self.nak_interval() {
                    self.send_periodic_nak(now);
                }
                self.drop_too_late(now);
                if self.tail_rexmit_due(now) {
                    // 送るものが尽きて ACK も進まない：最後尾の損失は後続がないため受信側が
                    // 検出できない（NAK が来ない）。未確認の分を送り直す。
                    let hi = self.snd.next_new.min(self.snd.acked.max(self.snd.base) + 256);
                    for s in self.snd.acked.max(self.snd.base)..hi {
                        self.snd.loss.insert(s);
                    }
                    self.snd.last_ack_progress = now;
                }
                if let Some(k) = &mut self.km_pending
                    && now >= k.last_sent + (self.srtt * 3 / 2).max(50_000)
                {
                    if k.retries >= KM_MAX_RETRY {
                        self.km_pending = None;
                    } else {
                        k.retries += 1;
                        k.last_sent = now;
                        let m = k.msg.clone();
                        self.send_ctrl(ControlType::UserDefined, EXT_KMREQ, 0, &m);
                    }
                }
                if now >= self.last_send_time + KEEPALIVE_INTERVAL && self.ctrl_out.is_empty() {
                    self.send_ctrl(ControlType::KeepAlive, 0, 0, &[0, 0, 0, 0]);
                }
                if let State::Closing { deadline } = self.state
                    && (now >= deadline || (self.all_acked() && now >= self.drained_at()))
                {
                    self.send_ctrl(ControlType::Shutdown, 0, 0, &[0, 0, 0, 0]);
                    self.close_with(CloseReason::Local);
                }
            }
        }
    }

    fn send_ack(&mut self, now: u64) {
        self.rcv.last_ack_time = now;
        let ack = self.rcv.contig;
        if ack <= self.rcv.last_ackack_abs {
            return; // 相手が確認済み
        }
        if ack == self.rcv.last_ack_abs && now < self.rcv.last_ack_sent_time + self.srtt + 4 * self.rttvar {
            return; // 同じ ACK は 1 RTT 待ってから繰り返す
        }
        self.rcv.ack_no = self.rcv.ack_no.wrapping_add(1).max(1);
        let n = self.rcv.ack_no;
        let used = (ack.saturating_sub(self.rcv.base)) as usize;
        let avail = self.cfg.rcv_buf_pkts.saturating_sub(used).max(2) as u32;
        self.rcv_meter.roll(now);
        let cif = words(&[
            abs_to_seq(ack),
            self.srtt as u32,
            self.rttvar as u32,
            avail,
            self.rcv_meter.pps as u32,
            self.rcv_meter.pps as u32,
            (self.rcv_meter.bps / 8) as u32,
        ]);
        self.send_ctrl(ControlType::Ack, 0, n, &cif);
        if self.rcv.acks.len() >= MAX_ACK_HISTORY {
            self.rcv.acks.pop_front();
        }
        self.rcv.acks.push_back((n, now, ack));
        self.rcv.last_ack_abs = ack;
        self.rcv.last_ack_sent_time = now;
    }

    fn send_periodic_nak(&mut self, now: u64) {
        self.rcv.last_nak_time = now;
        let mut ranges = Vec::new();
        let mut i = (self.rcv.contig.saturating_sub(self.rcv.base)) as usize;
        let n = self.rcv.slots.len();
        let mut wordsn = 0;
        while i < n && wordsn < MAX_NAK_WORDS {
            if matches!(self.rcv.slots[i], Slot::Empty) {
                let start = i;
                while i + 1 < n && matches!(self.rcv.slots[i + 1], Slot::Empty) {
                    i += 1;
                }
                let a = self.rcv.base + start as u64;
                let b = self.rcv.base + i as u64;
                ranges.push((abs_to_seq(a), abs_to_seq(b)));
                wordsn += if a == b { 1 } else { 2 };
            }
            i += 1;
        }
        if !ranges.is_empty() {
            let cif = encode_loss_list(&ranges, MAX_NAK_WORDS);
            self.send_ctrl(ControlType::Nak, 0, 0, &cif);
        }
    }

    /// 閉じる。未確認のデータがあれば、すべて確認され相手の受信遅延が過ぎるまで（最長 `linger_ms`）
    /// 送り続けてから SHUTDOWN を送る。
    pub fn close(&mut self, now: u64) {
        self.now = self.now.max(now);
        let now = self.now;
        match self.state {
            State::Connected => {
                let pending = !self.all_acked() || now < self.drained_at();
                if pending && self.cfg.linger_ms > 0 {
                    self.state = State::Closing { deadline: now + self.cfg.linger_ms * 1000 };
                } else {
                    self.send_ctrl(ControlType::Shutdown, 0, 0, &[0, 0, 0, 0]);
                    self.close_with(CloseReason::Local);
                }
            }
            State::Closing { .. } => {}
            State::Induction | State::Conclusion => self.close_with(CloseReason::Local),
            State::Closed => {}
        }
    }

    fn close_with(&mut self, reason: CloseReason) {
        if self.state == State::Closed {
            return;
        }
        self.state = State::Closed;
        self.snd.buf.clear();
        self.snd.loss.clear();
        self.rcv.slots.clear();
        self.km_pending = None;
        self.events.push_back(Event::Closed { reason });
    }

    /// 統計。
    pub fn stats(&self) -> Stats {
        let mut s = self.stats.clone();
        s.rtt_us = self.srtt;
        s.rtt_var_us = self.rttvar;
        let mut sm = self.snd_meter;
        sm.roll(self.now);
        let mut rm = self.rcv_meter;
        rm.roll(self.now);
        s.send_rate_bps = sm.bps;
        s.recv_rate_bps = rm.bps;
        s.snd_buf_pkts = self.snd.buf.len();
        s.rcv_buf_pkts = self.rcv.slots.len();
        s.latency_ms = self.latency_ms;
        s.peer_latency_ms = self.peer_latency_ms;
        s.flow_window = self.peer_flow_window.min(self.snd.peer_avail);
        s
    }

    /// 試験用：送信の時刻の起点と初期シーケンス番号を差し替える（巻き戻りの試験）。
    #[cfg(test)]
    pub(crate) fn force_start_isn(&mut self, start: u64, isn: u32) {
        let abs = ABS_BASE + (isn & SEQ_MASK) as u64;
        self.start = start;
        self.isn = isn & SEQ_MASK;
        self.snd.base = abs;
        self.snd.next_new = abs;
        self.snd.acked = abs;
        self.rcv.base = abs;
        self.rcv.contig = abs;
        self.rcv.last_ack_abs = abs;
        self.rcv.last_ackack_abs = abs;
        self.ctrl_out.clear();
        self.send_handshake_request(self.now);
    }

    /// 応答側：結論の応答を送り直す。
    pub(crate) fn resend_hs_response(&mut self) {
        if self.is_connected()
            && let Some(r) = self.hs_response.clone()
        {
            self.push_ctrl(r);
        }
    }

    /// Listener が出力を出し切ったか（閉じた接続を片付けてよいか）。
    pub(crate) fn is_drained(&self) -> bool {
        self.state == State::Closed && self.ctrl_out.is_empty() && self.events.is_empty()
    }
}

/// 相手の HSREQ/HSRSP の検査（live mode のメッセージ API と TSBPD が前提）。
fn check_peer_hs(s: &SrtHs) -> Result<(), RejectReason> {
    if s.version < MIN_PEER_VERSION {
        return Err(RejectReason::VERSION);
    }
    if s.flags & OPT_STREAM != 0 {
        return Err(RejectReason::MESSAGEAPI);
    }
    Ok(())
}
