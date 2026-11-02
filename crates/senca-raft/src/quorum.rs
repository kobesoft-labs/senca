//! 過半数の計算（joint consensus では新旧の両方の過半数）。

use crate::types::{ConfState, NodeId};
use std::collections::{BTreeMap, BTreeSet};

/// 集合の過半数が到達している最大の値。空の集合は制約なし（`u64::MAX`）。
fn majority_value(ids: &BTreeSet<NodeId>, f: &impl Fn(NodeId) -> u64) -> u64 {
    if ids.is_empty() {
        return u64::MAX;
    }
    let mut v: Vec<u64> = ids.iter().map(|&id| f(id)).collect();
    v.sort_unstable_by(|a, b| b.cmp(a));
    v[ids.len() / 2]
}

/// 構成の過半数（joint なら両方）が到達している最大の値。
pub(crate) fn quorum_value(conf: &ConfState, f: impl Fn(NodeId) -> u64) -> u64 {
    majority_value(&conf.voters, &f).min(majority_value(&conf.voters_outgoing, &f))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VoteResult {
    Won,
    Lost,
    Pending,
}

fn majority_vote(ids: &BTreeSet<NodeId>, votes: &BTreeMap<NodeId, bool>) -> VoteResult {
    if ids.is_empty() {
        return VoteResult::Won;
    }
    let (mut yes, mut missing) = (0usize, 0usize);
    for id in ids {
        match votes.get(id) {
            Some(true) => yes += 1,
            Some(false) => {}
            None => missing += 1,
        }
    }
    let q = ids.len() / 2 + 1;
    if yes >= q {
        VoteResult::Won
    } else if yes + missing >= q {
        VoteResult::Pending
    } else {
        VoteResult::Lost
    }
}

pub(crate) fn vote_result(conf: &ConfState, votes: &BTreeMap<NodeId, bool>) -> VoteResult {
    let a = majority_vote(&conf.voters, votes);
    let b = majority_vote(&conf.voters_outgoing, votes);
    match (a, b) {
        (VoteResult::Won, VoteResult::Won) => VoteResult::Won,
        (VoteResult::Lost, _) | (_, VoteResult::Lost) => VoteResult::Lost,
        _ => VoteResult::Pending,
    }
}

/// 構成の過半数（joint なら両方）が `active` を満たすか。
pub(crate) fn quorum_active(conf: &ConfState, active: impl Fn(NodeId) -> bool) -> bool {
    quorum_value(conf, |id| active(id) as u64) == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn majority() {
        let c = ConfState::new([1, 2, 3]);
        let m: BTreeMap<u64, u64> = [(1, 5), (2, 3), (3, 1)].into();
        assert_eq!(quorum_value(&c, |id| m[&id]), 3);
        let c4 = ConfState::new([1, 2, 3, 4]);
        let m4: BTreeMap<u64, u64> = [(1, 5), (2, 4), (3, 3), (4, 1)].into();
        assert_eq!(quorum_value(&c4, |id| m4[&id]), 3);
    }

    #[test]
    fn joint() {
        let c = ConfState {
            voters: [4, 5, 6].into(),
            voters_outgoing: [1, 2, 3].into(),
            learners: Default::default(),
        };
        let m: BTreeMap<u64, u64> = [(1, 9), (2, 9), (3, 9), (4, 1), (5, 2), (6, 0)].into();
        assert_eq!(quorum_value(&c, |id| m[&id]), 1);
        let mut v = BTreeMap::new();
        v.insert(1, true);
        v.insert(2, true);
        assert_eq!(vote_result(&c, &v), VoteResult::Pending);
        v.insert(4, false);
        v.insert(5, false);
        assert_eq!(vote_result(&c, &v), VoteResult::Lost);
    }
}
