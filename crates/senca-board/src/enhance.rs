//! 照明むら・反射の補正と、板書の強調（R6-5 / R6-6）。
//!
//! 処理の流れ（輝度）:
//! 1. 縮小（解析の幅 ≈ 400）。
//! 2. 反射（大きく明るい塊）を見つけ、周りから埋める（push-pull の補間）。
//! 3. 背景の推定: ホワイトボードはクロージング（暗く細い字を消す）、黒板はオープニング（明るく細い字を消す）、
//!    その後ぼかして元の大きさへ。
//! 4. 正規化: ホワイトボードは割り算（Y / 背景）、黒板は引き算（Y − 背景）。字の濃さ d（0〜1）を得る。
//! 5. 出力: ホワイトボードは白地に字、黒板は指定の地の色（既定は濃い緑）に字。黒板は白地への反転も選べる。
//! 6. 色差: 背景の色かぶりを除き、字の色（マーカー・色チョーク）を強める。
//!
//! 任意で CLAHE（領域ごとのヒストグラム平坦化）を輝度に掛ける。

use senca_image::filter::{box_blur_f32, closing, max_filter, opening};
use senca_image::frame::chroma_size;
use senca_image::scale::{ScaleFilter, scale_plane};
use senca_image::{ColorRange, I420Frame, Plane, PlaneF32, Result, YuvColor, YuvSpec};

/// 板の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoardKind {
    /// ホワイトボード（明るい地に暗い字）。
    #[default]
    Whiteboard,
    /// 黒板（暗い地に明るいチョーク）。
    Blackboard,
}

/// CLAHE のパラメーター。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClaheParams {
    pub tiles_x: usize,
    pub tiles_y: usize,
    /// 平均の何倍で頭打ちにするか（2〜4 が普通）。
    pub clip_limit: f32,
}

impl Default for ClaheParams {
    fn default() -> Self {
        Self {
            tiles_x: 8,
            tiles_y: 8,
            clip_limit: 2.0,
        }
    }
}

/// 補正のパラメーター（そのまま設定として保存できるよう、素朴な値だけ）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnhanceParams {
    pub kind: BoardKind,
    /// 入出力の YUV の規格。
    pub spec: YuvSpec,
    /// 字の線の太さの上限（入力の画素）。背景の推定で消す大きさ。
    pub max_stroke_width: usize,
    /// 背景との差がこれ未満（0〜1）なら地とみなす（地のノイズを消す）。
    pub noise_floor: f32,
    /// 字の濃さの倍率。
    pub stroke_gain: f32,
    /// 字の色の倍率。
    pub chroma_gain: f32,
    /// 反射の抑制。
    pub glare_suppression: bool,
    /// 反射とみなす明るさ（0〜1 に正規化した輝度）。
    pub glare_level: f32,
    /// 出力の地の色。None なら種類の既定（ホワイトボードは白、黒板は濃い緑）。
    pub background: Option<YuvColor>,
    /// 黒板を白地・暗い字に反転して出す。
    pub invert_to_white: bool,
    /// 輝度の CLAHE（正規化の後に掛ける）。
    pub clahe: Option<ClaheParams>,
    /// 解析（背景の推定）の幅。
    pub analysis_width: usize,
}

impl Default for EnhanceParams {
    fn default() -> Self {
        Self::whiteboard()
    }
}

impl EnhanceParams {
    /// ホワイトボードの既定値。
    pub fn whiteboard() -> Self {
        Self {
            kind: BoardKind::Whiteboard,
            spec: YuvSpec::BT709_LIMITED,
            max_stroke_width: 16,
            noise_floor: 0.06,
            stroke_gain: 1.6,
            chroma_gain: 1.8,
            glare_suppression: true,
            glare_level: 0.97,
            background: None,
            invert_to_white: false,
            clahe: None,
            analysis_width: 400,
        }
    }

    /// 黒板の既定値。
    pub fn blackboard() -> Self {
        Self {
            kind: BoardKind::Blackboard,
            noise_floor: 0.05,
            stroke_gain: 2.2,
            chroma_gain: 1.6,
            glare_level: 0.55,
            ..Self::whiteboard()
        }
    }

    fn background_color(&self) -> YuvColor {
        match (self.background, self.kind, self.invert_to_white) {
            (Some(c), _, _) => c,
            (None, BoardKind::Whiteboard, _) | (None, BoardKind::Blackboard, true) => {
                self.spec.white()
            }
            (None, BoardKind::Blackboard, false) => self.spec.rgb_to_yuv(28, 56, 40),
        }
    }
}

