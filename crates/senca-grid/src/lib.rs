//! SENCA のグリッドの配置係（ADR-0013）。
//!
//! sans-io で決定的: I/O・スレッド・時計・乱数を持たない。時刻（`now_ms`）と fencing token の
//! 通し番号は呼び手が渡す。同じ入力からはいつも同じ出力になる（`BTreeMap` と ID の順で回す）。
//!
//! 構成:
//! - [`model`] — ノード（[`NodeSpec`]・[`NodeStatus`]）、仕事（[`Workload`]）、割り当て（[`Assignment`]）、
//!   決定（[`Decision`]）。すべて serde で直列化でき、制御状態とコマンドに埋め込める。
//! - [`plan()`] — 配置係の本体。今の状態から決定の列を作る純粋な関数。
//! - [`GridState`]・[`GridCommand`] — 合意ログで複製する状態とコマンド。`apply` は決定的で、
//!   失敗したコマンドは状態を変えない。
//! - [`FailureDetector`] — リーダーの手元で心拍を受け、`NodeDown`・`NodeUp`・`DeviceDown`・`DeviceUp`
//!   のコマンドを作る（心拍そのものは合意ログに載せない）。
//!
//! 使い方（リーダー）:
//!
//! ```text
//! 毎秒:
//!   for cmd in detector.tick(&state, now) { propose(Command::Grid(cmd)) }
//!   let plan = state.plan(now, state.fencing_counter, &detector.stream_nodes_at(now), &config);
//!   if !plan.is_empty() { propose(Command::Grid(plan.into_command())) }
//! ```
//!
//! 各ノードは適用済みの状態から [`GridState::reconcile`] で自分の仕事を突き合わせ、
//! 外部への副作用の前に [`GridState::is_current`] で token を確かめる。

pub mod detect;
mod fit;
pub mod model;
pub mod plan;
pub mod state;

pub use detect::{DeviceHealth, FailureDetector, Heartbeat};
pub use model::*;
pub use plan::{Plan, PlanConfig, PlanInput, plan};
pub use state::{GridCommand, GridError, GridState, Reconcile, Running, is_current};
