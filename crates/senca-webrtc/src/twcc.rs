//! transport-wide congestion control（draft-holmer-rmcat-transport-wide-cc-extensions-01）。
//!
//! - [`TwccRecorder`]: 受けた packet の transport-wide 番号と到着時刻を記録し、feedback（RTCP RTPFB FMT=15）を作る。
//!   ブラウザーの送信側帯域推定（WHIP の上り）はこれで動く。
//! - [`SendHistory`]: 送った packet の番号・時刻・大きさを覚え、相手の feedback と突き合わせる（[`crate::bwe`] へ）。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use senca_rtp::rtcp::TransportCc;

/// 1 つの feedback に載せる packet の数の上限。
const MAX_PER_FEEDBACK: usize = 1000;
/// 覚えておく数の上限。
const MAX_RECORDS: usize = 8192;

fn extend(highest: Option<i64>, seq: u16) -> i64 {
    match highest {
        None => seq as i64 + (1 << 16),
        Some(h) => h + seq.wrapping_sub(h as u16) as i16 as i64,
    }
}

/// 受信側の記録。
#[derive(Debug)]
pub struct TwccRecorder {
    start: Instant,
    highest: Option<i64>,
    /// まだ報告していない最小の番号。
    next: Option<i64>,
    arrivals: BTreeMap<i64, i64>,
    feedback_count: u8,
    media_ssrc: u32,
}

impl TwccRecorder {
    pub fn new(start: Instant) -> Self {
        TwccRecorder {
            start,
            highest: None,
            next: None,
            arrivals: BTreeMap::new(),
            feedback_count: 0,
            media_ssrc: 0,
        }
    }

    /// packet を受けた。
    pub fn on_packet(&mut self, seq: u16, media_ssrc: u32, now: Instant) {
        let ext = extend(self.highest, seq);
        if self.next.is_some_and(|n| ext < n) {
            return; // 既に報告した範囲（遅れて届いた）
        }
        if self.highest.is_none_or(|h| ext > h) {
            self.highest = Some(ext);
        }
        if self.next.is_none() {
            self.next = Some(ext);
        }
        let t = now.saturating_duration_since(self.start).as_micros() as i64;
        self.arrivals.insert(ext, t);
        self.media_ssrc = media_ssrc;
        while self.arrivals.len() > MAX_RECORDS {
            if let Some((k, _)) = self.arrivals.pop_first() {
                self.next = Some(k + 1);
            }
        }
    }

    pub fn has_pending(&self) -> bool {
        !self.arrivals.is_empty()
    }

    /// 報告していない分の feedback を作る（無ければ空）。
    pub fn build_feedback(&mut self, sender_ssrc: u32) -> Vec<TransportCc> {
        let mut out = Vec::new();
        while let (Some(base), Some(_)) = (self.next, self.arrivals.keys().next()) {
            let last = *self.arrivals.keys().next_back().unwrap_or(&base);
            let end = last.min(base + MAX_PER_FEEDBACK as i64 - 1);
            let mut arr = Vec::with_capacity((end - base + 1) as usize);
            let mut prev: Option<i64> = None;
            let mut cut = end;
            for s in base..=end {
                let a = self.arrivals.get(&s).copied();
                if let (Some(p), Some(x)) = (prev, a)
                    && ((x - p) / 250 > i16::MAX as i64 || (x - p) / 250 < i16::MIN as i64)
                {
                    // 差が 16 bit に収まらない: ここで切る
                    cut = s - 1;
                    break;
                }
                if a.is_some() {
                    prev = a;
                }
                arr.push(a);
            }
            // 末尾の未着は次に回す（まだ届くかもしれない）
            while arr.last().is_some_and(|a| a.is_none()) {
                arr.pop();
                cut -= 1;
            }
            if arr.is_empty() {
                break;
            }
            if let Ok(fb) = TransportCc::from_arrivals(
                sender_ssrc,
                self.media_ssrc,
                base as u16,
                self.feedback_count,
                &arr,
            ) {
                self.feedback_count = self.feedback_count.wrapping_add(1);
                out.push(fb);
            }
            self.arrivals.retain(|&k, _| k > cut);
            self.next = Some(cut + 1);
            if out.len() >= 8 {
                break;
            }
        }
        out
    }
}

/// 送った packet 1 つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SentPacket {
    pub send_time: Instant,
    pub size: usize,
    /// 帯域を探る packet の組の番号。
    pub probe: Option<u32>,
}

/// feedback と突き合わせた結果 1 つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketResult {
    pub send_time: Instant,
    pub size: usize,
    pub probe: Option<u32>,
    /// 相手の到着時刻（相手の時計、µs）。`None` は未着。
    pub arrival_us: Option<i64>,
}

