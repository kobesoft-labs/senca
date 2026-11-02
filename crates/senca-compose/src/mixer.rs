//! 音声ミキサー（R8-6）: 入力ごとの再標本化（→ 48 kHz ステレオ）、音量（dB）・ミュート・パン・ソロ・遅延、
//! 時刻による同期、出力のリミッター、入力ごと・マスターのピーク／RMS のメーター。
//!
//! # 時刻
//! 出力の標本の位置 n は時刻 n / 48000 秒。`push` の時刻（µs）から置く位置を決める。
//! 前の続きから `resync_ms` 以内のずれは揺らぎとみなして続きに置く（ドリフト・ジッタを吸収）。
//! それより大きいずれは不連続として、隙間は無音で埋め、重なりは新しい方を採る。
//! `mix` の範囲にデータが無い入力は無音（取りこぼしは数える）。
//!
//! # パン
//! バランス方式: 左の利得 = min(1, 1 − pan)、右 = min(1, 1 + pan)。中央で左右とも 1。
//!
//! # リミッター
//! 先読み（既定 1.5 ms）で、窓の最小の利得を箱型に平滑して掛ける。ピークが来る前に利得を下げ終えるので、
//! 出力は天井（既定 −1 dBFS）を超えない。戻り（リリース）は指数で。遅延は先読みぶん。

use std::collections::{BTreeMap, VecDeque};

use crate::error::{ComposeError, Result};
use crate::resample::Resampler;

/// 出力の標本化周波数。
pub const OUTPUT_RATE: u32 = 48000;

/// ミキサー全体の設定。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MixerConfig {
    /// マスターの音量（dB）。
    pub master_gain_db: f32,
    /// 続きとみなすずれの上限（ms）。
    pub resync_ms: f32,
    /// 入力ごとに貯める上限（ms）。超えたら古いものを捨てる。
    pub max_buffer_ms: f32,
    pub limiter: LimiterConfig,
}

impl Default for MixerConfig {
    fn default() -> Self {
        Self {
            master_gain_db: 0.0,
            resync_ms: 20.0,
            max_buffer_ms: 1000.0,
            limiter: LimiterConfig::default(),
        }
    }
}

/// リミッターの設定。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LimiterConfig {
    pub enabled: bool,
    /// 天井（dBFS）。
    pub ceiling_db: f32,
    pub lookahead_ms: f32,
    pub release_ms: f32,
}

impl Default for LimiterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ceiling_db: -1.0,
            lookahead_ms: 1.5,
            release_ms: 150.0,
        }
    }
}

/// 入力ごとの設定。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputConfig {
    pub gain_db: f32,
    pub mute: bool,
    pub solo: bool,
    /// −1（左）〜 1（右）。
    pub pan: f32,
    /// 映像との同期のための遅延（ms、0〜5000）。
    pub delay_ms: f32,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            gain_db: 0.0,
            mute: false,
            solo: false,
            pan: 0.0,
            delay_ms: 0.0,
        }
    }
}

/// ピークと RMS（dBFS、無音は −120）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Meter {
    pub peak_db: [f32; 2],
    pub rms_db: [f32; 2],
}

impl Default for Meter {
    fn default() -> Self {
        Self {
            peak_db: [SILENCE_DB; 2],
            rms_db: [SILENCE_DB; 2],
        }
    }
}

const SILENCE_DB: f32 = -120.0;

fn to_db(v: f32) -> f32 {
    if v <= 1e-6 {
        SILENCE_DB
    } else {
        (20.0 * v.log10()).max(SILENCE_DB)
    }
}

fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn measure(frames: &[[f32; 2]]) -> Meter {
    if frames.is_empty() {
        return Meter::default();
    }
    let mut peak = [0f32; 2];
    let mut sq = [0f64; 2];
    for f in frames {
        for c in 0..2 {
            peak[c] = peak[c].max(f[c].abs());
            sq[c] += (f[c] as f64) * (f[c] as f64);
        }
    }
    let n = frames.len() as f64;
    Meter {
        peak_db: [to_db(peak[0]), to_db(peak[1])],
        rms_db: [
            to_db((sq[0] / n).sqrt() as f32),
            to_db((sq[1] / n).sqrt() as f32),
        ],
    }
}

