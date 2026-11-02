//! replay window（RFC 3711 3.3.2）。index は 64 bit の単調な値として扱う。

/// 判定の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayCheck {
    /// 未使用の index。
    Fresh,
    /// 既に受け付けた index。
    Duplicate,
    /// window より古い index。
    TooOld,
}

/// 幅 `size` bit の sliding window。最大 index を基準に、それより小さい index の使用済みを bit で持つ。
#[derive(Debug, Clone)]
pub struct ReplayWindow {
    size: usize,
    /// bit k が「top - k を受け付けた」を表す。
    bits: Vec<u64>,
    top: u64,
    any: bool,
}

impl ReplayWindow {
    /// 幅を 64 の倍数に切り上げて作る。範囲外は 64〜1024 に丸める。
    pub fn new(size: usize) -> Self {
        let size = size
            .clamp(crate::MIN_REPLAY_WINDOW, crate::MAX_REPLAY_WINDOW)
            .div_ceil(64)
            * 64;
        Self {
            size,
            bits: vec![0; size / 64],
            top: 0,
            any: false,
        }
    }

    /// window の幅。
    pub fn size(&self) -> usize {
        self.size
    }

    /// これまでの最大 index（未使用なら `None`）。
    pub fn top(&self) -> Option<u64> {
        self.any.then_some(self.top)
    }

    /// 状態を変えずに判定する。
    pub fn check(&self, index: u64) -> ReplayCheck {
        if !self.any || index > self.top {
            return ReplayCheck::Fresh;
        }
        let delta = self.top - index;
        if delta >= self.size as u64 {
            return ReplayCheck::TooOld;
        }
        let d = delta as usize;
        if self.bits[d / 64] & (1u64 << (d % 64)) != 0 {
            ReplayCheck::Duplicate
        } else {
            ReplayCheck::Fresh
        }
    }

    /// 使用済みにする。`check` が `Fresh` を返した後で呼ぶ。
    pub fn accept(&mut self, index: u64) {
        if !self.any {
            self.any = true;
            self.top = index;
            self.bits.iter_mut().for_each(|w| *w = 0);
            self.bits[0] = 1;
            return;
        }
        if index > self.top {
            let shift = index - self.top;
            self.shift(shift);
            self.top = index;
            self.bits[0] |= 1;
        } else {
            let d = (self.top - index) as usize;
            if d < self.size {
                self.bits[d / 64] |= 1u64 << (d % 64);
            }
        }
    }

    /// bit 列を `n` だけ古い方へずらす。
    fn shift(&mut self, n: u64) {
        if n >= self.size as u64 {
            self.bits.iter_mut().for_each(|w| *w = 0);
            return;
        }
        let n = n as usize;
        let words = n / 64;
        let bits = n % 64;
        let len = self.bits.len();
        for i in (0..len).rev() {
            let src = i.checked_sub(words);
            let hi = src.map(|s| self.bits[s]).unwrap_or(0);
            let lo = src
                .and_then(|s| s.checked_sub(1))
                .map(|s| self.bits[s])
                .unwrap_or(0);
            self.bits[i] = if bits == 0 {
                hi
            } else {
                (hi << bits) | (lo >> (64 - bits))
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let mut w = ReplayWindow::new(64);
        assert_eq!(w.check(10), ReplayCheck::Fresh);
        w.accept(10);
        assert_eq!(w.check(10), ReplayCheck::Duplicate);
        assert_eq!(w.check(9), ReplayCheck::Fresh);
        w.accept(9);
        assert_eq!(w.check(9), ReplayCheck::Duplicate);
        w.accept(100);
        assert_eq!(w.check(36), ReplayCheck::TooOld);
        assert_eq!(w.check(37), ReplayCheck::Fresh);
        assert_eq!(w.check(100), ReplayCheck::Duplicate);
    }

    #[test]
    fn wide_window_shift_across_words() {
        let mut w = ReplayWindow::new(1000);
        assert_eq!(w.size(), 1024);
        for i in (0..3000u64).step_by(7) {
            assert_eq!(w.check(i), ReplayCheck::Fresh);
            w.accept(i);
        }
        let top = w.top().unwrap();
        for i in (0..3000u64).step_by(7) {
            let expect = if top - i >= 1024 {
                ReplayCheck::TooOld
            } else {
                ReplayCheck::Duplicate
            };
            assert_eq!(w.check(i), expect, "i={i}");
        }
        for i in (top - 1023)..top {
            if i % 7 != 0 {
                assert_eq!(w.check(i), ReplayCheck::Fresh, "i={i}");
            }
        }
    }

    #[test]
    fn out_of_order_within_window() {
        let mut w = ReplayWindow::new(128);
        let order = [5u64, 3, 130, 4, 6, 129, 131, 70];
        for &i in &order {
            assert_eq!(w.check(i), ReplayCheck::Fresh, "{i}");
            w.accept(i);
        }
        for &i in &order {
            let e = if 131 - i >= 128 {
                ReplayCheck::TooOld
            } else {
                ReplayCheck::Duplicate
            };
            assert_eq!(w.check(i), e, "{i}");
        }
    }
}
