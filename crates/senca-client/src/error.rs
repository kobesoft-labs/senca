//! 誤り。API の誤り（problem+json）は [`ApiError`]、それ以外（接続・時間切れ・解析）は [`Error`] の他の枝。

use serde_json::Value;
use std::fmt;
use std::time::Duration;

use crate::models::ProblemCode;

/// クライアントの誤り。
#[derive(Debug, Clone)]
pub enum Error {
    /// API が誤りを返した（4xx・5xx）。
    Api(Box<ApiError>),
    /// 接続できなかった（要求はサーバーに届いていない）。
    Connect(String),
    /// 送受信の途中の誤り（要求が届いたかは分からない）。
    Transport(String),
    /// 時間切れ。
    Timeout,
    /// 応答の本文を読めない（型が合わない）。
    Decode(String),
    /// URL の形が違う。
    InvalidUrl(String),
    /// 引数の誤り（送る前に分かったもの）。
    InvalidArgument(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// API の誤りなら取り出す。
    pub fn api(&self) -> Option<&ApiError> {
        match self {
            Error::Api(e) => Some(e),
            _ => None,
        }
    }

    /// API の誤りの `code`（それ以外は `None`）。
    pub fn code(&self) -> Option<ProblemCode> {
        self.api().map(|e| e.code)
    }

    /// HTTP の状態コード（API の誤りだけ）。
    pub fn status(&self) -> Option<u16> {
        self.api().map(|e| e.status)
    }

    /// 送り直してよいか（サーバーが `retryable` と言った、429・502・503・504、接続できなかった、時間切れ）。
    /// 送り直す前に、要求が冪等か（GET・DELETE・Idempotency-Key 付きの POST）を確かめること。
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Api(e) => e.is_retryable(),
            Error::Connect(_) | Error::Timeout | Error::Transport(_) => true,
            Error::Decode(_) | Error::InvalidUrl(_) | Error::InvalidArgument(_) => false,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Api(e) => e.fmt(f),
            Error::Connect(m) => write!(f, "接続できません: {m}"),
            Error::Transport(m) => write!(f, "送受信の誤り: {m}"),
            Error::Timeout => write!(f, "時間切れ"),
            Error::Decode(m) => write!(f, "応答を読めません: {m}"),
            Error::InvalidUrl(u) => write!(f, "URL の形が違います: {u}"),
            Error::InvalidArgument(m) => write!(f, "引数の誤り: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<ApiError> for Error {
    fn from(e: ApiError) -> Self {
        Error::Api(Box::new(e))
    }
}

/// API の誤り（RFC 9457 の problem details、`application/problem+json`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: u16,
    /// 誤りの種類（知らない値・problem でない応答は `Unknown`。元の文字列は [`ApiError::raw_code`]）。
    pub code: ProblemCode,
    /// 本文の `code` の文字列（problem でなければ空）。
    pub raw_code: String,
    /// `urn:senca:error:...`。
    pub problem_type: String,
    /// 説明（日本語）。
    pub title: String,
    /// サーバーが送り直してよいと言ったか。
    pub retryable: bool,
    /// `X-Request-Id`（本文か応答ヘッダー）。
    pub request_id: Option<String>,
    /// code ごとの補足。
    pub details: Option<Value>,
    /// `Retry-After`。
    pub retry_after: Option<Duration>,
}

impl ApiError {
    /// 応答から作る（problem+json でなければ状態コードと本文の先頭から）。
    pub fn from_response(status: u16, headers: &[(String, String)], body: &[u8]) -> ApiError {
        let h = |n: &str| crate::transport::header(headers, n);
        let retry_after = h("retry-after").and_then(parse_retry_after);
        let header_rid = h("x-request-id").map(str::to_string);
        let v: Option<Value> = serde_json::from_slice(body).ok();
        let obj = v.as_ref().and_then(Value::as_object);
        let s = |k: &str| obj.and_then(|o| o.get(k)).and_then(Value::as_str).map(str::to_string);
        let raw_code = s("code").unwrap_or_default();
        let code = serde_json::from_value(Value::String(raw_code.clone())).unwrap_or(ProblemCode::Unknown);
        let title = s("title").unwrap_or_else(|| {
            let t = String::from_utf8_lossy(&body[..body.len().min(200)]).trim().to_string();
            if t.is_empty() { format!("HTTP {status}") } else { t }
        });
        ApiError {
            status,
            code,
            raw_code,
            problem_type: s("type").unwrap_or_default(),
            title,
            retryable: obj.and_then(|o| o.get("retryable")).and_then(Value::as_bool).unwrap_or(false),
            request_id: s("request_id").or(header_rid),
            details: obj.and_then(|o| o.get("details")).cloned().filter(|d| !d.is_null()),
            retry_after,
        }
    }

    /// 送り直してよいか（`retryable`、または 429・502・503・504）。
    pub fn is_retryable(&self) -> bool {
        self.retryable || matches!(self.status, 429 | 502 | 503 | 504)
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {} {}: {}", self.status, if self.raw_code.is_empty() { "-" } else { &self.raw_code }, self.title)?;
        if let Some(r) = &self.request_id {
            write!(f, " (request {r})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

/// `Retry-After`（秒の数だけ。HTTP 日付は読まない）。
fn parse_retry_after(v: &str) -> Option<Duration> {
    v.trim().parse::<u64>().ok().map(|s| Duration::from_secs(s.min(3600)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problem_json() {
        let body = r#"{"type":"urn:senca:error:precondition-failed","title":"古い版です","status":412,"code":"PRECONDITION_FAILED","retryable":false,"request_id":"req_1","details":{"current":"\"3\""}}"#.as_bytes();
        let e = ApiError::from_response(412, &[], body);
        assert_eq!(e.code, ProblemCode::PreconditionFailed);
        assert_eq!(e.title, "古い版です");
        assert_eq!(e.request_id.as_deref(), Some("req_1"));
        assert!(e.details.is_some());
        assert!(!e.is_retryable());
        assert_eq!(e.to_string(), "HTTP 412 PRECONDITION_FAILED: 古い版です (request req_1)");
    }

    #[test]
    fn non_problem_and_unknown_code() {
        let h = vec![("Retry-After".to_string(), "2".to_string()), ("X-Request-Id".to_string(), "abc".to_string())];
        let e = ApiError::from_response(503, &h, b"busy");
        assert_eq!((e.code, e.title.as_str(), e.request_id.as_deref()), (ProblemCode::Unknown, "busy", Some("abc")));
        assert_eq!(e.retry_after, Some(Duration::from_secs(2)));
        assert!(e.is_retryable());
        let e = ApiError::from_response(500, &[], br#"{"code":"SOMETHING_NEW","title":"x","retryable":true}"#);
        assert_eq!((e.code, e.raw_code.as_str()), (ProblemCode::Unknown, "SOMETHING_NEW"));
        assert!(e.is_retryable());
        assert!(!Error::from(ApiError::from_response(400, &[], b"")).is_retryable());
        assert!(Error::Connect("x".into()).is_retryable());
    }
}
