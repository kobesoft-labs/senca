//! パイプラインの誤り。制御 API のエラーコード（仕様 10.4）に対応付ける。

use serde::{Deserialize, Serialize};
use std::fmt;

/// 検証で見つけた問題の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IssueCode {
    /// JSON として読めない・知らない項目・版が違う。
    Malformed,
    /// 値が範囲外・形が不正。
    InvalidParam,
    /// 同じ id の部品が 2 つある。
    DuplicateId,
    /// 知らない部品・ポートを参照している。
    UnknownReference,
    /// ポートの型が合わない（符号化済みと生、映像と音声など）。
    TypeMismatch,
    /// 循環がある。
    Cycle,
    /// 出力がどこにも使われていない。
    UnconsumedOutput,
    /// 方針で拒否（ロイヤリティフリー限定で H.264 など、REQ-007）。
    PolicyRejected,
    /// 対応しないコーデック（AAC の復号・符号化など、ADR-0004）。
    UnsupportedCodec,
    /// 装置・枠が無い（H.264/H.265 のハードウェアが無い、セッション数の上限）。
    CapacityUnavailable,
    /// 上限（部品の数・解像度・総量）を超える。
    LimitExceeded,
}

impl IssueCode {
    pub fn as_str(self) -> &'static str {
        match self {
            IssueCode::Malformed => "MALFORMED",
            IssueCode::InvalidParam => "INVALID_PARAM",
            IssueCode::DuplicateId => "DUPLICATE_ID",
            IssueCode::UnknownReference => "UNKNOWN_REFERENCE",
            IssueCode::TypeMismatch => "TYPE_MISMATCH",
            IssueCode::Cycle => "CYCLE",
            IssueCode::UnconsumedOutput => "UNCONSUMED_OUTPUT",
            IssueCode::PolicyRejected => "POLICY_REJECTED",
            IssueCode::UnsupportedCodec => "UNSUPPORTED_CODEC",
            IssueCode::CapacityUnavailable => "CAPACITY_UNAVAILABLE",
            IssueCode::LimitExceeded => "LIMIT_EXCEEDED",
        }
    }
}

/// 1 つの問題（どの部品の、何が、なぜ）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Issue {
    pub code: IssueCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub message: String,
}

impl Issue {
    pub fn new(code: IssueCode, node: Option<&str>, message: impl Into<String>) -> Self {
        Issue {
            code,
            node: node.map(str::to_string),
            message: message.into(),
        }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.node {
            Some(n) => write!(f, "{} [{}]: {}", self.code.as_str(), n, self.message),
            None => write!(f, "{}: {}", self.code.as_str(), self.message),
        }
    }
}

/// パイプラインの誤り。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    /// 検証で拒否した（理由の一覧）。
    Invalid(Vec<Issue>),
    /// 実行の準備・実行中の失敗（コーデックを開けない等）。`code` は制御 API のエラーコード。
    Runtime { code: String, message: String },
    /// 世代の操作の順序が誤り（知らない世代、commit 済みを rollback など）。
    Generation(String),
    /// 操作（control）が不正。
    Control(String),
    /// 止めている・止まった。
    Closed,
}

impl PipelineError {
    pub fn runtime(code: &str, message: impl Into<String>) -> Self {
        PipelineError::Runtime {
            code: code.to_string(),
            message: message.into(),
        }
    }

    /// 制御 API のエラーコード（仕様 10.4）。
    pub fn api_code(&self) -> &str {
        match self {
            PipelineError::Invalid(issues) => {
                if issues
                    .iter()
                    .any(|i| i.code == IssueCode::CapacityUnavailable)
                {
                    "CAPACITY_UNAVAILABLE"
                } else if issues.iter().all(|i| i.code == IssueCode::UnsupportedCodec) {
                    "UNSUPPORTED_CODEC"
                } else {
                    "INVALID_CONFIG"
                }
            }
            PipelineError::Runtime { code, .. } => code,
            PipelineError::Generation(_) => "CONFLICT",
            PipelineError::Control(_) => "INVALID_CONFIG",
            PipelineError::Closed => "CAPACITY_UNAVAILABLE",
        }
    }

    /// HTTP の状態コードの目安。
    pub fn http_status(&self) -> u16 {
        match self.api_code() {
            "CAPACITY_UNAVAILABLE" => 503,
            "CONFLICT" => 409,
            "INTERNAL_ERROR" => 500,
            _ => 422,
        }
    }

    pub fn issues(&self) -> &[Issue] {
        match self {
            PipelineError::Invalid(i) => i,
            _ => &[],
        }
    }
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineError::Invalid(issues) => {
                write!(f, "pipeline rejected:")?;
                for i in issues {
                    write!(f, " {i};")?;
                }
                Ok(())
            }
            PipelineError::Runtime { code, message } => write!(f, "{code}: {message}"),
            PipelineError::Generation(m) => write!(f, "generation: {m}"),
            PipelineError::Control(m) => write!(f, "control: {m}"),
            PipelineError::Closed => write!(f, "pipeline is closed"),
        }
    }
}

impl std::error::Error for PipelineError {}

impl From<senca_codec::CodecError> for PipelineError {
    fn from(e: senca_codec::CodecError) -> Self {
        PipelineError::Runtime {
            code: e.kind.api_code().to_string(),
            message: e.message,
        }
    }
}

impl From<senca_image::ImageError> for PipelineError {
    fn from(e: senca_image::ImageError) -> Self {
        PipelineError::runtime("INTERNAL_ERROR", format!("image: {e}"))
    }
}

impl From<senca_compose::ComposeError> for PipelineError {
    fn from(e: senca_compose::ComposeError) -> Self {
        PipelineError::runtime("INVALID_CONFIG", format!("compose: {e}"))
    }
}
