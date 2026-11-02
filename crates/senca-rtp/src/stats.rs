//! 受信統計（RFC 3550 附属書 A.1 の送信元検証・A.3 の損失・A.8 のジッター）と受信報告の組み立て。

use crate::rtcp::ReportBlock;
use std::time::{Duration, Instant};

const MAX_DROPOUT: u16 = 3000;
const MAX_MISORDER: u16 = 100;
const MIN_SEQUENTIAL: u32 = 2;
const RTP_SEQ_MOD: u32 = 1 << 16;

/// パケット 1 つを受け取った結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqUpdate {
    /// 有効。拡張済みの番号。
    Valid(u64),
    /// 送信元の検証中（probation）。まだ使わない。
    Probation,
    /// 大きく飛んだ・大きく戻った。次が続けば送信元の再始動とみなす。
    Bad,
    /// 重複または少しの戻り（並べ替えで扱う）。拡張済みの番号。
    Reordered(u64),
}

/// 1 つの送信元（SSRC）の受信統計。
#[derive(Debug, Clone)]
pub struct ReceiverStats {
    clock_rate: u32,
    max_seq: u16,
    cycles: u32,
    base_seq: u32,
    bad_seq: u32,
    probation: u32,
    received: u32,
    expected_prior: u32,
    received_prior: u32,
    /// 前のパケットの transit（RTP 単位）。
    transit: Option<i64>,
    /// ジッター（RTP 単位の 16 倍。A.8 の整数版）。
    jitter_q4: u64,
    epoch: Option<Instant>,
    last_sr: Option<(u32, Instant)>,
    initialized: bool,
}

impl ReceiverStats {
    /// `clock_rate` は RTP の時計の周波数（ジッターの計算に使う）。
    pub fn new(clock_rate: u32) -> Self {
        ReceiverStats {
            clock_rate: clock_rate.max(1),
            max_seq: 0,
            cycles: 0,
            base_seq: 0,
            bad_seq: RTP_SEQ_MOD + 1,
            probation: MIN_SEQUENTIAL,
            received: 0,
            expected_prior: 0,
            received_prior: 0,
            transit: None,
            jitter_q4: 0,
            epoch: None,
            last_sr: None,
            initialized: false,
        }
    }

    fn init_seq(&mut self, seq: u16) {
        self.base_seq = seq as u32;
        self.max_seq = seq;
        self.bad_seq = RTP_SEQ_MOD + 1;
        self.cycles = 0;
        self.received = 0;
        self.received_prior = 0;
        self.expected_prior = 0;
    }

    fn extended_max(&self) -> u64 {
        self.cycles as u64 + self.max_seq as u64
    }

    /// A.1 の update_seq。
    fn update_seq(&mut self, seq: u16) -> SeqUpdate {
        if !self.initialized {
            self.initialized = true;
            self.init_seq(seq);
            self.max_seq = seq.wrapping_sub(1);
            self.probation = MIN_SEQUENTIAL;
        }
        let udelta = seq.wrapping_sub(self.max_seq);
        if self.probation > 0 {
            if seq == self.max_seq.wrapping_add(1) {
                self.probation -= 1;
                self.max_seq = seq;
                if self.probation == 0 {
                    self.init_seq(seq);
                    self.received += 1;
                    return SeqUpdate::Valid(self.extended_max());
                }
            } else {
                self.probation = MIN_SEQUENTIAL - 1;
                self.max_seq = seq;
            }
            return SeqUpdate::Probation;
        } else if udelta < MAX_DROPOUT {
            if seq < self.max_seq {
                self.cycles = self.cycles.wrapping_add(RTP_SEQ_MOD);
            }
            self.max_seq = seq;
        } else if udelta as u32 <= RTP_SEQ_MOD - MAX_MISORDER as u32 {
            if seq as u32 == self.bad_seq {
                // 2 つ続いた: 送信元が再始動したとみなす
                self.init_seq(seq);
            } else {
                self.bad_seq = (seq as u32 + 1) & (RTP_SEQ_MOD - 1);
                return SeqUpdate::Bad;
            }
        } else {
            // 重複・並べ替え
            self.received += 1;
            let back = self.max_seq.wrapping_sub(seq) as u64;
            return SeqUpdate::Reordered(self.extended_max().saturating_sub(back));
        }
        self.received += 1;
        SeqUpdate::Valid(self.extended_max())
    }

