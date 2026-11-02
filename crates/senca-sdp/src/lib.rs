//! SDP（RFC 8866）の解析・組み立てと、サーバー端点向けの offer/answer 交渉（JSEP-lite）。I/O を持たない。
//!
//! - [`session`] は [`SessionDescription`]・[`MediaDescription`] の厳格な解析（上限つき、仕様 9.4）と出力（CRLF）
//! - [`attr`] は 型付きの属性（rtpmap・fmtp・rtcp-fb・extmap・msid・ssrc・ssrc-group・rid・simulcast・group・
//!   candidate（RFC 8839）・fingerprint・setup・ice-ufrag/pwd/options）
//! - [`negotiate`] は [`create_answer`]・[`create_offer`]（WebRTC・RTSP）・[`apply_answer`]
//!
//! 属性の値は解析時には文字列のまま持ち（往復で変わらない）、型付きの値は補助関数で取り出す。
//! media 単位の属性は session 単位より優先する（[`SessionDescription::effective_attribute`] など）。
//!
//! ```text
//! let offer = SessionDescription::parse(text)?;
//! let answer = create_answer(&offer, &Capabilities::default_server(), &AnswerOptions { .. })?;
//! send(answer.sdp.to_string());
//! for m in &answer.media { /* m.codecs・m.extmaps・m.direction を RTP 側に渡す */ }
//! ```

pub mod attr;
pub mod error;
pub mod negotiate;
pub mod session;

pub use attr::{
    Attribute, Candidate, CandidateAddress, CandidateType, Direction, Extmap, Fingerprint, Fmtp,
    Group, Msid, Rid, RidDirection, RtcpFb, RtpMap, Setup, Simulcast, SimulcastRid, SsrcAttr,
    SsrcGroup, parse_ice_options, validate_ice_pwd, validate_ice_ufrag,
};
pub use error::{Error, Result};
pub use negotiate::{
    Answer, AnswerOptions, Capabilities, CodecCap, IceCredentials, MediaKind, NegotiatedCodec,
    NegotiatedMedia, OfferCodec, OfferMedia, OfferMode, OfferOptions, RtcpMuxPolicy, SendSource,
    apply_answer, create_answer, create_offer,
};
pub use session::{
    Bandwidth, Connection, MAX_SDP_BYTES, MediaDescription, Origin, ParseOptions,
    SessionDescription, Timing,
};
