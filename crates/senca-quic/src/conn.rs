//! 一つの QUIC 接続（RFC 9000・9001・9002）。sans-io の状態機械。

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::{Bytes, BytesMut};
use senca_tls::{CipherSuite, Level, QuicConnection, QuicOutput, Secret};
use subtle::ConstantTimeEq;

use crate::cid::{ConnectionId, ResetToken};
use crate::config::TransportConfig;
use crate::crypto::{CryptoFailure, DirKeys, HeaderKey, HmacKey, Keys, PacketKey, TAG_LEN};
use crate::error::{ConnectionError, StreamError, TransportError, TransportErrorCode, terr};
use crate::frame::{self, Frame, ty};
use crate::packet::{self, Header, LongType};
use crate::ranges::RangeSet;
use crate::recovery::{NewReno, PACKET_THRESHOLD, Pacer, Rtt, SentFrame, SentPacket};
pub use crate::streams::{RecvChunk, Side};
use crate::streams::{Assembler, Dir, SendBuffer, StreamEvent, StreamId, Streams};
use crate::transport_params::TransportParameters;
use crate::{VERSION_1, varint};

/// 送る UDP datagram。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transmit {
    pub destination: SocketAddr,
    pub contents: Vec<u8>,
}

/// アプリケーションへのイベント。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// TLS ハンドシェイクが終わった（ALPN などが読める）。
    HandshakeCompleted,
    /// 相手が stream を開いた。
    StreamOpened { id: StreamId },
    /// 読めるデータ・FIN・リセットがある。
    StreamReadable { id: StreamId },
    /// 送信バッファに空きができた。
    StreamWritable { id: StreamId },
    /// 相手が STOP_SENDING を送った。
    StreamStopped { id: StreamId, code: u64 },
    /// 送ったデータと FIN（またはリセット）がすべて ACK された。
    StreamFinished { id: StreamId },
    /// datagram を受け取った（[`Connection::recv_datagram`] で読む）。
    DatagramReceived,
    /// 相手のアドレスが変わり、新しい経路に移った。
    PathMigrated { remote: SocketAddr },
    /// 1-RTT の鍵を更新した（自分または相手の開始）。
    KeyUpdated,
    /// 接続が終わった（この後は送受信できない）。
    ConnectionLost(ConnectionError),
}

/// 統計。
#[derive(Debug, Clone, Default)]
pub struct ConnectionStats {
    pub sent_packets: u64,
    pub sent_bytes: u64,
    pub recv_packets: u64,
    pub recv_bytes: u64,
    pub lost_packets: u64,
    pub dropped_packets: u64,
    pub pto_count: u64,
    pub key_updates: u64,
    pub migrations: u64,
    pub datagrams_received: u64,
    pub datagrams_sent: u64,
    pub rtt: Duration,
    pub cwnd: u64,
}

/// endpoint への通知（connection ID の登録など）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToEndpoint {
    NewCid(ConnectionId),
    RetireCid(ConnectionId),
    ResetToken(ResetToken),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SpaceId {
    Initial = 0,
    Handshake = 1,
    Data = 2,
}

const SPACES: [SpaceId; 3] = [SpaceId::Initial, SpaceId::Handshake, SpaceId::Data];

impl SpaceId {
    fn level(self) -> Level {
        match self {
            SpaceId::Initial => Level::Initial,
            SpaceId::Handshake => Level::Handshake,
            SpaceId::Data => Level::OneRtt,
        }
    }
    fn from_level(l: Level) -> Self {
        match l {
            Level::Initial => SpaceId::Initial,
            Level::Handshake => SpaceId::Handshake,
            Level::OneRtt => SpaceId::Data,
        }
    }
}

/// 受信した packet number の区間の上限（古いものから忘れる）。
const MAX_RECV_RANGES: usize = 64;
/// 鍵が無いうちに届いたパケットを抱える数。
const MAX_UNDECRYPTABLE: usize = 8;
/// 相手の PATH_CHALLENGE に応える待ちの上限。
const MAX_PATH_RESPONSES: usize = 4;
/// 相手から受け付ける RETIRE 待ちの上限。
const MAX_PENDING_RETIRE: usize = 32;
/// AEAD の完全性の上限（RFC 9001 6.6。AES-GCM は 2^52、ChaCha20 は 2^36。小さい方）。
const AEAD_INTEGRITY_LIMIT: u64 = 1 << 36;

#[derive(Debug, Default)]
struct Space {
    local: Option<DirKeys>,
    remote: Option<DirKeys>,
    discarded: bool,
    next_pn: u64,
    largest_acked: Option<u64>,
    sent: BTreeMap<u64, SentPacket>,
    ack_eliciting_in_flight: usize,
    last_ack_eliciting_sent: Option<Instant>,
    loss_time: Option<Instant>,
    // 受信
    recv: RangeSet,
    recv_floor: u64,
    largest_recv: Option<(u64, Instant)>,
    unacked_eliciting: u32,
    ack_immediate: bool,
    ack_deadline: Option<Instant>,
    // CRYPTO
    crypto_send: SendBuffer,
    crypto_recv: Assembler,
    // PTO のプローブ
    probes: u8,
}

impl Space {
    fn has_ack_to_send(&self) -> bool {
        self.largest_recv.is_some() && (self.ack_immediate || self.unacked_eliciting > 0)
    }
}

/// 1-RTT の鍵（鍵更新つき、RFC 9001 6 章）。
struct OneRtt {
    suite: CipherSuite,
    local_secret: Secret,
    remote_secret: Secret,
    local: PacketKey,
    remote: PacketKey,
    next_local: PacketKey,
    next_remote: PacketKey,
    prev_remote: Option<(PacketKey, Instant)>,
    hp_local: HeaderKey,
    hp_remote: HeaderKey,
    phase: bool,
    /// この鍵で最初に送ったパケット。
    first_sent: Option<u64>,
    /// この鍵で送ったパケットが ACK された（次の更新を始めてよい）。
    update_acked: bool,
    /// この鍵で最初に受けたパケット。
    first_recv: Option<u64>,
    sent_in_phase: u64,
}

impl std::fmt::Debug for OneRtt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneRtt").field("phase", &self.phase).finish_non_exhaustive()
    }
}

impl OneRtt {
    fn new(suite: CipherSuite, local_secret: Secret, remote_secret: Secret) -> Result<Self, CryptoFailure> {
        let l = DirKeys::from_secret(suite, &local_secret)?;
        let r = DirKeys::from_secret(suite, &remote_secret)?;
        let nl = next_secret(suite, &local_secret)?;
        let nr = next_secret(suite, &remote_secret)?;
        Ok(Self {
            suite,
            next_local: PacketKey::from_secret(suite, &nl)?,
            next_remote: PacketKey::from_secret(suite, &nr)?,
            local_secret,
            remote_secret,
            local: l.packet,
            remote: r.packet,
            prev_remote: None,
            hp_local: l.header,
            hp_remote: r.header,
            phase: false,
            first_sent: None,
            update_acked: false,
            first_recv: None,
            sent_in_phase: 0,
        })
    }

    /// 次の世代へ進める（送受信とも）。
    fn rotate(&mut self, discard_prev_at: Instant) -> Result<(), CryptoFailure> {
        let nl = next_secret(self.suite, &self.local_secret)?;
        let nr = next_secret(self.suite, &self.remote_secret)?;
        let nnl = next_secret(self.suite, &nl)?;
        let nnr = next_secret(self.suite, &nr)?;
        let new_next_local = PacketKey::from_secret(self.suite, &nnl)?;
        let new_next_remote = PacketKey::from_secret(self.suite, &nnr)?;
        let old_remote = std::mem::replace(&mut self.remote, std::mem::replace(&mut self.next_remote, new_next_remote));
        self.local = std::mem::replace(&mut self.next_local, new_next_local);
        self.prev_remote = Some((old_remote, discard_prev_at));
        self.local_secret = nl;
        self.remote_secret = nr;
        self.phase = !self.phase;
        self.first_sent = None;
        self.update_acked = false;
        self.first_recv = None;
        self.sent_in_phase = 0;
        Ok(())
    }
}

fn next_secret(suite: CipherSuite, s: &Secret) -> Result<Secret, CryptoFailure> {
    senca_tls::quic::next_secret(suite, s).map_err(|_| CryptoFailure)
}

#[derive(Debug, Clone)]
struct Path {
    remote: SocketAddr,
    validated: bool,
    sent: u64,
    recv: u64,
    challenge: Option<[u8; 8]>,
    challenge_pending: bool,
    challenge_deadline: Option<Instant>,
}

impl Path {
    fn new(remote: SocketAddr, validated: bool) -> Self {
        Self {
            remote,
            validated,
            sent: 0,
            recv: 0,
            challenge: None,
            challenge_pending: false,
            challenge_deadline: None,
        }
    }

    /// anti-amplification（RFC 9000 8.1）: 検証前は受信量の 3 倍まで。
    fn budget(&self) -> u64 {
        if self.validated { u64::MAX } else { (self.recv * 3).saturating_sub(self.sent) }
    }
}

#[derive(Debug, Clone)]
struct LocalCid {
    seq: u64,
    cid: ConnectionId,
    token: ResetToken,
}

#[derive(Debug, Clone)]
struct PeerCid {
    cid: ConnectionId,
    token: Option<ResetToken>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Handshake,
    Established,
    Closing { until: Instant },
    Draining { until: Instant },
    Drained,
}

#[derive(Debug, Clone)]
struct CloseFrame {
    app: bool,
    code: u64,
    frame_type: u64,
    reason: Vec<u8>,
}

/// 一つの QUIC 接続。
pub struct Connection {
    side: Side,
    config: Arc<TransportConfig>,
    tls: QuicConnection,
    state: State,
    spaces: [Space; 3],
    one_rtt: Option<OneRtt>,
    pending_local_1rtt: Option<(CipherSuite, Secret)>,
    pending_remote_1rtt: Option<(CipherSuite, Secret)>,
    undecryptable: VecDeque<(SocketAddr, BytesMut)>,
    handshake_complete: bool,
    handshake_confirmed: bool,
    handshake_done_pending: bool,
    peer_params: Option<TransportParameters>,
    // connection ID
    cid_key: HmacKey,
    cid_len: usize,
    local_cids: Vec<LocalCid>,
    next_local_seq: u64,
    new_cid_pending: VecDeque<u64>,
    peer_cids: BTreeMap<u64, PeerCid>,
    active_peer_seq: u64,
    peer_retire_prior_to: u64,
    retire_pending: VecDeque<u64>,
    /// client: 最初に送った DCID（Initial 鍵と original_destination_connection_id）。
    orig_dcid: ConnectionId,
    /// client: Retry の SCID。
    retry_scid: Option<ConnectionId>,
    retry_token: Vec<u8>,
    /// server: 相手の initial_source_connection_id として期待する値。
    expected_peer_scid: Option<ConnectionId>,
    got_server_packet: bool,
    // 経路
    path: Path,
    prev_path: Option<Path>,
    path_responses: VecDeque<(SocketAddr, [u8; 8], usize)>,
    // 回復
    rtt: Rtt,
    cc: NewReno,
    pacer: Pacer,
    pacing_until: Option<Instant>,
    pto_count: u32,
    peer_max_ack_delay: Duration,
    peer_ack_delay_exponent: u64,
    // idle
    idle_timeout: Option<Duration>,
    idle_deadline: Option<Instant>,
    eliciting_since_recv: bool,
    // datagram
    dgram_send: VecDeque<Bytes>,
    dgram_recv: VecDeque<Bytes>,
    dgram_signaled: bool,
    peer_max_datagram: u64,
    // 終了
    close: Option<CloseFrame>,
    close_pending: bool,
    close_sends: u32,
    error: Option<ConnectionError>,
    aead_failures: u64,
    // ストリーム
    streams: Streams,
    events: VecDeque<Event>,
    to_endpoint: VecDeque<ToEndpoint>,
    stats: ConnectionStats,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("side", &self.side)
            .field("state", &self.state)
            .field("remote", &self.path.remote)
            .finish_non_exhaustive()
    }
}