/// 入力の統計。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputStats {
    /// データが無く無音で埋めた標本数（出力の標本）。
    pub underrun_frames: u64,
    /// 時刻の飛びで入れた無音の標本数。
    pub gap_frames: u64,
    /// 貯めすぎ・重なりで捨てた標本数。
    pub dropped_frames: u64,
    /// 不連続（再同期）の回数。
    pub resyncs: u64,
}

struct Input {
    channels: usize,
    cfg: InputConfig,
    resampler: Resampler,
    /// 今の区間の始まり（出力の位置、遅延なし）。
    seg_base: Option<i64>,
    seg_in_frames: u64,
    seg_out_frames: u64,
    seg_delay: i64,
    buf: VecDeque<[f32; 2]>,
    buf_start: i64,
    meter: Meter,
    stats: InputStats,
}

/// メーターの読み（直前の `mix` の範囲）。
#[derive(Debug, Clone, PartialEq)]
pub struct MixerMeters {
    /// 入力ごと（音量・パン・ミュートの後）。
    pub inputs: Vec<(u32, Meter)>,
    /// マスター（リミッターの後）。
    pub master: Meter,
    /// リミッターが下げた量の最大（dB、0 以下）。
    pub limiter_reduction_db: f32,
}

/// 先読みリミッター。
struct Limiter {
    cfg: LimiterConfig,
    ceiling: f32,
    look: usize,
    delay: VecDeque<[f32; 2]>,
    /// 先読み窓の目標の利得（単調な最小の窓）
    win: VecDeque<(u64, f32)>,
    /// 平滑の箱の中身
    boxq: VecDeque<f32>,
    box_sum: f64,
    gain: f32,
    release: f32,
    n: u64,
}

impl Limiter {
    fn new(cfg: LimiterConfig) -> Self {
        let look = ((cfg.lookahead_ms.clamp(0.1, 20.0) / 1000.0) * OUTPUT_RATE as f32)
            .round()
            .max(1.0) as usize;
        let release =
            1.0 - (-1.0 / (cfg.release_ms.clamp(1.0, 5000.0) / 1000.0 * OUTPUT_RATE as f32)).exp();
        let mut l = Self {
            cfg,
            ceiling: db_to_lin(cfg.ceiling_db.clamp(-40.0, 0.0)),
            look,
            delay: VecDeque::new(),
            win: VecDeque::new(),
            boxq: VecDeque::new(),
            box_sum: 0.0,
            gain: 1.0,
            release,
            n: 0,
        };
        for _ in 0..look - 1 {
            l.delay.push_back([0.0; 2]);
        }
        for _ in 0..look {
            l.boxq.push_back(1.0);
        }
        l.box_sum = look as f64;
        l
    }

    /// 1 標本を処理して (出力, 利得) を返す。出力は先読みぶん遅れる。
    fn process(&mut self, x: [f32; 2]) -> ([f32; 2], f32) {
        if !self.cfg.enabled {
            return (x, 1.0);
        }
        let peak = x[0].abs().max(x[1].abs());
        let target = if peak > self.ceiling {
            self.ceiling / peak
        } else {
            1.0
        };
        // 窓 [n - look + 1, n] の最小
        while self.win.back().is_some_and(|&(_, g)| g >= target) {
            self.win.pop_back();
        }
        self.win.push_back((self.n, target));
        while self
            .win
            .front()
            .is_some_and(|&(i, _)| i + (self.look as u64) <= self.n)
        {
            self.win.pop_front();
        }
        let m = self.win.front().map_or(1.0, |w| w.1);
        // 箱型の平滑
        self.boxq.push_back(m);
        self.box_sum += m as f64;
        if let Some(old) = self.boxq.pop_front() {
            self.box_sum -= old as f64;
        }
        let s = (self.box_sum / self.look as f64) as f32;
        // 箱の平均は窓の最小以上にはならない（重なる標本が全窓に含まれる）。下げるのは即時、戻すのは指数。
        self.gain = if s < self.gain {
            s
        } else {
            self.gain + (s - self.gain) * self.release
        };
        self.n += 1;
        self.delay.push_back(x);
        let y = self.delay.pop_front().unwrap_or([0.0; 2]);
        let g = self.gain;
        let c = self.ceiling;
        ([(y[0] * g).clamp(-c, c), (y[1] * g).clamp(-c, c)], g)
    }
}

