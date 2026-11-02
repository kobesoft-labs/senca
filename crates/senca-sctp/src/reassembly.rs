//! 受信側: TSN の記録（累積 TSN・gap・重複）、断片の組み立て、ストリームごとの順序合わせ。
//!
//! TSN は内部では 64 bit に広げて持つ（serial number の比較を 1 か所に閉じこめる）。
//! 持つ量はすべて上限を置く: 組み立て中・順番待ち・未配送のバイト数は受信窓、
//! TSN の数・順番待ちのメッセージ数・重複の記録数は定数で抑える。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bytes::{Bytes, BytesMut};

use crate::chunk::DataChunk;

/// 累積 TSN より先に持っておく TSN の数の上限。
pub(crate) const MAX_RX_TSNS: usize = 16384;
/// 累積 TSN からどこまで先の TSN を受けるか（gap block の位置は 16 bit）。
pub(crate) const MAX_TSN_AHEAD: u64 = 65535;
/// 順番待ちのメッセージ数の上限（全ストリーム合計）。
pub(crate) const MAX_READY_MESSAGES: usize = 16384;
/// SACK に載せる重複 TSN の数の上限。
pub(crate) const MAX_DUP_TSNS: usize = 32;

/// 32 bit の値を、基準 `r`（64 bit）の近くの 64 bit の値に広げる（serial number の比較）。
pub(crate) fn extend(r: u64, v: u32) -> u64 {
    let diff = v.wrapping_sub(r as u32) as i32 as i64;
    (r as i64).wrapping_add(diff) as u64
}

/// 初期値の 64 bit 表現（下に余裕を持たせる）。
pub(crate) fn initial_ext(v: u32) -> u64 {
    (1u64 << 32) + v as u64
}

/// 組み立て済みのメッセージ。
#[derive(Debug, Clone)]
pub(crate) struct RxMessage {
    pub stream: u16,
    pub ppid: u32,
    pub data: Bytes,
    pub unordered: bool,
}

/// DATA の扱いの結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DataOutcome {
    /// 受け取った（新しい TSN）。
    Accepted,
    /// すでに受け取っている。
    Duplicate,
    /// 窓や上限のため捨てた（ack しない。相手が再送する）。
    Dropped,
    /// TSN は受け取ったがストリーム番号が範囲外なので中身を捨てた。
    InvalidStream,
    /// 上限を超えるメッセージ・順番待ちの溢れ（相手の誤り。association を閉じる）。
    Violation,
}

struct Frag {
    stream: u16,
    ssn: u16,
    ppid: u32,
    unordered: bool,
    begin: bool,
    end: bool,
    payload: Bytes,
}

impl Frag {
    fn same_message(&self, o: &Frag) -> bool {
        self.stream == o.stream
            && self.unordered == o.unordered
            && (self.unordered || self.ssn == o.ssn)
    }
}

#[derive(Default)]
struct StreamIn {
    next_ssn: u16,
    /// この TSN までは前の世代（stream reset の前）の SSN。
    epoch_tsn: u64,
    ready: BTreeMap<u16, RxMessage>,
}

pub(crate) struct Receiver {
    /// 累積 TSN（64 bit）。
    pub cum: u64,
    received: BTreeSet<u64>,
    dups: Vec<u32>,
    frags: BTreeMap<u64, Frag>,
    streams: HashMap<u16, StreamIn>,
    /// 全ストリームのリセットの TSN（`StreamIn::epoch_tsn` と同じ意味）。
    all_epoch_tsn: u64,
    ready_count: usize,
    /// 組み立て中 + 順番待ち + 配送したがまだ取り出されていないバイト数。
    pub buffered: usize,
    window: usize,
    max_message: usize,
    in_streams: u16,
}

impl Receiver {
    pub fn new(peer_initial_tsn: u32, window: u32, max_message: usize, in_streams: u16) -> Self {
        Receiver {
            cum: initial_ext(peer_initial_tsn) - 1,
            received: BTreeSet::new(),
            dups: Vec::new(),
            frags: BTreeMap::new(),
            streams: HashMap::new(),
            all_epoch_tsn: 0,
            ready_count: 0,
            buffered: 0,
            window: window as usize,
            max_message,
            in_streams,
        }
    }

    pub fn extend(&self, v: u32) -> u64 {
        extend(self.cum, v)
    }

    /// 広告する受信窓。
    pub fn a_rwnd(&self) -> u32 {
        self.window
            .saturating_sub(self.buffered)
            .min(u32::MAX as usize) as u32
    }

    /// 取り出されたメッセージの分を窓に戻す。
    pub fn release(&mut self, n: usize) {
        self.buffered = self.buffered.saturating_sub(n);
    }

