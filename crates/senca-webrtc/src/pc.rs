//! サーバー側の PeerConnection（sans-io）。
//!
//! - 交渉: ブラウザーの offer に answer（[`PeerConnection::accept_offer`]、WHIP・WHEP の client offer）、
//!   自分から offer（[`PeerConnection::create_offer`] と [`PeerConnection::apply_answer`]、WHEP の server offer）。
//!   BUNDLE と rtcp-mux のない offer は拒否する。再交渉（m 行の追加・ICE restart）にも応える。
//! - 輸送: ICE（既定は lite）→ 振り分け（RFC 7983）→ DTLS → SRTP/SRTCP、DTLS の上に SCTP（DataChannel）。
//! - 受信: SSRC・mid・rid で受信ストリームを決め（simulcast は層ごとに別のトラック）、RTX を戻し、
//!   ジッターバッファ・NACK・PLI を経て AU を出す。
//! - 送信: 送信トラックごとに SSRC・番号・timestamp を持ち、pacer を通して送る。NACK に RTX で応え、
//!   transport-wide-cc の feedback で帯域を推定する。

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use senca_core::media::{AccessUnit, Codec, CodecConfig, MediaKind, TrackId};
use senca_dtls::{DtlsCertificate, DtlsConfig, DtlsEndpoint, DtlsError, DtlsEvent};
use senca_ice::{IceAgent, IceConfig, IceEvent, IceRole, IceState, Transmit};
use senca_rtp::keyframe::KeyframeRequestLimiter;
use senca_rtp::rtcp::{Nack, ReceiverReport, RtcpPacket, Sdes, SdesChunk};
use senca_rtp::{RtpPacket, build_compound, parse_reduced};
use senca_sctp::{ChannelInfo, ChannelOptions, DataChannelEvent, DataChannels, SctpError};
use senca_sdp::{
    AnswerOptions, Direction, Fingerprint, IceCredentials, Msid, NegotiatedCodec, NegotiatedMedia,
    OfferCodec, OfferMedia, OfferOptions, RtcpMuxPolicy, SendSource, SessionDescription, Setup,
};
use senca_srtp::{SrtpContext, SrtpProfile, dtls_srtp_contexts};
use sha2::Digest;

use crate::bwe::BandwidthEstimator;
use crate::config::PeerConfig;
use crate::ext::{self, ExtIds, OutExt};
use crate::inbound::{InboundStream, SessionClock, codec_of};
use crate::outbound::OutboundTrack;
use crate::pacer::{Pacer, PacketClass};
use crate::sdpfrag::{SdpFrag, SdpFragError};
use crate::twcc::{SendHistory, TwccRecorder};

/// 誤り。秘密（鍵・ICE の pwd）は含めない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebRtcError {
    Sdp(senca_sdp::Error),
    /// 交渉できない（BUNDLE・rtcp-mux・fingerprint がない、など）。
    Negotiation(&'static str),
    Ice(senca_ice::IceError),
    Dtls(DtlsError),
    Rtp(senca_rtp::Error),
    Sctp(SctpError),
    SdpFrag(SdpFragError),
    /// まだつながっていない（DTLS-SRTP の鍵がない）。
    NotConnected,
    /// 送信トラックが無い。
    UnknownTrack,
    /// このトラックで交渉していないコーデック。
    UnsupportedCodec,
    /// DataChannel を交渉していない。
    NoDataChannel,
    InvalidState(&'static str),
    /// 乱数源が使えない。
    Random,
    /// 閉じている。
    Closed,
}

impl fmt::Display for WebRtcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WebRtcError::Sdp(e) => write!(f, "SDP: {e}"),
            WebRtcError::Negotiation(s) => write!(f, "交渉できない: {s}"),
            WebRtcError::Ice(e) => write!(f, "ICE: {e}"),
            WebRtcError::Dtls(e) => write!(f, "DTLS: {e}"),
            WebRtcError::Rtp(e) => write!(f, "RTP: {e:?}"),
            WebRtcError::Sctp(e) => write!(f, "SCTP: {e}"),
            WebRtcError::SdpFrag(e) => write!(f, "{e}"),
            WebRtcError::NotConnected => f.write_str("つながっていない"),
            WebRtcError::UnknownTrack => f.write_str("トラックが無い"),
            WebRtcError::UnsupportedCodec => f.write_str("交渉していないコーデック"),
            WebRtcError::NoDataChannel => f.write_str("DataChannel を交渉していない"),
            WebRtcError::InvalidState(s) => write!(f, "状態が不正: {s}"),
            WebRtcError::Random => f.write_str("乱数源が使えない"),
            WebRtcError::Closed => f.write_str("閉じている"),
        }
    }
}

impl std::error::Error for WebRtcError {}

impl From<senca_sdp::Error> for WebRtcError {
    fn from(e: senca_sdp::Error) -> Self {
        WebRtcError::Sdp(e)
    }
}
impl From<senca_ice::IceError> for WebRtcError {
    fn from(e: senca_ice::IceError) -> Self {
        WebRtcError::Ice(e)
    }
}
impl From<SctpError> for WebRtcError {
    fn from(e: SctpError) -> Self {
        WebRtcError::Sctp(e)
    }
}

/// 受信トラックの情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInfo {
    pub track: TrackId,
    pub mid: String,
    pub kind: MediaKind,
    pub codec: Codec,
    /// simulcast の層（RFC 8851 の rid）。simulcast でなければ `None`。
    pub rid: Option<String>,
    pub ssrc: u32,
    pub payload_type: u8,
}

/// 送信トラックの情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendTrackInfo {
    pub track: TrackId,
    pub mid: String,
    /// simulcast の層（自分が simulcast で送るとき）。
    pub rid: Option<String>,
    pub kind: MediaKind,
    pub ssrc: u32,
    pub rtx_ssrc: Option<u32>,
    /// 交渉できたコーデック（相手の好みの順）。
    pub codecs: Vec<Codec>,
}

/// 切断の理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    /// ICE が失敗した（consent が切れた・候補が全部失敗した）。
    IceFailed,
    /// DTLS の handshake が失敗した。
    DtlsFailed(String),
    /// 相手が DTLS を閉じた。
    DtlsClosed,
    /// 相手の証明書が SDP の fingerprint と合わない。
    FingerprintMismatch,
    /// こちらが閉じた。
    Closed,
}

/// 出来事。
#[derive(Debug, Clone)]
pub enum Event {
    IceStateChanged(IceState),
    /// 自分の候補（trickle で相手へ。`candidate:...` の形）。full ICE のときだけ出る。
    LocalCandidate(String),
    GatheringComplete,
    /// DTLS-SRTP の鍵ができ、メディアを送受信できる。
    Connected,
    /// 受信トラックが始まった（最初の packet で出る。コーデックが変わったときも出る）。
    Track(TrackInfo),
    AccessUnit {
        track: TrackId,
        au: AccessUnit,
        /// 設定が変わったときだけ（H.264 の SPS・PPS など）。
        config: Option<CodecConfig>,
    },
    /// 送信トラックにキーフレームが要る（相手の PLI・FIR、または入力の切り替え・packet の破棄）。
    KeyframeRequest {
        track: TrackId,
    },
    DataChannelOpen(ChannelInfo),
    DataChannelMessage {
        id: u16,
        data: Bytes,
        binary: bool,
    },
    DataChannelClosed {
        id: u16,
    },
    /// 下り（こちらから相手へ）の推定帯域。
    BandwidthEstimate {
        bps: u64,
    },
    Disconnected {
        reason: DisconnectReason,
    },
}

/// offer に載せる m 行 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferMediaSpec {
    pub kind: MediaKind,
    /// 自分から見た方向（WHEP の配信は `SendOnly`）。
    pub direction: Direction,
    /// 載せるコーデック（`None` なら能力の全部）。
    pub codecs: Option<Vec<Codec>>,
    /// 映像を simulcast で送る層の rid（低い順、RFC 8853）。空なら simulcast にしない。
    pub simulcast: Vec<String>,
}

/// offer の内容。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OfferSpec {
    pub media: Vec<OfferMediaSpec>,
    pub datachannel: bool,
}

/// 交渉した m 行。
#[derive(Debug, Clone)]
struct MediaLine {
    mid: String,
    direction: Direction,
    codecs: Vec<NegotiatedCodec>,
    rids: Vec<String>,
    remote_ssrcs: Vec<u32>,
    /// FID（primary, rtx）。
    fid: Vec<(u32, u32)>,
    rtcp_rsize: bool,
}

#[derive(Debug, Clone)]
struct Queued {
    out: usize,
    pkt: RtpPacket,
    rtx: bool,
}

/// 受信の頻度の制限（token bucket）。
#[derive(Debug)]
struct Bucket {
    packets: f64,
    bytes: f64,
    last: Option<Instant>,
    dropped: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnState {
    New,
    Connected,
    Closed,
}

fn random_u32() -> Result<u32, WebRtcError> {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).map_err(|_| WebRtcError::Random)?;
    Ok(u32::from_be_bytes(b))
}

fn random_u64() -> Result<u64, WebRtcError> {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).map_err(|_| WebRtcError::Random)?;
    Ok(u64::from_be_bytes(b) & 0x3FFF_FFFF_FFFF_FFFF)
}

fn core_kind(k: senca_sdp::MediaKind) -> Option<MediaKind> {
    match k {
        senca_sdp::MediaKind::Audio => Some(MediaKind::Audio),
        senca_sdp::MediaKind::Video => Some(MediaKind::Video),
        _ => None,
    }
}

fn sdp_kind(k: MediaKind) -> senca_sdp::MediaKind {
    match k {
        MediaKind::Audio => senca_sdp::MediaKind::Audio,
        _ => senca_sdp::MediaKind::Video,
    }
}

/// STUN の Binding 要求の USERNAME（`受け手:送り手`）から受け手（このサーバー）の ufrag を取り出す。
/// 1 つの UDP ソケットを多くの接続で共有するとき、最初の packet の振り分けに使う。
pub fn stun_username_ufrag(datagram: &[u8]) -> Option<String> {
    if !senca_stun::is_stun(datagram) {
        return None;
    }
    let m = senca_stun::Message::parse(Bytes::copy_from_slice(datagram)).ok()?;
    let u = m.username()?;
    let (local, _) = u.split_once(':')?;
    Some(local.to_string())
}

