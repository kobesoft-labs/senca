//! 誤り。

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlsError {
    /// 文法違反。
    Syntax(&'static str),
    /// 属性の値が型に合わない（属性名）。
    Attribute(String),
    /// 必須の属性がない（属性名）。
    MissingAttribute(String),
    /// 必須の tag がない、または置き場所が違う（tag 名）。
    Tag(&'static str),
    /// 対応しない機能（暗号化など）。
    Unsupported(&'static str),
    /// 行の長さ・行数などの上限を超えた。
    TooLarge,
    /// 書き出す値が不正（CR/LF の混入など）。
    InvalidValue(&'static str),
}

impl fmt::Display for HlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HlsError::Syntax(r) => write!(f, "playlist syntax: {r}"),
            HlsError::Attribute(n) => write!(f, "invalid attribute {n}"),
            HlsError::MissingAttribute(n) => write!(f, "missing attribute {n}"),
            HlsError::Tag(t) => write!(f, "tag error: {t}"),
            HlsError::Unsupported(r) => write!(f, "unsupported: {r}"),
            HlsError::TooLarge => f.write_str("playlist too large"),
            HlsError::InvalidValue(r) => write!(f, "invalid value: {r}"),
        }
    }
}

impl std::error::Error for HlsError {}