    /// パケットを受け取る。`arrival` は受信した時刻（単調時計）。
    pub fn on_packet(&mut self, seq: u16, rtp_timestamp: u32, arrival: Instant) -> SeqUpdate {
        let r = self.update_seq(seq);
        if matches!(r, SeqUpdate::Valid(_) | SeqUpdate::Reordered(_)) {
            let epoch = *self.epoch.get_or_insert(arrival);
            let elapsed = arrival.saturating_duration_since(epoch);
            let arrival_rtp = (elapsed.as_nanos() * self.clock_rate as u128 / 1_000_000_000) as i64;
            // transit は 32 ビットで折り返す時刻の差なので、差は i32 として扱う
            let transit = arrival_rtp.wrapping_sub(rtp_timestamp as i64);
            if let Some(prev) = self.transit {
                let d = ((transit - prev) as i32).unsigned_abs() as u64;
                // J += (|D| - J) / 16 を 16 倍の整数で（A.8）
                self.jitter_q4 = self.jitter_q4 + d - ((self.jitter_q4 + 8) >> 4);
            }
            self.transit = Some(transit);
        }
        r
    }

    /// 送信者報告を受け取った（LSR・DLSR の計算に使う）。`ntp` は SR の 64 ビット NTP 時刻。
    pub fn on_sender_report(&mut self, ntp: u64, now: Instant) {
        self.last_sr = Some(((ntp >> 16) as u32, now));
    }

    /// 受け取った有効なパケットの数。
    pub fn received(&self) -> u32 {
        self.received
    }

    /// 期待したパケットの数（拡張済みの最大番号 − 最初の番号 + 1）。
    pub fn expected(&self) -> u64 {
        if !self.initialized || self.probation > 0 {
            return 0;
        }
        self.extended_max() + 1 - self.base_seq as u64
    }

    /// 累積の損失（重複があると負になりうる。24 ビットの符号付きに収める）。
    pub fn cumulative_lost(&self) -> i32 {
        let lost = self.expected() as i64 - self.received as i64;
        lost.clamp(-0x80_0000, 0x7F_FFFF) as i32
    }

    /// ジッター（RTP 単位）。
    pub fn jitter(&self) -> u32 {
        (self.jitter_q4 >> 4).min(u32::MAX as u64) as u32
    }

    /// 拡張済みの最大番号（下位 32 ビット）。
    pub fn extended_highest(&self) -> u32 {
        self.extended_max() as u32
    }

    /// 受信報告ブロックを作る（fraction lost の区間はここで区切る）。
    pub fn report_block(&mut self, ssrc: u32, now: Instant) -> ReportBlock {
        let expected = self.expected() as u32;
        let expected_interval = expected.wrapping_sub(self.expected_prior);
        self.expected_prior = expected;
        let received_interval = self.received.wrapping_sub(self.received_prior);
        self.received_prior = self.received;
        let lost_interval = expected_interval as i64 - received_interval as i64;
        let fraction_lost = if expected_interval == 0 || lost_interval <= 0 {
            0
        } else {
            ((lost_interval << 8) / expected_interval as i64).min(255) as u8
        };
        let (lsr, dlsr) = match self.last_sr {
            Some((lsr, at)) => {
                let d = now.saturating_duration_since(at);
                (lsr, duration_to_q16(d))
            }
            None => (0, 0),
        };
        ReportBlock {
            ssrc,
            fraction_lost,
            cumulative_lost: self.cumulative_lost(),
            highest_seq: self.extended_highest(),
            jitter: self.jitter(),
            lsr,
            dlsr,
        }
    }
}

/// 1/65536 秒単位（DLSR）に直す。
pub fn duration_to_q16(d: Duration) -> u32 {
    let v = d.as_nanos() * 65536 / 1_000_000_000;
    v.min(u32::MAX as u128) as u32
}

