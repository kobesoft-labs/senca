//! サーバー端点向けの JSEP-lite（RFC 9429 の一部、RFC 3264 の offer/answer）。
//!
//! - [`create_answer`]: 相手の offer と自分の能力（[`Capabilities`]）から answer を作る（WHIP・ブラウザーの offer）。
//! - [`create_offer`]: 自分から offer を作る（WHEP のサーバー送信 offer、RTSP の DESCRIBE 応答）。
//! - [`apply_answer`]: 自分の offer への answer を検査し、交渉結果を取り出す。
//!
//! コーデックは相手の payload type 番号をそのまま使う。fmtp の照合は H.264（packetization-mode と
//! profile-level-id、RFC 6184 8.2.2）・VP9（profile-id）・AV1（profile）・Opus（48000/2）・G.711 を見る。

use crate::attr::{
    Attribute, Candidate, Direction, Extmap, Fingerprint, Msid, Rid, RidDirection, RtcpFb, RtpMap,
    Setup, Simulcast, SsrcAttr, SsrcGroup, get_param, is_token, params_to_string, validate_ice_pwd,
    validate_ice_ufrag,
};
use crate::error::{Error, Result};
use crate::session::{Connection, MediaDescription, Origin, SessionDescription};
use std::collections::HashSet;

/// media の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Audio,
    Video,
    Application,
    /// それ以外（`text` など。交渉では拒否する）。
    Other,
}

impl MediaKind {
    pub fn from_media(s: &str) -> MediaKind {
        match s {
            "audio" => MediaKind::Audio,
            "video" => MediaKind::Video,
            "application" => MediaKind::Application,
            _ => MediaKind::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Audio => "audio",
            MediaKind::Video => "video",
            MediaKind::Application => "application",
            MediaKind::Other => "other",
        }
    }
}

/// 自分が扱えるコーデック 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecCap {
    pub kind: MediaKind,
    /// rtpmap の符号化名（`H264`・`VP8`・`VP9`・`AV1`・`opus`・`PCMA`・`PCMU`）。比較は大文字小文字を区別しない。
    pub encoding: String,
    pub clock_rate: u32,
    pub channels: Option<u16>,
    /// 自分の fmtp（answer・offer に書く値）。
    pub fmtp: Vec<(String, String)>,
    /// 受け付ける RTCP feedback（`nack`・`nack pli`・`ccm fir`・`goog-remb`・`transport-cc`）。
    pub rtcp_fb: Vec<String>,
    /// 自分で offer を作るときの payload type。
    pub payload_type: u8,
}

fn kv(k: &str, v: &str) -> (String, String) {
    (k.to_string(), v.to_string())
}

const VIDEO_FB: [&str; 5] = ["goog-remb", "transport-cc", "ccm fir", "nack", "nack pli"];

impl CodecCap {
    fn video(pt: u8, enc: &str, fmtp: Vec<(String, String)>) -> CodecCap {
        CodecCap {
            kind: MediaKind::Video,
            encoding: enc.into(),
            clock_rate: 90000,
            channels: None,
            fmtp,
            rtcp_fb: VIDEO_FB.iter().map(|s| s.to_string()).collect(),
            payload_type: pt,
        }
    }

    /// H.264。`profile_level_id` は 6 桁の 16 進（例: `42e01f`）。
    pub fn h264(pt: u8, profile_level_id: &str, packetization_mode: u8) -> CodecCap {
        Self::video(
            pt,
            "H264",
            vec![
                kv("level-asymmetry-allowed", "1"),
                kv("packetization-mode", &packetization_mode.to_string()),
                kv("profile-level-id", profile_level_id),
            ],
        )
    }

    pub fn vp8(pt: u8) -> CodecCap {
        Self::video(pt, "VP8", Vec::new())
    }

    pub fn vp9(pt: u8, profile_id: u8) -> CodecCap {
        Self::video(pt, "VP9", vec![kv("profile-id", &profile_id.to_string())])
    }

    pub fn av1(pt: u8) -> CodecCap {
        Self::video(pt, "AV1", Vec::new())
    }

    pub fn opus(pt: u8) -> CodecCap {
        CodecCap {
            kind: MediaKind::Audio,
            encoding: "opus".into(),
            clock_rate: 48000,
            channels: Some(2),
            fmtp: vec![kv("minptime", "10"), kv("useinbandfec", "1")],
            rtcp_fb: vec!["transport-cc".into()],
            payload_type: pt,
        }
    }

    fn g711(pt: u8, enc: &str) -> CodecCap {
        CodecCap {
            kind: MediaKind::Audio,
            encoding: enc.into(),
            clock_rate: 8000,
            channels: None,
            fmtp: Vec::new(),
            rtcp_fb: Vec::new(),
            payload_type: pt,
        }
    }

    pub fn pcmu() -> CodecCap {
        Self::g711(0, "PCMU")
    }

    pub fn pcma() -> CodecCap {
        Self::g711(8, "PCMA")
    }
}

/// よく使う RTP ヘッダー拡張の URI。
pub mod ext {
    pub const MID: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";
    pub const RTP_STREAM_ID: &str = "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id";
    pub const REPAIRED_RTP_STREAM_ID: &str =
        "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id";
    pub const ABS_SEND_TIME: &str = "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time";
    pub const TRANSPORT_CC: &str =
        "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";
    pub const AUDIO_LEVEL: &str = "urn:ietf:params:rtp-hdrext:ssrc-audio-level";
    pub const TOFFSET: &str = "urn:ietf:params:rtp-hdrext:toffset";
}

/// 自分の能力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// 優先順（offer を作るときの並び）。answer では相手の並びを保つ。
    pub codecs: Vec<CodecCap>,
    /// 受け付けるヘッダー拡張の URI。
    pub header_extensions: Vec<String>,
    /// RTX（RFC 4588）を受け付ける。
    pub rtx: bool,
}

