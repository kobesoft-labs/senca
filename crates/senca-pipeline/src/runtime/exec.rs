//! 実行の部品: 生のフレームの包み（[`Packet`]）、同じスレッドの中の部品の連なり（[`Chain`]）、
//! 1 入力 1 出力の処理（板書の補正・拡大縮小・切り抜き・変化の検出）と、スレッドごとの実行体
//! （復号・合成・ミキサー・符号化）。
//!
//! # スレッド（REQ-010）
//!
//! - 復号: `decode` の部品ごとに 1 つ。下流の 1 入力の処理（`board`・`scale`・`crop`・`snapshot_on_change`）は
//!   同じスレッドで続けて行う（フレームを渡し直さない）。
//! - 合成: `compose` ごとに 1 つ。描画は帯に分けて並列（`Compositor` の帯のスレッド）。
//! - ミキサー: `audio_mix` ごとに 1 つ。
//! - 符号化: `encode` ごとに 1 つ。
//!
//! スレッドの間は有界の [`Inbox`] でつなぐ（`Arc` のフレームを渡すだけでコピーしない）。

use super::{RtShared, RuntimeOutput, Snapshot};
use crate::clock::{Due, TickScheduler};
use crate::control::Event;
use crate::convert;
use crate::model::{BoardParamsSpec, ClockMode, EncodeNode, RateControlSpec, SnapshotParamsSpec};
use crate::queue::{AuItem, AuQueue, Inbox, Pop};
use bytes::Bytes;
use senca_board::{BoardChangeDetector, BoardCorrector, ChangeParams, DetectParams, Point};
use senca_codec::config::{
    AudioEncoderConfig, DecoderConfig, EncoderConfig, RateControl, VideoEncoderConfig,
};
use senca_codec::frame::{Frame, PixelFormat};
use senca_codec::provider::{Decoder, Encoder};
use senca_compose::{AudioMixer, Compositor, Scene, SourceId};
use senca_core::{Codec, MediaKind, Rescale, Timebase};
use senca_image::{I420Frame, Rect, YuvSpec};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 待つ間隔の上限（止める合図を見る間隔）。
const POLL: Duration = Duration::from_millis(20);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 生のフレーム（時刻はセッション時刻 µs）。
#[derive(Clone, Debug)]
pub enum Packet {
    Video {
        frame: Arc<I420Frame>,
        pts_us: i64,
    },
    Audio {
        samples: Arc<Vec<f32>>,
        rate: u32,
        channels: u8,
        pts_us: i64,
    },
}

impl Packet {
    pub fn pts(&self) -> i64 {
        match self {
            Packet::Video { pts_us, .. } | Packet::Audio { pts_us, .. } => *pts_us,
        }
    }

    /// データの終わりの時刻（音声は長さを足す）。
    pub fn end_us(&self) -> i64 {
        match self {
            Packet::Video { pts_us, .. } => *pts_us,
            Packet::Audio {
                samples,
                rate,
                channels,
                pts_us,
            } => {
                let n = samples.len() / (*channels).max(1) as usize;
                pts_us + (n as i64 * 1_000_000) / (*rate).max(1) as i64
            }
        }
    }
}

/// 生のフレームの受け口。
pub type InboxT = Inbox<Packet>;

/// 出力先。
#[derive(Clone)]
pub enum Target {
    /// 同じスレッドの部品（[`Chain`] の番号）。
    Local(usize),
    /// 別のスレッドの部品の入力。
    Remote {
        inbox: Arc<Inbox<Packet>>,
        idx: usize,
    },
}

/// 1 入力 1 出力の処理。
pub enum Proc {
    /// 根（スレッドの持ち主の出力）。そのまま渡す。
    Root,
    Board(Box<BoardProc>),
    Scale {
        width: usize,
        height: usize,
        filter: senca_image::ScaleFilter,
    },
    Crop {
        rect: Rect,
    },
    Snapshot(Box<SnapshotProc>),
}

pub struct ChainNode {
    pub id: String,
    pub proc: Proc,
    pub outs: Vec<Target>,
}

/// 同じスレッドの部品の連なり。
pub struct Chain {
    pub nodes: Vec<ChainNode>,
}

/// 実行の文脈。
#[derive(Clone)]
pub struct Ctx {
    pub rt: Arc<RtShared>,
    pub generation: u64,
    pub stop: Arc<AtomicBool>,
    pub clock_mode: ClockMode,
    pub latency_us: i64,
    pub missing_timeout_us: i64,
}

