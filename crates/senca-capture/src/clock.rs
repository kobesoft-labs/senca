//! 時計（`CLOCK_MONOTONIC`・`CLOCK_REALTIME` のナノ秒）。
//!
//! V4L2 のバッファの時刻は `CLOCK_MONOTONIC` の絶対値なので、`std::time::Instant`（中身を取り出せない）
//! ではなく `clock_gettime` で同じ時計を読む。

fn read(clock: libc::clockid_t) -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` は有効な書き込み先。clock_gettime は `ts` に書くだけ。
    let r = unsafe { libc::clock_gettime(clock, &mut ts) };
    if r != 0 {
        return 0;
    }
    // time_t・c_long は 32 bit の環境では i32（そこでは変換が要る）。
    #[allow(clippy::useless_conversion)]
    let ns = i64::from(ts.tv_sec).saturating_mul(1_000_000_000) + i64::from(ts.tv_nsec);
    ns
}

/// `CLOCK_MONOTONIC`（ナノ秒）。
pub fn monotonic_ns() -> i64 {
    read(libc::CLOCK_MONOTONIC)
}

/// `CLOCK_REALTIME`（UNIX 時刻のナノ秒）。
pub fn realtime_ns() -> i64 {
    read(libc::CLOCK_REALTIME)
}

/// 単調時刻と UNIX 時刻の組（AU の撮影時刻 `capture_time_ns` を求める）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockAnchor {
    pub monotonic_ns: i64,
    pub realtime_ns: i64,
}

impl ClockAnchor {
    pub fn now() -> Self {
        ClockAnchor {
            monotonic_ns: monotonic_ns(),
            realtime_ns: realtime_ns(),
        }
    }

    /// 単調時刻を UNIX 時刻に直す。
    pub fn to_realtime(&self, monotonic: i64) -> i64 {
        self.realtime_ns + (monotonic - self.monotonic_ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clocks_advance() {
        let a = monotonic_ns();
        let b = monotonic_ns();
        assert!(a > 0 && b >= a);
        assert!(realtime_ns() > 1_600_000_000_000_000_000);
        let an = ClockAnchor::now();
        assert_eq!(an.to_realtime(an.monotonic_ns + 5), an.realtime_ns + 5);
    }
}
