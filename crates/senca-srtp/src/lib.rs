//! SRTP/SRTCP（RFC 3711・RFC 7714）。
//!
//! I/O を持たない。RTP/RTCP のバイト列を入れると保護済み（または復号済み）のバイト列が返る。
//!
//! - 保護方式: `AES_CM_128_HMAC_SHA1_80`・`AES_CM_128_HMAC_SHA1_32`・`AEAD_AES_128_GCM`・`AEAD_AES_256_GCM`。
//! - 鍵導出は RFC 3711 4.3 の AES-CM PRF（kdr = 0）。AES-256-GCM は RFC 6188 の AES_256_CM_PRF。
//! - ROC（rollover counter）は RFC 3711 3.3.1 の推定。replay window は 64〜1024。
//! - 1 つの [`SrtpContext`] は 1 方向（送信か受信）だけに使う。DTLS-SRTP では方向ごとに鍵が違うため、
//!   [`dtls_srtp_contexts`] で送信用と受信用の 2 つを作る。
//! - 同じ鍵で同じ index（= nonce）を 2 回使わない。送信側は使った index を覚え、重なる場合・
//!   上限（SRTP 2^48、SRTCP 2^31）に達した場合は保護を拒否する（仕様 AC-20）。
//! - 受信側は認証に通った後でだけ平文を返し、状態（ROC・replay window・SSRC 表）を更新する。
//!
//! 暗号の演算（AES・HMAC-SHA1・GCM）は RustCrypto を使う（ADR-0002）。

mod context;
mod kdf;
mod profile;
mod replay;

pub use context::{Direction, SrtpContext, SrtpError, dtls_srtp_contexts};
pub use kdf::aes_cm_prf;
pub use profile::{MasterKey, SrtpProfile};
pub use replay::{ReplayCheck, ReplayWindow};

/// replay window の既定の幅（packet 数）。
pub const DEFAULT_REPLAY_WINDOW: usize = 128;
/// replay window の最小幅（RFC 3711 3.3.2 は 64 以上を求める）。
pub const MIN_REPLAY_WINDOW: usize = 64;
/// replay window の最大幅。
pub const MAX_REPLAY_WINDOW: usize = 1024;
/// 1 つの context が持つ SSRC の既定上限。
pub const DEFAULT_MAX_STREAMS: usize = 1024;
