//! SENCA の codec worker（外部コーデックを別プロセスに隔離する、仕様 3.1・9.4、AC-17、ADR-0004 の 5）。
//!
//! - [`protocol`] — 親と worker の間のフレーム形式とメッセージ（I/O なし、上限付きの厳格な解析）
//! - [`pool`] — 親側の `WorkerPool`（数の指定、テナントごとの公平さ、時間切れ、異常終了の検出と作り直し）
//! - [`worker`] — worker プロセスの本体（ジョブの実行、試験用の故障の注入）
//! - [`sandbox`] — Linux のサンドボックス（no_new_privs、rlimit、Landlock、seccomp）
//!
//! ノード本体は [`pool::WorkerPool`] だけを使い、コーデックのライブラリを自分のプロセスに読み込まない。

pub mod pool;
pub mod protocol;
#[allow(unsafe_code)]
pub mod sandbox;
pub mod worker;

pub use pool::{FailureCause, PoolConfig, PoolError, PoolStats, WorkerInfo, WorkerPool};
pub use protocol::{JobKind, JobOutput};
