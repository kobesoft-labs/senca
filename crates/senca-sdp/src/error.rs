//! エラー型。入力の中身（文字列）は持たない（秘密や巨大な値をログに出さないため）。

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// 全体が上限（バイト数）を超える。
    TooLarge { limit: usize },
    /// 行数・media 数・属性数・行の長さなどの上限を超える。
    Limit(&'static str),
    /// UTF-8 ではない。
    NotUtf8,
    /// 行の形式が正しくない（行番号は 1 から）。
    Syntax { line: usize, what: &'static str },
    /// 行の順序・重複が RFC 8866 の文法に合わない。
    Order { line: usize, what: &'static str },
    /// 必須の行がない。
    Missing(&'static str),
    /// 属性の値が正しくない。
    Invalid(&'static str),
    /// offer/answer の交渉が成立しない。
    Negotiation(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooLarge { limit } => write!(f, "SDP: 大きさが上限 {limit} バイトを超える"),
            Error::Limit(what) => write!(f, "SDP: 上限を超える: {what}"),
            Error::NotUtf8 => f.write_str("SDP: UTF-8 ではない"),
            Error::Syntax { line, what } => write!(f, "SDP: {line} 行目の形式が不正: {what}"),
            Error::Order { line, what } => write!(f, "SDP: {line} 行目の順序が不正: {what}"),
            Error::Missing(what) => write!(f, "SDP: 必須の行がない: {what}"),
            Error::Invalid(what) => write!(f, "SDP: 不正な値: {what}"),
            Error::Negotiation(what) => write!(f, "SDP: 交渉できない: {what}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
