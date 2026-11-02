//! 時計と出力の刻み（仕様 4.3、REQ-008「合成の出力は自前の出力クロックで一定のフレームレートを刻む」）。
//!
//! # 時間軸
//!
//! - **セッション時刻**（µs）: 実行を始めた時を 0 とする共通の時間軸。合成・ミキサーの出力の時刻、
//!   符号化した AU の時刻はすべてこれ。映像と音声は同じ時間軸に乗るので、出力の A/V は揃う。
//! - **入力の時刻**は入力（source）ごとに [`SourceTimeMap`] でセッション時刻に写す。同じ入力の映像と音声は
//!   同じ写像を使う（入力の中の A/V のずれを保つ）。
//!
//! # 刻み（[`TickScheduler`]）
//!
//! - `Realtime`: 単調時計のセッション時刻から `latency` を引いた時刻までの刻みを出す。遅れが大きければ
//!   刻みを飛ばす（ライブ優先、飛ばした数を数える）。
//! - `Input`: すべての入力がその刻みの時刻までそろったら出す。来ない入力（`missing_timeout` の間、何も来ない）は
//!   待たない。すべての入力が終わったら、残りを最後の入力の時刻まで出す。

use crate::model::ClockMode;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

/// 単調時計（µs、原点は任意）。
pub trait Clock: Send + Sync {
    fn now_us(&self) -> i64;
}

/// 本物の単調時計。
#[derive(Debug, Clone)]
pub struct MonotonicClock {
    origin: Instant,
}

impl Default for MonotonicClock {
    fn default() -> Self {
        MonotonicClock {
            origin: Instant::now(),
        }
    }
}

impl Clock for MonotonicClock {
    fn now_us(&self) -> i64 {
        self.origin.elapsed().as_micros() as i64
    }
}

/// 試験用の時計（手で進める）。
#[derive(Debug, Clone, Default)]
pub struct FakeClock {
    now: Arc<AtomicI64>,
}

impl FakeClock {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&self, us: i64) {
        self.now.store(us, Ordering::SeqCst);
    }
    pub fn advance(&self, us: i64) {
        self.now.fetch_add(us, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_us(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
}

/// 入力の状態（刻みの判断に使う）。時刻はすべてセッション時刻（µs）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputState {
    /// 届いたデータの最後の時刻（映像はフレームの時刻、音声はデータの終わりの時刻）。
    pub latest_us: Option<i64>,
    /// 最後に届いた時（単調時計のセッション時刻）。
    pub last_arrival_us: Option<i64>,
    /// 入力が終わった。
    pub ended: bool,
}

/// 次に何をするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// この時刻の刻みを出す。`skipped` は遅れのため飛ばした刻みの数。
    Tick { t_us: i64, skipped: u64 },
    /// 単調時計のこの時刻（セッション時刻）まで待つ。
    WaitUntil(i64),
    /// 入力を待つ（新しい入力か、この時刻のどちらか早い方で見直す）。
    WaitInput(i64),
    /// もう刻みは無い（すべての入力が終わった）。
    Done,
}

/// 一定の間隔の刻み。
#[derive(Debug, Clone)]
pub struct TickScheduler {
    mode: ClockMode,
    /// 間隔（µs の分数: num / den）。映像は 1e6 * fps_den / fps_num。
    period_num: i64,
    period_den: i64,
    latency_us: i64,
    missing_timeout_us: i64,
    /// 入力側の時刻の判断で、刻みの時刻にこれを足した所までデータを要る（音声は刻みの長さ、映像は 0）。
    need_ahead_us: i64,
    /// 遅れの上限（これを超えたら飛ばす）。
    max_lag_us: i64,
    next: i64,
    origin_us: i64,
    pub skipped_total: u64,
}

impl TickScheduler {
    /// 映像（キャンバスの fps）。
    pub fn video(
        mode: ClockMode,
        fps_num: u32,
        fps_den: u32,
        latency_us: i64,
        missing_timeout_us: i64,
    ) -> Self {
        Self::new(
            mode,
            1_000_000 * fps_den.max(1) as i64,
            fps_num.max(1) as i64,
            latency_us,
            missing_timeout_us,
            0,
        )
    }

