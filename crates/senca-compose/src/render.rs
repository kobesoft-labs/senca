//! 映像の合成（R6-7・R8-1〜R8-5）。
//!
//! - ソースごとに最新のフレーム（とその時刻）を持ち、出力の時刻に「その時刻以前で最新」のフレームを使う
//!   （フレームレートの変換は「前のフレームを保持」）。一定時間フレームが来ない・切れたと知らされた
//!   ソースは、スレート（色または画像）に置き換える（AC-R8-03）。
//! - レイヤーは奥から順に、拡大縮小（切り抜き込み）→ 角丸・枠・不透明度・画像のアルファで重ねる。
//! - 出力を横の帯に分け、`std::thread::scope` で帯ごとに並列に描く（REQ-010）。各帯は独立に全レイヤーを描く。
//! - トランジション: カット、クロスフェード、ワイプ（左→右、右→左）。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use senca_image::frame::chroma_size;
use senca_image::scale::I420Scaler;
use senca_image::{I420Frame, Plane, RectF, RgbaImage, YuvColor, YuvSpec};

use crate::error::{ComposeError, Result};
use crate::scene::{Border, Canvas, FitMode, ImageId, Layer, LayerContent, Scene, SourceId};

/// 重ねる画像（ロゴなど）。YUV とアルファ（輝度・色差の両方の大きさ）で持つ。
#[derive(Debug, Clone)]
pub struct OverlayImage {
    frame: I420Frame,
    alpha: Option<(Plane, Plane)>,
}

impl OverlayImage {
    /// RGBA（straight alpha）から作る。アルファがすべて 255 なら不透明として扱う。
    pub fn from_rgba(img: &RgbaImage, spec: YuvSpec) -> Self {
        let mut frame = senca_image::color::rgba_to_i420(img, spec);
        let a = img.alpha_plane();
        let opaque = a.to_packed().iter().all(|&v| v == 255);
        if opaque {
            return Self { frame, alpha: None };
        }
        // 色差はアルファで重み付けした平均にする（縁に透明部分の色がにじまないように）。
        let (w, h) = (img.width(), img.height());
        let (cw, ch) = chroma_size(w, h);
        let c = senca_image::color::rgba_to_i420(img, spec);
        let (_, u, v) = frame.planes_mut();
        for cy in 0..ch {
            for cx in 0..cw {
                let (mut su, mut sv, mut sa) = (0u32, 0u32, 0u32);
                for (x, y) in [
                    (2 * cx, 2 * cy),
                    (2 * cx + 1, 2 * cy),
                    (2 * cx, 2 * cy + 1),
                    (2 * cx + 1, 2 * cy + 1),
                ] {
                    let (x, y) = (x.min(w - 1), y.min(h - 1));
                    let p = img.pixel(x, y);
                    let al = p[3] as u32;
                    let yuv = spec.rgb_to_yuv(p[0], p[1], p[2]);
                    su += yuv.u as u32 * al;
                    sv += yuv.v as u32 * al;
                    sa += al;
                }
                if let (Some(uu), Some(vv)) =
                    ((su + sa / 2).checked_div(sa), (sv + sa / 2).checked_div(sa))
                {
                    u.set(cx, cy, uu as u8);
                    v.set(cx, cy, vv as u8);
                } else {
                    u.set(cx, cy, c.u().get(cx, cy));
                    v.set(cx, cy, c.v().get(cx, cy));
                }
            }
        }
        let ac = senca_image::color::downsample_chroma_420(&a);
        Self {
            frame,
            alpha: Some((a, ac)),
        }
    }

    /// 不透明な I420 から作る。
    pub fn from_i420(f: I420Frame) -> Self {
        Self {
            frame: f,
            alpha: None,
        }
    }

    pub fn width(&self) -> usize {
        self.frame.width()
    }
    pub fn height(&self) -> usize {
        self.frame.height()
    }
}

/// ソースが無い・古いときに出すもの。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Slate {
    Color(YuvColor),
    Image(ImageId),
}

/// ソースの扱いの方針。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourcePolicy {
    /// 最後のフレームを保持する時間（µs）。超えたらスレート。
    pub hold_timeout_us: i64,
    pub slate: Slate,
}

