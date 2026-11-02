//! ノード間の中継（ADR-0014）。
//!
//! ノードの組ごとに 1 本の多重化した接続を張り、stream ごとの論理チャネルで符号化済みの AU を運ぶ。
//! 核は I/O を持たない（sans-io）。tokio のアダプターは機能 `tokio` の [`net`] にある。
//!
//! - [`wire`]: 接続の上の枠（版つき・上限つき）。見出しは可変長整数、payload は `Bytes` の参照のまま
//!   （受信は受信バッファの切り出し、送信は見出しと payload を別々の断片にしてまとめ書き）。
//! - [`sched`]: 送りの順番（音声 > 映像のキーフレーム > 映像の差分 > データ）。詰まったら低い優先度から捨て、
//!   映像は次のキーフレームまで捨てる（壊れた GOP を送らない）。小さな AU は短い時間まとめて 1 つの枠にする。
//!   チャネル間は deficit round robin。チャネルごと・全体の帯域の上限。
//! - [`conn`]: 1 本の接続の状態機械（挨拶・購読・設定・AU・制御・RTT・統計・GoAway）。
//! - [`agg`]: 購読の集約。同じ stream を見る購読者が何人いても上流の購読は 1 つ。最後の購読者が去っても
//!   しばらく（linger）保つ。後から来た購読者には直近の GOP を先に渡す。サイマルキャストの層は和だけを頼む。
//! - [`net`]（機能 `tokio`）: 任意の `AsyncRead + AsyncWrite`（ノードは mTLS のストリームを渡す）の上で核を動かす
//!   [`net::RelayConnection`]、相手ごとの接続を管理する [`net::RelayEndpoint`]、集約を動かす [`net::RelayHub`]。

pub mod agg;
pub mod conn;
pub mod event;
pub mod sched;
pub mod varint;
pub mod wire;

#[cfg(feature = "tokio")]
pub mod net;

pub use agg::{AggAction, Aggregator, AggregatorConfig, LocalId};
pub use conn::{
    ConnConfig, ConnError, ConnEvent, ConnStats, Connection, DownstreamId, PeerInfo, UpstreamId,
};
pub use event::{LostReason, StreamEvent, TrackSel};
pub use sched::{Class, SchedConfig, SchedStats, Scheduler};
pub use wire::{ErrorCode, PROTOCOL_VERSION, WireError};

/// ノードの番号（cluster の node id と同じ）。
pub type NodeId = u64;
