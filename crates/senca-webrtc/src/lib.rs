//! サーバー側の WebRTC 端点（sans-io）と小さな SFU。
//!
//! 1 つの [`PeerConnection`] は 1 つの UDP の 5-tuple（ICE の選んだ pair）の上で、BUNDLE と rtcp-mux だけを扱う。
//! 受けた datagram を RFC 7983 で振り分け、ICE（STUN）→ DTLS → SRTP/SRTCP と SCTP（DataChannel）へ渡す。
//! I/O・スレッド・時計を持たない。時刻は引数で渡す。
//!
//! ## 使い方
//!
//! ```text
//! let mut pc = PeerConnection::new(PeerConfig::default(), now)?;
//! pc.add_host_address(now, "203.0.113.10:50000".parse()?)?;   // 受けるソケットのアドレス
//! let answer = pc.accept_offer(now, &offer_sdp)?;              // WHIP: ブラウザーの offer に answer
//! // または let offer = pc.create_offer(now, &OfferSpec { .. })?; pc.apply_answer(now, &answer)?;  // WHEP
//! loop {
//!     pc.handle_datagram(now, from, &buf)?;                   // ソケットで受けたら
//!     while let Some(t) = pc.poll_transmit() { socket.send_to(&t.data, t.remote) }
//!     while let Some(e) = pc.poll_event() {
//!         match e {
//!             Event::Track(info) => { .. }                        // 受信トラック（simulcast は層ごと）
//!             Event::AccessUnit { track, au, .. } => { .. }       // 組み立て済みの AU
//!             Event::KeyframeRequest { track } => { .. }          // 送信トラックにキーフレームが要る
//!             Event::BandwidthEstimate { bps } => { .. }          // 下りの推定帯域
//!             _ => {}
//!         }
//!     }
//!     pc.send_access_unit(now, track, &au, Some(&config))?;     // 送信トラックへ AU
//!     sleep_until(pc.poll_timeout()); pc.handle_timeout(now);
//! }
//! ```
//!
//! ## 構成
//!
//! - [`pc`] — [`PeerConnection`]（交渉・ICE・DTLS-SRTP・受信・送信・RTCP・DataChannel）
//! - [`sfu`] — [`Room`]（publisher・subscriber、層の選択、キーフレーム要求の集約）
//! - [`bwe`] — 送信側の帯域推定（transport-wide-cc の遅延勾配 + 損失、REMB は代わり）
//! - [`pacer`] — 送出の平準化（leaky bucket）
//! - [`nack`] — 受信側の NACK の生成（期限・回数・頻度の制限）
//! - [`twcc`] — transport-wide-cc の受信記録と feedback の生成、送信記録
//! - [`ext`] — RTP ヘッダー拡張（mid・rid・abs-send-time・transport-wide-cc・audio-level・playout-delay）
//! - [`sdpfrag`] — trickle ICE の `application/trickle-ice-sdpfrag`（RFC 8840、WHIP の PATCH は RFC 9725）
//!
//! ## 安全
//!
//! - DTLS の相手証明書は SDP の `a=fingerprint` と照合し、一致した後でだけ SRTP の鍵を作る。
//!   SRTP/SRTCP は認証に通ったものだけを扱う（鍵ができる前の RTP は捨てる）。
//! - 受信の頻度（packet/s・byte/s）に上限を置き、待ち行列（出来事・送信・pacer・ジッター・再送の履歴）はすべて有界。
//! - キーフレーム要求は集約し間隔を制限する（仕様 6.2・9.4）。

pub mod bwe;
pub mod config;
pub mod ext;
pub mod nack;
pub mod pacer;
pub mod pc;
pub mod sdpfrag;
pub mod sfu;
pub mod twcc;

mod inbound;
mod outbound;

#[cfg(test)]
mod e2e_tests;

pub use config::{BweConfig, NackConfig, PacerConfig, PeerConfig, RateLimit};
pub use pc::{
    DisconnectReason, Event, OfferMediaSpec, OfferSpec, PeerConnection, SendTrackInfo, TrackInfo,
    WebRtcError, stun_username_ufrag,
};
pub use sdpfrag::{SdpFrag, SdpFragMedia};
pub use sfu::{Room, RoomConfig, RoomOutput};

pub use senca_core::media::{AccessUnit, Codec, CodecConfig, MediaKind, TrackId};
pub use senca_core::time::Timebase;
pub use senca_dtls::DtlsCertificate;
pub use senca_ice::{IceState, Transmit};
pub use senca_sctp::{ChannelInfo, ChannelOptions, Reliability};
pub use senca_sdp::{Capabilities, CodecCap, Direction};