    pub fn has_gaps(&self) -> bool {
        !self.received.is_empty()
    }

    pub fn has_dups(&self) -> bool {
        !self.dups.is_empty()
    }

    pub fn take_dups(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.dups)
    }

    /// gap ack block（累積 TSN からの相対位置）。
    pub fn gap_blocks(&self, max: usize) -> Vec<(u16, u16)> {
        let mut out: Vec<(u16, u16)> = Vec::new();
        let mut run: Option<(u64, u64)> = None;
        for &t in &self.received {
            match run {
                Some((s, e)) if t == e + 1 => run = Some((s, t)),
                Some((s, e)) => {
                    out.push(((s - self.cum) as u16, (e - self.cum) as u16));
                    if out.len() >= max {
                        return out;
                    }
                    run = Some((t, t));
                }
                None => run = Some((t, t)),
            }
        }
        if let Some((s, e)) = run
            && out.len() < max
        {
            out.push(((s - self.cum) as u16, (e - self.cum) as u16));
        }
        out
    }

    fn advance_cum(&mut self) {
        while self.received.remove(&(self.cum + 1)) {
            self.cum += 1;
        }
    }

    /// DATA を受ける。組み立てて配れるメッセージを `out` に足す。
    pub fn on_data(&mut self, d: &DataChunk, out: &mut Vec<RxMessage>) -> DataOutcome {
        let tsn = self.extend(d.tsn);
        if tsn <= self.cum || self.received.contains(&tsn) {
            if self.dups.len() < MAX_DUP_TSNS {
                self.dups.push(d.tsn);
            }
            return DataOutcome::Duplicate;
        }
        if tsn - self.cum > MAX_TSN_AHEAD || self.received.len() >= MAX_RX_TSNS {
            return DataOutcome::Dropped;
        }
        let len = d.payload.len();
        // 次に待っている TSN は窓を少し超えてもとる（組み立ての行き詰まりを防ぐ。上限は 2 倍）。
        let limit = if tsn == self.cum + 1 {
            self.window.saturating_mul(2)
        } else {
            self.window
        };
        if self.buffered.saturating_add(len) > limit {
            return DataOutcome::Dropped;
        }
        self.received.insert(tsn);
        self.advance_cum();
        if d.stream >= self.in_streams {
            return DataOutcome::InvalidStream;
        }
        if len > self.max_message {
            return DataOutcome::Violation;
        }
        self.buffered += len;
        if d.begin && d.end {
            let m = RxMessage {
                stream: d.stream,
                ppid: d.ppid,
                data: d.payload.clone(),
                unordered: d.unordered,
            };
            return self.complete(m, d.ssn, out);
        }
        self.frags.insert(
            tsn,
            Frag {
                stream: d.stream,
                ssn: d.ssn,
                ppid: d.ppid,
                unordered: d.unordered,
                begin: d.begin,
                end: d.end,
                payload: d.payload.clone(),
            },
        );
        self.try_assemble(tsn, out)
    }

    /// `tsn` の断片を含む連続した断片の並びを調べ、そろっていれば組み立てる。
    fn try_assemble(&mut self, tsn: u64, out: &mut Vec<RxMessage>) -> DataOutcome {
        let Some(f) = self.frags.get(&tsn) else {
            return DataOutcome::Accepted;
        };
        let mut total = f.payload.len();
        let mut start = tsn;
        if !f.begin {
            for (&t, g) in self.frags.range(..tsn).rev() {
                if t + 1 != start || !g.same_message(f) || g.end {
                    break;
                }
                start = t;
                total += g.payload.len();
                if g.begin || total > self.max_message {
                    break;
                }
            }
        }
        let mut end = tsn;
        if !f.end {
            for (&t, g) in self.frags.range(tsn + 1..) {
                if t != end + 1 || !g.same_message(f) || g.begin {
                    break;
                }
                end = t;
                total += g.payload.len();
                if g.end || total > self.max_message {
                    break;
                }
            }
        }
        if total > self.max_message {
            return DataOutcome::Violation;
        }
        let complete = self.frags.get(&start).is_some_and(|g| g.begin)
            && self.frags.get(&end).is_some_and(|g| g.end);
        if !complete {
            return DataOutcome::Accepted;
        }
        let tail = self.frags.split_off(&(end + 1));
        let mut mid = self.frags.split_off(&start);
        self.frags.extend(tail);
        let Some(head) = mid.values().next() else {
            return DataOutcome::Accepted;
        };
        let (stream, ssn, ppid, unordered) = (head.stream, head.ssn, head.ppid, head.unordered);
        let data = if mid.len() == 1 {
            mid.pop_first().map(|(_, g)| g.payload).unwrap_or_default()
        } else {
            let mut b = BytesMut::with_capacity(total);
            for g in mid.values() {
                b.extend_from_slice(&g.payload);
            }
            b.freeze()
        };
        let m = RxMessage {
            stream,
            ppid,
            data,
            unordered,
        };
        self.complete(m, ssn, out)
    }

    /// 組み立て済みのメッセージを配る（順序付きなら SSN の順に）。
    fn complete(&mut self, m: RxMessage, ssn: u16, out: &mut Vec<RxMessage>) -> DataOutcome {
        if m.unordered {
            out.push(m);
            return DataOutcome::Accepted;
        }
        let st = self.streams.entry(m.stream).or_default();
        let diff = ssn.wrapping_sub(st.next_ssn) as i16;
        if diff == 0 {
            out.push(m);
            st.next_ssn = st.next_ssn.wrapping_add(1);
            while let Some(n) = st.ready.remove(&st.next_ssn) {
                self.ready_count -= 1;
                out.push(n);
                st.next_ssn = st.next_ssn.wrapping_add(1);
            }
        } else if diff > 0 {
            if self.ready_count >= MAX_READY_MESSAGES {
                return DataOutcome::Violation;
            }
            if let Some(old) = st.ready.insert(ssn, m) {
                // 同じ SSN が 2 つ（相手の誤り）。古い方を捨てる。
                self.buffered = self.buffered.saturating_sub(old.data.len());
            } else {
                self.ready_count += 1;
            }
        } else {
            // 古い SSN（相手の誤り）。捨てる。
            self.buffered = self.buffered.saturating_sub(m.data.len());
        }
        DataOutcome::Accepted
    }

    /// FORWARD-TSN（RFC 3758 3.6）。
    pub fn forward_tsn(&mut self, new_cum: u32, streams: &[(u16, u16)], out: &mut Vec<RxMessage>) {
        let t = self.extend(new_cum);
        if t + (1 << 31) <= self.cum {
            return;
        }
        if t > self.cum {
            self.cum = t;
            self.received = self.received.split_off(&(t + 1));
        }
        // 新しい累積 TSN までの断片を捨てる（捨てられたメッセージのうち受け取っていた分）。
        // 累積 TSN が送った分を全部受け取って先に進んでいても、残りが送られずに捨てられた
        // メッセージの断片は残っているので、t が累積 TSN 以下でも捨てる。
        let keep = self.frags.split_off(&(t + 1));
        let dropped = std::mem::replace(&mut self.frags, keep);
        for g in dropped.values() {
            self.buffered = self.buffered.saturating_sub(g.payload.len());
        }
        self.advance_cum();
        // ストリームごとの最大の SSN にまとめる。
        let mut per: BTreeMap<u16, u16> = BTreeMap::new();
        for &(s, ssn) in streams {
            if s >= self.in_streams {
                continue;
            }
            per.entry(s)
                .and_modify(|v| {
                    if (ssn.wrapping_sub(*v) as i16) > 0 {
                        *v = ssn
                    }
                })
                .or_insert(ssn);
        }
        for (s, ssn) in per {
            let epoch = self.all_epoch_tsn;
            let st = self.streams.entry(s).or_insert_with(|| StreamIn {
                epoch_tsn: epoch,
                ..Default::default()
            });
            let epoch = st.epoch_tsn.max(epoch);
            if t <= epoch {
                // stream reset の前の世代の SSN（遅れて届いた FORWARD-TSN）。
                continue;
            }
            if epoch != 0 && t - epoch <= u16::MAX as u64 && ssn as u64 >= t - epoch {
                // リセットの後に割り当てられた SSN はリセットの後の TSN の数より小さい。
                // それより大きければ前の世代のもの。
                continue;
            }
            let lim = ssn.wrapping_sub(st.next_ssn);
            if (lim as i16) < 0 {
                continue;
            }
            let mut keys: Vec<u16> = st
                .ready
                .keys()
                .copied()
                .filter(|k| k.wrapping_sub(st.next_ssn) <= lim)
                .collect();
            let base = st.next_ssn;
            keys.sort_by_key(|k| k.wrapping_sub(base));
            for k in keys {
                if let Some(m) = st.ready.remove(&k) {
                    self.ready_count -= 1;
                    out.push(m);
                }
            }
            st.next_ssn = ssn.wrapping_add(1);
            while let Some(n) = st.ready.remove(&st.next_ssn) {
                self.ready_count -= 1;
                out.push(n);
                st.next_ssn = st.next_ssn.wrapping_add(1);
            }
        }
    }

    /// 受信ストリームの SSN を 0 に戻す（RFC 6525 の Outgoing SSN Reset Request を受けた）。
    /// `streams` が空ならすべて。`last_tsn` は要求の Sender's Last Assigned TSN（64 bit）。
    ///
    /// 呼ぶのは累積 TSN が `last_tsn` に届いてから。そのとき前の世代のメッセージは、そろって
    /// 順番待ちにあるか捨てられたかのどちらかなので、順番待ちのものを SSN の順に配ってから戻す。
    pub fn reset_streams(&mut self, streams: &[u16], last_tsn: u64, out: &mut Vec<RxMessage>) {
        let mut flush = |st: &mut StreamIn| {
            let base = st.next_ssn;
            let mut keys: Vec<u16> = st.ready.keys().copied().collect();
            keys.sort_by_key(|k| k.wrapping_sub(base));
            for k in keys {
                if let Some(m) = st.ready.remove(&k) {
                    out.push(m);
                }
            }
            st.next_ssn = 0;
            st.epoch_tsn = last_tsn;
        };
        if streams.is_empty() {
            for st in self.streams.values_mut() {
                self.ready_count -= st.ready.len();
                flush(st);
            }
            self.all_epoch_tsn = last_tsn;
        } else {
            for s in streams {
                let st = self.streams.entry(*s).or_default();
                self.ready_count -= st.ready.len();
                flush(st);
            }
        }
    }

    /// 持っている TSN・断片の数（試験用の目安）。
    pub fn held_items(&self) -> usize {
        self.received.len() + self.frags.len() + self.ready_count
    }
}

