//! 受信順序の復元（並べ替え）と期限つきの待ち合わせ（ジッターバッファ）。
//!
//! 順番どおりのパケットはすぐに出す。抜けがあるときは、抜けの後ろのパケットが届いてから `latency` だけ待ち、
//! それでも埋まらなければ抜けを損失として飛ばす（再生期限を超えて待たない）。待っている間の抜けは
//! [`JitterBuffer::missing`] で NACK の候補として取り出せる。

use crate::packet::RtpPacket;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// [`JitterBuffer::push`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushResult {
    /// 受け付けた。
    Accepted,
    /// 同じ番号がまだ溜まっている。
    Duplicate,
    /// すでに出した、または損失として飛ばした番号（遅すぎる・重複）。
    TooLate,
    /// 番号が大きく飛んだので作り直した（直前に不連続）。
    Reset,
}

/// 取り出したもの。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JitterOutput {
    Packet(RtpPacket),
    /// `first` から `count` 個を損失として飛ばした。
    Lost {
        first: u16,
        count: u32,
    },
    /// 番号が大きく飛んだ（送信元の再始動など）。この後は新しい列。
    Reset,
}

/// 番号の差がこれより大きい飛びは作り直す。
const MAX_JUMP: i64 = 3000;

/// ジッターバッファ。
#[derive(Debug)]
pub struct JitterBuffer {
    latency: Duration,
    capacity: usize,
    next: Option<i64>,
    highest: Option<i64>,
    buf: BTreeMap<i64, (RtpPacket, Instant)>,
    pending_reset: bool,
    lost: u64,
    late: u64,
    duplicates: u64,
}

impl JitterBuffer {
    /// `latency` は抜けを待つ最長の時間、`capacity` は溜める最大のパケット数。
    pub fn new(latency: Duration, capacity: usize) -> Self {
        JitterBuffer {
            latency,
            capacity: capacity.clamp(1, 1 << 15),
            next: None,
            highest: None,
            buf: BTreeMap::new(),
            pending_reset: false,
            lost: 0,
            late: 0,
            duplicates: 0,
        }
    }

    fn extend(&self, seq: u16) -> i64 {
        match self.highest {
            None => seq as i64 + (1 << 16),
            Some(h) => h + seq.wrapping_sub(h as u16) as i16 as i64,
        }
    }

    /// パケットを入れる。
    pub fn push(&mut self, pkt: RtpPacket, now: Instant) -> PushResult {
        let ext = self.extend(pkt.header.sequence);
        let mut result = PushResult::Accepted;
        if let Some(next) = self.next {
            if ext < next {
                if next - ext > MAX_JUMP {
                    self.reset(ext);
                    result = PushResult::Reset;
                } else {
                    self.late += 1;
                    return PushResult::TooLate;
                }
            } else if ext - next > MAX_JUMP {
                self.reset(ext);
                result = PushResult::Reset;
            }
        } else {
            self.next = Some(ext);
        }
        if self.buf.contains_key(&ext) {
            self.duplicates += 1;
            return PushResult::Duplicate;
        }
        if self.highest.is_none_or(|h| ext > h) {
            self.highest = Some(ext);
        }
        self.buf.insert(ext, (pkt, now));
        result
    }

    fn reset(&mut self, ext: i64) {
        self.lost += self.buf.len() as u64;
        self.buf.clear();
        self.next = Some(ext);
        self.highest = Some(ext);
        self.pending_reset = true;
    }

    /// 出せるものを 1 つ取り出す。
    pub fn pop(&mut self, now: Instant) -> Option<JitterOutput> {
        if self.pending_reset {
            self.pending_reset = false;
            return Some(JitterOutput::Reset);
        }
        let next = self.next?;
        if let Some((pkt, _)) = self.buf.remove(&next) {
            self.next = Some(next + 1);
            return Some(JitterOutput::Packet(pkt));
        }
        let (&first, &(_, arrived)) = self.buf.iter().next()?;
        let expired = now.saturating_duration_since(arrived) >= self.latency;
        if expired || self.buf.len() >= self.capacity {
            let count = (first - next) as u32;
            self.lost += count as u64;
            self.next = Some(first);
            return Some(JitterOutput::Lost {
                first: next as u16,
                count,
            });
        }
        None
    }