impl Capabilities {
    /// サーバーの既定: H.264（Constrained Baseline・High、mode 1）・VP8・VP9 profile 0・AV1・Opus・PCMU・PCMA。
    pub fn default_server() -> Capabilities {
        Capabilities {
            codecs: vec![
                CodecCap::h264(102, "42e01f", 1),
                CodecCap::h264(104, "640c1f", 1),
                CodecCap::h264(106, "64001f", 1),
                CodecCap::vp8(96),
                CodecCap::vp9(98, 0),
                CodecCap::av1(45),
                CodecCap::opus(111),
                CodecCap::pcmu(),
                CodecCap::pcma(),
            ],
            header_extensions: [
                ext::MID,
                ext::RTP_STREAM_ID,
                ext::REPAIRED_RTP_STREAM_ID,
                ext::ABS_SEND_TIME,
                ext::TRANSPORT_CC,
                ext::AUDIO_LEVEL,
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            rtx: true,
        }
    }
}

/// ICE の資格情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceCredentials {
    pub ufrag: String,
    pub pwd: String,
}

/// 自分が送るストリームの情報（answer・offer に `a=msid`・`a=ssrc` を書く）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendSource {
    /// 当てはめる m 行の mid（`None` なら同じ種類の最初の未使用の m 行）。
    pub mid: Option<String>,
    pub kind: MediaKind,
    pub ssrc: u32,
    pub rtx_ssrc: Option<u32>,
    pub cname: String,
    pub msid: Option<Msid>,
}

/// offer に rtcp-mux がないときの扱い（RFC 9429 の rtcpMuxPolicy）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtcpMuxPolicy {
    /// rtcp-mux のない offer を拒否する（`Err`）。
    Require,
    /// rtcp-mux なしでも受け付ける。
    Negotiate,
}

/// answer を作る設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerOptions {
    pub ice: Option<IceCredentials>,
    pub ice_lite: bool,
    pub ice_options: Vec<String>,
    pub fingerprint: Option<Fingerprint>,
    /// 相手が actpass のときに選ぶ役割（既定 passive。actpass を渡すと active にする）。
    pub setup: Setup,
    pub candidates: Vec<Candidate>,
    pub end_of_candidates: bool,
    pub session_id: u64,
    pub session_version: u64,
    pub origin_address: String,
    /// 自分が許す方向（音声・映像）。
    pub audio_direction: Direction,
    pub video_direction: Direction,
    /// mid ごとの方向の上書き。
    pub direction_overrides: Vec<(String, Direction)>,
    pub accept_datachannel: bool,
    pub sctp_port: u16,
    pub max_message_size: u64,
    pub rtcp_mux: RtcpMuxPolicy,
    /// 相手の simulcast（send）を受ける。
    pub accept_simulcast: bool,
    pub send_sources: Vec<SendSource>,
}

impl Default for AnswerOptions {
    fn default() -> Self {
        AnswerOptions {
            ice: None,
            ice_lite: false,
            ice_options: Vec::new(),
            fingerprint: None,
            setup: Setup::Passive,
            candidates: Vec::new(),
            end_of_candidates: false,
            session_id: 0,
            session_version: 0,
            origin_address: "127.0.0.1".into(),
            audio_direction: Direction::SendRecv,
            video_direction: Direction::SendRecv,
            direction_overrides: Vec::new(),
            accept_datachannel: false,
            sctp_port: 5000,
            max_message_size: 262144,
            rtcp_mux: RtcpMuxPolicy::Require,
            accept_simulcast: true,
            send_sources: Vec::new(),
        }
    }
}

/// 交渉できたコーデック。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedCodec {
    pub payload_type: u8,
    pub encoding: String,
    pub clock_rate: u32,
    pub channels: Option<u16>,
    /// 自分の fmtp（自分が書いた側）。
    pub fmtp: Vec<(String, String)>,
    /// 相手の fmtp（sprop-parameter-sets など、受信に使う値）。
    pub remote_fmtp: Vec<(String, String)>,
    /// 両者が合意した feedback（`nack pli` など）。
    pub rtcp_fb: Vec<String>,
    pub rtx_payload_type: Option<u8>,
}

impl NegotiatedCodec {
    pub fn fmtp_get(&self, key: &str) -> Option<&str> {
        get_param(&self.fmtp, key)
    }

    pub fn remote_fmtp_get(&self, key: &str) -> Option<&str> {
        get_param(&self.remote_fmtp, key)
    }

    pub fn has_feedback(&self, fb: &str) -> bool {
        let fb = norm_fb(fb);
        self.rtcp_fb.iter().any(|f| norm_fb(f) == fb)
    }
}

/// m 行 1 つの交渉結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedMedia {
    /// SDP 中の m 行の位置。
    pub index: usize,
    pub mid: Option<String>,
    pub kind: MediaKind,
    pub protocol: String,
    /// 自分から見た方向。
    pub direction: Direction,
    /// 優先順（相手の offer の並び、または answer の並び）。
    pub codecs: Vec<NegotiatedCodec>,
    /// 合意したヘッダー拡張（id, URI）。
    pub extmaps: Vec<(u8, String)>,
    pub rejected: bool,
    pub rtcp_mux: bool,
    pub rtcp_rsize: bool,
    /// 相手の SSRC 情報。
    pub remote_ssrcs: Vec<SsrcAttr>,
    pub remote_ssrc_groups: Vec<SsrcGroup>,
    /// 相手が送る rid（simulcast）。
    pub rids: Vec<Rid>,
    /// 相手の simulcast 記述（相手から見た send/recv）。
    pub simulcast: Option<Simulcast>,
    /// DataChannel の相手の sctp-port・max-message-size。
    pub remote_sctp_port: Option<u16>,
    pub remote_max_message_size: Option<u64>,
}

