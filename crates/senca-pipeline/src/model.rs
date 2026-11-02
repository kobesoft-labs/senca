//! パイプラインの定義（JSON）。仕様 5.1: 有向非巡回グラフで、部品ごとに入力・出力の形式を宣言する。
//!
//! - 版付き（`"version": 1`）。知らない項目は拒否する（`deny_unknown_fields`）。
//! - 部品は `"type"` で種類を選ぶ。`"id"` は英数字・`_`・`-` の 1〜64 文字。
//! - 入力の参照は `"部品"` または `"部品.ポート"`。ポートを省くと既定のポート（`source` は省けない）。
//!   - `source`: `video`・`audio`（符号化済み）
//!   - `decode`・`board`・`scale`・`crop`・`audio_mix`・`encode`: `out`
//!   - `compose`: `program`（既定）・`preview`
//! - 場所（どのノードで動かすか）は持たない（REQ-005: ロジックと計算資源の分離）。
//!
//! 値の範囲の検査は [`crate::validate`] が行う（ここは形だけ）。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 定義の版。
pub const GRAPH_VERSION: u32 = 1;
/// JSON の大きさの上限（画像を base64 で含められるので大きめ）。
pub const MAX_GRAPH_BYTES: usize = 16 * 1024 * 1024;

/// パイプラインの定義。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Graph {
    pub version: u32,
    #[serde(default)]
    pub options: GraphOptions,
    pub nodes: Vec<Node>,
}

impl Graph {
    /// JSON から読む（大きさ・版・形を検査する）。
    pub fn from_json(s: &str) -> Result<Graph, crate::PipelineError> {
        use crate::error::{Issue, IssueCode, PipelineError};
        if s.len() > MAX_GRAPH_BYTES {
            return Err(PipelineError::Invalid(vec![Issue::new(
                IssueCode::LimitExceeded,
                None,
                format!("graph JSON exceeds {MAX_GRAPH_BYTES} bytes"),
            )]));
        }
        let g: Graph = serde_json::from_str(s).map_err(|e| {
            PipelineError::Invalid(vec![Issue::new(
                IssueCode::Malformed,
                None,
                format!("graph JSON: {e}"),
            )])
        })?;
        if g.version != GRAPH_VERSION {
            return Err(PipelineError::Invalid(vec![Issue::new(
                IssueCode::Malformed,
                None,
                format!(
                    "unsupported graph version {} (expected {GRAPH_VERSION})",
                    g.version
                ),
            )]));
        }
        Ok(g)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("graph serializes")
    }

    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id() == id)
    }
}

/// 時刻の進め方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockMode {
    /// 単調時計で出力の刻みを進める（ライブ）。入力の時刻は到着時に写像する。
    #[default]
    Realtime,
    /// 入力の時刻で進める（すべての入力がその時刻までそろったら出す。来ない入力は `missing_timeout_ms` で見限る）。
    /// ファイルからの処理・試験・時刻が揃った入力向け。
    Input,
}

/// キューが一杯のときの扱い。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverflowPolicy {
    /// 古いものを捨てる（ライブ優先。映像の AU は次のキーフレームまで捨てる）。
    #[default]
    Drop,
    /// 上流を待たせる（`block_timeout_ms` を超えたら古いものを捨てる。無期限には待たない）。
    Block,
}

/// 全体の設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GraphOptions {
    pub clock: ClockMode,
    pub overflow: OverflowPolicy,
    /// 入力の揺らぎを吸収する遅延（ms、ライブ）。合成・ミキサーはこの分だけ遅れた時刻を描く。
    pub latency_ms: u32,
    /// 入力が来ないとき「切れた」とみなすまでの時間（ms）。
    pub missing_timeout_ms: u32,
    /// `Block` のときに待つ上限（ms）。
    pub block_timeout_ms: u32,
    /// 映像の AU の入力キューの長さ（AU 数）。
    pub input_queue_aus: u32,
    /// 生のフレームのキューの長さ（部品の入力ごと）。
    pub frame_queue: u32,
    /// 状態・メーターの通知の間隔（ms、0 で出さない）。既定は 100 ms（約 10 Hz）。
    pub status_interval_ms: u32,
    /// 新しい世代への切り替えを待つ上限（ms）。超えたら旧世代を続けて失敗を返す。
    pub commit_timeout_ms: u32,
}

impl Default for GraphOptions {
    fn default() -> Self {
        GraphOptions {
            clock: ClockMode::Realtime,
            overflow: OverflowPolicy::Drop,
            latency_ms: 100,
            missing_timeout_ms: 2000,
            block_timeout_ms: 2000,
            input_queue_aus: 256,
            frame_queue: 8,
            status_interval_ms: 100,
            commit_timeout_ms: 10_000,
        }
    }
}