/// 音声ミキサー（出力は 48 kHz ステレオ、f32 interleaved）。
pub struct AudioMixer {
    cfg: MixerConfig,
    inputs: BTreeMap<u32, Input>,
    limiter: Limiter,
    master: Meter,
    reduction_db: f32,
}

/// 時刻（µs）→ 出力の標本の位置。
pub fn us_to_pos(us: i64) -> i64 {
    ((us as i128 * OUTPUT_RATE as i128 + 500_000).div_euclid(1_000_000)) as i64
}

/// 出力の標本の位置 → 時刻（µs）。
pub fn pos_to_us(pos: i64) -> i64 {
    ((pos as i128 * 1_000_000).div_euclid(OUTPUT_RATE as i128)) as i64
}

impl AudioMixer {
    pub fn new(cfg: MixerConfig) -> Self {
        Self {
            cfg,
            inputs: BTreeMap::new(),
            limiter: Limiter::new(cfg.limiter),
            master: Meter::default(),
            reduction_db: 0.0,
        }
    }

    /// 出力の遅延（リミッターの先読み、標本数）。
    pub fn latency_frames(&self) -> usize {
        if self.cfg.limiter.enabled {
            self.limiter.look - 1
        } else {
            0
        }
    }

    /// 入力を足す。
    pub fn add_input(
        &mut self,
        id: u32,
        sample_rate: u32,
        channels: usize,
        cfg: InputConfig,
    ) -> Result<()> {
        if !(8000..=192_000).contains(&sample_rate) {
            return Err(ComposeError::InvalidConfig("標本化周波数（8〜192 kHz）"));
        }
        if !(1..=8).contains(&channels) {
            return Err(ComposeError::InvalidConfig("チャネル数（1〜8）"));
        }
        validate_input(&cfg)?;
        if self.inputs.contains_key(&id) {
            return Err(ComposeError::DuplicateInput(id));
        }
        if self.inputs.len() >= 64 {
            return Err(ComposeError::InvalidConfig("入力が多すぎる"));
        }
        self.inputs.insert(
            id,
            Input {
                channels,
                cfg,
                resampler: Resampler::new(sample_rate, OUTPUT_RATE, 2),
                seg_base: None,
                seg_in_frames: 0,
                seg_out_frames: 0,
                seg_delay: 0,
                buf: VecDeque::new(),
                buf_start: 0,
                meter: Meter::default(),
                stats: InputStats::default(),
            },
        );
        Ok(())
    }

    pub fn remove_input(&mut self, id: u32) -> Result<()> {
        self.inputs
            .remove(&id)
            .map(|_| ())
            .ok_or(ComposeError::UnknownInput(id))
    }

    /// 入力の設定を変える（音量・ミュート・パン・ソロ・遅延）。
    pub fn set_input_config(&mut self, id: u32, cfg: InputConfig) -> Result<()> {
        validate_input(&cfg)?;
        self.inputs
            .get_mut(&id)
            .ok_or(ComposeError::UnknownInput(id))?
            .cfg = cfg;
        Ok(())
    }

    pub fn input_config(&self, id: u32) -> Option<InputConfig> {
        self.inputs.get(&id).map(|i| i.cfg)
    }

    pub fn input_stats(&self, id: u32) -> Option<InputStats> {
        self.inputs.get(&id).map(|i| i.stats)
    }

    pub fn set_master_gain_db(&mut self, db: f32) -> Result<()> {
        if !db.is_finite() || !(-120.0..=24.0).contains(&db) {
            return Err(ComposeError::InvalidConfig("マスターの音量"));
        }
        self.cfg.master_gain_db = db;
        Ok(())
    }