/// サーバー側の PeerConnection。
pub struct PeerConnection {
    cfg: PeerConfig,
    start: Instant,
    cert: DtlsCertificate,
    ice: Option<IceAgent>,
    ice_creds: senca_ice::Credentials,
    host_addrs: Vec<SocketAddr>,
    ice_role: Option<IceRole>,
    session_id: u64,
    session_version: u64,
    cname: String,
    rtcp_ssrc: u32,
    // 交渉
    local_offer: Option<(SessionDescription, OfferSpec)>,
    remote_ufrag: Option<String>,
    /// こちらから ICE restart を始め、相手の新しい資格情報を待っている。
    restart_pending: bool,
    remote_fp: Option<Fingerprint>,
    dtls_client: Option<bool>,
    media: Vec<MediaLine>,
    bundle_mid: Option<String>,
    bundle_m_line: String,
    datachannel_negotiated: bool,
    remote_max_message_size: Option<u64>,
    ext_ids: ExtIds,
    // 輸送
    dtls: Option<DtlsEndpoint>,
    pending_dtls: VecDeque<Bytes>,
    srtp_tx: Option<SrtpContext>,
    srtp_rx: Option<SrtpContext>,
    sctp: Option<DataChannels>,
    pending_channels: Vec<ChannelOptions>,
    // メディア
    inbound: Vec<InboundStream>,
    ssrc_map: HashMap<u32, (usize, bool)>,
    outbound: Vec<OutboundTrack>,
    next_track: u32,
    clock: SessionClock,
    keyframes: KeyframeRequestLimiter,
    kf_events: HashMap<TrackId, Instant>,
    twcc_rx: TwccRecorder,
    send_history: SendHistory,
    bwe: BandwidthEstimator,
    last_bwe: Option<(u64, Instant)>,
    pacer: Pacer<Queued>,
    rtt: Duration,
    rtt_known: bool,
    /// 送りたい速さ（bit/s、0 なら padding で探らない）。
    desired_bps: u64,
    /// 実際に送った量（時刻, byte）の直近 500 ms。
    sent_window: VecDeque<(Instant, usize)>,
    sent_window_bytes: usize,
    padding_budget: f64,
    padding_last: Option<Instant>,
    /// 探りの組（番号, 速さ, 終わる時刻）。
    probe: Option<(u32, f64, Instant)>,
    next_probe: Option<Instant>,
    last_estimate: u64,
    next_rtcp: Option<Instant>,
    next_twcc: Option<Instant>,
    // 出口
    transmits: VecDeque<Transmit>,
    events: VecDeque<Event>,
    bucket: Bucket,
    state: ConnState,
    last_now: Instant,
    /// 捨てた数: (出来事, 送信, 認証に通らない SRTP)。
    dropped: (u64, u64, u64),
}

impl fmt::Debug for PeerConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerConnection")
            .field("state", &self.state)
            .field("ice", &self.ice.as_ref().map(|i| i.state()))
            .field(
                "dtls_connected",
                &self.dtls.as_ref().map(|d| d.is_connected()),
            )
            .field("inbound", &self.inbound.len())
            .field("outbound", &self.outbound.len())
            .finish_non_exhaustive()
    }
}

impl PeerConnection {
    pub fn new(cfg: PeerConfig, now: Instant) -> Result<PeerConnection, WebRtcError> {
        let cert = match &cfg.certificate {
            Some(c) => c.clone(),
            None => DtlsCertificate::generate_self_signed("senca").map_err(WebRtcError::Dtls)?,
        };
        let ice_creds = senca_ice::Credentials::random()?;
        let cname = match &cfg.cname {
            Some(c) => c.clone(),
            None => format!("{:016x}", random_u64()?),
        };
        let rtcp_ssrc = random_u32()?;
        Ok(PeerConnection {
            keyframes: KeyframeRequestLimiter::new(rtcp_ssrc, cfg.keyframe_min_interval, false, 64),
            twcc_rx: TwccRecorder::new(now),
            send_history: SendHistory::new(),
            bwe: BandwidthEstimator::new(cfg.bwe.clone()),
            pacer: Pacer::new(cfg.pacer.clone(), cfg.bwe.start_bps),
            bucket: Bucket {
                packets: cfg.rate_limit.packets_per_second as f64,
                bytes: cfg.rate_limit.bytes_per_second as f64,
                last: None,
                dropped: 0,
            },
            start: now,
            cert,
            ice: None,
            ice_creds,
            host_addrs: Vec::new(),
            ice_role: None,
            session_id: random_u64()?,
            session_version: 1,
            cname,
            rtcp_ssrc,
            local_offer: None,
            remote_ufrag: None,
            restart_pending: false,
            remote_fp: None,
            dtls_client: None,
            media: Vec::new(),
            bundle_mid: None,
            bundle_m_line: "audio 9 UDP/TLS/RTP/SAVPF 0".into(),
            datachannel_negotiated: false,
            remote_max_message_size: None,
            ext_ids: ExtIds::default(),
            dtls: None,
            pending_dtls: VecDeque::new(),
            srtp_tx: None,
            srtp_rx: None,
            sctp: None,
            pending_channels: Vec::new(),
            inbound: Vec::new(),
            ssrc_map: HashMap::new(),
            outbound: Vec::new(),
            next_track: 1,
            clock: SessionClock {
                start: None,
                base_ns: None,
            },
            kf_events: HashMap::new(),
            last_bwe: None,
            rtt: Duration::from_millis(100),
            rtt_known: false,
            desired_bps: 0,
            sent_window: VecDeque::new(),
            sent_window_bytes: 0,
            padding_budget: 0.0,
            padding_last: None,
            probe: None,
            next_probe: None,
            last_estimate: 0,
            next_rtcp: None,
            next_twcc: None,
            transmits: VecDeque::new(),
            events: VecDeque::new(),
            state: ConnState::New,
            last_now: now,
            dropped: (0, 0, 0),
            cfg,
        })
    }

    // ------------------------------------------------------------------ 情報

    /// 自分の ICE の ufrag（共有ソケットでの振り分けに使う。[`stun_username_ufrag`] と比べる）。
    pub fn local_ufrag(&self) -> &str {
        &self.ice_creds.ufrag
    }

    /// 自分の証明書の SHA-256 fingerprint（SDP の形）。
    pub fn local_fingerprint(&self) -> String {
        self.cert.sdp_fingerprint()
    }

    pub fn ice_state(&self) -> IceState {
        self.ice.as_ref().map_or(IceState::New, |i| i.state())
    }

    /// DTLS-SRTP の鍵ができている。
    pub fn is_connected(&self) -> bool {
        self.state == ConnState::Connected
    }

    pub fn is_closed(&self) -> bool {
        self.state == ConnState::Closed
    }

    /// 選んだ経路（自分, 相手）。
    pub fn selected_path(&self) -> Option<(SocketAddr, SocketAddr)> {
        let (l, r) = self.ice.as_ref()?.selected_pair()?;
        Some((l.addr()?, r.addr()?))
    }

    /// 送信トラックの一覧。
    pub fn send_tracks(&self) -> Vec<SendTrackInfo> {
        self.outbound
            .iter()
            .filter(|o| o.active)
            .map(|o| SendTrackInfo {
                track: o.track,
                mid: o.mid.clone(),
                rid: o.rid.clone(),
                kind: o.kind,
                ssrc: o.ssrc,
                rtx_ssrc: o.rtx_ssrc,
                codecs: o.negotiated_codecs(),
            })
            .collect()
    }

    /// 受信トラックの一覧（packet が届いたもの）。
    pub fn receive_tracks(&self) -> Vec<TrackInfo> {
        self.inbound.iter().map(Self::info).collect()
    }

    fn info(s: &InboundStream) -> TrackInfo {
        TrackInfo {
            track: s.track,
            mid: s.mid.clone(),
            kind: s.kind,
            codec: s.codec,
            rid: s.rid.clone(),
            ssrc: s.ssrc,
            payload_type: s.payload_type,
        }
    }

    /// 今の推定帯域（下り）。
    pub fn bandwidth_estimate(&self) -> u64 {
        self.bwe.estimate(self.last_now)
    }

    /// 往復時間の見積もり。
    pub fn rtt(&self) -> Duration {
        self.rtt
    }

    /// 捨てた数（出来事の溢れ, 送信の溢れ, 認証に通らない SRTP/SRTCP, 頻度の制限）。
    pub fn dropped_counters(&self) -> (u64, u64, u64, u64) {
        (
            self.dropped.0,
            self.dropped.1,
            self.dropped.2,
            self.bucket.dropped,
        )
    }

    /// (再送した packet の数, NACK で求めた番号の延べ数)。
    pub fn retransmission_counters(&self) -> (u64, u64) {
        (
            self.outbound.iter().map(|o| o.retransmitted).sum(),
            self.inbound
                .iter()
                .filter_map(|s| s.nack.as_ref())
                .map(|n| n.counters().0)
                .sum(),
        )
    }

    // ------------------------------------------------------------------ ICE

    /// 受けるソケットのアドレスを足す（host 候補になる）。交渉の前に呼ぶ。
    pub fn add_host_address(&mut self, now: Instant, addr: SocketAddr) -> Result<(), WebRtcError> {
        if self.host_addrs.contains(&addr) {
            return Ok(());
        }
        if self.host_addrs.len() >= 16 {
            return Err(WebRtcError::InvalidState("host アドレスが多すぎる"));
        }
        self.host_addrs.push(addr);
        if let Some(ice) = self.ice.as_mut() {
            ice.add_local_socket(now, addr)?;
        }
        Ok(())
    }

    fn ensure_ice(&mut self, now: Instant, role: IceRole) -> Result<(), WebRtcError> {
        if self.ice.is_some() {
            return Ok(());
        }
        let cfg = IceConfig {
            role: if self.cfg.ice_lite {
                IceRole::Controlled
            } else {
                role
            },
            lite: self.cfg.ice_lite,
            stun_servers: if self.cfg.ice_lite {
                Vec::new()
            } else {
                self.cfg.stun_servers.clone()
            },
            turn_servers: if self.cfg.ice_lite {
                Vec::new()
            } else {
                self.cfg.turn_servers.clone()
            },
            local_credentials: Some(self.ice_creds.clone()),
            software: None,
            ..Default::default()
        };
        let mut ice = IceAgent::new(cfg)?;
        for a in &self.host_addrs {
            ice.add_local_socket(now, *a)?;
        }
        self.ice = Some(ice);
        self.ice_role = Some(role);
        self.pump(now);
        Ok(())
    }

    fn local_sdp_candidates(&self) -> Vec<senca_sdp::Candidate> {
        let Some(ice) = &self.ice else {
            return Vec::new();
        };
        ice.local_candidates()
            .iter()
            .filter_map(|c| {
                let s = c.to_sdp_attribute();
                senca_sdp::Candidate::parse(s.strip_prefix("candidate:").unwrap_or(&s)).ok()
            })
            .collect()
    }

    fn gathering_complete(&self) -> bool {
        self.cfg.ice_lite || self.ice.as_ref().is_some_and(|i| i.is_gathering_complete())
    }

    /// 相手の候補（`candidate:...`、trickle）を足す。mDNS の名前は解決できなければ無視する。
    pub fn add_remote_candidate(
        &mut self,
        now: Instant,
        candidate: &str,
    ) -> Result<(), WebRtcError> {
        let ice = self
            .ice
            .as_mut()
            .ok_or(WebRtcError::InvalidState("交渉の前"))?;
        match ice.add_remote_candidate_str(now, candidate) {
            Ok(_) | Err(senca_ice::IceError::UnresolvableHostname(_)) => {}
            Err(e) => return Err(e.into()),
        }
        self.pump(now);
        Ok(())
    }

    /// 相手の end-of-candidates。
    pub fn end_of_remote_candidates(&mut self, now: Instant) {
        if let Some(ice) = self.ice.as_mut() {
            ice.end_of_remote_candidates(now);
        }
        self.pump(now);
    }

