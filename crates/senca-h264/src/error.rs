//! エラー型。

use std::fmt;

/// 解析・組み立ての失敗。入力の中身（バイト列）は持たない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// 入力が途中で終わっている（Exp-Golomb 符号が長すぎる場合も含む）。
    Truncated,
    /// 値が仕様の範囲外、または構造が壊れている。
    Invalid(&'static str),
    /// 形式としては正しいが、この実装では扱わない。
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => f.write_str("H.264: データが途中で終わっている"),
            Error::Invalid(what) => write!(f, "H.264: 不正な値: {what}"),
            Error::Unsupported(what) => write!(f, "H.264: 未対応: {what}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
