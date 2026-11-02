//! エラー型。制御 API のエラーコード（仕様 10.4）に対応付ける。
//!
//! メッセージには内部のポインター・秘密を入れない。

use std::fmt;

/// エラーの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// 機器・ライブラリ・符号化の枠が無い（503 `CAPACITY_UNAVAILABLE`）。
    /// 例: DeckLink のドライバーが無い、H.264 の符号化を求められたがハードウェアが無い。
    Unavailable,
    /// 指定の機器が無い（404 `NOT_FOUND`）。
    NotFound,
    /// 機器が使用中（409 `CONFLICT`）。
    Busy,
    /// 対応しない形式・コーデック（`UNSUPPORTED_CODEC`）。
    Unsupported,
    /// 方針で拒否（ロイヤリティフリー限定で H.264 を求めた等、REQ-007）。
    PolicyRejected,
    /// 設定値が不正（`INVALID_CONFIG`）。
    InvalidConfig,
    /// 機器から来たデータが壊れている。
    InvalidInput,
    /// ドライバー・SDK がエラーを返した（取り込みの途中で外れた等）。
    Device,
    /// 機器の入力が終わった（試験用の機器が指定の枚数を出し切った）。
    EndOfStream,
    /// 内部の不整合。
    Internal,
}

impl ErrorKind {
    /// 制御 API のエラーコード（仕様 10.4）。
    pub fn api_code(self) -> &'static str {
        match self {
            ErrorKind::Unavailable => "CAPACITY_UNAVAILABLE",
            ErrorKind::NotFound => "NOT_FOUND",
            ErrorKind::Busy => "CONFLICT",
            ErrorKind::Unsupported => "UNSUPPORTED_CODEC",
            ErrorKind::PolicyRejected | ErrorKind::InvalidConfig => "INVALID_CONFIG",
            ErrorKind::InvalidInput
            | ErrorKind::Device
            | ErrorKind::EndOfStream
            | ErrorKind::Internal => "INTERNAL_ERROR",
        }
    }

    /// HTTP の状態コードの目安。
    pub fn http_status(self) -> u16 {
        match self {
            ErrorKind::Unavailable => 503,
            ErrorKind::NotFound => 404,
            ErrorKind::Busy => 409,
            ErrorKind::Unsupported | ErrorKind::PolicyRejected | ErrorKind::InvalidConfig => 422,
            ErrorKind::InvalidInput
            | ErrorKind::Device
            | ErrorKind::EndOfStream
            | ErrorKind::Internal => 500,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Unavailable => "unavailable",
            ErrorKind::NotFound => "not_found",
            ErrorKind::Busy => "busy",
            ErrorKind::Unsupported => "unsupported",
            ErrorKind::PolicyRejected => "policy_rejected",
            ErrorKind::InvalidConfig => "invalid_config",
            ErrorKind::InvalidInput => "invalid_input",
            ErrorKind::Device => "device",
            ErrorKind::EndOfStream => "end_of_stream",
            ErrorKind::Internal => "internal",
        }
    }

    pub fn parse(s: &str) -> Option<ErrorKind> {
        Some(match s {
            "unavailable" => ErrorKind::Unavailable,
            "not_found" => ErrorKind::NotFound,
            "busy" => ErrorKind::Busy,
            "unsupported" => ErrorKind::Unsupported,
            "policy_rejected" => ErrorKind::PolicyRejected,
            "invalid_config" => ErrorKind::InvalidConfig,
            "invalid_input" => ErrorKind::InvalidInput,
            "device" => ErrorKind::Device,
            "end_of_stream" => ErrorKind::EndOfStream,
            "internal" => ErrorKind::Internal,
            _ => return None,
        })
    }
}

/// 取り込みのエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureError {
    pub kind: ErrorKind,
    pub message: String,
}

pub type Result<T> = std::result::Result<T, CaptureError>;

impl CaptureError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        CaptureError {
            kind,
            message: message.into(),
        }
    }
    pub fn unavailable(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unavailable, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, m)
    }
    pub fn unsupported(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, m)
    }
    pub fn invalid_config(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidConfig, m)
    }
    pub fn invalid_input(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, m)
    }
    pub fn device(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Device, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, m)
    }
    pub fn end_of_stream() -> Self {
        Self::new(ErrorKind::EndOfStream, "end of stream")
    }

    /// OS のエラー（errno）から。`EBUSY` は [`ErrorKind::Busy`]、`ENOENT`・`ENODEV` は [`ErrorKind::NotFound`]。
    pub fn from_io(what: &str, e: &std::io::Error) -> Self {
        let kind = match e.raw_os_error() {
            Some(libc::EBUSY) => ErrorKind::Busy,
            Some(libc::ENOENT) | Some(libc::ENODEV) | Some(libc::ENXIO) => ErrorKind::NotFound,
            Some(libc::EACCES) | Some(libc::EPERM) => ErrorKind::Unavailable,
            _ => ErrorKind::Device,
        };
        Self::new(kind, format!("{what}: {e}"))
    }
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

impl std::error::Error for CaptureError {}

impl From<senca_codec::CodecError> for CaptureError {
    fn from(e: senca_codec::CodecError) -> Self {
        use senca_codec::ErrorKind as K;
        let kind = match e.kind {
            K::Unavailable => ErrorKind::Unavailable,
            K::Unsupported => ErrorKind::Unsupported,
            K::PolicyRejected => ErrorKind::PolicyRejected,
            K::InvalidConfig => ErrorKind::InvalidConfig,
            K::InvalidInput => ErrorKind::InvalidInput,
            K::Backend => ErrorKind::Device,
            K::Internal => ErrorKind::Internal,
        };
        CaptureError::new(kind, format!("codec: {}", e.message))
    }
}

impl From<senca_image::ImageError> for CaptureError {
    fn from(e: senca_image::ImageError) -> Self {
        CaptureError::invalid_input(format!("image: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        assert_eq!(ErrorKind::Unavailable.api_code(), "CAPACITY_UNAVAILABLE");
        assert_eq!(ErrorKind::Unavailable.http_status(), 503);
        for k in [
            ErrorKind::Unavailable,
            ErrorKind::NotFound,
            ErrorKind::Busy,
            ErrorKind::Unsupported,
            ErrorKind::PolicyRejected,
            ErrorKind::InvalidConfig,
            ErrorKind::InvalidInput,
            ErrorKind::Device,
            ErrorKind::EndOfStream,
            ErrorKind::Internal,
        ] {
            assert_eq!(ErrorKind::parse(k.as_str()), Some(k));
        }
        let e = CaptureError::from_io("open", &std::io::Error::from_raw_os_error(libc::EBUSY));
        assert_eq!(e.kind, ErrorKind::Busy);
        let e: CaptureError = senca_codec::CodecError::unavailable("no hw").into();
        assert_eq!(e.kind.api_code(), "CAPACITY_UNAVAILABLE");
    }
}