    /// WHIP の PATCH（`application/trickle-ice-sdpfrag`、RFC 9725 4.3）を処理する。
    ///
    /// 候補を足す。ufrag・pwd が今と違えば ICE restart とみなし、新しい自分の資格情報と候補を載せた
    /// 応答の断片を返す（HTTP 200 の本文）。trickle だけなら `None`（HTTP 204）。
    /// ufrag が今のものと違い pwd が無ければ `Err`（HTTP 409 相当）。
    pub fn handle_trickle_patch(
        &mut self,
        now: Instant,
        body: &str,
    ) -> Result<Option<String>, WebRtcError> {
        let frag = SdpFrag::parse(body).map_err(WebRtcError::SdpFrag)?;
        if self.ice.is_none() {
            return Err(WebRtcError::InvalidState("交渉の前"));
        }
        let mut response = None;
        if let Some((u, p)) = frag.credentials()
            && self.remote_ufrag.as_deref() != Some(u)
        {
            let (u, p) = (u.to_string(), p.to_string());
            if self.restart_pending {
                // こちらが始めた restart への応答（WHIP client 側）
                let ice = self
                    .ice
                    .as_mut()
                    .ok_or(WebRtcError::InvalidState("交渉の前"))?;
                ice.set_remote_credentials(&u, &p)?;
                self.remote_ufrag = Some(u);
                self.restart_pending = false;
                let cands: Vec<String> = frag.all_candidates().map(str::to_string).collect();
                for c in cands {
                    self.add_remote_candidate(now, &c)?;
                }
                if frag.has_end_of_candidates() {
                    self.end_of_remote_candidates(now);
                }
                return Ok(None);
            }
            self.restart_ice_with_remote(now, &u, &p)?;
            let cands: Vec<String> = self
                .local_sdp_candidates()
                .iter()
                .map(|c| format!("candidate:{c}"))
                .collect();
            let mid = self.bundle_mid.clone().unwrap_or_else(|| "0".into());
            response = Some(
                SdpFrag::build(
                    &mid,
                    &self.bundle_m_line,
                    &self.ice_creds.ufrag,
                    &self.ice_creds.pwd,
                    &cands,
                    self.gathering_complete(),
                )
                .to_string(),
            );
        } else if let Some(u) = frag.ufrag()
            && self.remote_ufrag.as_deref() != Some(u)
        {
            return Err(WebRtcError::Negotiation("ufrag が今の ICE session と違う"));
        }
        let cands: Vec<String> = frag.all_candidates().map(str::to_string).collect();
        for c in cands {
            self.add_remote_candidate(now, &c)?;
        }
        if frag.has_end_of_candidates() {
            self.end_of_remote_candidates(now);
        }
        Ok(response)
    }

    fn restart_ice_with_remote(
        &mut self,
        now: Instant,
        ufrag: &str,
        pwd: &str,
    ) -> Result<(), WebRtcError> {
        let ice = self
            .ice
            .as_mut()
            .ok_or(WebRtcError::InvalidState("交渉の前"))?;
        self.ice_creds = ice.restart(now)?;
        ice.set_remote_credentials(ufrag, pwd)?;
        self.remote_ufrag = Some(ufrag.to_string());
        self.session_version += 1;
        self.pump(now);
        Ok(())
    }

    /// こちらから ICE restart を始める。新しい資格情報は次の [`create_offer`](Self::create_offer) か
    /// [`local_sdpfrag`](Self::local_sdpfrag)（WHIP の PATCH の本文）で相手へ渡す。相手の新しい資格情報は
    /// [`apply_answer`](Self::apply_answer) か [`handle_trickle_patch`](Self::handle_trickle_patch)（PATCH の応答）で入れる。
    pub fn restart_ice(&mut self, now: Instant) -> Result<(), WebRtcError> {
        let ice = self
            .ice
            .as_mut()
            .ok_or(WebRtcError::InvalidState("交渉の前"))?;
        self.ice_creds = ice.restart(now)?;
        self.restart_pending = true;
        self.session_version += 1;
        self.pump(now);
        Ok(())
    }

    /// 自分の ICE の資格情報と候補の SDP 断片（trickle・ICE restart の PATCH の本文や、その応答）。
    pub fn local_sdpfrag(&self) -> String {
        let cands: Vec<String> = self
            .local_sdp_candidates()
            .iter()
            .map(|c| format!("candidate:{c}"))
            .collect();
        let mid = self.bundle_mid.clone().unwrap_or_else(|| "0".into());
        SdpFrag::build(
            &mid,
            &self.bundle_m_line,
            &self.ice_creds.ufrag,
            &self.ice_creds.pwd,
            &cands,
            self.gathering_complete(),
        )
        .to_string()
    }

    // ------------------------------------------------------------------ 交渉

    fn fingerprint(&self) -> Fingerprint {
        Fingerprint {
            hash_function: "sha-256".into(),
            bytes: self.cert.fingerprint_sha256().to_vec(),
        }
    }

    fn alloc_track(&mut self) -> TrackId {
        let t = TrackId(self.next_track);
        self.next_track = self.next_track.wrapping_add(1).max(1);
        t
    }

    fn new_outbound(
        &mut self,
        mid: &str,
        kind: MediaKind,
        rid: Option<&str>,
    ) -> Result<usize, WebRtcError> {
        if let Some(i) = self
            .outbound
            .iter()
            .position(|o| o.mid == mid && o.rid.as_deref() == rid)
        {
            return Ok(i);
        }
        if self.outbound.len() >= 64 {
            return Err(WebRtcError::InvalidState("送信トラックが多すぎる"));
        }
        let ssrc = random_u32()?;
        let rtx = if kind == MediaKind::Video && self.cfg.capabilities.rtx {
            Some(random_u32()?)
        } else {
            None
        };
        let track = self.alloc_track();
        self.outbound.push(OutboundTrack::new(
            track,
            mid,
            rid.map(str::to_string),
            kind,
            ssrc,
            rtx,
            self.cfg.retransmit_history,
        ));
        Ok(self.outbound.len() - 1)
    }

    fn send_source(&self, o: &OutboundTrack) -> SendSource {
        SendSource {
            mid: Some(o.mid.clone()),
            kind: sdp_kind(o.kind),
            ssrc: o.ssrc,
            rtx_ssrc: o.rtx_ssrc,
            cname: self.cname.clone(),
            msid: Some(Msid {
                stream_id: "senca".into(),
                track_id: Some(format!("senca-{}", o.track.0)),
            }),
        }
    }

    fn check_remote_transport(
        &mut self,
        sdp: &SessionDescription,
        idx: usize,
    ) -> Result<(String, String, Fingerprint), WebRtcError> {
        let ufrag = sdp
            .ice_ufrag(idx)
            .ok_or(WebRtcError::Negotiation("ice-ufrag がない"))?;
        let pwd = sdp
            .ice_pwd(idx)
            .ok_or(WebRtcError::Negotiation("ice-pwd がない"))?;
        let fp = sdp
            .fingerprint(idx)
            .ok_or(WebRtcError::Negotiation("fingerprint がない"))?;
        if !matches!(fp.hash_function.as_str(), "sha-256" | "sha-384" | "sha-512") {
            return Err(WebRtcError::Negotiation(
                "fingerprint のハッシュは sha-256/384/512 だけ",
            ));
        }
        if let Some(old) = &self.remote_fp
            && *old != fp
        {
            return Err(WebRtcError::Negotiation("再交渉で fingerprint が変わった"));
        }
        Ok((ufrag.to_string(), pwd.to_string(), fp))
    }

    /// BUNDLE の最初のグループに、受け付ける m 行が全部入っていること。
    fn check_bundle(sdp: &SessionDescription) -> Result<Vec<String>, WebRtcError> {
        let groups = sdp.bundle_groups();
        let g = groups
            .first()
            .ok_or(WebRtcError::Negotiation("BUNDLE がない"))?;
        for m in &sdp.media {
            if m.port == 0 && !m.has_flag("bundle-only") {
                continue;
            }
            match m.mid() {
                Some(mid) if g.iter().any(|x| x == mid) => {}
                _ => return Err(WebRtcError::Negotiation("BUNDLE に入っていない m 行がある")),
            }
        }
        Ok(g.clone())
    }