impl NegotiatedMedia {
    fn rejected(index: usize, m: &MediaDescription) -> NegotiatedMedia {
        NegotiatedMedia {
            index,
            mid: m.mid().map(str::to_string),
            kind: MediaKind::from_media(&m.media),
            protocol: m.protocol.clone(),
            direction: Direction::Inactive,
            codecs: Vec::new(),
            extmaps: Vec::new(),
            rejected: true,
            rtcp_mux: false,
            rtcp_rsize: false,
            remote_ssrcs: Vec::new(),
            remote_ssrc_groups: Vec::new(),
            rids: Vec::new(),
            simulcast: None,
            remote_sctp_port: None,
            remote_max_message_size: None,
        }
    }

    /// RTX 以外の最初のコーデック（送るときに使う）。
    pub fn primary_codec(&self) -> Option<&NegotiatedCodec> {
        self.codecs.first()
    }

    pub fn extmap_id(&self, uri: &str) -> Option<u8> {
        self.extmaps.iter().find(|(_, u)| u == uri).map(|(i, _)| *i)
    }
}

/// answer と交渉結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub sdp: SessionDescription,
    pub media: Vec<NegotiatedMedia>,
    /// 合意した BUNDLE グループ（mid の並び）。
    pub bundle: Vec<Vec<String>>,
}

fn norm_fb(s: &str) -> String {
    s.split_whitespace()
        .map(|w| w.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ")
}

/// H.264 のプロファイル分類（libwebrtc と同じ考え方。constraint_set の組み合わせで決める）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum H264Profile {
    ConstrainedBaseline,
    Baseline,
    Main,
    ConstrainedHigh,
    High,
    PredictiveHigh444,
    Other(u8),
}

/// profile-level-id（6 桁の 16 進）を（プロファイル, profile_idc, profile-iop, level_idc）にする。
fn parse_plid(s: &str) -> Option<(H264Profile, u8, u8, u8)> {
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    let (idc, iop, level) = ((v >> 16) as u8, (v >> 8) as u8, v as u8);
    let p = match idc {
        0x42 if iop & 0x40 != 0 => H264Profile::ConstrainedBaseline,
        0x42 => H264Profile::Baseline,
        0x4D if iop & 0x80 != 0 => H264Profile::ConstrainedBaseline,
        0x4D => H264Profile::Main,
        0x58 if iop & 0xC0 == 0xC0 => H264Profile::ConstrainedBaseline,
        0x58 if iop & 0x80 != 0 => H264Profile::Baseline,
        0x64 if iop & 0x0C == 0x0C => H264Profile::ConstrainedHigh,
        0x64 => H264Profile::High,
        0xF4 => H264Profile::PredictiveHigh444,
        other => H264Profile::Other(other),
    };
    Some((p, idc, iop, level))
}

/// profile-level-id がないときの値。RFC 6184 の既定（42000a、Baseline 1.0）ではなく、
/// ブラウザー（libwebrtc）と同じ Constrained Baseline 3.1 とみなす。
const DEFAULT_PLID: &str = "42e01f";

fn set_param(params: &mut Vec<(String, String)>, key: &str, value: String) {
    match params.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
        Some(p) => p.1 = value,
        None => params.push((key.to_string(), value)),
    }
}

/// 相手のコーデックと自分の能力が合えば、answer に書く fmtp を返す。
fn match_codec(
    offer: &RtpMap,
    offer_fmtp: &[(String, String)],
    cap: &CodecCap,
) -> Option<Vec<(String, String)>> {
    if !offer.encoding.eq_ignore_ascii_case(&cap.encoding) || offer.clock_rate != cap.clock_rate {
        return None;
    }
    let enc = cap.encoding.to_ascii_uppercase();
    match enc.as_str() {
        "OPUS" => {
            // RFC 7587: 常に opus/48000/2
            if offer.channels != Some(2) {
                return None;
            }
        }
        _ => {
            if offer.channels.unwrap_or(1) != cap.channels.unwrap_or(1) {
                return None;
            }
        }
    }
    let mut answer = cap.fmtp.clone();
    match enc.as_str() {
        "H264" => {
            let pm = |p: &[(String, String)]| {
                get_param(p, "packetization-mode")
                    .unwrap_or("0")
                    .to_string()
            };
            if pm(offer_fmtp) != pm(&cap.fmtp) {
                return None;
            }
            let (po, _, _, lo) =
                parse_plid(get_param(offer_fmtp, "profile-level-id").unwrap_or(DEFAULT_PLID))?;
            let (pc, ic, iopc, lc) =
                parse_plid(get_param(&cap.fmtp, "profile-level-id").unwrap_or(DEFAULT_PLID))?;
            if po != pc {
                return None;
            }
            let asym =
                |p: &[(String, String)]| get_param(p, "level-asymmetry-allowed") == Some("1");
            let both_asym = asym(offer_fmtp) && asym(&cap.fmtp);
            // level 1b の特例（level_idc 11 + constraint_set3）は区別しない（数値の大小で比べる）
            let level = if both_asym { lc } else { lo.min(lc) };
            set_param(
                &mut answer,
                "profile-level-id",
                format!("{ic:02x}{iopc:02x}{level:02x}"),
            );
            if !both_asym {
                answer.retain(|(k, _)| !k.eq_ignore_ascii_case("level-asymmetry-allowed"));
            }
        }
        "VP9" => {
            let pid =
                |p: &[(String, String)]| get_param(p, "profile-id").unwrap_or("0").to_string();
            if pid(offer_fmtp) != pid(&cap.fmtp) {
                return None;
            }
        }
        "AV1" => {
            let pr = |p: &[(String, String)]| get_param(p, "profile").unwrap_or("0").to_string();
            if pr(offer_fmtp) != pr(&cap.fmtp) {
                return None;
            }
        }
        _ => {}
    }
    Some(answer)
}

fn intersect_fb(offered: &[RtcpFb], cap: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in offered {
        let s = f.feedback();
        let n = norm_fb(&s);
        if cap.iter().any(|c| norm_fb(c) == n) && !out.iter().any(|o| norm_fb(o) == n) {
            out.push(s);
        }
    }
    out
}