impl Default for SourcePolicy {
    fn default() -> Self {
        Self {
            hold_timeout_us: 2_000_000,
            slate: Slate::Color(YuvColor::BLACK_LIMITED),
        }
    }
}

#[derive(Debug, Default)]
struct SourceSlot {
    frames: VecDeque<(i64, Arc<I420Frame>)>,
    missing: bool,
}

/// トランジションの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionKind {
    Cut,
    /// クロスフェード。
    Fade,
    /// 左から右へ新しいシーンが現れる。
    WipeLeftToRight,
    /// 右から左へ。
    WipeRightToLeft,
}

/// トランジション（種類と長さ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub kind: TransitionKind,
    pub duration_ms: u32,
}

#[derive(Debug, Clone)]
struct ActiveTransition {
    from: Scene,
    kind: TransitionKind,
    start_us: i64,
    duration_us: i64,
}

/// 合成器。
pub struct Compositor {
    canvas: Canvas,
    threads: usize,
    sources: HashMap<SourceId, SourceSlot>,
    images: HashMap<ImageId, Arc<OverlayImage>>,
    policy: SourcePolicy,
    program: Scene,
    transition: Option<ActiveTransition>,
    last_missing: Vec<SourceId>,
}

/// 準備済みのレイヤー（1 回の描画の間だけ使う）。
struct Prepared<'a> {
    kind: PrepKind<'a>,
    /// 内容の矩形（輝度、偶数に揃えた位置と大きさ）。
    x: i64,
    y: i64,
    w: usize,
    h: usize,
    opacity: f32,
    radius: f32,
    border: Option<Border>,
}

enum PrepKind<'a> {
    Color(YuvColor),
    Picture {
        frame: &'a I420Frame,
        alpha: Option<&'a (Plane, Plane)>,
        scaler: Box<I420Scaler>,
    },
}

