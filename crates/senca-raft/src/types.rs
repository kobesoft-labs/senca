//! ログ・状態・構成の型。

use bytes::Bytes;
use std::collections::BTreeSet;

/// ノードの ID。0 は「なし」を表すので使えない。
pub type NodeId = u64;

/// entry の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// 利用者のコマンド（`data` を状態機械に適用する）。
    Normal,
    /// 構成（`data` は [`ConfState`] の符号化）。核が解釈する。利用者は適用しなくてよい。
    ConfChange,
    /// リーダーが就任時に書く空の entry。利用者は読み飛ばす。
    Noop,
}

impl EntryKind {
    pub(crate) fn to_u8(self) -> u8 {
        match self {
            EntryKind::Normal => 0,
            EntryKind::ConfChange => 1,
            EntryKind::Noop => 2,
        }
    }

    pub(crate) fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(EntryKind::Normal),
            1 => Some(EntryKind::ConfChange),
            2 => Some(EntryKind::Noop),
            _ => None,
        }
    }
}

/// ログの 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub term: u64,
    pub index: u64,
    pub kind: EntryKind,
    pub data: Bytes,
}

impl Entry {
    /// 流量制御に使う大きさ（見出しの分を足す）。
    pub fn size(&self) -> u64 {
        self.data.len() as u64 + 24
    }

    /// 構成の entry なら構成を返す。
    pub fn conf_state(&self) -> Option<ConfState> {
        if self.kind != EntryKind::ConfChange {
            return None;
        }
        crate::codec::decode_conf_state(&self.data).ok()
    }
}

/// 永続化する状態（投票・term・commit）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HardState {
    pub term: u64,
    /// 投票先。0 は未投票。
    pub vote: NodeId,
    pub commit: u64,
}

/// 構成。joint consensus 中は `voters_outgoing` が空でない（C_old,new: 両方の過半数が必要）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfState {
    /// 投票ノード（joint 中は新しい側 C_new）。
    pub voters: BTreeSet<NodeId>,
    /// joint 中の古い側 C_old。joint でなければ空。
    pub voters_outgoing: BTreeSet<NodeId>,
    /// 投票しないノード（追いつかせてから投票ノードに上げる）。
    pub learners: BTreeSet<NodeId>,
}

impl ConfState {
    pub fn new(voters: impl IntoIterator<Item = NodeId>) -> Self {
        ConfState {
            voters: voters.into_iter().collect(),
            ..Default::default()
        }
    }

    pub fn is_joint(&self) -> bool {
        !self.voters_outgoing.is_empty()
    }

    /// 投票に関わるか（新旧どちらかの投票ノード）。
    pub fn is_voter(&self, id: NodeId) -> bool {
        self.voters.contains(&id) || self.voters_outgoing.contains(&id)
    }

    /// 複製の相手すべて（投票ノード・古い側・learner）。
    pub fn all_ids(&self) -> BTreeSet<NodeId> {
        let mut s = self.voters.clone();
        s.extend(self.voters_outgoing.iter().copied());
        s.extend(self.learners.iter().copied());
        s
    }

    /// 投票ノード（新旧の和）。
    pub fn all_voters(&self) -> BTreeSet<NodeId> {
        let mut s = self.voters.clone();
        s.extend(self.voters_outgoing.iter().copied());
        s
    }

    /// 形の検査: 投票ノードが空でない、ID が 0 でない、learner は投票ノードと重ならない。
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.voters.is_empty() {
            return Err("voters is empty");
        }
        if self.all_ids().contains(&0) {
            return Err("node id 0 is reserved");
        }
        if self.learners.iter().any(|l| self.is_voter(*l)) {
            return Err("learner is also a voter");
        }
        Ok(())
    }
}

/// 構成変更の 1 操作。`propose_conf_change` にまとめて渡す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfChangeOp {
    /// 投票ノードに加える（learner なら昇格）。
    AddVoter(NodeId),
    RemoveVoter(NodeId),
    /// learner として加える（追いつかせてから AddVoter で昇格する）。
    AddLearner(NodeId),
    RemoveLearner(NodeId),
}

/// snapshot の見出し。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotMeta {
    /// snapshot に含まれる最後の entry の index。
    pub index: u64,
    pub term: u64,
    /// `index` の時点の構成。
    pub conf_state: ConfState,
}

/// 状態機械の写し（`data` は利用者の形式）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub meta: SnapshotMeta,
    pub data: Bytes,
}

impl Snapshot {
    /// 新しいクラスタの初期構成（index 0・term 0・データ空）。最初に起動する投票ノードの記憶域に書く。
    pub fn bootstrap(voters: impl IntoIterator<Item = NodeId>) -> Snapshot {
        Snapshot {
            meta: SnapshotMeta {
                index: 0,
                term: 0,
                conf_state: ConfState::new(voters),
            },
            data: Bytes::new(),
        }
    }
}

/// fencing token（仕様 8.2）。(term, index) の辞書順で比べる。
///
/// commit 済みのログでは entry の (term, index) が厳密に増えるので、所有権を与えた entry の
/// `FencingToken::of_entry` は合意の順に単調に増える。受け手は「これまでに見た最大値より小さい token の
/// 書込みを拒否する」だけでよい。`as_u128` は 1 つの数で比べたいとき用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct FencingToken {
    pub term: u64,
    pub index: u64,
}

impl FencingToken {
    pub fn of_entry(e: &Entry) -> FencingToken {
        FencingToken {
            term: e.term,
            index: e.index,
        }
    }

    pub fn as_u128(self) -> u128 {
        ((self.term as u128) << 64) | self.index as u128
    }

    pub fn from_u128(v: u128) -> FencingToken {
        FencingToken {
            term: (v >> 64) as u64,
            index: v as u64,
        }
    }
}