/// 部品。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Node {
    Source(SourceNode),
    Decode(DecodeNode),
    Board(BoardNode),
    Scale(ScaleNode),
    Crop(CropNode),
    Compose(ComposeNode),
    AudioMix(AudioMixNode),
    Encode(EncodeNode),
    SnapshotOnChange(SnapshotNode),
    Sink(SinkNode),
}

impl Node {
    pub fn id(&self) -> &str {
        match self {
            Node::Source(n) => &n.id,
            Node::Decode(n) => &n.id,
            Node::Board(n) => &n.id,
            Node::Scale(n) => &n.id,
            Node::Crop(n) => &n.id,
            Node::Compose(n) => &n.id,
            Node::AudioMix(n) => &n.id,
            Node::Encode(n) => &n.id,
            Node::SnapshotOnChange(n) => &n.id,
            Node::Sink(n) => &n.id,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Node::Source(_) => "source",
            Node::Decode(_) => "decode",
            Node::Board(_) => "board",
            Node::Scale(_) => "scale",
            Node::Crop(_) => "crop",
            Node::Compose(_) => "compose",
            Node::AudioMix(_) => "audio_mix",
            Node::Encode(_) => "encode",
            Node::SnapshotOnChange(_) => "snapshot_on_change",
            Node::Sink(_) => "sink",
        }
    }

    /// 入力の参照（名前付きの入力は名前の順）。
    pub fn inputs(&self) -> Vec<&str> {
        match self {
            Node::Source(_) => vec![],
            Node::Decode(n) => vec![&n.input],
            Node::Board(n) => vec![&n.input],
            Node::Scale(n) => vec![&n.input],
            Node::Crop(n) => vec![&n.input],
            Node::Compose(n) => n.inputs.values().map(String::as_str).collect(),
            Node::AudioMix(n) => n.inputs.values().map(|i| i.from.as_str()).collect(),
            Node::Encode(n) => vec![&n.input],
            Node::SnapshotOnChange(n) => vec![&n.input],
            Node::Sink(n) => n.inputs.iter().map(String::as_str).collect(),
        }
    }
}

/// 入力の参照を `(部品, ポート)` に分ける。
pub fn split_ref(r: &str) -> (&str, Option<&str>) {
    match r.split_once('.') {
        Some((n, p)) => (n, Some(p)),
        None => (r, None),
    }
}

/// 入力（SENCA の stream）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceNode {
    pub id: String,
    /// SENCA の stream id。
    pub stream: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<SourceTrack>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<SourceTrack>,
}

/// 入力のトラックの選択と、期待する形式（検証・見積もりに使う）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTrack {
    /// stream の中のトラック番号（省略時は種類の最初のトラック）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<u32>,
    /// コーデック（`h264`・`h265`・`av1`・`vp8`・`vp9`・`opus`・`aac` など）。
    pub codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u8>,
}

/// 復号。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecodeNode {
    pub id: String,
    pub input: String,
    /// 受け付ける解像度の上限（既定 3840x2160）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u32>,
    /// ソフトウェアコーデックのスレッド数（0 = 自動）。
    #[serde(default)]
    pub threads: u32,
}

/// 板の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardMode {
    #[default]
    Whiteboard,
    Blackboard,
}

/// 板書の補正（REQ-006 R6-5/R6-6）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardNode {
    pub id: String,
    pub input: String,
    #[serde(default)]
    pub params: BoardParamsSpec,
}

/// 板の補正の設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BoardParamsSpec {
    pub mode: BoardMode,
    /// 板の 4 隅（左上・右上・右下・左下、入力の画素）。省くと `auto_detect` に従う。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corners: Option<[[f64; 2]; 4]>,
    /// 4 隅を自動で検出する（見つかるまで毎フレーム試す。見つからなければ台形補正なし）。
    pub auto_detect: bool,
    /// 自動検出をやり直す間隔（フレーム、0 = 見つかった後はやり直さない）。
    pub redetect_frames: u32,
    pub output_width: u32,
    pub output_height: u32,
    /// 照明むら・反射の補正と強調。
    pub enhance: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke_gain: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chroma_gain: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glare_suppression: Option<bool>,
    /// 黒板を白地・暗い字に反転する。
    pub invert_to_white: bool,
    /// 輝度の CLAHE。
    pub clahe: bool,
}

impl Default for BoardParamsSpec {
    fn default() -> Self {
        BoardParamsSpec {
            mode: BoardMode::Whiteboard,
            corners: None,
            auto_detect: false,
            redetect_frames: 0,
            output_width: 1280,
            output_height: 720,
            enhance: true,
            stroke_gain: None,
            chroma_gain: None,
            glare_suppression: None,
            invert_to_white: false,
            clahe: false,
        }
    }
}