    /// 入力の PCM（f32 interleaved、`pts_us` は最初の標本の時刻）を入れる。
    pub fn push(&mut self, id: u32, pts_us: i64, samples: &[f32]) -> Result<()> {
        let max_buf =
            (self.cfg.max_buffer_ms.clamp(10.0, 60_000.0) / 1000.0 * OUTPUT_RATE as f32) as usize;
        let resync = (self.cfg.resync_ms.clamp(0.0, 1000.0) / 1000.0 * OUTPUT_RATE as f32) as i64;
        let inp = self
            .inputs
            .get_mut(&id)
            .ok_or(ComposeError::UnknownInput(id))?;
        let ch = inp.channels;
        if !samples.len().is_multiple_of(ch) {
            return Err(ComposeError::InvalidConfig(
                "標本数がチャネル数の倍数でない",
            ));
        }
        let n = samples.len() / ch;
        if n > inp.resampler.in_rate() as usize * 10 {
            return Err(ComposeError::InvalidConfig("一度に 10 秒を超える"));
        }
        // ステレオにする（壊れた値は 0）。
        let clean = |v: f32| {
            if v.is_finite() {
                v.clamp(-16.0, 16.0)
            } else {
                0.0
            }
        };
        let mut st = Vec::with_capacity(n * 2);
        for f in samples.chunks_exact(ch) {
            let (l, r) = downmix(f, &clean);
            st.push(l);
            st.push(r);
        }
        let raw = us_to_pos(pts_us);
        let delay = (inp.cfg.delay_ms as f64 / 1000.0 * OUTPUT_RATE as f64).round() as i64;
        let in_rate = inp.resampler.in_rate() as f64;
        let continuous = inp.seg_base.is_some_and(|b| {
            let expect =
                b + (inp.seg_in_frames as f64 * OUTPUT_RATE as f64 / in_rate).round() as i64;
            (raw - expect).abs() <= resync && delay == inp.seg_delay
        });
        if !continuous {
            if inp.seg_base.is_some() {
                inp.stats.resyncs += 1;
            }
            inp.seg_base = Some(raw);
            inp.seg_in_frames = 0;
            inp.seg_out_frames = 0;
            inp.seg_delay = delay;
            inp.resampler.reset();
        }
        let mut out = Vec::new();
        inp.resampler.process(&st, &mut out);
        let start = inp.seg_base.unwrap_or(raw) + inp.seg_delay + inp.seg_out_frames as i64;
        inp.seg_in_frames += n as u64;
        inp.seg_out_frames += (out.len() / 2) as u64;
        place(inp, start, &out, max_buf);
        Ok(())
    }

    /// 出力の時刻 `start_us` から `frames` 標本を混ぜる（48 kHz ステレオ interleaved）。
    pub fn mix(&mut self, start_us: i64, frames: usize) -> Vec<f32> {
        self.mix_at(us_to_pos(start_us), frames)
    }

    /// 出力の標本の位置で指定して混ぜる。
    pub fn mix_at(&mut self, start: i64, frames: usize) -> Vec<f32> {
        let frames = frames.min(OUTPUT_RATE as usize * 10);
        let mut acc = vec![[0f32; 2]; frames];
        let any_solo = self.inputs.values().any(|i| i.cfg.solo);
        let mut tmp = vec![[0f32; 2]; frames];
        for inp in self.inputs.values_mut() {
            // start より前を捨てる
            if inp.buf_start < start {
                let drop = ((start - inp.buf_start) as usize).min(inp.buf.len());
                inp.buf.drain(..drop);
                inp.buf_start = start.max(inp.buf_start);
                if inp.buf.is_empty() {
                    inp.buf_start = start;
                }
            }
            let audible = !inp.cfg.mute && (!any_solo || inp.cfg.solo);
            let g = db_to_lin(inp.cfg.gain_db);
            let p = inp.cfg.pan.clamp(-1.0, 1.0);
            let (gl, gr) = (g * (1.0 - p).min(1.0), g * (1.0 + p).min(1.0));
            let off = inp.buf_start - start;
            let mut underrun = 0u64;
            for (k, t) in tmp.iter_mut().enumerate() {
                let idx = k as i64 - off;
                let s = if idx >= 0 && (idx as usize) < inp.buf.len() {
                    inp.buf[idx as usize]
                } else {
                    underrun += 1;
                    [0.0; 2]
                };
                *t = if audible {
                    [s[0] * gl, s[1] * gr]
                } else {
                    [0.0; 2]
                };
            }
            if inp.seg_base.is_some() {
                inp.stats.underrun_frames += underrun;
            }
            inp.meter = measure(&tmp);
            for (a, t) in acc.iter_mut().zip(&tmp) {
                a[0] += t[0];
                a[1] += t[1];
            }
            // 使った分を捨てる
            let end = start + frames as i64;
            if inp.buf_start < end {
                let drop = ((end - inp.buf_start) as usize).min(inp.buf.len());
                inp.buf.drain(..drop);
                inp.buf_start = if inp.buf.is_empty() {
                    end
                } else {
                    inp.buf_start + drop as i64
                };
            }
        }
        let mg = db_to_lin(self.cfg.master_gain_db);
        let mut out = Vec::with_capacity(frames * 2);
        let mut min_gain = 1.0f32;
        let mut post = Vec::with_capacity(frames);
        for a in acc {
            let (y, g) = self.limiter.process([a[0] * mg, a[1] * mg]);
            min_gain = min_gain.min(g);
            post.push(y);
            out.push(y[0]);
            out.push(y[1]);
        }
        self.master = measure(&post);
        self.reduction_db = to_db(min_gain).min(0.0);
        out
    }

