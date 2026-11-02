//! 損失検出・RTT・輻輳制御（RFC 9002）。

use std::time::{Duration, Instant};

use crate::streams::StreamId;

/// timer の粒度（RFC 9002 6.1.2）。
pub(crate) const GRANULARITY: Duration = Duration::from_millis(1);
/// packet threshold（RFC 9002 6.1.1）。
pub(crate) const PACKET_THRESHOLD: u64 = 3;
/// persistent congestion の係数（RFC 9002 7.6.1）。
const PERSISTENT_CONGESTION_THRESHOLD: u32 = 3;

/// 送ったフレームの記録（ACK・損失時の処理に使う）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SentFrame {
    Ack { largest: u64 },
    Crypto { offset: u64, len: usize },
    Stream { id: StreamId, offset: u64, len: usize, fin: bool },
    ResetStream { id: StreamId },
    StopSending { id: StreamId, code: u64 },
    MaxData,
    MaxStreamData { id: StreamId },
    MaxStreams { bidi: bool },
    DataBlocked,
    StreamDataBlocked { id: StreamId },
    StreamsBlocked { bidi: bool },
    NewConnectionId { seq: u64 },
    RetireConnectionId { seq: u64 },
    HandshakeDone,
    Ping,
    Datagram,
    PathChallenge,
    PathResponse,
}

#[derive(Debug, Clone)]
pub(crate) struct SentPacket {
    pub time: Instant,
    pub size: usize,
    pub ack_eliciting: bool,
    pub in_flight: bool,
    pub frames: Vec<SentFrame>,
}

/// RTT の推定（RFC 9002 5 章）。
#[derive(Debug, Clone)]
pub(crate) struct Rtt {
    latest: Duration,
    smoothed: Option<Duration>,
    var: Duration,
    min: Duration,
    initial: Duration,
}

impl Rtt {
    pub(crate) fn new(initial: Duration) -> Self {
        Self {
            latest: initial,
            smoothed: None,
            var: initial / 2,
            min: Duration::ZERO,
            initial,
        }
    }

    pub(crate) fn update(&mut self, latest: Duration, ack_delay: Duration, confirmed: bool, max_ack_delay: Duration) {
        self.latest = latest;
        match self.smoothed {
            None => {
                self.min = latest;
                self.smoothed = Some(latest);
                self.var = latest / 2;
            }
            Some(srtt) => {
                self.min = self.min.min(latest);
                let ack_delay = if confirmed { ack_delay.min(max_ack_delay) } else { ack_delay };
                let adjusted = if latest >= self.min + ack_delay { latest - ack_delay } else { latest };
                let diff = srtt.abs_diff(adjusted);
                self.var = (self.var * 3 + diff) / 4;
                self.smoothed = Some((srtt * 7 + adjusted) / 8);
            }
        }
    }

    pub(crate) fn smoothed(&self) -> Duration {
        self.smoothed.unwrap_or(self.initial)
    }

    pub(crate) fn has_sample(&self) -> bool {
        self.smoothed.is_some()
    }

    /// PTO の基本（max_ack_delay を除く）。
    pub(crate) fn pto_base(&self) -> Duration {
        self.smoothed() + (self.var * 4).max(GRANULARITY)
    }

    /// 時間による損失判定の遅れ（RFC 9002 6.1.2）。
    pub(crate) fn loss_delay(&self) -> Duration {
        (self.latest.max(self.smoothed()) * 9 / 8).max(GRANULARITY)
    }

    pub(crate) fn persistent_congestion_duration(&self, max_ack_delay: Duration) -> Duration {
        (self.smoothed() + (self.var * 4).max(GRANULARITY) + max_ack_delay) * PERSISTENT_CONGESTION_THRESHOLD
    }

    /// 経路が変わったら初期値に戻す（RFC 9000 9.4）。
    pub(crate) fn reset(&mut self) {
        *self = Self::new(self.initial);
    }
}

/// NewReno（RFC 9002 7 章、付録 B）。
#[derive(Debug, Clone)]
pub(crate) struct NewReno {
    window: u64,
    ssthresh: u64,
    recovery_start: Option<Instant>,
    acked_in_ca: u64,
    max_datagram: u64,
    in_flight: u64,
}

impl NewReno {
    pub(crate) fn new(max_datagram: usize) -> Self {
        let md = max_datagram as u64;
        Self {
            window: Self::initial_window(md),
            ssthresh: u64::MAX,
            recovery_start: None,
            acked_in_ca: 0,
            max_datagram: md,
            in_flight: 0,
        }
    }

    fn initial_window(md: u64) -> u64 {
        (10 * md).min((2 * md).max(14720))
    }

    fn min_window(&self) -> u64 {
        2 * self.max_datagram
    }

    pub(crate) fn window(&self) -> u64 {
        self.window
    }

    /// 今送ってよいバイト数。
    pub(crate) fn available(&self) -> u64 {
        self.window.saturating_sub(self.in_flight)
    }

    pub(crate) fn on_sent(&mut self, bytes: usize) {
        self.in_flight += bytes as u64;
    }

