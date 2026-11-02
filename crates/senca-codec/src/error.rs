//! エラー型。制御 API のエラーコード（仕様 10.4）に対応付ける。
//!
//! メッセージには内部のポインター・スタック・秘密を入れない（仕様 10.4）。

use std::fmt;

/// エラーの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// 処理できる装置・ライブラリ・セッション枠が無い（503 `CAPACITY_UNAVAILABLE`）。
    /// 例: H.264 を要求されたがハードウェアが無い、NVENC の同時セッション上限。
    Unavailable,
    /// 対応しないコーデック・形式（`UNSUPPORTED_CODEC`）。例: AAC の符号化・復号。
    Unsupported,
    /// 方針で拒否（ロイヤリティフリー限定で H.264 を要求した等、REQ-007）。`INVALID_CONFIG` として返す。
    PolicyRejected,
    /// 設定値が不正（解像度 0、上限超過など）（`INVALID_CONFIG`）。
    InvalidConfig,
    /// 入力（AU・フレーム）が壊れている・上限を超えている。
    InvalidInput,
    /// 外部ライブラリ・ドライバーがエラーを返した。
    Backend,
    /// 呼び出し順序の誤りなど、内部の不整合。
    Internal,
}

impl ErrorKind {
    /// 制御 API のエラーコード（仕様 10.4）。
    pub fn api_code(self) -> &'static str {
        match self {
            ErrorKind::Unavailable => "CAPACITY_UNAVAILABLE",
            ErrorKind::Unsupported => "UNSUPPORTED_CODEC",
            ErrorKind::PolicyRejected | ErrorKind::InvalidConfig => "INVALID_CONFIG",
            ErrorKind::InvalidInput | ErrorKind::Backend | ErrorKind::Internal => "INTERNAL_ERROR",
        }
    }

    /// HTTP の状態コードの目安。
    pub fn http_status(self) -> u16 {
        match self {
            ErrorKind::Unavailable => 503,
            ErrorKind::Unsupported | ErrorKind::PolicyRejected | ErrorKind::InvalidConfig => 422,
            ErrorKind::InvalidInput => 422,
            ErrorKind::Backend | ErrorKind::Internal => 500,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Unavailable => "unavailable",
            ErrorKind::Unsupported => "unsupported",
            ErrorKind::PolicyRejected => "policy_rejected",
            ErrorKind::InvalidConfig => "invalid_config",
            ErrorKind::InvalidInput => "invalid_input",
            ErrorKind::Backend => "backend",
            ErrorKind::Internal => "internal",
        }
    }

    pub fn from_u8(v: u8) -> Option<ErrorKind> {
        Some(match v {
            0 => ErrorKind::Unavailable,
            1 => ErrorKind::Unsupported,
            2 => ErrorKind::PolicyRejected,
            3 => ErrorKind::InvalidConfig,
            4 => ErrorKind::InvalidInput,
            5 => ErrorKind::Backend,
            6 => ErrorKind::Internal,
            _ => return None,
        })
    }

    pub fn to_u8(self) -> u8 {
        match self {
            ErrorKind::Unavailable => 0,
            ErrorKind::Unsupported => 1,
            ErrorKind::PolicyRejected => 2,
            ErrorKind::InvalidConfig => 3,
            ErrorKind::InvalidInput => 4,
            ErrorKind::Backend => 5,
            ErrorKind::Internal => 6,
        }
    }
}

/// コーデック処理のエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError {
    pub kind: ErrorKind,
    pub message: String,
}

impl CodecError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        CodecError {
            kind,
            message: message.into(),
        }
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unavailable, message)
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, message)
    }
    pub fn policy(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::PolicyRejected, message)
    }
    pub fn invalid_config(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidConfig, message)
    }
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, message)
    }
    pub fn backend(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Backend, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

impl std::error::Error for CodecError {}

pub type Result<T> = std::result::Result<T, CodecError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        assert_eq!(ErrorKind::Unavailable.api_code(), "CAPACITY_UNAVAILABLE");
        assert_eq!(ErrorKind::Unavailable.http_status(), 503);
        assert_eq!(ErrorKind::Unsupported.api_code(), "UNSUPPORTED_CODEC");
        for v in 0..=6u8 {
            assert_eq!(ErrorKind::from_u8(v).unwrap().to_u8(), v);
        }
        assert_eq!(ErrorKind::from_u8(7), None);
    }
}
