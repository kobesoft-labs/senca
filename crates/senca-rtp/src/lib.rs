//! RTP/RTCP（sans-io）。I/O・スレッド・時計を持たない。時刻は引数で渡す。
//!
//! - [`packet`]: RTP ヘッダー（CSRC・RFC 8285 のヘッダー拡張・padding）の解析と組み立て、RTP/RTCP の多重の判別
//! - [`rtcp`]: SR・RR・SDES・BYE・APP、複合パケット、reduced-size（RFC 5506）、feedback（NACK・PLI・FIR・REMB・transport-cc）
//! - [`stats`]: 受信統計（RFC 3550 A.1 の送信元検証、A.3 の損失、A.8 のジッター）と受信報告
//! - [`jitter`]: 並べ替えと期限つきの待ち合わせ（NACK の候補を出す）
//! - [`ssrc`]: SSRC の衝突・ループの検出
//! - [`keyframe`]: PLI・FIR の集約と間隔制限（仕様 6.2）
//! - [`rtx`]: RTX（RFC 4588）の包み直しと再送履歴
//! - [`interval`]: RTCP の送信間隔（RFC 3550 A.7）
//! - [`clock`]: SR による NTP ↔ RTP の対応（lip sync・撮影時刻）
//! - [`payload`]: payload format（H.264・H.265・VP8・VP9・AV1・Opus・AAC・G.711）の packetizer / depacketizer
//! - [`session`]: [`RtpSender`]（AU → RTP）と [`TrackReceiver`]（RTP → AU）
//!
//! ## 受信の流れ
//!
//! ```text
//! let pkt = RtpPacket::parse(datagram)?;          // RTCP なら rtcp::parse_compound / parse_reduced
//! match ssrc_table.check(pkt.header.ssrc, &from, now) { ... }   // 衝突・ループを捨てる
//! receiver.observe(&pkt, now);                     // 統計（到着順）
//! jitter.push(pkt, now);
//! while let Some(o) = jitter.pop(now) {            // jitter.poll_timeout() の時刻にも呼ぶ
//!     match o {
//!         JitterOutput::Packet(p) => { let _ = receiver.push(&p); }  // Err は壊れた payload（数えて続ける）
//!         JitterOutput::Lost { .. } => receiver.mark_loss(),
//!         JitterOutput::Reset => receiver.reset(),
//!     }
//! }
//! while let Some((au, new_config)) = receiver.pop() { ... }
//! ```
//!
//! ## 送信の流れ
//!
//! ```text
//! let (packetizer, format) = payload::packetizer(codec, Some(&config), payload::DEFAULT_MTU)?;  // format は SDP の rtpmap/fmtp
//! let mut sender = RtpSender::new(packetizer, pt, ssrc, random_seq, random_ts);
//! for pkt in sender.send(&au, Some(&config))? { socket.send(&pkt.to_bytes()?)?; }
//! ```

pub mod clock;
pub mod error;
pub mod interval;
pub mod jitter;
pub mod keyframe;
pub mod packet;
pub mod payload;
pub mod rtcp;
pub mod rtx;
pub mod session;
pub mod ssrc;
pub mod stats;

pub use error::{Error, Result};
pub use jitter::{JitterBuffer, JitterOutput, PushResult};
pub use packet::{ExtensionElement, HeaderExtension, RtpHeader, RtpPacket, is_rtcp};
pub use payload::{Depacketizer, Frame, Packetizer, Payload, PayloadFormat};
pub use rtcp::{RtcpPacket, build_compound, parse_compound, parse_reduced};
pub use session::{RtpSender, TrackReceiver};
pub use stats::ReceiverStats;