    /// in flight から外す（鍵の破棄など、ACK・損失以外の理由）。
    pub(crate) fn remove(&mut self, bytes: usize) {
        self.in_flight = self.in_flight.saturating_sub(bytes as u64);
    }

    fn in_recovery(&self, sent: Instant) -> bool {
        self.recovery_start.is_some_and(|r| sent <= r)
    }

    pub(crate) fn on_ack(&mut self, sent: Instant, bytes: usize, app_limited: bool) {
        self.remove(bytes);
        if self.in_recovery(sent) || app_limited {
            return;
        }
        if self.window < self.ssthresh {
            self.window += bytes as u64;
        } else {
            self.acked_in_ca += bytes as u64;
            if self.acked_in_ca >= self.window {
                self.acked_in_ca -= self.window;
                self.window += self.max_datagram;
            }
        }
    }

    /// 損失・ECN による輻輳（最も新しい失われたパケットの送信時刻）。
    pub(crate) fn on_congestion(&mut self, sent: Instant, now: Instant) {
        if self.in_recovery(sent) {
            return;
        }
        self.recovery_start = Some(now);
        self.window = (self.window / 2).max(self.min_window());
        self.ssthresh = self.window;
        self.acked_in_ca = 0;
    }

    pub(crate) fn on_persistent_congestion(&mut self) {
        self.window = self.min_window();
        self.recovery_start = None;
        self.acked_in_ca = 0;
    }

    /// 経路が変わったら初期値に戻す（in flight はそのまま）。
    pub(crate) fn reset(&mut self) {
        let f = self.in_flight;
        *self = Self::new(self.max_datagram as usize);
        self.in_flight = f;
    }
}

/// pacing（RFC 9002 7.7）。cwnd/srtt の 1.25 倍の速さで、最大 10 パケットのバーストを許す。
#[derive(Debug, Clone)]
pub(crate) struct Pacer {
    tokens: u64,
    last: Option<Instant>,
}

impl Pacer {
    pub(crate) fn new() -> Self {
        Self { tokens: 0, last: None }
    }

    /// `bytes` を送れる最も早い時刻。今送れるなら `None`。
    pub(crate) fn delay(&mut self, now: Instant, srtt: Duration, window: u64, mtu: usize) -> Option<Instant> {
        let mtu = mtu as u64;
        let capacity = (10 * mtu).min(window).max(2 * mtu);
        let srtt_ns = srtt.as_nanos().max(1_000_000) as u64;
        // bytes / ns = window * 5/4 / srtt
        match self.last {
            None => {
                self.tokens = capacity;
                self.last = Some(now);
            }
            Some(l) => {
                let el = now.saturating_duration_since(l).as_nanos() as u64;
                let add = (u128::from(el) * u128::from(window) * 5 / 4 / u128::from(srtt_ns)) as u64;
                // 端数を失わないよう、増えたときだけ基準時刻を進める
                if add > 0 {
                    self.tokens = self.tokens.saturating_add(add).min(capacity);
                    self.last = Some(now);
                }
            }
        }
        if self.tokens >= mtu {
            return None;
        }
        let need = mtu - self.tokens;
        let wait_ns = (u128::from(need) * u128::from(srtt_ns) * 4).div_ceil(5 * u128::from(window.max(1))) as u64 + 1;
        Some(now + Duration::from_nanos(wait_ns.max(1)))
    }

    pub(crate) fn on_sent(&mut self, bytes: usize) {
        self.tokens = self.tokens.saturating_sub(bytes as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newreno_halves_on_loss_and_grows() {
        let now = Instant::now();
        let mut c = NewReno::new(1200);
        let w0 = c.window();
        assert_eq!(w0, 12000);
        c.on_sent(1200);
        c.on_ack(now, 1200, false);
        assert_eq!(c.window(), w0 + 1200);
        c.on_sent(1200);
        c.on_congestion(now, now + Duration::from_millis(1));
        assert_eq!(c.window(), (w0 + 1200) / 2);
        // 回復期間中の損失では減らさない
        c.on_congestion(now, now + Duration::from_millis(2));
        assert_eq!(c.window(), (w0 + 1200) / 2);
        c.on_persistent_congestion();
        assert_eq!(c.window(), 2400);
    }

    #[test]
    fn rtt_estimator() {
        let mut r = Rtt::new(Duration::from_millis(333));
        r.update(Duration::from_millis(100), Duration::ZERO, false, Duration::from_millis(25));
        assert_eq!(r.smoothed(), Duration::from_millis(100));
        r.update(Duration::from_millis(200), Duration::from_millis(50), true, Duration::from_millis(25));
        // ack_delay は max_ack_delay に丸めて引く: 175
        assert_eq!(r.smoothed(), (Duration::from_millis(100) * 7 + Duration::from_millis(175)) / 8);
    }

    #[test]
    fn pacer_bursts_then_waits() {
        let now = Instant::now();
        let mut p = Pacer::new();
        let srtt = Duration::from_millis(100);
        for _ in 0..10 {
            assert!(p.delay(now, srtt, 12000, 1200).is_none());
            p.on_sent(1200);
        }
        assert!(p.delay(now, srtt, 12000, 1200).is_some());
    }
}
