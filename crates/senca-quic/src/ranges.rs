//! 重ならない半開区間 [start, end) の集合。ACK の追跡と stream の再送範囲に使う。

use std::collections::BTreeMap;
use std::ops::Range;

#[derive(Debug, Default, Clone)]
pub(crate) struct RangeSet {
    // start -> end（半開）
    map: BTreeMap<u64, u64>,
}

impl RangeSet {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 区間の数。
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    pub(crate) fn contains(&self, x: u64) -> bool {
        self.map.range(..=x).next_back().is_some_and(|(_, &e)| x < e)
    }

    pub(crate) fn min(&self) -> Option<u64> {
        self.map.keys().next().copied()
    }

    /// 最大の要素（end - 1）。
    #[cfg(test)]
    pub(crate) fn max(&self) -> Option<u64> {
        self.map.values().next_back().map(|e| e - 1)
    }

    /// 区間を足す。新しく加わった要素があれば `true`。
    pub(crate) fn insert(&mut self, r: Range<u64>) -> bool {
        if r.start >= r.end || self.gaps_within(r.clone()).is_empty() {
            return false;
        }
        let (mut s, mut e) = (r.start, r.end);
        if let Some((&ps, &pe)) = self.map.range(..=s).next_back()
            && pe >= s
        {
            s = ps;
            e = e.max(pe);
            self.map.remove(&ps);
        }
        while let Some((&ns, &ne)) = self.map.range(s..=e).next() {
            self.map.remove(&ns);
            e = e.max(ne);
        }
        self.map.insert(s, e);
        true
    }

    /// 区間を取り除く。
    pub(crate) fn remove(&mut self, r: Range<u64>) {
        if r.start >= r.end {
            return;
        }
        let mut affected = Vec::new();
        if let Some((&ps, &pe)) = self.map.range(..r.start).next_back()
            && pe > r.start
        {
            affected.push((ps, pe));
        }
        for (&s, &e) in self.map.range(r.start..r.end) {
            affected.push((s, e));
        }
        for (s, e) in affected {
            self.map.remove(&s);
            if s < r.start {
                self.map.insert(s, r.start);
            }
            if e > r.end {
                self.map.insert(r.end, e);
            }
        }
    }

    /// 最小の区間を取り出す（長さは最大 `max_len`）。
    pub(crate) fn pop_min(&mut self, max_len: u64) -> Option<Range<u64>> {
        let (&s, &e) = self.map.iter().next()?;
        self.map.remove(&s);
        let end = e.min(s.saturating_add(max_len.max(1)));
        if end < e {
            self.map.insert(end, e);
        }
        Some(s..end)
    }

    /// 最小の区間を取り除く（区間が多すぎるときの間引き）。
    pub(crate) fn remove_min_range(&mut self) {
        if let Some(&s) = self.map.keys().next() {
            self.map.remove(&s);
        }
    }

    /// 昇順の区間。
    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = Range<u64>> + '_ {
        self.map.iter().map(|(&s, &e)| s..e)
    }

    /// `r` のうち、まだ集合に含まれない部分。
    pub(crate) fn gaps_within(&self, r: Range<u64>) -> Vec<Range<u64>> {
        let mut out = Vec::new();
        let mut cur = r.start;
        if let Some((_, &pe)) = self.map.range(..=cur).next_back()
            && pe > cur
        {
            cur = pe;
        }
        if cur >= r.end {
            return out;
        }
        for (&s, &e) in self.map.range(cur..r.end) {
            if s > cur {
                out.push(cur..s);
            }
            cur = cur.max(e);
            if cur >= r.end {
                break;
            }
        }
        if cur < r.end {
            out.push(cur..r.end);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_merge_remove() {
        let mut s = RangeSet::new();
        assert!(s.insert(5..10));
        assert!(s.insert(0..2));
        assert!(!s.insert(6..8));
        assert!(s.insert(2..5));
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![0..10]);
        s.remove(3..4);
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![0..3, 4..10]);
        assert!(s.contains(4) && !s.contains(3));
        assert_eq!(s.gaps_within(0..12), vec![3..4, 10..12]);
        assert_eq!(s.pop_min(2), Some(0..2));
        assert_eq!(s.max(), Some(9));
        assert!(s.insert(20..30));
        assert!(s.insert(9..21));
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![2..3, 4..30]);
    }
}
