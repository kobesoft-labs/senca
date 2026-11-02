//! HTTP/3・QPACK のエラーコード（RFC 9114 8.1、RFC 9204 6）と WebTransport の写像。

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct H3ErrorCode(pub u64);

impl H3ErrorCode {
    pub const NO_ERROR: Self = Self(0x100);
    pub const GENERAL_PROTOCOL_ERROR: Self = Self(0x101);
    pub const INTERNAL_ERROR: Self = Self(0x102);
    pub const STREAM_CREATION_ERROR: Self = Self(0x103);
    pub const CLOSED_CRITICAL_STREAM: Self = Self(0x104);
    pub const FRAME_UNEXPECTED: Self = Self(0x105);
    pub const FRAME_ERROR: Self = Self(0x106);
    pub const EXCESSIVE_LOAD: Self = Self(0x107);
    pub const ID_ERROR: Self = Self(0x108);
    pub const SETTINGS_ERROR: Self = Self(0x109);
    pub const MISSING_SETTINGS: Self = Self(0x10a);
    pub const REQUEST_REJECTED: Self = Self(0x10b);
    pub const REQUEST_CANCELLED: Self = Self(0x10c);
    pub const REQUEST_INCOMPLETE: Self = Self(0x10d);
    pub const MESSAGE_ERROR: Self = Self(0x10e);
    pub const CONNECT_ERROR: Self = Self(0x10f);
    pub const VERSION_FALLBACK: Self = Self(0x110);
    pub const QPACK_DECOMPRESSION_FAILED: Self = Self(0x200);
    pub const QPACK_ENCODER_STREAM_ERROR: Self = Self(0x201);
    pub const QPACK_DECODER_STREAM_ERROR: Self = Self(0x202);
    /// RFC 9297 5.2
    pub const DATAGRAM_ERROR: Self = Self(0x33);
    /// WebTransport draft-07
    pub const WEBTRANSPORT_BUFFERED_STREAM_REJECTED: Self = Self(0x3994_bd84);
    pub const WEBTRANSPORT_SESSION_GONE: Self = Self(0x170d_7b68);
}

impl fmt::Debug for H3ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "H3ErrorCode(0x{:x})", self.0)
    }
}

/// 接続を閉じる HTTP/3 のエラー。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct H3Error {
    pub code: H3ErrorCode,
    pub reason: String,
}

impl H3Error {
    pub(crate) fn new(code: H3ErrorCode, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for H3Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.reason)
    }
}

impl std::error::Error for H3Error {}

macro_rules! h3err {
    ($code:ident, $($arg:tt)*) => {
        $crate::error::H3Error::new($crate::error::H3ErrorCode::$code, format!($($arg)*))
    };
}
pub(crate) use h3err;

const WT_FIRST: u64 = 0x52e4_a40f_a8db;
const WT_LAST: u64 = 0x52e5_ac98_3162;

/// WebTransport のアプリケーションエラー（32 bit）を HTTP/3 のエラーコードへ（draft-07 4.3）。
pub fn wt_error_to_h3(n: u32) -> u64 {
    let n = u64::from(n);
    WT_FIRST + n + n / 0x1e
}

/// HTTP/3 のエラーコードを WebTransport のアプリケーションエラーへ。範囲外・予約値は `None`。
pub fn h3_to_wt_error(h: u64) -> Option<u32> {
    if !(WT_FIRST..=WT_LAST).contains(&h) || (h - 0x21).is_multiple_of(0x1f) {
        return None;
    }
    let shifted = h - WT_FIRST;
    u32::try_from(shifted - shifted / 0x1f).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wt_error_mapping_roundtrip() {
        assert_eq!(wt_error_to_h3(0), WT_FIRST);
        assert_eq!(wt_error_to_h3(u32::MAX), WT_LAST);
        for n in [0u32, 1, 0x1d, 0x1e, 0x1f, 1000, 0xffff, u32::MAX] {
            let h = wt_error_to_h3(n);
            assert_ne!((h - 0x21) % 0x1f, 0, "{n} maps onto a reserved codepoint");
            assert_eq!(h3_to_wt_error(h), Some(n));
        }
        assert_eq!(h3_to_wt_error(0x100), None);
    }
}
