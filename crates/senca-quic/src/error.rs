//! QUIC のエラー（RFC 9000 20 章）。

use std::fmt;

use bytes::Bytes;

/// transport エラーコード。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransportErrorCode(pub u64);

impl TransportErrorCode {
    pub const NO_ERROR: Self = Self(0x0);
    pub const INTERNAL_ERROR: Self = Self(0x1);
    pub const CONNECTION_REFUSED: Self = Self(0x2);
    pub const FLOW_CONTROL_ERROR: Self = Self(0x3);
    pub const STREAM_LIMIT_ERROR: Self = Self(0x4);
    pub const STREAM_STATE_ERROR: Self = Self(0x5);
    pub const FINAL_SIZE_ERROR: Self = Self(0x6);
    pub const FRAME_ENCODING_ERROR: Self = Self(0x7);
    pub const TRANSPORT_PARAMETER_ERROR: Self = Self(0x8);
    pub const CONNECTION_ID_LIMIT_ERROR: Self = Self(0x9);
    pub const PROTOCOL_VIOLATION: Self = Self(0xa);
    pub const INVALID_TOKEN: Self = Self(0xb);
    pub const APPLICATION_ERROR: Self = Self(0xc);
    pub const CRYPTO_BUFFER_EXCEEDED: Self = Self(0xd);
    pub const KEY_UPDATE_ERROR: Self = Self(0xe);
    pub const AEAD_LIMIT_REACHED: Self = Self(0xf);
    pub const NO_VIABLE_PATH: Self = Self(0x10);

    /// TLS の alert を CRYPTO_ERROR（0x0100 + alert）にする。
    pub const fn crypto(alert: u8) -> Self {
        Self(0x100 + alert as u64)
    }

    pub fn name(self) -> &'static str {
        match self.0 {
            0x0 => "NO_ERROR",
            0x1 => "INTERNAL_ERROR",
            0x2 => "CONNECTION_REFUSED",
            0x3 => "FLOW_CONTROL_ERROR",
            0x4 => "STREAM_LIMIT_ERROR",
            0x5 => "STREAM_STATE_ERROR",
            0x6 => "FINAL_SIZE_ERROR",
            0x7 => "FRAME_ENCODING_ERROR",
            0x8 => "TRANSPORT_PARAMETER_ERROR",
            0x9 => "CONNECTION_ID_LIMIT_ERROR",
            0xa => "PROTOCOL_VIOLATION",
            0xb => "INVALID_TOKEN",
            0xc => "APPLICATION_ERROR",
            0xd => "CRYPTO_BUFFER_EXCEEDED",
            0xe => "KEY_UPDATE_ERROR",
            0xf => "AEAD_LIMIT_REACHED",
            0x10 => "NO_VIABLE_PATH",
            0x100..=0x1ff => "CRYPTO_ERROR",
            _ => "UNKNOWN",
        }
    }
}

impl fmt::Debug for TransportErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if (0x100..=0x1ff).contains(&self.0) {
            write!(f, "CRYPTO_ERROR(alert {})", self.0 - 0x100)
        } else {
            write!(f, "{}(0x{:x})", self.name(), self.0)
        }
    }
}

/// 自分が検出した transport エラー。CONNECTION_CLOSE（0x1c）で送る。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TransportError {
    pub code: TransportErrorCode,
    pub frame_type: u64,
    pub reason: String,
}

impl TransportError {
    pub(crate) fn new(code: TransportErrorCode, reason: impl Into<String>) -> Self {
        Self {
            code,
            frame_type: 0,
            reason: reason.into(),
        }
    }
    pub(crate) fn frame(mut self, ty: u64) -> Self {
        self.frame_type = ty;
        self
    }
}

macro_rules! terr {
    ($code:ident, $($arg:tt)*) => {
        $crate::error::TransportError::new($crate::error::TransportErrorCode::$code, format!($($arg)*))
    };
}
pub(crate) use terr;

/// 接続が終わった理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionError {
    /// 相手が v1 を話さない（Version Negotiation を受けた）。
    VersionMismatch,
    /// 自分が transport エラーを検出して閉じた。
    TransportError {
        code: TransportErrorCode,
        frame_type: u64,
        reason: String,
    },
    /// 相手が transport の CONNECTION_CLOSE（0x1c）で閉じた。
    ConnectionClosed {
        code: TransportErrorCode,
        frame_type: u64,
        reason: Bytes,
    },
    /// 相手がアプリケーションの CONNECTION_CLOSE（0x1d）で閉じた。
    ApplicationClosed { code: u64, reason: Bytes },
    /// stateless reset を受けた。
    Reset,
    /// idle timeout。
    TimedOut,
    /// 自分で閉じた（[`crate::Connection::close`]）。
    LocallyClosed,
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VersionMismatch => write!(f, "版の不一致（Version Negotiation）"),
            Self::TransportError { code, reason, .. } => write!(f, "transport エラー {code:?}: {reason}"),
            Self::ConnectionClosed { code, reason, .. } => {
                write!(f, "相手が閉じた {code:?}: {}", String::from_utf8_lossy(reason))
            }
            Self::ApplicationClosed { code, reason } => {
                write!(f, "相手のアプリケーションが閉じた 0x{code:x}: {}", String::from_utf8_lossy(reason))
            }
            Self::Reset => write!(f, "stateless reset"),
            Self::TimedOut => write!(f, "idle timeout"),
            Self::LocallyClosed => write!(f, "自分で閉じた"),
        }
    }
}

impl std::error::Error for ConnectionError {}

/// stream の操作の誤り。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamError {
    /// 存在しない（閉じた・まだ開いていない・方向が違う）stream。
    UnknownStream,
    /// 相手が STOP_SENDING で送信を止めた（アプリケーションエラーコード）。
    Stopped(u64),
    /// 相手が RESET_STREAM で受信を打ち切った（アプリケーションエラーコード）。
    Reset(u64),
    /// 既に FIN・リセットした stream への書き込み。
    Finished,
    /// 接続が閉じている。
    ConnectionClosed,
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for StreamError {}