/// 接続を作るときの材料（endpoint が渡す）。
pub(crate) struct ConnParams {
    pub side: Side,
    pub config: Arc<TransportConfig>,
    pub remote: SocketAddr,
    pub local_cid: ConnectionId,
    pub cid_key: HmacKey,
    pub cid_len: usize,
    pub now: Instant,
}

impl Connection {
    fn base(p: &ConnParams, tls: QuicConnection, dcid: ConnectionId, orig_dcid: ConnectionId, validated: bool) -> Self {
        let token = p.cid_key.reset_token(&p.local_cid);
        let mut peer_cids = BTreeMap::new();
        peer_cids.insert(0, PeerCid { cid: dcid, token: None });
        let mut c = Self {
            side: p.side,
            config: p.config.clone(),
            tls,
            state: State::Handshake,
            spaces: Default::default(),
            one_rtt: None,
            pending_local_1rtt: None,
            pending_remote_1rtt: None,
            undecryptable: VecDeque::new(),
            handshake_complete: false,
            handshake_confirmed: false,
            handshake_done_pending: false,
            peer_params: None,
            cid_key: p.cid_key.clone(),
            cid_len: p.cid_len,
            local_cids: vec![LocalCid {
                seq: 0,
                cid: p.local_cid,
                token,
            }],
            next_local_seq: 1,
            new_cid_pending: VecDeque::new(),
            peer_cids,
            active_peer_seq: 0,
            peer_retire_prior_to: 0,
            retire_pending: VecDeque::new(),
            orig_dcid,
            retry_scid: None,
            retry_token: Vec::new(),
            expected_peer_scid: None,
            got_server_packet: false,
            path: Path::new(p.remote, validated),
            prev_path: None,
            path_responses: VecDeque::new(),
            rtt: Rtt::new(p.config.initial_rtt),
            cc: NewReno::new(p.config.max_udp_payload_size),
            pacer: Pacer::new(),
            pacing_until: None,
            pto_count: 0,
            peer_max_ack_delay: Duration::from_millis(25),
            peer_ack_delay_exponent: 3,
            idle_timeout: None,
            idle_deadline: None,
            eliciting_since_recv: false,
            dgram_send: VecDeque::new(),
            dgram_recv: VecDeque::new(),
            dgram_signaled: false,
            peer_max_datagram: 0,
            close: None,
            close_pending: false,
            close_sends: 0,
            error: None,
            aead_failures: 0,
            streams: Streams::new(p.side, &p.config),
            events: VecDeque::new(),
            to_endpoint: VecDeque::new(),
            stats: ConnectionStats::default(),
        };
        if !p.config.max_idle_timeout.is_zero() {
            c.idle_timeout = Some(p.config.max_idle_timeout);
            c.idle_deadline = Some(p.now + p.config.max_idle_timeout);
        }
        c
    }

    fn local_params(config: &TransportConfig, streams: &Streams, scid: &ConnectionId) -> TransportParameters {
        let mut tp = TransportParameters {
            max_idle_timeout: config.max_idle_timeout,
            max_udp_payload_size: 65527,
            ack_delay_exponent: 3,
            max_ack_delay: config.max_ack_delay,
            disable_active_migration: config.disable_active_migration,
            active_connection_id_limit: config.active_connection_id_limit.max(2),
            initial_source_connection_id: Some(*scid),
            max_datagram_frame_size: config.max_datagram_frame_size,
            ..Default::default()
        };
        streams.local_params(&mut tp);
        tp
    }

    /// client の接続を作る。
    pub(crate) fn new_client(
        p: ConnParams,
        tls: Arc<senca_tls::ClientConfig>,
        server_name: &str,
        wall: SystemTime,
    ) -> Result<Self, ConnectionError> {
        let dcid = ConnectionId::random(16).map_err(|_| internal("乱数源"))?;
        let streams = Streams::new(Side::Client, &p.config);
        let tp = Self::local_params(&p.config, &streams, &p.local_cid);
        let tls = QuicConnection::client(tls, server_name, tp.encode(), wall)
            .map_err(|e| internal(&format!("TLS の初期化: {e}")))?;
        let mut c = Self::base(&p, tls, dcid, dcid, true);
        let keys = Keys::initial(&dcid, true).map_err(|_| internal("Initial 鍵"))?;
        c.spaces[0].local = Some(keys.local);
        c.spaces[0].remote = Some(keys.remote);
        c.process_tls()?;
        Ok(c)
    }

