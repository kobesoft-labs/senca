//! STUN（RFC 8489）のメッセージの解析・組み立て、完全性検査、資格情報、再送タイマー。
//!
//! sans-io: ソケット・スレッド・時計を持たない。時刻は `std::time::Instant` を引数で受け取る。
//!
//! - [`Message::parse`] は長さ・padding・属性の値の長さを厳格に検査し、壊れた入力は `Err` を返す。
//!   FINGERPRINT があれば解析時に検証する。理解必須（0x0000-0x7FFF）の未知属性は
//!   [`Message::unknown_comprehension_required`] に集め、要求なら 420 で応える（[`error_response`]）。
//! - [`MessageBuilder`] は属性を積み、MESSAGE-INTEGRITY・MESSAGE-INTEGRITY-SHA256・FINGERPRINT を最後に付ける。
//! - [`Message::verify_integrity`] は定数時間で比較する（仕様 9.3）。
//! - 鍵は [`auth`]（長期資格情報 MD5/SHA-256、短期資格情報、USERHASH）。
//! - 再送は [`rto`]（RFC 8489 6.2.1 の Rc/Rm、RFC 6298 の RTO 推定）。

pub mod attr;
pub mod auth;
mod crc32;
pub mod message;
pub mod rto;

pub use attr::{AddressFamily, Attribute, PasswordAlgorithm, attr_type};
pub use auth::{long_term_key, short_term_key, userhash};
pub use message::{
    Class, MAGIC_COOKIE, Message, MessageBuilder, Method, TransactionId, binding_success,
    error_response, is_stun, method,
};
pub use rto::{Retransmission, RetransmitAction, RtoConfig, RttEstimator};

use std::fmt;

/// STUN の処理で起きる誤り。秘密（鍵・パスワード）は含めない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StunError {
    /// ヘッダー（20 バイト）に満たない。
    TooShort,
    /// 先頭 2 bit・magic cookie が STUN でない。
    NotStun,
    /// メッセージ長が 4 の倍数でない・実際の長さと合わない。
    BadLength,
    /// 属性の長さ・値が壊れている。
    BadAttribute(u16),
    /// 属性の数が上限を超えた。
    TooManyAttributes,
    /// FINGERPRINT が一致しない・最後の属性でない。
    BadFingerprint,
    /// MESSAGE-INTEGRITY（-SHA256）が無い。
    NoIntegrity,
    /// MESSAGE-INTEGRITY（-SHA256）が一致しない。
    IntegrityMismatch,
    /// 組み立てたメッセージ・属性が長さの上限を超えた。
    TooLarge,
    /// 乱数源が使えない（仕様 9.3: 取得に失敗したら作らない）。
    Random,
}

impl fmt::Display for StunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StunError::TooShort => write!(f, "STUN メッセージが短すぎる"),
            StunError::NotStun => write!(f, "STUN メッセージではない"),
            StunError::BadLength => write!(f, "STUN メッセージ長が不正"),
            StunError::BadAttribute(t) => write!(f, "STUN 属性 0x{t:04x} が不正"),
            StunError::TooManyAttributes => write!(f, "STUN 属性が多すぎる"),
            StunError::BadFingerprint => write!(f, "FINGERPRINT が不正"),
            StunError::NoIntegrity => write!(f, "MESSAGE-INTEGRITY が無い"),
            StunError::IntegrityMismatch => write!(f, "MESSAGE-INTEGRITY が一致しない"),
            StunError::TooLarge => write!(f, "STUN メッセージが大きすぎる"),
            StunError::Random => write!(f, "乱数源が使えない"),
        }
    }
}

impl std::error::Error for StunError {}
