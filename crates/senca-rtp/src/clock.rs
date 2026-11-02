//! NTP 時刻と RTP 時刻の対応（SR から。RFC 3550 6.4.1）。音声・映像の同期（lip sync）と撮影時刻の推定に使う。

use std::time::Duration;

/// 1900-01-01 から 1970-01-01 までの秒数。
pub const NTP_UNIX_OFFSET_SECS: u64 = 2_208_988_800;

/// 64 ビット NTP 時刻を UNIX 時刻（ns）に。1970 年より前は負。
pub fn ntp_to_unix_ns(ntp: u64) -> i64 {
    let secs = (ntp >> 32) as i64 - NTP_UNIX_OFFSET_SECS as i64;
    let frac = (((ntp & 0xFFFF_FFFF) as u128 * 1_000_000_000) >> 32) as i64;
    secs * 1_000_000_000 + frac
}

/// UNIX 時刻（ns）を 64 ビット NTP 時刻に（2036 年の折り返しは era 0 のまま扱う）。
pub fn unix_ns_to_ntp(unix_ns: i64) -> u64 {
    let total = unix_ns as i128 + NTP_UNIX_OFFSET_SECS as i128 * 1_000_000_000;
    let total = total.max(0) as u128;
    let secs = (total / 1_000_000_000) as u64;
    let frac = ((total % 1_000_000_000) << 32) / 1_000_000_000;
    (secs << 32) | frac as u64
}

/// 1 つの送信元の NTP ↔ RTP の対応。最新の SR を使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderClock {
    clock_rate: u32,
    ntp: u64,
    rtp: u32,
}

impl SenderClock {
    /// SR の (NTP, RTP timestamp) から作る。
    pub fn new(clock_rate: u32, ntp: u64, rtp_timestamp: u32) -> Self {
        SenderClock {
            clock_rate: clock_rate.max(1),
            ntp,
            rtp: rtp_timestamp,
        }
    }

    /// SR を受け取るたびに更新する。
    pub fn update(&mut self, ntp: u64, rtp_timestamp: u32) {
        self.ntp = ntp;
        self.rtp = rtp_timestamp;
    }

    /// RTP timestamp に対応する UNIX 時刻（ns）。SR からの差は 32 ビットの符号付きとして扱う（±半周）。
    pub fn rtp_to_unix_ns(&self, rtp_timestamp: u32) -> i64 {
        let diff = rtp_timestamp.wrapping_sub(self.rtp) as i32 as i64;
        ntp_to_unix_ns(self.ntp) + diff * 1_000_000_000 / self.clock_rate as i64
    }

    /// UNIX 時刻（ns）に対応する RTP timestamp。
    pub fn unix_ns_to_rtp(&self, unix_ns: i64) -> u32 {
        let d = unix_ns as i128 - ntp_to_unix_ns(self.ntp) as i128;
        let ticks = d * self.clock_rate as i128 / 1_000_000_000;
        self.rtp.wrapping_add(ticks as i64 as u32)
    }
}

/// 2 つのトラックの同じ瞬間の差（音声を基準にした映像のずれ）を求める。
///
/// `a`・`b` の SR 対応から、`a_rtp`・`b_rtp` の壁時計の差（b − a）を返す。
pub fn wallclock_offset(a: &SenderClock, a_rtp: u32, b: &SenderClock, b_rtp: u32) -> i64 {
    b.rtp_to_unix_ns(b_rtp) - a.rtp_to_unix_ns(a_rtp)
}

/// NTP 時刻の差を `Duration` に（負は 0）。
pub fn ntp_diff(later: u64, earlier: u64) -> Duration {
    let d = later.saturating_sub(earlier);
    Duration::from_nanos(((d as u128 * 1_000_000_000) >> 32) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntp_unix_roundtrip() {
        let t = 1_700_000_000_123_456_789i64;
        let n = unix_ns_to_ntp(t);
        assert_eq!(n >> 32, 1_700_000_000 + NTP_UNIX_OFFSET_SECS);
        assert!((ntp_to_unix_ns(n) - t).abs() <= 1);
        assert_eq!(ntp_to_unix_ns(NTP_UNIX_OFFSET_SECS << 32), 0);
    }

    #[test]
    fn rtp_mapping_and_wrap() {
        let base = unix_ns_to_ntp(1_000_000_000_000_000_000);
        let c = SenderClock::new(90000, base, 0xFFFF_FF00);
        // 1 秒後（折り返しをまたぐ）
        let r = 0xFFFF_FF00u32.wrapping_add(90000);
        assert_eq!(c.rtp_to_unix_ns(r) - ntp_to_unix_ns(base), 1_000_000_000);
        // 少し前
        assert_eq!(
            c.rtp_to_unix_ns(0xFFFF_FF00 - 900) - ntp_to_unix_ns(base),
            -10_000_000
        );
        assert_eq!(c.unix_ns_to_rtp(ntp_to_unix_ns(base) + 1_000_000_000), r);
    }

    #[test]
    fn lip_sync_offset() {
        let n = unix_ns_to_ntp(5_000_000_000);
        let audio = SenderClock::new(48000, n, 1000);
        let video = SenderClock::new(90000, n, 50000);
        // 音声 +480（10 ms）、映像 +2700（30 ms）→ 映像が 20 ms 後
        assert_eq!(wallclock_offset(&audio, 1480, &video, 52700), 20_000_000);
        assert_eq!(ntp_diff(n + (1 << 31), n), Duration::from_millis(500));
    }
}
