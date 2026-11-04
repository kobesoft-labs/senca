//! SENCA の制御面。
//!
//! - `state`: 制御状態と、状態を変える `state::Command`。すべての変更はコマンドを
//!   `state::ControlState::apply` に通して行う決定的な状態機械（同じコマンド列から同じ状態）。
//!   単一ノードではファイルのログに、分散（M9）では合意ログに載せる。
//! - `auth`: API トークン・ログインのセッション・JWT による認証、権限（scope）。
//! - `users`: 利用者・セッション・パスワード（Argon2id）・TOTP・回復コード（ADR-0017）。
//! - `oidc`: OIDC のログインのクレームの読み方・PKCE（ID トークンの検証は `jwt`）。
//! - `problem`: 仕様 10.4 のエラー形式。
//! - `model`: stream・録画・予約・トークンなどの資源。

pub mod auth;
pub mod jwt;
pub mod model;
pub mod oidc;
pub mod problem;
pub mod schedule;
pub mod secret;
pub mod state;
pub mod users;
pub mod workloads;

pub use problem::{Problem, ProblemCode};