    /// 次に [`pop`](Self::pop) を呼ぶべき時刻（抜けを待っているときの期限）。
    pub fn poll_timeout(&self) -> Option<Instant> {
        let next = self.next?;
        if self.buf.contains_key(&next) {
            return None;
        }
        self.buf.values().next().map(|(_, t)| *t + self.latency)
    }

    /// 待っている抜けの番号（NACK の候補）。最大 `limit` 個。
    pub fn missing(&self, limit: usize) -> Vec<u16> {
        let (Some(next), Some(high)) = (self.next, self.highest) else {
            return Vec::new();
        };
        (next..high)
            .filter(|s| !self.buf.contains_key(s))
            .take(limit)
            .map(|s| s as u16)
            .collect()
    }

    /// 溜まっているパケットの数。
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// (損失として飛ばした数, 遅れて捨てた数, 重複の数)。
    pub fn counters(&self) -> (u64, u64, u64) {
        (self.lost, self.late, self.duplicates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn p(seq: u16) -> RtpPacket {
        RtpPacket::new(96, seq, 0, 1, false, Bytes::new())
    }

    fn seqs(jb: &mut JitterBuffer, now: Instant) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(o) = jb.pop(now) {
            out.push(match o {
                JitterOutput::Packet(p) => p.header.sequence.to_string(),
                JitterOutput::Lost { first, count } => format!("L{first}+{count}"),
                JitterOutput::Reset => "R".into(),
            });
        }
        out
    }

    #[test]
    fn reorders_within_deadline() {
        let t = Instant::now();
        let mut jb = JitterBuffer::new(Duration::from_millis(50), 100);
        jb.push(p(10), t);
        jb.push(p(12), t);
        jb.push(p(13), t);
        assert_eq!(seqs(&mut jb, t), vec!["10"]);
        assert_eq!(jb.missing(10), vec![11]);
        assert_eq!(jb.poll_timeout(), Some(t + Duration::from_millis(50)));
        jb.push(p(11), t + Duration::from_millis(10));
        assert_eq!(
            seqs(&mut jb, t + Duration::from_millis(10)),
            vec!["11", "12", "13"]
        );
        assert_eq!(jb.push(p(12), t), PushResult::TooLate);
        jb.push(p(15), t);
        assert_eq!(jb.push(p(15), t), PushResult::Duplicate);
    }

    #[test]
    fn skips_after_deadline_and_drops_late() {
        let t = Instant::now();
        let mut jb = JitterBuffer::new(Duration::from_millis(50), 100);
        jb.push(p(65534), t);
        jb.push(p(1), t);
        assert_eq!(seqs(&mut jb, t), vec!["65534"]);
        assert!(seqs(&mut jb, t + Duration::from_millis(49)).is_empty());
        assert_eq!(
            seqs(&mut jb, t + Duration::from_millis(50)),
            vec!["L65535+2", "1"]
        );
        assert_eq!(jb.push(p(0), t), PushResult::TooLate);
        assert_eq!(jb.counters().0, 2);
    }

    #[test]
    fn capacity_forces_skip_and_reset_on_jump() {
        let t = Instant::now();
        let mut jb = JitterBuffer::new(Duration::from_secs(10), 3);
        jb.push(p(0), t);
        for s in 2..5 {
            jb.push(p(s), t);
        }
        assert_eq!(seqs(&mut jb, t), vec!["0", "L1+1", "2", "3", "4"]);
        assert_eq!(jb.push(p(40000), t), PushResult::Reset);
        assert_eq!(seqs(&mut jb, t), vec!["R", "40000"]);
    }

    #[test]
    fn fuzz_order() {
        let t = Instant::now();
        let mut jb = JitterBuffer::new(Duration::from_millis(1), 64);
        let mut seed = 7u32;
        let mut out = 0;
        for i in 0..5000u32 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let s = (i as u16).wrapping_add((seed % 7) as u16).wrapping_sub(3);
            jb.push(p(s), t + Duration::from_millis(i as u64));
            while let Some(o) = jb.pop(t + Duration::from_millis(i as u64)) {
                if let JitterOutput::Packet(_) = o {
                    out += 1;
                }
            }
            assert!(jb.len() <= 64);
        }
        assert!(out > 1000);
    }
}
