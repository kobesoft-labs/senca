//! 受信側の NACK の生成（RFC 4585 の Generic NACK、仕様 6.2）。
//!
//! 番号の抜けを見つけたら覚え、往復時間に応じた間隔で求め直す。次の場合は求めない・諦める:
//!
//! - 再生期限（ジッターバッファが抜けを飛ばす時刻）までに再送が間に合わない（残り < 往復時間）
//! - 回数の上限に達した
//! - 1 秒あたりの数の上限に達した（次の機会に回す）
//! - 覚えている数の上限を超えた（古いものから諦め、[`NackGenerator::take_keyframe_needed`] が真になる）

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::config::NackConfig;

#[derive(Debug, Clone, Copy)]
struct Entry {
    detected: Instant,
    deadline: Instant,
    last_sent: Option<Instant>,
    retries: u32,
}

/// 1 つのストリームの NACK の生成器。番号は 16 bit を広げた値で扱う。
#[derive(Debug)]
pub struct NackGenerator {
    cfg: NackConfig,
    highest: Option<i64>,
    missing: BTreeMap<i64, Entry>,
    window_start: Option<Instant>,
    window_count: u32,
    keyframe_needed: bool,
    /// 求めた番号ごとの最後に送った時刻（再送が届いたら往復時間の見積もりに使う）。
    sent_at: BTreeMap<i64, Instant>,
    requested: u64,
    given_up: u64,
}

impl NackGenerator {
    pub fn new(cfg: NackConfig) -> Self {
        NackGenerator {
            cfg,
            highest: None,
            missing: BTreeMap::new(),
            window_start: None,
            window_count: 0,
            keyframe_needed: false,
            sent_at: BTreeMap::new(),
            requested: 0,
            given_up: 0,
        }
    }

    fn extend(&self, seq: u16) -> i64 {
        match self.highest {
            None => seq as i64 + (1 << 16),
            Some(h) => h + seq.wrapping_sub(h as u16) as i16 as i64,
        }
    }

    /// packet を受けた（再送を含む）。`deadline` はこの時点で見つかった抜けの再生期限。
    /// 再送で埋まった番号なら、その番号を求めてからの時間（往復時間の見本）を返す。
    pub fn on_packet(&mut self, seq: u16, now: Instant, deadline: Instant) -> Option<Duration> {
        let ext = self.extend(seq);
        let Some(h) = self.highest else {
            self.highest = Some(ext);
            return None;
        };
        if ext > h {
            let gap = ext - h - 1;
            if gap as usize > self.cfg.max_missing {
                // 大きな飛び（送信元の再始動など）: 覚えていた抜けを捨てて作り直す
                self.given_up += self.missing.len() as u64;
                self.missing.clear();
                self.sent_at.clear();
                if gap < 3000 {
                    self.keyframe_needed = true;
                }
            } else {
                for s in h + 1..ext {
                    self.missing.insert(
                        s,
                        Entry {
                            detected: now,
                            deadline,
                            last_sent: None,
                            retries: 0,
                        },
                    );
                }
                while self.missing.len() > self.cfg.max_missing {
                    self.missing.pop_first();
                    self.given_up += 1;
                    self.keyframe_needed = true;
                }
            }
            self.highest = Some(ext);
            // 古い送信記録は捨てる
            while self.sent_at.len() > self.cfg.max_missing {
                self.sent_at.pop_first();
            }
            None
        } else {
            self.missing.remove(&ext);
            self.sent_at
                .remove(&ext)
                .map(|t| now.saturating_duration_since(t))
        }
    }

