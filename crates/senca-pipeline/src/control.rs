//! 運用の操作と通知（REQ-008 R8-4〜R8-6・R8-11、AC-R8-06）。
//!
//! 操作（[`Control`]）は JSON（`"op"` で種類を選ぶ、知らない項目は拒否）。合成・ミキサーへの操作は
//! 次の出力の刻み（フレーム境界）で反映する。通知（[`Event`]）は状態・メーター（約 10 Hz）と、
//! 世代・誤り・取りこぼしの知らせ。

use crate::generation::GenerationId;
use crate::model::{Graph, LayerSpec, TransitionSpec};
use crate::validate::Plan;
use serde::{Deserialize, Serialize};

/// 操作の JSON の大きさの上限。
pub const MAX_CONTROL_BYTES: usize = 16 * 1024 * 1024;

/// レイヤーの部分的な変更（与えた項目だけ変える）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub z: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    /// 中身を入力に替える。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// 操作。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    /// プログラムのシーンを切り替える（省略時はカット）。
    SwitchProgram {
        node: String,
        scene: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transition: Option<TransitionSpec>,
    },
    /// プレビューのシーンを決める。
    SetPreview { node: String, scene: String },
    /// プレビューをプログラムへ（既定のトランジション、または指定）。プレビューには元のプログラムが入る。
    Take {
        node: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transition: Option<TransitionSpec>,
    },
    /// レイヤーを部分的に変える（位置・大きさ・重なり・不透明度・表示）。
    UpdateLayer {
        node: String,
        scene: String,
        layer: usize,
        patch: LayerPatch,
    },
    /// シーンを作る・置き換える。
    SetScene {
        node: String,
        scene: String,
        layers: Vec<LayerSpec>,
    },
    /// 音声の入力（与えた項目だけ変える）。
    SetAudio {
        node: String,
        input: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gain_db: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mute: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        solo: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pan: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delay_ms: Option<f32>,
    },
    /// マスターの音量。
    SetMaster { node: String, gain_db: f32 },
    /// 板書の変化の検出を始める・止める。
    Snapshot { node: String, enabled: bool },
    /// キーフレームを求める（`node` を省くとすべての映像の符号化）。
    RequestKeyframe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node: Option<String>,
    },
    /// 新しい世代を準備する。
    Prepare { graph: Box<Graph> },
    /// 準備した世代に切り替える（切り替わるか失敗するまで待つ）。
    Commit { generation: GenerationId },
    /// 準備した世代を捨てる。
    Rollback { generation: GenerationId },
}

impl Control {
    pub fn from_json(s: &str) -> Result<Control, crate::PipelineError> {
        if s.len() > MAX_CONTROL_BYTES {
            return Err(crate::PipelineError::Control("control too large".into()));
        }
        serde_json::from_str(s).map_err(|e| crate::PipelineError::Control(format!("{e}")))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("control serializes")
    }

    pub fn name(&self) -> &'static str {
        match self {
            Control::SwitchProgram { .. } => "switch_program",
            Control::SetPreview { .. } => "set_preview",
            Control::Take { .. } => "take",
            Control::UpdateLayer { .. } => "update_layer",
            Control::SetScene { .. } => "set_scene",
            Control::SetAudio { .. } => "set_audio",
            Control::SetMaster { .. } => "set_master",
            Control::Snapshot { .. } => "snapshot",
            Control::RequestKeyframe { .. } => "request_keyframe",
            Control::Prepare { .. } => "prepare",
            Control::Commit { .. } => "commit",
            Control::Rollback { .. } => "rollback",
        }
    }
}

/// 操作の返事。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ControlReply {
    /// `prepare` で作った世代、`commit` で切り替わった世代。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Box<Plan>>,
}

/// メーターの 1 つの読み（dBFS、無音は −120）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeterReading {
    pub name: String,
    pub peak_db: [f32; 2],
    pub rms_db: [f32; 2],
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub solo: bool,
    #[serde(default)]
    pub gain_db: f32,
}

