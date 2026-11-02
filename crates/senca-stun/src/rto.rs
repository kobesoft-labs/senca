//! 要求の再送タイマー（RFC 8489 6.2.1・6.2.2）と RTO の推定（RFC 6298）。

use std::time::{Duration, Instant};

/// 再送の設定。
#[derive(Debug, Clone, Copy)]
pub struct RtoConfig {
    /// 最初の RTO（既定 500 ms）。
    pub initial_rto: Duration,
    /// RTO の上限（倍々で伸ばすときの頭打ち）。
    pub max_rto: Duration,
    /// 送る回数の上限 Rc（既定 7）。
    pub max_transmits: u32,
    /// 最後に送ってから待つ RTO の倍数 Rm（既定 16）。
    pub rm: u32,
    /// TCP など信頼できる輸送なら再送せず、`reliable_timeout`（Ti、既定 39.5 秒）だけ待つ。
    pub reliable: bool,
    pub reliable_timeout: Duration,
}

impl Default for RtoConfig {
    fn default() -> Self {
        RtoConfig {
            initial_rto: Duration::from_millis(500),
            max_rto: Duration::from_secs(60),
            max_transmits: 7,
            rm: 16,
            reliable: false,
            reliable_timeout: Duration::from_millis(39_500),
        }
    }
}

/// `handle_timeout` の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetransmitAction {
    /// まだ時間ではない。
    Wait,
    /// いま再送する。
    Retransmit,
    /// 応答が来ないまま終わった。
    TimedOut,
}

/// 1 つの transaction の再送状態。最初の送信は作った時点で済んでいるものとする。
#[derive(Debug, Clone)]
pub struct Retransmission {
    cfg: RtoConfig,
    rto: Duration,
    transmits: u32,
    deadline: Instant,
    done: bool,
}

impl Retransmission {
    /// `now` に最初の送信をした。`rto` は推定値（`None` なら `cfg.initial_rto`）。
    pub fn new(now: Instant, cfg: RtoConfig, rto: Option<Duration>) -> Self {
        let rto = rto.unwrap_or(cfg.initial_rto).min(cfg.max_rto);
        let deadline = if cfg.reliable {
            now + cfg.reliable_timeout
        } else if cfg.max_transmits <= 1 {
            now + rto * cfg.rm
        } else {
            now + rto
        };
        Retransmission {
            cfg,
            rto,
            transmits: 1,
            deadline,
            done: false,
        }
    }

    /// 次に `handle_timeout` を呼ぶべき時刻。
    pub fn poll_timeout(&self) -> Option<Instant> {
        if self.done { None } else { Some(self.deadline) }
    }

    /// 送った回数。
    pub fn transmits(&self) -> u32 {
        self.transmits
    }

    pub fn handle_timeout(&mut self, now: Instant) -> RetransmitAction {
        if self.done {
            return RetransmitAction::TimedOut;
        }
        if now < self.deadline {
            return RetransmitAction::Wait;
        }
        if self.cfg.reliable || self.transmits >= self.cfg.max_transmits {
            self.done = true;
            return RetransmitAction::TimedOut;
        }
        self.transmits += 1;
        self.rto = (self.rto * 2).min(self.cfg.max_rto);
        if self.transmits >= self.cfg.max_transmits {
            // 最後の送信の後は Rm × 最初の RTO だけ待つ（RFC 8489 6.2.1 の例: 39.5 秒）。
            let base = self.cfg.initial_rto.min(self.cfg.max_rto);
            self.deadline = now + base * self.cfg.rm;
        } else {
            self.deadline = now + self.rto;
        }
        RetransmitAction::Retransmit
    }
}

/// RFC 6298 の RTO 推定（SRTT・RTTVAR）。再送した transaction の RTT は使わないこと（Karn の規則）。
#[derive(Debug, Clone)]
pub struct RttEstimator {
    srtt: Option<Duration>,
    rttvar: Duration,
    min_rto: Duration,
    max_rto: Duration,
    initial: Duration,
}

impl RttEstimator {
    pub fn new(initial: Duration, min_rto: Duration, max_rto: Duration) -> Self {
        RttEstimator {
            srtt: None,
            rttvar: Duration::ZERO,
            min_rto,
            max_rto,
            initial,
        }
    }

    pub fn update(&mut self, rtt: Duration) {
        match self.srtt {
            None => {
                self.srtt = Some(rtt);
                self.rttvar = rtt / 2;
            }
            Some(srtt) => {
                let diff = srtt.abs_diff(rtt);
                self.rttvar = (self.rttvar * 3 + diff) / 4;
                self.srtt = Some((srtt * 7 + rtt) / 8);
            }
        }
    }

    pub fn rto(&self) -> Duration {
        match self.srtt {
            None => self.initial,
            Some(srtt) => (srtt + (self.rttvar * 4).max(Duration::from_millis(1)))
                .clamp(self.min_rto, self.max_rto),
        }
    }
}

impl Default for RttEstimator {
    fn default() -> Self {
        RttEstimator::new(
            Duration::from_millis(500),
            Duration::from_millis(250),
            Duration::from_secs(60),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc8489_schedule() {
        // RTO 500 ms・Rc 7・Rm 16: 0, 500, 1500, 3500, 7500, 15500, 31500 ms に送り、39500 ms で諦める。
        let t0 = Instant::now();
        let mut r = Retransmission::new(t0, RtoConfig::default(), None);
        let mut sends = vec![0u64];
        let mut ms = 0u64;
        loop {
            ms += 50;
            match r.handle_timeout(t0 + Duration::from_millis(ms)) {
                RetransmitAction::Wait => {}
                RetransmitAction::Retransmit => sends.push(ms),
                RetransmitAction::TimedOut => break,
            }
        }
        assert_eq!(sends, vec![0, 500, 1500, 3500, 7500, 15500, 31500]);
        assert_eq!(ms, 39500);
        assert_eq!(r.poll_timeout(), None);
    }

    #[test]
    fn reliable_does_not_retransmit() {
        let t0 = Instant::now();
        let cfg = RtoConfig {
            reliable: true,
            ..Default::default()
        };
        let mut r = Retransmission::new(t0, cfg, None);
        assert_eq!(
            r.handle_timeout(t0 + Duration::from_secs(39)),
            RetransmitAction::Wait
        );
        assert_eq!(
            r.handle_timeout(t0 + Duration::from_millis(39_500)),
            RetransmitAction::TimedOut
        );
    }

    #[test]
    fn estimator() {
        let mut e = RttEstimator::default();
        assert_eq!(e.rto(), Duration::from_millis(500));
        e.update(Duration::from_millis(100));
        assert_eq!(e.rto(), Duration::from_millis(300));
        for _ in 0..50 {
            e.update(Duration::from_millis(100));
        }
        assert_eq!(e.rto(), Duration::from_millis(250));
    }
}
