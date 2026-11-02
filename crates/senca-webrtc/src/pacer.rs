//! 送出の平準化（leaky bucket）。
//!
//! 映像のキーフレームのような塊を、速さ `rate`（既定は推定帯域 × 2.5）で少しずつ出す。
//! 優先順は 音声 → 再送 → 映像。予算は `burst` の分まで貯められる。
//! 待ち行列は packet 数と待ち時間で抑え、溢れた映像 packet は捨てて呼び出し側に返す（キーフレームを求める合図）。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::config::PacerConfig;

/// packet の種類（優先順）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketClass {
    Audio,
    Retransmission,
    Video,
}

#[derive(Debug)]
struct Item<T> {
    item: T,
    size: usize,
    enqueued: Instant,
}

/// pacer。
#[derive(Debug)]
pub struct Pacer<T> {
    cfg: PacerConfig,
    queues: [VecDeque<Item<T>>; 3],
    /// 使える byte 数（負なら借り）。
    budget: f64,
    last: Option<Instant>,
    rate_bps: u64,
}

fn idx(c: PacketClass) -> usize {
    match c {
        PacketClass::Audio => 0,
        PacketClass::Retransmission => 1,
        PacketClass::Video => 2,
    }
}

impl<T> Pacer<T> {
    pub fn new(cfg: PacerConfig, rate_bps: u64) -> Self {
        let rate = cfg
            .fixed_rate_bps
            .unwrap_or(rate_bps)
            .max(cfg.min_rate_bps)
            .max(1);
        Pacer {
            cfg,
            queues: [VecDeque::new(), VecDeque::new(), VecDeque::new()],
            budget: 0.0,
            last: None,
            rate_bps: rate,
        }
    }

    /// 推定帯域を知らせる（固定の速さなら無視）。
    pub fn set_estimate(&mut self, bps: u64) {
        if self.cfg.fixed_rate_bps.is_none() {
            self.rate_bps = ((bps as f64 * self.cfg.multiplier) as u64)
                .max(self.cfg.min_rate_bps)
                .max(1);
        }
    }

    pub fn rate_bps(&self) -> u64 {
        self.rate_bps
    }

    pub fn len(&self) -> usize {
        self.queues.iter().map(VecDeque::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.queues.iter().all(VecDeque::is_empty)
    }

    /// 待っている byte 数。
    pub fn queued_bytes(&self) -> usize {
        self.queues.iter().flatten().map(|i| i.size).sum()
    }

    fn refill(&mut self, now: Instant) {
        let last = *self.last.get_or_insert(now);
        let dt = now.saturating_duration_since(last).as_secs_f64();
        self.last = Some(now.max(last));
        let max = self.rate_bps as f64 / 8.0 * self.cfg.burst.as_secs_f64();
        self.budget = (self.budget + dt * self.rate_bps as f64 / 8.0).min(max.max(1500.0));
    }

    /// 入れる。溢れて捨てた映像 packet を返す。
    pub fn push(&mut self, now: Instant, class: PacketClass, item: T, size: usize) -> Vec<T> {
        let mut dropped = Vec::new();
        self.queues[idx(class)].push_back(Item {
            item,
            size,
            enqueued: now,
        });
        while self.len() > self.cfg.max_queue_packets {
            let q = if !self.queues[2].is_empty() {
                2
            } else if !self.queues[1].is_empty() {
                1
            } else {
                0
            };
            if let Some(i) = self.queues[q].pop_front() {
                dropped.push(i.item);
            }
        }
        dropped
    }

    /// 待ち時間の上限を超えた映像・再送の packet を捨てて返す。
    pub fn expire(&mut self, now: Instant) -> Vec<T> {
        let mut out = Vec::new();
        for q in [1usize, 2] {
            while self.queues[q].front().is_some_and(|i| {
                now.saturating_duration_since(i.enqueued) > self.cfg.max_queue_delay
            }) {
                if let Some(i) = self.queues[q].pop_front() {
                    out.push(i.item);
                }
            }
        }
        out
    }

    /// 今出してよい packet を 1 つ取り出す。
    pub fn pop(&mut self, now: Instant) -> Option<T> {
        let q = self.queues.iter().position(|q| !q.is_empty())?;
        if !self.cfg.enabled {
            return self.queues[q].pop_front().map(|i| i.item);
        }
        self.refill(now);
        // 音声は予算を借りてでも出す（小さく、遅らせると目立つ）
        if self.budget <= 0.0 && q != 0 {
            return None;
        }
        let i = self.queues[q].pop_front()?;
        self.budget -= i.size as f64;
        Some(i.item)
    }

    /// 次に [`pop`](Self::pop) で出せる時刻。待っていなければ `None`。
    pub fn poll_timeout(&self) -> Option<Instant> {
        if self.is_empty() {
            return None;
        }
        let last = self.last?;
        if !self.cfg.enabled || self.budget > 0.0 || !self.queues[0].is_empty() {
            return Some(last);
        }
        let deficit = -self.budget + 1.0;
        let secs = deficit * 8.0 / self.rate_bps as f64;
        Some(last + Duration::from_secs_f64(secs.min(1.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paces_at_rate_with_priority() {
        let t = Instant::now();
        let cfg = PacerConfig {
            fixed_rate_bps: Some(1_000_000),
            ..Default::default()
        };
        let mut p: Pacer<u32> = Pacer::new(cfg, 0);
        for i in 0..100 {
            p.push(t, PacketClass::Video, i, 1250);
        }
        p.push(t, PacketClass::Audio, 1000, 100);
        p.push(t, PacketClass::Retransmission, 2000, 1250);
        // 100 × 1250 byte = 1 Mbit → 1 秒かかるはず
        let mut now = t;
        let mut out = Vec::new();
        while out.len() < 102 {
            while let Some(x) = p.pop(now) {
                out.push((x, now));
            }
            match p.poll_timeout() {
                Some(n) => now = n.max(now + Duration::from_micros(100)),
                None => break,
            }
        }
        assert_eq!(out[0].0, 1000);
        assert_eq!(out[1].0, 2000);
        let span = out.last().unwrap().1 - t;
        assert!(
            span > Duration::from_millis(900) && span < Duration::from_millis(1100),
            "{span:?}"
        );
    }

    #[test]
    fn bounded_and_expire() {
        let t = Instant::now();
        let cfg = PacerConfig {
            max_queue_packets: 10,
            fixed_rate_bps: Some(100_000),
            ..Default::default()
        };
        let mut p: Pacer<u32> = Pacer::new(cfg, 0);
        let mut dropped = 0;
        for i in 0..20 {
            dropped += p.push(t, PacketClass::Video, i, 1000).len();
        }
        assert_eq!(dropped, 10);
        assert_eq!(p.len(), 10);
        assert_eq!(p.expire(t + Duration::from_secs(3)).len(), 10);
        assert!(p.is_empty());
        // 無効なら全部すぐ出る
        let mut p: Pacer<u32> = Pacer::new(
            PacerConfig {
                enabled: false,
                ..Default::default()
            },
            1,
        );
        for i in 0..5 {
            p.push(t, PacketClass::Video, i, 100_000);
        }
        let mut n = 0;
        while p.pop(t).is_some() {
            n += 1;
        }
        assert_eq!(n, 5);
    }
}