    /// 音声（`frames` 標本ずつ、48 kHz）。
    pub fn audio(mode: ClockMode, frames: u32, latency_us: i64, missing_timeout_us: i64) -> Self {
        let period = frames as i64 * 1_000_000;
        Self::new(
            mode,
            period,
            48_000,
            latency_us,
            missing_timeout_us,
            period / 48_000,
        )
    }

    fn new(
        mode: ClockMode,
        period_num: i64,
        period_den: i64,
        latency_us: i64,
        missing_timeout_us: i64,
        need_ahead_us: i64,
    ) -> Self {
        TickScheduler {
            mode,
            period_num,
            period_den,
            latency_us: latency_us.max(0),
            missing_timeout_us: missing_timeout_us.max(1),
            need_ahead_us,
            max_lag_us: 500_000,
            next: 0,
            origin_us: 0,
            skipped_total: 0,
        }
    }

    /// 刻みの番号 0 の時刻（セッション時刻）を決める（途中から始める世代など）。
    pub fn start_at(&mut self, origin_us: i64) {
        self.origin_us = origin_us;
        self.next = 0;
    }

    /// `n` 番目の刻みの時刻。
    pub fn tick_time(&self, n: i64) -> i64 {
        self.origin_us + ((n as i128 * self.period_num as i128) / self.period_den as i128) as i64
    }

    pub fn next_index(&self) -> i64 {
        self.next
    }

    /// 次の刻みを `t_us` 以後の最初の刻みまで進める（刻みの格子は保つ）。
    pub fn skip_to(&mut self, t_us: i64) {
        while self.tick_time(self.next) < t_us {
            self.next += 1;
        }
    }

    /// 刻みの間隔（µs、丸め）。
    pub fn period_us(&self) -> i64 {
        self.period_num / self.period_den
    }

    /// 何をするかを決める。`now_us` は単調時計のセッション時刻。`Tick` を返したら刻みを 1 つ進めたことになる。
    pub fn poll(&mut self, now_us: i64, inputs: &[InputState]) -> Due {
        let t = self.tick_time(self.next);
        // すべての入力が終わった: 残りを最後のデータまで出して終わる（どちらのモードでも、時計を待たない）。
        if !inputs.is_empty() && inputs.iter().all(|s| s.ended) {
            let max_latest = inputs.iter().filter_map(|s| s.latest_us).max();
            return match max_latest {
                Some(m) if t + self.need_ahead_us <= m => {
                    self.next += 1;
                    Due::Tick {
                        t_us: t,
                        skipped: 0,
                    }
                }
                _ => Due::Done,
            };
        }
        match self.mode {
            ClockMode::Realtime => {
                let target = now_us - self.latency_us;
                if target < t {
                    return Due::WaitUntil(t + self.latency_us);
                }
                let mut skipped = 0;
                if target - t > self.max_lag_us {
                    // 遅れすぎ: 今の時刻の刻みまで飛ばす。
                    let n = ((target - self.origin_us) as i128 * self.period_den as i128
                        / self.period_num as i128) as i64;
                    skipped = (n - self.next).max(0) as u64;
                    self.next = n.max(self.next);
                    self.skipped_total += skipped;
                }
                let t = self.tick_time(self.next);
                self.next += 1;
                Due::Tick { t_us: t, skipped }
            }
            ClockMode::Input => {
                let need = t + self.need_ahead_us;
                let mut any_live = false;
                let mut all_ended = true;
                let mut earliest_timeout = i64::MAX;
                for s in inputs {
                    if s.ended {
                        continue;
                    }
                    all_ended = false;
                    if s.latest_us.is_some_and(|l| l >= need) {
                        any_live = true;
                        continue;
                    }
                    // まだ届いていない: 見限る時刻を過ぎていなければ待つ。
                    let since = s.last_arrival_us.unwrap_or(0);
                    let deadline = since + self.missing_timeout_us;
                    if now_us < deadline {
                        earliest_timeout = earliest_timeout.min(deadline);
                    }
                }
                if all_ended {
                    // 入力が無い（`inputs` が空）: 刻みを出さない。
                    return Due::Done;
                }
                if earliest_timeout != i64::MAX {
                    return Due::WaitInput(earliest_timeout);
                }
                if any_live {
                    self.next += 1;
                    return Due::Tick {
                        t_us: t,
                        skipped: 0,
                    };
                }
                // 生きている入力が無い（全部見限った）: 見直しの時刻だけ返す。
                Due::WaitInput(now_us + self.missing_timeout_us)
            }
        }
    }
}

