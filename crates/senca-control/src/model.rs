//! 制御面の資源。JSON で保存・複製するので、フィールドの追加は `#[serde(default)]` で後方互換に保つ。

use crate::auth::Scope;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LatencyProfile {
    /// 通常の HLS（セグメント 2 秒前後）。
    #[default]
    Standard,
    /// LL-HLS。
    Low,
    /// WebRTC・MoQ。
    Interactive,
}

/// REQ-007。`RoyaltyFree` は特許プールのあるコーデック（H.264・H.265・AAC）を理由付きで拒否する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CodecProfile {
    #[default]
    Any,
    RoyaltyFree,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RecordingPolicy {
    /// publish が始まったら自動で録画する。
    #[serde(default)]
    pub enabled: bool,
    /// 保持日数（None = 無期限）。
    #[serde(default)]
    pub retention_days: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotPolicy {
    /// 一定間隔（秒）で今の画を保存する。
    pub interval_secs: u32,
    /// 残す枚数（古いものから消す）。
    pub keep: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default)]
    pub max_viewers: Option<u32>,
    #[serde(default)]
    pub max_ingress_bps: Option<u64>,
}

/// HLS の出し方。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HlsSettings {
    /// セグメントの目標長（ミリ秒）。
    #[serde(default = "default_segment_ms")]
    pub segment_ms: u32,
    /// ライブの窓（セグメント数）。
    #[serde(default = "default_window")]
    pub window: u32,
    /// "ts" または "fmp4"。
    #[serde(default = "default_container")]
    pub container: String,
}

fn default_segment_ms() -> u32 {
    2000
}
fn default_window() -> u32 {
    6
}
fn default_container() -> String {
    "ts".into()
}

impl Default for HlsSettings {
    fn default() -> Self {
        HlsSettings { segment_ms: default_segment_ms(), window: default_window(), container: default_container() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    pub id: String,
    pub tenant: String,
    pub name: String,
    pub generation: u64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// publish 鍵の SHA-256（平文は持たない）。
    pub key_hash: String,
    #[serde(default)]
    pub latency_profile: LatencyProfile,
    #[serde(default)]
    pub codec_profile: CodecProfile,
    #[serde(default)]
    pub recording: RecordingPolicy,
    #[serde(default)]
    pub snapshots: Option<SnapshotPolicy>,
    #[serde(default)]
    pub hls: HlsSettings,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    /// "running"（publish を受け付ける）または "stopped"。
    #[serde(default = "default_desired")]
    pub desired_state: String,
}

fn default_desired() -> String {
    "running".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiToken {
    pub id: String,
    pub tenant: String,
    pub name: String,
    pub hash: String,
    pub scopes: BTreeSet<Scope>,
    pub created_at_ms: i64,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingState {
    /// 録画中。
    Recording,
    /// 正常に確定。
    Finalized,
    /// ノードの障害で途中まで（回収済み）。
    Interrupted,
    /// 削除の途中。
    Deleting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recording {
    pub id: String,
    pub tenant: String,
    pub stream: String,
    pub generation: u64,
    pub state: RecordingState,
    pub started_at_ms: i64,
    #[serde(default)]
    pub ended_at_ms: Option<i64>,
    #[serde(default)]
    pub retention_until_ms: Option<i64>,
    #[serde(default)]
    pub legal_hold: bool,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// 録画を作ったノード（分散で使う）。
    #[serde(default)]
    pub node: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleAction {
    /// 窓の間、録画する。
    Record,
    /// 窓の間だけ publish を受け付ける（窓の外は拒否）。
    Live,
    /// 窓の間、一定間隔で静止画を保存する。
    Snapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleState {
    Pending,
    Running,
    Completed,
    Missed,
    Canceled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recurrence {
    /// "daily" または "weekly"。
    pub freq: String,
    /// weekly のときの曜日（0 = 日曜）。
    #[serde(default)]
    pub weekdays: Vec<u8>,
    /// IANA タイムゾーン名（例: "Asia/Tokyo"）。壁時計の時刻で繰り返す。
    pub timezone: String,
    /// 繰り返しの終わり（含まない、UTC ミリ秒）。
    #[serde(default)]
    pub until_ms: Option<i64>,
    #[serde(default)]
    pub count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleRun {
    pub occurrence: u32,
    pub start_at_ms: i64,
    pub end_at_ms: i64,
    pub state: ScheduleState,
    #[serde(default)]
    pub recording: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub fencing_token: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub tenant: String,
    pub stream: String,
    pub generation: u64,
    pub name: Option<String>,
    pub action: ScheduleAction,
    pub start_at_ms: i64,
    pub end_at_ms: i64,
    #[serde(default)]
    pub pre_roll_ms: u32,
    #[serde(default)]
    pub post_roll_ms: u32,
    /// 実行時刻を過ぎても、この猶予の間なら途中から実行する（それ以上は missed）。
    #[serde(default = "default_grace")]
    pub grace_ms: u32,
    #[serde(default)]
    pub snapshot_interval_secs: Option<u32>,
    #[serde(default)]
    pub recurrence: Option<Recurrence>,
    #[serde(default)]
    pub retention_days: Option<u32>,
    pub state: ScheduleState,
    #[serde(default)]
    pub runs: Vec<ScheduleRun>,
    pub created_at_ms: i64,
}

fn default_grace() -> u32 {
    60_000
}

/// Idempotency-Key の記録（仕様 10.1、24 時間）。秘密は伏せた応答だけを覚える。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdempotencyRecord {
    pub tenant: String,
    pub key: String,
    pub fingerprint: String,
    pub status: u16,
    pub body: String,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub etag: Option<String>,
    /// 最初の要求の主体。別の主体が同じ鍵を使ったら 409（権限の確認を飛ばして答えないため）。
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    pub created_at_ms: i64,
}

/// ノード（分散で使う。単一ノードでも自分を登録する）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: String,
    pub name: String,
    pub draining: bool,
    pub registered_at_ms: i64,
    #[serde(default)]
    pub media_addr: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}