/// 受信報告から往復時間を求める（RFC 3550 6.4.1）。`now_ntp` は報告を受け取った時刻の NTP 時刻。
pub fn round_trip_time(block: &ReportBlock, now_ntp: u64) -> Option<Duration> {
    if block.lsr == 0 {
        return None;
    }
    let a = (now_ntp >> 16) as u32;
    let rtt = a.wrapping_sub(block.lsr).wrapping_sub(block.dlsr);
    if rtt > 0x8000_0000 {
        return None;
    }
    Some(Duration::from_nanos(rtt as u64 * 1_000_000_000 / 65536))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probation_then_valid() {
        let t = Instant::now();
        let mut s = ReceiverStats::new(90000);
        assert_eq!(s.on_packet(100, 0, t), SeqUpdate::Probation);
        assert_eq!(s.on_packet(101, 0, t), SeqUpdate::Valid(101));
        assert_eq!(s.on_packet(102, 0, t), SeqUpdate::Valid(102));
        assert_eq!(s.received(), 2);
        assert_eq!(s.expected(), 2);
    }

    #[test]
    fn wrap_and_loss() {
        let t = Instant::now();
        let mut s = ReceiverStats::new(8000);
        s.on_packet(65530, 0, t);
        for seq in 65531u16..=65535 {
            s.on_packet(seq, 0, t);
        }
        // 0, 1 を落として 2.. を受ける
        for seq in 2u16..10 {
            assert!(matches!(s.on_packet(seq, 0, t), SeqUpdate::Valid(_)));
        }
        assert_eq!(s.extended_highest(), 65536 + 9);
        assert_eq!(s.cumulative_lost(), 2);
        let rb = s.report_block(1, t);
        assert_eq!(rb.cumulative_lost, 2);
        assert_eq!(rb.fraction_lost, (2 * 256 / 15) as u8);
        // 次の区間は損失なし
        s.on_packet(10, 0, t);
        assert_eq!(s.report_block(1, t).fraction_lost, 0);
    }

    #[test]
    fn reorder_and_duplicate() {
        let t = Instant::now();
        let mut s = ReceiverStats::new(8000);
        for seq in 0u16..5 {
            s.on_packet(seq, 0, t);
        }
        assert_eq!(s.on_packet(3, 0, t), SeqUpdate::Reordered(3));
        // 重複を数えるので損失は負になりうる
        assert_eq!(s.cumulative_lost(), -1);
    }

    #[test]
    fn restart_after_two_bad() {
        let t = Instant::now();
        let mut s = ReceiverStats::new(8000);
        for seq in 0u16..5 {
            s.on_packet(seq, 0, t);
        }
        assert_eq!(s.on_packet(30000, 0, t), SeqUpdate::Bad);
        assert!(matches!(s.on_packet(30001, 0, t), SeqUpdate::Valid(_)));
    }

    #[test]
    fn jitter_constant_is_zero_and_grows() {
        let t0 = Instant::now();
        let mut s = ReceiverStats::new(8000);
        for i in 0u16..50 {
            let at = t0 + Duration::from_millis(20 * i as u64);
            s.on_packet(i, 160 * i as u32, at);
        }
        assert!(s.jitter() <= 1);
        // 到着が 10 ms（80 単位）ずつ交互にずれる
        let mut s = ReceiverStats::new(8000);
        for i in 0u16..400 {
            let off = if i % 2 == 0 { 0 } else { 10 };
            let at = t0 + Duration::from_millis(20 * i as u64 + off);
            s.on_packet(i, 160 * i as u32, at);
        }
        // |D| = 80 → J は 80 に近づく
        assert!((70..=80).contains(&s.jitter()), "{}", s.jitter());
    }

    #[test]
    fn lsr_dlsr_rtt() {
        let t = Instant::now();
        let mut s = ReceiverStats::new(90000);
        s.on_packet(1, 0, t);
        s.on_packet(2, 0, t);
        let ntp = 0x0001_0002_0003_0004u64;
        s.on_sender_report(ntp, t);
        let rb = s.report_block(5, t + Duration::from_millis(500));
        assert_eq!(rb.lsr, 0x0002_0003);
        assert_eq!(rb.dlsr, 32768);
        // 送り手側: LSR から 700 ms 後に受け取った → RTT 200 ms
        let now_ntp = ntp + ((7u64 << 32) / 10);
        let rtt = round_trip_time(&rb, now_ntp).unwrap();
        assert!((rtt.as_millis() as i64 - 200).abs() <= 1, "{rtt:?}");
    }
}
