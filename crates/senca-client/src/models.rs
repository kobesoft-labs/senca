//! 管理 API のモデル（`docs/api/openapi.yaml` の `components.schemas`）。
//!
//! 生成物: 手で直さない。`python3 crates/senca-client/gen/gen_models.py` で作り直す。
//! 規則は生成器の先頭の説明を参照（未知のフィールドは読み飛ばす、知らない enum の値は `Unknown`、
//! `*Patch` の null になりうるフィールドは `Option<Option<T>>`）。
#![allow(clippy::large_enum_variant, clippy::doc_lazy_continuation, clippy::doc_overindented_list_items)]

use serde::{Deserialize, Serialize};
#[allow(unused_imports)]
use serde_json::Value;
#[allow(unused_imports)]
use std::collections::BTreeMap;

pub type Timestamp = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scope {
    #[serde(rename = "stream:read")]
    StreamRead,
    #[serde(rename = "stream:write")]
    StreamWrite,
    #[serde(rename = "publish")]
    Publish,
    #[serde(rename = "subscribe")]
    Subscribe,
    #[serde(rename = "recording:read")]
    RecordingRead,
    #[serde(rename = "recording:delete")]
    RecordingDelete,
    #[serde(rename = "tenant:admin")]
    TenantAdmin,
    #[serde(rename = "node:admin")]
    NodeAdmin,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl Scope {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            Scope::StreamRead => Some("stream:read"),
            Scope::StreamWrite => Some("stream:write"),
            Scope::Publish => Some("publish"),
            Scope::Subscribe => Some("subscribe"),
            Scope::RecordingRead => Some("recording:read"),
            Scope::RecordingDelete => Some("recording:delete"),
            Scope::TenantAdmin => Some("tenant:admin"),
            Scope::NodeAdmin => Some("node:admin"),
            Scope::Unknown => None,
        }
    }
}

/// standard = HLS、low = LL-HLS（part は min(500 ms, segment_ms / 4)）、interactive = WebRTC・MoQ 向け（LL-HLS の part は 200 ms、出だしの待ちと送りの溜まりを小さくする）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LatencyProfile {
    #[serde(rename = "standard")]
    Standard,
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "interactive")]
    Interactive,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl LatencyProfile {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            LatencyProfile::Standard => Some("standard"),
            LatencyProfile::Low => Some("low"),
            LatencyProfile::Interactive => Some("interactive"),
            LatencyProfile::Unknown => None,
        }
    }
}

/// royalty_free は H.264・H.265・AAC を理由付きで拒否する（REQ-007）。royalty_free では hls.container は fmp4 にする。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CodecProfile {
    #[serde(rename = "any")]
    Any,
    #[serde(rename = "royalty_free")]
    RoyaltyFree,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl CodecProfile {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            CodecProfile::Any => Some("any"),
            CodecProfile::RoyaltyFree => Some("royalty_free"),
            CodecProfile::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RecordingPolicy {
    /// publish が始まったら自動で録画する
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// 保持日数（null は無期限）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SnapshotPolicy {
    pub interval_secs: u64,
    pub keep: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HlsSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<HlsSettingsContainer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HlsSettingsContainer {
    #[serde(rename = "ts")]
    Ts,
    #[serde(rename = "fmp4")]
    Fmp4,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl HlsSettingsContainer {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            HlsSettingsContainer::Ts => Some("ts"),
            HlsSettingsContainer::Fmp4 => Some("fmp4"),
            HlsSettingsContainer::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Limits {
    /// 入力のビットレートの上限（直近 3 秒の平均）。5 秒続けて超えたら送り手を切り、10 秒は新しい送り手を断る
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ingress_bps: Option<u64>,
    /// ノードごとの同時の視聴者の上限（最近プレイリストを取得した HLS のクライアントと、RTMP・SRT・RTSP・WHEP・MoQ の接続の合計）。超える新しい視聴は断る（HLS・WHEP・RTSP は 503）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_viewers: Option<u64>,
}

pub type Metadata = BTreeMap<String, String>;

pub type StreamName = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StreamCreate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captions: Option<CaptionSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec_profile: Option<CodecProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_state: Option<StreamCreateDesiredState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hls: Option<HlsSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_profile: Option<LatencyProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub name: StreamName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots: Option<SnapshotPolicy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StreamCreateDesiredState {
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "stopped")]
    Stopped,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl StreamCreateDesiredState {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            StreamCreateDesiredState::Running => Some("running"),
            StreamCreateDesiredState::Stopped => Some("stopped"),
            StreamCreateDesiredState::Unknown => None,
        }
    }
}

/// 書いたフィールドだけ置き換える。`snapshots: null` は静止画の方針を、`captions: null` は字幕を消す。他のフィールドの null は「変えない」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StreamPatch {
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub captions: Option<Option<CaptionSettings>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec_profile: Option<CodecProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_state: Option<StreamCreateDesiredState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hls: Option<HlsSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_profile: Option<LatencyProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<StreamName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub snapshots: Option<Option<SnapshotPolicy>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Ingest {
    /// RTMP の publish 先（`{public-rtmp-url}/live`）。stream name に stream_key を使う。
    pub rtmp_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srt_streamid_format: Option<Value>,
    /// `--public-srt-url` を設定したとき
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srt_url: Option<String>,
    /// publish 鍵。作成・作り直しの応答でだけ値が入る（それ以外は null）。
    #[serde(default)]
    pub stream_key: Option<String>,
    /// Idempotency-Key の再送で鍵を伏せたとき
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_key_redacted: Option<Value>,
    /// `--public-http-url` を設定したとき
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whip_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LiveInfo {
    pub bitrate_bps: u64,
    /// 再接続ごとに増える
    pub epoch: u64,
    pub started_at: Timestamp,
    pub tracks: Vec<Value>,
    /// rtmp・srt など
    pub transport: String,
    /// このノードの視聴者の数（`limits.max_viewers` で数えるもの）。`hls` は最近プレイリストを取得した別々の HLS のクライアント
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewers: Option<LiveInfoViewers>,
    /// hub の購読の数（中継・録画以外の内部の購読も含む目安）
    pub viewers_hint: u64,
}

/// このノードの視聴者の数（`limits.max_viewers` で数えるもの）。`hls` は最近プレイリストを取得した別々の HLS のクライアント
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LiveInfoViewers {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hls: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtmp: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtsp: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srt: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webrtc: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stream {
    /// 字幕と翻訳の設定（無ければ null）。資格情報は `secret:{name}` の参照で、値は返さない。
    #[serde(default)]
    pub captions: Option<CaptionSettings>,
    pub codec_profile: CodecProfile,
    pub created_at: Timestamp,
    pub desired_state: StreamCreateDesiredState,
    pub generation: u64,
    pub hls: HlsSettings,
    pub id: String,
    pub ingest: Ingest,
    pub latency_profile: LatencyProfile,
    pub limits: Limits,
    #[serde(default)]
    pub live: Option<LiveInfo>,
    pub metadata: Metadata,
    pub name: String,
    /// 仕様 10.4 の provisioning・draining・degraded・failed は今は出さない。
    pub observed_state: StreamObservedState,
    pub recording: RecordingPolicy,
    #[serde(default)]
    pub snapshots: Option<SnapshotPolicy>,
    pub updated_at: Timestamp,
}

/// 仕様 10.4 の provisioning・draining・degraded・failed は今は出さない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StreamObservedState {
    #[serde(rename = "waiting_input")]
    WaitingInput,
    #[serde(rename = "live")]
    Live,
    #[serde(rename = "stopped")]
    Stopped,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl StreamObservedState {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            StreamObservedState::WaitingInput => Some("waiting_input"),
            StreamObservedState::Live => Some("live"),
            StreamObservedState::Stopped => Some("stopped"),
            StreamObservedState::Unknown => None,
        }
    }
}

/// 字幕と翻訳（R6-4）。配信中の音声を `asr_endpoint` に送って文字にし、`targets` の言語へ翻訳して、ライブ HLS に WebVTT の字幕を足す。
/// 検査: `asr_provider`・`translate_provider` の値、`asr_endpoint`・`translate_endpoint` が URL として読めること、言語は `language` と `targets` 合わせて英数字と `-` の 1〜8 文字（`targets` は 8 個まで）。違えば 400（`INVALID_CONFIG`）。
/// `asr_credential_ref`・`translate_credential_ref` は `secret:{name}` の形で、テナントに秘密値があること（無ければ 400）。参照している秘密値は消せない（409）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptionSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_credential_ref: Option<CredentialRef>,
    /// 送り先の URL（例: `https://api.openai.com/v1`、社内の whisper サーバー）
    pub asr_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_model: Option<String>,
    /// openai = OpenAI 互換の `/audio/transcriptions`、whisper_cpp = whisper.cpp のサーバー
    pub asr_provider: CaptionSettingsAsrProvider,
    /// 話している言語（ISO 639-1）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// 翻訳先の言語
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub targets: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translate_credential_ref: Option<CredentialRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translate_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translate_model: Option<String>,
    /// 翻訳（省けば翻訳しない）。書くなら translate_endpoint が要る
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translate_provider: Option<CaptionSettingsTranslateProvider>,
}

/// openai = OpenAI 互換の `/audio/transcriptions`、whisper_cpp = whisper.cpp のサーバー
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CaptionSettingsAsrProvider {
    #[serde(rename = "openai")]
    Openai,
    #[serde(rename = "whisper_cpp")]
    WhisperCpp,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl CaptionSettingsAsrProvider {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            CaptionSettingsAsrProvider::Openai => Some("openai"),
            CaptionSettingsAsrProvider::WhisperCpp => Some("whisper_cpp"),
            CaptionSettingsAsrProvider::Unknown => None,
        }
    }
}

