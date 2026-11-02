//! 核が持つログ（最後の snapshot より後ろの entry をすべてメモリに持つ）。
//!
//! 制御面のログは小さい（圧縮すれば数千件）ので、全体をメモリに置いて単純に保つ。
//! 永続化は Ready で呼び手に任せる。

use crate::types::{ConfState, Entry, EntryKind, Snapshot};

#[derive(Debug)]
pub(crate) struct RaftLog {
    pub snap_index: u64,
    pub snap_term: u64,
    pub snap_conf: ConfState,
    /// 最後の snapshot の中身（相手に送る）。index 0 の初期構成ではデータなし。
    pub snapshot: Option<Snapshot>,
    /// `entries[i].index == snap_index + 1 + i`。
    pub entries: Vec<Entry>,
    pub committed: u64,
    /// Ready で適用に渡した最後の index。
    pub applied: u64,
    /// これより後ろはまだ Ready で永続化に渡していない。
    pub unstable_from: u64,
    /// 受け取って、まだ Ready で渡していない snapshot。
    pub pending_snapshot: Option<Snapshot>,
}

impl RaftLog {
    pub fn first_index(&self) -> u64 {
        self.snap_index + 1
    }

    pub fn last_index(&self) -> u64 {
        self.snap_index + self.entries.len() as u64
    }

    pub fn term(&self, i: u64) -> Option<u64> {
        if i == self.snap_index {
            return Some(self.snap_term);
        }
        if i < self.snap_index || i > self.last_index() {
            return None;
        }
        Some(self.entries[(i - self.snap_index - 1) as usize].term)
    }

    pub fn last_term(&self) -> u64 {
        self.entries
            .last()
            .map(|e| e.term)
            .unwrap_or(self.snap_term)
    }

    pub fn entry(&self, i: u64) -> Option<&Entry> {
        if i <= self.snap_index || i > self.last_index() {
            return None;
        }
        Some(&self.entries[(i - self.snap_index - 1) as usize])
    }

    pub fn match_term(&self, i: u64, t: u64) -> bool {
        self.term(i) == Some(t)
    }

    pub fn is_up_to_date(&self, last_index: u64, last_term: u64) -> bool {
        let lt = self.last_term();
        last_term > lt || (last_term == lt && last_index >= self.last_index())
    }

    /// `[lo, hi]` の entry。`max_bytes` を超えないように切る（少なくとも 1 件）。
    pub fn slice(&self, lo: u64, hi: u64, max_bytes: u64) -> Vec<Entry> {
        let mut out = Vec::new();
        if lo <= self.snap_index || lo > hi || hi > self.last_index() {
            return out;
        }
        let mut size = 0u64;
        for i in lo..=hi {
            let e = &self.entries[(i - self.snap_index - 1) as usize];
            size += e.size();
            if !out.is_empty() && size > max_bytes {
                break;
            }
            out.push(e.clone());
        }
        out
    }

    /// `ents` と食い違う最初の index（なければ 0）。
    pub fn find_conflict(&self, ents: &[Entry]) -> u64 {
        for e in ents {
            if !self.match_term(e.index, e.term) {
                return e.index;
            }
        }
        0
    }

    /// `index` 以下で term が `term` 以下の最大の位置（巻き戻しの手がかり）。
    pub fn find_conflict_by_term(&self, index: u64, term: u64) -> u64 {
        let mut i = index.min(self.last_index());
        loop {
            match self.term(i) {
                Some(t) if t > term && i > 0 => i -= 1,
                _ => return i,
            }
        }
    }

    /// `ents[0].index` 以降を捨てて `ents` を足す。`ents[0].index` は `first_index..=last_index+1`。
    pub fn truncate_append(&mut self, ents: &[Entry]) {
        let Some(first) = ents.first() else { return };
        let at = first.index;
        debug_assert!(at > self.snap_index && at <= self.last_index() + 1);
        debug_assert!(at > self.committed, "truncating committed entries");
        self.entries.truncate((at - self.snap_index - 1) as usize);
        self.entries.extend_from_slice(ents);
        self.unstable_from = self.unstable_from.min(at);
    }

    pub fn commit_to(&mut self, c: u64) -> bool {
        let c = c.min(self.last_index());
        if c > self.committed {
            self.committed = c;
            true
        } else {
            false
        }
    }

    /// `index` の時点の構成。
    pub fn conf_at(&self, index: u64) -> (ConfState, u64) {
        let mut i = index.min(self.last_index());
        while i > self.snap_index {
            let e = &self.entries[(i - self.snap_index - 1) as usize];
            if e.kind == EntryKind::ConfChange
                && let Some(c) = e.conf_state()
            {
                return (c, i);
            }
            i -= 1;
        }
        (self.snap_conf.clone(), self.snap_index)
    }

    /// 自分で作った snapshot でログを詰める（`index` 以下を捨てる）。
    pub fn compact(&mut self, snap: Snapshot) {
        let idx = snap.meta.index;
        debug_assert!(idx > self.snap_index && idx <= self.last_index());
        self.entries.drain(..(idx - self.snap_index) as usize);
        self.snap_index = idx;
        self.snap_term = snap.meta.term;
        self.snap_conf = snap.meta.conf_state.clone();
        self.snapshot = Some(snap);
        self.unstable_from = self.unstable_from.max(idx + 1);
    }

    /// 受け取った snapshot を入れる。同じ (index, term) の entry があれば後ろを残す。
    pub fn restore(&mut self, snap: Snapshot) {
        let idx = snap.meta.index;
        if self.match_term(idx, snap.meta.term) {
            self.entries.drain(..(idx - self.snap_index) as usize);
        } else {
            self.entries.clear();
        }
        self.snap_index = idx;
        self.snap_term = snap.meta.term;
        self.snap_conf = snap.meta.conf_state.clone();
        self.committed = self.committed.max(idx);
        self.applied = self.applied.max(idx);
        self.unstable_from = self.unstable_from.max(idx + 1);
        self.snapshot = Some(snap.clone());
        self.pending_snapshot = Some(snap);
    }
}