/// 入力ごとの時刻の写像（入力の時刻 → セッション時刻）。
#[derive(Debug, Clone, Default)]
pub struct SourceTimeMap {
    offset: Option<i64>,
    last_mapped: Option<i64>,
    pub reanchors: u64,
}

/// 入力の時刻がこれ以上飛んだら写し直す（µs）。
pub const MAX_JUMP_US: i64 = 5_000_000;

impl SourceTimeMap {
    /// `pts_us`（入力の時刻）をセッション時刻にする。`anchor_us` は最初の対応付け（と写し直し）に使う
    /// セッション時刻（ライブなら到着時の単調時計、入力時刻のモードなら最初の刻みの時刻）。
    pub fn map(&mut self, pts_us: i64, anchor_us: i64, discontinuity: bool) -> i64 {
        let off = match self.offset {
            None => anchor_us - pts_us,
            Some(o) => {
                let m = pts_us + o;
                let jumped = self
                    .last_mapped
                    .is_some_and(|l| (m - l).abs() > MAX_JUMP_US);
                if discontinuity || jumped {
                    self.reanchors += 1;
                    // 前の続きに置く（時刻を逆行させない）。
                    let base = self
                        .last_mapped
                        .map_or(anchor_us, |l| l.max(anchor_us.min(l + MAX_JUMP_US)));
                    base - pts_us
                } else {
                    o
                }
            }
        };
        self.offset = Some(off);
        let m = pts_us + off;
        self.last_mapped = Some(self.last_mapped.map_or(m, |l| l.max(m)));
        m
    }