/// 拡大縮小の核。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterSpec {
    Nearest,
    #[default]
    Bilinear,
    Bicubic,
    Lanczos3,
    Area,
}

impl FilterSpec {
    pub fn to_image(self) -> senca_image::ScaleFilter {
        use senca_image::ScaleFilter as F;
        match self {
            FilterSpec::Nearest => F::Nearest,
            FilterSpec::Bilinear => F::Bilinear,
            FilterSpec::Bicubic => F::Bicubic,
            FilterSpec::Lanczos3 => F::Lanczos3,
            FilterSpec::Area => F::Area,
        }
    }
}

/// 拡大縮小。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScaleNode {
    pub id: String,
    pub input: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub filter: FilterSpec,
}

/// 切り抜き（入力の画素、偶数に切り下げる）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CropNode {
    pub id: String,
    pub input: String,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// 合成（制作、REQ-008）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeNode {
    pub id: String,
    /// シーンから参照する名前 → 上流の参照。
    pub inputs: BTreeMap<String, String>,
    pub canvas: CanvasSpec,
    /// 静止画（ロゴなど）。名前 → 画像。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub images: BTreeMap<String, ImageSpec>,
    pub scenes: BTreeMap<String, SceneSpec>,
    /// プログラム（出力）のシーン。
    pub program: String,
    /// プレビューのシーン（`preview` ポートを使うときに描く）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// 既定のトランジション（`take` で使う）。
    #[serde(default)]
    pub transition: TransitionSpec,
    /// 入力が来ないときの扱い。
    #[serde(default)]
    pub missing: MissingSpec,
    /// 帯の並列度（0 = CPU の数）。
    #[serde(default)]
    pub threads: u32,
}

/// キャンバス。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanvasSpec {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    #[serde(default = "one")]
    pub fps_den: u32,
    /// 地の色（`#RRGGBB`）。
    #[serde(default = "black")]
    pub background: String,
}

fn one() -> u32 {
    1
}

fn black() -> String {
    "#000000".into()
}

/// 静止画。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageSpec {
    /// PNG（base64）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub png_base64: Option<String>,
    /// JPEG（base64）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jpeg_base64: Option<String>,
}

/// シーン。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SceneSpec {
    pub layers: Vec<LayerSpec>,
}

/// 縦横比の扱い。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitSpec {
    Stretch,
    #[default]
    Contain,
    Cover,
}

/// 切り抜き（各辺から割合で削る）。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CropSpec {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// 枠線。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BorderSpec {
    pub width: u32,
    pub color: String,
}

/// レイヤー。中身は `source`・`image`・`color` のどれか 1 つ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "is_default")]
    pub crop: CropSpec,
    #[serde(default)]
    pub fit: FitSpec,
    #[serde(default)]
    pub z: i32,
    #[serde(default = "one_f32")]
    pub opacity: f32,
    #[serde(default)]
    pub corner_radius: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<BorderSpec>,
    #[serde(default = "yes")]
    pub visible: bool,
    #[serde(default = "bicubic")]
    pub filter: FilterSpec,
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

fn one_f32() -> f32 {
    1.0
}

fn yes() -> bool {
    true
}

fn bicubic() -> FilterSpec {
    FilterSpec::Bicubic
}

/// トランジションの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionKindSpec {
    #[default]
    Cut,
    Fade,
    WipeLeft,
    WipeRight,
}

/// トランジション。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionSpec {
    #[serde(default)]
    pub kind: TransitionKindSpec,
    #[serde(default)]
    pub duration_ms: u32,
}

impl TransitionSpec {
    pub fn to_compose(self) -> senca_compose::Transition {
        use senca_compose::TransitionKind as K;
        senca_compose::Transition {
            kind: match self.kind {
                TransitionKindSpec::Cut => K::Cut,
                TransitionKindSpec::Fade => K::Fade,
                TransitionKindSpec::WipeLeft => K::WipeLeftToRight,
                TransitionKindSpec::WipeRight => K::WipeRightToLeft,
            },
            duration_ms: self.duration_ms,
        }
    }
}

/// 入力が来ないときの扱い（AC-R8-03）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MissingSpec {
    /// 最後のフレームを保持する時間（ms）。超えたらスレート。
    pub hold_ms: u32,
    /// スレートの色（`#RRGGBB`）。
    pub slate_color: String,
    /// スレートの画像（`images` の名前）。色より優先。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slate_image: Option<String>,
}

