//! キーフレーム要求（PLI・FIR）の集約と間隔制限（仕様 6.2）。
//!
//! 多数の視聴者からの要求を、送信元（SSRC）ごとに `min_interval` に 1 回までまとめる。
//! 間隔内に来た要求は保留し、間隔が明けたら 1 回だけ出す。

use crate::rtcp::{Fir, Pli, RtcpPacket};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
struct State {
    last_sent: Option<Instant>,
    pending: bool,
    fir_seq: u8,
}

/// 要求の集約器。
#[derive(Debug)]
pub struct KeyframeRequestLimiter {
    min_interval: Duration,
    sender_ssrc: u32,
    use_fir: bool,
    max_sources: usize,
    sources: HashMap<u32, State>,
    suppressed: u64,
}

impl KeyframeRequestLimiter {
    /// `use_fir` が真なら FIR（RFC 5104）、偽なら PLI（RFC 4585）を作る。
    pub fn new(
        sender_ssrc: u32,
        min_interval: Duration,
        use_fir: bool,
        max_sources: usize,
    ) -> Self {
        KeyframeRequestLimiter {
            min_interval,
            sender_ssrc,
            use_fir,
            max_sources,
            sources: HashMap::new(),
            suppressed: 0,
        }
    }

    /// 要求を受ける。今すぐ送るべきなら RTCP パケットを返す（間隔内なら保留して `None`）。
    pub fn request(&mut self, media_ssrc: u32, now: Instant) -> Option<RtcpPacket> {
        if !self.sources.contains_key(&media_ssrc) && self.sources.len() >= self.max_sources {
            self.suppressed += 1;
            return None;
        }
        let st = self.sources.entry(media_ssrc).or_insert(State {
            last_sent: None,
            pending: false,
            fir_seq: 0,
        });
        if st
            .last_sent
            .is_some_and(|t| now.saturating_duration_since(t) < self.min_interval)
        {
            st.pending = true;
            self.suppressed += 1;
            return None;
        }
        Some(Self::emit(
            self.sender_ssrc,
            self.use_fir,
            media_ssrc,
            st,
            now,
        ))
    }

    fn emit(
        sender: u32,
        use_fir: bool,
        media_ssrc: u32,
        st: &mut State,
        now: Instant,
    ) -> RtcpPacket {
        st.last_sent = Some(now);
        st.pending = false;
        if use_fir {
            st.fir_seq = st.fir_seq.wrapping_add(1);
            RtcpPacket::Fir(Fir {
                sender_ssrc: sender,
                entries: vec![(media_ssrc, st.fir_seq)],
            })
        } else {
            RtcpPacket::Pli(Pli {
                sender_ssrc: sender,
                media_ssrc,
            })
        }
    }

    /// 間隔が明けた保留中の要求を出す。
    pub fn poll(&mut self, now: Instant) -> Vec<RtcpPacket> {
        let mut out = Vec::new();
        for (ssrc, st) in self.sources.iter_mut() {
            if st.pending
                && st
                    .last_sent
                    .is_none_or(|t| now.saturating_duration_since(t) >= self.min_interval)
            {
                out.push(Self::emit(self.sender_ssrc, self.use_fir, *ssrc, st, now));
            }
        }
        out
    }

    /// 次に [`poll`](Self::poll) を呼ぶべき時刻。
    pub fn poll_timeout(&self) -> Option<Instant> {
        self.sources
            .values()
            .filter(|s| s.pending)
            .filter_map(|s| s.last_sent.map(|t| t + self.min_interval))
            .min()
    }

    /// 送信元を忘れる（BYE・timeout）。
    pub fn remove(&mut self, media_ssrc: u32) {
        self.sources.remove(&media_ssrc);
    }

    /// まとめて抑えた要求の数。
    pub fn suppressed(&self) -> u64 {
        self.suppressed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_and_rate_limits() {
        let t = Instant::now();
        let mut l = KeyframeRequestLimiter::new(1, Duration::from_millis(500), false, 4);
        assert!(matches!(l.request(7, t), Some(RtcpPacket::Pli(_))));
        // 100 人が続けて要求しても間隔内は出さない
        for i in 0..100 {
            assert!(l.request(7, t + Duration::from_millis(i)).is_none());
        }
        assert!(l.poll(t + Duration::from_millis(499)).is_empty());
        assert_eq!(l.poll_timeout(), Some(t + Duration::from_millis(500)));
        assert_eq!(l.poll(t + Duration::from_millis(500)).len(), 1);
        assert!(l.poll(t + Duration::from_secs(5)).is_empty());
        assert_eq!(l.suppressed(), 100);
    }

    #[test]
    fn fir_sequence_increments() {
        let t = Instant::now();
        let mut l = KeyframeRequestLimiter::new(1, Duration::from_millis(10), true, 1);
        let a = l.request(9, t);
        let b = l.request(9, t + Duration::from_millis(20));
        match (a, b) {
            (Some(RtcpPacket::Fir(a)), Some(RtcpPacket::Fir(b))) => {
                assert_eq!(a.entries[0].1 + 1, b.entries[0].1);
            }
            _ => panic!(),
        }
        // 上限を超える送信元は受けない
        assert!(l.request(10, t).is_none());
    }
}