/// 有効な画素だけから欠けた所を埋める（push-pull: 縮小で平均し、拡大で穴を埋める）。
pub fn inpaint(values: &PlaneF32, valid: &[bool]) -> PlaneF32 {
    let (w, h) = (values.width, values.height);
    let wts: Vec<f32> = valid.iter().map(|&v| if v { 1.0 } else { 0.0 }).collect();
    let vals: Vec<f32> = values.data.iter().zip(&wts).map(|(v, w)| v * w).collect();
    let filled = push_pull(&vals, &wts, w, h);
    let mut out = values.clone();
    for i in 0..w * h {
        if !valid[i] {
            out.data[i] = filled[i];
        }
    }
    out
}

/// (重み付きの値, 重み) を再帰的に縮小して穴を埋めた値を返す。
fn push_pull(vals: &[f32], wts: &[f32], w: usize, h: usize) -> Vec<f32> {
    let mut out: Vec<f32> = vals
        .iter()
        .zip(wts)
        .map(|(v, &wt)| if wt > 0.0 { v / wt } else { 0.0 })
        .collect();
    if wts.iter().all(|&x| x >= 1.0) {
        return out;
    }
    if w <= 1 && h <= 1 {
        return out;
    }
    let (w2, h2) = (w.div_ceil(2), h.div_ceil(2));
    let mut v2 = vec![0.0; w2 * h2];
    let mut wt2 = vec![0.0; w2 * h2];
    for y in 0..h {
        for x in 0..w {
            let i = (y / 2) * w2 + x / 2;
            v2[i] += vals[y * w + x];
            wt2[i] += wts[y * w + x];
        }
    }
    if wt2.iter().all(|&x| x == 0.0) {
        return out;
    }
    // 重みは 1 で頭打ち（値は平均に保つ）
    for i in 0..w2 * h2 {
        if wt2[i] > 1.0 {
            v2[i] /= wt2[i];
            wt2[i] = 1.0;
        }
    }
    let coarse = push_pull(&v2, &wt2, w2, h2);
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let wt = wts[i].min(1.0);
            if wt < 1.0 {
                // 粗い段の値を双線形で
                let fx = ((x as f32 + 0.5) / 2.0 - 0.5).clamp(0.0, (w2 - 1) as f32);
                let fy = ((y as f32 + 0.5) / 2.0 - 0.5).clamp(0.0, (h2 - 1) as f32);
                let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
                let (x1, y1) = ((x0 + 1).min(w2 - 1), (y0 + 1).min(h2 - 1));
                let (ax, ay) = (fx - x0 as f32, fy - y0 as f32);
                let c = (coarse[y0 * w2 + x0] * (1.0 - ax) + coarse[y0 * w2 + x1] * ax)
                    * (1.0 - ay)
                    + (coarse[y1 * w2 + x0] * (1.0 - ax) + coarse[y1 * w2 + x1] * ax) * ay;
                let own = if wts[i] > 0.0 { vals[i] / wts[i] } else { 0.0 };
                out[i] = own * wt + c * (1.0 - wt);
            }
        }
    }
    out
}

/// CLAHE（8 bit の平面）。
pub fn clahe(p: &Plane, params: &ClaheParams) -> Plane {
    let (w, h) = (p.width(), p.height());
    let tx = params.tiles_x.clamp(1, 64).min(w);
    let ty = params.tiles_y.clamp(1, 64).min(h);
    let mut maps = vec![[0u8; 256]; tx * ty];
    for j in 0..ty {
        for i in 0..tx {
            let (x0, x1) = (i * w / tx, (i + 1) * w / tx);
            let (y0, y1) = (j * h / ty, (j + 1) * h / ty);
            let mut hist = [0u32; 256];
            for y in y0..y1 {
                for &v in &p.row(y)[x0..x1] {
                    hist[v as usize] += 1;
                }
            }
            let n = ((x1 - x0) * (y1 - y0)).max(1) as f32;
            let limit = (params.clip_limit.max(1.0) * n / 256.0).max(1.0) as u32;
            let mut excess = 0u32;
            for c in hist.iter_mut() {
                if *c > limit {
                    excess += *c - limit;
                    *c = limit;
                }
            }
            let add = excess / 256;
            let mut rem = excess % 256;
            let mut acc = 0u32;
            for (v, c) in hist.iter().enumerate() {
                let extra = if rem > 0 {
                    rem -= 1;
                    1
                } else {
                    0
                };
                acc += c + add + extra;
                maps[j * tx + i][v] = ((acc as f32 * 255.0 / n) + 0.5).min(255.0) as u8;
            }
        }
    }
    let mut out = Plane::filled(w, h, 0).unwrap_or_else(|_| p.clone());
    for y in 0..h {
        let fy = ((y as f32 + 0.5) * ty as f32 / h as f32 - 0.5).clamp(0.0, (ty - 1) as f32);
        let (j0, ay) = (fy.floor() as usize, fy - fy.floor());
        let j1 = (j0 + 1).min(ty - 1);
        let src = p.row(y);
        let dst = out.row_mut(y);
        for x in 0..w {
            let fx = ((x as f32 + 0.5) * tx as f32 / w as f32 - 0.5).clamp(0.0, (tx - 1) as f32);
            let (i0, ax) = (fx.floor() as usize, fx - fx.floor());
            let i1 = (i0 + 1).min(tx - 1);
            let v = src[x] as usize;
            let m = |i: usize, j: usize| maps[j * tx + i][v] as f32;
            let r = (m(i0, j0) * (1.0 - ax) + m(i1, j0) * ax) * (1.0 - ay)
                + (m(i0, j1) * (1.0 - ax) + m(i1, j1) * ax) * ay;
            dst[x] = (r + 0.5) as u8;
        }
    }
    out
}