    pub fn is_anchored(&self) -> bool {
        self.offset.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(latest: Option<i64>, arrival: Option<i64>, ended: bool) -> InputState {
        InputState {
            latest_us: latest,
            last_arrival_us: arrival,
            ended,
        }
    }

    #[test]
    fn realtime_ticks_follow_clock_with_latency() {
        let clock = FakeClock::new();
        let mut s = TickScheduler::video(ClockMode::Realtime, 30, 1, 100_000, 2_000_000);
        assert_eq!(s.poll(clock.now_us(), &[]), Due::WaitUntil(100_000));
        clock.set(100_000);
        assert_eq!(
            s.poll(clock.now_us(), &[]),
            Due::Tick {
                t_us: 0,
                skipped: 0
            }
        );
        assert_eq!(s.poll(clock.now_us(), &[]), Due::WaitUntil(133_333));
        clock.advance(40_000);
        assert_eq!(
            s.poll(clock.now_us(), &[]),
            Due::Tick {
                t_us: 33_333,
                skipped: 0
            }
        );
        // 1 秒遅れたら飛ばす
        clock.advance(1_000_000);
        let Due::Tick { t_us, skipped } = s.poll(clock.now_us(), &[]) else {
            panic!()
        };
        assert!(skipped >= 29, "{skipped}");
        assert!(t_us <= clock.now_us() - 100_000 && t_us > clock.now_us() - 100_000 - 34_000);
        assert_eq!(s.skipped_total, skipped);
        // 29.97 fps の刻みは丸めがたまらない
        let s = TickScheduler::video(ClockMode::Realtime, 30000, 1001, 0, 1);
        assert_eq!(s.tick_time(30000), 1001 * 1_000_000);
        // 入力がすべて終わったら時計を待たずに残りを出して終わる
        let mut s = TickScheduler::video(ClockMode::Realtime, 10, 1, 0, 1);
        let ended = InputState {
            latest_us: Some(150_000),
            last_arrival_us: Some(0),
            ended: true,
        };
        assert_eq!(
            s.poll(0, &[ended]),
            Due::Tick {
                t_us: 0,
                skipped: 0
            }
        );
        assert_eq!(
            s.poll(0, &[ended]),
            Due::Tick {
                t_us: 100_000,
                skipped: 0
            }
        );
        assert_eq!(s.poll(0, &[ended]), Due::Done);
        let mut s = TickScheduler::video(ClockMode::Input, 10, 1, 0, 1);
        s.start_at(50_000);
        s.skip_to(260_000);
        assert_eq!(s.tick_time(s.next_index()), 350_000);
    }

    #[test]
    fn input_mode_waits_for_all_live_inputs() {
        let mut s = TickScheduler::video(ClockMode::Input, 10, 1, 0, 1_000_000);
        // 片方だけ届いた: もう片方を待つ（見限る時刻まで）
        let a = st(Some(0), Some(0), false);
        let b = st(None, None, false);
        assert_eq!(s.poll(0, &[a, b]), Due::WaitInput(1_000_000));
        // 両方届いた
        let b = st(Some(0), Some(10), false);
        assert_eq!(
            s.poll(10, &[a, b]),
            Due::Tick {
                t_us: 0,
                skipped: 0
            }
        );
        // 次の刻み（100 ms）には a がまだ
        let b = st(Some(200_000), Some(20), false);
        assert_eq!(s.poll(20, &[a, b]), Due::WaitInput(1_000_000));
        // a が切れた（1 秒何も来ない）: b だけで進む
        assert_eq!(
            s.poll(1_000_001, &[a, b]),
            Due::Tick {
                t_us: 100_000,
                skipped: 0
            }
        );
        assert_eq!(
            s.poll(1_000_002, &[a, b]),
            Due::Tick {
                t_us: 200_000,
                skipped: 0
            }
        );
        assert!(matches!(s.poll(1_000_003, &[a, b]), Due::WaitInput(_)));
        // 終わり: 最後のデータまで出して Done
        let a = st(Some(0), Some(0), true);
        let b = st(Some(450_000), Some(30), true);
        let mut ts = Vec::new();
        loop {
            match s.poll(2_000_000, &[a, b]) {
                Due::Tick { t_us, .. } => ts.push(t_us),
                Due::Done => break,
                d => panic!("{d:?}"),
            }
        }
        assert_eq!(ts, vec![300_000, 400_000]);
    }

    #[test]
    fn input_mode_audio_needs_full_period() {
        let mut s = TickScheduler::audio(ClockMode::Input, 960, 0, 1_000_000);
        assert_eq!(s.period_us(), 20_000);
        let a = st(Some(19_000), Some(0), false);
        assert!(matches!(s.poll(0, &[a]), Due::WaitInput(_)));
        let a = st(Some(20_000), Some(0), false);
        assert_eq!(
            s.poll(0, &[a]),
            Due::Tick {
                t_us: 0,
                skipped: 0
            }
        );
        // 終わりでは、刻み全体が埋まらない残りは出さない
        let a = st(Some(50_000), Some(0), true);
        assert_eq!(
            s.poll(0, &[a]),
            Due::Tick {
                t_us: 20_000,
                skipped: 0
            }
        );
        assert_eq!(s.poll(0, &[a]), Due::Done);
    }

    #[test]
    fn source_map_anchors_and_reanchors() {
        let mut m = SourceTimeMap::default();
        assert_eq!(m.map(90_000_000, 1_000, false), 1_000);
        assert_eq!(m.map(90_033_000, 5_000, false), 34_000);
        // 時刻の飛び（送り手の再起動）: 前の続きに置く
        let x = m.map(0, 50_000, false);
        assert!(x >= 34_000, "{x}");
        assert_eq!(m.reanchors, 1);
        assert_eq!(m.map(33_000, 60_000, false), x + 33_000);
        // 不連続の印
        let y = m.map(10_000_000, 70_000, true);
        assert!(y >= x + 33_000);
        assert_eq!(m.reanchors, 2);
    }
}