fn is_rtx(r: &RtpMap) -> bool {
    r.encoding.eq_ignore_ascii_case("rtx")
}

fn check_text(s: &str, what: &'static str) -> Result<()> {
    if s.chars().any(|c| c.is_control()) {
        return Err(Error::Invalid(what));
    }
    Ok(())
}

fn check_params(p: &[(String, String)]) -> Result<()> {
    for (k, v) in p {
        if !is_token(k) || v.chars().any(|c| c.is_control() || c == ';' || c == ' ') {
            return Err(Error::Invalid("fmtp の項目"));
        }
    }
    Ok(())
}

fn push_transport_attrs(
    m: &mut MediaDescription,
    ice: Option<&IceCredentials>,
    ice_options: &[String],
    fingerprint: Option<&Fingerprint>,
    setup: Option<Setup>,
) {
    if let Some(ice) = ice {
        m.push_attr("ice-ufrag", ice.ufrag.clone());
        m.push_attr("ice-pwd", ice.pwd.clone());
    }
    if !ice_options.is_empty() {
        m.push_attr("ice-options", ice_options.join(" "));
    }
    if let Some(fp) = fingerprint {
        m.push_attr("fingerprint", fp.to_string());
    }
    if let (Some(s), Some(_)) = (setup, fingerprint) {
        m.push_attr("setup", s.as_str());
    }
}

fn push_candidates(m: &mut MediaDescription, cands: &[Candidate], end: bool) {
    for c in cands {
        m.push_attr("candidate", c.to_string());
    }
    if end {
        m.push_flag("end-of-candidates");
    }
}

/// 候補から m 行のポートと c= を決める（候補がなければ JSEP の既定 9 と 0.0.0.0）。
fn default_address(cands: &[Candidate]) -> (u16, Connection) {
    use crate::attr::CandidateAddress;
    for c in cands {
        match &c.address {
            CandidateAddress::V4(a) => return (c.port, Connection::ip4(a.to_string())),
            CandidateAddress::V6(a) => {
                return (
                    c.port,
                    Connection {
                        net_type: "IN".into(),
                        addr_type: "IP6".into(),
                        address: a.to_string(),
                    },
                );
            }
            CandidateAddress::Fqdn(_) => {}
        }
    }
    (9, Connection::ip4("0.0.0.0"))
}

fn push_send_source(m: &mut MediaDescription, src: &SendSource) {
    if let Some(msid) = &src.msid {
        m.push_attr("msid", msid.to_string());
    }
    if let Some(rtx) = src.rtx_ssrc {
        m.push_attr("ssrc-group", format!("FID {} {}", src.ssrc, rtx));
    }
    m.push_attr("ssrc", format!("{} cname:{}", src.ssrc, src.cname));
    if let Some(rtx) = src.rtx_ssrc {
        m.push_attr("ssrc", format!("{} cname:{}", rtx, src.cname));
    }
}

fn check_send_source(s: &SendSource) -> Result<()> {
    if s.cname.is_empty() || s.cname.chars().any(|c| c.is_control() || c == ' ') {
        return Err(Error::Invalid("cname"));
    }
    if let Some(m) = &s.mid
        && !is_token(m)
    {
        return Err(Error::Invalid("mid"));
    }
    Ok(())
}

