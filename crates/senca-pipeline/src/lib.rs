//! SENCA のパイプライン（仕様 5.1・5.2・7・11 章、REQ-005・REQ-006・REQ-008・REQ-010）。
//!
//! - [`model`] — パイプラインの定義（JSON、版付き、知らない項目は拒否）。DAG の部品: `source`・`decode`・`board`・
//!   `scale`・`crop`・`compose`・`audio_mix`・`encode`・`snapshot_on_change`・`sink`
//! - [`validate`] — 検証（ポートの型、非巡回、出力の使い残し、コーデックの方針、装置の能力、資源の見積もり）と
//!   変換計画（部品ごとの passthrough/transcode・追加の遅延・資源）
//! - [`generation`] — 世代の状態（prepare → commit → active、失敗・rollback では旧世代を続ける）と、
//!   出力の切り替え（トラックごとに新世代の最初のキーフレームで切り替える）
//! - [`clock`] — 時計（単調時計・試験用の偽の時計）、出力の刻み（キャンバスの fps・音声 20 ms）、入力の時刻の写像
//! - [`queue`] — 有界のキュー（ライブ優先: 古いものを捨てる。映像の AU は次のキーフレームまで捨てる）
//! - [`control`] — 運用の操作（シーンの切り替え・プレビュー・レイヤー・音声・スナップショット・キーフレーム要求）と
//!   通知（メーター・状態・タリー・誤り）
//! - [`runtime`] — 実行（codec worker の中で動く。復号はソースごとのスレッド、合成は帯のスレッド、符号化は別スレッド）
//!
//! 生のフレームはこのプロセスの外に出さない（REQ-010 R10-8）。外へ出るのは符号化済みの AU と小さな通知だけ。

pub mod clock;
pub mod control;
pub mod convert;
pub mod error;
pub mod generation;
pub mod model;
pub mod queue;
pub mod runtime;
pub mod validate;

pub use control::{Control, ControlReply, Event};
pub use error::{Issue, IssueCode, PipelineError};
pub use generation::{GenerationId, GenerationManager, GenerationState};
pub use model::Graph;
pub use runtime::{Runtime, RuntimeOptions, RuntimeOutput, Snapshot};
pub use validate::{Plan, ValidationContext, validate};
