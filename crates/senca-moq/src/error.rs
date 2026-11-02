//! MoQ のエラーコード（draft-22 12 章）。

use std::fmt;

/// セッションを閉じるコード（12.2）。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionErrorCode(pub u64);

impl SessionErrorCode {
    pub const NO_ERROR: Self = Self(0x0);
    pub const INTERNAL_ERROR: Self = Self(0x1);
    pub const UNAUTHORIZED: Self = Self(0x2);
    pub const PROTOCOL_VIOLATION: Self = Self(0x3);
    pub const INVALID_REQUEST_ID: Self = Self(0x4);
    pub const DUPLICATE_TRACK_ALIAS: Self = Self(0x5);
    pub const KEY_VALUE_FORMATTING_ERROR: Self = Self(0x6);
    pub const INVALID_PATH: Self = Self(0x8);
    pub const MALFORMED_PATH: Self = Self(0x9);
    pub const GOAWAY_TIMEOUT: Self = Self(0x10);
    pub const CONTROL_MESSAGE_TIMEOUT: Self = Self(0x11);
    pub const DATA_STREAM_TIMEOUT: Self = Self(0x12);
    pub const AUTH_TOKEN_CACHE_OVERFLOW: Self = Self(0x13);
    pub const DUPLICATE_AUTH_TOKEN_ALIAS: Self = Self(0x14);
    pub const MALFORMED_AUTH_TOKEN: Self = Self(0x16);
    pub const UNKNOWN_AUTH_TOKEN_ALIAS: Self = Self(0x17);
    pub const EXPIRED_AUTH_TOKEN: Self = Self(0x18);
    pub const INVALID_AUTHORITY: Self = Self(0x19);
    pub const MALFORMED_AUTHORITY: Self = Self(0x1a);
    pub const TOO_MANY_REQUEST_UPDATES: Self = Self(0x1b);
}

impl fmt::Debug for SessionErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionErrorCode(0x{:x})", self.0)
    }
}

/// REQUEST_ERROR のコード（12.3、16.11.2）。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestErrorCode(pub u64);

impl RequestErrorCode {
    pub const INTERNAL_ERROR: Self = Self(0x0);
    pub const UNAUTHORIZED: Self = Self(0x1);
    pub const TIMEOUT: Self = Self(0x2);
    pub const NOT_SUPPORTED: Self = Self(0x3);
    pub const MALFORMED_AUTH_TOKEN: Self = Self(0x4);
    pub const EXPIRED_AUTH_TOKEN: Self = Self(0x5);
    pub const GOING_AWAY: Self = Self(0x6);
    pub const EXCESSIVE_LOAD: Self = Self(0x9);
    pub const DOES_NOT_EXIST: Self = Self(0x10);
    pub const INVALID_RANGE: Self = Self(0x11);
    pub const MALFORMED_TRACK: Self = Self(0x12);
    pub const UNINTERESTED: Self = Self(0x20);
    pub const PREFIX_OVERLAP: Self = Self(0x30);
    pub const NAMESPACE_TOO_LARGE: Self = Self(0x31);
    pub const UNSUPPORTED_EXTENSION: Self = Self(0x33);
    pub const REDIRECT: Self = Self(0x34);
    pub const CONFLICTING_FILTERS: Self = Self(0x35);
    pub const INVALID_FILTER: Self = Self(0x36);
}

impl fmt::Debug for RequestErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RequestErrorCode(0x{:x})", self.0)
    }
}

/// PUBLISH_DONE の status（12.4）。
pub mod publish_done {
    pub const INTERNAL_ERROR: u64 = 0x0;
    pub const UNAUTHORIZED: u64 = 0x1;
    pub const TRACK_ENDED: u64 = 0x2;
    pub const GOING_AWAY: u64 = 0x4;
    pub const TOO_FAR_BEHIND: u64 = 0x5;
    pub const EXPIRED: u64 = 0x6;
    pub const UPDATE_FAILED: u64 = 0x8;
    pub const EXCESSIVE_LOAD: u64 = 0x9;
    pub const MALFORMED_TRACK: u64 = 0x12;
}

/// stream のリセットのコード（12.5）。
pub mod reset {
    pub const INTERNAL_ERROR: u64 = 0x0;
    pub const CANCELLED: u64 = 0x1;
    pub const DELIVERY_TIMEOUT: u64 = 0x2;
    pub const SESSION_CLOSED: u64 = 0x3;
    pub const GOING_AWAY: u64 = 0x4;
    pub const TOO_FAR_BEHIND: u64 = 0x5;
    pub const UNKNOWN_OBJECT_STATUS: u64 = 0x6;
    pub const EXPIRED_AUTH_TOKEN: u64 = 0x7;
    pub const EXCESSIVE_LOAD: u64 = 0x9;
    pub const MALFORMED_TRACK: u64 = 0x12;
}

/// セッションを閉じる誤り。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoqError {
    pub code: SessionErrorCode,
    pub reason: String,
}

impl MoqError {
    pub(crate) fn new(code: SessionErrorCode, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
    pub(crate) fn proto(reason: impl Into<String>) -> Self {
        Self::new(SessionErrorCode::PROTOCOL_VIOLATION, reason)
    }
}

impl fmt::Display for MoqError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.reason)
    }
}

impl std::error::Error for MoqError {}