/// 相手の offer に answer を作る。
pub fn create_answer(
    offer: &SessionDescription,
    caps: &Capabilities,
    opts: &AnswerOptions,
) -> Result<Answer> {
    if let Some(ice) = &opts.ice {
        validate_ice_ufrag(&ice.ufrag)?;
        validate_ice_pwd(&ice.pwd)?;
    }
    for s in &opts.send_sources {
        check_send_source(s)?;
    }
    check_text(&opts.origin_address, "origin のアドレス")?;
    // mid の重複を拒否する
    let mut mids = HashSet::new();
    for m in &offer.media {
        if let Some(mid) = m.mid()
            && !mids.insert(mid)
        {
            return Err(Error::Negotiation("mid が重複"));
        }
    }
    let offer_bundles = offer.bundle_groups();
    for g in &offer_bundles {
        for mid in g {
            if !mids.contains(mid.as_str()) {
                return Err(Error::Negotiation("BUNDLE の mid に対応する m 行がない"));
            }
        }
    }
    let in_bundle = |mid: Option<&str>| {
        mid.is_some_and(|m| offer_bundles.iter().any(|g| g.iter().any(|x| x == m)))
    };

    let (port, conn) = default_address(&opts.candidates);
    let mut sdp = SessionDescription {
        origin: Origin {
            session_id: opts.session_id,
            session_version: opts.session_version,
            address: opts.origin_address.clone(),
            ..Origin::default()
        },
        ..SessionDescription::default()
    };
    if opts.ice_lite {
        sdp.push_flag("ice-lite");
    }
    if offer.has_flag("extmap-allow-mixed") {
        sdp.push_flag("extmap-allow-mixed");
    }
    let mut results = Vec::new();
    let mut used_sources = vec![false; opts.send_sources.len()];

    for (idx, om) in offer.media.iter().enumerate() {
        let kind = MediaKind::from_media(&om.media);
        let mid = om.mid();
        let mut am = MediaDescription::new(om.media.clone(), 0, om.protocol.clone());
        let bundle_only = om.port == 0 && om.has_flag("bundle-only") && in_bundle(mid);
        let reject = |am: &mut MediaDescription| {
            am.port = 0;
            am.formats = om.formats.clone();
            if let Some(mid) = mid {
                am.push_attr("mid", mid);
            }
        };
        if (om.port == 0 && !bundle_only) || kind == MediaKind::Other {
            reject(&mut am);
            sdp.media.push(am);
            results.push(NegotiatedMedia::rejected(idx, om));
            continue;
        }

        let offer_setup = offer.setup(idx);
        let our_setup = match offer_setup {
            None | Some(Setup::ActPass) => match opts.setup {
                Setup::ActPass => Setup::Active,
                s => s,
            },
            Some(Setup::Active) => Setup::Passive,
            Some(Setup::Passive) => Setup::Active,
            Some(Setup::HoldConn) => return Err(Error::Negotiation("setup:holdconn")),
        };
        if opts.fingerprint.is_some()
            && offer.fingerprint(idx).is_none()
            && om.protocol.contains("TLS")
        {
            return Err(Error::Negotiation("DTLS の offer に fingerprint がない"));
        }

        let mut nm = NegotiatedMedia::rejected(idx, om);
        nm.rejected = false;

        if kind == MediaKind::Application {
            let is_dc = om.protocol.ends_with("DTLS/SCTP")
                && om.formats.iter().any(|f| f == "webrtc-datachannel");
            if !is_dc || !opts.accept_datachannel {
                reject(&mut am);
                sdp.media.push(am);
                results.push(NegotiatedMedia::rejected(idx, om));
                continue;
            }
            am.port = port;
            am.formats = vec!["webrtc-datachannel".into()];
            am.connection = Some(conn.clone());
            if let Some(mid) = mid {
                am.push_attr("mid", mid);
            }
            push_transport_attrs(
                &mut am,
                opts.ice.as_ref(),
                &opts.ice_options,
                opts.fingerprint.as_ref(),
                Some(our_setup),
            );
            am.push_attr("sctp-port", opts.sctp_port.to_string());
            am.push_attr("max-message-size", opts.max_message_size.to_string());
            push_candidates(&mut am, &opts.candidates, opts.end_of_candidates);
            nm.direction = Direction::SendRecv;
            nm.remote_sctp_port = om.sctp_port();
            nm.remote_max_message_size = om.max_message_size();
            sdp.media.push(am);
            results.push(nm);
            continue;
        }

        // RTP の m 行
        let rtcp_mux = om.has_flag("rtcp-mux");
        if !rtcp_mux && opts.rtcp_mux == RtcpMuxPolicy::Require {
            return Err(Error::Negotiation("rtcp-mux がない"));
        }

        // コーデックの共通部分（offer の並びを保つ）
        let pts = om.payload_types();
        let mut codecs: Vec<(NegotiatedCodec, &CodecCap)> = Vec::new();
        for &pt in &pts {
            let Some(rm) = om.rtpmap(pt) else { continue };
            if is_rtx(&rm) {
                continue;
            }
            let ofmtp = om.fmtp(pt).map(|f| f.params).unwrap_or_default();
            let Some((cap, fmtp)) = caps
                .codecs
                .iter()
                .filter(|c| c.kind == kind)
                .find_map(|c| match_codec(&rm, &ofmtp, c).map(|f| (c, f)))
            else {
                continue;
            };
            let fb = intersect_fb(&om.rtcp_fbs(pt), &cap.rtcp_fb);
            codecs.push((
                NegotiatedCodec {
                    payload_type: pt,
                    encoding: rm.encoding.clone(),
                    clock_rate: rm.clock_rate,
                    channels: rm.channels,
                    fmtp,
                    remote_fmtp: ofmtp,
                    rtcp_fb: fb,
                    rtx_payload_type: None,
                },
                cap,
            ));
        }
        if codecs.is_empty() {
            reject(&mut am);
            sdp.media.push(am);
            results.push(NegotiatedMedia::rejected(idx, om));
            continue;
        }
        // RTX（apt で結び付ける）
        if caps.rtx {
            for &pt in &pts {
                let Some(rm) = om.rtpmap(pt) else { continue };
                if !is_rtx(&rm) {
                    continue;
                }
                let apt = om
                    .fmtp(pt)
                    .and_then(|f| f.get("apt").and_then(|a| a.parse::<u8>().ok()));
                if let Some(c) = codecs
                    .iter_mut()
                    .find(|(c, _)| Some(c.payload_type) == apt && c.rtx_payload_type.is_none())
                {
                    c.0.rtx_payload_type = Some(pt);
                }
            }
        }

        // 方向
        let local = opts
            .direction_overrides
            .iter()
            .find(|(m, _)| Some(m.as_str()) == mid)
            .map(|(_, d)| *d)
            .unwrap_or(match kind {
                MediaKind::Audio => opts.audio_direction,
                _ => opts.video_direction,
            });
        let direction = offer.effective_direction(idx).reverse().intersect(local);

        // ヘッダー拡張（offer の id を保つ）
        let mut extmaps = Vec::new();
        for e in om.extmaps() {
            if caps.header_extensions.contains(&e.uri) && !extmaps.iter().any(|(i, _)| *i == e.id) {
                extmaps.push((e.id, e.uri.clone()));
            }
        }

        // 組み立て
        am.port = port;
        am.connection = Some(conn.clone());
        for (c, _) in &codecs {
            am.formats.push(c.payload_type.to_string());
            if let Some(r) = c.rtx_payload_type {
                am.formats.push(r.to_string());
            }
        }
        if let Some(mid) = mid {
            am.push_attr("mid", mid);
        }
        push_transport_attrs(
            &mut am,
            opts.ice.as_ref(),
            &opts.ice_options,
            opts.fingerprint.as_ref(),
            Some(our_setup),
        );
        for (id, uri) in &extmaps {
            am.push_attr("extmap", format!("{id} {uri}"));
        }
        am.push_flag(direction.as_str());
        let src = if direction.sends() {
            opts.send_sources.iter().enumerate().find(|(i, s)| {
                !used_sources[*i] && s.kind == kind && (s.mid.is_none() || s.mid.as_deref() == mid)
            })
        } else {
            None
        };
        if rtcp_mux {
            am.push_flag("rtcp-mux");
        }
        if om.has_flag("rtcp-rsize") {
            am.push_flag("rtcp-rsize");
            nm.rtcp_rsize = true;
        }
        for (c, _) in &codecs {
            let rm = RtpMap {
                payload_type: c.payload_type,
                encoding: c.encoding.clone(),
                clock_rate: c.clock_rate,
                channels: c.channels,
            };
            am.push_attr("rtpmap", rm.to_string());
            for fb in &c.rtcp_fb {
                am.push_attr("rtcp-fb", format!("{} {}", c.payload_type, fb));
            }
            if !c.fmtp.is_empty() {
                am.push_attr(
                    "fmtp",
                    format!("{} {}", c.payload_type, params_to_string(&c.fmtp)),
                );
            }
            if let Some(r) = c.rtx_payload_type {
                am.push_attr("rtpmap", format!("{r} rtx/{}", c.clock_rate));
                am.push_attr("fmtp", format!("{r} apt={}", c.payload_type));
            }
        }
        // simulcast（相手が送る）
        let offer_sc = om.simulcast();
        let offer_rids: Vec<Rid> = om
            .rids()
            .into_iter()
            .filter(|r| r.direction == RidDirection::Send)
            .collect();
        if let Some(sc) = &offer_sc
            && !sc.send.is_empty()
            && opts.accept_simulcast
            && direction.recvs()
        {
            let known: HashSet<&str> = offer_rids.iter().map(|r| r.id.as_str()).collect();
            let mut recv = Vec::new();
            for stream in &sc.send {
                let alts: Vec<_> = stream
                    .iter()
                    .filter(|r| known.contains(r.rid.as_str()))
                    .cloned()
                    .collect();
                if !alts.is_empty() {
                    recv.push(alts);
                }
            }
            for r in &offer_rids {
                if recv.iter().flatten().any(|x| x.rid == r.id) {
                    am.push_attr("rid", format!("{} recv", r.id));
                }
            }
            if !recv.is_empty() {
                am.push_attr(
                    "simulcast",
                    Simulcast {
                        send: Vec::new(),
                        recv,
                    }
                    .to_string(),
                );
                nm.rids = offer_rids.clone();
                nm.simulcast = offer_sc.clone();
            }
        }
        if let Some((i, s)) = src {
            used_sources[i] = true;
            push_send_source(&mut am, s);
        }
        push_candidates(&mut am, &opts.candidates, opts.end_of_candidates);

        nm.direction = direction;
        nm.codecs = codecs.into_iter().map(|(c, _)| c).collect();
        nm.extmaps = extmaps;
        nm.rtcp_mux = rtcp_mux;
        nm.remote_ssrcs = om.ssrcs();
        nm.remote_ssrc_groups = om.ssrc_groups();
        sdp.media.push(am);
        results.push(nm);
    }

    // BUNDLE（受け付けた mid だけ残す）
    let mut bundle = Vec::new();
    for g in &offer_bundles {
        let accepted: Vec<String> = g
            .iter()
            .filter(|mid| {
                results
                    .iter()
                    .any(|r| !r.rejected && r.mid.as_deref() == Some(mid.as_str()))
            })
            .cloned()
            .collect();
        if !accepted.is_empty() {
            sdp.attributes.insert(
                bundle.len(),
                Attribute::new("group", format!("BUNDLE {}", accepted.join(" "))),
            );
            bundle.push(accepted);
        }
    }
    if results.iter().all(|r| r.rejected) && !offer.media.is_empty() {
        return Err(Error::Negotiation("受け付けられる m 行がない"));
    }
    Ok(Answer {
        sdp,
        media: results,
        bundle,
    })
}

