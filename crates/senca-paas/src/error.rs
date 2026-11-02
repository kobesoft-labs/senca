//! 連携先の誤りと、再試行してよいかの分類。

use std::fmt;

/// 誤りの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 一時的（5xx・429・408・throttling・通信の失敗）。同じ要求を間を置いて再試行してよい。
    Retryable,
    /// 対象が無い（404）。
    NotFound,
    /// 状態の衝突（409。使用中の資源の削除など）。状況が変わるまで再試行しても同じ。
    Conflict,
    /// 認証・認可の失敗（401・403）。資格情報を直すまで再試行しない。
    Unauthorized,
    /// 要求が受け付けられない（400・422 など）。
    Invalid,
    /// 応答の形が想定と違う。
    Malformed,
    /// 設定（options・資格情報の形）の誤り。
    Config,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Retryable => "retryable",
            ErrorKind::NotFound => "not_found",
            ErrorKind::Conflict => "conflict",
            ErrorKind::Unauthorized => "unauthorized",
            ErrorKind::Invalid => "invalid",
            ErrorKind::Malformed => "malformed",
            ErrorKind::Config => "config",
        }
    }
}

/// 連携先の誤り。`message` には秘密を入れない（連携先のメッセージは短く切り、制御文字を除いて入れる）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaasError {
    pub kind: ErrorKind,
    /// HTTP の状態コード（応答があったとき）。
    pub status: Option<u16>,
    /// 連携先の誤りコード（AWS の `__type`、Cloudflare の `errors[].code`）。
    pub code: Option<String>,
    pub message: String,
}

impl PaasError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        PaasError { kind, status: None, code: None, message: sanitize(&message.into()) }
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, message)
    }

    pub fn malformed(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Malformed, message)
    }

    /// 通信の失敗・タイムアウト（呼び出し側の送信関数が使う）。
    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Retryable, message)
    }

    /// 同じ要求を再試行してよいか。
    pub fn is_retryable(&self) -> bool {
        self.kind == ErrorKind::Retryable
    }

    pub(crate) fn with_kind(mut self, kind: ErrorKind) -> Self {
        self.kind = kind;
        self
    }
}

impl fmt::Display for PaasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.as_str())?;
        if let Some(s) = self.status {
            write!(f, " (HTTP {s})")?;
        }
        if let Some(c) = &self.code {
            write!(f, " {c}")?;
        }
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for PaasError {}

/// HTTP の状態コードから種類を決める。
pub fn classify_status(status: u16) -> ErrorKind {
    match status {
        401 | 403 => ErrorKind::Unauthorized,
        404 | 410 => ErrorKind::NotFound,
        408 | 425 | 429 => ErrorKind::Retryable,
        409 => ErrorKind::Conflict,
        500..=599 => ErrorKind::Retryable,
        _ => ErrorKind::Invalid,
    }
}

/// AWS の誤りコードから種類を決める（状態コードより優先する）。
pub(crate) fn classify_aws_code(code: &str) -> Option<ErrorKind> {
    Some(match code {
        "ThrottlingException" | "TooManyRequestsException" | "ServiceUnavailableException" | "InternalServerException"
        | "InternalServerErrorException" | "InternalFailureException" | "RequestTimeoutException" | "GatewayTimeoutException"
        | "BadGatewayException" => ErrorKind::Retryable,
        "ResourceNotFoundException" | "NotFoundException" => ErrorKind::NotFound,
        "ConflictException" => ErrorKind::Conflict,
        "AccessDeniedException" | "UnrecognizedClientException" | "InvalidSignatureException" | "ForbiddenException"
        | "ExpiredTokenException" | "IncompleteSignatureException" | "MissingAuthenticationTokenException" => ErrorKind::Unauthorized,
        "ValidationException" | "BadRequestException" | "ServiceQuotaExceededException" | "UnprocessableEntityException" => {
            ErrorKind::Invalid
        }
        _ => return None,
    })
}

/// 連携先のメッセージを短くし、制御文字を除く。
pub(crate) fn sanitize(s: &str) -> String {
    const MAX: usize = 200;
    let mut out = String::new();
    for c in s.chars() {
        if out.len() >= MAX {
            out.push('…');
            break;
        }
        out.push(if c.is_control() { ' ' } else { c });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping() {
        assert_eq!(classify_status(503), ErrorKind::Retryable);
        assert_eq!(classify_status(429), ErrorKind::Retryable);
        assert_eq!(classify_status(403), ErrorKind::Unauthorized);
        assert_eq!(classify_status(404), ErrorKind::NotFound);
        assert_eq!(classify_status(400), ErrorKind::Invalid);
        assert_eq!(classify_aws_code("ThrottlingException"), Some(ErrorKind::Retryable));
        assert_eq!(classify_aws_code("Whatever"), None);
    }

    #[test]
    fn sanitize_truncates_and_strips_controls() {
        let s = sanitize(&format!("a\r\nb{}", "x".repeat(500)));
        assert!(s.starts_with("a  b"));
        assert!(s.chars().count() <= 202);
        let e = PaasError::config("bad");
        assert_eq!(e.to_string(), "config: bad");
        assert!(!e.is_retryable());
        assert!(PaasError::transport("timeout").is_retryable());
    }
}
