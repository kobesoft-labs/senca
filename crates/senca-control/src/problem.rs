//! 仕様 10.4 のエラー（application/problem+json）。内部の詳細や秘密は入れない。

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProblemCode {
    InvalidConfig,
    InvalidRequest,
    UnsupportedProtocolVersion,
    UnsupportedCodec,
    UnsupportedMediaType,
    AuthFailed,
    Forbidden,
    NotFound,
    MethodNotAllowed,
    Conflict,
    IdempotencyConflict,
    PreconditionFailed,
    PreconditionRequired,
    QuotaExceeded,
    RateLimited,
    CapacityUnavailable,
    InputTimeout,
    OwnershipLost,
    PayloadTooLarge,
    InternalError,
}

impl ProblemCode {
    pub fn default_status(self) -> u16 {
        use ProblemCode::*;
        match self {
            InvalidConfig | InvalidRequest => 400,
            UnsupportedProtocolVersion | UnsupportedCodec => 422,
            UnsupportedMediaType => 415,
            AuthFailed => 401,
            Forbidden => 403,
            NotFound => 404,
            MethodNotAllowed => 405,
            Conflict | IdempotencyConflict => 409,
            PreconditionFailed => 412,
            PreconditionRequired => 428,
            QuotaExceeded => 403,
            RateLimited => 429,
            CapacityUnavailable => 503,
            InputTimeout => 504,
            OwnershipLost => 409,
            PayloadTooLarge => 413,
            InternalError => 500,
        }
    }

    pub fn slug(self) -> &'static str {
        use ProblemCode::*;
        match self {
            InvalidConfig => "invalid-config",
            InvalidRequest => "invalid-request",
            UnsupportedProtocolVersion => "unsupported-protocol-version",
            UnsupportedCodec => "unsupported-codec",
            UnsupportedMediaType => "unsupported-media-type",
            AuthFailed => "auth-failed",
            Forbidden => "forbidden",
            NotFound => "not-found",
            MethodNotAllowed => "method-not-allowed",
            Conflict => "conflict",
            IdempotencyConflict => "idempotency-conflict",
            PreconditionFailed => "precondition-failed",
            PreconditionRequired => "precondition-required",
            QuotaExceeded => "quota-exceeded",
            RateLimited => "rate-limited",
            CapacityUnavailable => "capacity-unavailable",
            InputTimeout => "input-timeout",
            OwnershipLost => "ownership-lost",
            PayloadTooLarge => "payload-too-large",
            InternalError => "internal-error",
        }
    }

    fn retryable(self) -> bool {
        matches!(
            self,
            ProblemCode::RateLimited | ProblemCode::CapacityUnavailable | ProblemCode::InputTimeout | ProblemCode::InternalError
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Problem {
    #[serde(rename = "type")]
    pub type_uri: String,
    pub title: String,
    pub status: u16,
    pub code: ProblemCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub details: Value,
}

impl Problem {
    pub fn new(code: ProblemCode, title: impl Into<String>) -> Self {
        Problem {
            type_uri: format!("urn:senca:error:{}", code.slug()),
            title: title.into(),
            status: code.default_status(),
            code,
            request_id: None,
            retryable: code.retryable(),
            details: Value::Null,
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    pub fn with_retryable(mut self, r: bool) -> Self {
        self.retryable = r;
        self
    }

    pub fn not_found() -> Self {
        Problem::new(ProblemCode::NotFound, "見つかりません")
    }

    pub fn invalid(title: impl Into<String>) -> Self {
        Problem::new(ProblemCode::InvalidRequest, title)
    }

    pub fn internal() -> Self {
        Problem::new(ProblemCode::InternalError, "内部エラー")
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.status, self.code.slug(), self.title)
    }
}

impl std::error::Error for Problem {}
