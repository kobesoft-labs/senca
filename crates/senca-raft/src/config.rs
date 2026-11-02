//! 設定。

use crate::types::NodeId;

/// [`crate::RawNode`] の設定。時間はすべて tick の回数。
#[derive(Debug, Clone)]
pub struct Config {
    /// 自分の ID（0 は不可）。
    pub id: NodeId,
    /// 選挙タイムアウトの下限。実際は `[election_tick, 2*election_tick)` から乱数で選ぶ。
    pub election_tick: u32,
    /// リーダーが心拍を送る間隔。
    pub heartbeat_tick: u32,
    /// PreVote（分断から戻ったノードが term を上げてクラスタを乱さない）。
    pub pre_vote: bool,
    /// CheckQuorum（過半数と連絡が取れないリーダーは退く。リーダーが健在な間は投票に応じない）。
    pub check_quorum: bool,
    /// lease read（心拍の確認なしに読む）。時計の進みがノード間でほぼ同じという前提に頼るので既定は無効。
    /// `check_quorum` が必要。
    pub lease_read: bool,
    /// 1 つの相手に応答待ちで送れる Append の数。
    pub max_inflight_msgs: usize,
    /// 1 つの Append に入れる entry の総量（少なくとも 1 件は入れる）。
    pub max_msg_bytes: u64,
    /// 1 件の entry の上限。超える提案は拒否する。
    pub max_entry_bytes: u64,
    /// commit 前の entry の総量の上限。超える提案は拒否する（過負荷・分断中の積み上がりを防ぐ）。
    pub max_uncommitted_bytes: u64,
    /// 1 つの Ready に入れる commit 済み entry の総量。
    pub max_committed_bytes_per_ready: u64,
    /// snapshot の 1 片の大きさ。
    pub snapshot_chunk_bytes: u64,
    /// 受け取る snapshot の上限。
    pub max_snapshot_bytes: u64,
    /// 乱数の種（選挙タイムアウト）。ノードごと・起動ごとに変える。
    pub seed: u64,
    /// 利用者の状態機械が適用済みの index（状態機械を別に永続化している場合）。0 なら snapshot の index。
    pub applied: u64,
}

impl Config {
    pub fn new(id: NodeId) -> Config {
        Config {
            id,
            election_tick: 10,
            heartbeat_tick: 1,
            pre_vote: true,
            check_quorum: true,
            lease_read: false,
            max_inflight_msgs: 256,
            max_msg_bytes: 1 << 20,
            max_entry_bytes: 4 << 20,
            max_uncommitted_bytes: 64 << 20,
            max_committed_bytes_per_ready: 16 << 20,
            snapshot_chunk_bytes: 1 << 20,
            max_snapshot_bytes: 1 << 30,
            seed: id,
            applied: 0,
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.id == 0 {
            return Err(ConfigError("id must not be 0"));
        }
        if self.heartbeat_tick == 0 {
            return Err(ConfigError("heartbeat_tick must be > 0"));
        }
        if self.election_tick <= self.heartbeat_tick {
            return Err(ConfigError("election_tick must be > heartbeat_tick"));
        }
        if self.election_tick < 3 {
            return Err(ConfigError("election_tick must be >= 3"));
        }
        if self.max_inflight_msgs == 0 {
            return Err(ConfigError("max_inflight_msgs must be > 0"));
        }
        if self.lease_read && !self.check_quorum {
            return Err(ConfigError("lease_read requires check_quorum"));
        }
        if self.max_msg_bytes == 0 || self.max_entry_bytes == 0 || self.snapshot_chunk_bytes == 0 {
            return Err(ConfigError("size limits must be > 0"));
        }
        if self.max_entry_bytes > crate::codec::MAX_ENTRY_DATA as u64 {
            return Err(ConfigError("max_entry_bytes exceeds codec limit"));
        }
        if self.snapshot_chunk_bytes > crate::codec::MAX_CHUNK as u64 {
            return Err(ConfigError("snapshot_chunk_bytes exceeds codec limit"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub &'static str);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid raft config: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}