/// 送信側の記録。
#[derive(Debug, Default)]
pub struct SendHistory {
    next: u16,
    highest: Option<i64>,
    sent: BTreeMap<i64, SentPacket>,
}

impl SendHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// 次の番号を取って記録する。
    pub fn on_sent(&mut self, now: Instant, size: usize) -> u16 {
        self.on_sent_probe(now, size, None)
    }

    /// 探りの組の packet として記録する。
    pub fn on_sent_probe(&mut self, now: Instant, size: usize, probe: Option<u32>) -> u16 {
        let seq = self.next;
        self.next = self.next.wrapping_add(1);
        let ext = extend(self.highest, seq);
        self.highest = Some(ext);
        self.sent.insert(
            ext,
            SentPacket {
                send_time: now,
                size,
                probe,
            },
        );
        while self.sent.len() > MAX_RECORDS {
            self.sent.pop_first();
        }
        seq
    }

    /// feedback を送信記録と突き合わせる（記録に無い番号は除く）。
    pub fn on_feedback(&mut self, fb: &TransportCc) -> Vec<PacketResult> {
        let mut out = Vec::with_capacity(fb.packets.len());
        for (seq, arr) in fb.arrivals() {
            let ext = extend(self.highest, seq);
            if let Some(s) = self.sent.get(&ext) {
                out.push(PacketResult {
                    send_time: s.send_time,
                    size: s.size,
                    probe: s.probe,
                    arrival_us: arr,
                });
            }
        }
        // 報告された範囲より十分古い記録を捨てる
        if let Some((last, _)) = fb.arrivals().last() {
            let ext = extend(self.highest, *last);
            let keep_from = ext - 4096;
            self.sent.retain(|&k, _| k >= keep_from);
        }
        out
    }

    /// 送ってからこれだけ経った記録は捨てる。
    pub fn expire(&mut self, now: Instant, age: Duration) {
        while let Some((_, s)) = self.sent.first_key_value() {
            if now.saturating_duration_since(s.send_time) > age {
                self.sent.pop_first();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use senca_rtp::rtcp::{RtcpPacket, parse_reduced};

    #[test]
    fn recorder_feedback_roundtrip() {
        let t = Instant::now();
        let mut r = TwccRecorder::new(t);
        let mut h = SendHistory::new();
        let mut seqs = Vec::new();
        for i in 0..20u64 {
            seqs.push(h.on_sent(t + Duration::from_millis(i * 5), 1000));
        }
        for (i, s) in seqs.iter().enumerate() {
            if i % 7 == 3 {
                continue; // 失われた
            }
            r.on_packet(*s, 9, t + Duration::from_millis(i as u64 * 5 + 30));
        }
        let fbs = r.build_feedback(1);
        assert_eq!(fbs.len(), 1);
        let bytes = senca_rtp::build_compound(&[RtcpPacket::TransportCc(fbs[0].clone())]).unwrap();
        let parsed = parse_reduced(&bytes).unwrap();
        let RtcpPacket::TransportCc(fb) = &parsed[0] else {
            panic!()
        };
        let res = h.on_feedback(fb);
        assert_eq!(res.len(), 20);
        assert_eq!(res.iter().filter(|p| p.arrival_us.is_none()).count(), 3);
        // 到着の間隔は 5 ms
        let a: Vec<i64> = res.iter().filter_map(|p| p.arrival_us).collect();
        assert_eq!(a[1] - a[0], 5000);
        assert!(r.build_feedback(1).is_empty());
        // 遅れて届いたものは無視
        r.on_packet(seqs[3], 9, t + Duration::from_millis(500));
        assert!(r.build_feedback(1).is_empty());
        r.on_packet(seqs[19].wrapping_add(1), 9, t + Duration::from_millis(600));
        assert_eq!(r.build_feedback(1)[0].packets.len(), 1);
    }

    #[test]
    fn long_gap_is_split() {
        let t = Instant::now();
        let mut r = TwccRecorder::new(t);
        r.on_packet(0, 1, t);
        r.on_packet(1, 1, t + Duration::from_secs(20));
        let fbs = r.build_feedback(1);
        assert_eq!(fbs.len(), 2);
    }

    #[test]
    fn bounded_records() {
        let t = Instant::now();
        let mut r = TwccRecorder::new(t);
        for i in 0..20000u32 {
            r.on_packet(i as u16, 1, t + Duration::from_micros(i as u64 * 100));
        }
        assert!(r.arrivals.len() <= MAX_RECORDS);
        let mut total = 0;
        loop {
            let f = r.build_feedback(1);
            if f.is_empty() {
                break;
            }
            total += f.iter().map(|x| x.packets.len()).sum::<usize>();
        }
        assert_eq!(total, MAX_RECORDS);
    }
}
