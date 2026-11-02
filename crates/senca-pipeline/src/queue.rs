//! 有界のキュー（仕様 5.3: 期限切れ・溢れは捨てる。依存フレームだけを残して壊れた GOP を渡さない。無限には待たない）。
//!
//! - [`AuQueue`]: 符号化済みの AU（入力 → 復号のスレッド）。溢れたら、映像は溜まっている AU をすべて捨てて
//!   次のキーフレームから受け直す。音声は古いものを 1 つずつ捨てる。
//! - [`Inbox`]: 生のフレーム（部品の間）。入力ごとに有界。溢れたら古いものを捨てる（`Block` のときは
//!   上限の時間だけ待ってから捨てる）。合成・ミキサーの刻みの判断に使う「どこまで届いたか」も持つ。

use crate::clock::InputState;
use crate::model::OverflowPolicy;
use senca_core::{AccessUnit, CodecConfig};
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // 中の処理はパニックしないので、毒されていても状態は一貫している。
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// AU のキューの要素。
#[derive(Debug, Clone)]
pub enum AuItem {
    /// トラックの設定が変わった（次の AU から）。
    Config(CodecConfig),
    Au(AccessUnit),
}

/// 取り出しの結果。
#[derive(Debug)]
pub enum Pop<T> {
    Item(T),
    /// 時間内に何も来なかった。
    Empty,
    /// 入力が終わり、残りも無い。
    Ended,
}

#[derive(Debug, Default)]
struct AuState {
    q: VecDeque<AuItem>,
    aus: usize,
    await_key: bool,
    ended: bool,
    closed: bool,
    dropped: u64,
}

/// 符号化済みの AU のキュー。
#[derive(Debug)]
pub struct AuQueue {
    st: Mutex<AuState>,
    cv: Condvar,
    cap: usize,
    video: bool,
    policy: OverflowPolicy,
    block_timeout: Duration,
}

impl AuQueue {
    /// `video` なら最初のキーフレームまで捨てる（復号はキーフレームから始める）。
    pub fn new(cap: usize, video: bool, policy: OverflowPolicy, block_timeout: Duration) -> Self {
        AuQueue {
            st: Mutex::new(AuState {
                await_key: video,
                ..Default::default()
            }),
            cv: Condvar::new(),
            cap: cap.max(1),
            video,
            policy,
            block_timeout,
        }
    }

    /// 設定の変更を入れる（捨てない）。
    pub fn push_config(&self, c: CodecConfig) {
        let mut st = lock(&self.st);
        if st.closed || st.ended {
            return;
        }
        st.q.push_back(AuItem::Config(c));
        drop(st);
        self.cv.notify_all();
    }

