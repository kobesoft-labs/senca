//! 時間軸の換算と、折り返す時刻の拡張（仕様 4.2・4.3）。
//!
//! 換算はすべて i128 で行い、結果が i64 に収まらなければ `None` を返す（黙って丸めない）。

/// 時間軸（1 刻み = num/den 秒）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Timebase {
    pub num: u32,
    pub den: u32,
}

impl Timebase {
    pub const MILLIS: Timebase = Timebase::new(1, 1000);
    pub const MICROS: Timebase = Timebase::new(1, 1_000_000);
    pub const NANOS: Timebase = Timebase::new(1, 1_000_000_000);
    /// MPEG-TS・RTP 映像の 90 kHz。
    pub const MPEG: Timebase = Timebase::new(1, 90_000);

    pub const fn new(num: u32, den: u32) -> Self {
        assert!(num != 0 && den != 0, "timebase must be non-zero");
        Timebase { num, den }
    }

    /// 1 秒あたりの刻み数が整数のとき（1/den の形）はその値。
    pub fn hz(&self) -> Option<u32> {
        (self.num == 1).then_some(self.den)
    }
}

/// 時間軸の換算。
pub trait Rescale: Sized {
    /// `from` から `to` へ。最も近い値に丸める（ちょうど半分は 0 から遠い方）。収まらなければ `None`。
    fn rescale(self, from: Timebase, to: Timebase) -> Option<Self>;
    /// 切り捨て（負の無限大方向）で換算する。
    fn rescale_floor(self, from: Timebase, to: Timebase) -> Option<Self>;
}

impl Rescale for i64 {
    fn rescale(self, from: Timebase, to: Timebase) -> Option<i64> {
        if from == to {
            return Some(self);
        }
        let n = self as i128 * from.num as i128 * to.den as i128;
        let d = from.den as i128 * to.num as i128;
        let q = n / d;
        let r = n % d;
        let q = if 2 * r.abs() >= d { q + n.signum() } else { q };
        i64::try_from(q).ok()
    }

    fn rescale_floor(self, from: Timebase, to: Timebase) -> Option<i64> {
        if from == to {
            return Some(self);
        }
        let n = self as i128 * from.num as i128 * to.den as i128;
        let d = from.den as i128 * to.num as i128;
        i64::try_from(n.div_euclid(d)).ok()
    }
}

/// 折り返す時刻（RTMP の 32 bit ミリ秒、TS の 33 bit 90 kHz、RTP の 32 bit）を、
/// 単調に伸びる i64 に広げる。前の値から半周以内の進み・戻りを同じ周回とみなす。
#[derive(Debug, Clone)]
pub struct RolloverExtender {
    bits: u32,
    last: Option<i64>,
}

impl RolloverExtender {
    pub fn new(bits: u32) -> Self {
        assert!((1..=62).contains(&bits));
        RolloverExtender { bits, last: None }
    }

    /// 生の値（下位 `bits` ビットだけ使う）を広げる。
    pub fn extend(&mut self, raw: u64) -> i64 {
        let modulus = 1i64 << self.bits;
        let raw = (raw & (modulus as u64 - 1)) as i64;
        let v = match self.last {
            None => raw,
            Some(last) => {
                let base = last - last.rem_euclid(modulus);
                let mut cand = base + raw;
                let half = modulus / 2;
                if cand - last > half {
                    cand -= modulus;
                } else if last - cand > half {
                    cand += modulus;
                }
                cand
            }
        };
        self.last = Some(v);
        v
    }

    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rescale_rounds() {
        assert_eq!(1i64.rescale(Timebase::MILLIS, Timebase::MPEG), Some(90));
        assert_eq!(1001i64.rescale(Timebase::MPEG, Timebase::MILLIS), Some(11));
        assert_eq!((-1001i64).rescale(Timebase::MPEG, Timebase::MILLIS), Some(-11));
        assert_eq!(45i64.rescale(Timebase::MPEG, Timebase::MILLIS), Some(1)); // 0.5 → 1
        assert_eq!(44i64.rescale(Timebase::MPEG, Timebase::MILLIS), Some(0));
        assert_eq!((-45i64).rescale_floor(Timebase::MPEG, Timebase::MILLIS), Some(-1));
    }

    #[test]
    fn rescale_overflow() {
        assert_eq!(i64::MAX.rescale(Timebase::MILLIS, Timebase::NANOS), None);
        assert_eq!(i64::MAX.rescale(Timebase::NANOS, Timebase::MILLIS), Some(9_223_372_036_855));
    }

    #[test]
    fn rollover_32() {
        let mut e = RolloverExtender::new(32);
        assert_eq!(e.extend(0xFFFF_FF00), 0xFFFF_FF00);
        assert_eq!(e.extend(0x10), 0x1_0000_0010);
        // 少し戻る（B フレーム等）
        assert_eq!(e.extend(0xFFFF_FFF0), 0xFFFF_FFF0);
        assert_eq!(e.extend(0x20), 0x1_0000_0020);
    }

    #[test]
    fn rollover_33_backwards_from_start() {
        let mut e = RolloverExtender::new(33);
        assert_eq!(e.extend(10), 10);
        assert_eq!(e.extend((1u64 << 33) - 10), -10);
    }
}