impl Compositor {
    /// `threads` は帯の数（並列度）。0 なら使える CPU の数。
    pub fn new(canvas: Canvas, threads: usize) -> Result<Self> {
        canvas.validate()?;
        let threads = if threads == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        } else {
            threads
        };
        Ok(Self {
            canvas,
            threads: threads.clamp(1, 64),
            sources: HashMap::new(),
            images: HashMap::new(),
            policy: SourcePolicy::default(),
            program: Scene::default(),
            transition: None,
            last_missing: Vec::new(),
        })
    }

    pub fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    pub fn set_threads(&mut self, n: usize) {
        self.threads = n.clamp(1, 64);
    }

    pub fn set_policy(&mut self, p: SourcePolicy) {
        self.policy = p;
    }

    /// 画像を登録する（同じ番号なら置き換え）。
    pub fn set_image(&mut self, id: ImageId, img: OverlayImage) {
        self.images.insert(id, Arc::new(img));
    }

    pub fn remove_image(&mut self, id: ImageId) {
        self.images.remove(&id);
    }

    /// ソースのフレームを入れる（時刻 µs）。少し先の時刻のフレームも 4 枚まで待たせておける。
    pub fn push_frame(&mut self, id: SourceId, frame: Arc<I420Frame>, pts_us: i64) {
        let slot = self.sources.entry(id).or_default();
        slot.missing = false;
        // 時刻が戻ったら（ソースの再起動など）古いものを捨てる。
        if slot.frames.back().is_some_and(|(p, _)| *p >= pts_us) {
            slot.frames.clear();
        }
        slot.frames.push_back((pts_us, frame));
        while slot.frames.len() > 4 {
            slot.frames.pop_front();
        }
    }

    /// ソースが切れたと知らせる（すぐにスレートにする）。
    pub fn mark_missing(&mut self, id: SourceId) {
        let slot = self.sources.entry(id).or_default();
        slot.missing = true;
        slot.frames.clear();
    }

    pub fn remove_source(&mut self, id: SourceId) {
        self.sources.remove(&id);
    }

    /// 直前の描画でスレートになったソース。
    pub fn last_missing(&self) -> &[SourceId] {
        &self.last_missing
    }

    /// プログラム（出力）のシーンを直ちに切り替える（カット）。
    pub fn set_program(&mut self, scene: Scene) -> Result<()> {
        scene.validate()?;
        self.program = scene;
        self.transition = None;
        Ok(())
    }

    pub fn program(&self) -> &Scene {
        &self.program
    }

    /// トランジションつきで切り替える（`now_us` から始まる）。
    pub fn transition_to(&mut self, scene: Scene, t: Transition, now_us: i64) -> Result<()> {
        scene.validate()?;
        if t.kind == TransitionKind::Cut || t.duration_ms == 0 {
            return self.set_program(scene);
        }
        if t.duration_ms > 60_000 {
            return Err(ComposeError::InvalidConfig("トランジションが長すぎる"));
        }
        let from = std::mem::replace(&mut self.program, scene);
        self.transition = Some(ActiveTransition {
            from,
            kind: t.kind,
            start_us: now_us,
            duration_us: t.duration_ms as i64 * 1000,
        });
        Ok(())
    }

    /// トランジションの途中か。
    pub fn in_transition(&self) -> bool {
        self.transition.is_some()
    }

    /// 時刻 `t_us` のソースのフレームを選ぶ（古いものは捨てる）。
    fn select_frames(&mut self, t_us: i64) -> HashMap<SourceId, Arc<I420Frame>> {
        let mut out = HashMap::new();
        for (id, slot) in self.sources.iter_mut() {
            if slot.missing {
                continue;
            }
            // t 以前で最新のものを残し、それより古いものを捨てる。
            while slot.frames.len() > 1 && slot.frames[1].0 <= t_us {
                slot.frames.pop_front();
            }
            if let Some((pts, f)) = slot.frames.front()
                && t_us.saturating_sub(*pts) <= self.policy.hold_timeout_us
            {
                out.insert(*id, f.clone());
            }
        }
        out
    }

    /// 出力の時刻 `t_us` の 1 フレームを描く（プログラム＋トランジション）。
    pub fn render(&mut self, t_us: i64) -> Result<I420Frame> {
        let frames = self.select_frames(t_us);
        let mut missing = Vec::new();
        let to = self.render_with(&self.program, &frames, &mut missing)?;
        let out = if let Some(tr) = &self.transition {
            let p = (t_us.saturating_sub(tr.start_us) as f64 / tr.duration_us as f64)
                .clamp(0.0, 1.0) as f32;
            if p >= 1.0 {
                self.transition = None;
                to
            } else {
                let from = self.render_with(&tr.from, &frames, &mut missing)?;
                self.mix(&from, &to, tr.kind, p)?
            }
        } else {
            to
        };
        missing.sort();
        missing.dedup();
        self.last_missing = missing;
        Ok(out)
    }

    /// 任意のシーンを描く（プレビュー用。プログラムの状態は変えない）。
    pub fn render_scene(&mut self, scene: &Scene, t_us: i64) -> Result<I420Frame> {
        scene.validate()?;
        let frames = self.select_frames(t_us);
        let mut missing = Vec::new();
        self.render_with(scene, &frames, &mut missing)
    }

    fn render_with(
        &self,
        scene: &Scene,
        frames: &HashMap<SourceId, Arc<I420Frame>>,
        missing: &mut Vec<SourceId>,
    ) -> Result<I420Frame> {
        let mut prepared = Vec::new();
        for l in scene.ordered() {
            if let Some(p) = self.prepare(l, frames, missing)? {
                prepared.push(p);
            }
        }
        let mut out = I420Frame::new(
            self.canvas.width,
            self.canvas.height,
            self.canvas.background,
        )?;
        self.run_bands(&mut out, |y0, rows, yb, ub, vb, w| {
            render_band(&prepared, self.canvas.background, y0, rows, yb, ub, vb, w);
        });
        Ok(out)
    }

    /// 出力を帯に分けて並列に処理する。`f(先頭の行, 行数, Y, U, V, 幅)`。
    fn run_bands<F>(&self, out: &mut I420Frame, f: F)
    where
        F: Fn(usize, usize, &mut [u8], &mut [u8], &mut [u8], usize) + Sync,
    {
        let (w, h) = (self.canvas.width, self.canvas.height);
        let bands = self.threads.min(h / 16).max(1);
        let band_h = h.div_ceil(bands).next_multiple_of(2);
        let (yp, up, vp) = out.planes_mut();
        let (ys, us, vs) = (yp.data_mut(), up.data_mut(), vp.data_mut());
        if bands == 1 {
            f(0, h, ys, us, vs, w);
            return;
        }
        let cw = w / 2;
        std::thread::scope(|s| {
            let parts = ys
                .chunks_mut(band_h * w)
                .zip(us.chunks_mut(band_h / 2 * cw))
                .zip(vs.chunks_mut(band_h / 2 * cw));
            for (i, ((yb, ub), vb)) in parts.enumerate() {
                let f = &f;
                let rows = yb.len() / w;
                s.spawn(move || f(i * band_h, rows, yb, ub, vb, w));
            }
        });
    }

    fn mix(&self, a: &I420Frame, b: &I420Frame, kind: TransitionKind, p: f32) -> Result<I420Frame> {
        let mut out = a.clone();
        let w = self.canvas.width;
        let wipe_x = (p * w as f32).round() as usize;
        let k = (p * 256.0).round() as u32;
        self.run_bands(&mut out, |y0, rows, yb, ub, vb, w| {
            let cw = w / 2;
            let blend = |dst: &mut [u8], src_b: &[u8], width: usize, xcut: usize| match kind {
                TransitionKind::Fade => {
                    for (d, &s) in dst.iter_mut().zip(src_b) {
                        *d = ((*d as u32 * (256 - k) + s as u32 * k + 128) >> 8) as u8;
                    }
                }
                TransitionKind::WipeLeftToRight => {
                    for (row_d, row_s) in dst.chunks_mut(width).zip(src_b.chunks(width)) {
                        row_d[..xcut].copy_from_slice(&row_s[..xcut]);
                    }
                }
                TransitionKind::WipeRightToLeft => {
                    for (row_d, row_s) in dst.chunks_mut(width).zip(src_b.chunks(width)) {
                        row_d[width - xcut..].copy_from_slice(&row_s[width - xcut..]);
                    }
                }
                TransitionKind::Cut => dst.copy_from_slice(src_b),
            };
            let bys = &b.y().data()[y0 * w..(y0 + rows) * w];
            blend(yb, bys, w, wipe_x.min(w));
            let (c0, cr) = (y0 / 2, rows / 2);
            blend(
                ub,
                &b.u().data()[c0 * cw..(c0 + cr) * cw],
                cw,
                (wipe_x / 2).min(cw),
            );
            blend(
                vb,
                &b.v().data()[c0 * cw..(c0 + cr) * cw],
                cw,
                (wipe_x / 2).min(cw),
            );
        });
        Ok(out)
    }

    fn prepare<'a>(
        &'a self,
        l: &Layer,
        frames: &'a HashMap<SourceId, Arc<I420Frame>>,
        missing: &mut Vec<SourceId>,
    ) -> Result<Option<Prepared<'a>>> {
        if l.dest.width < 2 || l.dest.height < 2 || l.opacity <= 0.0 {
            return Ok(None);
        }
        enum Src<'b> {
            Color(YuvColor),
            Pic(&'b I420Frame, Option<&'b (Plane, Plane)>),
        }
        let src = match l.content {
            LayerContent::Color(c) => Src::Color(c),
            LayerContent::Image(id) => match self.images.get(&id) {
                Some(img) => Src::Pic(&img.frame, img.alpha.as_ref()),
                None => return Ok(None),
            },
            LayerContent::Video(id) => match frames.get(&id) {
                Some(f) => Src::Pic(f.as_ref(), None),
                None => {
                    missing.push(id);
                    match self.policy.slate {
                        Slate::Color(c) => Src::Color(c),
                        Slate::Image(iid) => match self.images.get(&iid) {
                            Some(img) => Src::Pic(&img.frame, img.alpha.as_ref()),
                            None => Src::Color(YuvColor::BLACK_LIMITED),
                        },
                    }
                }
            },
        };
        let (dx, dy, dw, dh) = (
            l.dest.x as f64,
            l.dest.y as f64,
            l.dest.width as f64,
            l.dest.height as f64,
        );
        let even_down = |v: f64| (v.floor() as i64) & !1;
        let even_up = |v: f64| (((v.round() as i64).max(2) + 1) & !1) as usize;
        match src {
            Src::Color(c) => Ok(Some(Prepared {
                kind: PrepKind::Color(c),
                x: even_down(dx),
                y: even_down(dy),
                w: even_up(dw),
                h: even_up(dh),
                opacity: l.opacity,
                radius: l.corner_radius as f32,
                border: l.border,
            })),
            Src::Pic(f, alpha) => {
                let (sw, sh) = (f.width() as f64, f.height() as f64);
                let c = l.crop;
                let mut sr = RectF::new(
                    sw * c.left as f64,
                    sh * c.top as f64,
                    sw * (1.0 - c.left as f64 - c.right as f64),
                    sh * (1.0 - c.top as f64 - c.bottom as f64),
                );
                let (mut cx, mut cy, mut cw, mut chh) = (dx, dy, dw, dh);
                match l.fit {
                    FitMode::Stretch => {}
                    FitMode::Contain => {
                        let s = (dw / sr.width).min(dh / sr.height);
                        cw = sr.width * s;
                        chh = sr.height * s;
                        cx = dx + (dw - cw) / 2.0;
                        cy = dy + (dh - chh) / 2.0;
                    }
                    FitMode::Cover => {
                        let s = (dw / sr.width).max(dh / sr.height);
                        let (vw, vh) = (dw / s, dh / s);
                        sr = RectF::new(
                            sr.x + (sr.width - vw) / 2.0,
                            sr.y + (sr.height - vh) / 2.0,
                            vw,
                            vh,
                        );
                    }
                }
                let (x, y, w, h) = (even_down(cx), even_down(cy), even_up(cw), even_up(chh));
                if w as u32 > 16384 || h as u32 > 16384 {
                    return Err(ComposeError::InvalidConfig("レイヤーが大きすぎる"));
                }
                let scaler = I420Scaler::new(f.width(), f.height(), sr, w, h, l.filter)?;
                Ok(Some(Prepared {
                    kind: PrepKind::Picture {
                        frame: f,
                        alpha,
                        scaler: Box::new(scaler),
                    },
                    x,
                    y,
                    w,
                    h,
                    opacity: l.opacity,
                    radius: l.corner_radius as f32,
                    border: l.border,
                }))
            }
        }
    }
}