    /// 直前の `mix` のメーター。
    pub fn meters(&self) -> MixerMeters {
        MixerMeters {
            inputs: self.inputs.iter().map(|(id, i)| (*id, i.meter)).collect(),
            master: self.master,
            limiter_reduction_db: self.reduction_db,
        }
    }
}

fn validate_input(c: &InputConfig) -> Result<()> {
    if !c.gain_db.is_finite() || !(-120.0..=24.0).contains(&c.gain_db) {
        return Err(ComposeError::InvalidConfig("音量（−120〜24 dB）"));
    }
    if !c.pan.is_finite() || !(-1.0..=1.0).contains(&c.pan) {
        return Err(ComposeError::InvalidConfig("パン（−1〜1）"));
    }
    if !c.delay_ms.is_finite() || !(0.0..=5000.0).contains(&c.delay_ms) {
        return Err(ComposeError::InvalidConfig("遅延（0〜5000 ms）"));
    }
    Ok(())
}

/// 1 フレームをステレオにする。3 チャネル以上は L R C LFE Ls Rs … の並びとみなす。
fn downmix(f: &[f32], clean: &dyn Fn(f32) -> f32) -> (f32, f32) {
    match f.len() {
        1 => (clean(f[0]), clean(f[0])),
        2 => (clean(f[0]), clean(f[1])),
        n => {
            const K: f32 = std::f32::consts::FRAC_1_SQRT_2;
            let mut l = clean(f[0]);
            let mut r = clean(f[1]);
            let c = clean(f[2]) * K;
            l += c;
            r += c;
            // f[3] は LFE（捨てる）。残りは左右交互。
            for (i, &v) in f.iter().enumerate().skip(4).take(n - 4) {
                if i % 2 == 0 {
                    l += clean(v) * K;
                } else {
                    r += clean(v) * K;
                }
            }
            (l, r)
        }
    }
}

/// 再標本化した標本を位置 `start` に置く。
fn place(inp: &mut Input, start: i64, out: &[f32], max_buf: usize) {
    let frames: Vec<[f32; 2]> = out
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| [c[0], c[1]])
        .collect();
    if frames.is_empty() {
        return;
    }
    let end = inp.buf_start + inp.buf.len() as i64;
    if inp.buf.is_empty() {
        inp.buf_start = start;
    } else if start > end {
        let gap = (start - end) as usize;
        if gap > max_buf {
            inp.stats.dropped_frames += inp.buf.len() as u64;
            inp.buf.clear();
            inp.buf_start = start;
        } else {
            inp.stats.gap_frames += gap as u64;
            inp.buf.extend(std::iter::repeat_n([0.0; 2], gap));
        }
    } else if start < end {
        if start <= inp.buf_start {
            inp.stats.dropped_frames += inp.buf.len() as u64;
            inp.buf.clear();
            inp.buf_start = start;
        } else {
            let keep = (start - inp.buf_start) as usize;
            inp.stats.dropped_frames += (inp.buf.len() - keep) as u64;
            inp.buf.truncate(keep);
        }
    }
    inp.buf.extend(frames);
    if inp.buf.len() > max_buf {
        let drop = inp.buf.len() - max_buf;
        inp.buf.drain(..drop);
        inp.buf_start += drop as i64;
        inp.stats.dropped_frames += drop as u64;
    }
}