impl Ctx {
    pub fn now(&self) -> i64 {
        self.rt.session_now()
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn error(&self, node: &str, code: &str, message: impl Into<String>) {
        self.rt.emit_event(Event::Error {
            node: Some(node.to_string()),
            code: code.to_string(),
            message: message.into(),
        });
    }
}

impl Chain {
    /// `start` の部品に `pkt` を入れて、下流へ流す。
    pub fn run(&mut self, start: usize, pkt: Packet, ctx: &Ctx) {
        let mut stack = vec![(start, pkt)];
        while let Some((i, p)) = stack.pop() {
            let Some(node) = self.nodes.get_mut(i) else {
                continue;
            };
            let out = match &mut node.proc {
                Proc::Root => Some(p),
                Proc::Board(b) => b.process(&node.id, p, ctx),
                Proc::Scale {
                    width,
                    height,
                    filter,
                } => match &p {
                    Packet::Video { frame, pts_us } => {
                        if frame.width() == *width && frame.height() == *height {
                            Some(p)
                        } else {
                            match senca_image::scale::scale_i420(frame, *width, *height, *filter) {
                                Ok(f) => Some(Packet::Video {
                                    frame: Arc::new(f),
                                    pts_us: *pts_us,
                                }),
                                Err(e) => {
                                    ctx.error(&node.id, "INTERNAL_ERROR", format!("scale: {e}"));
                                    None
                                }
                            }
                        }
                    }
                    Packet::Audio { .. } => None,
                },
                Proc::Crop { rect } => match &p {
                    Packet::Video { frame, pts_us } => {
                        // 入力より大きい指定は入力に収める。
                        let x = rect.x.min(frame.width().saturating_sub(2)) & !1;
                        let y = rect.y.min(frame.height().saturating_sub(2)) & !1;
                        let w = rect.width.min(frame.width() - x).max(1);
                        let h = rect.height.min(frame.height() - y).max(1);
                        match senca_image::geom::crop_i420(frame, Rect::new(x, y, w, h)) {
                            Ok(f) => Some(Packet::Video {
                                frame: Arc::new(f),
                                pts_us: *pts_us,
                            }),
                            Err(e) => {
                                ctx.error(&node.id, "INTERNAL_ERROR", format!("crop: {e}"));
                                None
                            }
                        }
                    }
                    Packet::Audio { .. } => None,
                },
                Proc::Snapshot(s) => {
                    s.process(&node.id, &p, ctx);
                    None
                }
            };
            let Some(out) = out else { continue };
            let now = ctx.now();
            for t in &node.outs {
                match t {
                    Target::Local(j) => stack.push((*j, out.clone())),
                    Target::Remote { inbox, idx } => {
                        inbox.push(*idx, out.pts(), out.end_us(), out.clone(), now);
                    }
                }
            }
        }
    }