/// offer の生成のしかた。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferMode {
    /// WebRTC（ICE・DTLS・BUNDLE・rtcp-mux、`UDP/TLS/RTP/SAVPF`）。WHEP のサーバー送信 offer。
    WebRtc,
    /// RTSP の DESCRIBE 応答（`RTP/AVP`、ポート 0、`a=control`）。
    Rtsp,
}

/// offer に載せるコーデック。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferCodec {
    pub payload_type: u8,
    pub encoding: String,
    pub clock_rate: u32,
    pub channels: Option<u16>,
    /// `sprop-parameter-sets`・`config` なども呼び出し側が入れる。
    pub fmtp: Vec<(String, String)>,
    pub rtcp_fb: Vec<String>,
    pub rtx_payload_type: Option<u8>,
}

impl From<&CodecCap> for OfferCodec {
    fn from(c: &CodecCap) -> Self {
        OfferCodec {
            payload_type: c.payload_type,
            encoding: c.encoding.clone(),
            clock_rate: c.clock_rate,
            channels: c.channels,
            fmtp: c.fmtp.clone(),
            rtcp_fb: c.rtcp_fb.clone(),
            rtx_payload_type: None,
        }
    }
}

/// offer の m 行 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferMedia {
    pub kind: MediaKind,
    pub mid: String,
    pub direction: Direction,
    pub codecs: Vec<OfferCodec>,
    pub extmaps: Vec<(u8, String)>,
    pub send: Option<SendSource>,
    /// RTSP の `a=control`（`None` なら `trackID=<番号>`）。
    pub control: Option<String>,
    /// DataChannel の m 行（kind は Application）。
    pub datachannel: bool,
}

/// offer を作る設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferOptions {
    pub mode: OfferMode,
    pub session_id: u64,
    pub session_version: u64,
    pub origin_address: String,
    pub session_name: String,
    pub ice: Option<IceCredentials>,
    pub ice_lite: bool,
    pub ice_options: Vec<String>,
    pub fingerprint: Option<Fingerprint>,
    /// 既定 actpass。
    pub setup: Setup,
    pub candidates: Vec<Candidate>,
    pub end_of_candidates: bool,
    pub sctp_port: u16,
    pub max_message_size: u64,
    /// RTSP: session 単位の `a=range:`（例 `npt=now-`、`npt=0-12.5`）。
    pub rtsp_range: Option<String>,
    /// RTSP: session 単位の `a=control:`（例 `*`）。
    pub rtsp_control: Option<String>,
}