    fn apply_remote_ice(
        &mut self,
        now: Instant,
        sdp: &SessionDescription,
        idx: usize,
        ufrag: &str,
        pwd: &str,
    ) -> Result<(), WebRtcError> {
        let restart =
            self.remote_ufrag.as_deref().is_some_and(|u| u != ufrag) || self.restart_pending;
        self.restart_pending = false;
        let ice = self.ice.as_mut().ok_or(WebRtcError::InvalidState("ICE"))?;
        if restart {
            // 相手からの ICE restart: 自分の資格情報は accept_offer の前に作り直している
            ice.set_remote_credentials(ufrag, pwd)?;
        } else if self.remote_ufrag.is_none() {
            ice.set_remote_credentials(ufrag, pwd)?;
        }
        self.remote_ufrag = Some(ufrag.to_string());
        let m = &sdp.media[idx];
        let mut eoc = m.has_flag("end-of-candidates") || sdp.has_flag("end-of-candidates");
        for c in m.attributes_named("candidate") {
            let s = format!("candidate:{c}");
            match ice.add_remote_candidate_str(now, &s) {
                Ok(_) | Err(senca_ice::IceError::UnresolvableHostname(_)) => {}
                Err(senca_ice::IceError::BadCandidate(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if self.cfg.ice_lite {
            eoc = false;
        }
        if eoc {
            ice.end_of_remote_candidates(now);
        }
        Ok(())
    }

    fn apply_media(&mut self, results: &[NegotiatedMedia]) -> Result<(), WebRtcError> {
        let mut extmaps: Vec<(u8, String)> = Vec::new();
        for nm in results {
            if nm.rejected {
                continue;
            }
            let Some(mid) = nm.mid.clone() else {
                return Err(WebRtcError::Negotiation("mid のない m 行"));
            };
            if nm.kind == senca_sdp::MediaKind::Application {
                self.datachannel_negotiated = true;
                self.remote_max_message_size = nm.remote_max_message_size;
                continue;
            }
            let Some(kind) = core_kind(nm.kind) else {
                continue;
            };
            if !nm.rtcp_mux {
                return Err(WebRtcError::Negotiation("rtcp-mux がない"));
            }
            for e in &nm.extmaps {
                if !extmaps.iter().any(|x| x.0 == e.0) {
                    extmaps.push(e.clone());
                }
            }
            let mut fid = Vec::new();
            for g in &nm.remote_ssrc_groups {
                if g.semantics.eq_ignore_ascii_case("FID") && g.ssrcs.len() == 2 {
                    fid.push((g.ssrcs[0], g.ssrcs[1]));
                }
            }
            let mut remote_ssrcs: Vec<u32> = nm.remote_ssrcs.iter().map(|s| s.ssrc).collect();
            remote_ssrcs.dedup();
            let rids: Vec<String> = nm
                .rids
                .iter()
                .take(self.cfg.max_simulcast_layers)
                .map(|r| r.id.clone())
                .collect();
            if nm.direction.sends() {
                if !self.outbound.iter().any(|o| o.mid == mid) {
                    self.new_outbound(&mid, kind, None)?;
                }
                // simulcast の層は相手が受けると答えたときだけ（answer の a=simulcast:recv）
                let sc = nm.simulcast.is_some();
                let mut first = true;
                for o in self.outbound.iter_mut().filter(|o| o.mid == mid) {
                    let active = o.rid.is_none() || sc || first;
                    first = false;
                    o.set_codecs(nm.codecs.clone(), active);
                }
            } else {
                for o in self.outbound.iter_mut().filter(|o| o.mid == mid) {
                    o.active = false;
                }
            }
            let line = MediaLine {
                mid: mid.clone(),
                direction: nm.direction,
                codecs: nm.codecs.clone(),
                rids,
                remote_ssrcs,
                fid,
                rtcp_rsize: nm.rtcp_rsize,
            };
            match self.media.iter_mut().find(|m| m.mid == mid) {
                Some(m) => *m = line,
                None => self.media.push(line),
            }
        }
        self.ext_ids = ExtIds::from_pairs(&extmaps);
        Ok(())
    }

    fn after_negotiation(&mut self, now: Instant, we_are_dtls_client: bool, fp: Fingerprint) {
        if self.dtls_client.is_none() {
            self.dtls_client = Some(we_are_dtls_client);
        }
        self.remote_fp = Some(fp);
        if self.dtls_client == Some(true) {
            self.maybe_start_dtls_client(now);
        }
        self.pump(now);
    }

    /// 相手（ブラウザー）の offer に answer を返す。
    ///
    /// 相手が送る m 行（sendonly・sendrecv）は受信トラックに、相手が受ける m 行（recvonly・sendrecv）は
    /// 送信トラックになる（[`send_tracks`](Self::send_tracks)）。2 回目以降は再交渉（m 行の追加・ICE restart）。
    pub fn accept_offer(&mut self, now: Instant, offer: &str) -> Result<String, WebRtcError> {
        self.last_now = now;
        if self.state == ConnState::Closed {
            return Err(WebRtcError::Closed);
        }
        if self.local_offer.is_some() {
            return Err(WebRtcError::InvalidState("自分の offer が答えを待っている"));
        }
        let sdp = SessionDescription::parse(offer)?;
        let bundle = Self::check_bundle(&sdp)?;
        let (bidx, _) = sdp
            .media_by_mid(&bundle[0])
            .ok_or(WebRtcError::Negotiation("BUNDLE の mid"))?;
        let (ufrag, pwd, fp) = self.check_remote_transport(&sdp, bidx)?;
        let remote_lite = sdp.has_flag("ice-lite");
        if remote_lite && self.cfg.ice_lite {
            return Err(WebRtcError::Negotiation("両方が ICE-lite"));
        }
        let role = if remote_lite {
            IceRole::Controlling
        } else {
            IceRole::Controlled
        };
        self.ensure_ice(now, role)?;
        // 相手からの ICE restart
        if self.remote_ufrag.as_deref().is_some_and(|u| u != ufrag) {
            let ice = self.ice.as_mut().ok_or(WebRtcError::InvalidState("ICE"))?;
            self.ice_creds = ice.restart(now)?;
        }
        // 送信の源を先に作る（answer に a=ssrc・a=msid を書く）
        let mut sources = Vec::new();
        for (i, m) in sdp.media.iter().enumerate() {
            if m.port == 0 && !m.has_flag("bundle-only") {
                continue;
            }
            let Some(kind) = core_kind(senca_sdp::MediaKind::from_media(&m.media)) else {
                continue;
            };
            let Some(mid) = m.mid() else { continue };
            if sdp.effective_direction(i).recvs() {
                let oi = self.new_outbound(mid, kind, None)?;
                sources.push(self.send_source(&self.outbound[oi]));
            }
        }
        let setup = match self.dtls_client {
            Some(true) => Setup::Active,
            Some(false) => Setup::Passive,
            None => Setup::Passive,
        };
        let opts = AnswerOptions {
            ice: Some(IceCredentials {
                ufrag: self.ice_creds.ufrag.clone(),
                pwd: self.ice_creds.pwd.clone(),
            }),
            ice_lite: self.cfg.ice_lite,
            ice_options: if self.cfg.ice_lite {
                Vec::new()
            } else {
                vec!["trickle".into()]
            },
            fingerprint: Some(self.fingerprint()),
            setup,
            candidates: self.local_sdp_candidates(),
            end_of_candidates: self.gathering_complete(),
            session_id: self.session_id,
            session_version: self.session_version,
            origin_address: "127.0.0.1".into(),
            audio_direction: Direction::SendRecv,
            video_direction: Direction::SendRecv,
            direction_overrides: Vec::new(),
            accept_datachannel: self.cfg.datachannel,
            sctp_port: self.cfg.sctp.local_port,
            max_message_size: self.cfg.sctp.max_receive_message_size as u64,
            rtcp_mux: RtcpMuxPolicy::Require,
            accept_simulcast: true,
            send_sources: sources,
        };
        let answer = senca_sdp::create_answer(&sdp, &self.cfg.capabilities, &opts)?;
        if answer.bundle.is_empty() {
            return Err(WebRtcError::Negotiation("BUNDLE を合意できない"));
        }
        let aidx = answer
            .media
            .iter()
            .position(|m| !m.rejected)
            .ok_or(WebRtcError::Negotiation("受け付ける m 行がない"))?;
        let our_setup = answer.sdp.setup(aidx).unwrap_or(Setup::Passive);
        self.apply_remote_ice(now, &sdp, bidx, &ufrag, &pwd)?;
        self.apply_media(&answer.media)?;
        self.bundle_mid = answer.bundle.first().and_then(|g| g.first()).cloned();
        if let Some(m) = answer.sdp.media.get(aidx) {
            self.bundle_m_line = format!(
                "{} 9 {} {}",
                m.media,
                m.protocol,
                m.formats.first().map_or("0", |s| s)
            );
        }
        self.session_version += 1;
        self.after_negotiation(now, our_setup == Setup::Active, fp);
        Ok(answer.sdp.to_string())
    }

    /// 自分から offer を作る（WHEP の server offer など）。答えは [`apply_answer`](Self::apply_answer) で入れる。
    ///
    /// 再交渉では、前の offer と同じ順の m 行は同じ mid・同じ送信トラックを使う。
    pub fn create_offer(&mut self, now: Instant, spec: &OfferSpec) -> Result<String, WebRtcError> {
        self.last_now = now;
        if self.state == ConnState::Closed {
            return Err(WebRtcError::Closed);
        }
        self.ensure_ice(now, IceRole::Controlling)?;
        let caps = self.cfg.capabilities.clone();
        let mut used_pts: Vec<u8> = caps.codecs.iter().map(|c| c.payload_type).collect();
        let mut extmaps = Vec::new();
        for (i, uri) in caps.header_extensions.iter().enumerate().take(14) {
            extmaps.push(((i + 1) as u8, uri.clone()));
        }
        let mut media = Vec::new();
        for (i, m) in spec.media.iter().enumerate() {
            let mid = i.to_string();
            let mut codecs = Vec::new();
            for c in caps
                .codecs
                .iter()
                .filter(|c| core_kind(c.kind) == Some(m.kind))
            {
                let pf = senca_rtp::PayloadFormat {
                    encoding: c.encoding.clone(),
                    clock_rate: c.clock_rate,
                    channels: c.channels,
                    fmtp: c.fmtp.clone(),
                };
                let Some(codec) = pf.codec() else { continue };
                if m.codecs.as_ref().is_some_and(|l| !l.contains(&codec)) {
                    continue;
                }
                let mut oc = OfferCodec::from(c);
                if m.kind == MediaKind::Video && caps.rtx {
                    let mut pt = c.payload_type.wrapping_add(1);
                    if used_pts.contains(&pt) || pt > 127 {
                        pt = (96..=127)
                            .chain(35..=63)
                            .find(|p| !used_pts.contains(p))
                            .ok_or(WebRtcError::Negotiation("payload type が足りない"))?;
                    }
                    used_pts.push(pt);
                    oc.rtx_payload_type = Some(pt);
                }
                codecs.push(oc);
            }
            if codecs.is_empty() {
                return Err(WebRtcError::Negotiation("載せられるコーデックがない"));
            }
            let simulcast =
                m.kind == MediaKind::Video && m.direction.sends() && !m.simulcast.is_empty();
            let send = if m.direction.sends() && !simulcast {
                let oi = self.new_outbound(&mid, m.kind, None)?;
                Some(self.send_source(&self.outbound[oi]))
            } else {
                None
            };
            if simulcast {
                if m.simulcast.len() > 8
                    || m.simulcast.iter().any(|r| {
                        r.is_empty()
                            || r.len() > 16
                            || !r.bytes().all(|b| b.is_ascii_alphanumeric())
                    })
                {
                    return Err(WebRtcError::Negotiation("simulcast の rid が不正"));
                }
                for r in &m.simulcast {
                    self.new_outbound(&mid, m.kind, Some(r))?;
                }
            }
            media.push(OfferMedia {
                kind: sdp_kind(m.kind),
                mid,
                direction: m.direction,
                codecs,
                extmaps: extmaps.clone(),
                send,
                control: None,
                datachannel: false,
            });
        }
        if spec.datachannel && self.cfg.datachannel {
            media.push(OfferMedia {
                kind: senca_sdp::MediaKind::Application,
                mid: spec.media.len().to_string(),
                direction: Direction::SendRecv,
                codecs: Vec::new(),
                extmaps: Vec::new(),
                send: None,
                control: None,
                datachannel: true,
            });
        }
        if media.is_empty() {
            return Err(WebRtcError::Negotiation("m 行がない"));
        }
        let setup = match self.dtls_client {
            Some(true) => Setup::Active,
            Some(false) => Setup::Passive,
            None => Setup::ActPass,
        };
        let opts = OfferOptions {
            session_id: self.session_id,
            session_version: self.session_version,
            ice: Some(IceCredentials {
                ufrag: self.ice_creds.ufrag.clone(),
                pwd: self.ice_creds.pwd.clone(),
            }),
            ice_lite: self.cfg.ice_lite,
            ice_options: if self.cfg.ice_lite {
                Vec::new()
            } else {
                vec!["trickle".into()]
            },
            fingerprint: Some(self.fingerprint()),
            setup,
            candidates: self.local_sdp_candidates(),
            end_of_candidates: self.gathering_complete(),
            sctp_port: self.cfg.sctp.local_port,
            max_message_size: self.cfg.sctp.max_receive_message_size as u64,
            ..Default::default()
        };
        let mut sdp = senca_sdp::create_offer(&media, &opts)?;
        for (i, m) in spec.media.iter().enumerate() {
            if m.kind != MediaKind::Video || !m.direction.sends() || m.simulcast.is_empty() {
                continue;
            }
            if let Some(md) = sdp.media.get_mut(i) {
                for r in &m.simulcast {
                    md.push_attr("rid", format!("{r} send"));
                }
                md.push_attr("simulcast", format!("send {}", m.simulcast.join(";")));
            }
        }
        self.session_version += 1;
        let text = sdp.to_string();
        self.bundle_mid = Some("0".into());
        if let Some(m) = sdp.media.first() {
            self.bundle_m_line = format!(
                "{} 9 {} {}",
                m.media,
                m.protocol,
                m.formats.first().map_or("0", |s| s)
            );
        }
        self.local_offer = Some((sdp, spec.clone()));
        Ok(text)
    }

    /// 自分の offer への answer を入れる。
    pub fn apply_answer(&mut self, now: Instant, answer: &str) -> Result<(), WebRtcError> {
        self.last_now = now;
        let (offer, _) = self
            .local_offer
            .take()
            .ok_or(WebRtcError::InvalidState("offer を出していない"))?;
        let sdp = SessionDescription::parse(answer)?;
        let bundle = Self::check_bundle(&sdp)?;
        let results = senca_sdp::apply_answer(&offer, &sdp)?;
        let (bidx, _) = sdp
            .media_by_mid(&bundle[0])
            .ok_or(WebRtcError::Negotiation("BUNDLE の mid"))?;
        let (ufrag, pwd, fp) = self.check_remote_transport(&sdp, bidx)?;
        if sdp.has_flag("ice-lite") && self.cfg.ice_lite {
            return Err(WebRtcError::Negotiation("両方が ICE-lite"));
        }
        let remote_setup = sdp
            .setup(bidx)
            .ok_or(WebRtcError::Negotiation("setup がない"))?;
        let we_client = match remote_setup {
            Setup::Active => false,
            Setup::Passive => true,
            _ => {
                return Err(WebRtcError::Negotiation(
                    "answer の setup は active か passive",
                ));
            }
        };
        if self.dtls_client.is_some_and(|c| c != we_client) {
            return Err(WebRtcError::Negotiation("再交渉で DTLS の役割が変わった"));
        }
        self.apply_remote_ice(now, &sdp, bidx, &ufrag, &pwd)?;
        self.apply_media(&results)?;
        self.after_negotiation(now, we_client, fp);
        Ok(())
    }

    // ------------------------------------------------------------------ DataChannel

    /// DataChannel を開く（DCEP）。つながる前に呼ぶと、つながってから開く。番号はつながってから決まるので、
    /// つながる前は `Ok(None)` を返し、開いたら [`Event::DataChannelOpen`] で知らせる。
    pub fn create_data_channel(
        &mut self,
        now: Instant,
        opts: ChannelOptions,
    ) -> Result<Option<u16>, WebRtcError> {
        if !self.datachannel_negotiated && self.local_offer.is_none() && self.ice.is_some() {
            return Err(WebRtcError::NoDataChannel);
        }
        match self.sctp.as_mut() {
            Some(s) => {
                let id = s.open(now, opts)?;
                self.pump(now);
                Ok(Some(id))
            }
            None => {
                if self.pending_channels.len() >= 64 {
                    return Err(WebRtcError::InvalidState(
                        "開く待ちの DataChannel が多すぎる",
                    ));
                }
                self.pending_channels.push(opts);
                Ok(None)
            }
        }
    }

    /// 文字列を送る（字幕・メタデータ、R6-4）。
    pub fn send_data_text(&mut self, now: Instant, id: u16, text: &str) -> Result<(), WebRtcError> {
        let s = self.sctp.as_mut().ok_or(WebRtcError::NotConnected)?;
        s.send_text(now, id, text)?;
        self.pump(now);
        Ok(())
    }

    /// バイナリを送る。
    pub fn send_data_binary(
        &mut self,
        now: Instant,
        id: u16,
        data: Bytes,
    ) -> Result<(), WebRtcError> {
        let s = self.sctp.as_mut().ok_or(WebRtcError::NotConnected)?;
        s.send_binary(now, id, data)?;
        self.pump(now);
        Ok(())
    }

    /// DataChannel を閉じる（stream reset）。
    pub fn close_data_channel(&mut self, now: Instant, id: u16) -> Result<(), WebRtcError> {
        let s = self.sctp.as_mut().ok_or(WebRtcError::NotConnected)?;
        s.close(now, id)?;
        self.pump(now);
        Ok(())
    }

    /// 開いている DataChannel。
    pub fn data_channels(&self) -> Vec<ChannelInfo> {
        self.sctp.as_ref().map(|s| s.channels()).unwrap_or_default()
    }

    /// 名前（label）で DataChannel を探す。
    pub fn data_channel_by_label(&self, label: &str) -> Option<u16> {
        self.data_channels()
            .into_iter()
            .find(|c| c.label == label && c.open)
            .map(|c| c.id)
    }

    // ------------------------------------------------------------------ 入力

    fn allow_packet(&mut self, now: Instant, len: usize) -> bool {
        let rl = &self.cfg.rate_limit;
        let b = &mut self.bucket;
        let last = *b.last.get_or_insert(now);
        let dt = now.saturating_duration_since(last).as_secs_f64();
        b.last = Some(now.max(last));
        b.packets =
            (b.packets + dt * rl.packets_per_second as f64).min(rl.packets_per_second as f64);
        b.bytes = (b.bytes + dt * rl.bytes_per_second as f64).min(rl.bytes_per_second as f64);
        if b.packets < 1.0 || b.bytes < len as f64 {
            b.dropped += 1;
            return false;
        }
        b.packets -= 1.0;
        b.bytes -= len as f64;
        true
    }

    /// 受けた datagram を処理する（受けたソケットが最初の host アドレスのとき）。
    pub fn handle_datagram(&mut self, now: Instant, from: SocketAddr, data: &[u8]) {
        let Some(local) = self.host_addrs.first().copied() else {
            return;
        };
        self.handle_datagram_on(now, local, from, data);
    }

    /// 受けた datagram を処理する。`local` は受けたソケットのアドレス。
    pub fn handle_datagram_on(
        &mut self,
        now: Instant,
        local: SocketAddr,
        from: SocketAddr,
        data: &[u8],
    ) {
        self.last_now = now;
        if self.state == ConnState::Closed || data.is_empty() || data.len() > 65535 {
            return;
        }
        if !self.allow_packet(now, data.len()) {
            return;
        }
        let Some(ice) = self.ice.as_mut() else {
            return;
        };
        let Some(app) = ice.handle_input(now, local, from, Bytes::copy_from_slice(data)) else {
            self.pump(now);
            return;
        };
        match senca_ice::classify(&app) {
            senca_ice::PacketKind::Dtls => self.on_dtls_datagram(now, &app),
            senca_ice::PacketKind::Rtp => {
                if senca_ice::demux::is_rtcp(&app) {
                    self.on_srtcp(now, &app);
                } else {
                    self.on_srtp(now, &app);
                }
            }
            _ => {}
        }
        self.pump(now);
    }

    fn on_dtls_datagram(&mut self, now: Instant, data: &[u8]) {
        if self.dtls.is_none() {
            if self.dtls_client != Some(false) {
                return; // 自分が client なら相手の ClientHello は受けない（作ってから受ける）
            }
            if let Err(e) = self.create_dtls(now, false) {
                self.fail(DisconnectReason::DtlsFailed(e.to_string()));
                return;
            }
        }
        let Some(d) = self.dtls.as_mut() else { return };
        if let Err(e) = d.handle_datagram(data, now) {
            self.on_dtls_error(e);
        }
    }

    fn on_dtls_error(&mut self, e: DtlsError) {
        match e {
            DtlsError::Fatal {
                alert: senca_dtls::AlertDescription::BadCertificate,
                ..
            } => self.fail(DisconnectReason::FingerprintMismatch),
            DtlsError::Closed => {}
            e => {
                if !self.is_connected() {
                    self.fail(DisconnectReason::DtlsFailed(e.to_string()));
                }
            }
        }
    }

    fn create_dtls(&mut self, now: Instant, client: bool) -> Result<(), DtlsError> {
        let mut c = if client {
            DtlsConfig::client(self.cert.clone())
        } else {
            DtlsConfig::server(self.cert.clone())
        };
        c.mtu = 1200;
        if let Some(fp) = &self.remote_fp
            && fp.hash_function == "sha-256"
            && let Ok(b) = <[u8; 32]>::try_from(&fp.bytes[..])
        {
            c.expected_peer_fingerprint = Some(b);
        }
        self.dtls = Some(DtlsEndpoint::new(c, now)?);
        Ok(())
    }

    fn maybe_start_dtls_client(&mut self, now: Instant) {
        if self.dtls.is_some() || self.dtls_client != Some(true) {
            return;
        }
        let ready = self
            .ice
            .as_ref()
            .is_some_and(|i| i.selected_pair().is_some());
        if !ready {
            return;
        }
        if let Err(e) = self.create_dtls(now, true) {
            self.fail(DisconnectReason::DtlsFailed(e.to_string()));
        }
    }

    fn verify_fingerprint(&self, d: &DtlsEndpoint) -> bool {
        let Some(fp) = &self.remote_fp else {
            return false;
        };
        match fp.hash_function.as_str() {
            "sha-256" => d.verify_peer_fingerprint(&fp.bytes),
            "sha-384" | "sha-512" => {
                let Some(cert) = d.peer_certificate() else {
                    return false;
                };
                let h: Vec<u8> = if fp.hash_function == "sha-384" {
                    sha2::Sha384::digest(cert).to_vec()
                } else {
                    sha2::Sha512::digest(cert).to_vec()
                };
                senca_dtls::fingerprint_matches(&h, &fp.bytes)
            }
            _ => false,
        }
    }

    fn on_dtls_connected(&mut self, now: Instant) {
        let Some(d) = self.dtls.as_ref() else { return };
        // SDP の fingerprint と照合してから鍵を作る（仕様 9.3）
        if !self.verify_fingerprint(d) {
            if let Some(d) = self.dtls.as_mut() {
                d.close();
            }
            self.fail(DisconnectReason::FingerprintMismatch);
            return;
        }
        let client = self.dtls_client == Some(true);
        let keys = d.export_srtp_keying_material().ok().and_then(|(p, km)| {
            let profile = SrtpProfile::from_dtls_id(p.id())?;
            dtls_srtp_contexts(profile, &km, client).ok()
        });
        let Some((tx, rx)) = keys else {
            self.fail(DisconnectReason::DtlsFailed("SRTP の鍵を作れない".into()));
            return;
        };
        self.srtp_tx = Some(tx);
        self.srtp_rx = Some(rx);
        if self.datachannel_negotiated {
            let mut sc = self.cfg.sctp.clone();
            if let Some(m) = self.remote_max_message_size {
                sc.max_send_message_size = m.min(usize::MAX as u64) as usize;
            } else {
                // RFC 8841 6: 属性が無ければ 64 KiB
                sc.max_send_message_size = 65536;
            }
            if let Ok(mut s) = DataChannels::new(sc, client, now) {
                if client {
                    s.connect(now);
                }
                for o in std::mem::take(&mut self.pending_channels) {
                    let _ = s.open(now, o);
                }
                self.sctp = Some(s);
            }
        }
        self.state = ConnState::Connected;
        self.next_rtcp = Some(now + Duration::from_millis(100));
        self.push_event(Event::Connected);
    }

    fn fail(&mut self, reason: DisconnectReason) {
        if self.state == ConnState::Closed {
            return;
        }
        self.state = ConnState::Closed;
        self.srtp_tx = None;
        self.srtp_rx = None;
        self.events.push_back(Event::Disconnected { reason });
    }

    fn push_event(&mut self, e: Event) {
        if self.events.len() >= self.cfg.max_events {
            // 溢れたら AU を捨てる（制御の出来事は残す）
            if matches!(e, Event::AccessUnit { .. }) {
                self.dropped.0 += 1;
                return;
            }
            if let Some(i) = self
                .events
                .iter()
                .position(|x| matches!(x, Event::AccessUnit { .. }))
            {
                self.events.remove(i);
                self.dropped.0 += 1;
            } else {
                self.dropped.0 += 1;
                return;
            }
        }
        self.events.push_back(e);
    }

    fn on_srtp(&mut self, now: Instant, data: &[u8]) {
        let Some(rx) = self.srtp_rx.as_mut() else {
            return; // 鍵が無い間の RTP は捨てる
        };
        let Ok(plain) = rx.unprotect_rtp(data) else {
            self.dropped.2 += 1;
            return;
        };
        self.on_rtp(now, Bytes::from(plain));
    }

    #[cfg(test)]
    pub(crate) fn debug_bwe(&self) -> String {
        format!(
            "desired {} pad_rate {} acked {:?} usage {:?} loss {:.3} pacer {}",
            self.desired_bps,
            self.padding_rate(self.last_now),
            self.bwe.acked_bitrate(),
            self.bwe.usage(),
            self.bwe.loss_fraction(),
            self.pacer.len()
        )
    }

    #[cfg(test)]
    pub(crate) fn debug_inbound(&self) -> Vec<String> {
        self.inbound
            .iter()
            .map(|s| {
                format!(
                    "{:?} ssrc {} rtx {:?} {}",
                    s.codec,
                    s.ssrc,
                    s.rtx_ssrc,
                    s.debug()
                )
            })
            .collect()
    }

    /// 試験用: 復号した後の RTP を入れる。
    #[cfg(test)]
    pub(crate) fn inject_plain_rtp(&mut self, now: Instant, data: &[u8]) {
        self.on_rtp(now, Bytes::copy_from_slice(data));
        self.pump(now);
    }

    /// 試験用: 復号した後の RTCP を入れる。
    #[cfg(test)]
    pub(crate) fn inject_plain_rtcp(&mut self, now: Instant, data: &[u8]) {
        self.on_rtcp(now, Bytes::copy_from_slice(data));
        self.pump(now);
    }

    fn on_rtp(&mut self, now: Instant, plain: Bytes) {
        let Ok(pkt) = RtpPacket::parse(plain) else {
            return;
        };
        let ext = ext::parse(&pkt, &self.ext_ids);
        if let Some(seq) = ext.transport_seq {
            self.twcc_rx.on_packet(seq, pkt.header.ssrc, now);
            if self.next_twcc.is_none() {
                self.next_twcc = Some(now + self.cfg.twcc_interval);
            }
        }
        let ssrc = pkt.header.ssrc;
        let (idx, is_rtx) = match self.ssrc_map.get(&ssrc) {
            Some(v) => *v,
            None => match self.identify(now, &pkt, &ext) {
                Some(v) => v,
                None => return,
            },
        };
        if idx >= self.inbound.len() {
            return;
        }
        let pkt = if is_rtx {
            let primary_pt = self
                .media
                .iter()
                .find(|m| m.mid == self.inbound[idx].mid)
                .and_then(|m| {
                    m.codecs
                        .iter()
                        .find(|c| c.rtx_payload_type == Some(pkt.header.payload_type))
                        .map(|c| c.payload_type)
                });
            let Some(pt) = primary_pt else { return };
            match senca_rtp::rtx::rtx_unwrap(&pkt, pt, self.inbound[idx].ssrc) {
                Ok(Some(p)) => p,
                _ => return, // padding（帯域の探り）・壊れたもの
            }
        } else {
            pkt
        };
        if pkt.payload.is_empty() {
            return; // padding だけ
        }
        // コーデックの切り替え
        if pkt.header.payload_type != self.inbound[idx].payload_type {
            let mid = self.inbound[idx].mid.clone();
            let codec = self
                .media
                .iter()
                .find(|m| m.mid == mid)
                .and_then(|m| {
                    m.codecs
                        .iter()
                        .find(|c| c.payload_type == pkt.header.payload_type)
                })
                .cloned();
            let Some(codec) = codec else { return };
            if self.inbound[idx].change_codec(&codec).is_err() {
                return;
            }
            let info = Self::info(&self.inbound[idx]);
            self.push_event(Event::Track(info));
        }
        if let Some(sample) = self.inbound[idx].push(now, pkt, is_rtx) {
            // NACK から再送までの時間は往復時間の上限の見本
            self.on_rtt_sample(sample);
        }
        self.drain_inbound(now, idx);
    }

    /// 知らない SSRC の packet がどの m 行・層か決める。
    fn identify(
        &mut self,
        now: Instant,
        pkt: &RtpPacket,
        ext: &ext::ParsedExt,
    ) -> Option<(usize, bool)> {
        let ssrc = pkt.header.ssrc;
        let pt = pkt.header.payload_type;
        // m 行
        let li = if let Some(mid) = &ext.mid {
            self.media.iter().position(|m| &m.mid == mid)?
        } else if let Some(i) = self.media.iter().position(|m| {
            m.remote_ssrcs.contains(&ssrc) || m.fid.iter().any(|f| f.0 == ssrc || f.1 == ssrc)
        }) {
            i
        } else {
            // payload type が 1 つの受信 m 行にだけあればそれ
            let cands: Vec<usize> = self
                .media
                .iter()
                .enumerate()
                .filter(|(_, m)| {
                    m.direction.recvs()
                        && m.codecs
                            .iter()
                            .any(|c| c.payload_type == pt || c.rtx_payload_type == Some(pt))
                })
                .map(|(i, _)| i)
                .collect();
            if cands.len() != 1 {
                return None;
            }
            cands[0]
        };
        let line = self.media[li].clone();
        if !line.direction.recvs() {
            return None;
        }
        let is_rtx = line.codecs.iter().any(|c| c.rtx_payload_type == Some(pt))
            || ext.repaired_rid.is_some()
            || line.fid.iter().any(|f| f.1 == ssrc);
        if is_rtx {
            // 元のストリームを探す
            let primary = if let Some(f) = line.fid.iter().find(|f| f.1 == ssrc) {
                self.inbound.iter().position(|s| s.ssrc == f.0)
            } else if let Some(r) = &ext.repaired_rid {
                self.inbound
                    .iter()
                    .position(|s| s.mid == line.mid && s.rid.as_deref() == Some(r))
            } else if line.rids.is_empty() {
                self.inbound.iter().position(|s| s.mid == line.mid)
            } else {
                None
            }?;
            self.ssrc_map.insert(ssrc, (primary, true));
            self.inbound[primary].rtx_ssrc = Some(ssrc);
            return Some((primary, true));
        }
        let codec = line.codecs.iter().find(|c| c.payload_type == pt)?.clone();
        codec_of(&codec)?;
        let rid = if line.rids.is_empty() {
            None
        } else {
            // simulcast: rid が要る（無い packet は SSRC が分かるまで捨てる）
            let r = ext.rid.clone()?;
            if !line.rids.contains(&r) {
                return None;
            }
            Some(r)
        };
        // 同じ m 行・同じ層のストリームが既にあれば SSRC が変わったとみなして置き換える
        if let Some(i) = self
            .inbound
            .iter()
            .position(|s| s.mid == line.mid && s.rid == rid)
        {
            let track = self.inbound[i].track;
            let old = self.inbound[i].ssrc;
            self.ssrc_map.retain(|_, v| v.0 != i);
            self.keyframes.remove(old);
            let s = InboundStream::new(&self.cfg, track, &line.mid, rid, ssrc, &codec, now).ok()?;
            self.inbound[i] = s;
            self.ssrc_map.insert(ssrc, (i, false));
            let info = Self::info(&self.inbound[i]);
            self.push_event(Event::Track(info));
            return Some((i, false));
        }
        if self.inbound.len() >= self.cfg.max_inbound_streams {
            return None;
        }
        let track = self.alloc_track();
        let s = InboundStream::new(&self.cfg, track, &line.mid, rid, ssrc, &codec, now).ok()?;
        self.inbound.push(s);
        let i = self.inbound.len() - 1;
        self.ssrc_map.insert(ssrc, (i, false));
        // 先に届いていた RTX の SSRC（FID）も結び付ける
        if let Some(f) = line.fid.iter().find(|f| f.0 == ssrc) {
            self.ssrc_map.insert(f.1, (i, true));
            self.inbound[i].rtx_ssrc = Some(f.1);
        }
        let info = Self::info(&self.inbound[i]);
        self.push_event(Event::Track(info));
        Some((i, false))
    }

    fn drain_inbound(&mut self, now: Instant, idx: usize) {
        let mut aus = Vec::new();
        self.inbound[idx].drain(now, &mut self.clock, &mut aus);
        let track = self.inbound[idx].track;
        for (au, config) in aus {
            self.push_event(Event::AccessUnit { track, au, config });
        }
    }

    fn on_srtcp(&mut self, now: Instant, data: &[u8]) {
        let Some(rx) = self.srtp_rx.as_mut() else {
            return;
        };
        let Ok(plain) = rx.unprotect_rtcp(data) else {
            self.dropped.2 += 1;
            return;
        };
        self.on_rtcp(now, Bytes::from(plain));
    }

    fn on_rtcp(&mut self, now: Instant, plain: Bytes) {
        let Ok(pkts) = parse_reduced(&plain) else {
            return;
        };
        for p in pkts {
            match p {
                RtcpPacket::SenderReport(sr) => {
                    if let Some(&(i, false)) = self.ssrc_map.get(&sr.ssrc) {
                        self.inbound[i].on_sender_report(&sr, now);
                    }
                    for b in &sr.reports {
                        self.on_report_block(now, b);
                    }
                }
                RtcpPacket::ReceiverReport(rr) => {
                    for b in &rr.reports {
                        self.on_report_block(now, b);
                    }
                }
                RtcpPacket::Nack(n) => self.on_nack(now, &n),
                RtcpPacket::Pli(p) => self.on_keyframe_request(now, p.media_ssrc),
                RtcpPacket::Fir(f) => {
                    for (ssrc, _) in f.entries.iter().take(8) {
                        self.on_keyframe_request(now, *ssrc);
                    }
                }
                RtcpPacket::Remb(r) => {
                    self.bwe.on_remb(now, r.bitrate);
                    self.report_bwe(now, false);
                }
                RtcpPacket::TransportCc(fb) => {
                    let res = self.send_history.on_feedback(&fb);
                    self.bwe.on_feedback(now, &res);
                    self.report_bwe(now, false);
                }
                _ => {}
            }
        }
    }

    fn now_ntp(&self, now: Instant) -> u64 {
        let base = self.cfg.unix_time_ns_at_start.unwrap_or(0);
        let el = now.saturating_duration_since(self.start).as_nanos() as i64;
        senca_rtp::clock::unix_ns_to_ntp(base + el)
    }

    fn on_report_block(&mut self, now: Instant, b: &senca_rtp::rtcp::ReportBlock) {
        if !self.outbound.iter().any(|o| o.ssrc == b.ssrc) {
            return;
        }
        if let Some(rtt) = senca_rtp::stats::round_trip_time(b, self.now_ntp(now))
            && rtt < Duration::from_secs(10)
        {
            self.on_rtt_sample(rtt);
        }
        self.bwe.on_receiver_report(now, b.fraction_lost);
    }

    fn on_rtt_sample(&mut self, s: Duration) {
        let s = s.clamp(Duration::from_millis(1), Duration::from_secs(5));
        self.rtt = if self.rtt_known {
            (self.rtt * 7 + s) / 8
        } else {
            s
        };
        self.rtt_known = true;
        self.bwe.set_rtt(self.rtt);
    }

    fn on_nack(&mut self, now: Instant, n: &Nack) {
        let Some(oi) = self.outbound.iter().position(|o| o.ssrc == n.media_ssrc) else {
            return;
        };
        let seqs = n.sequences();
        for seq in seqs.into_iter().take(256) {
            if let Some(p) = self.outbound[oi].retransmit(seq, now, self.rtt) {
                let size = p.encoded_len();
                let dropped = self.pacer.push(
                    now,
                    PacketClass::Retransmission,
                    Queued {
                        out: oi,
                        pkt: p,
                        rtx: true,
                    },
                    size,
                );
                self.on_pacer_dropped(dropped);
            }
        }
        self.flush_pacer(now);
    }

    fn on_keyframe_request(&mut self, now: Instant, ssrc: u32) {
        let Some(o) = self.outbound.iter().find(|o| o.ssrc == ssrc) else {
            return;
        };
        let track = o.track;
        self.emit_keyframe_request(now, track);
    }

    fn emit_keyframe_request(&mut self, now: Instant, track: TrackId) {
        // 同じトラックへの要求は 100 ms に 1 回まで（連打を防ぐ、仕様 9.4）
        if self
            .kf_events
            .get(&track)
            .is_some_and(|t| now.saturating_duration_since(*t) < Duration::from_millis(100))
        {
            return;
        }
        self.kf_events.insert(track, now);
        self.push_event(Event::KeyframeRequest { track });
    }

    fn report_bwe(&mut self, now: Instant, force: bool) {
        let bps = self.bwe.estimate(now);
        self.pacer.set_estimate(bps);
        let changed = self.last_bwe.is_none_or(|(b, t)| {
            let diff = (b as f64 - bps as f64).abs() / (b.max(1) as f64);
            diff > 0.05 || now.saturating_duration_since(t) >= Duration::from_secs(1)
        });
        if force || changed {
            self.last_bwe = Some((bps, now));
            self.push_event(Event::BandwidthEstimate { bps });
        }
    }

    // ------------------------------------------------------------------ 送信

    /// 送信トラックへ AU を送る。`config` は H.264 のパラメーターセットなど（キーフレームの前に入れる）。
    ///
    /// 映像は始まりと入力の切り替え（`au.track`・`au.epoch` の変化）をキーフレームからにする。
    /// キーフレームでない AU は捨て、[`Event::KeyframeRequest`] を出す。
    pub fn send_access_unit(
        &mut self,
        now: Instant,
        track: TrackId,
        au: &AccessUnit,
        config: Option<&CodecConfig>,
    ) -> Result<(), WebRtcError> {
        self.last_now = now;
        if self.state != ConnState::Connected {
            return Err(WebRtcError::NotConnected);
        }
        let oi = self
            .outbound
            .iter()
            .position(|o| o.track == track)
            .ok_or(WebRtcError::UnknownTrack)?;
        let mtu = self.cfg.mtu.saturating_sub(40).max(200);
        let p = self.outbound[oi].packetize(now, au, config, mtu)?;
        if p.keyframe_needed {
            self.emit_keyframe_request(now, track);
        }
        let class = if self.outbound[oi].kind == MediaKind::Audio {
            PacketClass::Audio
        } else {
            PacketClass::Video
        };
        for pkt in p.packets {
            let size = pkt.encoded_len();
            let dropped = self.pacer.push(
                now,
                class,
                Queued {
                    out: oi,
                    pkt,
                    rtx: false,
                },
                size,
            );
            self.on_pacer_dropped(dropped);
        }
        self.flush_pacer(now);
        Ok(())
    }

    /// 送りたい速さ（bit/s）を知らせる。今の送信がこれと推定帯域より少なければ、差を padding（RTX の
    /// padding だけの packet）で埋めて帯域を探る。simulcast の上の層へ上げられるかを確かめるのに使う
    /// （[`RoomOutput::DesiredBitrate`](crate::RoomOutput::DesiredBitrate)）。0 で止める。
    pub fn set_desired_bitrate(&mut self, bps: u64) {
        self.desired_bps = bps;
        if bps == 0 {
            self.padding_budget = 0.0;
            self.padding_last = None;
        }
    }

    /// 直近 500 ms に送ったメディア（再送を含み padding を除く）の速さ（bit/s）。
    pub fn send_bitrate(&self) -> u64 {
        (self.sent_window_bytes as u64 * 8 * 2).min(u64::MAX / 2)
    }

    fn padding_rate(&self, now: Instant) -> f64 {
        if self.desired_bps == 0 || self.ext_ids.transport_cc.is_none() {
            return 0.0;
        }
        let mut target = self.desired_bps.min(self.bwe.estimate(now)) as f64;
        if let Some((_, rate, until)) = self.probe
            && now < until
        {
            target = target.max(rate);
        }
        (target - self.send_bitrate() as f64).max(0.0)
    }

    /// 送りたい速さが推定を上回るとき、ときどき（大きく下げた直後は早めに）短い探りの塊を送る。
    fn maybe_probe(&mut self, now: Instant) {
        let est = self.bwe.estimate(now);
        if self.probe.is_some_and(|(_, _, until)| now >= until) {
            self.probe = None;
        }
        // 推定が大きく下がったら 1 秒後に探る
        if self.last_estimate > 0 && (est as f64) < 0.7 * self.last_estimate as f64 {
            self.next_probe = Some(now + Duration::from_secs(1));
        }
        self.last_estimate = est;
        if self.desired_bps == 0
            || (self.desired_bps as f64) < est as f64 * 1.1
            || self.ext_ids.transport_cc.is_none()
        {
            if self.next_probe.is_some_and(|t| t <= now) {
                self.next_probe = None;
            }
            return;
        }
        if self.probe.is_some() {
            return;
        }
        let at = *self.next_probe.get_or_insert(now + Duration::from_secs(1));
        if now < at {
            return;
        }
        let rate = (self.desired_bps as f64)
            .min(est as f64 * 2.0)
            .max(est as f64 * 1.2);
        let id = self.bwe.start_probe(now);
        self.probe = Some((id, rate, now + Duration::from_millis(60)));
        self.next_probe = Some(now + Duration::from_secs(5));
    }

    fn send_padding(&mut self, now: Instant) {
        let rate = self.padding_rate(now);
        if rate <= 0.0 {
            self.padding_last = None;
            self.padding_budget = 0.0;
            return;
        }
        let last = *self.padding_last.get_or_insert(now);
        let dt = now.saturating_duration_since(last).as_secs_f64().min(0.1);
        self.padding_last = Some(now);
        self.padding_budget = (self.padding_budget + dt * rate / 8.0).min(rate / 8.0 * 0.05);
        let Some(oi) = self
            .outbound
            .iter()
            .position(|o| o.active && o.kind == MediaKind::Video && o.rtx_ssrc.is_some())
        else {
            return;
        };
        let mut n = 0;
        while self.padding_budget >= 280.0 && n < 32 {
            let Some(p) = self.outbound[oi].padding_packet(240) else {
                break;
            };
            self.padding_budget -= 280.0;
            n += 1;
            self.send_rtp_probe(
                now,
                Queued {
                    out: oi,
                    pkt: p,
                    rtx: true,
                },
                self.probe.map(|p| p.0),
            );
        }
    }

    fn padding_timeout(&self) -> Option<Instant> {
        if self.padding_rate(self.last_now) <= 0.0 {
            return None;
        }
        Some(
            self.padding_last
                .map_or(self.last_now, |t| t + Duration::from_millis(10)),
        )
    }

    /// 受信トラックの送り手にキーフレームを求める（PLI、交渉が FIR だけなら FIR。間隔は制限する）。
    pub fn request_keyframe(&mut self, now: Instant, track: TrackId) -> Result<(), WebRtcError> {
        let mut found = false;
        for s in self.inbound.iter_mut().filter(|s| s.track == track) {
            s.keyframe_needed = true;
            found = true;
        }
        if !found {
            return Err(WebRtcError::UnknownTrack);
        }
        self.send_rtcp_feedback(now);
        Ok(())
    }

    fn on_pacer_dropped(&mut self, dropped: Vec<Queued>) {
        let mut tracks = Vec::new();
        for d in dropped {
            if !tracks.contains(&d.out) {
                tracks.push(d.out);
            }
        }
        for oi in tracks {
            self.outbound[oi].require_keyframe();
            let t = self.outbound[oi].track;
            let now = self.last_now;
            self.emit_keyframe_request(now, t);
        }
    }

    fn flush_pacer(&mut self, now: Instant) {
        let expired = self.pacer.expire(now);
        self.on_pacer_dropped(expired);
        while let Some(q) = self.pacer.pop(now) {
            self.send_rtp(now, q);
        }
    }

    fn send_rtp(&mut self, now: Instant, q: Queued) {
        self.send_rtp_probe(now, q, None);
    }

    fn send_rtp_probe(&mut self, now: Instant, q: Queued, probe: Option<u32>) {
        let Some(o) = self.outbound.get(q.out) else {
            return;
        };
        let mut pkt = q.pkt;
        let video = o.kind == MediaKind::Video;
        let est = pkt.encoded_len() + 24;
        let tseq = self
            .ext_ids
            .transport_cc
            .map(|_| self.send_history.on_sent_probe(now, est, probe));
        let mid = o.mid.clone();
        let rid = o.rid.clone();
        ext::apply(
            &mut pkt,
            &self.ext_ids,
            &OutExt {
                mid: Some(&mid),
                rid: if q.rtx { None } else { rid.as_deref() },
                repaired_rid: if q.rtx { rid.as_deref() } else { None },
                abs_send_time: Some(ext::abs_send_time(
                    now.saturating_duration_since(self.start),
                )),
                transport_seq: tseq,
                playout_delay: if video {
                    self.cfg
                        .playout_delay
                        .map(|(a, b)| ext::playout_delay(a, b))
                } else {
                    None
                },
            },
        );
        let Ok(bytes) = pkt.to_bytes() else { return };
        let Some(tx) = self.srtp_tx.as_mut() else {
            return;
        };
        let Ok(prot) = tx.protect_rtp(&bytes) else {
            return;
        };
        if !pkt.payload.is_empty() {
            // メディア（再送を含む）だけを数える。padding は数えない
            self.sent_window.push_back((now, prot.len()));
            self.sent_window_bytes += prot.len();
        }
        while let Some(&(t, n)) = self.sent_window.front() {
            if now.saturating_duration_since(t) > Duration::from_millis(500)
                || self.sent_window.len() > 8192
            {
                self.sent_window.pop_front();
                self.sent_window_bytes -= n;
            } else {
                break;
            }
        }
        self.send_raw(Bytes::from(prot));
    }

    fn send_raw(&mut self, data: Bytes) {
        let Some(ice) = self.ice.as_mut() else { return };
        if let Some(t) = ice.send(data) {
            if self.transmits.len() >= self.cfg.max_transmits {
                self.dropped.1 += 1;
                return;
            }
            self.transmits.push_back(t);
        }
    }

    fn send_rtcp(&mut self, pkts: &[RtcpPacket], rsize: bool) {
        if pkts.is_empty() {
            return;
        }
        let mut list: Vec<RtcpPacket> = Vec::with_capacity(pkts.len() + 1);
        if !rsize
            && !matches!(
                pkts[0],
                RtcpPacket::SenderReport(_) | RtcpPacket::ReceiverReport(_)
            )
        {
            list.push(RtcpPacket::ReceiverReport(ReceiverReport {
                ssrc: self.rtcp_ssrc,
                reports: Vec::new(),
                extension: Bytes::new(),
            }));
        }
        list.extend_from_slice(pkts);
        let Ok(b) = build_compound(&list) else { return };
        let Some(tx) = self.srtp_tx.as_mut() else {
            return;
        };
        let Ok(prot) = tx.protect_rtcp(&b) else {
            return;
        };
        self.send_raw(Bytes::from(prot));
    }

    fn rsize(&self) -> bool {
        self.media.iter().all(|m| m.rtcp_rsize)
    }

    /// 定期の RTCP（SR・RR・SDES）。
    fn send_rtcp_reports(&mut self, now: Instant) {
        let ntp = self.now_ntp(now);
        let mut blocks: Vec<_> = self
            .inbound
            .iter_mut()
            .filter(|s| now.saturating_duration_since(s.last_packet) < Duration::from_secs(5))
            .map(|s| s.report_block(now))
            .collect();
        blocks.truncate(31);
        let mut pkts = Vec::new();
        let mut chunks = vec![SdesChunk::cname(self.rtcp_ssrc, &self.cname)];
        for o in &self.outbound {
            if let Some(mut sr) = o.sender_report(now, ntp) {
                if pkts.is_empty() {
                    sr.reports = std::mem::take(&mut blocks);
                }
                pkts.push(RtcpPacket::SenderReport(sr));
                chunks.push(SdesChunk::cname(o.ssrc, &self.cname));
            }
        }
        if pkts.is_empty() {
            pkts.push(RtcpPacket::ReceiverReport(ReceiverReport {
                ssrc: self.rtcp_ssrc,
                reports: blocks,
                extension: Bytes::new(),
            }));
        }
        chunks.truncate(31);
        pkts.push(RtcpPacket::Sdes(Sdes { chunks }));
        self.send_rtcp(&pkts, false);
    }

    /// NACK・PLI/FIR。
    fn send_rtcp_feedback(&mut self, now: Instant) {
        let rsize = self.rsize();
        let rtt = self.rtt;
        let mut fb = Vec::new();
        for i in 0..self.inbound.len() {
            let s = &mut self.inbound[i];
            if let Some(n) = s.nack.as_mut() {
                let seqs = n.poll(now, rtt);
                if !seqs.is_empty() {
                    fb.push(RtcpPacket::Nack(Nack::from_sequences(
                        self.rtcp_ssrc,
                        s.ssrc,
                        &seqs,
                    )));
                }
            }
            if s.keyframe_needed && s.can_request_keyframe {
                s.keyframe_needed = false;
                let ssrc = s.ssrc;
                if let Some(p) = self.keyframes.request(ssrc, now) {
                    fb.push(p);
                }
            } else {
                s.keyframe_needed = false;
            }
        }
        fb.extend(self.keyframes.poll(now));
        // FIR しか交渉していない送り手には FIR を送る
        for p in fb.iter_mut() {
            if let RtcpPacket::Pli(pli) = p
                && self
                    .inbound
                    .iter()
                    .any(|s| s.ssrc == pli.media_ssrc && s.use_fir)
            {
                *p = RtcpPacket::Fir(senca_rtp::rtcp::Fir {
                    sender_ssrc: self.rtcp_ssrc,
                    entries: vec![(
                        pli.media_ssrc,
                        (now.saturating_duration_since(self.start).as_millis() / 100) as u8,
                    )],
                });
            }
        }
        for chunk in fb.chunks(16) {
            self.send_rtcp(chunk, rsize);
        }
    }

    fn send_twcc(&mut self, now: Instant) {
        let fbs = self.twcc_rx.build_feedback(self.rtcp_ssrc);
        let rsize = self.rsize();
        for f in fbs {
            self.send_rtcp(&[RtcpPacket::TransportCc(f)], rsize);
        }
        self.next_twcc = if self.twcc_rx.has_pending() {
            Some(now + self.cfg.twcc_interval)
        } else {
            None
        };
    }

    // ------------------------------------------------------------------ 出口と時間

    /// 送る datagram を 1 つ取り出す。
    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmits.pop_front()
    }

    /// 出来事を 1 つ取り出す。
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// 次に [`handle_timeout`](Self::handle_timeout) を呼ぶ時刻。
    pub fn poll_timeout(&self) -> Option<Instant> {
        let mut t: Option<Instant> = None;
        let mut upd = |x: Option<Instant>| {
            if let Some(x) = x {
                t = Some(t.map_or(x, |t| t.min(x)));
            }
        };
        if self.state == ConnState::Closed {
            upd(self.ice.as_ref().and_then(|i| i.poll_timeout()));
            return t;
        }
        upd(self.ice.as_ref().and_then(|i| i.poll_timeout()));
        upd(self.dtls.as_ref().and_then(|d| d.poll_timeout()));
        upd(self.sctp.as_ref().and_then(|s| s.poll_timeout()));
        if self.state == ConnState::Connected {
            upd(self.pacer.poll_timeout());
            upd(self.next_rtcp);
            upd(self.next_twcc);
            upd(self.keyframes.poll_timeout());
            upd(self.padding_timeout());
            if self.desired_bps > 0 {
                upd(self.next_probe);
                upd(self.probe.map(|p| p.2));
            }
            for s in &self.inbound {
                upd(s.poll_timeout());
                upd(s.nack_timeout(self.rtt));
            }
        }
        t
    }

    /// 時刻を進める。
    pub fn handle_timeout(&mut self, now: Instant) {
        self.last_now = now;
        if let Some(i) = self.ice.as_mut() {
            i.handle_timeout(now);
        }
        if self.state == ConnState::Closed {
            self.pump(now);
            return;
        }
        if let Some(d) = self.dtls.as_mut()
            && let Err(e) = d.handle_timeout(now)
        {
            self.on_dtls_error(e);
        }
        if let Some(s) = self.sctp.as_mut() {
            s.handle_timeout(now);
        }
        if self.state == ConnState::Connected {
            for i in 0..self.inbound.len() {
                if self.inbound[i].poll_timeout().is_some_and(|t| t <= now) {
                    self.drain_inbound(now, i);
                }
            }
            self.send_rtcp_feedback(now);
            if self.next_twcc.is_some_and(|t| t <= now) {
                self.send_twcc(now);
            }
            if self.next_rtcp.is_some_and(|t| t <= now) {
                self.send_rtcp_reports(now);
                self.next_rtcp = Some(now + self.cfg.rtcp_interval);
                self.send_history.expire(now, Duration::from_secs(10));
            }
            self.flush_pacer(now);
            self.maybe_probe(now);
            if self.pacer.is_empty() {
                self.send_padding(now);
            } else if self.padding_last.is_some() {
                // メディアが待っている間は padding を出さない（予算も貯めない）
                self.padding_last = Some(now);
            }
        }
        self.pump(now);
    }

    /// 閉じる（DTLS の close_notify、SCTP の ABORT、ICE を閉じる）。
    pub fn close(&mut self, now: Instant) {
        if self.state == ConnState::Closed {
            return;
        }
        if let Some(s) = self.sctp.as_mut() {
            s.shutdown(now);
        }
        self.pump(now);
        if let Some(d) = self.dtls.as_mut() {
            d.close();
        }
        self.pump(now);
        if let Some(i) = self.ice.as_mut() {
            i.close(now);
        }
        self.pump(now);
        self.fail(DisconnectReason::Closed);
    }

    /// ICE・DTLS・SCTP の出力と出来事を流す。
    fn pump(&mut self, now: Instant) {
        for _ in 0..64 {
            let mut progressed = false;
            // ICE
            if let Some(ice) = self.ice.as_mut() {
                while let Some(t) = ice.poll_transmit() {
                    progressed = true;
                    if self.transmits.len() < self.cfg.max_transmits {
                        self.transmits.push_back(t);
                    } else {
                        self.dropped.1 += 1;
                    }
                }
                let mut evs = Vec::new();
                while let Some(e) = ice.poll_event() {
                    evs.push(e);
                }
                for e in evs {
                    progressed = true;
                    self.on_ice_event(now, e);
                }
            }
            // SCTP → DTLS
            if let (Some(s), Some(d)) = (self.sctp.as_mut(), self.dtls.as_mut()) {
                while let Some(p) = s.poll_transmit() {
                    progressed = true;
                    let _ = d.send(&p);
                }
            }
            // DTLS
            if let Some(d) = self.dtls.as_mut() {
                let mut out = Vec::new();
                while let Some(p) = d.poll_transmit() {
                    out.push(Bytes::from(p));
                }
                let mut evs = Vec::new();
                while let Some(e) = d.poll_event() {
                    evs.push(e);
                }
                for p in out {
                    progressed = true;
                    if self.selected_path().is_some() {
                        self.send_raw(p);
                    } else if self.pending_dtls.len() < 64 {
                        self.pending_dtls.push_back(p);
                    }
                }
                for e in evs {
                    progressed = true;
                    match e {
                        DtlsEvent::Connected => self.on_dtls_connected(now),
                        DtlsEvent::ApplicationData(b) => {
                            if let Some(s) = self.sctp.as_mut() {
                                let _ = s.handle_packet(now, &b);
                            }
                        }
                        DtlsEvent::Closed => {
                            if self.state != ConnState::Closed {
                                self.fail(DisconnectReason::DtlsClosed);
                            }
                        }
                    }
                }
            }
            if !self.pending_dtls.is_empty() && self.selected_path().is_some() {
                progressed = true;
                while let Some(p) = self.pending_dtls.pop_front() {
                    self.send_raw(p);
                }
            }
            // SCTP の出来事
            if let Some(s) = self.sctp.as_mut() {
                let mut evs = Vec::new();
                while let Some(e) = s.poll_event() {
                    evs.push(e);
                }
                for e in evs {
                    progressed = true;
                    match e {
                        DataChannelEvent::Open(info) => {
                            self.push_event(Event::DataChannelOpen(info))
                        }
                        DataChannelEvent::Message { id, data, binary } => {
                            self.push_event(Event::DataChannelMessage { id, data, binary })
                        }
                        DataChannelEvent::Closed { id } => {
                            self.push_event(Event::DataChannelClosed { id })
                        }
                        DataChannelEvent::Connected
                        | DataChannelEvent::BufferedAmountLow { .. }
                        | DataChannelEvent::Disconnected { .. } => {}
                    }
                }
            }
            if !progressed {
                break;
            }
        }
    }

    fn on_ice_event(&mut self, now: Instant, e: IceEvent) {
        match e {
            IceEvent::LocalCandidate(c) => {
                if !self.cfg.ice_lite {
                    self.push_event(Event::LocalCandidate(c.to_sdp_attribute()));
                }
            }
            IceEvent::GatheringComplete => {
                if !self.cfg.ice_lite {
                    self.push_event(Event::GatheringComplete);
                }
            }
            IceEvent::StateChanged(s) => {
                self.push_event(Event::IceStateChanged(s));
                match s {
                    IceState::Connected | IceState::Completed => self.maybe_start_dtls_client(now),
                    IceState::Failed => self.fail(DisconnectReason::IceFailed),
                    _ => {}
                }
            }
            IceEvent::SelectedPair { .. } => self.maybe_start_dtls_client(now),
            IceEvent::RoleChanged(_) => {}
        }
    }
}