#[cfg(test)]
impl Receiver {
    pub fn debug_stream(&self, s: u16) -> String {
        let st = self.streams.get(&s);
        format!(
            "cum={} received={:?} frags={:?} next_ssn={:?} ready={:?}",
            self.cum as u32,
            self.received.iter().map(|t| *t as u32).collect::<Vec<_>>(),
            self.frags
                .iter()
                .map(|(t, f)| (*t as u32, f.stream, f.ssn, f.begin, f.end))
                .collect::<Vec<_>>(),
            st.map(|s| s.next_ssn),
            st.map(|s| s.ready.keys().copied().collect::<Vec<_>>())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(tsn: u32, ssn: u16, b: bool, e: bool, u: bool, p: &'static [u8]) -> DataChunk {
        DataChunk {
            tsn,
            stream: 1,
            ssn,
            ppid: 53,
            unordered: u,
            begin: b,
            end: e,
            immediate: false,
            payload: Bytes::from_static(p),
        }
    }

    #[test]
    fn extend_wraps() {
        let r = initial_ext(u32::MAX);
        assert_eq!(extend(r, 0), r + 1);
        assert_eq!(extend(r, u32::MAX - 1), r - 1);
    }

    #[test]
    fn reorder_and_fragments() {
        let mut r = Receiver::new(u32::MAX - 1, 1 << 20, 1 << 16, 10);
        let mut out = Vec::new();
        // TSN: MAX-1 (ssn0 B), MAX (ssn0 E), 0 (ssn1 B+E)
        assert_eq!(
            r.on_data(&data(0, 1, true, true, false, b"c"), &mut out),
            DataOutcome::Accepted
        );
        assert!(out.is_empty());
        assert_eq!(r.gap_blocks(10), vec![(3, 3)]);
        r.on_data(&data(u32::MAX, 0, false, true, false, b"b"), &mut out);
        assert!(out.is_empty());
        r.on_data(&data(u32::MAX - 1, 0, true, false, false, b"a"), &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(&out[0].data[..], b"ab");
        assert_eq!(&out[1].data[..], b"c");
        assert!(!r.has_gaps());
        assert_eq!(
            r.on_data(&data(0, 1, true, true, false, b"c"), &mut out),
            DataOutcome::Duplicate
        );
        assert_eq!(r.take_dups(), vec![0]);
    }

    #[test]
    fn forward_tsn_skips() {
        let mut r = Receiver::new(10, 1 << 20, 1 << 16, 10);
        let mut out = Vec::new();
        // TSN 10 (ssn0 B) 失われる 11 (ssn0 E)、12 は ssn1。
        r.on_data(&data(10, 0, true, false, false, b"a"), &mut out);
        r.on_data(&data(12, 1, true, true, false, b"z"), &mut out);
        assert!(out.is_empty());
        r.forward_tsn(11, &[(1, 0)], &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(&out[0].data[..], b"z");
        assert_eq!(r.cum, initial_ext(12));
        assert_eq!(r.buffered, 1);
    }

    #[test]
    fn oversized_is_violation() {
        let mut r = Receiver::new(1, 1 << 20, 3, 10);
        let mut out = Vec::new();
        r.on_data(&data(1, 0, true, false, false, b"ab"), &mut out);
        assert_eq!(
            r.on_data(&data(2, 0, false, false, false, b"cd"), &mut out),
            DataOutcome::Violation
        );
    }
}