/// 輝度の補正の中間結果。
struct LumaResult {
    /// 出力の輝度。
    y: Plane,
    /// 字の濃さ（0〜1、輝度の大きさ）。
    stroke: PlaneF32,
}

fn to_norm(p: &Plane, range: ColorRange) -> PlaneF32 {
    let mut f = PlaneF32::from_plane(p);
    for v in f.data.iter_mut() {
        *v = range.normalize_luma(*v);
    }
    f
}

fn upscale_f32(p: &PlaneF32, w: usize, h: usize) -> Result<PlaneF32> {
    // 0〜1 の値を 8 bit 以上の精度で拡大するため、双線形を自前で行う。
    let mut out = PlaneF32::filled(w, h, 0.0)?;
    let (sw, sh) = (p.width as f32, p.height as f32);
    for y in 0..h {
        let fy = ((y as f32 + 0.5) * sh / h as f32 - 0.5).clamp(0.0, sh - 1.0);
        let (y0, ay) = (fy.floor() as usize, fy - fy.floor());
        let y1 = (y0 + 1).min(p.height - 1);
        let row = out.row_mut(y);
        for (x, o) in row.iter_mut().enumerate() {
            let fx = ((x as f32 + 0.5) * sw / w as f32 - 0.5).clamp(0.0, sw - 1.0);
            let (x0, ax) = (fx.floor() as usize, fx - fx.floor());
            let x1 = (x0 + 1).min(p.width - 1);
            *o = (p.get(x0, y0) * (1.0 - ax) + p.get(x1, y0) * ax) * (1.0 - ay)
                + (p.get(x0, y1) * (1.0 - ax) + p.get(x1, y1) * ax) * ay;
        }
    }
    Ok(out)
}

/// 背景（照明）の推定。戻り値は (全解像度の背景（0〜1）, 全解像度の反射マスク)。
fn estimate_background(y: &Plane, params: &EnhanceParams) -> Result<(PlaneF32, Vec<bool>)> {
    let range = params.spec.range;
    let (w, h) = (y.width(), y.height());
    let aw = params.analysis_width.clamp(32, 2048).min(w);
    let f = w as f64 / aw as f64;
    let ah = ((h as f64 / f).round() as usize).max(8);
    let small = scale_plane(y, aw, ah, ScaleFilter::Area)?;
    // 字を消す半径（解析の解像度で）
    let r = ((params.max_stroke_width as f64 / f / 2.0).ceil() as usize + 1).max(2);
    let mut glare_full = vec![false; w * h];
    let mut base = small.clone();
    if params.glare_suppression {
        let lvl = (range.black() as f32
            + params.glare_level * (range.white() as f32 - range.black() as f32))
            .clamp(0.0, 255.0) as u8;
        let mut mask = small.clone();
        mask.map_in_place(|v| if v >= lvl { 255 } else { 0 });
        // 細いもの（チョークの字）を除き、大きな塊だけ残して少し広げる。
        let big = max_filter(&opening(&mask, r + 1), 2);
        let valid: Vec<bool> = big.to_packed().iter().map(|&v| v == 0).collect();
        let n_glare = valid.iter().filter(|&&v| !v).count();
        if n_glare > 0 && n_glare < valid.len() {
            let filled = inpaint(&PlaneF32::from_plane(&small), &valid);
            base = filled.to_plane();
            let gm = senca_image::scale::scale_plane(&big, w, h, ScaleFilter::Nearest)?;
            for yy in 0..h {
                for (xx, &v) in gm.row(yy).iter().enumerate() {
                    glare_full[yy * w + xx] = v != 0;
                }
            }
        }
    }
    let bg = match params.kind {
        BoardKind::Whiteboard => closing(&base, r),
        BoardKind::Blackboard => opening(&base, r),
    };
    let bgf = box_blur_f32(&to_norm(&bg, range), r);
    let bgf = box_blur_f32(&bgf, r);
    Ok((upscale_f32(&bgf, w, h)?, glare_full))
}

