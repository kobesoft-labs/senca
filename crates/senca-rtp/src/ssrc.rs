//! SSRC の衝突とループの検出（RFC 3550 8.2）。送信元の取り違えを防ぐ。
//!
//! 送信元のアドレスは呼び出し側の型 `A`（`SocketAddr` やチャンネル番号など）で持つ。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 判定の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsrcCheck {
    /// 初めての SSRC（登録した）。
    New,
    /// 既知の SSRC が同じアドレスから来た。
    Known,
    /// 自分の SSRC と同じ値を他者が使っている。自分の SSRC を選び直し、古い SSRC で BYE を送る。
    LocalCollision,
    /// 既知の SSRC が別のアドレスから来た（第三者の衝突かループ）。パケットは捨てる。
    Conflict,
    /// 送信元の数が上限に達した。パケットは捨てる。
    TooMany,
}

#[derive(Debug, Clone)]
struct Entry<A> {
    addr: A,
    last_seen: Instant,
}

/// SSRC と送信元アドレスの対応表。
#[derive(Debug, Clone)]
pub struct SsrcTable<A> {
    local: Vec<u32>,
    sources: HashMap<u32, Entry<A>>,
    max_sources: usize,
    collisions: u64,
    conflicts: u64,
}

impl<A: Clone + PartialEq> SsrcTable<A> {
    /// `local` は自分が送信に使う SSRC。`max_sources` は覚える送信元の上限。
    pub fn new(local: &[u32], max_sources: usize) -> Self {
        SsrcTable {
            local: local.to_vec(),
            sources: HashMap::new(),
            max_sources,
            collisions: 0,
            conflicts: 0,
        }
    }

    /// 自分の SSRC を置き換える（衝突のあと）。
    pub fn replace_local(&mut self, old: u32, new: u32) {
        for s in &mut self.local {
            if *s == old {
                *s = new;
            }
        }
    }

    /// 受け取った RTP・RTCP の SSRC を調べる。
    pub fn check(&mut self, ssrc: u32, from: &A, now: Instant) -> SsrcCheck {
        if self.local.contains(&ssrc) {
            self.collisions += 1;
            return SsrcCheck::LocalCollision;
        }
        match self.sources.get_mut(&ssrc) {
            Some(e) if e.addr == *from => {
                e.last_seen = now;
                SsrcCheck::Known
            }
            Some(_) => {
                self.conflicts += 1;
                SsrcCheck::Conflict
            }
            None => {
                if self.sources.len() >= self.max_sources {
                    return SsrcCheck::TooMany;
                }
                self.sources.insert(
                    ssrc,
                    Entry {
                        addr: from.clone(),
                        last_seen: now,
                    },
                );
                SsrcCheck::New
            }
        }
    }

    /// BYE を受け取った。
    pub fn remove(&mut self, ssrc: u32) {
        self.sources.remove(&ssrc);
    }

    /// `timeout` より長く音沙汰のない送信元を消す。消した SSRC を返す。
    pub fn expire(&mut self, now: Instant, timeout: Duration) -> Vec<u32> {
        let mut gone = Vec::new();
        self.sources.retain(|ssrc, e| {
            let keep = now.saturating_duration_since(e.last_seen) <= timeout;
            if !keep {
                gone.push(*ssrc);
            }
            keep
        });
        gone
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// (自分との衝突の回数, 第三者の衝突・ループの回数)。
    pub fn counters(&self) -> (u64, u64) {
        (self.collisions, self.conflicts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_collisions() {
        let t = Instant::now();
        let mut tb: SsrcTable<u16> = SsrcTable::new(&[42], 2);
        assert_eq!(tb.check(1, &5000, t), SsrcCheck::New);
        assert_eq!(tb.check(1, &5000, t), SsrcCheck::Known);
        assert_eq!(tb.check(1, &6000, t), SsrcCheck::Conflict);
        assert_eq!(tb.check(42, &7000, t), SsrcCheck::LocalCollision);
        tb.replace_local(42, 43);
        assert_eq!(tb.check(42, &7000, t), SsrcCheck::New);
        assert_eq!(tb.check(99, &7000, t), SsrcCheck::TooMany);
        assert_eq!(tb.counters(), (1, 1));
        let gone = tb.expire(t + Duration::from_secs(60), Duration::from_secs(30));
        assert_eq!(gone.len(), 2);
        assert!(tb.is_empty());
    }
}