/// 翻訳（省けば翻訳しない）。書くなら translate_endpoint が要る
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CaptionSettingsTranslateProvider {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "deepl")]
    Deepl,
    #[serde(rename = "openai")]
    Openai,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl CaptionSettingsTranslateProvider {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            CaptionSettingsTranslateProvider::Anthropic => Some("anthropic"),
            CaptionSettingsTranslateProvider::Deepl => Some("deepl"),
            CaptionSettingsTranslateProvider::Openai => Some("openai"),
            CaptionSettingsTranslateProvider::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StreamList {
    pub items: Vec<Stream>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PlaybackRequest {
    /// 既定 300、上限は `--max-playback-ttl`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LivePlayback {
    pub expires_at: Timestamp,
    pub playback: LivePlaybackPlayback,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LivePlaybackPlayback {
    /// `{http}/s/{exp}/{sig}/live/{id}/index.m3u8`
    pub hls: String,
    /// MoQ（draft-22）の WebTransport の URL `https://{host}:{quic port}/moq?exp=&sig=`。namespace は `senca/live/{id}`（`--quic` を設定したときだけ）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moq: Option<String>,
    /// `{rtmp}/play/{id}?exp=&sig=`
    pub rtmp: String,
    /// `{public-rtsp-url}/live/{id}?exp=&sig=`（`--public-rtsp-url` を設定したときだけ）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtsp: Option<String>,
    /// `{http}/whep/{id}?exp=&sig=`（`--webrtc-udp` を設定しないノードでは null）
    #[serde(default)]
    pub whep: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct VodPlayback {
    pub expires_at: Timestamp,
    pub playback: VodPlaybackPlayback,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct VodPlaybackPlayback {
    /// `{http}/s/{exp}/{sig}/vod/{id}/index.m3u8`
    pub hls: String,
    /// 今は常に null
    pub mp4: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_latency_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<HlsSettingsContainer>,
    pub mode: PlanOutputMode,
    pub output: PlanOutputOutput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub track: PlanOutputTrack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlanOutputMode {
    #[serde(rename = "passthrough")]
    Passthrough,
    #[serde(rename = "remux")]
    Remux,
    #[serde(rename = "repacketize")]
    Repacketize,
    #[serde(rename = "transcode")]
    Transcode,
    #[serde(rename = "unsupported")]
    Unsupported,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PlanOutputMode {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PlanOutputMode::Passthrough => Some("passthrough"),
            PlanOutputMode::Remux => Some("remux"),
            PlanOutputMode::Repacketize => Some("repacketize"),
            PlanOutputMode::Transcode => Some("transcode"),
            PlanOutputMode::Unsupported => Some("unsupported"),
            PlanOutputMode::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlanOutputOutput {
    #[serde(rename = "hls")]
    Hls,
    #[serde(rename = "webrtc")]
    Webrtc,
    #[serde(rename = "recording")]
    Recording,
    #[serde(rename = "rtmp")]
    Rtmp,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PlanOutputOutput {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PlanOutputOutput::Hls => Some("hls"),
            PlanOutputOutput::Webrtc => Some("webrtc"),
            PlanOutputOutput::Recording => Some("recording"),
            PlanOutputOutput::Rtmp => Some("rtmp"),
            PlanOutputOutput::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlanOutputTrack {
    #[serde(rename = "video")]
    Video,
    #[serde(rename = "audio")]
    Audio,
    #[serde(rename = "data")]
    Data,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PlanOutputTrack {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PlanOutputTrack::Video => Some("video"),
            PlanOutputTrack::Audio => Some("audio"),
            PlanOutputTrack::Data => Some("data"),
            PlanOutputTrack::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub outputs: Vec<PlanOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub state: PlanState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlanState {
    #[serde(rename = "provisional")]
    Provisional,
    #[serde(rename = "ready")]
    Ready,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PlanState {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PlanState::Provisional => Some("provisional"),
            PlanState::Ready => Some("ready"),
            PlanState::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SnapshotList {
    pub items: Vec<SnapshotListItemsItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotListItemsItem {
    /// JPEG の大きさ
    pub bytes: u64,
    /// 前回の保存からの変化の割合（パイプラインの静止画だけ）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_fraction: Option<f64>,
    pub height: u64,
    pub id: String,
    /// `snapshot_on_change` の部品の ID（パイプラインの静止画だけ）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// 撮った理由
    pub reason: SnapshotListItemsItemReason,
    pub taken_at: Timestamp,
    pub width: u64,
}

/// 撮った理由
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SnapshotListItemsItemReason {
    #[serde(rename = "interval")]
    Interval,
    #[serde(rename = "schedule")]
    Schedule,
    #[serde(rename = "board_change")]
    BoardChange,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl SnapshotListItemsItemReason {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            SnapshotListItemsItemReason::Interval => Some("interval"),
            SnapshotListItemsItemReason::Schedule => Some("schedule"),
            SnapshotListItemsItemReason::BoardChange => Some("board_change"),
            SnapshotListItemsItemReason::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RecordingStart {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RecordingPatch {
    /// 解除には recording:delete が要る
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legal_hold: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// RFC 3339。null で無期限。
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub retention_until: Option<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Gap {
    pub at: Timestamp,
    pub dropped: u64,
    /// publisher_reconnect など
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recording {
    #[serde(default)]
    pub bytes: Option<u64>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub ended_at: Option<String>,
    #[serde(default)]
    pub gaps: Option<Vec<Gap>>,
    pub generation: u64,
    pub id: String,
    pub legal_hold: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub retention_until: Option<String>,
    /// 予約で作った録画なら予約の ID
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub segments: Option<u64>,
    pub started_at: Timestamp,
    /// 削除中（deleting）の録画は API から見えない（404）。
    pub state: RecordingState,
    /// 元の stream の ID（stream を消しても残る）
    pub stream: String,
    /// `GET /recordings/{id}/thumbnail` があるか
    pub thumbnail: bool,
}

/// 削除中（deleting）の録画は API から見えない（404）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RecordingState {
    #[serde(rename = "recording")]
    Recording,
    #[serde(rename = "finalized")]
    Finalized,
    #[serde(rename = "interrupted")]
    Interrupted,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl RecordingState {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            RecordingState::Recording => Some("recording"),
            RecordingState::Finalized => Some("finalized"),
            RecordingState::Interrupted => Some("interrupted"),
            RecordingState::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RecordingDeleting {
    pub id: String,
    pub state: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RecordingList {
    pub items: Vec<Recording>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// record = 窓の間録画、live = 窓の間だけ publish を受け付ける、snapshot = 窓の間一定間隔で静止画
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScheduleAction {
    #[serde(rename = "record")]
    Record,
    #[serde(rename = "live")]
    Live,
    #[serde(rename = "snapshot")]
    Snapshot,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl ScheduleAction {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            ScheduleAction::Record => Some("record"),
            ScheduleAction::Live => Some("live"),
            ScheduleAction::Snapshot => Some("snapshot"),
            ScheduleAction::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScheduleState {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "missed")]
    Missed,
    #[serde(rename = "canceled")]
    Canceled,
    #[serde(rename = "failed")]
    Failed,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl ScheduleState {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            ScheduleState::Pending => Some("pending"),
            ScheduleState::Running => Some("running"),
            ScheduleState::Completed => Some("completed"),
            ScheduleState::Missed => Some("missed"),
            ScheduleState::Canceled => Some("canceled"),
            ScheduleState::Failed => Some("failed"),
            ScheduleState::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recurrence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    pub freq: RecurrenceFreq,
    /// IANA 名（例: Asia/Tokyo）。壁時計の時刻で繰り返す
    pub timezone: String,
    /// 繰り返しの終わり（含まない、UTC ミリ秒）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<i64>,
    /// weekly の曜日（0 = 日曜）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekdays: Option<Vec<u64>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RecurrenceFreq {
    #[serde(rename = "daily")]
    Daily,
    #[serde(rename = "weekly")]
    Weekly,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl RecurrenceFreq {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            RecurrenceFreq::Daily => Some("daily"),
            RecurrenceFreq::Weekly => Some("weekly"),
            RecurrenceFreq::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleCreate {
    pub action: ScheduleAction,
    /// RFC 3339。start_at より後、窓は 24 時間まで、未来
    pub end_at: String,
    /// 既定 60000。実行時刻を過ぎてもこの間なら途中から実行
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 既定 0
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_roll_ms: Option<u64>,
    /// 既定 0
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_roll_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<Recurrence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u64>,
    /// action が snapshot のときだけ（必須）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_interval_secs: Option<u64>,
    /// RFC 3339
    pub start_at: String,
    pub stream: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SchedulePatch {
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub end_at: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub grace_ms: Option<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub name: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub post_roll_ms: Option<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub pre_roll_ms: Option<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub retention_days: Option<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub snapshot_interval_secs: Option<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub start_at: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub state: Option<Option<SchedulePatchState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SchedulePatchState {
    #[serde(rename = "canceled")]
    Canceled,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl SchedulePatchState {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            SchedulePatchState::Canceled => Some("canceled"),
            SchedulePatchState::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleRun {
    pub end_at: Timestamp,
    pub occurrence: u64,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub recording: Option<String>,
    pub start_at: Timestamp,
    pub state: ScheduleState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub action: ScheduleAction,
    pub created_at: Timestamp,
    pub end_at: Timestamp,
    pub generation: u64,
    pub grace_ms: u64,
    pub id: String,
    #[serde(default)]
    pub last_run: Option<ScheduleRun>,
    #[serde(default)]
    pub name: Option<String>,
    pub post_roll_ms: u64,
    pub pre_roll_ms: u64,
    #[serde(default)]
    pub recurrence: Option<Recurrence>,
    #[serde(default)]
    pub retention_days: Option<i64>,
    #[serde(default)]
    pub snapshot_interval_secs: Option<i64>,
    pub start_at: Timestamp,
    pub state: ScheduleState,
    pub stream: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ScheduleList {
    pub items: Vec<Schedule>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ScheduleRunList {
    pub items: Vec<ScheduleRun>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ScheduleCheck {
    pub ok: Value,
    /// 同じ stream で時間が重なる録画予約（pre/post roll を含む）
    pub overlaps: Vec<ScheduleCheckOverlapsItem>,
    /// 重なりがあり、入力を共有するか
    pub shared_input: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ScheduleCheckOverlapsItem {
    pub end_at: Timestamp,
    pub id: String,
    pub start_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TokenCreate {
    /// RFC 3339（未来）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub name: String,
    pub scopes: Vec<Scope>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Token {
    pub created_at: Timestamp,
    #[serde(default)]
    pub expires_at: Option<String>,
    pub id: String,
    pub name: String,
    pub scopes: Vec<Scope>,
    /// 値。発行の応答でだけ入る（一覧では null）。
    #[serde(default)]
    pub token: Option<String>,
    /// Idempotency-Key の再送で値を伏せたとき
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_redacted: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TokenList {
    pub items: Vec<Token>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Capabilities {
    pub api_version: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captions: Option<CapabilitiesCaptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<bool>,
    pub codec_profiles: Vec<CodecProfile>,
    pub codecs: CapabilitiesCodecs,
    pub ingest: CapabilitiesIngest,
    /// 使える入力の種類
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<String>>,
    /// このノードの ID
    pub node: String,
    pub output: CapabilitiesOutput,
    /// 使える出力の種類
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paas_providers: Option<Vec<String>>,
    /// パイプライン（codec worker が動いているとき）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipelines: Option<bool>,
    pub recordings: CapabilitiesRecordings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtsp_play: Option<bool>,
    pub schedules: CapabilitiesSchedules,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CapabilitiesCaptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translate: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CapabilitiesCodecs {
    pub passthrough: Vec<String>,
    /// コーデック worker が報告する変換能力（未接続なら空）
    pub transcode: Vec<TranscodeCapability>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CapabilitiesIngest {
    pub rtmp: bool,
    pub rtmps: bool,
    pub rtsp_pull: bool,
    pub srt: bool,
    pub whip: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CapabilitiesOutput {
    pub hls: CapabilitiesOutputHls,
    /// HTTP/3（`--quic`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http3: Option<bool>,
    /// MoQ（draft-22）の購読と送出（`--quic`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moq: Option<bool>,
    pub rtmp_play: bool,
    pub whep: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CapabilitiesRecordings {
    pub export: Vec<String>,
    pub max_concurrent_per_stream: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CapabilitiesSchedules {
    pub actions: Vec<ScheduleAction>,
    pub recurrence: Vec<RecurrenceFreq>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CapabilitiesOutputHls {
    pub containers: Vec<HlsSettingsContainer>,
    pub ll_hls: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Node {
    pub draining: bool,
    pub id: String,
    pub labels: BTreeMap<String, String>,
    /// 配信中の stream 数（他ノードは今は 0）
    pub live_streams: u64,
    pub name: String,
    pub registered_at: Timestamp,
    /// ノード間の中継（ADR-0014）の様子。応答したノード自身（`self: true`）で、クラスタを組んでいるときだけ値がある（単一ノード・他ノードは null）。
    #[serde(default)]
    pub relay: Option<RelayStatus>,
    /// 部屋の係の様子（カスケード・イベント Webhook の配送・中継の購読の数）。応答したノード自身（`self: true`）で WebRTC（`--webrtc-udp`）があるときだけ値がある（他は null）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rooms: Option<Value>,
    /// 応答したノード自身か
    #[serde(rename = "self")]
    pub is_self: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RelayStatus {
    /// 中継で受けている写しの stream の ID
    pub mirrors: Vec<String>,
    /// 中継の接続（相手のノードごと）
    pub peers: Vec<RelayStatusPeersItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RelayStatusPeersItem {
    /// この接続で送っている stream の数
    pub downstreams: u64,
    /// どちらかが接続を閉じようとしている
    pub going_away: bool,
    /// 相手のクラスタのノード番号（クラスタ設定の `peers[].id`。API のノード ID とは別）
    pub node: u64,
    /// 送った AU 1 つ当たりの見出しのバイト数（枠の見出し・制御の枠を含む）
    pub overhead_per_au: f64,
    /// RTT（平滑化、ミリ秒）。まだ測っていなければ null
    #[serde(default)]
    pub rtt_ms: Option<f64>,
    /// 受けた AU の数
    pub rx_aus: u64,
    /// 直近の受けの速さ（bit/s）
    pub rx_bps: u64,
    pub rx_bytes: u64,
    /// 送った AU の数
    pub tx_aus: u64,
    /// 直近の送りの速さ（bit/s）
    pub tx_bps: u64,
    pub tx_bytes: u64,
    /// この接続で受けている stream の数
    pub upstreams: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Resources {
    /// CPU（ミリコア）
    pub cpu_millis: u64,
    pub memory_mb: u64,
    pub net_in_mbps: u64,
    pub net_out_mbps: u64,
}

/// 割り当て・決定の理由（`code` で種類を選ぶ。`node_down` は `node`、`device_down` は `device`、`preempted` は `by` を持つ）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridReason {
    /// 追い出した仕事の ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// placed = 普通に置いた、no_capacity = 資源が足りず縮退した段で置いた・縮退した、node_down = ノードが止まった・消えた、device_down = 装置が故障した（手で止めたときも）、
    /// draining = ノードの排出で移した、constraint_changed = 条件・ラベルが変わって合わなくなった、preempted = 高い優先度の仕事のために縮退・追い出しされた、
    /// overcommitted = ノードの容量が減って収まらなくなった、capacity_returned = 資源が戻ったので元の段へ戻した、pin_returned = 手動の割り当ての対象が戻ったので戻した、
    /// pin_target_down = 手動の割り当て（strict）の対象が止まっている。
    pub code: GridReasonCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// placed = 普通に置いた、no_capacity = 資源が足りず縮退した段で置いた・縮退した、node_down = ノードが止まった・消えた、device_down = 装置が故障した（手で止めたときも）、
/// draining = ノードの排出で移した、constraint_changed = 条件・ラベルが変わって合わなくなった、preempted = 高い優先度の仕事のために縮退・追い出しされた、
/// overcommitted = ノードの容量が減って収まらなくなった、capacity_returned = 資源が戻ったので元の段へ戻した、pin_returned = 手動の割り当ての対象が戻ったので戻した、
/// pin_target_down = 手動の割り当て（strict）の対象が止まっている。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GridReasonCode {
    #[serde(rename = "placed")]
    Placed,
    #[serde(rename = "no_capacity")]
    NoCapacity,
    #[serde(rename = "node_down")]
    NodeDown,
    #[serde(rename = "device_down")]
    DeviceDown,
    #[serde(rename = "draining")]
    Draining,
    #[serde(rename = "constraint_changed")]
    ConstraintChanged,
    #[serde(rename = "preempted")]
    Preempted,
    #[serde(rename = "overcommitted")]
    Overcommitted,
    #[serde(rename = "capacity_returned")]
    CapacityReturned,
    #[serde(rename = "pin_returned")]
    PinReturned,
    #[serde(rename = "pin_target_down")]
    PinTargetDown,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl GridReasonCode {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            GridReasonCode::Placed => Some("placed"),
            GridReasonCode::NoCapacity => Some("no_capacity"),
            GridReasonCode::NodeDown => Some("node_down"),
            GridReasonCode::DeviceDown => Some("device_down"),
            GridReasonCode::Draining => Some("draining"),
            GridReasonCode::ConstraintChanged => Some("constraint_changed"),
            GridReasonCode::Preempted => Some("preempted"),
            GridReasonCode::Overcommitted => Some("overcommitted"),
            GridReasonCode::CapacityReturned => Some("capacity_returned"),
            GridReasonCode::PinReturned => Some("pin_returned"),
            GridReasonCode::PinTargetDown => Some("pin_target_down"),
            GridReasonCode::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RejectCount {
    /// そのノードの ID（8 個まで）
    pub examples: Vec<String>,
    /// この理由で受けられないノードの数
    pub nodes: u64,
    /// ノードが仕事を受けられない理由（`contended` は単独なら収まるが、優先度の同じか高い仕事で埋まっている）
    pub reason: RejectCountReason,
}

/// ノードが仕事を受けられない理由（`contended` は単独なら収まるが、優先度の同じか高い仕事で埋まっている）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RejectCountReason {
    #[serde(rename = "node_down")]
    NodeDown,
    #[serde(rename = "not_media")]
    NotMedia,
    #[serde(rename = "draining")]
    Draining,
    #[serde(rename = "pinned_elsewhere")]
    PinnedElsewhere,
    #[serde(rename = "pin_target_down")]
    PinTargetDown,
    #[serde(rename = "labels")]
    Labels,
    #[serde(rename = "anti_affinity")]
    AntiAffinity,
    #[serde(rename = "no_device")]
    NoDevice,
    #[serde(rename = "capture")]
    Capture,
    #[serde(rename = "cpu")]
    Cpu,
    #[serde(rename = "memory")]
    Memory,
    #[serde(rename = "net_in")]
    NetIn,
    #[serde(rename = "net_out")]
    NetOut,
    #[serde(rename = "gpu_encode")]
    GpuEncode,
    #[serde(rename = "gpu_decode")]
    GpuDecode,
    #[serde(rename = "gpu_memory")]
    GpuMemory,
    #[serde(rename = "contended")]
    Contended,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl RejectCountReason {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            RejectCountReason::NodeDown => Some("node_down"),
            RejectCountReason::NotMedia => Some("not_media"),
            RejectCountReason::Draining => Some("draining"),
            RejectCountReason::PinnedElsewhere => Some("pinned_elsewhere"),
            RejectCountReason::PinTargetDown => Some("pin_target_down"),
            RejectCountReason::Labels => Some("labels"),
            RejectCountReason::AntiAffinity => Some("anti_affinity"),
            RejectCountReason::NoDevice => Some("no_device"),
            RejectCountReason::Capture => Some("capture"),
            RejectCountReason::Cpu => Some("cpu"),
            RejectCountReason::Memory => Some("memory"),
            RejectCountReason::NetIn => Some("net_in"),
            RejectCountReason::NetOut => Some("net_out"),
            RejectCountReason::GpuEncode => Some("gpu_encode"),
            RejectCountReason::GpuDecode => Some("gpu_decode"),
            RejectCountReason::GpuMemory => Some("gpu_memory"),
            RejectCountReason::Contended => Some("contended"),
            RejectCountReason::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Grid {
    pub nodes: Vec<GridNodesItem>,
    /// 応答したノードが配置係（リーダー）ならその ID、違えば null
    #[serde(default)]
    pub scheduler: Option<String>,
    pub workloads: Vec<GridWorkloadsItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GridNodesItem {
    /// 仕事に貸せる分（`--capacity-cpu-millis`・`--capacity-memory-mb`・`--net-mbps`、既定は実機の 90%・85%・10000）
    pub capacity: Resources,
    pub capture: Vec<GridNodesItemCaptureItem>,
    pub draining: bool,
    pub gpus: Vec<GridNodesItemGpusItem>,
    pub id: String,
    /// `--label` の値と、予約のラベル `senca.io/name`（ノードの名前）
    pub labels: BTreeMap<String, String>,
    /// 最後の心拍（配置係のノードだけが知っている。違えば null）
    #[serde(default)]
    pub last_heartbeat: Option<Timestamp>,
    /// ノードの名前（`GET /nodes` に無ければ null）
    #[serde(default)]
    pub name: Option<String>,
    /// 割り当てた仕事の要求（今の段）の和
    pub reserved: Resources,
    /// control = Raft の投票者、media = 仕事を動かす（仕事は media にだけ置く）
    pub roles: Vec<GridNodesItemRolesItem>,
    /// 心拍が届いている（5 秒途絶えると false）
    pub up: bool,
    /// 最後の心拍の実測の使用量（CPU は /proc/stat の差、メモリは /proc/meminfo の MemTotal − MemAvailable、帯域はループバックを除く /proc/net/dev の差）。配置係のノードだけが知っている。違えば null。配置は予約（reserved）で数え、この値は使わない
    #[serde(default)]
    pub usage: Option<Resources>,
    /// このノードに割り当てた仕事の数
    pub workloads: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridWorkloadsItem {
    #[serde(default)]
    pub device: Option<String>,
    /// 資源から導いた仕事の ID（`input:{id}`・`output:{id}`・`pipeline:{id}`・`room:{id}`）
    pub id: String,
    /// 入力は `ingest`（`device` は `capture`）、出力は `egress`、パイプラインは `pipeline`、部屋は `room`
    pub kind: GridWorkloadsItemKind,
    /// 今の縮退の段（0 = full）
    #[serde(default)]
    pub level: Option<u64>,
    #[serde(default)]
    pub level_label: Option<String>,
    #[serde(default)]
    pub node: Option<String>,
    /// `placement.node`・`placement.device` で手動で割り当てた
    pub pinned: bool,
    pub priority: PlacementPriority,
    #[serde(default)]
    pub reason: Option<GridReason>,
    pub state: GridWorkloadsItemState,
    #[serde(default)]
    pub unschedulable: Option<Vec<RejectCount>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GridNodesItemCaptureItem {
    /// 運用者が手で止めた（止めたときだけ現れる）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    pub healthy: bool,
    /// 機器の ID（`GET /devices` の `capture.devices[].id`、例: `synthetic:0`）
    pub id: String,
    /// 機器の種類（機器の一覧の `backend`、例: `synthetic`・`v4l2`）
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GridNodesItemGpusItem {
    pub decode_reserved: u64,
    pub decode_sessions: u64,
    /// 運用者が手で止めていない（`disable`・`enable`）。仕事を置くのは healthy かつ enabled の装置だけ
    pub enabled: bool,
    pub encode_reserved: u64,
    pub encode_sessions: u64,
    /// 測った健康（心拍で変わる）
    pub healthy: bool,
    /// `{provider}:{装置の番号}`（例: `nvidia:0`）
    pub id: String,
    /// `nvidia`・`intel`・`amd`・`vaapi` など
    pub vendor: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GridNodesItemRolesItem {
    #[serde(rename = "control")]
    Control,
    #[serde(rename = "media")]
    Media,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl GridNodesItemRolesItem {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            GridNodesItemRolesItem::Control => Some("control"),
            GridNodesItemRolesItem::Media => Some("media"),
            GridNodesItemRolesItem::Unknown => None,
        }
    }
}

/// 入力は `ingest`（`device` は `capture`）、出力は `egress`、パイプラインは `pipeline`、部屋は `room`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GridWorkloadsItemKind {
    #[serde(rename = "ingest")]
    Ingest,
    #[serde(rename = "room")]
    Room,
    #[serde(rename = "pipeline")]
    Pipeline,
    #[serde(rename = "recording")]
    Recording,
    #[serde(rename = "transcode")]
    Transcode,
    #[serde(rename = "capture")]
    Capture,
    #[serde(rename = "relay")]
    Relay,
    #[serde(rename = "media_pipeline")]
    MediaPipeline,
    #[serde(rename = "egress")]
    Egress,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl GridWorkloadsItemKind {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            GridWorkloadsItemKind::Ingest => Some("ingest"),
            GridWorkloadsItemKind::Room => Some("room"),
            GridWorkloadsItemKind::Pipeline => Some("pipeline"),
            GridWorkloadsItemKind::Recording => Some("recording"),
            GridWorkloadsItemKind::Transcode => Some("transcode"),
            GridWorkloadsItemKind::Capture => Some("capture"),
            GridWorkloadsItemKind::Relay => Some("relay"),
            GridWorkloadsItemKind::MediaPipeline => Some("media_pipeline"),
            GridWorkloadsItemKind::Egress => Some("egress"),
            GridWorkloadsItemKind::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GridWorkloadsItemState {
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "degraded")]
    Degraded,
    #[serde(rename = "unschedulable")]
    Unschedulable,
    #[serde(rename = "pending")]
    Pending,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl GridWorkloadsItemState {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            GridWorkloadsItemState::Running => Some("running"),
            GridWorkloadsItemState::Degraded => Some("degraded"),
            GridWorkloadsItemState::Unschedulable => Some("unschedulable"),
            GridWorkloadsItemState::Pending => Some("pending"),
            GridWorkloadsItemState::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DeviceHealth {
    pub device: String,
    /// disable は false、enable は true
    pub enabled: bool,
    /// ノードの ID（名前で指定しても ID で返す）
    pub node: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NodeList {
    pub items: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Devices {
    pub nodes: Vec<DevicesNodesItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DevicesNodesItem {
    /// このノードのキャプチャ機器（REQ-004、隔離した `senca-capture-worker` に問い合わせる。入力の `protocol: device` で使う）。
    /// worker が無い・失敗したときは `{available: false, reason}`、ランタイムが無ければ null。
    pub capture: Value,
    /// コーデック worker のプールが動いていなければ `providers` は空、`sandbox` は null。
    pub devices: DeviceStatus,
    pub node: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DeviceStatus {
    pub providers: Vec<DeviceStatusProvidersItem>,
    /// worker のサンドボックスの種類（worker がまだ情報を返していなければ null）
    #[serde(default)]
    pub sandbox: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DeviceStatusProvidersItem {
    pub available: bool,
    /// 使えない理由、または読み込んだライブラリの説明
    pub detail: String,
    /// コーデックのプロバイダー（例: nvenc、dav1d）
    pub provider: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscodeCapability {
    pub codec: String,
    pub device: String,
    pub direction: TranscodeCapabilityDirection,
    pub hardware: bool,
    pub max_height: u64,
    #[serde(default)]
    pub max_sessions: Option<u64>,
    pub max_width: u64,
    pub provider: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TranscodeCapabilityDirection {
    #[serde(rename = "encode")]
    Encode,
    #[serde(rename = "decode")]
    Decode,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl TranscodeCapabilityDirection {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            TranscodeCapabilityDirection::Encode => Some("encode"),
            TranscodeCapabilityDirection::Decode => Some("decode"),
            TranscodeCapabilityDirection::Unknown => None,
        }
    }
}

/// `secret:{name}`（`POST /secrets` で登録した秘密値の名前）。無い秘密値・形の違いは 400。
pub type CredentialRef = Option<String>;

/// プロトコルごとの設定（例: rtsp_pull の `allow_basic: "true"`・`tls_spki_sha256`（rtsps の公開鍵の固定。SHA-256 の base64 か 16 進）、device の `width`・`height`・`fps`・`codec`・`bitrate_bps`・`audio_device`・`audio_codec`）。16 個まで、キー 64 文字・値 256 文字まで。RTSP は TCP interleaved だけで受ける。
pub type IoOptions = BTreeMap<String, String>;

/// このノードでの実行の様子（制御状態には保存しない）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IoStatus {
    /// 送った・受けたバイト数（目安）
    pub bytes: u64,
    /// 接続に成功した回数
    pub connects: u64,
    /// 最後の失敗の理由（宛先の拒否は「… は許されていない宛先です」）。成功しても消えない
    #[serde(default)]
    pub last_error: Option<String>,
    /// 今の state になった時刻
    #[serde(default)]
    pub since: Option<String>,
    /// starting = 実行を始めた、provisioning = PaaS の資源を用意している、waiting_input = 出力で配信を待っている、connecting = 接続中、
    /// active = 送受信している、backoff = 失敗の後の待ち（1〜60 秒の指数バックオフ）、error = 失敗した（すぐ backoff になる）、ended = VOD の HLS を読み終えた（`hls_pull` だけ）。
    pub state: IoStatusState,
}

/// starting = 実行を始めた、provisioning = PaaS の資源を用意している、waiting_input = 出力で配信を待っている、connecting = 接続中、
/// active = 送受信している、backoff = 失敗の後の待ち（1〜60 秒の指数バックオフ）、error = 失敗した（すぐ backoff になる）、ended = VOD の HLS を読み終えた（`hls_pull` だけ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IoStatusState {
    #[serde(rename = "starting")]
    Starting,
    #[serde(rename = "provisioning")]
    Provisioning,
    #[serde(rename = "waiting_input")]
    WaitingInput,
    #[serde(rename = "connecting")]
    Connecting,
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "backoff")]
    Backoff,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "ended")]
    Ended,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl IoStatusState {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            IoStatusState::Starting => Some("starting"),
            IoStatusState::Provisioning => Some("provisioning"),
            IoStatusState::WaitingInput => Some("waiting_input"),
            IoStatusState::Connecting => Some("connecting"),
            IoStatusState::Active => Some("active"),
            IoStatusState::Backoff => Some("backoff"),
            IoStatusState::Error => Some("error"),
            IoStatusState::Ended => Some("ended"),
            IoStatusState::Unknown => None,
        }
    }
}

/// 優先度（low < normal < high < critical）。資源が足りないとき、低い仕事から縮退・追い出しする。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlacementPriority {
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "normal")]
    Normal,
    #[serde(rename = "high")]
    High,
    #[serde(rename = "critical")]
    Critical,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PlacementPriority {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PlacementPriority::Low => Some("low"),
            PlacementPriority::Normal => Some("normal"),
            PlacementPriority::High => Some("high"),
            PlacementPriority::Critical => Some("critical"),
            PlacementPriority::Unknown => None,
        }
    }
}

/// ノードのラベル（`--label`、予約の `senca.io/name`）の条件。`op` で種類を選ぶ。キー・値は 1〜128 文字で制御文字を含めない。
pub type LabelSelector = Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DegradeLevel {
    /// この段で止めるグラフの部品の id（グラフに無ければ 400）。下流は入力がすべて止まれば止まる。段は積み重なる（段 2 は段 1 の分も止める）
    pub disable: Vec<PipelineNodeId>,
    /// 段の名前（例: `no_translation`・`audio_only`）。`full`（段 0 の名前）と重複は不可
    pub label: String,
}

/// 配置の指定（ADR-0013）。省略・null なら配置係に任せる（優先度 normal、spread）。検査の失敗は 400（`INVALID_CONFIG`、未知のフィールド・型の違いは `INVALID_REQUEST`）:
/// `node` が無い、文字列が空・128 文字超・制御文字を含む、`labels` が 17 個以上、`values` が空・33 個以上、`degrade` を入力・出力に書いた、
/// `degrade` が 9 段以上・段の名前が `full` か重複・`disable` が空・グラフに無い部品。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PlacementSpec {
    /// 組の名前。同じ組の仕事は同じノードに置かない
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anti_affinity: Option<String>,
    /// 縮退の段（**パイプラインだけ**、よい順）。資源が足りないとき、段を下げて（部品を止めて）動かし続ける。資源が戻れば元の段へ戻す
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degrade: Option<Vec<DegradeLevel>>,
    /// 手動の割り当て。装置の ID（GPU・キャプチャ機器、`GET /grid`）。`node` が無ければ、その装置を持つノードのどれか
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// すべてに合うノードにだけ置く
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<LabelSelector>>,
    /// 手動の割り当て。ノードの ID か名前（`GET /nodes`）。保存するときは ID に直す（応答は ID）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// 手動の割り当て先が止まったとき。prefer = 他へ移し、戻ったら（冷却の後に）戻す、strict = 止めたまま（`placement_status.state: unschedulable`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin_policy: Option<PlacementSpecPinPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<PlacementPriority>,
    /// spread = 余裕の多いノードへ、binpack = 詰める
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<PlacementSpecStrategy>,
}

/// 手動の割り当て先が止まったとき。prefer = 他へ移し、戻ったら（冷却の後に）戻す、strict = 止めたまま（`placement_status.state: unschedulable`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlacementSpecPinPolicy {
    #[serde(rename = "strict")]
    Strict,
    #[serde(rename = "prefer")]
    Prefer,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PlacementSpecPinPolicy {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PlacementSpecPinPolicy::Strict => Some("strict"),
            PlacementSpecPinPolicy::Prefer => Some("prefer"),
            PlacementSpecPinPolicy::Unknown => None,
        }
    }
}

/// spread = 余裕の多いノードへ、binpack = 詰める
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlacementSpecStrategy {
    #[serde(rename = "spread")]
    Spread,
    #[serde(rename = "binpack")]
    Binpack,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PlacementSpecStrategy {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PlacementSpecStrategy::Spread => Some("spread"),
            PlacementSpecStrategy::Binpack => Some("binpack"),
            PlacementSpecStrategy::Unknown => None,
        }
    }
}

/// 保存した配置の指定（`node` は ID に直したもの）。無い項目・空の配列は省く。`pin_policy`・`priority`・`strategy` は既定値でも出る。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacementSpecView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anti_affinity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degrade: Option<Vec<DegradeLevel>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<LabelSelector>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub pin_policy: PlacementSpecPinPolicy,
    pub priority: PlacementPriority,
    pub strategy: PlacementSpecStrategy,
}

/// 配置の結果（グリッドの割り当て）。`state` で形が変わる:
/// - `not_scheduled`: 仕事になっていない（入力・出力の `enabled: false`、パイプラインの `desired_state: stopped`）。
/// - `running`・`degraded`: 割り当てた（`degraded` は縮退の段 `level` が 1 以上）。割り当てたノードだけが動かす（実際の実行の様子は `status`・`observed_state`）。
/// - `pending`: 置き場所を探している（作成・変更の直後。配置係は約 1 秒ごと、資源の変更のたびに計画する）。
/// - `unschedulable`: 置けるノードが無い（`reasons` に理由ごとのノード）。
pub type PlacementStatus = Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputCreate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<CredentialRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<IoOptions>,
    /// 配置の指定（`degrade` は使えない）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementSpec>,
    pub protocol: InputCreateProtocol,
    /// 取りに行く URL。スキームは protocol で決まる（rtmp_pull: `rtmp`・`rtmps`、rtsp_pull: `rtsp`・`rtsps`、srt_pull: `srt`、hls_pull: `http`・`https`、device: `device:{機器 ID}`（300 文字まで））。
    /// ホストが要る。空白・制御文字は不可。資格情報（`user:pass@`）は入れられない（`credential_ref` を使う）。
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InputCreateProtocol {
    #[serde(rename = "rtmp_pull")]
    RtmpPull,
    #[serde(rename = "rtsp_pull")]
    RtspPull,
    #[serde(rename = "srt_pull")]
    SrtPull,
    #[serde(rename = "hls_pull")]
    HlsPull,
    #[serde(rename = "device")]
    Device,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl InputCreateProtocol {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            InputCreateProtocol::RtmpPull => Some("rtmp_pull"),
            InputCreateProtocol::RtspPull => Some("rtsp_pull"),
            InputCreateProtocol::SrtPull => Some("srt_pull"),
            InputCreateProtocol::HlsPull => Some("hls_pull"),
            InputCreateProtocol::Device => Some("device"),
            InputCreateProtocol::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputCreate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<CredentialRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<IoOptions>,
    /// 配置の指定（`degrade` は使えない）。指定が無ければ、配信を受けたノードを好む
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementSpec>,
    pub protocol: OutputCreateProtocol,
    /// `paas` で必須（`credential_ref` も必須）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<OutputCreateProvider>,
    /// `paas` 以外で必須。rtmp_push: `rtmp`・`rtmps`、srt_push: `srt`。資格情報は入れられない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OutputCreateProtocol {
    #[serde(rename = "rtmp_push")]
    RtmpPush,
    #[serde(rename = "srt_push")]
    SrtPush,
    #[serde(rename = "paas")]
    Paas,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl OutputCreateProtocol {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            OutputCreateProtocol::RtmpPush => Some("rtmp_push"),
            OutputCreateProtocol::SrtPush => Some("srt_push"),
            OutputCreateProtocol::Paas => Some("paas"),
            OutputCreateProtocol::Unknown => None,
        }
    }
}

/// `paas` で必須（`credential_ref` も必須）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OutputCreateProvider {
    #[serde(rename = "cloudflare_stream")]
    CloudflareStream,
    #[serde(rename = "aws_ivs")]
    AwsIvs,
    #[serde(rename = "aws_medialive")]
    AwsMedialive,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl OutputCreateProvider {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            OutputCreateProvider::CloudflareStream => Some("cloudflare_stream"),
            OutputCreateProvider::AwsIvs => Some("aws_ivs"),
            OutputCreateProvider::AwsMedialive => Some("aws_medialive"),
            OutputCreateProvider::Unknown => None,
        }
    }
}

/// 書いたフィールドだけ置き換える。`credential_ref: null` で参照を外す、`placement: null` で配置の指定を外す（配置係に任せる）。他の null は「変えない」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct IoPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<CredentialRef>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub enabled: Option<Option<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub options: Option<Option<IoOptions>>,
    /// 配置の指定を丸ごと置き換える（部分の変更は無い。`degrade` は使えない）。null で外す
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub placement: Option<Option<PlacementSpec>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub url: Option<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Input {
    pub created_at: Timestamp,
    pub credential_ref: CredentialRef,
    pub enabled: bool,
    pub generation: u64,
    pub id: String,
    pub options: BTreeMap<String, String>,
    /// 配置の指定。指定が無ければ null
    #[serde(default)]
    pub placement: Option<PlacementSpecView>,
    pub placement_status: PlacementStatus,
    pub protocol: InputCreateProtocol,
    /// このノードでの実行の様子。`enabled: false`、実行をまだ始めていない、またはこのノードに割り当てられていなければ null（分散のときは、割り当てたノード `placement_status.node` に問い合わせる）。作成・PATCH の応答では、実行の作り直しが非同期なので前の実行のもの（または null）のことがある。
    #[serde(default)]
    pub status: Option<IoStatus>,
    pub stream: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct InputList {
    pub items: Vec<Input>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Output {
    pub created_at: Timestamp,
    pub credential_ref: CredentialRef,
    pub enabled: bool,
    pub generation: u64,
    pub id: String,
    pub options: BTreeMap<String, String>,
    /// 配置の指定。指定が無ければ null
    #[serde(default)]
    pub placement: Option<PlacementSpecView>,
    pub placement_status: PlacementStatus,
    pub protocol: OutputCreateProtocol,
    #[serde(default)]
    pub provider: Option<OutputCreateProvider>,
    /// SENCA が PaaS に作った資源（後片付けに使う）
    #[serde(default)]
    pub provider_resource: Option<String>,
    /// このノードでの実行の様子。`enabled: false`、実行をまだ始めていない、またはこのノードに割り当てられていなければ null（分散のときは、割り当てたノード `placement_status.node` に問い合わせる）。作成・PATCH の応答では、実行の作り直しが非同期なので前の実行のもの（または null）のことがある。
    #[serde(default)]
    pub status: Option<IoStatus>,
    pub stream: String,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OutputList {
    pub items: Vec<Output>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SecretCreate {
    pub name: StreamName,
    /// 1〜8192 バイト（UTF-8）。応答には出ない
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Secret {
    pub created_at: Timestamp,
    pub id: String,
    pub name: String,
    /// `credential_ref` に書く値
    pub r#ref: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SecretList {
    pub items: Vec<Secret>,
    /// 常に null（ページングは無い）
    pub next_cursor: Value,
}

/// パイプラインの定義（senca-pipeline の `Graph`、`crates/senca-pipeline/src/model.rs`）。有向非巡回グラフで、知らない項目は拒否する。大きさは 16 MiB まで（ただし HTTP 層の本文の上限 1 MiB が先にかかる）。
/// 部品（`nodes`）は `type` で種類を選ぶ。入力の参照は `"部品"` または `"部品.ポート"`（`source` のポートは `video`・`audio` で省けない、`compose` は `program`（既定）・`preview`、他は `out`）。
/// 部品ごとの設定のすべては crate の文書（`cargo doc -p senca-pipeline`）を見る。ここでは種類と必須の項目だけを書く。値の範囲は検証（`POST /pipelines/{id}/validate`）で調べる。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PipelineGraph {
    pub nodes: Vec<PipelineNode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<PipelineOptions>,
    pub version: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PipelineOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<PipelineOptionsClock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_queue: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_queue_aus: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow: Option<PipelineOptionsOverflow>,
    /// 状態・メーターの更新の間隔（0 で出さない）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_interval_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PipelineOptionsClock {
    #[serde(rename = "realtime")]
    Realtime,
    #[serde(rename = "input")]
    Input,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PipelineOptionsClock {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PipelineOptionsClock::Realtime => Some("realtime"),
            PipelineOptionsClock::Input => Some("input"),
            PipelineOptionsClock::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PipelineOptionsOverflow {
    #[serde(rename = "drop")]
    Drop,
    #[serde(rename = "block")]
    Block,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PipelineOptionsOverflow {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PipelineOptionsOverflow::Drop => Some("drop"),
            PipelineOptionsOverflow::Block => Some("block"),
            PipelineOptionsOverflow::Unknown => None,
        }
    }
}

/// 部品。`type` ごとの必須の項目だけを書く（他の項目は crate の文書）。`id` は英数字・`_`・`-` の 1〜64 文字でグラフ内で一意。
pub type PipelineNode = Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PipelineNodeType {
    #[serde(rename = "source")]
    Source,
    #[serde(rename = "decode")]
    Decode,
    #[serde(rename = "board")]
    Board,
    #[serde(rename = "scale")]
    Scale,
    #[serde(rename = "crop")]
    Crop,
    #[serde(rename = "compose")]
    Compose,
    #[serde(rename = "audio_mix")]
    AudioMix,
    #[serde(rename = "encode")]
    Encode,
    #[serde(rename = "snapshot_on_change")]
    SnapshotOnChange,
    #[serde(rename = "sink")]
    Sink,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PipelineNodeType {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PipelineNodeType::Source => Some("source"),
            PipelineNodeType::Decode => Some("decode"),
            PipelineNodeType::Board => Some("board"),
            PipelineNodeType::Scale => Some("scale"),
            PipelineNodeType::Crop => Some("crop"),
            PipelineNodeType::Compose => Some("compose"),
            PipelineNodeType::AudioMix => Some("audio_mix"),
            PipelineNodeType::Encode => Some("encode"),
            PipelineNodeType::SnapshotOnChange => Some("snapshot_on_change"),
            PipelineNodeType::Sink => Some("sink"),
            PipelineNodeType::Unknown => None,
        }
    }
}

pub type PipelineNodeId = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PipelineSourceTrack {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<i64>,
    pub codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
}

/// 検証の失敗（400・503 の `details.issues` の要素）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineIssue {
    pub code: PipelineIssueCode,
    pub message: String,
    /// 問題の部品の id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PipelineIssueCode {
    #[serde(rename = "MALFORMED")]
    Malformed,
    #[serde(rename = "INVALID_PARAM")]
    InvalidParam,
    #[serde(rename = "DUPLICATE_ID")]
    DuplicateId,
    #[serde(rename = "UNKNOWN_REFERENCE")]
    UnknownReference,
    #[serde(rename = "TYPE_MISMATCH")]
    TypeMismatch,
    #[serde(rename = "CYCLE")]
    Cycle,
    #[serde(rename = "UNCONSUMED_OUTPUT")]
    UnconsumedOutput,
    #[serde(rename = "POLICY_REJECTED")]
    PolicyRejected,
    #[serde(rename = "UNSUPPORTED_CODEC")]
    UnsupportedCodec,
    #[serde(rename = "CAPACITY_UNAVAILABLE")]
    CapacityUnavailable,
    #[serde(rename = "LIMIT_EXCEEDED")]
    LimitExceeded,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PipelineIssueCode {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PipelineIssueCode::Malformed => Some("MALFORMED"),
            PipelineIssueCode::InvalidParam => Some("INVALID_PARAM"),
            PipelineIssueCode::DuplicateId => Some("DUPLICATE_ID"),
            PipelineIssueCode::UnknownReference => Some("UNKNOWN_REFERENCE"),
            PipelineIssueCode::TypeMismatch => Some("TYPE_MISMATCH"),
            PipelineIssueCode::Cycle => Some("CYCLE"),
            PipelineIssueCode::UnconsumedOutput => Some("UNCONSUMED_OUTPUT"),
            PipelineIssueCode::PolicyRejected => Some("POLICY_REJECTED"),
            PipelineIssueCode::UnsupportedCodec => Some("UNSUPPORTED_CODEC"),
            PipelineIssueCode::CapacityUnavailable => Some("CAPACITY_UNAVAILABLE"),
            PipelineIssueCode::LimitExceeded => Some("LIMIT_EXCEEDED"),
            PipelineIssueCode::Unknown => None,
        }
    }
}

/// 実行の計画（処理の形・必要な資源・追加の遅延）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PipelinePlan {
    pub nodes: Vec<PipelinePlanNodesItem>,
    /// 実行の順（トポロジカル順）
    pub order: Vec<String>,
    pub sinks: Vec<PipelinePlanSinksItem>,
    pub totals: PipelinePlanTotals,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelinePlanNodesItem {
    pub added_latency_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    pub cpu_codec_sessions: u64,
    pub gpu_codec_sessions: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware: Option<bool>,
    pub id: String,
    pub mode: PipelinePlanNodesItemMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<Vec<String>>,
    /// 出力の型の説明
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<String>>,
    pub pixels_per_sec: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub r#type: PipelineNodeType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PipelinePlanSinksItem {
    pub id: String,
    pub stream: String,
    pub tracks: Vec<PipelinePlanSinksItemTracksItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PipelinePlanTotals {
    pub cpu_codec_sessions: u64,
    pub gpu_codec_sessions: u64,
    pub max_latency_ms: f64,
    pub pixels_per_sec: u64,
    pub threads: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PipelinePlanNodesItemMode {
    #[serde(rename = "passthrough")]
    Passthrough,
    #[serde(rename = "transcode")]
    Transcode,
    #[serde(rename = "process")]
    Process,
    #[serde(rename = "terminal")]
    Terminal,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PipelinePlanNodesItemMode {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PipelinePlanNodesItemMode::Passthrough => Some("passthrough"),
            PipelinePlanNodesItemMode::Transcode => Some("transcode"),
            PipelinePlanNodesItemMode::Process => Some("process"),
            PipelinePlanNodesItemMode::Terminal => Some("terminal"),
            PipelinePlanNodesItemMode::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelinePlanSinksItemTracksItem {
    pub codec: String,
    pub from: String,
    pub kind: PipelinePlanSinksItemTracksItemKind,
    pub latency_ms: f64,
    pub mode: PipelinePlanNodesItemMode,
    pub track: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PipelinePlanSinksItemTracksItemKind {
    #[serde(rename = "video")]
    Video,
    #[serde(rename = "audio")]
    Audio,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PipelinePlanSinksItemTracksItemKind {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PipelinePlanSinksItemTracksItemKind::Video => Some("video"),
            PipelinePlanSinksItemTracksItemKind::Audio => Some("audio"),
            PipelinePlanSinksItemTracksItemKind::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PipelineCreate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_state: Option<PipelineCreateDesiredState>,
    pub graph: PipelineGraph,
    pub name: StreamName,
    /// 配置の指定と縮退の段
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PipelineCreateDesiredState {
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "stopped")]
    Stopped,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PipelineCreateDesiredState {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PipelineCreateDesiredState::Running => Some("running"),
            PipelineCreateDesiredState::Stopped => Some("stopped"),
            PipelineCreateDesiredState::Unknown => None,
        }
    }
}

/// 書いたフィールドだけ置き換える（null は「変えない」。ただし `placement: null` は配置の指定を外す）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PipelinePatch {
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub desired_state: Option<Option<PipelineCreateDesiredState>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub graph: Option<Option<PipelineGraph>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub name: Option<Option<StreamName>>,
    /// 配置の指定を丸ごと置き換える（部分の変更は無い）。null で外す。書かなければ今の指定を新しい `graph` で検査し直す（縮退の段の `disable` が新しいグラフに無ければ 400）
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub placement: Option<Option<PlacementSpec>>,
}

/// パイプラインの直近の通知（senca-pipeline の `Event`、`event` で種類を選ぶ）。
/// `event: status`（`status_interval_ms` ごと）は `sources`（入力ごとの state `waiting`・`live`・`stalled`・`ended`、AU 数、タリー `on_program`/`on_preview`）、
/// `compose`（シーン・トランジション中か・スレートを出している入力）、`encoders`（フレーム数・誤り）を持つ。
/// メーター（`event: meters`）は `meters` に入り、ここにはそれ以外の直近の通知が残る（世代・誤りの通知のこともある）。
pub type PipelineStatus = Value;

/// `audio_mix` の部品の id → 直近のメーター（`Event::Meters`: `inputs`・`master` の peak/rms dBFS、`limiter_reduction_db`）。
pub type PipelineMeters = BTreeMap<String, PipelineMetersValue>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PipelineMetersValue {
    pub event: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pipeline {
    pub created_at: Timestamp,
    pub desired_state: StreamCreateDesiredState,
    pub generation: u64,
    pub graph: PipelineGraph,
    pub id: String,
    /// 直近の失敗（例: `codec worker が動いていません`）。動き出すと null に戻る
    #[serde(default)]
    pub last_error: Option<String>,
    /// 実行していなければ null、実行中は `audio_mix` の部品の id → 直近のメーター（ミキサーが無い・まだ通知が無ければ `{}`）。
    #[serde(default)]
    pub meters: Option<PipelineMeters>,
    pub name: String,
    /// このノードでの実行の様子。starting = セッションを開いている（`desired_state: running` でまだ実行を始めていないときも）、running = 動いている、
    /// error = 失敗して作り直しを待っている（`last_error`、1〜32 秒のバックオフ）、stopped = 止まっている。
    pub observed_state: PipelineObservedState,
    /// 配置の指定。指定が無ければ null
    #[serde(default)]
    pub placement: Option<PlacementSpecView>,
    pub placement_status: PlacementStatus,
    /// 実行中の計画（worker が開いたセッションのもの）。動いていなければ null（検証の計画は `POST /pipelines/{id}/validate`）。
    #[serde(default)]
    pub plan: Option<PipelinePlan>,
    /// 直近の状態の通知。まだ無ければ null。
    #[serde(default)]
    pub status: Option<PipelineStatus>,
    pub updated_at: Timestamp,
}

/// このノードでの実行の様子。starting = セッションを開いている（`desired_state: running` でまだ実行を始めていないときも）、running = 動いている、
/// error = 失敗して作り直しを待っている（`last_error`、1〜32 秒のバックオフ）、stopped = 止まっている。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PipelineObservedState {
    #[serde(rename = "starting")]
    Starting,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "stopped")]
    Stopped,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PipelineObservedState {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PipelineObservedState::Starting => Some("starting"),
            PipelineObservedState::Running => Some("running"),
            PipelineObservedState::Error => Some("error"),
            PipelineObservedState::Stopped => Some("stopped"),
            PipelineObservedState::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PipelineList {
    pub items: Vec<Pipeline>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PipelineValidation {
    pub ok: Value,
    pub plan: PipelinePlan,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PipelineTransition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<PipelineTransitionKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PipelineTransitionKind {
    #[serde(rename = "cut")]
    Cut,
    #[serde(rename = "fade")]
    Fade,
    #[serde(rename = "wipe_left")]
    WipeLeft,
    #[serde(rename = "wipe_right")]
    WipeRight,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl PipelineTransitionKind {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            PipelineTransitionKind::Cut => Some("cut"),
            PipelineTransitionKind::Fade => Some("fade"),
            PipelineTransitionKind::WipeLeft => Some("wipe_left"),
            PipelineTransitionKind::WipeRight => Some("wipe_right"),
            PipelineTransitionKind::Unknown => None,
        }
    }
}

/// シーンのレイヤー（senca-pipeline の `LayerSpec`: source・image・color のどれか、x・y・width・height、crop・fit・z・opacity・corner_radius・border・visible・filter）
pub type PipelineLayer = Value;

/// 操作（senca-pipeline の `Control`、`op` で種類を選ぶ。知らない項目は 400）。`node` は対象の部品の id（`compose` または `audio_mix`）。
/// `prepare`・`commit`・`rollback` は型としては読めるが、この API では 400（グラフの変更は `PATCH /pipelines/{id}`）。
pub type PipelineControl = Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PipelineControlResult {
    pub ok: Value,
    /// 操作の返事（今の操作では空のオブジェクト）
    pub reply: Value,
}

/// 部屋の ID（1〜128 文字の英数字と `-_.:@`、テナントをまたいで一意）
pub type RoomId = String;

/// 部屋・参加者の metadata（任意の JSON、JSON にして 8 KiB まで）。参加者の metadata は他の参加者に見える
pub type RoomMetadata = Value;

/// スポットライト（話している人を注目し、注目の外の映像を下げる）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RoomSpotlight {
    /// 注目した送り手の層（無ければ帯域で自動）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_rid: Option<String>,
    /// 注目する話者の数（受け手は自分を除いてこの人数）
    pub number: u64,
    /// lowest = 注目の外は一番低い層、off = 配らない（枠は `paused`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unfocused: Option<RoomSpotlightUnfocused>,
}

/// lowest = 注目の外は一番低い層、off = 配らない（枠は `paused`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RoomSpotlightUnfocused {
    #[serde(rename = "lowest")]
    Lowest,
    #[serde(rename = "False")]
    False,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl RoomSpotlightUnfocused {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            RoomSpotlightUnfocused::Lowest => Some("lowest"),
            RoomSpotlightUnfocused::False => Some("False"),
            RoomSpotlightUnfocused::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForwardingRule {
    /// 送り手の属性: `connection_id`・`client_id`・`bundle_id`・`kind`・`metadata.<key>`（key は 1〜64 文字。値が文字列・数・真偽のとき）
    pub field: String,
    pub operator: ForwardingRuleOperator,
    /// `kind` なら `audio`・`video`
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ForwardingRuleOperator {
    #[serde(rename = "is_in")]
    IsIn,
    #[serde(rename = "is_not_in")]
    IsNotIn,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl ForwardingRuleOperator {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            ForwardingRuleOperator::IsIn => Some("is_in"),
            ForwardingRuleOperator::IsNotIn => Some("is_not_in"),
            ForwardingRuleOperator::Unknown => None,
        }
    }
}

/// 転送フィルター（`docs/realtime/signaling.md` の 8 章）。`rules` は「AND の組」の OR。`block` は一致したものを配らない、`allow` は一致したものだけ配る。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForwardingFilter {
    pub action: ForwardingFilterAction,
    pub rules: Vec<Vec<ForwardingRule>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ForwardingFilterAction {
    #[serde(rename = "block")]
    Block,
    #[serde(rename = "allow")]
    Allow,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl ForwardingFilterAction {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            ForwardingFilterAction::Block => Some("block"),
            ForwardingFilterAction::Allow => Some("allow"),
            ForwardingFilterAction::Unknown => None,
        }
    }
}

/// 部屋の設定。応答では既定値を埋めた全体を返す（`max_video_bitrate_kbps`・`spotlight`・`forwarding_filter` は無ければ省く）。
/// API で検査するのは形と `max_participants` だけ（範囲の他の制約は部屋の核が部屋を開くときに検査する。`POST /rooms` の説明）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RoomSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<RoomSettingsAudioCodec>,
    /// 使ってよい DataChannel のラベルの型（`#` で始まる。末尾の `*` は前方一致）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_labels: Option<Vec<String>>,
    /// E2EE（SFrame）の部屋。SFU は中身を見ずに配り、合成・合成の録画はできない
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2ee: Option<bool>,
    /// 誰もいなくなってから消すまでの秒数（参加トークンで自動に作った部屋だけ）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_timeout_secs: Option<u64>,
    /// 部屋全体の転送フィルター（購読者ごとのフィルターと両方を満たすものだけ配る）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarding_filter: Option<ForwardingFilter>,
    /// 参加者の上限（ノードをまたぐ参加者も数える）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_participants: Option<u64>,
    /// 送り手に勧める映像の上限（kbit/s）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_video_bitrate_kbps: Option<u64>,
    /// 録画の方針（`composite` は `e2ee` と併用できない）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RoomSettingsRecording>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spotlight: Option<RoomSpotlight>,
    /// 部屋の映像のコーデック（SFU は変換しないので全員が同じ）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<RoomSettingsVideoCodec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RoomSettingsAudioCodec {
    #[serde(rename = "opus")]
    Opus,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl RoomSettingsAudioCodec {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            RoomSettingsAudioCodec::Opus => Some("opus"),
            RoomSettingsAudioCodec::Unknown => None,
        }
    }
}

/// 録画の方針（`composite` は `e2ee` と併用できない）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RoomSettingsRecording {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "per_participant")]
    PerParticipant,
    #[serde(rename = "composite")]
    Composite,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl RoomSettingsRecording {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            RoomSettingsRecording::None => Some("none"),
            RoomSettingsRecording::PerParticipant => Some("per_participant"),
            RoomSettingsRecording::Composite => Some("composite"),
            RoomSettingsRecording::Unknown => None,
        }
    }
}

/// 部屋の映像のコーデック（SFU は変換しないので全員が同じ）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RoomSettingsVideoCodec {
    #[serde(rename = "vp8")]
    Vp8,
    #[serde(rename = "vp9")]
    Vp9,
    #[serde(rename = "h264")]
    H264,
    #[serde(rename = "av1")]
    Av1,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl RoomSettingsVideoCodec {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            RoomSettingsVideoCodec::Vp8 => Some("vp8"),
            RoomSettingsVideoCodec::Vp9 => Some("vp9"),
            RoomSettingsVideoCodec::H264 => Some("h264"),
            RoomSettingsVideoCodec::Av1 => Some("av1"),
            RoomSettingsVideoCodec::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RoomCreate {
    /// 省けば作る（24 文字の 16 進）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<RoomId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<RoomMetadata>,
    /// 配置の指定（`degrade` は使えない）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementSpec>,
    /// 部屋の設定（省略・null は既定値）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<RoomSettings>,
}

/// 書いたフィールドだけ変える。`settings` は全体を置き換える。`settings`・`metadata` の null は「変えない」、`placement: null` は配置の指定を外す。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RoomPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<RoomMetadata>,
    /// 配置の指定を丸ごと置き換える（`degrade` は使えない）。null で外す
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub placement: Option<Option<PlacementSpec>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub settings: Option<Option<RoomSettings>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Room {
    /// 参加トークンの発行で自動に作った部屋（誰もいないまま `empty_timeout_secs` 経つと消える）
    pub auto: bool,
    pub created_at: Timestamp,
    pub generation: u64,
    pub id: RoomId,
    pub metadata: RoomMetadata,
    /// 配置の指定。指定が無ければ null
    #[serde(default)]
    pub placement: Option<PlacementSpecView>,
    pub placement_status: PlacementStatus,
    pub settings: RoomSettings,
    /// つなぐ先（`GET /signaling` の WebSocket）。部屋を別のノードに置いたらそのノードの URL。このノードの `--public-http-url` が無く、別のノードにも置いていなければ null
    #[serde(default)]
    pub signaling_url: Option<String>,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RoomList {
    pub items: Vec<Room>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// 参加者に許すこと（省けばすべて）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RoomPermissions {
    /// 使ってよい DataChannel のラベルの型（`#*` で全部）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Vec<String>>,
    /// 送ってよいトラックの種類
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish: Option<Vec<RoomPermissionsPublishItem>>,
    /// 他の参加者のトラックを受けてよいか
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribe: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RoomPermissionsPublishItem {
    #[serde(rename = "audio")]
    Audio,
    #[serde(rename = "video")]
    Video,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl RoomPermissionsPublishItem {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            RoomPermissionsPublishItem::Audio => Some("audio"),
            RoomPermissionsPublishItem::Video => Some("video"),
            RoomPermissionsPublishItem::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomTokenRequest {
    /// 同じ値の接続どうしは互いのトラックを受けない
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    /// あれば `connect.client_id` はこれと同じでなければならない（転送フィルターの条件になる）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// 参加者の metadata（他の参加者に見える）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<RoomMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<RoomPermissions>,
    /// 許す役割の上限（`sendrecv` のトークンはどの役割でも参加できる）
    pub role: RoomTokenRequestRole,
    /// 有効期間（秒）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

/// 許す役割の上限（`sendrecv` のトークンはどの役割でも参加できる）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RoomTokenRequestRole {
    #[serde(rename = "sendrecv")]
    Sendrecv,
    #[serde(rename = "sendonly")]
    Sendonly,
    #[serde(rename = "recvonly")]
    Recvonly,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl RoomTokenRequestRole {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            RoomTokenRequestRole::Sendrecv => Some("sendrecv"),
            RoomTokenRequestRole::Sendonly => Some("sendonly"),
            RoomTokenRequestRole::Recvonly => Some("recvonly"),
            RoomTokenRequestRole::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RoomToken {
    pub expires_at: Timestamp,
    pub room_id: RoomId,
    /// `Room.signaling_url` と同じ
    #[serde(default)]
    pub signaling_url: Option<String>,
    /// 参加トークン（JWT）。Idempotency-Key の再送では null
    #[serde(default)]
    pub token: Option<String>,
    /// Idempotency-Key の再送で値を伏せたとき
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_redacted: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomTrack {
    /// 送り手の接続
    pub connection_id: String,
    pub kind: RoomPermissionsPublishItem,
    /// サイマルキャストの層（低い順）。サイマルキャストでなければ省く
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rids: Option<Vec<String>>,
    pub track_id: String,
}

/// 受信の枠（クライアントの受ける m 行 1 つ）と、そこに今流しているトラック。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomSlot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    pub kind: RoomPermissionsPublishItem,
    pub mid: String,
    /// active = 配っている、paused = 割り当てているが配っていない、empty = 空き
    pub state: RoomSlotState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<String>,
}

/// active = 配っている、paused = 割り当てているが配っていない、empty = 空き
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RoomSlotState {
    #[serde(rename = "empty")]
    Empty,
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "paused")]
    Paused,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl RoomSlotState {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            RoomSlotState::Empty => Some("empty"),
            RoomSlotState::Active => Some("active"),
            RoomSlotState::Paused => Some("paused"),
            RoomSlotState::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Participant {
    pub bundle_id: String,
    /// 無ければ省く
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    pub connection_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joined_at: Option<Timestamp>,
    pub metadata: RoomMetadata,
    /// 他のノードにいる参加者（カスケード）のノード
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// WebRTC（ICE・DTLS）がつながっているか（他のノードの参加者は true）
    pub peer_connected: bool,
    pub role: RoomTokenRequestRole,
    /// シグナリングの経路（remote は他のノードの参加者）
    pub signaling: ParticipantSignaling,
    /// 受信の枠（他のノードの参加者は空）
    pub subscriptions: Vec<RoomSlot>,
    /// 送っているトラック
    pub tracks: Vec<RoomTrack>,
}

/// シグナリングの経路（remote は他のノードの参加者）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ParticipantSignaling {
    #[serde(rename = "websocket")]
    Websocket,
    #[serde(rename = "datachannel")]
    Datachannel,
    #[serde(rename = "remote")]
    Remote,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl ParticipantSignaling {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            ParticipantSignaling::Websocket => Some("websocket"),
            ParticipantSignaling::Datachannel => Some("datachannel"),
            ParticipantSignaling::Remote => Some("remote"),
            ParticipantSignaling::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ParticipantList {
    pub items: Vec<Participant>,
    /// 続きを読む `cursor`（続きがあるときだけ値、無ければ null）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ParticipantPatch {
    pub metadata: RoomMetadata,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RoomMessage {
    /// 送るもの（JSON で 64 KiB まで）。`label` があれば文字列
    pub data: Value,
    /// DataChannel のラベル（`#` で始まる）。無ければシグナリングの `push`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// 送る相手の connection_id（空なら全員）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RoomMessageResult {
    /// 送った相手の数
    pub delivered: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RoomLayerStats {
    #[serde(default)]
    pub bitrate_bps: Option<u64>,
    /// 層（層の無いトラックは null）
    #[serde(default)]
    pub rid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RoomParticipantStats {
    /// 受け手への推定帯域
    pub bandwidth_bps: u64,
    /// クライアントが送った getStats の要約（無ければ省く）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<Value>,
    pub connection_id: String,
    pub forwarded_bytes: u64,
    pub forwarded_frames: u64,
    pub nack_requested: u64,
    pub published: Vec<RoomParticipantStatsPublishedItem>,
    pub received_bytes: u64,
    pub retransmitted_packets: u64,
    pub rtt_ms: u64,
    /// この参加者へ送っている量
    pub send_bitrate_bps: u64,
    pub subscriptions: Vec<RoomParticipantStatsSubscriptionsItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomParticipantStatsPublishedItem {
    pub kind: RoomPermissionsPublishItem,
    pub layers: Vec<RoomLayerStats>,
    pub track_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomParticipantStatsSubscriptionsItem {
    pub mid: String,
    /// 今配っている層（層の無いトラック・まだなら null）
    #[serde(default)]
    pub rid: Option<String>,
    pub state: RoomSlotState,
    #[serde(default)]
    pub track_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RoomStats {
    pub at: Timestamp,
    /// 部屋を置いたノードにつないだ参加者
    pub participants: Vec<RoomParticipantStats>,
    pub room_id: RoomId,
}

/// テナント内で一意（作成のとき）。1〜40 文字の英数字と `-_.`（`.` で始まらない）。子の stream・パイプラインの名前の頭に使う
pub type MediaPipelineName = String;

/// running = 部屋を置いたノードの係が動かす、stopped = 止める（子の合成・送り出しを消す。書き出した stream・録画は残る）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaPipelineState {
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "stopped")]
    Stopped,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl MediaPipelineState {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            MediaPipelineState::Running => Some("running"),
            MediaPipelineState::Stopped => Some("stopped"),
            MediaPipelineState::Unknown => None,
        }
    }
}

/// 書き出す参加者の `client_id`（参加トークンの `client_id`）。空なら全員（`client_id` の無い参加者は `connection_id` で区別する）
pub type MediaPipelineParticipants = Vec<String>;

/// 合成（会議の 1 本の映像・音声）。書かなかったフィールドは既定値（応答ではすべて入る）。映像のある参加者を並べ、音声はすべて混ぜて Opus 96 kbit/s にする。
/// `grid` は格子（列数は人数の平方根の切り上げ）、`speaker` は最初に書き出した参加者を上に大きく、残りを下の 1/4 の帯に並べる（話者の検出はまだ合成に届かない）。
/// H.264・H.265 は GPU が要る。GOP は `fps` の 2 倍。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MediaPipelineComposite {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<MediaPipelineCompositeLayout>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<MediaPipelineCompositeVideoCodec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaPipelineCompositeLayout {
    #[serde(rename = "grid")]
    Grid,
    #[serde(rename = "speaker")]
    Speaker,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl MediaPipelineCompositeLayout {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            MediaPipelineCompositeLayout::Grid => Some("grid"),
            MediaPipelineCompositeLayout::Speaker => Some("speaker"),
            MediaPipelineCompositeLayout::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaPipelineCompositeVideoCodec {
    #[serde(rename = "av1")]
    Av1,
    #[serde(rename = "vp9")]
    Vp9,
    #[serde(rename = "vp8")]
    Vp8,
    #[serde(rename = "h264")]
    H264,
    #[serde(rename = "h265")]
    H265,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl MediaPipelineCompositeVideoCodec {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            MediaPipelineCompositeVideoCodec::Av1 => Some("av1"),
            MediaPipelineCompositeVideoCodec::Vp9 => Some("vp9"),
            MediaPipelineCompositeVideoCodec::Vp8 => Some("vp8"),
            MediaPipelineCompositeVideoCodec::H264 => Some("h264"),
            MediaPipelineCompositeVideoCodec::H265 => Some("h265"),
            MediaPipelineCompositeVideoCodec::Unknown => None,
        }
    }
}

/// 録画（AWS の media capture pipeline 相当）。書き出した stream の自動録画（`RecordingPolicy`）を有効にする。録画は普通の録画の API（`GET /recordings?stream=...`）で読む。
/// 録画の設定は stream を作るときに決まる（後から `recording` を変えても、作った stream には効かない）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MediaPipelineRecording {
    /// 合成の stream を録る（`composite` が要る）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composite: Option<bool>,
    /// 参加者ごとの stream を録る（`export_participants: true` が要る）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub participants: Option<bool>,
    /// 保持日数（省けば録画の既定。応答では省いたとき欄が無い）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u64>,
}

/// 送り出し（AWS の media live connector pipeline 相当）。合成の stream の出力（`Output`）として作る。形と検査は `OutputCreate` と同じ
/// （`paas` には `credential_ref` が要る）。応答では null・空のフィールドは欄が無い。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaPipelineEgress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<CredentialRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<IoOptions>,
    pub protocol: OutputCreateProtocol,
    /// `paas` で必須
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<OutputCreateProvider>,
    /// `paas` 以外で必須。rtmp_push: `rtmp`・`rtmps`、srt_push: `srt`。資格情報は入れられない。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MediaPipelineCreate {
    /// 文字起こし・翻訳（AWS の media insights pipeline 相当）。参加者ごとの stream の `captions` になる（省略・null は付けない）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captions: Option<CaptionSettings>,
    /// 合成（省略・null は合成しない）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composite: Option<MediaPipelineComposite>,
    /// 省略・null は running
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_state: Option<MediaPipelineState>,
    /// 送り出し（`composite` が要る）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<Vec<MediaPipelineEgress>>,
    /// 参加者ごとの stream を残す（AWS の media stream pipeline 相当）。false でも合成のために書き出す（`recording.participants`・`captions` は使えない）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export_participants: Option<bool>,
    pub name: MediaPipelineName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub participants: Option<MediaPipelineParticipants>,
    /// 録画（省略・null は録らない）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<MediaPipelineRecording>,
    /// 部屋の ID（テナントの部屋。無ければ 400）
    pub room: String,
}

/// 書いたフィールドだけ置き換える。`composite`・`recording`・`captions` の null は外す、他の null は「変えない」。`egress` は列ごと置き換える。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MediaPipelinePatch {
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub captions: Option<Option<CaptionSettings>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub composite: Option<Option<MediaPipelineComposite>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub desired_state: Option<Option<MediaPipelineState>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub egress: Option<Option<Vec<MediaPipelineEgress>>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub export_participants: Option<Option<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub name: Option<Option<MediaPipelineName>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub participants: Option<Option<MediaPipelineParticipants>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::double_option")]
    pub recording: Option<Option<MediaPipelineRecording>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MediaPipelineParticipantStream {
    /// stream の名前（`{name}-{参加者}-{ID の頭 6 文字}`）
    pub name: String,
    /// stream の ID
    pub stream: String,
}

/// 合成の子の資源の ID（メディアパイプラインの ID から決まる。まだ作っていなくても ID は出る）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MediaPipelineCompositeArtifacts {
    /// `egress` の順の送り出しの出力の ID（`GET /outputs/{id}`）
    pub outputs: Vec<String>,
    /// 合成のパイプラインの ID（`GET /pipelines/{id}`。映像のある参加者が流れ始めるまでは無いことがある）
    pub pipeline: String,
    /// 合成の stream の ID（`GET /streams/{id}`）
    pub stream: String,
}

/// 子の資源
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MediaPipelineArtifacts {
    /// `composite` が無ければ null
    #[serde(default)]
    pub composite: Option<MediaPipelineCompositeArtifacts>,
    /// 書き出した参加者ごとの stream（`metadata.media_pipeline` がこの ID の stream。今いない参加者のものも残る）。ID の昇順
    pub participant_streams: Vec<MediaPipelineParticipantStream>,
}

/// このノードでの実行の様子（制御状態には保存しない）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MediaPipelineStatus {
    /// 合成の図に入っている参加者の数（`composite` が無い、または映像のある参加者が流れ始めていなければ null）
    #[serde(default)]
    pub composite_members: Option<u64>,
    /// 今書き出している参加者
    pub participants: Vec<MediaPipelineStatusParticipantsItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MediaPipelineStatusParticipantsItem {
    /// `client_id`（無ければ `connection_id`）
    pub participant: String,
    /// 書き出し先の stream の ID
    pub stream: String,
    /// 映像を送っているか
    pub video: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaPipeline {
    pub artifacts: MediaPipelineArtifacts,
    /// 資格情報は `secret:{name}` の参照で、値は返さない
    #[serde(default)]
    pub captions: Option<CaptionSettings>,
    #[serde(default)]
    pub composite: Option<MediaPipelineComposite>,
    pub created_at: Timestamp,
    pub desired_state: MediaPipelineState,
    pub egress: Vec<MediaPipelineEgress>,
    pub export_participants: bool,
    pub generation: u64,
    pub id: String,
    pub name: MediaPipelineName,
    pub participants: MediaPipelineParticipants,
    #[serde(default)]
    pub recording: Option<MediaPipelineRecording>,
    pub room: String,
    /// このノードでの実行の様子。部屋をこのノードに置いていない・`stopped`・係がまだ見回っていない（作った直後）なら null
    /// （分散のときは部屋を置いたノードに問い合わせる）。止めた・消した直後の 1 秒ほどは前の様子が残ることがある。
    #[serde(default)]
    pub status: Option<MediaPipelineStatus>,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MediaPipelineList {
    pub items: Vec<MediaPipeline>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SseEventData {
    /// 種類ごとの中身（秘密は含まない）。部屋の核の出来事は `{type, data}`
    pub data: Value,
    /// 部屋の核の出来事の ID（部屋の核の出来事だけ。SSE の `id` は無い）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// 出来事を出したノードの ID（部屋の核の出来事だけ）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub occurred_at: Timestamp,
    /// 資源の ID（部屋の出来事は部屋の ID、`grid.decisions` は空）
    pub resource_id: String,
    pub r#type: SseEventDataType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SseEventDataType {
    #[serde(rename = "stream.created")]
    StreamCreated,
    #[serde(rename = "stream.updated")]
    StreamUpdated,
    #[serde(rename = "stream.deleted")]
    StreamDeleted,
    #[serde(rename = "stream.owner")]
    StreamOwner,
    #[serde(rename = "recording.updated")]
    RecordingUpdated,
    #[serde(rename = "recording.deleted")]
    RecordingDeleted,
    #[serde(rename = "schedule.created")]
    ScheduleCreated,
    #[serde(rename = "schedule.updated")]
    ScheduleUpdated,
    #[serde(rename = "schedule.deleted")]
    ScheduleDeleted,
    #[serde(rename = "schedule.run")]
    ScheduleRun,
    #[serde(rename = "node.updated")]
    NodeUpdated,
    #[serde(rename = "input.updated")]
    InputUpdated,
    #[serde(rename = "input.deleted")]
    InputDeleted,
    #[serde(rename = "output.updated")]
    OutputUpdated,
    #[serde(rename = "output.deleted")]
    OutputDeleted,
    #[serde(rename = "secret.created")]
    SecretCreated,
    #[serde(rename = "secret.deleted")]
    SecretDeleted,
    #[serde(rename = "pipeline.updated")]
    PipelineUpdated,
    #[serde(rename = "pipeline.deleted")]
    PipelineDeleted,
    #[serde(rename = "room.updated")]
    RoomUpdated,
    #[serde(rename = "room.deleted")]
    RoomDeleted,
    #[serde(rename = "media_pipeline.updated")]
    MediaPipelineUpdated,
    #[serde(rename = "media_pipeline.deleted")]
    MediaPipelineDeleted,
    #[serde(rename = "node.down")]
    NodeDown,
    #[serde(rename = "node.up")]
    NodeUp,
    #[serde(rename = "device.down")]
    DeviceDown,
    #[serde(rename = "device.up")]
    DeviceUp,
    #[serde(rename = "device.disabled")]
    DeviceDisabled,
    #[serde(rename = "device.enabled")]
    DeviceEnabled,
    #[serde(rename = "grid.decisions")]
    GridDecisions,
    #[serde(rename = "room.started")]
    RoomStarted,
    #[serde(rename = "room.stopped")]
    RoomStopped,
    #[serde(rename = "room.connection.created")]
    RoomConnectionCreated,
    #[serde(rename = "room.connection.updated")]
    RoomConnectionUpdated,
    #[serde(rename = "room.connection.destroyed")]
    RoomConnectionDestroyed,
    #[serde(rename = "room.track.published")]
    RoomTrackPublished,
    #[serde(rename = "room.track.unpublished")]
    RoomTrackUnpublished,
    #[serde(rename = "room.spotlight.changed")]
    RoomSpotlightChanged,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl SseEventDataType {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            SseEventDataType::StreamCreated => Some("stream.created"),
            SseEventDataType::StreamUpdated => Some("stream.updated"),
            SseEventDataType::StreamDeleted => Some("stream.deleted"),
            SseEventDataType::StreamOwner => Some("stream.owner"),
            SseEventDataType::RecordingUpdated => Some("recording.updated"),
            SseEventDataType::RecordingDeleted => Some("recording.deleted"),
            SseEventDataType::ScheduleCreated => Some("schedule.created"),
            SseEventDataType::ScheduleUpdated => Some("schedule.updated"),
            SseEventDataType::ScheduleDeleted => Some("schedule.deleted"),
            SseEventDataType::ScheduleRun => Some("schedule.run"),
            SseEventDataType::NodeUpdated => Some("node.updated"),
            SseEventDataType::InputUpdated => Some("input.updated"),
            SseEventDataType::InputDeleted => Some("input.deleted"),
            SseEventDataType::OutputUpdated => Some("output.updated"),
            SseEventDataType::OutputDeleted => Some("output.deleted"),
            SseEventDataType::SecretCreated => Some("secret.created"),
            SseEventDataType::SecretDeleted => Some("secret.deleted"),
            SseEventDataType::PipelineUpdated => Some("pipeline.updated"),
            SseEventDataType::PipelineDeleted => Some("pipeline.deleted"),
            SseEventDataType::RoomUpdated => Some("room.updated"),
            SseEventDataType::RoomDeleted => Some("room.deleted"),
            SseEventDataType::MediaPipelineUpdated => Some("media_pipeline.updated"),
            SseEventDataType::MediaPipelineDeleted => Some("media_pipeline.deleted"),
            SseEventDataType::NodeDown => Some("node.down"),
            SseEventDataType::NodeUp => Some("node.up"),
            SseEventDataType::DeviceDown => Some("device.down"),
            SseEventDataType::DeviceUp => Some("device.up"),
            SseEventDataType::DeviceDisabled => Some("device.disabled"),
            SseEventDataType::DeviceEnabled => Some("device.enabled"),
            SseEventDataType::GridDecisions => Some("grid.decisions"),
            SseEventDataType::RoomStarted => Some("room.started"),
            SseEventDataType::RoomStopped => Some("room.stopped"),
            SseEventDataType::RoomConnectionCreated => Some("room.connection.created"),
            SseEventDataType::RoomConnectionUpdated => Some("room.connection.updated"),
            SseEventDataType::RoomConnectionDestroyed => Some("room.connection.destroyed"),
            SseEventDataType::RoomTrackPublished => Some("room.track.published"),
            SseEventDataType::RoomTrackUnpublished => Some("room.track.unpublished"),
            SseEventDataType::RoomSpotlightChanged => Some("room.spotlight.changed"),
            SseEventDataType::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProblemCode {
    #[serde(rename = "INVALID_CONFIG")]
    InvalidConfig,
    #[serde(rename = "INVALID_REQUEST")]
    InvalidRequest,
    #[serde(rename = "UNSUPPORTED_PROTOCOL_VERSION")]
    UnsupportedProtocolVersion,
    #[serde(rename = "UNSUPPORTED_CODEC")]
    UnsupportedCodec,
    #[serde(rename = "UNSUPPORTED_MEDIA_TYPE")]
    UnsupportedMediaType,
    #[serde(rename = "AUTH_FAILED")]
    AuthFailed,
    #[serde(rename = "FORBIDDEN")]
    Forbidden,
    #[serde(rename = "NOT_FOUND")]
    NotFound,
    #[serde(rename = "METHOD_NOT_ALLOWED")]
    MethodNotAllowed,
    #[serde(rename = "CONFLICT")]
    Conflict,
    #[serde(rename = "IDEMPOTENCY_CONFLICT")]
    IdempotencyConflict,
    #[serde(rename = "PRECONDITION_FAILED")]
    PreconditionFailed,
    #[serde(rename = "PRECONDITION_REQUIRED")]
    PreconditionRequired,
    #[serde(rename = "QUOTA_EXCEEDED")]
    QuotaExceeded,
    #[serde(rename = "RATE_LIMITED")]
    RateLimited,
    #[serde(rename = "CAPACITY_UNAVAILABLE")]
    CapacityUnavailable,
    #[serde(rename = "INPUT_TIMEOUT")]
    InputTimeout,
    #[serde(rename = "OWNERSHIP_LOST")]
    OwnershipLost,
    #[serde(rename = "PAYLOAD_TOO_LARGE")]
    PayloadTooLarge,
    #[serde(rename = "INTERNAL_ERROR")]
    InternalError,
    /// この版の SDK が知らない値（サーバーが値を足したとき）。
    #[serde(other)]
    Unknown,
}

impl ProblemCode {
    /// JSON での値（`Unknown` は `None`）。
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            ProblemCode::InvalidConfig => Some("INVALID_CONFIG"),
            ProblemCode::InvalidRequest => Some("INVALID_REQUEST"),
            ProblemCode::UnsupportedProtocolVersion => Some("UNSUPPORTED_PROTOCOL_VERSION"),
            ProblemCode::UnsupportedCodec => Some("UNSUPPORTED_CODEC"),
            ProblemCode::UnsupportedMediaType => Some("UNSUPPORTED_MEDIA_TYPE"),
            ProblemCode::AuthFailed => Some("AUTH_FAILED"),
            ProblemCode::Forbidden => Some("FORBIDDEN"),
            ProblemCode::NotFound => Some("NOT_FOUND"),
            ProblemCode::MethodNotAllowed => Some("METHOD_NOT_ALLOWED"),
            ProblemCode::Conflict => Some("CONFLICT"),
            ProblemCode::IdempotencyConflict => Some("IDEMPOTENCY_CONFLICT"),
            ProblemCode::PreconditionFailed => Some("PRECONDITION_FAILED"),
            ProblemCode::PreconditionRequired => Some("PRECONDITION_REQUIRED"),
            ProblemCode::QuotaExceeded => Some("QUOTA_EXCEEDED"),
            ProblemCode::RateLimited => Some("RATE_LIMITED"),
            ProblemCode::CapacityUnavailable => Some("CAPACITY_UNAVAILABLE"),
            ProblemCode::InputTimeout => Some("INPUT_TIMEOUT"),
            ProblemCode::OwnershipLost => Some("OWNERSHIP_LOST"),
            ProblemCode::PayloadTooLarge => Some("PAYLOAD_TOO_LARGE"),
            ProblemCode::InternalError => Some("INTERNAL_ERROR"),
            ProblemCode::Unknown => None,
        }
    }
}

/// RFC 9457 の problem details（仕様 10.4）。内部の詳細や秘密は入れない。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    pub code: ProblemCode,
    /// code ごとの補足（例: 本文の解析エラーは `{reason}`、予約の衝突は `{limit, overlaps}`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// `X-Request-Id` と同じ（Idempotency-Key の再送では、再送の要求の ID）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub retryable: bool,
    pub status: u64,
    /// 日本語の説明
    pub title: String,
    pub r#type: String,
}