impl Default for OfferOptions {
    fn default() -> Self {
        OfferOptions {
            mode: OfferMode::WebRtc,
            session_id: 0,
            session_version: 0,
            origin_address: "127.0.0.1".into(),
            session_name: "-".into(),
            ice: None,
            ice_lite: false,
            ice_options: Vec::new(),
            fingerprint: None,
            setup: Setup::ActPass,
            candidates: Vec::new(),
            end_of_candidates: false,
            sctp_port: 5000,
            max_message_size: 262144,
            rtsp_range: None,
            rtsp_control: None,
        }
    }
}

fn check_offer_codec(c: &OfferCodec) -> Result<()> {
    if c.payload_type > 127 || c.rtx_payload_type.is_some_and(|p| p > 127) {
        return Err(Error::Invalid("payload type"));
    }
    if !is_token(&c.encoding) || c.clock_rate == 0 {
        return Err(Error::Invalid("コーデック"));
    }
    check_params(&c.fmtp)?;
    for fb in &c.rtcp_fb {
        if fb.is_empty() || !fb.split(' ').all(is_token) {
            return Err(Error::Invalid("rtcp-fb"));
        }
    }
    Ok(())
}

/// offer を作る。
pub fn create_offer(media: &[OfferMedia], opts: &OfferOptions) -> Result<SessionDescription> {
    if media.len() > crate::session::MAX_MEDIA {
        return Err(Error::Limit("media の数"));
    }
    check_text(&opts.session_name, "s=")?;
    if opts.session_name.is_empty() {
        return Err(Error::Invalid("s= が空"));
    }
    check_text(&opts.origin_address, "origin のアドレス")?;
    if let Some(ice) = &opts.ice {
        validate_ice_ufrag(&ice.ufrag)?;
        validate_ice_pwd(&ice.pwd)?;
    }
    let mut mids = HashSet::new();
    for m in media {
        if !is_token(&m.mid) || !mids.insert(m.mid.as_str()) {
            return Err(Error::Invalid("mid が不正か重複"));
        }
        for c in &m.codecs {
            check_offer_codec(c)?;
        }
        if let Some(s) = &m.send {
            check_send_source(s)?;
        }
        if let Some(c) = &m.control
            && (c.is_empty() || c.chars().any(|ch| ch.is_control() || ch == ' '))
        {
            return Err(Error::Invalid("control"));
        }
        for (id, uri) in &m.extmaps {
            if *id == 0
                || *id == 15
                || uri.is_empty()
                || uri.chars().any(|ch| ch.is_control() || ch == ' ')
            {
                return Err(Error::Invalid("extmap"));
            }
        }
        if !m.datachannel && m.codecs.is_empty() {
            return Err(Error::Invalid("コーデックのない m 行"));
        }
    }
    for r in [&opts.rtsp_range, &opts.rtsp_control].into_iter().flatten() {
        check_text(r, "RTSP の属性")?;
    }

    let mut sdp = SessionDescription {
        origin: Origin {
            session_id: opts.session_id,
            session_version: opts.session_version,
            address: opts.origin_address.clone(),
            ..Origin::default()
        },
        session_name: opts.session_name.clone(),
        ..SessionDescription::default()
    };
    let webrtc = opts.mode == OfferMode::WebRtc;
    let (port, conn) = if webrtc {
        default_address(&opts.candidates)
    } else {
        (0, Connection::ip4("0.0.0.0"))
    };
    if webrtc {
        if !media.is_empty() {
            let mids: Vec<&str> = media.iter().map(|m| m.mid.as_str()).collect();
            sdp.push_attr("group", format!("BUNDLE {}", mids.join(" ")));
        }
        if opts.ice_lite {
            sdp.push_flag("ice-lite");
        }
        sdp.push_flag("extmap-allow-mixed");
    } else {
        sdp.connection = Some(conn.clone());
        if let Some(c) = &opts.rtsp_control {
            sdp.push_attr("control", c.clone());
        }
        if let Some(r) = &opts.rtsp_range {
            sdp.push_attr("range", r.clone());
        }
    }
    for (i, om) in media.iter().enumerate() {
        let proto = match (webrtc, om.datachannel) {
            (true, true) => "UDP/DTLS/SCTP",
            (true, false) => "UDP/TLS/RTP/SAVPF",
            (false, _) => "RTP/AVP",
        };
        if om.datachannel && !webrtc {
            return Err(Error::Invalid("RTSP に DataChannel は載せられない"));
        }
        let mut m = MediaDescription::new(om.kind.as_str(), port, proto);
        if om.datachannel {
            m.media = "application".into();
            m.formats.push("webrtc-datachannel".into());
        }
        for c in &om.codecs {
            m.formats.push(c.payload_type.to_string());
            if let Some(r) = c.rtx_payload_type {
                m.formats.push(r.to_string());
            }
        }
        if webrtc {
            m.connection = Some(conn.clone());
            m.push_attr("mid", om.mid.clone());
            push_transport_attrs(
                &mut m,
                opts.ice.as_ref(),
                &opts.ice_options,
                opts.fingerprint.as_ref(),
                Some(opts.setup),
            );
            if om.datachannel {
                m.push_attr("sctp-port", opts.sctp_port.to_string());
                m.push_attr("max-message-size", opts.max_message_size.to_string());
                push_candidates(&mut m, &opts.candidates, opts.end_of_candidates);
                sdp.media.push(m);
                continue;
            }
            for (id, uri) in &om.extmaps {
                m.push_attr("extmap", format!("{id} {uri}"));
            }
            m.push_flag(om.direction.as_str());
            m.push_flag("rtcp-mux");
            m.push_flag("rtcp-rsize");
        }
        for c in &om.codecs {
            let rm = RtpMap {
                payload_type: c.payload_type,
                encoding: c.encoding.clone(),
                clock_rate: c.clock_rate,
                channels: c.channels,
            };
            m.push_attr("rtpmap", rm.to_string());
            if webrtc {
                for fb in &c.rtcp_fb {
                    m.push_attr("rtcp-fb", format!("{} {}", c.payload_type, fb));
                }
            }
            if !c.fmtp.is_empty() {
                m.push_attr(
                    "fmtp",
                    format!("{} {}", c.payload_type, params_to_string(&c.fmtp)),
                );
            }
            if let Some(r) = c.rtx_payload_type
                && webrtc
            {
                m.push_attr("rtpmap", format!("{r} rtx/{}", c.clock_rate));
                m.push_attr("fmtp", format!("{r} apt={}", c.payload_type));
            }
        }
        if webrtc {
            if let Some(s) = &om.send
                && om.direction.sends()
            {
                push_send_source(&mut m, s);
            }
            push_candidates(&mut m, &opts.candidates, opts.end_of_candidates);
        } else {
            m.push_attr(
                "control",
                om.control.clone().unwrap_or_else(|| format!("trackID={i}")),
            );
        }
        sdp.media.push(m);
    }
    Ok(sdp)
}

