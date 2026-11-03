//! 制御状態の持ち主。変更は `propose` だけを通す: 写しに適用 → ログに書く → 入れ替え。
//! 書けなければ何も変わらない（500、retryable）。分散（M9）では `propose` を合意に置き換える。
//!
//! 適用したコマンドは、位置（`ControlState::applied_index`、適用に成功したコマンドの数）つきで通知し、
//! 直近の分を環（[`EventLog`]）に残す。SSE（`GET /api/v1/events`）の `Last-Event-ID` からの再開に使う。
//! 位置は単一ノードでもクラスタでも同じ数え方（失敗したコマンドは数えない。Raft の空のエントリや構成の
//! 変更も数えない）なので、クラスタのどのノードにつなぎ直しても同じ ID で続きから受けられる。

use crate::store::Store;
use senca_control::Problem;
use senca_control::jwt::JwtVerifier;
use senca_control::state::{Command, ControlState};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::broadcast;

/// 適用済みのコマンドの通知（SSE のイベントと、ノードの各部の反映に使う）。
#[derive(Debug, Clone)]
pub struct Applied {
    /// 適用後の `applied_index`（SSE の `id`）。
    pub index: u64,
    /// 適用した時刻（UNIX ミリ秒。起動時に読み直したものは読み直した時刻）。
    pub at_ms: i64,
    pub cmd: Arc<Command>,
}

/// 残すイベントの既定の数。
pub const DEFAULT_EVENT_LOG: usize = 4096;

/// 直近の適用の環。`floor` より後の位置はすべて入っている（`floor` 以前は捨てた・知らない）。
pub struct EventLog {
    cap: usize,
    items: VecDeque<Applied>,
    floor: u64,
}

impl EventLog {
    pub fn new(cap: usize, floor: u64) -> EventLog {
        EventLog { cap, items: VecDeque::new(), floor }
    }

    fn push(&mut self, a: Applied) {
        if self.cap == 0 {
            self.floor = a.index;
            return;
        }
        while self.items.len() >= self.cap {
            if let Some(x) = self.items.pop_front() {
                self.floor = x.index;
            }
        }
        self.items.push_back(a);
    }

    /// 位置を飛ばして状態を置き換えた（スナップショット）: それより前は続きを出せない。
    fn reset(&mut self, floor: u64) {
        self.items.clear();
        self.floor = floor;
    }

    /// `after` より後のものをすべて出せるなら、その一覧。出せない（古すぎる）なら None。
    pub fn since(&self, after: u64) -> Option<Vec<Applied>> {
        if after < self.floor {
            return None;
        }
        Some(self.items.iter().filter(|a| a.index > after).cloned().collect())
    }

    #[cfg(test)]
    pub fn floor(&self) -> u64 {
        self.floor
    }
}

pub struct ControlCore {
    pub state: ControlState,
    store: Option<Store>,
    pub jwt: Option<JwtVerifier>,
    pub applied_tx: broadcast::Sender<Applied>,
    pub events: EventLog,
}

impl ControlCore {
    #[cfg(test)]
    pub fn new(state: ControlState, store: Option<Store>, jwt: Option<JwtVerifier>) -> Self {
        Self::with_event_log(state, store, jwt, DEFAULT_EVENT_LOG)
    }

    pub fn with_event_log(state: ControlState, mut store: Option<Store>, jwt: Option<JwtVerifier>, cap: usize) -> Self {
        let (applied_tx, _) = broadcast::channel(1024);
        // 単一ノード: 起動時に読み直したログの分は、再起動をまたいでも続きを出せる
        let replayed = store.as_mut().map(|s| std::mem::take(&mut s.replayed)).unwrap_or_default();
        let first = replayed.first().map(|(i, _)| i.saturating_sub(1)).unwrap_or(state.applied_index);
        let mut events = EventLog::new(cap, first);
        let now = crate::util::now_ms();
        for (index, cmd) in replayed {
            events.push(Applied { index, at_ms: now, cmd: Arc::new(cmd) });
        }
        ControlCore { state, store, jwt, applied_tx, events }
    }

    fn notify(&mut self, cmd: Command) {
        let a = Applied { index: self.state.applied_index, at_ms: crate::util::now_ms(), cmd: Arc::new(cmd) };
        self.events.push(a.clone());
        let _ = self.applied_tx.send(a);
    }

    /// 合意で確定したコマンドを適用する（分散。全ノードで同じ順に呼ばれる）。
    /// 通知の位置は Raft のログの位置ではなく `applied_index`（全ノードで同じ。SSE の ID をそろえるため）。
    pub fn apply_committed(&mut self, cmd: Command) -> Result<(), Problem> {
        let r = self.state.apply(cmd.clone());
        if r.is_ok() {
            self.notify(cmd);
        }
        r
    }

    /// スナップショットから状態を置き換える。
    pub fn replace_state(&mut self, st: ControlState) {
        self.events.reset(st.applied_index);
        self.state = st;
    }

    /// 単一ノード: 写しに適用 → ログに書く → 入れ替え。
    pub fn propose(&mut self, cmd: Command) -> Result<(), Problem> {
        let mut next = self.state.clone();
        next.apply(cmd.clone())?;
        if let Some(store) = self.store.as_mut()
            && let Err(e) = store.append(&cmd, &next)
        {
            crate::util::log(&format!("control: failed to persist command: {e}"));
            return Err(Problem::internal().with_retryable(true));
        }
        self.state = next;
        self.notify(cmd);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(i: u64) -> Applied {
        Applied { index: i, at_ms: 0, cmd: Arc::new(Command::NextFencingToken) }
    }

    #[test]
    fn ring_keeps_the_recent_window() {
        let mut l = EventLog::new(3, 10);
        for i in 11..=15 {
            l.push(a(i));
        }
        // 11・12 は捨てた: 12 より後なら出せる
        assert_eq!(l.floor(), 12);
        assert!(l.since(11).is_none());
        assert_eq!(l.since(12).unwrap().iter().map(|x| x.index).collect::<Vec<_>>(), vec![13, 14, 15]);
        assert_eq!(l.since(15).unwrap().len(), 0);
        l.reset(100);
        assert!(l.since(99).is_none());
        assert!(l.since(100).unwrap().is_empty());
    }

    #[test]
    fn index_counts_applied_commands_only() {
        let mut c = ControlCore::new(ControlState::default(), None, None);
        let mut rx = c.applied_tx.subscribe();
        c.propose(Command::NextFencingToken).unwrap();
        // 失敗したコマンドは位置を進めない
        assert!(c.propose(Command::DeleteRoom { tenant: "t".into(), id: "x".into() }).is_err());
        c.apply_committed(Command::NextFencingToken).unwrap();
        assert_eq!(rx.try_recv().unwrap().index, 1);
        assert_eq!(rx.try_recv().unwrap().index, 2);
        assert_eq!(c.events.since(0).unwrap().len(), 2);
    }
}
