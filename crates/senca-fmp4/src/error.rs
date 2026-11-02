use senca_core::Codec;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mp4Error {
    /// MP4 に載せられないコーデック。
    UnsupportedCodec(Codec),
    /// 設定レコードや必要な値（映像の幅・高さなど）が足りない・読めない。
    BadConfig(&'static str),
    /// 時刻が表せない（負の DTS、逆行、桁あふれ）。
    Timestamp(&'static str),
    /// 大きさの上限を超えた。
    TooLarge(&'static str),
    UnknownTrack(u32),
    DuplicateTrack(u32),
    /// 入力の箱構造が壊れている。
    Malformed(&'static str),
}

impl fmt::Display for Mp4Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mp4Error::UnsupportedCodec(c) => write!(f, "codec {c} is not supported in MP4"),
            Mp4Error::BadConfig(s) => write!(f, "bad codec config: {s}"),
            Mp4Error::Timestamp(s) => write!(f, "timestamp: {s}"),
            Mp4Error::TooLarge(s) => write!(f, "too large: {s}"),
            Mp4Error::UnknownTrack(t) => write!(f, "unknown track {t}"),
            Mp4Error::DuplicateTrack(t) => write!(f, "duplicate track {t}"),
            Mp4Error::Malformed(s) => write!(f, "malformed box structure: {s}"),
        }
    }
}

impl std::error::Error for Mp4Error {}
