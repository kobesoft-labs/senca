//! 2 つの端点をメモリ上の回線でつなぐ試験（決まった種の xorshift で損失・順序入れ替え・重複を起こす）。

mod assoc;
mod dc;
mod fuzz;

use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::{Association, DataChannelEvent, DataChannels, SctpError, SctpEvent};

/// 決まった種の乱数（xorshift64*）。
pub(crate) struct XorShift(u64);

impl XorShift {
    pub fn new(seed: u64) -> Self {
        XorShift(seed.max(1))
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        // 下位 bit は質が悪いので上位 bit を使う。
        if n == 0 { 0 } else { (self.next() >> 32) % n }
    }
    pub fn chance(&mut self, pct: u32) -> bool {
        self.below(100) < pct as u64
    }
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next() >> 56) as u8).collect()
    }
}

pub(crate) trait Endpoint {
    type Event;
    fn ep_handle_packet(&mut self, now: Instant, p: &[u8]) -> Result<(), SctpError>;
    fn ep_poll_transmit(&mut self) -> Option<Bytes>;
    fn ep_poll_timeout(&self) -> Option<Instant>;
    fn ep_handle_timeout(&mut self, now: Instant);
    fn ep_poll_event(&mut self) -> Option<Self::Event>;
}

impl Endpoint for Association {
    type Event = SctpEvent;
    fn ep_handle_packet(&mut self, now: Instant, p: &[u8]) -> Result<(), SctpError> {
        let r = self.handle_packet(now, p);
        self.check_invariants();
        r
    }
    fn ep_poll_transmit(&mut self) -> Option<Bytes> {
        let r = self.poll_transmit();
        self.check_invariants();
        r
    }
    fn ep_poll_timeout(&self) -> Option<Instant> {
        self.poll_timeout()
    }
    fn ep_handle_timeout(&mut self, now: Instant) {
        self.handle_timeout(now);
        self.check_invariants();
    }
    fn ep_poll_event(&mut self) -> Option<SctpEvent> {
        self.poll_event()
    }
}

impl Endpoint for DataChannels {
    type Event = DataChannelEvent;
    fn ep_handle_packet(&mut self, now: Instant, p: &[u8]) -> Result<(), SctpError> {
        self.handle_packet(now, p)
    }
    fn ep_poll_transmit(&mut self) -> Option<Bytes> {
        self.poll_transmit()
    }
    fn ep_poll_timeout(&self) -> Option<Instant> {
        self.poll_timeout()
    }
    fn ep_handle_timeout(&mut self, now: Instant) {
        self.handle_timeout(now)
    }
    fn ep_poll_event(&mut self) -> Option<DataChannelEvent> {
        self.poll_event()
    }
}

/// 回線の性質。
#[derive(Clone, Copy, Debug)]
pub(crate) struct Link {
    pub loss_pct: u32,
    pub dup_pct: u32,
    pub reorder_pct: u32,
    pub delay: Duration,
}

impl Default for Link {
    fn default() -> Self {
        Link {
            loss_pct: 0,
            dup_pct: 0,
            reorder_pct: 0,
            delay: Duration::from_millis(20),
        }
    }
}

impl Link {
    pub fn lossy(loss: u32, dup: u32, reorder: u32) -> Link {
        Link {
            loss_pct: loss,
            dup_pct: dup,
            reorder_pct: reorder,
            ..Link::default()
        }
    }
}

pub(crate) struct Sim<A: Endpoint, B: Endpoint> {
    pub a: A,
    pub b: B,
    pub now: Instant,
    pub rng: XorShift,
    pub link: Link,
    /// 回線を切る（a→b、b→a）。
    pub block: [bool; 2],
    /// 送られたパケット（a→b なら true）。
    pub sent: Vec<(bool, Bytes)>,
    pub record: bool,
    /// 出来事を取り出さない（受け手のアプリが遅い場合）。
    pub hold_events: [bool; 2],
    pub a_events: Vec<A::Event>,
    pub b_events: Vec<B::Event>,
    pub errors: usize,
    queue: Vec<(Instant, u64, bool, Bytes)>,
    seq: u64,
}

