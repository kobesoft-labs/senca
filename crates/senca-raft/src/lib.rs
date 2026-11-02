//! SENCA 制御面の合意（Raft、Ongaro の博士論文に沿う）。仕様 8.1・8.2。
//!
//! 核（[`RawNode`]）は sans-io: I/O・スレッド・時計を持たない。時間は [`RawNode::tick`] の回数、
//! 乱数は [`Config::seed`] で渡す。入出力は etcd-raft と同じ「Ready」方式:
//!
//! ```text
//! loop {
//!     // 1. 入力: tick() / step(msg) / propose(data) / read_index(ctx) / propose_conf_change(ops)
//!     while let Some(rd) = node.ready() {
//!         // 2. 永続化（fsync まで）: rd.snapshot → rd.entries → rd.hard_state（Storage::persist）
//!         //    失敗したらノードを止める（メッセージを送らない）。
//!         // 3. 送信: rd.messages
//!         // 4. 適用: rd.snapshot があれば状態機械を置き換え、rd.committed_entries を順に適用
//!         // 5. rd.read_states: 適用済み index が ReadState.index 以上になったら読んでよい
//!     }
//! }
//! ```
//!
//! 主な機能: 乱択タイムアウトの選挙、PreVote・CheckQuorum（分断されたノードが乱さない）、
//! 項で巻き戻すログ複製、現在の term の entry だけを直接 commit、snapshot（分割送信）と
//! ログ圧縮、joint consensus による構成変更と learner、リーダー移譲（TimeoutNow）、
//! ReadIndex（既定）と lease read（既定で無効）、流量制御（inflight 数・メッセージ・entry の上限）、
//! fencing 用の単調な (term, index)。
//!
//! I/O を行うのは [`storage_file`] だけ（ファイルによる永続化、再利用のためここに置く）。

pub mod codec;
mod config;
mod log;
mod message;
mod progress;
mod quorum;
mod raft;
pub mod storage;
pub mod storage_file;
mod types;

pub use config::{Config, ConfigError};
pub use message::{Message, MsgBody};
pub use raft::{Proposal, ProposeError, RawNode, ReadState, Ready, Role, Status, StepError};
pub use storage::{InitialState, MemStorage, Storage};
pub use types::{
    ConfChangeOp, ConfState, Entry, EntryKind, FencingToken, HardState, NodeId, Snapshot,
    SnapshotMeta,
};