/// 入力（source）の状態。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceStatus {
    pub id: String,
    pub stream: String,
    /// `waiting`（まだ来ない）・`live`・`stalled`（`missing_timeout_ms` の間来ない）・`ended`。
    pub state: String,
    pub aus: u64,
    pub dropped_aus: u64,
    pub decoded_frames: u64,
    pub decode_errors: u64,
    /// 直近の秒間のフレーム数（映像）。
    pub fps: f32,
    /// 最後のフレームのセッション時刻（µs）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_us: Option<i64>,
    /// プログラム・プレビューに映っている（タリー）。
    pub on_program: bool,
    pub on_preview: bool,
}

/// 合成の状態（タリー）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComposeStatus {
    pub node: String,
    pub program: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    pub in_transition: bool,
    pub frames: u64,
    pub skipped_ticks: u64,
    /// スレートを出している入力。
    pub missing: Vec<String>,
    pub program_inputs: Vec<String>,
    pub preview_inputs: Vec<String>,
}

/// 符号化の状態。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncoderStatus {
    pub node: String,
    pub frames_in: u64,
    pub aus_out: u64,
    pub dropped_frames: u64,
    pub errors: u64,
}

/// 通知。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// 状態（約 10 Hz）。
    Status {
        generation: GenerationId,
        session_us: i64,
        sources: Vec<SourceStatus>,
        compose: Vec<ComposeStatus>,
        encoders: Vec<EncoderStatus>,
    },
    /// ミキサーのメーター（約 10 Hz、VU メーター用）。
    Meters {
        node: String,
        inputs: Vec<MeterReading>,
        master: MeterReading,
        limiter_reduction_db: f32,
    },
    /// 入力を捨てた（溢れ・キーフレーム待ち）。
    InputDropped {
        source: String,
        count: u64,
        reason: String,
    },
    /// 実行中の誤り（部品は続けるか、止めた）。
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node: Option<String>,
        code: String,
        message: String,
    },
    GenerationPrepared {
        generation: GenerationId,
    },
    GenerationCommitted {
        generation: GenerationId,
    },
    /// 切り替えに失敗（旧世代を続けている）。
    GenerationFailed {
        generation: GenerationId,
        code: String,
        message: String,
    },
    GenerationRolledBack {
        generation: GenerationId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_parse_strictly() {
        let c = Control::from_json(
            r#"{"op":"switch_program","node":"prod","scene":"b","transition":{"kind":"fade","duration_ms":500}}"#,
        )
        .unwrap();
        assert_eq!(c.name(), "switch_program");
        assert_eq!(Control::from_json(&c.to_json()).unwrap(), c);
        let c = Control::from_json(
            r#"{"op":"update_layer","node":"prod","scene":"a","layer":1,"patch":{"opacity":0.5,"visible":false}}"#,
        )
        .unwrap();
        let Control::UpdateLayer { patch, layer, .. } = &c else {
            panic!()
        };
        assert_eq!(*layer, 1);
        assert_eq!(patch.opacity, Some(0.5));
        assert_eq!(patch.visible, Some(false));
        assert!(Control::from_json(r#"{"op":"set_master","node":"m","gain_db":1,"x":1}"#).is_err());
        assert!(Control::from_json(r#"{"op":"explode"}"#).is_err());
        assert!(
            Control::from_json(
                r#"{"op":"update_layer","node":"p","scene":"a","layer":0,"patch":{"nope":1}}"#
            )
            .is_err()
        );
        let c = Control::from_json(r#"{"op":"request_keyframe"}"#).unwrap();
        assert_eq!(c, Control::RequestKeyframe { node: None });
    }

    #[test]
    fn events_serialize() {
        let e = Event::InputDropped {
            source: "cam".into(),
            count: 3,
            reason: "overflow".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"event\":\"input_dropped\""), "{s}");
        assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), e);
    }
}
