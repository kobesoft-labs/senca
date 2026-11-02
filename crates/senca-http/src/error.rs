//! 誤りの種類と、応答に使う状態コードの対応。

use std::fmt;

/// 解析・生成の誤り。どれも致命的で、受け取った側は応答を返して接続を閉じる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// 文法違反（400）。理由は固定の短い文字列（入力の値は含めない）。
    Malformed(&'static str),
    /// request line が長すぎる（414）。
    UriTooLong,
    /// header の合計・個数・1 行が上限を超えた（431）。
    HeadersTooLarge,
    /// body が上限を超えた（413）。
    BodyTooLarge,
    /// HTTP/1.0・1.1 以外（505）。
    UnsupportedVersion,
    /// 対応しない Transfer-Encoding など（501）。
    NotImplemented(&'static str),
    /// 100-continue 以外の Expect（417）。
    ExpectationFailed,
    /// 通信の途中で相手が閉じた。
    Incomplete,
    /// 生成側に渡した値が不正（CR/LF の混入、禁止 header など）。応答の状態コードはない。
    InvalidInput(&'static str),
}

impl HttpError {
    /// 相手に返す状態コード。
    pub fn status(&self) -> u16 {
        match self {
            HttpError::Malformed(_) | HttpError::Incomplete => 400,
            HttpError::UriTooLong => 414,
            HttpError::HeadersTooLarge => 431,
            HttpError::BodyTooLarge => 413,
            HttpError::UnsupportedVersion => 505,
            HttpError::NotImplemented(_) => 501,
            HttpError::ExpectationFailed => 417,
            HttpError::InvalidInput(_) => 500,
        }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::Malformed(r) => write!(f, "malformed message: {r}"),
            HttpError::UriTooLong => f.write_str("request line too long"),
            HttpError::HeadersTooLarge => f.write_str("header section too large"),
            HttpError::BodyTooLarge => f.write_str("body too large"),
            HttpError::UnsupportedVersion => f.write_str("unsupported HTTP version"),
            HttpError::NotImplemented(r) => write!(f, "not implemented: {r}"),
            HttpError::ExpectationFailed => f.write_str("expectation failed"),
            HttpError::Incomplete => f.write_str("connection closed mid-message"),
            HttpError::InvalidInput(r) => write!(f, "invalid input: {r}"),
        }
    }
}

impl std::error::Error for HttpError {}