    /// 今送る NACK の番号。`rtt` は往復時間の見積もり。
    pub fn poll(&mut self, now: Instant, rtt: Duration) -> Vec<u16> {
        if self
            .window_start
            .is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_secs(1))
        {
            self.window_start = Some(now);
            self.window_count = 0;
        }
        let interval = self.cfg.min_retry_interval.max(rtt + rtt / 2);
        let mut out = Vec::new();
        let mut drop = Vec::new();
        for (&s, e) in self.missing.iter_mut() {
            // 再生期限までに届かないなら求めない（仕様 6.2）
            if e.deadline.saturating_duration_since(now) <= rtt / 2
                || e.retries >= self.cfg.max_retries
            {
                drop.push(s);
                continue;
            }
            if e.last_sent
                .is_some_and(|t| now.saturating_duration_since(t) < interval)
            {
                continue;
            }
            if self.window_count >= self.cfg.max_per_second {
                break;
            }
            e.last_sent = Some(now);
            e.retries += 1;
            self.window_count += 1;
            self.sent_at.insert(s, now);
            out.push(s as u16);
        }
        for s in drop {
            self.missing.remove(&s);
            self.given_up += 1;
        }
        self.requested += out.len() as u64;
        out
    }

    /// 次に [`poll`](Self::poll) を呼ぶ時刻。
    pub fn poll_timeout(&self, rtt: Duration) -> Option<Instant> {
        let interval = self.cfg.min_retry_interval.max(rtt + rtt / 2);
        let exhausted = self
            .window_start
            .filter(|_| self.window_count >= self.cfg.max_per_second)
            .map(|w| w + Duration::from_secs(1));
        self.missing
            .values()
            .map(|e| {
                let t = match e.last_sent {
                    // まだ求めていない抜けはすぐ（見つけた時刻）
                    None => e.detected,
                    Some(t) => (t + interval).min(e.deadline),
                };
                exhausted.map_or(t, |x| t.max(x))
            })
            .min()
    }

    /// 抜けを諦めてキーフレームが要るようになったら真（一度だけ）。
    pub fn take_keyframe_needed(&mut self) -> bool {
        std::mem::take(&mut self.keyframe_needed)
    }

    pub fn missing_count(&self) -> usize {
        self.missing.len()
    }

    /// (求めた番号の延べ数, 諦めた数)。
    pub fn counters(&self) -> (u64, u64) {
        (self.requested, self.given_up)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_retries_with_deadline() {
        let t = Instant::now();
        let mut n = NackGenerator::new(NackConfig::default());
        let dl = t + Duration::from_millis(200);
        n.on_packet(100, t, dl);
        n.on_packet(103, t, dl);
        let rtt = Duration::from_millis(20);
        assert_eq!(n.poll(t, rtt), vec![101, 102]);
        // 間隔内は送らない
        assert!(n.poll(t + Duration::from_millis(10), rtt).is_empty());
        let s = n.on_packet(101, t + Duration::from_millis(25), dl);
        assert_eq!(s, Some(Duration::from_millis(25)));
        assert_eq!(n.poll(t + Duration::from_millis(31), rtt), vec![102]);
        // 期限が近い（残り <= rtt/2）なら諦める
        assert!(n.poll(t + Duration::from_millis(195), rtt).is_empty());
        assert_eq!(n.missing_count(), 0);
    }

    #[test]
    fn rtt_beyond_deadline_never_requests() {
        let t = Instant::now();
        let mut n = NackGenerator::new(NackConfig::default());
        n.on_packet(1, t, t + Duration::from_millis(50));
        n.on_packet(5, t, t + Duration::from_millis(50));
        assert!(n.poll(t, Duration::from_millis(200)).is_empty());
    }

    #[test]
    fn bounded_and_rate_limited() {
        let t = Instant::now();
        let cfg = NackConfig {
            max_missing: 10,
            max_per_second: 5,
            ..Default::default()
        };
        let mut n = NackGenerator::new(cfg);
        let dl = t + Duration::from_secs(10);
        n.on_packet(0, t, dl);
        n.on_packet(9, t, dl);
        assert_eq!(n.poll(t, Duration::ZERO).len(), 5);
        assert!(
            n.poll(t + Duration::from_millis(30), Duration::ZERO)
                .is_empty()
        );
        assert_eq!(n.poll(t + Duration::from_secs(1), Duration::ZERO).len(), 5);
        // 上限を超える抜けは古いものから諦める
        n.on_packet(20, t, dl + Duration::from_secs(5));
        assert!(n.missing_count() <= 10);
        assert!(n.take_keyframe_needed());
        assert!(!n.take_keyframe_needed());
        // 折り返し
        let mut n = NackGenerator::new(NackConfig::default());
        n.on_packet(65534, t, dl);
        n.on_packet(1, t, dl);
        assert_eq!(n.poll(t, Duration::ZERO), vec![65535, 0]);
    }
}
