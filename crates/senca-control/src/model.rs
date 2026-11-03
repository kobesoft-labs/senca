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
    /// LL-HLS（part は min(500 ms, セグメント / 4)）。
    Low,
    /// 対話向け（WebRTC・MoQ で見る前提）。LL-HLS の part を 200 ms にし、出だしの待ちと送りの溜まりを小さくする。
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
    /// 字幕と翻訳（R6-4）。無ければ外部へは何も送らない。
    #[serde(default)]
    pub captions: Option<CaptionSettings>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptionSettings {
    /// 音声認識: "openai"（OpenAI 互換の /audio/transcriptions）か "whisper_cpp"。
    pub asr_provider: String,
    /// 送り先の URL（例: https://api.openai.com/v1、社内の whisper サーバー）。
    pub asr_endpoint: String,
    #[serde(default)]
    pub asr_credential_ref: Option<String>,
    #[serde(default)]
    pub asr_model: Option<String>,
    /// 話している言語（ISO 639-1）。
    #[serde(default = "default_lang")]
    pub language: String,
    /// 翻訳: "anthropic"・"deepl"・"openai"。
    #[serde(default)]
    pub translate_provider: Option<String>,
    #[serde(default)]
    pub translate_endpoint: Option<String>,
    #[serde(default)]
    pub translate_credential_ref: Option<String>,
    #[serde(default)]
    pub translate_model: Option<String>,
    #[serde(default)]
    pub targets: Vec<String>,
}

fn default_lang() -> String {
    "ja".into()
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

/// 入力（仕様 10.2 `POST /streams/{id}/inputs`）。push（送り手がつなぐ）は stream の鍵で受けるので、ここは pull だけ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    pub id: String,
    pub tenant: String,
    pub stream: String,
    pub generation: u64,
    /// "rtmp_pull"・"rtsp_pull"・"srt_pull"・"hls_pull"
    pub protocol: String,
    /// 取りに行く URL（資格情報は入れない。`credential_ref` で渡す）。
    pub url: String,
    #[serde(default)]
    pub credential_ref: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// プロトコルごとの設定（文字列）。rtsp_pull: `allow_basic`（`true` で Basic 認証を許す）・`tls_spki_sha256`
    /// （rtsps の公開鍵の固定）。device: `codec`・`width`・`height`・`fps`・`bitrate_bps`・`audio_device` など。
    /// RTSP は TCP interleaved だけで受ける（UDP の転送の設定は無い）。
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    pub created_at_ms: i64,
    /// 配置の指定（ADR-0013）。無ければ配置係に任せる。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementSpec>,
}

/// 出力（仕様 10.2 `POST /streams/{id}/outputs`）。外部への push と PaaS 連携（REQ-001）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    pub id: String,
    pub tenant: String,
    pub stream: String,
    pub generation: u64,
    /// "rtmp_push"・"srt_push"・"whip_push"・"paas"
    pub protocol: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub credential_ref: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// PaaS 連携（"cloudflare_stream"・"aws_ivs" など）。
    #[serde(default)]
    pub provider: Option<String>,
    /// SENCA が外部に作った資源（後片付けに使う）。
    #[serde(default)]
    pub provider_resource: Option<String>,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    pub created_at_ms: i64,
    /// 配置の指定（ADR-0013）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementSpec>,
}

fn default_true() -> bool {
    true
}

/// 秘密値（仕様 10.2「credentials は secret reference」）。値はノードの鍵で暗号化して持つ（AES-256-GCM）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretEntry {
    pub id: String,
    pub tenant: String,
    pub name: String,
    /// base64(nonce 12 バイト || 暗号文 || タグ)。
    pub sealed: String,
    pub created_at_ms: i64,
}

/// stream の所有権（仕様 8.2）。送り手を受けているノードと、その fencing token。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ownership {
    pub node: String,
    pub token: u64,
    pub since_ms: i64,
}

/// パイプライン（REQ-005・REQ-008）。グラフは senca-pipeline の JSON（ここでは解釈しない）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pipeline {
    pub id: String,
    pub tenant: String,
    pub name: String,
    pub generation: u64,
    pub graph: serde_json::Value,
    /// "running" か "stopped"。
    pub desired_state: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// 配置の指定と縮退の段（ADR-0013）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementSpec>,
}