fn enhance_luma(y: &Plane, params: &EnhanceParams) -> Result<LumaResult> {
    let range = params.spec.range;
    let (w, h) = (y.width(), y.height());
    let (bg, glare) = estimate_background(y, params)?;
    let yn = to_norm(y, range);
    let (lo, hi) = (range.black() as f32, range.white() as f32);
    let bgc = params.background_color();
    let target = range.normalize_luma(bgc.y as f32);
    let mut out = Plane::filled(w, h, 0)?;
    let mut stroke = PlaneF32::filled(w, h, 0.0)?;
    for yy in 0..h {
        for xx in 0..w {
            let i = yy * w + xx;
            let b = bg.data[i];
            let v = yn.data[i];
            let d = if glare[i] {
                0.0
            } else {
                match params.kind {
                    BoardKind::Whiteboard => 1.0 - (v / b.max(0.05)).min(1.0),
                    BoardKind::Blackboard => ((v - b) / (1.0 - b).max(0.2)).max(0.0),
                }
            };
            let s = ((d - params.noise_floor) * params.stroke_gain / (1.0 - params.noise_floor))
                .clamp(0.0, 1.0);
            stroke.data[i] = s;
            let o = match (params.kind, params.invert_to_white) {
                (BoardKind::Whiteboard, _) | (BoardKind::Blackboard, true) => target * (1.0 - s),
                (BoardKind::Blackboard, false) => target + (1.0 - target) * s,
            };
            out.set(xx, yy, (lo + o * (hi - lo) + 0.5).clamp(0.0, 255.0) as u8);
        }
    }
    if let Some(c) = &params.clahe {
        out = clahe(&out, c);
    }
    Ok(LumaResult { y: out, stroke })
}

/// 輝度だけの補正（グレーの入出力）。
pub fn enhance_gray(y: &Plane, params: &EnhanceParams) -> Result<Plane> {
    Ok(enhance_luma(y, params)?.y)
}

/// I420 の補正（照明むら・反射の除去、字の強調、地の色の統一）。
pub fn enhance_i420(f: &I420Frame, params: &EnhanceParams) -> Result<I420Frame> {
    let lr = enhance_luma(f.y(), params)?;
    let (cw, ch) = chroma_size(f.width(), f.height());
    let bgc = params.background_color();
    // 色差の背景（色かぶり）: 大きくぼかした色差。
    let rad = ((params.max_stroke_width * 2) / 2).max(4);
    let chroma_bg = |c: &Plane| -> Result<PlaneF32> {
        let aw = (cw / 4).max(1);
        let ah = (ch / 4).max(1);
        let s = scale_plane(c, aw, ah, ScaleFilter::Area)?;
        let b = box_blur_f32(&PlaneF32::from_plane(&s), (rad / 4).max(2));
        upscale_f32(&b, cw, ch)
    };
    let ub = chroma_bg(f.u())?;
    let vb = chroma_bg(f.v())?;
    // 字の濃さを色差の大きさへ（2x2 の最大）。
    let mut s_c = vec![0f32; cw * ch];
    for y in 0..f.height() {
        for x in 0..f.width() {
            let i = (y / 2) * cw + x / 2;
            s_c[i] = s_c[i].max(lr.stroke.data[y * f.width() + x]);
        }
    }
    let mut u = Plane::filled(cw, ch, bgc.u)?;
    let mut v = Plane::filled(cw, ch, bgc.v)?;
    let g = params.chroma_gain;
    for y in 0..ch {
        for x in 0..cw {
            let i = y * cw + x;
            let s = s_c[i];
            if s <= 0.0 {
                continue;
            }
            let du = (f.u().get(x, y) as f32 - ub.data[i]) * g * s;
            let dv = (f.v().get(x, y) as f32 - vb.data[i]) * g * s;
            u.set(x, y, (bgc.u as f32 + du + 0.5).clamp(0.0, 255.0) as u8);
            v.set(x, y, (bgc.v as f32 + dv + 0.5).clamp(0.0, 255.0) as u8);
        }
    }
    I420Frame::from_planes(lr.y, u, v)
}
