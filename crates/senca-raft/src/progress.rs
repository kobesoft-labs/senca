//! リーダーが相手ごとに持つ複製の進み具合。

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProgressState {
    /// 一致する位置を探している（1 件ずつ送り、応答を待つ）。
    Probe,
    /// 一致している。応答を待たずに続けて送る（inflight の上限まで）。
    Replicate,
    /// snapshot を送っている。
    Snapshot,
}

#[derive(Debug, Clone)]
pub(crate) struct Progress {
    pub match_: u64,
    pub next: u64,
    pub state: ProgressState,
    /// Probe で応答待ち。
    pub paused: bool,
    /// Replicate で送った Append の最後の index（古い順）。
    pub inflights: VecDeque<u64>,
    /// CheckQuorum の期間内に何か受け取ったか。
    pub recent_active: bool,
    /// ReadIndex の確認番号の応答の最大値。
    pub read_ack: u64,
    /// 送っている snapshot。
    pub snap_index: u64,
    pub snap_term: u64,
    pub snap_offset: u64,
    /// 前の心拍の応答の時点の `snap_offset`（進んでいなければ片を送り直す）。
    pub snap_hb_offset: u64,
}

impl Progress {
    pub fn new(next: u64) -> Progress {
        Progress {
            match_: 0,
            next,
            state: ProgressState::Probe,
            paused: false,
            inflights: VecDeque::new(),
            recent_active: true,
            read_ack: 0,
            snap_index: 0,
            snap_term: 0,
            snap_offset: 0,
            snap_hb_offset: u64::MAX,
        }
    }

    pub fn become_probe(&mut self) {
        if self.state == ProgressState::Snapshot {
            self.next = (self.match_ + 1).max(self.snap_index + 1);
        } else {
            self.next = self.match_ + 1;
        }
        self.state = ProgressState::Probe;
        self.paused = false;
        self.inflights.clear();
        self.snap_index = 0;
        self.snap_term = 0;
        self.snap_offset = 0;
    }

    pub fn become_replicate(&mut self) {
        self.state = ProgressState::Replicate;
        self.next = self.match_ + 1;
        self.paused = false;
        self.inflights.clear();
    }

    pub fn become_snapshot(&mut self, index: u64, term: u64) {
        self.state = ProgressState::Snapshot;
        self.paused = false;
        self.inflights.clear();
        self.snap_index = index;
        self.snap_term = term;
        self.snap_offset = 0;
        self.snap_hb_offset = u64::MAX;
    }

    /// 一致の報告。進んだら true。
    pub fn maybe_update(&mut self, n: u64) -> bool {
        let mut updated = false;
        if self.match_ < n {
            self.match_ = n;
            updated = true;
            self.paused = false;
        }
        self.next = self.next.max(n + 1);
        updated
    }

    /// 拒否の報告。`rejected` は拒否された prev_index、`hint` は巻き戻し先。古い応答なら false。
    pub fn maybe_decr_to(&mut self, rejected: u64, hint: u64) -> bool {
        if self.state == ProgressState::Replicate {
            if rejected <= self.match_ {
                return false;
            }
            self.next = self.match_ + 1;
            return true;
        }
        if self.next == 0 || self.next - 1 != rejected {
            return false;
        }
        self.next = rejected.min(hint.saturating_add(1)).max(self.match_ + 1);
        self.paused = false;
        true
    }

    pub fn is_paused(&self, max_inflight: usize) -> bool {
        match self.state {
            ProgressState::Probe => self.paused,
            ProgressState::Replicate => self.inflights.len() >= max_inflight,
            ProgressState::Snapshot => true,
        }
    }

    pub fn free_le(&mut self, index: u64) {
        while self.inflights.front().is_some_and(|&i| i <= index) {
            self.inflights.pop_front();
        }
    }
}
