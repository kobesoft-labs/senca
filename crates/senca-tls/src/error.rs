//! エラーと alert（RFC 8446 6 章）。

use std::fmt;

/// TLS の alert の説明値（RFC 8446 6 章）。未知の値もそのまま保持する。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AlertDescription(pub u8);

impl AlertDescription {
    pub const CLOSE_NOTIFY: Self = Self(0);
    pub const UNEXPECTED_MESSAGE: Self = Self(10);
    pub const BAD_RECORD_MAC: Self = Self(20);
    pub const RECORD_OVERFLOW: Self = Self(22);
    pub const HANDSHAKE_FAILURE: Self = Self(40);
    pub const BAD_CERTIFICATE: Self = Self(42);
    pub const UNSUPPORTED_CERTIFICATE: Self = Self(43);
    pub const CERTIFICATE_REVOKED: Self = Self(44);
    pub const CERTIFICATE_EXPIRED: Self = Self(45);
    pub const CERTIFICATE_UNKNOWN: Self = Self(46);
    pub const ILLEGAL_PARAMETER: Self = Self(47);
    pub const UNKNOWN_CA: Self = Self(48);
    pub const ACCESS_DENIED: Self = Self(49);
    pub const DECODE_ERROR: Self = Self(50);
    pub const DECRYPT_ERROR: Self = Self(51);
    pub const PROTOCOL_VERSION: Self = Self(70);
    pub const INSUFFICIENT_SECURITY: Self = Self(71);
    pub const INTERNAL_ERROR: Self = Self(80);
    pub const INAPPROPRIATE_FALLBACK: Self = Self(86);
    pub const USER_CANCELED: Self = Self(90);
    pub const MISSING_EXTENSION: Self = Self(109);
    pub const UNSUPPORTED_EXTENSION: Self = Self(110);
    pub const UNRECOGNIZED_NAME: Self = Self(112);
    pub const BAD_CERTIFICATE_STATUS_RESPONSE: Self = Self(113);
    pub const UNKNOWN_PSK_IDENTITY: Self = Self(115);
    pub const CERTIFICATE_REQUIRED: Self = Self(116);
    pub const NO_APPLICATION_PROTOCOL: Self = Self(120);

    /// 表示用の名前。
    pub fn name(self) -> &'static str {
        match self.0 {
            0 => "close_notify",
            10 => "unexpected_message",
            20 => "bad_record_mac",
            22 => "record_overflow",
            40 => "handshake_failure",
            42 => "bad_certificate",
            43 => "unsupported_certificate",
            44 => "certificate_revoked",
            45 => "certificate_expired",
            46 => "certificate_unknown",
            47 => "illegal_parameter",
            48 => "unknown_ca",
            49 => "access_denied",
            50 => "decode_error",
            51 => "decrypt_error",
            70 => "protocol_version",
            71 => "insufficient_security",
            80 => "internal_error",
            86 => "inappropriate_fallback",
            90 => "user_canceled",
            109 => "missing_extension",
            110 => "unsupported_extension",
            112 => "unrecognized_name",
            113 => "bad_certificate_status_response",
            115 => "unknown_psk_identity",
            116 => "certificate_required",
            120 => "no_application_protocol",
            _ => "unknown",
        }
    }
}

impl fmt::Debug for AlertDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.name(), self.0)
    }
}

impl fmt::Display for AlertDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// senca-tls のエラー。
///
/// 秘密（鍵・PSK・チケットの中身）は含めない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// 自分が検出した致命的な誤り。`alert` を相手へ送ってから接続を終える。
    Fatal {
        alert: AlertDescription,
        reason: &'static str,
    },
    /// 証明書の検証に失敗した（詳細は webpki のエラー表示）。
    Certificate {
        alert: AlertDescription,
        detail: String,
    },
    /// 相手から致命的な alert を受け取った。
    PeerAlert(AlertDescription),
    /// OS の乱数源が使えなかった（仕様 9.3: セッションを確立しない）。
    Rng,
    /// 設定・証明書・鍵の読み込みの誤り。
    Config(String),
    /// API の使い方の誤り（閉じた後の送信など）。
    InvalidState(&'static str),
}

impl Error {
    pub(crate) const fn fatal(alert: AlertDescription, reason: &'static str) -> Self {
        Self::Fatal { alert, reason }
    }
    pub(crate) const fn decode(reason: &'static str) -> Self {
        Self::fatal(AlertDescription::DECODE_ERROR, reason)
    }
    pub(crate) const fn illegal(reason: &'static str) -> Self {
        Self::fatal(AlertDescription::ILLEGAL_PARAMETER, reason)
    }
    pub(crate) const fn unexpected(reason: &'static str) -> Self {
        Self::fatal(AlertDescription::UNEXPECTED_MESSAGE, reason)
    }
    pub(crate) const fn internal(reason: &'static str) -> Self {
        Self::fatal(AlertDescription::INTERNAL_ERROR, reason)
    }

    /// 相手へ送るべき alert。相手の alert を受けた場合などは `None`。
    pub fn alert(&self) -> Option<AlertDescription> {
        match self {
            Self::Fatal { alert, .. } | Self::Certificate { alert, .. } => Some(*alert),
            Self::Rng => Some(AlertDescription::INTERNAL_ERROR),
            Self::PeerAlert(_) | Self::Config(_) | Self::InvalidState(_) => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fatal { alert, reason } => write!(f, "TLS エラー ({alert}): {reason}"),
            Self::Certificate { alert, detail } => write!(f, "証明書エラー ({alert}): {detail}"),
            Self::PeerAlert(a) => write!(f, "相手から alert を受信: {a}"),
            Self::Rng => write!(f, "乱数源が使えない"),
            Self::Config(s) => write!(f, "設定エラー: {s}"),
            Self::InvalidState(s) => write!(f, "状態エラー: {s}"),
        }
    }
}

impl std::error::Error for Error {}
