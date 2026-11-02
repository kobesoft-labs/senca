//! 誤り。外部サービスの応答から取った文は短く切り、API キーは入れない。

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// 設定の値が不正。
    InvalidConfig(&'static str),
    /// 渡した値が不正（順序の違反、空の入力など）。
    InvalidInput(&'static str),
    /// 対応しない（コーデック・形式）。復号が要る変換は常にこれ（ADR-0004）。
    Unsupported(&'static str),
    /// 入れ物（ADTS・MP4・Ogg・HLS）を作れない。
    Container(String),
    /// 大きさの上限を超えた。
    TooLarge(&'static str),
    /// 相手が 2xx 以外を返した。`message` は相手の誤りの文（短く切ったもの）。
    Status { status: u16, message: String },
    /// 応答の形が読めない。
    Malformed(String),
    /// HTTP の組み立て・解析の誤り。
    Http(String),
    /// 相手が拒否した（翻訳の refusal・content filter など）。
    Refused(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidConfig(s) => write!(f, "invalid config: {s}"),
            Error::InvalidInput(s) => write!(f, "invalid input: {s}"),
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::Container(s) => write!(f, "container: {s}"),
            Error::TooLarge(s) => write!(f, "too large: {s}"),
            Error::Status { status, message } => write!(f, "remote returned {status}: {message}"),
            Error::Malformed(s) => write!(f, "malformed response: {s}"),
            Error::Http(s) => write!(f, "http: {s}"),
            Error::Refused(s) => write!(f, "refused: {s}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

impl From<senca_aac::Error> for Error {
    fn from(e: senca_aac::Error) -> Self {
        Error::Container(e.to_string())
    }
}

impl From<senca_opus::Error> for Error {
    fn from(e: senca_opus::Error) -> Self {
        Error::Container(e.to_string())
    }
}

impl From<senca_fmp4::Mp4Error> for Error {
    fn from(e: senca_fmp4::Mp4Error) -> Self {
        Error::Container(e.to_string())
    }
}

impl From<senca_hls::HlsError> for Error {
    fn from(e: senca_hls::HlsError) -> Self {
        Error::Container(e.to_string())
    }
}

impl From<senca_http::HttpError> for Error {
    fn from(e: senca_http::HttpError) -> Self {
        Error::Http(e.to_string())
    }
}

/// 外部の文を誤りに入れる前に短くし、制御文字を除く。
pub(crate) fn clip_message(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().filter(|c| !c.is_control()).take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}
