//! エラー型。認証情報・nonce・URL の userinfo は含めない。

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// 構文が壊れている。
    Malformed(&'static str),
    /// 上限（ヘッダー長・ヘッダー数・本文長・バッファ量・リダイレクト回数）を超えた。
    LimitExceeded(&'static str),
    /// 手順・状態の食い違い（CSeq・Session の不一致など）。
    Protocol(&'static str),
    /// 認証に失敗した、または受け入れられる方式がない。
    Auth(&'static str),
    /// 応答が期限内に来ない。
    Timeout,
    /// 今の状態では呼べない操作。
    InvalidState(&'static str),
    /// 未対応の形式。
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Malformed(w) => write!(f, "RTSP: 不正な形式: {w}"),
            Error::LimitExceeded(w) => write!(f, "RTSP: 上限超過: {w}"),
            Error::Protocol(w) => write!(f, "RTSP: 手順の誤り: {w}"),
            Error::Auth(w) => write!(f, "RTSP: 認証: {w}"),
            Error::Timeout => f.write_str("RTSP: 応答の期限切れ"),
            Error::InvalidState(w) => write!(f, "RTSP: 状態の誤り: {w}"),
            Error::Unsupported(w) => write!(f, "RTSP: 未対応: {w}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