/// 自分の offer に対する answer を検査し、交渉結果を返す。
///
/// m 行の数・種類・mid が offer と同じこと、payload type・ヘッダー拡張が offer の部分集合であること、
/// 方向が offer の逆に収まること、BUNDLE の mid が offer のグループに含まれることを確かめる。
pub fn apply_answer(
    offer: &SessionDescription,
    answer: &SessionDescription,
) -> Result<Vec<NegotiatedMedia>> {
    if offer.media.len() != answer.media.len() {
        return Err(Error::Negotiation("m 行の数が offer と違う"));
    }
    let offer_bundles = offer.bundle_groups();
    for g in answer.bundle_groups() {
        for mid in g {
            if !offer_bundles.iter().any(|og| og.contains(&mid)) {
                return Err(Error::Negotiation("answer の BUNDLE に offer にない mid"));
            }
        }
    }
    let mut out = Vec::new();
    for (idx, (om, am)) in offer.media.iter().zip(&answer.media).enumerate() {
        if om.media != am.media {
            return Err(Error::Negotiation("m 行の種類が offer と違う"));
        }
        if om.mid() != am.mid() && am.mid().is_some() {
            return Err(Error::Negotiation("mid が offer と違う"));
        }
        if am.port == 0 && !am.has_flag("bundle-only") {
            out.push(NegotiatedMedia::rejected(idx, om));
            continue;
        }
        if om.port == 0 && !om.has_flag("bundle-only") {
            return Err(Error::Negotiation("拒否した m 行を answer が受け付けた"));
        }
        let kind = MediaKind::from_media(&am.media);
        let mut nm = NegotiatedMedia::rejected(idx, am);
        nm.rejected = false;
        nm.mid = om.mid().map(str::to_string);
        if kind == MediaKind::Application {
            nm.direction = Direction::SendRecv;
            nm.remote_sctp_port = am.sctp_port();
            nm.remote_max_message_size = am.max_message_size();
            out.push(nm);
            continue;
        }
        let answer_dir = answer.effective_direction(idx);
        let ours = answer_dir.reverse();
        let offered = offer.effective_direction(idx);
        if offered.intersect(ours) != ours {
            return Err(Error::Negotiation("answer の方向が offer に収まらない"));
        }
        let offer_pts = om.payload_types();
        let mut codecs: Vec<NegotiatedCodec> = Vec::new();
        let apts = am.payload_types();
        for &pt in &apts {
            if !offer_pts.contains(&pt) {
                return Err(Error::Negotiation("answer に offer にない payload type"));
            }
            let (Some(arm), Some(orm)) = (am.rtpmap(pt), om.rtpmap(pt)) else {
                return Err(Error::Negotiation("rtpmap がない"));
            };
            if !arm.encoding.eq_ignore_ascii_case(&orm.encoding) || arm.clock_rate != orm.clock_rate
            {
                return Err(Error::Negotiation("payload type の意味が offer と違う"));
            }
            if is_rtx(&arm) {
                continue;
            }
            codecs.push(NegotiatedCodec {
                payload_type: pt,
                encoding: orm.encoding.clone(),
                clock_rate: orm.clock_rate,
                channels: orm.channels,
                fmtp: om.fmtp(pt).map(|f| f.params).unwrap_or_default(),
                remote_fmtp: am.fmtp(pt).map(|f| f.params).unwrap_or_default(),
                rtcp_fb: am.rtcp_fbs(pt).iter().map(|f| f.feedback()).collect(),
                rtx_payload_type: None,
            });
        }
        for &pt in &apts {
            if am.rtpmap(pt).is_some_and(|r| is_rtx(&r)) {
                let apt = am
                    .fmtp(pt)
                    .and_then(|f| f.get("apt").and_then(|a| a.parse::<u8>().ok()));
                if let Some(c) = codecs.iter_mut().find(|c| Some(c.payload_type) == apt) {
                    c.rtx_payload_type = Some(pt);
                }
            }
        }
        if codecs.is_empty() {
            return Err(Error::Negotiation("answer にコーデックがない"));
        }
        let oext = om.extmaps();
        for e in am.extmaps() {
            if !oext.iter().any(|o| o.id == e.id && o.uri == e.uri) {
                return Err(Error::Negotiation("answer に offer にないヘッダー拡張"));
            }
            nm.extmaps.push((e.id, e.uri));
        }
        nm.direction = ours;
        nm.codecs = codecs;
        nm.rtcp_mux = am.has_flag("rtcp-mux");
        nm.rtcp_rsize = am.has_flag("rtcp-rsize");
        nm.remote_ssrcs = am.ssrcs();
        nm.remote_ssrc_groups = am.ssrc_groups();
        nm.rids = am
            .rids()
            .into_iter()
            .filter(|r| r.direction == RidDirection::Send)
            .collect();
        nm.simulcast = am.simulcast();
        out.push(nm);
    }
    Ok(out)
}

/// `Extmap` を (id, URI) の並びにする補助。
pub fn extmap_pairs(m: &MediaDescription) -> Vec<(u8, String)> {
    m.extmaps()
        .into_iter()
        .map(|e: Extmap| (e.id, e.uri))
        .collect()
}