/// 角丸の矩形の符号付き距離（内側が負）。座標は矩形の左上を原点とする。
#[inline]
fn rounded_sdf(px: f32, py: f32, w: f32, h: f32, r: f32) -> f32 {
    let (hx, hy) = (w / 2.0, h / 2.0);
    let r = r.min(hx).min(hy);
    let qx = (px - hx).abs() - (hx - r);
    let qy = (py - hy).abs() - (hy - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

/// (内容の重み, 枠の重み)。
#[inline]
fn coverage(p: &Prepared, px: f32, py: f32) -> (f32, f32) {
    let d = rounded_sdf(px, py, p.w as f32, p.h as f32, p.radius);
    let cov = (0.5 - d).clamp(0.0, 1.0);
    match p.border {
        Some(b) if b.width > 0 => {
            let inner = (0.5 - (d + b.width as f32)).clamp(0.0, 1.0);
            (inner, cov - inner)
        }
        _ => (cov, 0.0),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_band(
    layers: &[Prepared],
    bg: YuvColor,
    y0: usize,
    rows: usize,
    yb: &mut [u8],
    ub: &mut [u8],
    vb: &mut [u8],
    w: usize,
) {
    yb.fill(bg.y);
    ub.fill(bg.u);
    vb.fill(bg.v);
    let cw = w / 2;
    for l in layers {
        draw_plane(l, 0, y0, rows, yb, w, false);
        draw_plane(l, 1, y0 / 2, rows / 2, ub, cw, true);
        draw_plane(l, 2, y0 / 2, rows / 2, vb, cw, true);
    }
}

/// 1 平面ぶんを描く。`plane` 0 = Y、1 = U、2 = V。`y0`・`rows`・`w` はその平面の座標。
fn draw_plane(
    l: &Prepared,
    plane: usize,
    y0: usize,
    rows: usize,
    buf: &mut [u8],
    w: usize,
    chroma: bool,
) {
    let sub = if chroma { 2 } else { 1 };
    let (lx, ly) = (l.x / sub as i64, l.y / sub as i64);
    let (lw, lh) = (l.w / sub, l.h / sub);
    // 帯と層の重なり（層の座標の行）
    let r0 = (y0 as i64 - ly).max(0);
    let r1 = ((y0 + rows) as i64 - ly).min(lh as i64);
    if r1 <= r0 {
        return;
    }
    let c0 = (-lx).max(0) as usize;
    let c1 = ((w as i64 - lx).min(lw as i64)).max(0) as usize;
    if c1 <= c0 {
        return;
    }
    let simple = l.radius <= 0.0 && l.border.is_none_or(|b| b.width == 0);
    let op = l.opacity.clamp(0.0, 1.0);
    let border_v = l
        .border
        .map(|b| [b.color.y, b.color.u, b.color.v][plane] as f32)
        .unwrap_or(0.0);
    // 層の座標 (lxp, lyp) の画素を、輝度の座標で見た中心位置
    let center = |lxp: usize, lyp: usize| -> (f32, f32) {
        if chroma {
            (2.0 * lxp as f32 + 1.0, 2.0 * lyp as f32 + 1.0)
        } else {
            (lxp as f32 + 0.5, lyp as f32 + 0.5)
        }
    };
    let mut blend_row = |lyp: usize, src: Option<&[u8]>, color: u8, alpha: Option<&[u8]>| {
        let cy = (ly + lyp as i64) as usize - y0;
        let row = &mut buf[cy * w..(cy + 1) * w];
        let dx0 = (lx + c0 as i64) as usize;
        let dst = &mut row[dx0..dx0 + (c1 - c0)];
        if simple && alpha.is_none() && op >= 1.0 {
            match src {
                Some(s) => dst.copy_from_slice(&s[c0..c1]),
                None => dst.fill(color),
            }
            return;
        }
        if simple && alpha.is_none() {
            let k = (op * 256.0).round() as u32;
            for (i, d) in dst.iter_mut().enumerate() {
                let s = src.map_or(color, |s| s[c0 + i]) as u32;
                *d = ((*d as u32 * (256 - k) + s * k + 128) >> 8) as u8;
            }
            return;
        }
        for (i, d) in dst.iter_mut().enumerate() {
            let lxp = c0 + i;
            let (ci, bi) = if simple {
                (1.0, 0.0)
            } else {
                let (px, py) = center(lxp, lyp);
                coverage(l, px, py)
            };
            let a_src = alpha.map_or(1.0, |a| a[lxp] as f32 / 255.0);
            let wc = op * ci * a_src;
            let wb = op * bi;
            if wc <= 0.0 && wb <= 0.0 {
                continue;
            }
            let s = src.map_or(color, |s| s[lxp]) as f32;
            let v = *d as f32 * (1.0 - wc - wb) + s * wc + border_v * wb;
            *d = (v + 0.5).clamp(0.0, 255.0) as u8;
        }
    };
    match &l.kind {
        PrepKind::Color(c) => {
            let color = [c.y, c.u, c.v][plane];
            for lyp in r0 as usize..r1 as usize {
                blend_row(lyp, None, color, None);
            }
        }
        PrepKind::Picture {
            frame,
            alpha,
            scaler,
        } => {
            let (src, ps) = match plane {
                0 => (frame.y(), &scaler.y),
                1 => (frame.u(), &scaler.c),
                _ => (frame.v(), &scaler.c),
            };
            // アルファは先にまとめて拡大縮小しておく。
            let mut arows: Vec<u8> = Vec::new();
            if let Some((a, ac)) = alpha {
                let ap = if chroma { ac } else { a };
                arows = vec![0; (r1 - r0) as usize * lw];
                let _ = ps.scale_rows(ap, r0 as usize..r1 as usize, |yy, r| {
                    let o = (yy - r0 as usize) * lw;
                    arows[o..o + lw].copy_from_slice(&r[..lw]);
                });
            }
            let _ = ps.scale_rows(src, r0 as usize..r1 as usize, |yy, r| {
                let a = if alpha.is_some() {
                    let o = (yy - r0 as usize) * lw;
                    Some(&arows[o..o + lw])
                } else {
                    None
                };
                blend_row(yy, Some(&r[..lw]), 0, a);
            });
        }
    }
}