    /// AU を 1 つ入れる。捨てた AU の数を返す（この AU を含む）。
    pub fn push(&self, au: AccessUnit) -> u64 {
        let mut st = lock(&self.st);
        if st.closed || st.ended {
            return 1;
        }
        if self.policy == OverflowPolicy::Block && st.aus >= self.cap {
            let deadline = Instant::now() + self.block_timeout;
            while st.aus >= self.cap && !st.closed {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                st = self
                    .cv
                    .wait_timeout(st, deadline - now)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            if st.closed {
                return 1;
            }
        }
        let mut dropped = 0;
        if st.aus >= self.cap {
            if self.video {
                // 溜まった AU を捨て、次のキーフレームから受け直す（設定の変更は残す）。
                let before = st.aus as u64;
                st.q.retain(|i| matches!(i, AuItem::Config(_)));
                st.aus = 0;
                st.await_key = true;
                dropped += before;
            } else {
                while st.aus >= self.cap {
                    let Some(pos) = st.q.iter().position(|i| matches!(i, AuItem::Au(_))) else {
                        break;
                    };
                    st.q.remove(pos);
                    st.aus -= 1;
                    dropped += 1;
                }
            }
        }
        if self.video && st.await_key {
            if !au.keyframe {
                st.dropped += dropped + 1;
                return dropped + 1;
            }
            st.await_key = false;
        }
        st.q.push_back(AuItem::Au(au));
        st.aus += 1;
        st.dropped += dropped;
        drop(st);
        self.cv.notify_all();
        dropped
    }

    /// 1 つ取り出す（`timeout` まで待つ）。
    pub fn pop(&self, timeout: Duration) -> Pop<AuItem> {
        let mut st = lock(&self.st);
        let deadline = Instant::now() + timeout;
        loop {
            if st.closed {
                return Pop::Ended;
            }
            if let Some(i) = st.q.pop_front() {
                if matches!(i, AuItem::Au(_)) {
                    st.aus -= 1;
                }
                drop(st);
                self.cv.notify_all();
                return Pop::Item(i);
            }
            if st.ended {
                return Pop::Ended;
            }
            let now = Instant::now();
            if now >= deadline {
                return Pop::Empty;
            }
            st = self
                .cv
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// 入力の終わり（残りは取り出せる）。
    pub fn end(&self) {
        lock(&self.st).ended = true;
        self.cv.notify_all();
    }

    /// 止める（残りも捨てる）。
    pub fn close(&self) {
        let mut st = lock(&self.st);
        st.closed = true;
        st.q.clear();
        st.aus = 0;
        drop(st);
        self.cv.notify_all();
    }

    pub fn dropped(&self) -> u64 {
        lock(&self.st).dropped
    }

    pub fn len(&self) -> usize {
        lock(&self.st).aus
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 次のキーフレームを待っている。
    pub fn awaiting_keyframe(&self) -> bool {
        lock(&self.st).await_key
    }
}

#[derive(Debug)]
struct InboxState<T> {
    queues: Vec<VecDeque<(i64, T)>>,
    states: Vec<InputState>,
    dropped: Vec<u64>,
    closed: bool,
    /// 変化のたびに増える（待つ側が取りこぼさないように）。
    seq: u64,
}

/// 生のフレームの受け口（入力が `n` 個）。時刻はセッション時刻（µs）。
#[derive(Debug)]
pub struct Inbox<T> {
    st: Mutex<InboxState<T>>,
    cv: Condvar,
    cap: usize,
    policy: OverflowPolicy,
    block_timeout: Duration,
}

impl<T> Inbox<T> {
    pub fn new(n: usize, cap: usize, policy: OverflowPolicy, block_timeout: Duration) -> Self {
        Inbox {
            st: Mutex::new(InboxState {
                queues: (0..n).map(|_| VecDeque::new()).collect(),
                states: vec![InputState::default(); n],
                dropped: vec![0; n],
                closed: false,
                seq: 0,
            }),
            cv: Condvar::new(),
            cap: cap.max(1),
            policy,
            block_timeout,
        }
    }

    pub fn inputs(&self) -> usize {
        lock(&self.st).queues.len()
    }

    /// 入れる。`end_us` はデータの終わりの時刻（映像はフレームの時刻、音声は終わりの時刻）、
    /// `now_us` は届いた時の単調時計のセッション時刻。捨てた数を返す。
    pub fn push(&self, idx: usize, pts_us: i64, end_us: i64, item: T, now_us: i64) -> u64 {
        let mut st = lock(&self.st);
        if st.closed || idx >= st.queues.len() || st.states[idx].ended {
            return 1;
        }
        if self.policy == OverflowPolicy::Block && st.queues[idx].len() >= self.cap {
            let deadline = Instant::now() + self.block_timeout;
            while st.queues[idx].len() >= self.cap && !st.closed {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                st = self
                    .cv
                    .wait_timeout(st, deadline - now)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            if st.closed {
                return 1;
            }
        }
        let mut dropped = 0;
        while st.queues[idx].len() >= self.cap {
            st.queues[idx].pop_front();
            dropped += 1;
        }
        st.dropped[idx] += dropped;
        st.queues[idx].push_back((pts_us, item));
        let s = &mut st.states[idx];
        s.latest_us = Some(s.latest_us.map_or(end_us, |l| l.max(end_us)));
        s.last_arrival_us = Some(now_us);
        st.seq += 1;
        drop(st);
        self.cv.notify_all();
        dropped
    }

    /// 入力 `idx` の、時刻 `t_us` 以前のものを取り出す。
    pub fn drain_until(&self, idx: usize, t_us: i64) -> Vec<(i64, T)> {
        let mut st = lock(&self.st);
        let mut out = Vec::new();
        if let Some(q) = st.queues.get_mut(idx) {
            while q.front().is_some_and(|(p, _)| *p <= t_us) {
                out.extend(q.pop_front());
            }
        }
        if !out.is_empty() {
            st.seq += 1;
            drop(st);
            self.cv.notify_all();
        }
        out
    }

    /// 入力 `idx` の先頭の時刻。
    pub fn first_pts(&self, idx: usize) -> Option<i64> {
        lock(&self.st).queues.get(idx)?.front().map(|(p, _)| *p)
    }

    /// 入力 `idx` のものをすべて取り出す。
    pub fn drain_all(&self, idx: usize) -> Vec<(i64, T)> {
        self.drain_until(idx, i64::MAX)
    }

    /// どれかの入力から 1 つ取り出す（入力 0 から順に見る）。`timeout` まで待つ。
    /// すべての入力が終わって空なら `Ended`。
    pub fn pop(&self, timeout: Duration) -> Pop<(usize, i64, T)> {
        let mut st = lock(&self.st);
        let deadline = Instant::now() + timeout;
        loop {
            if st.closed {
                return Pop::Ended;
            }
            if let Some(i) = st.queues.iter().position(|q| !q.is_empty())
                && let Some((p, x)) = st.queues[i].pop_front()
            {
                st.seq += 1;
                drop(st);
                self.cv.notify_all();
                return Pop::Item((i, p, x));
            }
            if st.states.iter().all(|s| s.ended) {
                return Pop::Ended;
            }
            let now = Instant::now();
            if now >= deadline {
                return Pop::Empty;
            }
            st = self
                .cv
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// 入力ごとの状態。
    pub fn states(&self) -> Vec<InputState> {
        lock(&self.st).states.clone()
    }

    pub fn dropped(&self) -> Vec<u64> {
        lock(&self.st).dropped.clone()
    }

    /// 変化の番号。
    pub fn seq(&self) -> u64 {
        lock(&self.st).seq
    }

    /// 変化の番号が `seen` から変わるか、`timeout` まで待つ。
    pub fn wait_change(&self, seen: u64, timeout: Duration) -> u64 {
        let mut st = lock(&self.st);
        let deadline = Instant::now() + timeout;
        while st.seq == seen && !st.closed {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            st = self
                .cv
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        st.seq
    }

    /// 入力 `idx` の終わり。
    pub fn end(&self, idx: usize) {
        let mut st = lock(&self.st);
        if let Some(s) = st.states.get_mut(idx) {
            s.ended = true;
        }
        st.seq += 1;
        drop(st);
        self.cv.notify_all();
    }

    /// 止める。
    pub fn close(&self) {
        let mut st = lock(&self.st);
        st.closed = true;
        for q in &mut st.queues {
            q.clear();
        }
        st.seq += 1;
        drop(st);
        self.cv.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        lock(&self.st).closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use senca_core::{Codec, Timebase, TrackId};

    fn au(seq: u64, key: bool) -> AccessUnit {
        AccessUnit {
            track: TrackId(0),
            codec: Codec::Vp9,
            epoch: 0,
            sequence: seq,
            config_generation: 0,
            pts: seq as i64,
            dts: seq as i64,
            duration: 1,
            timebase: Timebase::MPEG,
            keyframe: key,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::new(),
        }
    }

    fn seqs(q: &AuQueue) -> Vec<u64> {
        let mut v = Vec::new();
        while let Pop::Item(AuItem::Au(a)) = q.pop(Duration::ZERO) {
            v.push(a.sequence);
        }
        v
    }

    #[test]
    fn video_starts_at_keyframe_and_overflow_waits_for_next_keyframe() {
        let q = AuQueue::new(3, true, OverflowPolicy::Drop, Duration::ZERO);
        assert_eq!(q.push(au(0, false)), 1); // 最初のキーフレームまで捨てる
        assert_eq!(q.push(au(1, true)), 0);
        assert_eq!(q.push(au(2, false)), 0);
        assert_eq!(q.push(au(3, false)), 0);
        // 溢れ: 溜まった 3 つを捨て、これもキーフレームでないので捨てる
        assert_eq!(q.push(au(4, false)), 4);
        assert!(q.awaiting_keyframe());
        assert_eq!(q.push(au(5, false)), 1);
        assert_eq!(q.push(au(6, true)), 0);
        assert_eq!(q.push(au(7, false)), 0);
        assert_eq!(seqs(&q), vec![6, 7]);
        assert_eq!(q.dropped(), 6);
        q.end();
        assert!(matches!(q.pop(Duration::ZERO), Pop::Ended));
    }

    #[test]
    fn audio_drops_oldest() {
        let q = AuQueue::new(2, false, OverflowPolicy::Drop, Duration::ZERO);
        for i in 0..5 {
            q.push(au(i, true));
        }
        assert_eq!(seqs(&q), vec![3, 4]);
        assert_eq!(q.dropped(), 3);
    }

    #[test]
    fn block_policy_waits_then_drops() {
        let q = std::sync::Arc::new(AuQueue::new(
            1,
            false,
            OverflowPolicy::Block,
            Duration::from_millis(500),
        ));
        q.push(au(0, true));
        let q2 = q.clone();
        let t = std::thread::spawn(move || q2.push(au(1, true)));
        std::thread::sleep(Duration::from_millis(50));
        // 取り出すと、待っていた方が入る（捨てない）
        assert!(matches!(q.pop(Duration::ZERO), Pop::Item(_)));
        assert_eq!(t.join().unwrap(), 0);
        assert_eq!(seqs(&q), vec![1]);
        // 取り出されなければ、時間切れの後で古いものを捨てる（無期限には待たない）
        let q = AuQueue::new(1, false, OverflowPolicy::Block, Duration::from_millis(30));
        q.push(au(0, true));
        let t0 = Instant::now();
        assert_eq!(q.push(au(1, true)), 1);
        assert!(t0.elapsed() >= Duration::from_millis(25));
        assert_eq!(seqs(&q), vec![1]);
    }

    #[test]
    fn inbox_tracks_latest_and_drains_by_time() {
        let ib: Inbox<u32> = Inbox::new(2, 3, OverflowPolicy::Drop, Duration::ZERO);
        ib.push(0, 0, 0, 10, 5);
        ib.push(0, 100, 100, 11, 6);
        ib.push(1, 50, 70, 20, 7);
        let s = ib.states();
        assert_eq!(s[0].latest_us, Some(100));
        assert_eq!(s[1].latest_us, Some(70));
        assert_eq!(s[1].last_arrival_us, Some(7));
        assert_eq!(ib.drain_until(0, 50), vec![(0, 10)]);
        assert_eq!(ib.drain_all(0), vec![(100, 11)]);
        for i in 0..5 {
            ib.push(1, 100 + i, 100 + i, i as u32, 8);
        }
        assert_eq!(ib.dropped()[1], 3);
        assert_eq!(ib.drain_all(1).len(), 3);
        let seen = ib.seq();
        assert_eq!(ib.wait_change(seen, Duration::from_millis(10)), seen);
        ib.end(0);
        assert!(ib.states()[0].ended);
        assert_eq!(ib.push(0, 1, 1, 0, 0), 1, "ended input rejects");
        ib.end(1);
        assert!(matches!(ib.pop(Duration::ZERO), Pop::Ended));
    }
}