impl<A: Endpoint, B: Endpoint> Sim<A, B> {
    pub fn new(a: A, b: B, now: Instant, seed: u64, link: Link) -> Self {
        Sim {
            a,
            b,
            now,
            rng: XorShift::new(seed),
            link,
            block: [false, false],
            sent: Vec::new(),
            record: false,
            hold_events: [false, false],
            a_events: Vec::new(),
            b_events: Vec::new(),
            errors: 0,
            queue: Vec::new(),
            seq: 0,
        }
    }

    fn enqueue(&mut self, to_b: bool, p: Bytes) {
        if self.record {
            self.sent.push((to_b, p.clone()));
        }
        if self.block[if to_b { 0 } else { 1 }] {
            return;
        }
        if self.rng.chance(self.link.loss_pct) {
            return;
        }
        let copies = if self.rng.chance(self.link.dup_pct) {
            2
        } else {
            1
        };
        for _ in 0..copies {
            let mut d = self.link.delay;
            if self.rng.chance(self.link.reorder_pct) {
                d += Duration::from_millis(1 + self.rng.below(40));
            }
            self.seq += 1;
            self.queue.push((self.now + d, self.seq, to_b, p.clone()));
        }
    }

    pub fn flush(&mut self) {
        loop {
            let mut any = false;
            while let Some(p) = self.a.ep_poll_transmit() {
                self.enqueue(true, p);
                any = true;
            }
            while let Some(p) = self.b.ep_poll_transmit() {
                self.enqueue(false, p);
                any = true;
            }
            while !self.hold_events[0]
                && let Some(e) = self.a.ep_poll_event()
            {
                self.a_events.push(e);
                any = true;
            }
            while !self.hold_events[1]
                && let Some(e) = self.b.ep_poll_event()
            {
                self.b_events.push(e);
                any = true;
            }
            if !any {
                break;
            }
        }
    }

    /// 次の出来事まで進める。何もなければ偽。
    pub fn step(&mut self) -> bool {
        self.flush();
        let next_pkt = self.queue.iter().map(|q| q.0).min();
        let next = [next_pkt, self.a.ep_poll_timeout(), self.b.ep_poll_timeout()]
            .into_iter()
            .flatten()
            .min();
        let Some(t) = next else {
            return false;
        };
        if t > self.now {
            self.now = t;
        }
        let now = self.now;
        self.queue.sort_by_key(|q| (q.0, q.1));
        let due: Vec<_> = {
            let n = self.queue.iter().take_while(|q| q.0 <= now).count();
            self.queue.drain(..n).collect()
        };
        for (_, _, to_b, p) in due {
            let r = if to_b {
                self.b.ep_handle_packet(now, &p)
            } else {
                self.a.ep_handle_packet(now, &p)
            };
            if r.is_err() {
                self.errors += 1;
            }
        }
        if self.a.ep_poll_timeout().is_some_and(|t| t <= now) {
            self.a.ep_handle_timeout(now);
        }
        if self.b.ep_poll_timeout().is_some_and(|t| t <= now) {
            self.b.ep_handle_timeout(now);
        }
        self.flush();
        true
    }

    /// 条件が満たされるまで（または `limit` の時間まで）進める。
    pub fn run_until(&mut self, limit: Duration, mut cond: impl FnMut(&Self) -> bool) -> bool {
        let end = self.now + limit;
        let mut steps = 0u64;
        loop {
            steps += 1;
            assert!(steps < 3_000_000, "no progress (time stuck at a timer?)");
            self.flush();
            if cond(self) {
                return true;
            }
            if self.now > end {
                return false;
            }
            if !self.step() {
                return cond(self);
            }
        }
    }

    /// `d` だけ時間を進める（その間の出来事を処理する）。
    pub fn run_for(&mut self, d: Duration) {
        let end = self.now + d;
        let mut steps = 0u64;
        loop {
            steps += 1;
            assert!(steps < 3_000_000, "no progress (time stuck at a timer?)");
            self.flush();
            let next_pkt = self.queue.iter().map(|q| q.0).min();
            let next = [next_pkt, self.a.ep_poll_timeout(), self.b.ep_poll_timeout()]
                .into_iter()
                .flatten()
                .min();
            match next {
                Some(t) if t <= end => {
                    self.step();
                }
                _ => {
                    self.now = end;
                    return;
                }
            }
        }
    }
}