    /// server の接続を作る（最初の Initial を受けたとき）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_server(
        p: ConnParams,
        tls: Arc<senca_tls::ServerConfig>,
        wall: SystemTime,
        client_dcid: ConnectionId,
        client_scid: ConnectionId,
        odcid: ConnectionId,
        retry_scid: Option<ConnectionId>,
    ) -> Result<Self, ConnectionError> {
        let streams = Streams::new(Side::Server, &p.config);
        let mut tp = Self::local_params(&p.config, &streams, &p.local_cid);
        tp.original_destination_connection_id = Some(odcid);
        tp.retry_source_connection_id = retry_scid;
        tp.stateless_reset_token = Some(p.cid_key.reset_token(&p.local_cid));
        let tls = QuicConnection::server(tls, tp.encode(), wall).map_err(|e| internal(&format!("TLS の初期化: {e}")))?;
        let mut c = Self::base(&p, tls, client_scid, odcid, retry_scid.is_some());
        let keys = Keys::initial(&client_dcid, false).map_err(|_| internal("Initial 鍵"))?;
        c.spaces[0].local = Some(keys.local);
        c.spaces[0].remote = Some(keys.remote);
        c.expected_peer_scid = Some(client_scid);
        c.retry_scid = retry_scid;
        Ok(c)
    }

    // ------------------------------------------------------------------ 公開 API

    pub fn side(&self) -> Side {
        self.side
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.path.remote
    }

    pub fn is_handshaking(&self) -> bool {
        self.state == State::Handshake
    }

    pub fn is_established(&self) -> bool {
        self.state == State::Established
    }

    /// 閉じ始めた（closing・draining・drained）。
    pub fn is_closed(&self) -> bool {
        matches!(self.state, State::Closing { .. } | State::Draining { .. } | State::Drained)
    }

    /// 完全に終わった（endpoint から取り除いてよい）。
    pub fn is_drained(&self) -> bool {
        self.state == State::Drained
    }

    pub fn is_handshake_confirmed(&self) -> bool {
        self.handshake_confirmed
    }

    /// 終わった理由。
    pub fn close_reason(&self) -> Option<&ConnectionError> {
        self.error.as_ref()
    }

    /// 交渉した ALPN。
    pub fn alpn(&self) -> Option<&[u8]> {
        self.tls.alpn_protocol()
    }

    /// client が送った SNI（server 側）。
    pub fn server_name(&self) -> Option<&str> {
        self.tls.server_name()
    }

    /// TLS の接続（証明書・exporter など）。
    pub fn tls(&self) -> &QuicConnection {
        &self.tls
    }

    pub fn peer_transport_parameters(&self) -> Option<&TransportParameters> {
        self.peer_params.as_ref()
    }

    pub fn stats(&self) -> ConnectionStats {
        let mut s = self.stats.clone();
        s.rtt = self.rtt.smoothed();
        s.cwnd = self.cc.window();
        s
    }

    /// 現在の 1-RTT の key phase。
    pub fn key_phase(&self) -> Option<bool> {
        self.one_rtt.as_ref().map(|k| k.phase)
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.pull_stream_events();
        self.events.pop_front()
    }

    pub(crate) fn poll_endpoint(&mut self) -> Option<ToEndpoint> {
        self.to_endpoint.pop_front()
    }

    pub(crate) fn local_cids(&self) -> impl Iterator<Item = ConnectionId> + '_ {
        self.local_cids.iter().map(|c| c.cid)
    }

    fn pull_stream_events(&mut self) {
        while let Some(e) = self.streams.events.pop_front() {
            self.events.push_back(match e {
                StreamEvent::Opened(id) => Event::StreamOpened { id },
                StreamEvent::Readable(id) => Event::StreamReadable { id },
                StreamEvent::Writable(id) => Event::StreamWritable { id },
                StreamEvent::Stopped(id, code) => Event::StreamStopped { id, code },
                StreamEvent::Finished(id) => Event::StreamFinished { id },
            });
        }
    }

    fn check_open(&self) -> Result<(), StreamError> {
        if self.is_closed() { Err(StreamError::ConnectionClosed) } else { Ok(()) }
    }

    /// stream を開く。相手の上限に達していれば `None`（上限が増えるまで待つ）。
    pub fn open_stream(&mut self, dir: Dir) -> Option<StreamId> {
        if self.is_closed() {
            return None;
        }
        self.streams.open(dir)
    }

    /// あと何本開けるか。
    pub fn streams_available(&self, dir: Dir) -> u64 {
        self.streams.remaining_local(dir)
    }

    /// データを送る。受け付けたバイト数を返す（送信バッファが一杯なら一部だけ）。
    pub fn stream_send(&mut self, id: StreamId, data: &[u8]) -> Result<usize, StreamError> {
        self.stream_send_bytes(id, Bytes::copy_from_slice(data))
    }

    /// [`Self::stream_send`] の `Bytes` 版（コピーしない）。
    pub fn stream_send_bytes(&mut self, id: StreamId, data: Bytes) -> Result<usize, StreamError> {
        self.check_open()?;
        self.streams.write(id, &data)
    }

    /// FIN を送る。
    pub fn stream_finish(&mut self, id: StreamId) -> Result<(), StreamError> {
        self.check_open()?;
        self.streams.finish(id)
    }

    /// 送信側を RESET_STREAM で打ち切る。
    pub fn stream_reset(&mut self, id: StreamId, code: u64) -> Result<(), StreamError> {
        self.check_open()?;
        self.streams.reset(id, code)
    }

    /// 受信側を STOP_SENDING で止める（以後のデータは捨てる）。
    pub fn stream_stop(&mut self, id: StreamId, code: u64) -> Result<(), StreamError> {
        self.check_open()?;
        self.streams.stop(id, code)
    }

    /// 最大 `max` バイト読む。FIN まで読むと `fin` が立ち、stream は解放される。
    pub fn stream_recv(&mut self, id: StreamId, max: usize) -> Result<RecvChunk, StreamError> {
        self.streams.read(id, max)
    }

    /// 送信の優先度（大きいほど先）。
    pub fn set_stream_priority(&mut self, id: StreamId, priority: i32) -> Result<(), StreamError> {
        self.streams.set_priority(id, priority)
    }

    /// stream がまだ存在するか。
    pub fn stream_exists(&self, id: StreamId) -> bool {
        self.streams.exists(id)
    }

    /// 送信側で未 ACK のバイト数。
    pub fn stream_send_buffered(&self, id: StreamId) -> Option<u64> {
        self.streams.send_buffered(id)
    }

    /// 送れる datagram の最大長（相手が DATAGRAM を受け付けなければ `None`）。
    pub fn max_datagram_size(&self) -> Option<usize> {
        if self.peer_max_datagram == 0 {
            return None;
        }
        let dcid = self.peer_cids.get(&self.active_peer_seq).map_or(20, |c| c.cid.len());
        let mtu = self.mtu();
        let room = mtu.saturating_sub(1 + dcid + 4 + TAG_LEN + 1 + 2);
        Some(room.min(self.peer_max_datagram.saturating_sub(3) as usize))
    }

    /// datagram を送る（信頼性なし）。送信待ちが多すぎると古いものから捨てる。
    pub fn send_datagram(&mut self, data: Bytes) -> Result<(), DatagramError> {
        if self.is_closed() {
            return Err(DatagramError::ConnectionClosed);
        }
        let max = self.max_datagram_size().ok_or(DatagramError::Unsupported)?;
        if data.len() > max {
            return Err(DatagramError::TooLarge);
        }
        if self.dgram_send.len() >= self.config.datagram_send_queue {
            self.dgram_send.pop_front();
        }
        self.dgram_send.push_back(data);
        Ok(())
    }

    /// 受け取った datagram。
    pub fn recv_datagram(&mut self) -> Option<Bytes> {
        let d = self.dgram_recv.pop_front();
        if d.is_none() {
            self.dgram_signaled = false;
        }
        d
    }

    /// アプリケーションのエラーコードで閉じる（APPLICATION_CLOSE）。
    pub fn close(&mut self, now: Instant, code: u64, reason: &[u8]) {
        if self.is_closed() {
            return;
        }
        let mut reason = reason.to_vec();
        reason.truncate(256);
        self.close = Some(CloseFrame {
            app: true,
            code,
            frame_type: 0,
            reason,
        });
        self.error = Some(ConnectionError::LocallyClosed);
        self.enter_closing(now);
    }

    /// 1-RTT の鍵更新を始める（RFC 9001 6.1）。ハンドシェイク確定前・前回の更新が
    /// ACK されていないときは `false`。
    pub fn initiate_key_update(&mut self, now: Instant) -> bool {
        if !self.handshake_confirmed || self.is_closed() {
            return false;
        }
        let pto = self.pto_duration(SpaceId::Data);
        let Some(k) = self.one_rtt.as_mut() else { return false };
        if !k.update_acked {
            return false;
        }
        if k.rotate(now + pto * 3).is_err() {
            return false;
        }
        self.stats.key_updates += 1;
        self.events.push_back(Event::KeyUpdated);
        true
    }

    /// client が経路を変える（ローカルのアドレス・ポートを変えた後に呼ぶ）。新しい connection ID に
    /// 切り替えて PATH_CHALLENGE を送る。未使用の connection ID が無ければ `false`。
    pub fn migrate(&mut self, now: Instant) -> bool {
        if self.side != Side::Client
            || !self.handshake_confirmed
            || self.is_closed()
            || self.peer_params.as_ref().is_some_and(|p| p.disable_active_migration)
        {
            return false;
        }
        if !self.switch_peer_cid() {
            return false;
        }
        self.start_path_challenge(now);
        self.stats.migrations += 1;
        true
    }

    // ------------------------------------------------------------------ 受信

    /// 受け取った UDP datagram を処理する。
    pub fn handle_datagram(&mut self, now: Instant, remote: SocketAddr, data: BytesMut) {
        if self.state == State::Drained {
            return;
        }
        let dlen = data.len();
        self.stats.recv_bytes += dlen as u64;
        if remote == self.path.remote {
            self.path.recv += dlen as u64;
        }
        if let State::Draining { .. } = self.state {
            return;
        }
        if let State::Closing { .. } = self.state {
            // 閉じている間は受けたら CONNECTION_CLOSE をもう一度送る（回数を抑える）
            if self.close_sends < 32 {
                self.close_pending = true;
            }
            return;
        }
        let mut rest = data;
        let mut first = true;
        while !rest.is_empty() {
            let Ok(partial) = packet::parse(&rest, self.cid_len) else {
                self.stats.dropped_packets += 1;
                break;
            };
            let len = partial.len;
            let header = to_owned_header(&partial.header, len);
            let pn_offset = partial.pn_offset;
            let pkt = rest.split_to(len);
            if let Err(e) = self.handle_packet(now, remote, header, pn_offset, pkt, dlen, first) {
                self.transport_error(now, e);
                return;
            }
            first = false;
            if self.state == State::Drained || matches!(self.state, State::Draining { .. }) {
                return;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_packet(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        header: OwnedHeader,
        pn_offset: usize,
        mut pkt: BytesMut,
        dlen: usize,
        first_in_datagram: bool,
    ) -> Result<(), TransportError> {
        let (space, long) = match &header {
            OwnedHeader::VersionNegotiation { dcid, scid, versions } => {
                self.handle_version_negotiation(now, dcid, scid, versions);
                return Ok(());
            }
            OwnedHeader::Retry { scid, token, body_len } => {
                self.handle_retry(now, *scid, token, &pkt[..*body_len], &pkt[*body_len..]);
                return Ok(());
            }
            OwnedHeader::Unsupported => return Ok(()),
            OwnedHeader::Long { ty, .. } => match ty {
                LongType::Initial => (SpaceId::Initial, true),
                LongType::Handshake => (SpaceId::Handshake, true),
                // 0-RTT は使わない（仕様 6.4）。読まずに捨てる。
                _ => return Ok(()),
            },
            OwnedHeader::Short => (SpaceId::Data, false),
        };
        // server は 1200 バイト未満の datagram の Initial を捨てる（RFC 9000 14.1）
        if space == SpaceId::Initial && self.side == Side::Server && dlen < 1200 {
            self.stats.dropped_packets += 1;
            return Ok(());
        }
        let sp = &self.spaces[space as usize];
        if sp.discarded {
            return Ok(());
        }
        let have_keys = if space == SpaceId::Data { self.one_rtt.is_some() } else { sp.remote.is_some() };
        if !have_keys {
            // 鍵がまだ無い（並べ替えで先に届いた）。少しだけ抱えておく。
            if space != SpaceId::Initial && self.undecryptable.len() < MAX_UNDECRYPTABLE {
                self.undecryptable.push_back((remote, pkt));
            }
            return Ok(());
        }
        // header protection を外す
        let hp = match space {
            SpaceId::Data => &self.one_rtt.as_ref().map(|k| &k.hp_remote),
            _ => &self.spaces[space as usize].remote.as_ref().map(|k| &k.header),
        };
        let Some(hp) = hp else { return Ok(()) };
        let Ok((first, pn_len, truncated)) = packet::unprotect(&mut pkt, pn_offset, hp) else {
            self.stats.dropped_packets += 1;
            return Ok(());
        };
        let largest = self.spaces[space as usize].largest_recv.map(|(p, _)| p);
        let pn = packet::decode_pn(largest, truncated, pn_len);
        let hdr_end = pn_offset + pn_len;
        if pkt.len() < hdr_end + TAG_LEN {
            self.stats.dropped_packets += 1;
            return Ok(());
        }
        let body_end = pkt.len() - TAG_LEN;
        // 復号
        let opened = {
            let (hdr, rest) = pkt.split_at_mut(hdr_end);
            let (body, tag) = rest.split_at_mut(body_end - hdr_end);
            match space {
                SpaceId::Data => self.open_1rtt(now, first, pn, hdr, body, tag),
                _ => self.spaces[space as usize]
                    .remote
                    .as_ref()
                    .map_or(Err(CryptoFailure), |k| k.packet.open(pn, hdr, body, tag)),
            }
        };
        if opened.is_err() {
            self.stats.dropped_packets += 1;
            self.aead_failures += 1;
            if self.aead_failures > AEAD_INTEGRITY_LIMIT {
                return Err(terr!(AEAD_LIMIT_REACHED, "AEAD の完全性の上限"));
            }
            if !long && self.is_stateless_reset(&pkt) {
                self.error = Some(ConnectionError::Reset);
                self.events.push_back(Event::ConnectionLost(ConnectionError::Reset));
                self.enter_draining(now);
            }
            return Ok(());
        }
        // 予約ビット（保護を外した後）
        let reserved = if long { first & 0x0c } else { first & 0x18 };
        if reserved != 0 {
            return Err(terr!(PROTOCOL_VIOLATION, "予約ビットが 0 でない"));
        }
        // 重複
        {
            let sp = &mut self.spaces[space as usize];
            if pn < sp.recv_floor || sp.recv.contains(pn) {
                self.stats.dropped_packets += 1;
                return Ok(());
            }
        }
        // ここから認証済み
        self.stats.recv_packets += 1;
        if let OwnedHeader::Long { scid, dcid, .. } = &header {
            self.on_long_header(space, scid, dcid)?;
        }
        if self.side == Side::Server && space == SpaceId::Handshake {
            // Handshake パケットを受けた = アドレス検証済み、Initial 鍵を捨てる（RFC 9001 4.9.1）
            self.path.validated = true;
            self.discard_space(SpaceId::Initial);
        }
        let payload = pkt.freeze().slice(hdr_end..body_end);
        if payload.is_empty() {
            return Err(terr!(PROTOCOL_VIOLATION, "空の payload"));
        }
        let is_largest = largest.is_none_or(|l| pn > l);
        let (eliciting, probing_only) = self.handle_frames(now, space, remote, &payload, dlen)?;
        // 受信の記録
        {
            let sp = &mut self.spaces[space as usize];
            let out_of_order = largest.is_some_and(|l| pn != l + 1);
            sp.recv.insert(pn..pn + 1);
            while sp.recv.len() > MAX_RECV_RANGES {
                sp.recv.remove_min_range();
                sp.recv_floor = sp.recv.min().unwrap_or(0);
            }
            if is_largest {
                sp.largest_recv = Some((pn, now));
            }
            if eliciting {
                sp.unacked_eliciting += 1;
                if space != SpaceId::Data || out_of_order || sp.unacked_eliciting >= 2 {
                    sp.ack_immediate = true;
                } else if sp.ack_deadline.is_none() {
                    sp.ack_deadline = Some(now + self.config.max_ack_delay);
                }
            }
        }
        // idle timer
        if let Some(t) = self.idle_timeout {
            self.idle_deadline = Some(now + t.max(self.pto_duration(SpaceId::Data) * 3));
        }
        self.eliciting_since_recv = false;
        // 経路の変化（RFC 9000 9.3）
        if space == SpaceId::Data && remote != self.path.remote {
            if probing_only || !is_largest {
                // probing だけのパケットは移行しない（PATH_RESPONSE は既に積んだ）
            } else {
                self.on_peer_migrated(now, remote, dlen);
            }
        }
        let _ = first_in_datagram;
        // 鍵が増えたら抱えていたパケットを処理し直す
        if !self.undecryptable.is_empty() {
            let pending: Vec<_> = self.undecryptable.drain(..).collect();
            for (addr, d) in pending {
                let l = d.len();
                self.handle_datagram_inner(now, addr, d, l)?;
            }
        }
        Ok(())
    }

    fn handle_datagram_inner(&mut self, now: Instant, remote: SocketAddr, mut rest: BytesMut, dlen: usize) -> Result<(), TransportError> {
        while !rest.is_empty() {
            let Ok(partial) = packet::parse(&rest, self.cid_len) else { break };
            let len = partial.len;
            let header = to_owned_header(&partial.header, len);
            let pn_offset = partial.pn_offset;
            let pkt = rest.split_to(len);
            self.handle_packet(now, remote, header, pn_offset, pkt, dlen, false)?;
        }
        Ok(())
    }

    fn open_1rtt(&mut self, now: Instant, first: u8, pn: u64, hdr: &[u8], body: &mut [u8], tag: &[u8]) -> Result<(), CryptoFailure> {
        let pto = self.pto_duration(SpaceId::Data);
        let Some(k) = self.one_rtt.as_mut() else { return Err(CryptoFailure) };
        let phase = first & 0x04 != 0;
        if phase == k.phase {
            k.remote.open(pn, hdr, body, tag)?;
            if k.first_recv.is_none_or(|f| pn < f) {
                k.first_recv = Some(pn);
            }
            return Ok(());
        }
        // 前の世代の遅れて届いたパケット
        if let Some((prev, _)) = &k.prev_remote
            && k.first_recv.is_none_or(|f| pn < f)
        {
            return prev.open(pn, hdr, body, tag);
        }
        // 相手が鍵更新を始めた
        if !self.handshake_confirmed {
            return Err(CryptoFailure);
        }
        // 次の世代の鍵で試す（本文を壊さないよう写してから）
        let mut trial = body.to_vec();
        k.next_remote.open(pn, hdr, &mut trial, tag)?;
        body.copy_from_slice(&trial);
        k.rotate(now + pto * 3)?;
        k.first_recv = Some(pn);
        self.stats.key_updates += 1;
        self.events.push_back(Event::KeyUpdated);
        Ok(())
    }

    fn is_stateless_reset(&self, pkt: &[u8]) -> bool {
        if pkt.len() < 21 {
            return false;
        }
        let tail = &pkt[pkt.len() - 16..];
        self.peer_cids
            .values()
            .filter_map(|c| c.token.as_ref())
            .any(|t| bool::from(t.ct_eq(tail)))
    }

    fn handle_version_negotiation(&mut self, now: Instant, dcid: &[u8], scid: &[u8], versions: &[u8]) {
        // client だけ、server のパケットを一つも受けていないとき（RFC 9000 6.2）
        if self.side != Side::Client || self.got_server_packet || self.state != State::Handshake {
            return;
        }
        let ours = self.local_cids[0].cid;
        if dcid != ours.as_slice() || scid != self.orig_dcid.as_slice() {
            return;
        }
        if versions.chunks(4).any(|v| v == VERSION_1.to_be_bytes()) {
            return; // v1 を含むなら偽物（ダウングレード）として無視
        }
        self.error = Some(ConnectionError::VersionMismatch);
        self.events.push_back(Event::ConnectionLost(ConnectionError::VersionMismatch));
        self.state = State::Drained;
        let _ = now;
    }

    fn handle_retry(&mut self, now: Instant, scid: ConnectionId, token: &[u8], body: &[u8], tag: &[u8]) {
        if self.side != Side::Client || self.got_server_packet || self.retry_scid.is_some() || token.is_empty() {
            return;
        }
        let expected = crate::crypto::retry_tag(&self.orig_dcid, body);
        if !bool::from(expected.ct_eq(tag)) {
            return;
        }
        // 新しい DCID で Initial 鍵を作り直し、ClientHello を送り直す
        let Ok(keys) = Keys::initial(&scid, true) else { return };
        self.retry_scid = Some(scid);
        self.retry_token = token.to_vec();
        if let Some(c) = self.peer_cids.get_mut(&0) {
            c.cid = scid;
        }
        let sp = &mut self.spaces[0];
        sp.local = Some(keys.local);
        sp.remote = Some(keys.remote);
        let sent = std::mem::take(&mut sp.sent);
        for p in sent.values() {
            if p.in_flight {
                self.cc.remove(p.size);
            }
        }
        let sp = &mut self.spaces[0];
        sp.ack_eliciting_in_flight = 0;
        sp.loss_time = None;
        sp.crypto_send.requeue_unacked();
        self.pto_count = 0;
        let _ = now;
    }

    fn on_long_header(&mut self, space: SpaceId, scid: &ConnectionId, _dcid: &ConnectionId) -> Result<(), TransportError> {
        match self.side {
            Side::Client => {
                if !self.got_server_packet {
                    // server の最初のパケットの SCID を以後の DCID にする（RFC 9000 7.2）
                    self.got_server_packet = true;
                    if let Some(c) = self.peer_cids.get_mut(&0) {
                        c.cid = *scid;
                    }
                } else if self.peer_cids.get(&0).is_some_and(|c| c.cid != *scid) && space != SpaceId::Data {
                    // 途中で SCID を変えたパケットは捨てる（エラーにはしない）
                }
            }
            Side::Server => {}
        }
        Ok(())
    }

    /// フレームを処理する。`(ack_eliciting, probing だけ)` を返す。
    fn handle_frames(
        &mut self,
        now: Instant,
        space: SpaceId,
        remote: SocketAddr,
        payload: &Bytes,
        dlen: usize,
    ) -> Result<(bool, bool), TransportError> {
        let mut it = frame::Iter::new(payload);
        let mut eliciting = false;
        let mut probing_only = true;
        while let Some(f) = it.next_frame() {
            let f = f?;
            let ft = it.last_type;
            if space != SpaceId::Data && !f.allowed_in_handshake() {
                return Err(terr!(PROTOCOL_VIOLATION, "この段階では使えないフレーム").frame(ft));
            }
            eliciting |= f.ack_eliciting();
            probing_only &= f.probing();
            match f {
                Frame::Padding | Frame::Ping => {}
                Frame::Ack { ranges, delay } => self.on_ack(now, space, &ranges, delay)?,
                Frame::Crypto { offset, data } => {
                    let data = payload.slice_ref(data);
                    self.on_crypto(space, offset, data)?;
                }
                Frame::NewToken { .. } => {
                    if self.side == Side::Server {
                        return Err(terr!(PROTOCOL_VIOLATION, "client からの NEW_TOKEN").frame(ft));
                    }
                    // 0-RTT・再接続でトークンを使わないので保存しない
                }
                Frame::Stream { id, offset, data, fin } => {
                    let data = payload.slice_ref(data);
                    self.streams.on_stream(StreamId(id), offset, data, fin)?;
                }
                Frame::ResetStream { id, code, final_size } => self.streams.on_reset(StreamId(id), code, final_size)?,
                Frame::StopSending { id, code } => self.streams.on_stop_sending(StreamId(id), code)?,
                Frame::MaxData(m) => self.streams.on_max_data(m),
                Frame::MaxStreamData { id, max } => self.streams.on_max_stream_data(StreamId(id), max)?,
                Frame::MaxStreams { bidi, max } => self.streams.on_max_streams(bidi, max),
                Frame::DataBlocked(_) | Frame::StreamsBlocked { .. } => {}
                Frame::StreamDataBlocked { id, .. } => self.streams.on_stream_data_blocked(StreamId(id))?,
                Frame::NewConnectionId {
                    seq,
                    retire_prior_to,
                    cid,
                    token,
                } => self.on_new_cid(seq, retire_prior_to, cid, token)?,
                Frame::RetireConnectionId { seq } => self.on_retire_cid(seq)?,
                Frame::PathChallenge(d) => {
                    if self.path_responses.len() >= MAX_PATH_RESPONSES {
                        self.path_responses.pop_front();
                    }
                    self.path_responses.push_back((remote, d, dlen));
                }
                Frame::PathResponse(d) => self.on_path_response(now, remote, d),
                Frame::ConnectionClose {
                    app,
                    code,
                    frame_type,
                    reason,
                } => {
                    let reason = payload.slice_ref(reason);
                    let e = if app {
                        ConnectionError::ApplicationClosed { code, reason }
                    } else {
                        ConnectionError::ConnectionClosed {
                            code: TransportErrorCode(code),
                            frame_type,
                            reason,
                        }
                    };
                    self.error = Some(e.clone());
                    self.events.push_back(Event::ConnectionLost(e));
                    self.enter_draining(now);
                    return Ok((false, false));
                }
                Frame::HandshakeDone => {
                    if self.side == Side::Server {
                        return Err(terr!(PROTOCOL_VIOLATION, "client からの HANDSHAKE_DONE").frame(ft));
                    }
                    self.confirm_handshake();
                }
                Frame::Datagram { data } => {
                    let max = self.config.max_datagram_frame_size;
                    if max == 0 || (data.len() as u64) > max {
                        return Err(terr!(PROTOCOL_VIOLATION, "DATAGRAM が上限を超える").frame(ft));
                    }
                    self.stats.datagrams_received += 1;
                    if self.dgram_recv.len() >= self.config.datagram_recv_queue {
                        self.dgram_recv.pop_front();
                    }
                    self.dgram_recv.push_back(payload.slice_ref(data));
                    if !self.dgram_signaled {
                        self.dgram_signaled = true;
                        self.events.push_back(Event::DatagramReceived);
                    }
                }
            }
        }
        Ok((eliciting, probing_only))
    }

    fn on_crypto(&mut self, space: SpaceId, offset: u64, data: Bytes) -> Result<(), TransportError> {
        let limit = self.config.crypto_buffer as u64;
        let sp = &mut self.spaces[space as usize];
        let end = offset + data.len() as u64;
        if end > sp.crypto_recv.read_offset() + limit {
            return Err(terr!(CRYPTO_BUFFER_EXCEEDED, "CRYPTO の並べ直し待ちが多すぎる"));
        }
        sp.crypto_recv.insert(offset, data);
        while let Some(chunk) = self.spaces[space as usize].crypto_recv.pop(usize::MAX) {
            if let Err(e) = self.tls.read_handshake(space.level(), &chunk) {
                let alert = e.alert().map_or(80, |a| a.0);
                return Err(TransportError::new(TransportErrorCode::crypto(alert), format!("TLS: {e}")).frame(ty::CRYPTO));
            }
            self.process_tls().map_err(|e| match e {
                ConnectionError::TransportError { code, reason, .. } => TransportError::new(code, reason),
                _ => terr!(INTERNAL_ERROR, "TLS"),
            })?;
        }
        Ok(())
    }

    /// TLS の出力（送るハンドシェイクデータ・鍵・完了）を取り込む。
    fn process_tls(&mut self) -> Result<(), ConnectionError> {
        while let Some(o) = self.tls.poll_output() {
            match o {
                QuicOutput::Data { level, data } => {
                    self.spaces[SpaceId::from_level(level) as usize].crypto_send.write(Bytes::from(data));
                }
                QuicOutput::WriteSecret { level, suite, secret } => match level {
                    Level::OneRtt => self.pending_local_1rtt = Some((suite, secret)),
                    l => {
                        let k = DirKeys::from_secret(suite, &secret).map_err(|_| internal("鍵の導出"))?;
                        self.spaces[SpaceId::from_level(l) as usize].local = Some(k);
                    }
                },
                QuicOutput::ReadSecret { level, suite, secret } => match level {
                    Level::OneRtt => self.pending_remote_1rtt = Some((suite, secret)),
                    l => {
                        let k = DirKeys::from_secret(suite, &secret).map_err(|_| internal("鍵の導出"))?;
                        self.spaces[SpaceId::from_level(l) as usize].remote = Some(k);
                    }
                },
                QuicOutput::HandshakeComplete => {
                    self.handshake_complete = true;
                    self.state = State::Established;
                    self.events.push_back(Event::HandshakeCompleted);
                    if self.side == Side::Server {
                        self.handshake_done_pending = true;
                        self.confirm_handshake();
                    }
                }
            }
            if self.one_rtt.is_none()
                && let (Some((s, _)), Some(_)) = (&self.pending_local_1rtt, &self.pending_remote_1rtt)
            {
                let suite = *s;
                let (_, l) = self.pending_local_1rtt.take().ok_or_else(|| internal("鍵"))?;
                let (_, r) = self.pending_remote_1rtt.take().ok_or_else(|| internal("鍵"))?;
                self.one_rtt = Some(OneRtt::new(suite, l, r).map_err(|_| internal("鍵の導出"))?);
            }
        }
        if self.peer_params.is_none()
            && let Some(raw) = self.tls.peer_transport_parameters()
        {
            let p = TransportParameters::decode(raw, self.side == Side::Client).map_err(|e| ConnectionError::TransportError {
                code: e.code,
                frame_type: 0,
                reason: e.reason,
            })?;
            self.apply_peer_params(p).map_err(|e| ConnectionError::TransportError {
                code: e.code,
                frame_type: 0,
                reason: e.reason,
            })?;
        }
        Ok(())
    }

    fn apply_peer_params(&mut self, p: TransportParameters) -> Result<(), TransportError> {
        let bad = |w: &str| terr!(TRANSPORT_PARAMETER_ERROR, "{w}");
        match self.side {
            Side::Client => {
                if p.original_destination_connection_id != Some(self.orig_dcid) {
                    return Err(bad("original_destination_connection_id の不一致"));
                }
                if p.retry_source_connection_id != self.retry_scid {
                    return Err(bad("retry_source_connection_id の不一致"));
                }
                if p.initial_source_connection_id != self.peer_cids.get(&0).map(|c| c.cid) {
                    return Err(bad("initial_source_connection_id の不一致"));
                }
                if let Some(t) = p.stateless_reset_token
                    && let Some(c) = self.peer_cids.get_mut(&0)
                {
                    c.token = Some(t);
                    self.to_endpoint.push_back(ToEndpoint::ResetToken(t));
                }
            }
            Side::Server => {
                if p.initial_source_connection_id != self.expected_peer_scid {
                    return Err(bad("initial_source_connection_id の不一致"));
                }
            }
        }
        self.streams.apply_peer_params(&p);
        self.peer_max_ack_delay = p.max_ack_delay;
        self.peer_ack_delay_exponent = p.ack_delay_exponent;
        self.peer_max_datagram = p.max_datagram_frame_size;
        let local = self.config.max_idle_timeout;
        let peer = p.max_idle_timeout;
        self.idle_timeout = match (local.is_zero(), peer.is_zero()) {
            (true, true) => None,
            (true, false) => Some(peer),
            (false, true) => Some(local),
            (false, false) => Some(local.min(peer)),
        };
        self.peer_params = Some(p);
        Ok(())
    }

    fn confirm_handshake(&mut self) {
        if self.handshake_confirmed {
            return;
        }
        self.handshake_confirmed = true;
        // Handshake 鍵を捨てる（RFC 9001 4.9.2）
        self.discard_space(SpaceId::Handshake);
        self.discard_space(SpaceId::Initial);
        // connection ID を発行する
        let limit = self
            .peer_params
            .as_ref()
            .map_or(2, |p| p.active_connection_id_limit)
            .min(self.config.active_connection_id_limit.max(2))
            .min(8);
        while (self.local_cids.len() as u64) < limit {
            if !self.issue_cid() {
                break;
            }
        }
    }

    fn issue_cid(&mut self) -> bool {
        let Ok(cid) = ConnectionId::random(self.cid_len) else { return false };
        let token = self.cid_key.reset_token(&cid);
        let seq = self.next_local_seq;
        self.next_local_seq += 1;
        self.local_cids.push(LocalCid { seq, cid, token });
        self.new_cid_pending.push_back(seq);
        self.to_endpoint.push_back(ToEndpoint::NewCid(cid));
        true
    }

    fn discard_space(&mut self, id: SpaceId) {
        let sp = &mut self.spaces[id as usize];
        if sp.discarded {
            return;
        }
        sp.discarded = true;
        sp.local = None;
        sp.remote = None;
        let sent = std::mem::take(&mut sp.sent);
        sp.ack_eliciting_in_flight = 0;
        sp.loss_time = None;
        sp.probes = 0;
        sp.crypto_send = SendBuffer::default();
        sp.crypto_recv = Assembler::default();
        sp.unacked_eliciting = 0;
        sp.ack_immediate = false;
        for p in sent.values() {
            if p.in_flight {
                self.cc.remove(p.size);
            }
        }
        self.pto_count = 0;
    }

    fn on_new_cid(&mut self, seq: u64, retire_prior_to: u64, cid: ConnectionId, token: ResetToken) -> Result<(), TransportError> {
        let ft = ty::NEW_CONNECTION_ID;
        if self.peer_cids.get(&self.active_peer_seq).is_some_and(|c| c.cid.is_empty()) {
            return Err(terr!(PROTOCOL_VIOLATION, "長さ 0 の connection ID の相手からの NEW_CONNECTION_ID").frame(ft));
        }
        if let Some(c) = self.peer_cids.get(&seq) {
            if c.cid != cid || c.token != Some(token) {
                return Err(terr!(PROTOCOL_VIOLATION, "同じ番号で違う connection ID").frame(ft));
            }
            return Ok(());
        }
        if seq < self.peer_retire_prior_to {
            // 既に退役を求められた番号: すぐ退役させる
            self.queue_retire(seq);
            return Ok(());
        }
        self.peer_cids.insert(seq, PeerCid { cid, token: Some(token) });
        self.to_endpoint.push_back(ToEndpoint::ResetToken(token));
        if retire_prior_to > self.peer_retire_prior_to {
            self.peer_retire_prior_to = retire_prior_to;
            let old: Vec<u64> = self.peer_cids.range(..retire_prior_to).map(|(&s, _)| s).collect();
            for s in old {
                self.peer_cids.remove(&s);
                self.queue_retire(s);
            }
            if !self.peer_cids.contains_key(&self.active_peer_seq) {
                match self.peer_cids.keys().next() {
                    Some(&s) => self.active_peer_seq = s,
                    None => return Err(terr!(PROTOCOL_VIOLATION, "使える connection ID が無い").frame(ft)),
                }
            }
        }
        if self.peer_cids.len() as u64 > self.config.active_connection_id_limit.max(2) {
            return Err(terr!(CONNECTION_ID_LIMIT_ERROR, "connection ID が多すぎる").frame(ft));
        }
        Ok(())
    }

    fn queue_retire(&mut self, seq: u64) {
        if self.retire_pending.len() < MAX_PENDING_RETIRE {
            self.retire_pending.push_back(seq);
        }
    }

    fn on_retire_cid(&mut self, seq: u64) -> Result<(), TransportError> {
        let ft = ty::RETIRE_CONNECTION_ID;
        if seq >= self.next_local_seq {
            return Err(terr!(PROTOCOL_VIOLATION, "発行していない connection ID の退役").frame(ft));
        }
        if let Some(i) = self.local_cids.iter().position(|c| c.seq == seq) {
            if self.local_cids.len() == 1 {
                return Err(terr!(PROTOCOL_VIOLATION, "最後の connection ID の退役").frame(ft));
            }
            let c = self.local_cids.remove(i);
            self.to_endpoint.push_back(ToEndpoint::RetireCid(c.cid));
            if self.handshake_confirmed {
                self.issue_cid();
            }
        }
        Ok(())
    }

    /// 未使用の相手の connection ID に切り替え、古いものを退役させる。
    fn switch_peer_cid(&mut self) -> bool {
        let next = self.peer_cids.range(self.active_peer_seq + 1..).next().map(|(&s, _)| s);
        let Some(next) = next else { return false };
        let old = self.active_peer_seq;
        self.active_peer_seq = next;
        self.peer_cids.remove(&old);
        self.queue_retire(old);
        true
    }

    fn start_path_challenge(&mut self, now: Instant) {
        let mut d = [0u8; 8];
        if getrandom::fill(&mut d).is_err() {
            return;
        }
        self.path.challenge = Some(d);
        self.path.challenge_pending = true;
        let pto = self.pto_duration(SpaceId::Data).max(self.rtt_initial_pto());
        self.path.challenge_deadline = Some(now + pto * 3);
    }

    fn rtt_initial_pto(&self) -> Duration {
        self.config.initial_rtt * 2
    }

    fn on_path_response(&mut self, _now: Instant, _remote: SocketAddr, d: [u8; 8]) {
        if self.path.challenge == Some(d) {
            self.path.validated = true;
            self.path.challenge = None;
            self.path.challenge_pending = false;
            self.path.challenge_deadline = None;
            self.prev_path = None;
        }
    }

    fn on_peer_migrated(&mut self, now: Instant, remote: SocketAddr, dlen: usize) {
        // ハンドシェイク確定前の移行・移行を禁止したときは受け入れない（RFC 9000 9）
        if !self.handshake_confirmed || self.config.disable_active_migration || self.side == Side::Client {
            return;
        }
        let only_port = remote.ip() == self.path.remote.ip();
        let mut new = Path::new(remote, false);
        new.recv = dlen as u64;
        let old = std::mem::replace(&mut self.path, new);
        if old.validated {
            self.prev_path = Some(old);
        }
        if !only_port {
            self.cc.reset();
            self.rtt.reset();
        }
        // 新しい経路では新しい connection ID を使う（RFC 9000 9.5）
        self.switch_peer_cid();
        self.start_path_challenge(now);
        self.stats.migrations += 1;
        self.events.push_back(Event::PathMigrated { remote });
    }

    // ------------------------------------------------------------------ ACK・損失

    fn on_ack(&mut self, now: Instant, space: SpaceId, ranges: &[(u64, u64)], delay: u64) -> Result<(), TransportError> {
        let largest = ranges[0].1;
        if largest >= self.spaces[space as usize].next_pn {
            return Err(terr!(PROTOCOL_VIOLATION, "送っていないパケットへの ACK").frame(ty::ACK));
        }
        let mut acked: Vec<(u64, SentPacket)> = Vec::new();
        {
            let sp = &mut self.spaces[space as usize];
            for &(lo, hi) in ranges {
                let keys: Vec<u64> = sp.sent.range(lo..=hi).map(|(&k, _)| k).collect();
                for k in keys {
                    if let Some(p) = sp.sent.remove(&k) {
                        acked.push((k, p));
                    }
                }
            }
            if acked.is_empty() {
                return Ok(());
            }
            if sp.largest_acked.is_none_or(|l| largest > l) {
                sp.largest_acked = Some(largest);
            }
        }
        // RTT の標本（最大の番号が新たに ACK され、ACK を引き起こすパケットを含むとき）
        if let Some((_, p)) = acked.iter().find(|(pn, _)| *pn == largest)
            && acked.iter().any(|(_, p)| p.ack_eliciting)
        {
            let latest = now.saturating_duration_since(p.time);
            let ack_delay = if space == SpaceId::Data {
                Duration::from_micros(delay.checked_shl(self.peer_ack_delay_exponent as u32).unwrap_or(u64::MAX).min(1 << 40))
            } else {
                Duration::ZERO
            };
            self.rtt.update(latest, ack_delay, self.handshake_confirmed, self.peer_max_ack_delay);
        }
        for (pn, p) in &acked {
            if p.ack_eliciting {
                let sp = &mut self.spaces[space as usize];
                sp.ack_eliciting_in_flight = sp.ack_eliciting_in_flight.saturating_sub(1);
            }
            if p.in_flight {
                self.cc.on_ack(p.time, p.size, false);
            }
            if space == SpaceId::Data
                && let Some(k) = self.one_rtt.as_mut()
                && k.first_sent.is_some_and(|f| *pn >= f)
            {
                k.update_acked = true;
            }
            for f in &p.frames {
                self.on_frame_acked(space, f);
            }
        }
        // client: 1-RTT パケットの ACK でハンドシェイク確定（RFC 9001 4.1.2）
        if space == SpaceId::Data && self.side == Side::Client && self.handshake_complete {
            self.confirm_handshake();
        }
        self.detect_lost(now, space);
        self.pto_count = 0;
        Ok(())
    }

    fn on_frame_acked(&mut self, space: SpaceId, f: &SentFrame) {
        match f {
            SentFrame::Crypto { offset, len } => self.spaces[space as usize].crypto_send.ack(*offset, *len),
            SentFrame::HandshakeDone => self.handshake_done_pending = false,
            SentFrame::Stream { .. } | SentFrame::ResetStream { .. } => self.streams.on_acked(f),
            _ => {}
        }
    }

    fn on_frame_lost(&mut self, space: SpaceId, f: &SentFrame) {
        match f {
            SentFrame::Crypto { offset, len } => self.spaces[space as usize].crypto_send.lost(*offset, *len),
            SentFrame::HandshakeDone => self.handshake_done_pending = true,
            SentFrame::NewConnectionId { seq } => {
                if self.local_cids.iter().any(|c| c.seq == *seq) {
                    self.new_cid_pending.push_back(*seq);
                }
            }
            SentFrame::RetireConnectionId { seq } => self.queue_retire(*seq),
            SentFrame::PathChallenge => {
                if self.path.challenge.is_some() {
                    self.path.challenge_pending = true;
                }
            }
            SentFrame::Ack { .. } | SentFrame::Ping | SentFrame::Datagram | SentFrame::PathResponse => {}
            _ => self.streams.on_lost(f),
        }
    }

    fn detect_lost(&mut self, now: Instant, space: SpaceId) {
        let loss_delay = self.rtt.loss_delay();
        let sp = &mut self.spaces[space as usize];
        sp.loss_time = None;
        let Some(largest) = sp.largest_acked else { return };
        let lost_before = now.checked_sub(loss_delay);
        let mut lost = Vec::new();
        for (&pn, p) in sp.sent.range(..=largest) {
            if lost_before.is_some_and(|t| p.time <= t) || largest >= pn + PACKET_THRESHOLD {
                lost.push(pn);
            } else {
                let t = p.time + loss_delay;
                sp.loss_time = Some(sp.loss_time.map_or(t, |x| x.min(t)));
            }
        }
        if lost.is_empty() {
            return;
        }
        let mut lost_pkts = Vec::with_capacity(lost.len());
        for pn in lost {
            if let Some(p) = sp.sent.remove(&pn) {
                if p.ack_eliciting {
                    sp.ack_eliciting_in_flight = sp.ack_eliciting_in_flight.saturating_sub(1);
                }
                lost_pkts.push(p);
            }
        }
        self.stats.lost_packets += lost_pkts.len() as u64;
        let mut newest: Option<Instant> = None;
        let mut first_eliciting: Option<Instant> = None;
        let mut last_eliciting: Option<Instant> = None;
        for p in &lost_pkts {
            if p.in_flight {
                self.cc.remove(p.size);
                newest = Some(newest.map_or(p.time, |n| n.max(p.time)));
            }
            if p.ack_eliciting {
                first_eliciting = Some(first_eliciting.map_or(p.time, |n| n.min(p.time)));
                last_eliciting = Some(last_eliciting.map_or(p.time, |n| n.max(p.time)));
            }
            for f in &p.frames {
                self.on_frame_lost(space, f);
            }
        }
        if let Some(t) = newest {
            self.cc.on_congestion(t, now);
        }
        // persistent congestion（RFC 9002 7.6、簡略: 失われた ACK 誘発パケットの時間幅で判定）
        if let (Some(a), Some(b)) = (first_eliciting, last_eliciting)
            && self.rtt.has_sample()
            && b.saturating_duration_since(a) > self.rtt.persistent_congestion_duration(self.peer_max_ack_delay)
        {
            self.cc.on_persistent_congestion();
        }
    }

    fn pto_duration(&self, space: SpaceId) -> Duration {
        let mut d = self.rtt.pto_base();
        if space == SpaceId::Data && self.handshake_confirmed {
            d += self.peer_max_ack_delay;
        }
        d
    }

    /// 損失検出の timer（RFC 9002 A.8）。`(時刻, 空間, PTO か)`。
    fn loss_timer(&self) -> Option<(Instant, SpaceId, bool)> {
        // 時間による損失判定が先
        let mut best: Option<(Instant, SpaceId)> = None;
        for s in SPACES {
            if let Some(t) = self.spaces[s as usize].loss_time
                && best.is_none_or(|(b, _)| t < b)
            {
                best = Some((t, s));
            }
        }
        if let Some((t, s)) = best {
            return Some((t, s, false));
        }
        let backoff = 1u32 << self.pto_count.min(16);
        let in_flight = SPACES.iter().any(|&s| self.spaces[s as usize].ack_eliciting_in_flight > 0);
        if !in_flight {
            // client はハンドシェイク確定まで、何も飛んでいなくても PTO を張る（anti-deadlock）
            if self.side == Side::Client && !self.handshake_confirmed && !self.is_closed() {
                let s = if self.spaces[1].local.is_some() { SpaceId::Handshake } else { SpaceId::Initial };
                let base = self.spaces[s as usize].last_ack_eliciting_sent?;
                return Some((base + self.pto_duration(s) * backoff, s, true));
            }
            return None;
        }
        let mut best: Option<(Instant, SpaceId)> = None;
        for s in SPACES {
            let sp = &self.spaces[s as usize];
            if sp.ack_eliciting_in_flight == 0 {
                continue;
            }
            if s == SpaceId::Data && !self.handshake_complete {
                continue;
            }
            let Some(base) = sp.last_ack_eliciting_sent else { continue };
            let t = base + self.pto_duration(s) * backoff;
            if best.is_none_or(|(b, _)| t < b) {
                best = Some((t, s));
            }
        }
        best.map(|(t, s)| (t, s, true))
    }

    // ------------------------------------------------------------------ timer

    /// 次に [`Self::handle_timeout`] を呼ぶ時刻。
    pub fn poll_timeout(&self) -> Option<Instant> {
        let mut t: Option<Instant> = None;
        let mut add = |x: Option<Instant>| {
            if let Some(x) = x {
                t = Some(t.map_or(x, |y| y.min(x)));
            }
        };
        match self.state {
            State::Drained => return None,
            State::Closing { until } | State::Draining { until } => return Some(until),
            _ => {}
        }
        add(self.idle_deadline);
        add(self.loss_timer().map(|(t, _, _)| t));
        for s in &self.spaces {
            if s.unacked_eliciting > 0 {
                add(s.ack_deadline);
            }
        }
        add(self.path.challenge_deadline);
        add(self.one_rtt.as_ref().and_then(|k| k.prev_remote.as_ref().map(|(_, t)| *t)));
        add(self.pacing_until);
        t
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        match self.state {
            State::Drained => return,
            State::Closing { until } | State::Draining { until } => {
                if now >= until {
                    self.state = State::Drained;
                }
                return;
            }
            _ => {}
        }
        if self.idle_deadline.is_some_and(|t| now >= t) {
            self.error = Some(ConnectionError::TimedOut);
            self.events.push_back(Event::ConnectionLost(ConnectionError::TimedOut));
            self.state = State::Drained;
            return;
        }
        if self.pacing_until.is_some_and(|t| now >= t) {
            self.pacing_until = None;
        }
        for s in &mut self.spaces {
            if s.ack_deadline.is_some_and(|t| now >= t) {
                s.ack_deadline = None;
                s.ack_immediate = true;
            }
        }
        if let Some(k) = self.one_rtt.as_mut()
            && k.prev_remote.as_ref().is_some_and(|(_, t)| now >= *t)
        {
            k.prev_remote = None;
        }
        if self.path.challenge_deadline.is_some_and(|t| now >= t) {
            self.path.challenge_deadline = None;
            self.path.challenge = None;
            self.path.challenge_pending = false;
            if !self.path.validated {
                match self.prev_path.take() {
                    Some(p) => self.path = p,
                    None => {
                        self.transport_error(now, terr!(NO_VIABLE_PATH, "経路の検証に失敗"));
                        return;
                    }
                }
            }
        }
        if let Some((t, space, pto)) = self.loss_timer()
            && now >= t
        {
            if pto {
                self.on_pto(space);
            } else {
                self.detect_lost(now, space);
            }
        }
    }

    fn on_pto(&mut self, space: SpaceId) {
        self.pto_count += 1;
        self.stats.pto_count += 1;
        let sp = &mut self.spaces[space as usize];
        sp.probes = 2;
        if space != SpaceId::Data {
            sp.crypto_send.requeue_unacked();
            // client の anti-deadlock: Handshake 鍵があれば Initial は送らない
        } else {
            sp.crypto_send.requeue_unacked();
            self.streams.requeue_for_probe();
        }
    }

    // ------------------------------------------------------------------ 終了

    fn transport_error(&mut self, now: Instant, e: TransportError) {
        if self.is_closed() {
            return;
        }
        let ce = ConnectionError::TransportError {
            code: e.code,
            frame_type: e.frame_type,
            reason: e.reason.clone(),
        };
        self.error = Some(ce.clone());
        self.events.push_back(Event::ConnectionLost(ce));
        let mut reason = e.reason.into_bytes();
        reason.truncate(128);
        self.close = Some(CloseFrame {
            app: false,
            code: e.code.0,
            frame_type: e.frame_type,
            reason,
        });
        self.enter_closing(now);
    }

    fn enter_closing(&mut self, now: Instant) {
        let pto = self.pto_duration(SpaceId::Data);
        self.state = State::Closing { until: now + pto * 3 };
        self.close_pending = true;
        self.close_sends = 0;
    }

    fn enter_draining(&mut self, now: Instant) {
        let pto = self.pto_duration(SpaceId::Data);
        self.state = State::Draining { until: now + pto * 3 };
        self.close_pending = false;
    }

    // ------------------------------------------------------------------ 送信

    fn mtu(&self) -> usize {
        let peer = self.peer_params.as_ref().map_or(65527, |p| p.max_udp_payload_size) as usize;
        self.config.max_udp_payload_size.min(peer).max(1200)
    }

    /// 送る datagram を一つ作る。何も無ければ `None`。
    pub fn poll_transmit(&mut self, now: Instant) -> Option<Transmit> {
        self.pull_stream_events();
        match self.state {
            State::Drained | State::Draining { .. } => return None,
            State::Closing { .. } => {
                if !self.close_pending {
                    return None;
                }
                self.close_pending = false;
                self.close_sends += 1;
                return self.build_close(now);
            }
            _ => {}
        }
        // 別のアドレスからの PATH_CHALLENGE への応答
        if let Some(i) = self.path_responses.iter().position(|(a, _, _)| *a != self.path.remote)
            && let Some((addr, d, rlen)) = self.path_responses.remove(i)
            && let Some(t) = self.build_off_path_response(now, addr, d, rlen)
        {
            return Some(t);
        }
        self.build_datagram(now)
    }

    fn peer_dcid(&self) -> ConnectionId {
        self.peer_cids
            .get(&self.active_peer_seq)
            .or_else(|| self.peer_cids.values().next())
            .map_or_else(|| ConnectionId::new(&[]).unwrap_or(self.orig_dcid), |c| c.cid)
    }

    /// 空間が鍵を持ち送信できるか。
    fn can_send_in(&self, s: SpaceId) -> bool {
        match s {
            SpaceId::Data => self.one_rtt.is_some(),
            _ => !self.spaces[s as usize].discarded && self.spaces[s as usize].local.is_some(),
        }
    }

    /// ACK 以外に送るものがあるか（`congestion_ok` が偽なら probe だけ）。
    fn has_eliciting(&self, s: SpaceId) -> bool {
        let sp = &self.spaces[s as usize];
        if sp.probes > 0 || sp.crypto_send.has_retransmit() || sp.crypto_send.has_unsent() {
            return true;
        }
        if s != SpaceId::Data {
            return false;
        }
        self.handshake_done_pending && self.side == Side::Server
            || !self.new_cid_pending.is_empty()
            || !self.retire_pending.is_empty()
            || self.path.challenge_pending
            || self.path_responses.iter().any(|(a, _, _)| *a == self.path.remote)
            || !self.dgram_send.is_empty()
            || self.streams.has_pending()
    }

    /// CONNECTION_CLOSE だけの datagram（closing 中）。
    fn build_close(&mut self, now: Instant) -> Option<Transmit> {
        let c = self.close.clone()?;
        let limit = (self.mtu() as u64).min(self.path.budget()) as usize;
        if limit < 64 {
            return None;
        }
        let mut buf = Vec::with_capacity(limit);
        let mut pending: Option<Building> = None;
        for s in SPACES {
            if !self.can_send_in(s) {
                continue;
            }
            // 確定後は 1-RTT だけ。確定前は鍵のある全段階で送る（RFC 9000 10.2.3）
            if self.handshake_confirmed && s != SpaceId::Data {
                continue;
            }
            if let Some(p) = pending.take() {
                self.seal(now, &mut buf, p, 0)?;
            }
            if limit.saturating_sub(buf.len()) < 64 {
                break;
            }
            let mut b = self.begin_packet(&mut buf, s)?;
            let room = (limit - TAG_LEN).saturating_sub(buf.len());
            let mut reason: &[u8] = &c.reason;
            if reason.len() + 24 > room {
                reason = &[];
            }
            if c.app && s != SpaceId::Data {
                // Initial・Handshake ではアプリケーションの理由を出さない（RFC 9000 10.2.3）
                frame::write_close(&mut buf, false, TransportErrorCode::APPLICATION_ERROR.0, 0, &[]);
            } else {
                frame::write_close(&mut buf, c.app, c.code, c.frame_type, reason);
            }
            b.frames.clear();
            pending = Some(b);
            if s == SpaceId::Data {
                break;
            }
        }
        let p = pending?;
        let pad = if self.side == Side::Client && p.space == SpaceId::Initial { 1200.min(limit) } else { 0 };
        self.seal(now, &mut buf, p, pad)?;
        self.path.sent += buf.len() as u64;
        Some(Transmit {
            destination: self.path.remote,
            contents: buf,
        })
    }

    /// 今の経路以外のアドレスからの PATH_CHALLENGE への応答（RFC 9000 8.2.2）。
    fn build_off_path_response(&mut self, now: Instant, addr: SocketAddr, d: [u8; 8], rlen: usize) -> Option<Transmit> {
        self.one_rtt.as_ref()?;
        let mut buf = Vec::with_capacity(1200);
        let mut b = self.begin_packet(&mut buf, SpaceId::Data)?;
        buf.push(ty::PATH_RESPONSE as u8);
        buf.extend_from_slice(&d);
        b.frames.clear();
        // 1200 バイトまで埋める。ただし未検証のアドレスへは受けた量の 3 倍まで
        let pad = 1200.min(rlen * 3);
        self.seal(now, &mut buf, b, pad)?;
        Some(Transmit {
            destination: addr,
            contents: buf,
        })
    }

    fn build_datagram(&mut self, now: Instant) -> Option<Transmit> {
        let mtu = self.mtu();
        let budget = self.path.budget();
        let limit = (mtu as u64).min(budget) as usize;
        if limit < 64 {
            return None; // anti-amplification で止まっている
        }
        let probes = self.spaces.iter().any(|s| s.probes > 0);
        let cwnd_ok = self.cc.available() >= mtu as u64 || probes;
        let mut pacing_ok = true;
        if cwnd_ok && !probes {
            let any_eliciting = SPACES.iter().any(|&s| self.can_send_in(s) && self.has_eliciting(s));
            if any_eliciting
                && let Some(t) = self.pacer.delay(now, self.rtt.smoothed(), self.cc.window(), mtu)
            {
                self.pacing_until = Some(t);
                pacing_ok = false;
            }
        }
        let allow_eliciting = cwnd_ok && pacing_ok;
        let mut buf: Vec<u8> = Vec::with_capacity(limit);
        let mut pending: Option<Building> = None;
        let mut sealed: Vec<(SpaceId, u64, SentPacket)> = Vec::new();
        let mut has_initial = false;
        let mut eliciting_initial = false;
        let mut has_challenge = false;
        for s in SPACES {
            if !self.can_send_in(s) {
                continue;
            }
            let wants_ack = self.spaces[s as usize].has_ack_to_send() && (s != SpaceId::Data || self.ack_due(s));
            let wants_more = allow_eliciting && self.has_eliciting(s);
            let probe = self.spaces[s as usize].probes > 0;
            if !wants_ack && !wants_more && !probe {
                continue;
            }
            if let Some(p) = pending.take() {
                let rec = self.seal(now, &mut buf, p, 0)?;
                sealed.push(rec);
            }
            let room = limit.saturating_sub(buf.len());
            if room < 40 {
                break;
            }
            let Some(mut b) = self.begin_packet(&mut buf, s) else { continue };
            let max_end = limit - TAG_LEN;
            self.write_packet_frames(now, s, &mut buf, max_end, &mut b, allow_eliciting || probe);
            if b.frames.is_empty() && buf.len() == b.payload_start {
                // 何も書けなかった
                buf.truncate(b.start);
                self.spaces[s as usize].next_pn -= 1;
                continue;
            }
            if s == SpaceId::Initial {
                has_initial = true;
                eliciting_initial |= b.ack_eliciting;
            }
            has_challenge |= b.frames.iter().any(|f| matches!(f, SentFrame::PathChallenge | SentFrame::PathResponse));
            pending = Some(b);
            if s == SpaceId::Data {
                break;
            }
        }
        let p = pending?;
        // client の Initial を含む datagram・server の ACK 誘発 Initial・経路検証は 1200 バイトまで埋める
        let pad = if (has_initial && (self.side == Side::Client || eliciting_initial)) || has_challenge {
            1200.min(limit)
        } else {
            0
        };
        let rec = self.seal(now, &mut buf, p, pad)?;
        sealed.push(rec);
        let mut sent_handshake = false;
        for (s, pn, pkt) in sealed {
            if pkt.ack_eliciting {
                let sp = &mut self.spaces[s as usize];
                sp.ack_eliciting_in_flight += 1;
                sp.last_ack_eliciting_sent = Some(now);
                if sp.probes > 0 {
                    sp.probes -= 1;
                }
                if !self.eliciting_since_recv
                    && let Some(t) = self.idle_timeout
                {
                    self.idle_deadline = Some(now + t.max(self.pto_duration(SpaceId::Data) * 3));
                    self.eliciting_since_recv = true;
                }
            }
            if pkt.in_flight {
                self.cc.on_sent(pkt.size);
            }
            if s == SpaceId::Handshake {
                sent_handshake = true;
            }
            self.stats.sent_packets += 1;
            self.spaces[s as usize].sent.insert(pn, pkt);
        }
        // Handshake を送った client は Initial 鍵を捨てる（RFC 9001 4.9.1）
        if sent_handshake && self.side == Side::Client {
            self.discard_space(SpaceId::Initial);
        }
        // 鍵の使用量が上限に近づいたら更新する（RFC 9001 6.6）
        if self
            .one_rtt
            .as_ref()
            .is_some_and(|k| k.sent_in_phase >= self.config.key_update_interval && k.update_acked)
        {
            self.initiate_key_update(now);
        }
        self.pacer.on_sent(buf.len());
        self.path.sent += buf.len() as u64;
        self.stats.sent_bytes += buf.len() as u64;
        Some(Transmit {
            destination: self.path.remote,
            contents: buf,
        })
    }

    fn ack_due(&self, s: SpaceId) -> bool {
        let sp = &self.spaces[s as usize];
        sp.ack_immediate
    }

    fn begin_packet(&mut self, buf: &mut Vec<u8>, s: SpaceId) -> Option<Building> {
        let start = buf.len();
        let sp = &mut self.spaces[s as usize];
        let pn = sp.next_pn;
        sp.next_pn += 1;
        let pn_len = packet::pn_len(pn, sp.largest_acked);
        let dcid = self.peer_dcid();
        let mut len_pos = None;
        match s {
            SpaceId::Data => {
                let k = self.one_rtt.as_ref()?;
                buf.push(0x40 | if k.phase { 0x04 } else { 0 } | (pn_len as u8 - 1));
                buf.extend_from_slice(dcid.as_slice());
            }
            _ => {
                let t = if s == SpaceId::Initial { LongType::Initial } else { LongType::Handshake };
                buf.push(0xc0 | (t.bits() << 4) | (pn_len as u8 - 1));
                buf.extend_from_slice(&VERSION_1.to_be_bytes());
                buf.push(dcid.len() as u8);
                buf.extend_from_slice(dcid.as_slice());
                let scid = self.local_cids[0].cid;
                buf.push(scid.len() as u8);
                buf.extend_from_slice(scid.as_slice());
                if s == SpaceId::Initial {
                    let tok = if self.side == Side::Client { self.retry_token.as_slice() } else { &[] };
                    varint::write(buf, tok.len() as u64);
                    buf.extend_from_slice(tok);
                }
                len_pos = Some(buf.len());
                buf.extend_from_slice(&[0x40, 0x00]); // 2 バイトの長さ（後で埋める）
            }
        }
        let pn_offset = buf.len();
        buf.extend_from_slice(&pn.to_be_bytes()[8 - pn_len..]);
        Some(Building {
            space: s,
            start,
            len_pos,
            pn_offset,
            pn_len,
            pn,
            payload_start: buf.len(),
            ack_eliciting: false,
            frames: Vec::new(),
        })
    }

    fn write_ack(&mut self, now: Instant, s: SpaceId, buf: &mut Vec<u8>, max_end: usize, b: &mut Building) {
        let sp = &mut self.spaces[s as usize];
        let Some((largest, t)) = sp.largest_recv else { return };
        if sp.recv.is_empty() {
            return;
        }
        let ranges: Vec<(u64, u64)> = sp.recv.iter().rev().map(|r| (r.start, r.end - 1)).collect();
        let delay = if s == SpaceId::Data {
            (now.saturating_duration_since(t).as_micros() as u64) >> 3
        } else {
            0
        };
        let mut tmp = Vec::new();
        let max_ranges = frame::MAX_ACK_RANGES.min(32);
        frame::write_ack(&mut tmp, &ranges, delay, max_ranges);
        if buf.len() + tmp.len() > max_end {
            return;
        }
        buf.extend_from_slice(&tmp);
        sp.unacked_eliciting = 0;
        sp.ack_immediate = false;
        sp.ack_deadline = None;
        b.frames.push(SentFrame::Ack { largest });
    }

    fn write_packet_frames(&mut self, now: Instant, s: SpaceId, buf: &mut Vec<u8>, max_end: usize, b: &mut Building, eliciting: bool) {
        // ACK はいつでも（送るパケットに相乗りさせる）
        if self.spaces[s as usize].largest_recv.is_some()
            && (self.spaces[s as usize].unacked_eliciting > 0 || self.spaces[s as usize].ack_immediate)
        {
            self.write_ack(now, s, buf, max_end, b);
        }
        if !eliciting {
            return;
        }
        let room = |buf: &Vec<u8>| max_end.saturating_sub(buf.len());
        if s == SpaceId::Data {
            // 経路検証
            while let Some(i) = self.path_responses.iter().position(|(a, _, _)| *a == self.path.remote) {
                if room(buf) < 9 {
                    break;
                }
                if let Some((_, d, _)) = self.path_responses.remove(i) {
                    buf.push(ty::PATH_RESPONSE as u8);
                    buf.extend_from_slice(&d);
                    b.push(SentFrame::PathResponse);
                }
            }
            if self.path.challenge_pending
                && room(buf) >= 9
                && let Some(d) = self.path.challenge
            {
                buf.push(ty::PATH_CHALLENGE as u8);
                buf.extend_from_slice(&d);
                self.path.challenge_pending = false;
                b.push(SentFrame::PathChallenge);
            }
            if self.handshake_done_pending && self.side == Side::Server && room(buf) >= 1 {
                buf.push(ty::HANDSHAKE_DONE as u8);
                self.handshake_done_pending = false;
                b.push(SentFrame::HandshakeDone);
            }
            while let Some(&seq) = self.retire_pending.front() {
                if room(buf) < 9 {
                    break;
                }
                self.retire_pending.pop_front();
                frame::write2(buf, ty::RETIRE_CONNECTION_ID, seq);
                b.push(SentFrame::RetireConnectionId { seq });
            }
            while let Some(&seq) = self.new_cid_pending.front() {
                if room(buf) < 48 {
                    break;
                }
                self.new_cid_pending.pop_front();
                if let Some(c) = self.local_cids.iter().find(|c| c.seq == seq) {
                    frame::write_new_cid(buf, c.seq, 0, &c.cid, &c.token);
                    b.push(SentFrame::NewConnectionId { seq });
                }
            }
        }
        // CRYPTO
        loop {
            let r = room(buf);
            if r < 16 {
                break;
            }
            let sp = &mut self.spaces[s as usize];
            let Some((off, len, _)) = sp.crypto_send.next_range(r - 12, u64::MAX) else { break };
            frame::write_crypto_header(buf, off, len);
            sp.crypto_send.copy_into(off, len, buf);
            b.push(SentFrame::Crypto { offset: off, len });
        }
        if s == SpaceId::Data {
            // datagram（遅延に敏感なので stream より先）
            while let Some(d) = self.dgram_send.front() {
                let need = 1 + varint::encoded_len(d.len() as u64) + d.len();
                if need > room(buf) {
                    if need > self.mtu() {
                        self.dgram_send.pop_front(); // どうしても入らない
                        continue;
                    }
                    break;
                }
                let Some(d) = self.dgram_send.pop_front() else { break };
                varint::write(buf, ty::DATAGRAM_LEN);
                varint::write(buf, d.len() as u64);
                buf.extend_from_slice(&d);
                self.stats.datagrams_sent += 1;
                b.push(SentFrame::Datagram);
            }
            let r = room(buf);
            if r > 0 {
                let mut sent = Vec::new();
                self.streams.write_frames(buf, r, &mut sent);
                for f in sent {
                    b.push(f);
                }
            }
        }
        // probe で何も無ければ PING
        if self.spaces[s as usize].probes > 0 && !b.ack_eliciting && room(buf) >= 1 {
            buf.push(ty::PING as u8);
            b.push(SentFrame::Ping);
        }
    }

    /// パケットを閉じる: padding・長さ・暗号化・header protection。
    fn seal(&mut self, now: Instant, buf: &mut Vec<u8>, b: Building, pad_datagram_to: usize) -> Option<(SpaceId, u64, SentPacket)> {
        let mut padded = false;
        // sample のための最小長（pn_len + payload >= 4）
        let min_payload = 4usize.saturating_sub(b.pn_len);
        while buf.len() - b.payload_start < min_payload {
            buf.push(0);
            padded = true;
        }
        if buf.len() + TAG_LEN < pad_datagram_to {
            buf.resize(pad_datagram_to - TAG_LEN, 0);
            padded = true;
        }
        if let Some(lp) = b.len_pos {
            let l = buf.len() - b.pn_offset + TAG_LEN;
            if l >= 1 << 14 {
                return None;
            }
            let v = (l as u16) | 0x4000;
            buf[lp..lp + 2].copy_from_slice(&v.to_be_bytes());
        }
        let tag = {
            let (hdr, payload) = buf.split_at_mut(b.payload_start);
            let hdr = &hdr[b.start..];
            match b.space {
                SpaceId::Data => {
                    let k = self.one_rtt.as_mut()?;
                    if k.first_sent.is_none() {
                        k.first_sent = Some(b.pn);
                    }
                    k.sent_in_phase += 1;
                    k.local.seal(b.pn, hdr, payload).ok()?
                }
                s => self.spaces[s as usize].local.as_ref()?.packet.seal(b.pn, hdr, payload).ok()?,
            }
        };
        buf.extend_from_slice(&tag);
        {
            let hp = match b.space {
                SpaceId::Data => &self.one_rtt.as_ref()?.hp_local,
                s => &self.spaces[s as usize].local.as_ref()?.header,
            };
            packet::protect(&mut buf[b.start..], b.pn_offset - b.start, b.pn_len, hp);
        }
        let size = buf.len() - b.start;
        Some((
            b.space,
            b.pn,
            SentPacket {
                time: now,
                size,
                ack_eliciting: b.ack_eliciting,
                in_flight: b.ack_eliciting || padded,
                frames: b.frames,
            },
        ))
    }
}

/// 組み立て中のパケット。
struct Building {
    space: SpaceId,
    start: usize,
    len_pos: Option<usize>,
    pn_offset: usize,
    pn_len: usize,
    pn: u64,
    payload_start: usize,
    ack_eliciting: bool,
    frames: Vec<SentFrame>,
}

impl Building {
    fn push(&mut self, f: SentFrame) {
        if !matches!(f, SentFrame::Ack { .. }) {
            self.ack_eliciting = true;
        }
        self.frames.push(f);
    }
}

/// datagram の送信の誤り。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatagramError {
    /// 相手が DATAGRAM 拡張を広告していない。
    Unsupported,
    /// 一つのパケットに入らない。
    TooLarge,
    ConnectionClosed,
}

impl std::fmt::Display for DatagramError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for DatagramError {}

fn internal(why: &str) -> ConnectionError {
    ConnectionError::TransportError {
        code: TransportErrorCode::INTERNAL_ERROR,
        frame_type: 0,
        reason: why.to_string(),
    }
}

/// パケットのヘッダー（借用を切るため所有する形にしたもの）。
enum OwnedHeader {
    Long {
        ty: LongType,
        dcid: ConnectionId,
        scid: ConnectionId,
    },
    Retry {
        scid: ConnectionId,
        token: Vec<u8>,
        body_len: usize,
    },
    Short,
    VersionNegotiation {
        dcid: Vec<u8>,
        scid: Vec<u8>,
        versions: Vec<u8>,
    },
    Unsupported,
}

fn to_owned_header(h: &Header<'_>, len: usize) -> OwnedHeader {
    match h {
        Header::Long { ty, dcid, scid, .. } => OwnedHeader::Long {
            ty: *ty,
            dcid: *dcid,
            scid: *scid,
        },
        Header::Retry { scid, token, .. } => OwnedHeader::Retry {
            scid: *scid,
            token: token.to_vec(),
            body_len: len - TAG_LEN,
        },
        Header::Short { .. } => OwnedHeader::Short,
        Header::VersionNegotiation { dcid, scid, versions } => OwnedHeader::VersionNegotiation {
            dcid: dcid.to_vec(),
            scid: scid.to_vec(),
            versions: versions.to_vec(),
        },
        Header::Unsupported { .. } => OwnedHeader::Unsupported,
    }
}
