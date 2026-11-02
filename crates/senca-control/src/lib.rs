//! SENCA の制御面。
//!
//! - `state`: 制御状態と、状態を変える `state::Command`。すべての変更はコマンドを
//!   `state::ControlState::apply` に通して行う決定的な状態機械（同じコマンド列から同じ状態）。
//!   単一ノードではファイルのログに、分散（M9）では合意ログに載せる。
//! - `auth`: API トークンと JWT による認証、権限（scope）。
//! - `problem`: 仕様 10.4 のエラー形式。
//! - `model`: stream・録画・予約・トークンなどの資源。

pub mod auth;
pub mod jwt;
pub mod model;
pub mod problem;
pub mod schedule;
pub mod secret;
pub mod state;

pub use problem::{Problem, ProblemCode};