    /// 下流の別スレッドの入力に終わりを知らせる。
    pub fn end(&self) {
        for n in &self.nodes {
            for t in &n.outs {
                if let Target::Remote { inbox, idx } = t {
                    inbox.end(*idx);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 1 入力 1 出力の処理

/// 板書の補正（自動検出つき）。
pub struct BoardProc {
    spec: BoardParamsSpec,
    corrector: Option<BoardCorrector>,
    found: bool,
    frames: u64,
}

impl BoardProc {
    pub fn new(spec: BoardParamsSpec) -> Result<Self, String> {
        let corrector = if spec.corners.is_some() || !spec.auto_detect {
            Some(
                BoardCorrector::new(convert::board_params(&spec, None))
                    .map_err(|e| e.to_string())?,
            )
        } else {
            None
        };
        Ok(BoardProc {
            found: spec.corners.is_some(),
            spec,
            corrector,
            frames: 0,
        })
    }

    fn process(&mut self, id: &str, p: Packet, ctx: &Ctx) -> Option<Packet> {
        let Packet::Video { frame, pts_us } = p else {
            return None;
        };
        let auto = self.spec.auto_detect && self.spec.corners.is_none();
        let redetect = self.spec.redetect_frames > 0
            && self.frames.is_multiple_of(self.spec.redetect_frames as u64);
        if auto && (!self.found || redetect) {
            match senca_board::detect_board(frame.y(), &DetectParams::default()) {
                Ok(Some(b)) => {
                    match BoardCorrector::new(convert::board_params(&self.spec, Some(b.corners))) {
                        Ok(c) => {
                            self.corrector = Some(c);
                            self.found = true;
                        }
                        Err(e) => ctx.error(id, "INTERNAL_ERROR", format!("board: {e}")),
                    }
                }
                Ok(None) => {}
                Err(e) => ctx.error(id, "INTERNAL_ERROR", format!("board detect: {e}")),
            }
            if self.corrector.is_none() {
                // まだ見つからない: 画面全体を出力の大きさにする（大きさを一定に保つ）。
                let (w, h) = (frame.width() as f64, frame.height() as f64);
                let whole = [
                    Point::new(0.0, 0.0),
                    Point::new(w, 0.0),
                    Point::new(w, h),
                    Point::new(0.0, h),
                ];
                self.corrector =
                    BoardCorrector::new(convert::board_params(&self.spec, Some(whole))).ok();
            }
        }
        self.frames += 1;
        let c = self.corrector.as_ref()?;
        match c.process(&frame) {
            Ok(f) => Some(Packet::Video {
                frame: Arc::new(f),
                pts_us,
            }),
            Err(e) => {
                ctx.error(id, "INTERNAL_ERROR", format!("board: {e}"));
                None
            }
        }
    }
}

/// 板書の変化で静止画を出す。
pub struct SnapshotProc {
    detector: BoardChangeDetector,
    params: SnapshotParamsSpec,
    pub enabled: Arc<AtomicBool>,
    was_enabled: bool,
    last_us: Option<i64>,
    pub fired: Arc<AtomicU64>,
}

impl SnapshotProc {
    pub fn new(params: SnapshotParamsSpec) -> Self {
        let cp = ChangeParams {
            kind: convert::board_kind(params.mode),
            stable_frames: params.stable_frames,
            change_fraction: params.change_fraction,
            ..ChangeParams::default()
        };
        SnapshotProc {
            detector: BoardChangeDetector::new(cp),
            enabled: Arc::new(AtomicBool::new(params.enabled)),
            was_enabled: params.enabled,
            params,
            last_us: None,
            fired: Arc::new(AtomicU64::new(0)),
        }
    }

    fn process(&mut self, id: &str, p: &Packet, ctx: &Ctx) {
        let Packet::Video { frame, pts_us } = p else {
            return;
        };
        let on = self.enabled.load(Ordering::Relaxed);
        if on != self.was_enabled {
            self.was_enabled = on;
            self.detector.reset();
            self.last_us = None;
        }
        if !on {
            return;
        }
        if self.params.analysis_fps > 0.0 {
            let interval = (1_000_000.0 / self.params.analysis_fps as f64) as i64;
            if self.last_us.is_some_and(|l| pts_us - l < interval) {
                return;
            }
        }
        self.last_us = Some(*pts_us);
        let report = match self.detector.push(frame.y()) {
            Ok(r) => r,
            Err(e) => {
                ctx.error(id, "INTERNAL_ERROR", format!("change detection: {e}"));
                return;
            }
        };
        if !report.snapshot {
            return;
        }
        match senca_image::jpeg::encode_jpeg_i420(
            frame,
            YuvSpec::BT709_LIMITED,
            self.params.jpeg_quality,
        ) {
            Ok(jpeg) => {
                self.fired.fetch_add(1, Ordering::Relaxed);
                ctx.rt.emit(RuntimeOutput::Snapshot(Snapshot {
                    node: id.to_string(),
                    generation: ctx.generation,
                    pts_us: *pts_us,
                    width: frame.width() as u32,
                    height: frame.height() as u32,
                    changed_fraction: report.changed_fraction,
                    jpeg: Bytes::from(jpeg),
                }));
            }
            Err(e) => ctx.error(id, "INTERNAL_ERROR", format!("jpeg: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// 復号

/// 復号の統計（入力ごと）。
#[derive(Debug, Default)]
pub struct DecodeStats {
    pub frames: AtomicU64,
    pub errors: AtomicU64,
    pub last_us: AtomicI64,
}

pub struct DecodeExec {
    pub node: String,
    pub source: String,
    pub kind: MediaKind,
    pub queue: Arc<AuQueue>,
    pub config: DecoderConfig,
    pub chain: Chain,
    pub stats: Arc<DecodeStats>,
}

impl DecodeExec {
    pub fn run(mut self, ctx: Ctx) {
        let mut dec: Option<Box<dyn Decoder>> = None;
        let mut need_key = self.kind == MediaKind::Video;
        let mut reported_open_error = false;
        let mut errors_reported = 0u32;
        loop {
            if ctx.stopped() {
                self.queue.close();
                break;
            }
            match self.queue.pop(POLL) {
                Pop::Empty => continue,
                Pop::Ended => {
                    if !ctx.stopped()
                        && let Some(d) = dec.as_mut()
                        && d.flush().is_ok()
                    {
                        self.drain(d.as_mut(), false, &ctx);
                    }
                    break;
                }
                Pop::Item(AuItem::Config(c)) => {
                    let changed = self.config.config.as_ref() != Some(&c);
                    self.config.config = Some(c);
                    if changed && dec.is_some() {
                        // 設定が変わったら作り直す（次の AU から。映像はキーフレームから）。
                        dec = None;
                        need_key = self.kind == MediaKind::Video;
                    }
                }
                Pop::Item(AuItem::Au(au)) => {
                    if need_key && !au.keyframe {
                        continue;
                    }
                    need_key = false;
                    if dec.is_none() {
                        match ctx.rt.registry.create_decoder(&self.config, &ctx.rt.policy) {
                            Ok((_, d)) => {
                                dec = Some(d);
                                reported_open_error = false;
                            }
                            Err(e) => {
                                if !reported_open_error {
                                    ctx.error(&self.node, e.kind.api_code(), e.message.clone());
                                    reported_open_error = true;
                                }
                                need_key = self.kind == MediaKind::Video;
                                continue;
                            }
                        }
                    }
                    let d = dec.as_mut().expect("decoder opened above");
                    if let Err(e) = d.send(&au) {
                        self.stats.errors.fetch_add(1, Ordering::Relaxed);
                        if errors_reported < 10 {
                            errors_reported += 1;
                            ctx.error(
                                &self.node,
                                e.kind.api_code(),
                                format!("decode: {}", e.message),
                            );
                        }
                        // 壊れた所の後はキーフレームから復号し直す。
                        dec = None;
                        need_key = self.kind == MediaKind::Video;
                        continue;
                    }
                    let disc = au.discontinuity;
                    if !self.drain(d.as_mut(), disc, &ctx) {
                        dec = None;
                        need_key = self.kind == MediaKind::Video;
                    }
                }
            }
        }
        self.chain.end();
    }

    /// 復号器からフレームを取り出して下流へ。誤りなら `false`。
    fn drain(&mut self, d: &mut dyn Decoder, disc: bool, ctx: &Ctx) -> bool {
        loop {
            let f = match d.receive() {
                Ok(Some(f)) => f,
                Ok(None) => return true,
                Err(e) => {
                    self.stats.errors.fetch_add(1, Ordering::Relaxed);
                    ctx.error(
                        &self.node,
                        e.kind.api_code(),
                        format!("decode: {}", e.message),
                    );
                    return false;
                }
            };
            let anchor = match ctx.clock_mode {
                ClockMode::Realtime => ctx.now(),
                ClockMode::Input => ctx.rt.media_clock.load(Ordering::Relaxed),
            };
            let pkt = match f {
                Frame::Video(v) => {
                    let us = v.pts.rescale(v.timebase, Timebase::MICROS).unwrap_or(0);
                    let t = ctx.rt.map_time(&self.source, us, anchor, disc);
                    match convert::video_to_i420(&v) {
                        Ok(img) => Packet::Video {
                            frame: Arc::new(img),
                            pts_us: t,
                        },
                        Err(e) => {
                            ctx.error(&self.node, "INTERNAL_ERROR", format!("frame: {e}"));
                            continue;
                        }
                    }
                }
                Frame::Audio(a) => {
                    let us = a.pts.rescale(a.timebase, Timebase::MICROS).unwrap_or(0);
                    let t = ctx.rt.map_time(&self.source, us, anchor, disc);
                    Packet::Audio {
                        samples: Arc::new(convert::audio_to_f32(&a)),
                        rate: a.sample_rate,
                        channels: a.channels,
                        pts_us: t,
                    }
                }
            };
            self.stats.frames.fetch_add(1, Ordering::Relaxed);
            self.stats.last_us.store(pkt.pts(), Ordering::Relaxed);
            self.chain.run(0, pkt, ctx);
        }
    }
}

// ---------------------------------------------------------------------------
// 合成

/// 合成の運用の状態（操作で変え、合成のスレッドが次の刻みで反映する）。
pub struct ComposeCtl {
    pub names: convert::ComposeNames,
    pub specs: BTreeMap<String, crate::model::SceneSpec>,
    pub scenes: BTreeMap<String, Scene>,
    pub program: String,
    pub preview: Option<String>,
    pub default_transition: crate::model::TransitionSpec,
    pub pending: Vec<ComposeCmd>,
    // 状態
    pub in_transition: bool,
    pub frames: u64,
    pub skipped: u64,
    pub missing: Vec<String>,
}

pub enum ComposeCmd {
    /// プログラムを替える（トランジション付き）。
    Program(Scene, senca_compose::Transition),
    /// 今のプログラムのシーンの中身が変わった。
    ProgramEdited(Scene),
}

impl ComposeCtl {
    /// シーンに見えている入力の名前。
    pub fn visible_inputs(&self, scene: &str) -> Vec<String> {
        let Some(s) = self.specs.get(scene) else {
            return vec![];
        };
        let mut v: Vec<String> = s
            .layers
            .iter()
            .filter(|l| l.visible && l.opacity > 0.0)
            .filter_map(|l| l.source.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

pub struct ComposeExec {
    pub node: String,
    pub inbox: Arc<Inbox<Packet>>,
    pub compositor: Compositor,
    pub ctl: Arc<Mutex<ComposeCtl>>,
    pub sched: TickScheduler,
    pub chain: Chain,
    /// プレビューの根（`preview` ポートが使われているとき）。
    pub preview_root: Option<usize>,
}

impl ComposeExec {
    pub fn run(mut self, ctx: Ctx) {
        let n_inputs = self.inbox.inputs();
        let mut seen = self.inbox.seq();
        // 入力時刻のモードでは、最初の刻みを最初のフレームに合わせる（途中から始まる世代がスレートから始まらない）。
        let mut aligned = ctx.clock_mode != ClockMode::Input;
        loop {
            if ctx.stopped() {
                self.inbox.close();
                break;
            }
            let now = ctx.now();
            let states = self.inbox.states();
            match self.sched.poll(now, &states) {
                Due::Tick { t_us, skipped } => {
                    if !aligned {
                        aligned = true;
                        let first = (0..n_inputs).filter_map(|i| self.inbox.first_pts(i)).min();
                        if let Some(f) = first
                            && t_us < f - self.sched.period_us()
                        {
                            self.sched.skip_to(f);
                            continue;
                        }
                    }
                    // 刻みの半分までの揺らぎ（時刻の丸めなど）は同じ刻みのフレームとみなす。
                    let tol = self.sched.period_us() / 2;
                    for i in 0..n_inputs {
                        for (pts, p) in self.inbox.drain_until(i, t_us + tol) {
                            if let Packet::Video { frame, .. } = p {
                                self.compositor.push_frame(
                                    SourceId(i as u32),
                                    frame,
                                    pts.min(t_us),
                                );
                            }
                        }
                    }
                    self.tick(t_us, skipped, &ctx);
                }
                Due::WaitUntil(t) => {
                    let wait =
                        Duration::from_micros((t - now).clamp(0, POLL.as_micros() as i64) as u64);
                    std::thread::sleep(wait);
                }
                Due::WaitInput(t) => {
                    let wait = Duration::from_micros(
                        (t - now).clamp(1000, POLL.as_micros() as i64) as u64,
                    );
                    seen = self.inbox.wait_change(seen, wait);
                }
                Due::Done => break,
            }
        }
        self.chain.end();
    }

    fn tick(&mut self, t: i64, skipped: u64, ctx: &Ctx) {
        let (cmds, preview) = {
            let mut c = lock(&self.ctl);
            c.skipped += skipped;
            let preview = if self.preview_root.is_some() {
                c.preview.as_ref().and_then(|p| c.scenes.get(p)).cloned()
            } else {
                None
            };
            (std::mem::take(&mut c.pending), preview)
        };
        for cmd in cmds {
            let r = match cmd {
                ComposeCmd::Program(s, tr) => self.compositor.transition_to(s, tr, t),
                ComposeCmd::ProgramEdited(s) => self.compositor.set_program(s),
            };
            if let Err(e) = r {
                ctx.error(&self.node, "INVALID_CONFIG", format!("compose: {e}"));
            }
        }
        match self.compositor.render(t) {
            Ok(f) => {
                ctx.rt.media_clock.fetch_max(t, Ordering::Relaxed);
                self.chain.run(
                    0,
                    Packet::Video {
                        frame: Arc::new(f),
                        pts_us: t,
                    },
                    ctx,
                );
            }
            Err(e) => ctx.error(&self.node, "INTERNAL_ERROR", format!("render: {e}")),
        }
        if let (Some(root), Some(scene)) = (self.preview_root, preview) {
            match self.compositor.render_scene(&scene, t) {
                Ok(f) => self.chain.run(
                    root,
                    Packet::Video {
                        frame: Arc::new(f),
                        pts_us: t,
                    },
                    ctx,
                ),
                Err(e) => ctx.error(&self.node, "INTERNAL_ERROR", format!("preview: {e}")),
            }
        }
        let missing: Vec<String> = {
            let c = lock(&self.ctl);
            self.compositor
                .last_missing()
                .iter()
                .filter_map(|s| c.names.inputs.get(s.0 as usize).cloned())
                .collect()
        };
        let mut c = lock(&self.ctl);
        c.frames += 1;
        c.in_transition = self.compositor.in_transition();
        c.missing = missing;
    }
}

// ---------------------------------------------------------------------------
// ミキサー

/// ミキサーの運用の状態。
pub struct MixCtl {
    pub names: Vec<String>,
    pub configs: Vec<senca_compose::InputConfig>,
    pub master_gain_db: f32,
    pub dirty: bool,
    pub meters: Option<senca_compose::MixerMeters>,
    pub frames: u64,
}

pub struct MixExec {
    pub node: String,
    pub inbox: Arc<Inbox<Packet>>,
    pub mixer: AudioMixer,
    pub ctl: Arc<Mutex<MixCtl>>,
    pub sched: TickScheduler,
    pub chain: Chain,
    pub added: Vec<bool>,
}

/// ミキサーの 1 回の長さ（20 ms）。
pub const MIX_FRAMES: u32 = 960;

impl MixExec {
    pub fn run(mut self, ctx: Ctx) {
        let n = self.inbox.inputs();
        let mut seen = self.inbox.seq();
        let mut aligned = ctx.clock_mode != ClockMode::Input;
        loop {
            if ctx.stopped() {
                self.inbox.close();
                break;
            }
            let now = ctx.now();
            let states = self.inbox.states();
            match self.sched.poll(now, &states) {
                Due::Tick { t_us, .. } => {
                    if !aligned {
                        aligned = true;
                        let first = (0..n).filter_map(|i| self.inbox.first_pts(i)).min();
                        if let Some(f) = first
                            && t_us < f - self.sched.period_us()
                        {
                            self.sched.skip_to(f - self.sched.period_us() + 1);
                            continue;
                        }
                    }
                    // この刻みの窓（t .. t + 20 ms）に始まるものまで入れる。先のものは受け口に残す
                    // （ミキサーの貯めすぎを防ぎ、上流に背圧をかける）。
                    let window_end = t_us + self.sched.period_us();
                    for i in 0..n {
                        for (pts, p) in self.inbox.drain_until(i, window_end - 1) {
                            self.push(i, pts, p, &ctx);
                        }
                    }
                    self.apply_controls(&ctx);
                    let out = self.mixer.mix(t_us, MIX_FRAMES as usize);
                    {
                        let mut c = lock(&self.ctl);
                        c.meters = Some(self.mixer.meters());
                        c.frames += 1;
                    }
                    self.chain.run(
                        0,
                        Packet::Audio {
                            samples: Arc::new(out),
                            rate: senca_compose::OUTPUT_RATE,
                            channels: 2,
                            pts_us: t_us,
                        },
                        &ctx,
                    );
                }
                Due::WaitUntil(t) => {
                    let wait =
                        Duration::from_micros((t - now).clamp(0, POLL.as_micros() as i64) as u64);
                    std::thread::sleep(wait);
                }
                Due::WaitInput(t) => {
                    let wait = Duration::from_micros(
                        (t - now).clamp(1000, POLL.as_micros() as i64) as u64,
                    );
                    seen = self.inbox.wait_change(seen, wait);
                }
                Due::Done => break,
            }
        }
        self.chain.end();
    }

    fn push(&mut self, i: usize, pts: i64, p: Packet, ctx: &Ctx) {
        let Packet::Audio {
            samples,
            rate,
            channels,
            ..
        } = p
        else {
            return;
        };
        if !self.added[i] {
            let cfg = lock(&self.ctl).configs[i];
            if let Err(e) = self.mixer.add_input(i as u32, rate, channels as usize, cfg) {
                ctx.error(&self.node, "INVALID_CONFIG", format!("mixer input: {e}"));
                return;
            }
            self.added[i] = true;
        }
        if let Err(e) = self.mixer.push(i as u32, pts, &samples) {
            ctx.error(&self.node, "INTERNAL_ERROR", format!("mixer: {e}"));
        }
    }

    fn apply_controls(&mut self, ctx: &Ctx) {
        let mut c = lock(&self.ctl);
        if !c.dirty {
            return;
        }
        c.dirty = false;
        for (i, cfg) in c.configs.iter().enumerate() {
            if self.added[i]
                && let Err(e) = self.mixer.set_input_config(i as u32, *cfg)
            {
                ctx.error(&self.node, "INVALID_CONFIG", format!("mixer: {e}"));
            }
        }
        if let Err(e) = self.mixer.set_master_gain_db(c.master_gain_db) {
            ctx.error(&self.node, "INVALID_CONFIG", format!("mixer: {e}"));
        }
    }
}

// ---------------------------------------------------------------------------
// 符号化

/// 符号化の統計。
#[derive(Debug, Default)]
pub struct EncodeStats {
    pub frames_in: AtomicU64,
    pub aus_out: AtomicU64,
    pub errors: AtomicU64,
}

/// 符号化の出力先（sink とトラック）。
#[derive(Debug, Clone)]
pub struct SinkTarget {
    pub sink: String,
    pub stream: String,
    pub track: u32,
}

pub struct EncodeExec {
    pub node: String,
    pub spec: EncodeNode,
    pub codec: Codec,
    /// 上流の fps（分かるとき）。
    pub fps: Option<f64>,
    pub inbox: Arc<Inbox<Packet>>,
    pub keyframe: Arc<AtomicBool>,
    pub targets: Vec<SinkTarget>,
    pub stats: Arc<EncodeStats>,
    /// 準備の時に開いた符号化器（大きさが分かっていたとき）。
    pub encoder: Option<(EncoderConfig, Box<dyn Encoder>)>,
}

/// 符号化の設定を作る。映像の大きさが分からなければ `None`。
pub fn encoder_config(
    spec: &EncodeNode,
    codec: Codec,
    fps: Option<f64>,
    size: Option<(u32, u32)>,
    audio: Option<(u32, u8)>,
) -> Option<EncoderConfig> {
    if codec.kind() == MediaKind::Video {
        let (w, h) = match (spec.width, spec.height) {
            (Some(w), Some(h)) => (w, h),
            _ => size?,
        };
        let fps = fps.unwrap_or(30.0).clamp(1.0, 240.0);
        let (fps_num, fps_den) = if (fps - fps.round()).abs() < 1e-6 {
            (fps.round() as u32, 1)
        } else {
            ((fps * 1001.0).round() as u32, 1001)
        };
        let mut c = VideoEncoderConfig::new(
            codec,
            w,
            h,
            fps.round() as u32,
            spec.bitrate_kbps.saturating_mul(1000),
        );
        c.fps_num = fps_num;
        c.fps_den = fps_den;
        c.gop = spec.gop.unwrap_or((fps * 2.0).round() as u32);
        c.low_latency = spec.low_latency;
        c.rate_control = match spec.rate_control {
            RateControlSpec::Cbr => RateControl::Cbr,
            RateControlSpec::Vbr => RateControl::Vbr,
        };
        c.threads = spec.threads;
        c.input_format = PixelFormat::I420;
        c.timebase = Timebase::MPEG;
        Some(EncoderConfig::Video(c))
    } else {
        let (rate, ch) = audio.unwrap_or((48000, 2));
        let mut c =
            AudioEncoderConfig::opus(rate, ch.clamp(1, 2), spec.bitrate_kbps.saturating_mul(1000));
        c.codec = codec;
        if let Some(f) = spec.frame_ms {
            c.frame_duration_us = (f * 1000.0) as u32;
        }
        Some(EncoderConfig::Audio(c))
    }
}

impl EncodeExec {
    pub fn run(mut self, ctx: Ctx) {
        let mut reported_open_error = false;
        loop {
            if ctx.stopped() {
                self.inbox.close();
                break;
            }
            match self.inbox.pop(POLL) {
                Pop::Empty => continue,
                Pop::Ended => {
                    if !ctx.stopped()
                        && let Some((_, e)) = self.encoder.as_mut()
                    {
                        let mut aus = Vec::new();
                        if e.flush().is_ok() {
                            while let Ok(Some(au)) = e.receive() {
                                aus.push(au);
                            }
                        }
                        self.emit(aus, &ctx);
                    }
                    break;
                }
                Pop::Item((_, _, p)) => {
                    self.stats.frames_in.fetch_add(1, Ordering::Relaxed);
                    if self.encoder.is_none() {
                        let size = match &p {
                            Packet::Video { frame, .. } => {
                                Some((frame.width() as u32, frame.height() as u32))
                            }
                            _ => None,
                        };
                        let audio = match &p {
                            Packet::Audio { rate, channels, .. } => Some((*rate, *channels)),
                            _ => None,
                        };
                        let Some(cfg) =
                            encoder_config(&self.spec, self.codec, self.fps, size, audio)
                        else {
                            continue;
                        };
                        match ctx.rt.registry.create_encoder(&cfg, &ctx.rt.policy) {
                            Ok((_, e)) => self.encoder = Some((cfg, e)),
                            Err(e) => {
                                self.stats.errors.fetch_add(1, Ordering::Relaxed);
                                if !reported_open_error {
                                    reported_open_error = true;
                                    ctx.error(&self.node, e.kind.api_code(), e.message);
                                }
                                continue;
                            }
                        }
                    }
                    if let Err(m) = self.encode(p, &ctx) {
                        self.stats.errors.fetch_add(1, Ordering::Relaxed);
                        ctx.error(&self.node, "INTERNAL_ERROR", m);
                    }
                }
            }
        }
    }

    fn encode(&mut self, p: Packet, ctx: &Ctx) -> Result<(), String> {
        let (cfg, enc) = self.encoder.as_mut().expect("encoder opened");
        let frame = match (p, &*cfg) {
            (Packet::Video { frame, pts_us }, EncoderConfig::Video(c)) => {
                let img =
                    if frame.width() != c.width as usize || frame.height() != c.height as usize {
                        Arc::new(
                            senca_image::scale::scale_i420(
                                &frame,
                                c.width as usize,
                                c.height as usize,
                                senca_image::ScaleFilter::Bilinear,
                            )
                            .map_err(|e| format!("scale: {e}"))?,
                        )
                    } else {
                        frame
                    };
                let pts = pts_us
                    .rescale(Timebase::MICROS, Timebase::MPEG)
                    .ok_or("pts out of range")?;
                let mut v = convert::i420_to_video(&img, pts, Timebase::MPEG)?;
                if self.keyframe.swap(false, Ordering::Relaxed) {
                    enc.request_keyframe();
                    v.keyframe = true;
                }
                Frame::Video(v)
            }
            (
                Packet::Audio {
                    samples,
                    rate,
                    channels,
                    pts_us,
                },
                EncoderConfig::Audio(c),
            ) => {
                if rate != c.sample_rate {
                    return Err(format!(
                        "audio rate {rate} does not match encoder {}",
                        c.sample_rate
                    ));
                }
                let samples: Vec<f32> = if channels == c.channels {
                    samples.to_vec()
                } else if channels == 1 && c.channels == 2 {
                    samples.iter().flat_map(|v| [*v, *v]).collect()
                } else {
                    // 2 以上 → 先頭の c.channels を取る（Opus は 2 まで）。
                    samples
                        .chunks_exact(channels.max(1) as usize)
                        .flat_map(|f| f.iter().take(c.channels as usize).copied())
                        .collect()
                };
                let pts = pts_us
                    .rescale(Timebase::MICROS, Timebase::new(1, rate))
                    .ok_or("pts out of range")?;
                Frame::Audio(convert::f32_to_audio(&samples, rate, c.channels, pts))
            }
            _ => return Err("packet kind does not match encoder".into()),
        };
        enc.send(&frame)
            .map_err(|e| format!("encode: {}", e.message))?;
        let mut aus = Vec::new();
        loop {
            match enc.receive() {
                Ok(Some(au)) => aus.push(au),
                Ok(None) => break,
                Err(e) => return Err(format!("encode: {}", e.message)),
            }
        }
        self.emit(aus, ctx);
        Ok(())
    }

    fn emit(&mut self, aus: Vec<senca_core::AccessUnit>, ctx: &Ctx) {
        if aus.is_empty() {
            return;
        }
        let config = self.encoder.as_ref().and_then(|(_, e)| e.codec_config());
        self.stats
            .aus_out
            .fetch_add(aus.len() as u64, Ordering::Relaxed);
        for t in &self.targets {
            let want_key = ctx.rt.route(ctx.generation, t, config.clone(), aus.clone());
            if want_key {
                self.keyframe.store(true, Ordering::Relaxed);
            }
        }
    }
}