impl Default for MissingSpec {
    fn default() -> Self {
        MissingSpec {
            hold_ms: 2000,
            slate_color: "#000000".into(),
            slate_image: None,
        }
    }
}

/// 音声ミキサー（R8-6）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioMixNode {
    pub id: String,
    /// 入力の名前 → 入力。
    pub inputs: BTreeMap<String, MixInputSpec>,
    #[serde(default)]
    pub master_gain_db: f32,
    #[serde(default = "yes")]
    pub limiter: bool,
}

/// ミキサーの入力。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixInputSpec {
    pub from: String,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub solo: bool,
    #[serde(default)]
    pub pan: f32,
    #[serde(default)]
    pub delay_ms: f32,
}

/// 符号量の制御。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateControlSpec {
    #[default]
    Cbr,
    Vbr,
}

/// 符号化。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncodeNode {
    pub id: String,
    pub input: String,
    /// `av1`・`vp8`・`vp9`・`h264`・`h265`・`opus`。
    pub codec: String,
    pub bitrate_kbps: u32,
    /// キーフレームの間隔（フレーム）。省略時は 2 秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gop: Option<u32>,
    #[serde(default = "yes")]
    pub low_latency: bool,
    #[serde(default)]
    pub rate_control: RateControlSpec,
    /// 出力の大きさ（省略時は入力の大きさ。違う大きさのフレームは拡大縮小する）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Opus の 1 パケットの長さ（ms、既定 20）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_ms: Option<f32>,
    #[serde(default)]
    pub threads: u32,
}

/// 板書の変化で静止画を出す（R6-8）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotNode {
    pub id: String,
    pub input: String,
    #[serde(default)]
    pub params: SnapshotParamsSpec,
}

/// 変化検出の設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SnapshotParamsSpec {
    pub mode: BoardMode,
    /// 始めから動かす（偽なら操作 `snapshot` で始める）。
    pub enabled: bool,
    /// 解析するフレームの間隔（毎秒の回数、0 = 全フレーム）。
    pub analysis_fps: f32,
    /// 何回の解析で落ち着いたら出すか。
    pub stable_frames: u32,
    /// 前回の保存からの変化（割合）。
    pub change_fraction: f64,
    pub jpeg_quality: u8,
}

impl Default for SnapshotParamsSpec {
    fn default() -> Self {
        SnapshotParamsSpec {
            mode: BoardMode::Whiteboard,
            enabled: true,
            analysis_fps: 2.0,
            stable_frames: 3,
            change_fraction: 0.004,
            jpeg_quality: 85,
        }
    }
}

/// 出力（送り手として SENCA の stream に入れる）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SinkNode {
    pub id: String,
    /// 符号化済みの入力（映像 1 つ・音声 1 つまで）。並びの順にトラック番号 0, 1 を付ける。
    pub inputs: Vec<String>,
    /// 出力先の SENCA の stream id。
    pub stream: String,
}

/// `#RRGGBB` を読む。
pub fn parse_color(s: &str) -> Option<[u8; 3]> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tagged_nodes_and_rejects_unknown_fields() {
        let g = Graph::from_json(
            r#"{"version":1,"nodes":[
                {"type":"source","id":"cam","stream":"s1","video":{"codec":"vp9"}},
                {"type":"decode","id":"d","input":"cam.video"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.nodes[1].inputs(), vec!["cam.video"]);
        assert_eq!(g.options, GraphOptions::default());
        // 知らない項目
        let e = Graph::from_json(
            r#"{"version":1,"nodes":[{"type":"decode","id":"d","input":"x","bogus":1}]}"#,
        )
        .unwrap_err();
        assert_eq!(e.issues()[0].code, crate::IssueCode::Malformed);
        assert!(Graph::from_json(r#"{"version":1,"nodes":[],"extra":0}"#).is_err());
        assert!(
            Graph::from_json(r#"{"version":1,"options":{"clock":"input","nope":1},"nodes":[]}"#)
                .is_err()
        );
        // 知らない種類・版
        assert!(Graph::from_json(r#"{"version":1,"nodes":[{"type":"magic","id":"m"}]}"#).is_err());
        assert!(Graph::from_json(r#"{"version":2,"nodes":[]}"#).is_err());
        // 往復
        let back = Graph::from_json(&g.to_json()).unwrap();
        assert_eq!(back, g);
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#ff8000"), Some([255, 128, 0]));
        assert_eq!(parse_color("ff8000"), None);
        assert_eq!(parse_color("#ff80"), None);
        assert_eq!(parse_color("#gg0000"), None);
        assert_eq!(split_ref("a.b"), ("a", Some("b")));
        assert_eq!(split_ref("a"), ("a", None));
    }
}