/// リアルタイムの部屋（ADR-0015）。部屋の ID はテナントをまたいで一意（シグナリングの `room_id`）。
/// 設定の中身は senca-rooms の `RoomSettings`（ノードが検査する）。部屋はグリッドの仕事として 1 つのノードに置く。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Room {
    pub id: String,
    pub tenant: String,
    pub generation: u64,
    #[serde(default)]
    pub settings: serde_json::Value,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementSpec>,
    /// 参加トークンの発行で自動に作った部屋（空いてしばらくすると消す）。
    #[serde(default)]
    pub auto: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// 部屋のメディアパイプライン（ADR-0016、AWS Chime SDK Media Pipelines 相当）。部屋を置いたノードで動く。
///
/// - 書き出し（stream pipeline）: 参加者ごとに SENCA の stream を作り、送ってきた映像・音声をそのまま流す
///   （再符号化しない）。以後は普通の stream と同じ（HLS・録画・出力・字幕）。
/// - 合成（composite）: 参加者の stream を並べた 1 本の stream（senca-pipeline の合成で作る）。
/// - 録画（capture）・送り出し（live connector）・文字起こし（insights）は、書き出した stream の録画・出力・字幕。
/// - まとめ（concatenation）は録画の書き出し（`/recordings/{id}/export.mp4`・HLS）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaPipeline {
    pub id: String,
    pub tenant: String,
    pub generation: u64,
    pub name: String,
    pub room: String,
    /// 書き出す参加者（client_id）。空なら全員。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub participants: Vec<String>,
    /// 参加者ごとの stream を残す（偽なら合成のためだけに使う）。
    #[serde(default = "default_true")]
    pub export_participants: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composite: Option<CompositeSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<MediaPipelineRecording>,
    /// 合成の送り出し（RTMP・SRT・PaaS。出力と同じ形）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub egress: Vec<MediaPipelineEgress>,
    /// 文字起こし・翻訳（stream の `captions` と同じ形。参加者ごとの stream に付ける）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captions: Option<serde_json::Value>,
    /// "running" か "stopped"。
    pub desired_state: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositeSpec {
    /// "grid"（全員を並べる）か "speaker"（話している人を大きく）。
    #[serde(default = "default_layout")]
    pub layout: String,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_fps")]
    pub fps: u32,
    /// "av1"・"vp9"・"vp8"・"h264"・"h265"（H.264/H.265 は GPU が要る）。
    #[serde(default = "default_vcodec")]
    pub video_codec: String,
    #[serde(default = "default_bitrate")]
    pub bitrate_kbps: u32,
}

fn default_layout() -> String {
    "grid".into()
}
fn default_width() -> u32 {
    1280
}
fn default_height() -> u32 {
    720
}
fn default_fps() -> u32 {
    30
}
fn default_vcodec() -> String {
    "av1".into()
}
fn default_bitrate() -> u32 {
    2500
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaPipelineRecording {
    #[serde(default = "default_true")]
    pub participants: bool,
    #[serde(default = "default_true")]
    pub composite: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaPipelineEgress {
    /// "rtmp_push"・"srt_push"・"paas"
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
}

/// 配置の指定（入力・出力・パイプライン。ADR-0013）。手動の割り当て（ノード・GPU）、ラベル、反親和性、
/// 優先度、縮退の段。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementSpec {
    /// ノードの ID（手動の割り当て）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// 装置の ID（GPU・キャプチャ機器。手動の割り当て）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<senca_grid::LabelSelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anti_affinity: Option<String>,
    /// 割り当て先が止まったとき: `prefer`（他へ移し、戻ったら戻す）か `strict`（止めたまま）。
    #[serde(default)]
    pub pin_policy: senca_grid::PinPolicy,
    #[serde(default)]
    pub priority: senca_grid::Priority,
    /// `spread`（余裕の多いノードへ）か `binpack`（詰める）。
    #[serde(default)]
    pub strategy: senca_grid::Placement,
    /// 縮退の段（パイプラインだけ。よい順。段ごとに止める節点を足していく）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degrade: Vec<DegradeLevel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DegradeLevel {
    pub label: String,
    /// この段で止める節点の ID（下流は入力がすべて止まれば止まる）。
    pub disable: Vec<String>,
}
