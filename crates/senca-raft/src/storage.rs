//! 永続化の抽象と、試験用のメモリ実装。

use crate::raft::Ready;
use crate::types::{Entry, HardState, Snapshot};
use std::io;

/// 起動時に記憶域から読む状態。
#[derive(Debug, Clone, Default)]
pub struct InitialState {
    pub hard_state: HardState,
    /// 最後の snapshot（新しいクラスタでは [`Snapshot::bootstrap`]、後から加わるノードでは `None`）。
    pub snapshot: Option<Snapshot>,
    /// snapshot の後ろの entry（index は連続）。
    pub entries: Vec<Entry>,
}

/// 永続化の書き手。どの操作も、返った時点で（`sync` の後に）電源断に耐えること。
/// 誤りを返したら、呼び手はノードを止める（メッセージを送らない・適用しない）。
pub trait Storage {
    /// 記憶域の状態を読む。
    fn load(&mut self) -> io::Result<InitialState>;
    /// `entries[0].index` 以降を捨ててから足す。
    fn append(&mut self, entries: &[Entry]) -> io::Result<()>;
    fn set_hard_state(&mut self, hs: &HardState) -> io::Result<()>;
    /// snapshot を保存する。`snap.meta.index` 以下の entry を捨てる。同じ (index, term) の entry を
    /// 持っていなければ、ログ全体を捨てる。
    fn save_snapshot(&mut self, snap: &Snapshot) -> io::Result<()>;
    /// ここまでの `append` を耐久にする。
    fn sync(&mut self) -> io::Result<()>;

    /// Ready の永続化（snapshot → entries → fsync → hard state の順）。
    /// hard state を最後に書くので、commit が手元のログより先を指すことはない。
    fn persist(&mut self, rd: &Ready) -> io::Result<()> {
        if let Some(s) = &rd.snapshot {
            self.save_snapshot(s)?;
        }
        if !rd.entries.is_empty() {
            self.append(&rd.entries)?;
            self.sync()?;
        }
        if let Some(hs) = &rd.hard_state {
            self.set_hard_state(hs)?;
        }
        Ok(())
    }
}

/// メモリだけの記憶域（試験用。書いた時点で「耐久」とみなす）。
#[derive(Debug, Clone, Default)]
pub struct MemStorage {
    pub hard_state: HardState,
    pub snapshot: Option<Snapshot>,
    pub entries: Vec<Entry>,
}

impl MemStorage {
    pub fn new() -> MemStorage {
        MemStorage::default()
    }

    /// 新しいクラスタの投票ノード用。
    pub fn bootstrap(voters: impl IntoIterator<Item = u64>) -> MemStorage {
        MemStorage {
            snapshot: Some(Snapshot::bootstrap(voters)),
            ..Default::default()
        }
    }

    fn snap_index(&self) -> u64 {
        self.snapshot.as_ref().map_or(0, |s| s.meta.index)
    }

    pub fn last_index(&self) -> u64 {
        self.entries.last().map_or(self.snap_index(), |e| e.index)
    }

    fn term_of(&self, i: u64) -> Option<u64> {
        if let Some(s) = &self.snapshot
            && s.meta.index == i
        {
            return Some(s.meta.term);
        }
        let first = self.entries.first()?.index;
        if i < first {
            return None;
        }
        self.entries.get((i - first) as usize).map(|e| e.term)
    }
}

impl Storage for MemStorage {
    fn load(&mut self) -> io::Result<InitialState> {
        Ok(InitialState {
            hard_state: self.hard_state,
            snapshot: self.snapshot.clone(),
            entries: self.entries.clone(),
        })
    }

    fn append(&mut self, entries: &[Entry]) -> io::Result<()> {
        let Some(first) = entries.first() else {
            return Ok(());
        };
        let si = self.snap_index();
        if first.index <= si || first.index > self.last_index() + 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "append out of range",
            ));
        }
        let keep = (first.index - si - 1) as usize;
        self.entries.truncate(keep);
        self.entries.extend_from_slice(entries);
        Ok(())
    }

    fn set_hard_state(&mut self, hs: &HardState) -> io::Result<()> {
        self.hard_state = *hs;
        Ok(())
    }

    fn save_snapshot(&mut self, snap: &Snapshot) -> io::Result<()> {
        let idx = snap.meta.index;
        if self.snapshot.is_some() && idx <= self.snap_index() {
            return Ok(());
        }
        if self.term_of(idx) == Some(snap.meta.term) {
            self.entries.retain(|e| e.index > idx);
        } else {
            self.entries.clear();
        }
        self.snapshot = Some(snap.clone());
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
}
