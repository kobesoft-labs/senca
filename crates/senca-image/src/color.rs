//! 色変換: YUV（BT.601 / BT.709、限定 / 全範囲）と RGB の相互変換。
//!
//! - 4:2:0 の色差は「中央」配置（JPEG・MPEG-1 と同じ。H.264 の既定の左寄せとは 1/4 画素ずれるが、
//!   映像の合成やサムネイルでは差が見えないので中央で統一する）。
//! - RGB → 4:2:0 は 2x2 の平均（中央配置と一致する）。4:2:0 → RGB は 9:3:3:1 の双線形補間。
//! - 係数は 14 bit の固定小数点。

use crate::error::{ImageError, Result};
use crate::frame::{I420Frame, Plane, RgbImage, RgbaImage, YuvColor, chroma_size};

/// 色の行列。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorMatrix {
    /// SD・JPEG（JFIF）。
    Bt601,
    /// HD。
    #[default]
    Bt709,
}

impl ColorMatrix {
    /// (Kr, Kb)。
    pub fn kr_kb(self) -> (f64, f64) {
        match self {
            Self::Bt601 => (0.299, 0.114),
            Self::Bt709 => (0.2126, 0.0722),
        }
    }
}

/// 値の範囲。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorRange {
    /// Y 16〜235、C 16〜240（映像の既定）。
    #[default]
    Limited,
    /// 0〜255（JPEG）。
    Full,
}

impl ColorRange {
    /// 黒の Y。
    pub fn black(self) -> u8 {
        match self {
            Self::Limited => 16,
            Self::Full => 0,
        }
    }
    /// 白の Y。
    pub fn white(self) -> u8 {
        match self {
            Self::Limited => 235,
            Self::Full => 255,
        }
    }
    /// Y を 0〜1 に正規化する。
    pub fn normalize_luma(self, y: f32) -> f32 {
        let (b, w) = (self.black() as f32, self.white() as f32);
        (y - b) / (w - b)
    }
}

/// YUV の規格（行列と範囲）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct YuvSpec {
    pub matrix: ColorMatrix,
    pub range: ColorRange,
}

impl YuvSpec {
    pub const BT709_LIMITED: Self = Self {
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Limited,
    };
    pub const BT601_LIMITED: Self = Self {
        matrix: ColorMatrix::Bt601,
        range: ColorRange::Limited,
    };
    /// JPEG（JFIF）の YCbCr。
    pub const BT601_FULL: Self = Self {
        matrix: ColorMatrix::Bt601,
        range: ColorRange::Full,
    };
    pub const BT709_FULL: Self = Self {
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Full,
    };

    /// 黒の YUV。
    pub fn black(self) -> YuvColor {
        YuvColor::new(self.range.black(), 128, 128)
    }
    /// 白の YUV。
    pub fn white(self) -> YuvColor {
        YuvColor::new(self.range.white(), 128, 128)
    }

    /// RGB の 1 色を YUV にする。
    pub fn rgb_to_yuv(self, r: u8, g: u8, b: u8) -> YuvColor {
        let c = Coeffs::new(self);
        let (y, u, v) = c.rgb_to_yuv(r as i32, g as i32, b as i32);
        YuvColor::new(y, u, v)
    }

    /// YUV の 1 色を RGB にする。
    pub fn yuv_to_rgb(self, c: YuvColor) -> [u8; 3] {
        Coeffs::new(self).yuv_to_rgb(c.y as i32, c.u as i32, c.v as i32)
    }
}

const SHIFT: i32 = 14;
const ONE: f64 = (1 << SHIFT) as f64;
const HALF: i32 = 1 << (SHIFT - 1);

/// 固定小数点の係数。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Coeffs {
    // RGB → YUV
    yr: i32,
    yg: i32,
    yb: i32,
    ur: i32,
    ug: i32,
    ub: i32,
    vr: i32,
    vg: i32,
    vb: i32,
    y_off: i32,
    // YUV → RGB
    cy: i32,
    rv: i32,
    gu: i32,
    gv: i32,
    bu: i32,
}

impl Coeffs {
    pub(crate) fn new(spec: YuvSpec) -> Self {
        let (kr, kb) = spec.matrix.kr_kb();
        let kg = 1.0 - kr - kb;
        let (ys, cs, y_off) = match spec.range {
            ColorRange::Limited => (219.0 / 255.0, 224.0 / 255.0, 16),
            ColorRange::Full => (1.0, 1.0, 0),
        };
        let q = |v: f64| (v * ONE).round() as i32;
        // Pb = (B - Y) / (2 (1 - Kb))、Pr = (R - Y) / (2 (1 - Kr))
        let ub_ = 0.5;
        let ur_ = -0.5 * kr / (1.0 - kb);
        let ug_ = -0.5 * kg / (1.0 - kb);
        let vr_ = 0.5;
        let vg_ = -0.5 * kg / (1.0 - kr);
        let vb_ = -0.5 * kb / (1.0 - kr);
        Self {
            yr: q(kr * ys),
            yg: q(kg * ys),
            yb: q(kb * ys),
            ur: q(ur_ * cs),
            ug: q(ug_ * cs),
            ub: q(ub_ * cs),
            vr: q(vr_ * cs),
            vg: q(vg_ * cs),
            vb: q(vb_ * cs),
            y_off,
            cy: q(1.0 / ys),
            rv: q(2.0 * (1.0 - kr) / cs),
            gu: q(-2.0 * (1.0 - kb) * kb / kg / cs),
            gv: q(-2.0 * (1.0 - kr) * kr / kg / cs),
            bu: q(2.0 * (1.0 - kb) / cs),
        }
    }

    #[inline]
    pub(crate) fn rgb_to_y(&self, r: i32, g: i32, b: i32) -> u8 {
        let y = ((self.yr * r + self.yg * g + self.yb * b + HALF) >> SHIFT) + self.y_off;
        y.clamp(0, 255) as u8
    }

    /// 色差は 2 倍精度（4 画素の和などを渡せるよう、`scale` 倍した RGB を受ける）。
    #[inline]
    pub(crate) fn rgb_to_uv_scaled(&self, r: i32, g: i32, b: i32, scale_shift: i32) -> (u8, u8) {
        let s = SHIFT + scale_shift;
        let h = 1 << (s - 1);
        let u = ((self.ur * r + self.ug * g + self.ub * b + h) >> s) + 128;
        let v = ((self.vr * r + self.vg * g + self.vb * b + h) >> s) + 128;
        (u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
    }

    #[inline]
    pub(crate) fn rgb_to_yuv(&self, r: i32, g: i32, b: i32) -> (u8, u8, u8) {
        let (u, v) = self.rgb_to_uv_scaled(r, g, b, 0);
        (self.rgb_to_y(r, g, b), u, v)
    }

    /// YUV → RGB。`u`・`v` は `1 << cshift` 倍の値を受ける（補間の端数を保つため）。
    #[inline]
    pub(crate) fn yuv_to_rgb_scaled(&self, y: i32, u: i32, v: i32, cshift: i32) -> [u8; 3] {
        let yy = self.cy * (y - self.y_off);
        let cc = 128 << cshift;
        let (u, v) = (u - cc, v - cc);
        let r = (yy + ((self.rv * v) >> cshift) + HALF) >> SHIFT;
        let g = (yy + ((self.gu * u + self.gv * v) >> cshift) + HALF) >> SHIFT;
        let b = (yy + ((self.bu * u) >> cshift) + HALF) >> SHIFT;
        [
            r.clamp(0, 255) as u8,
            g.clamp(0, 255) as u8,
            b.clamp(0, 255) as u8,
        ]
    }

    #[inline]
    pub(crate) fn yuv_to_rgb(&self, y: i32, u: i32, v: i32) -> [u8; 3] {
        self.yuv_to_rgb_scaled(y, u, v, 0)
    }
}

/// 中央配置の 4:2:0 色差を 1 行ぶん横に 2 倍し、4 倍精度で返す（9:3:3:1 の横成分）。
/// `above`/`below` は縦の補間（3:1）を先に済ませた 4 倍値の行。
fn upsample_row_h(src4: &[i32], out_w: usize, out: &mut [i32]) {
    // src4 は「縦方向に 3:1 で混ぜた 4 倍値」。横に 3:1 で混ぜて 16 倍値を作る。
    let n = src4.len();
    for (x, o) in out.iter_mut().enumerate().take(out_w) {
        let i = x / 2;
        let j = if x % 2 == 0 {
            i.saturating_sub(1)
        } else {
            (i + 1).min(n - 1)
        };
        *o = 3 * src4[i] + src4[j];
    }
}

/// 4:2:0 の色差平面の 1 行（輝度の行 `y`）を、輝度の幅に戻した 16 倍値で得る。
fn chroma_row_16x(c: &Plane, y: usize, out_w: usize, tmp: &mut Vec<i32>, out: &mut [i32]) {
    let ch = c.height();
    let i = y / 2;
    let j = if y.is_multiple_of(2) {
        i.saturating_sub(1)
    } else {
        (i + 1).min(ch - 1)
    };
    let (a, b) = (c.row(i), c.row(j));
    tmp.clear();
    tmp.extend(a.iter().zip(b).map(|(&p, &q)| 3 * p as i32 + q as i32));
    upsample_row_h(tmp, out_w, out);
}

/// 4:2:0 の色差平面を輝度の大きさに補間する（中央配置、9:3:3:1）。
pub fn upsample_chroma_420(c: &Plane, width: usize, height: usize) -> Result<Plane> {
    if chroma_size(width, height) != (c.width(), c.height()) {
        return Err(ImageError::Mismatch("色差の大きさ"));
    }
    let mut out = Plane::alloc(width, height, 128);
    let mut tmp = Vec::new();
    let mut row = vec![0i32; width];
    for y in 0..height {
        chroma_row_16x(c, y, width, &mut tmp, &mut row);
        for (d, &s) in out.row_mut(y).iter_mut().zip(&row) {
            *d = ((s + 8) >> 4) as u8;
        }
    }
    Ok(out)
}

/// 輝度の大きさの色差平面を 2x2 平均で 4:2:0 にする（中央配置）。
pub fn downsample_chroma_420(c: &Plane) -> Plane {
    let (cw, ch) = chroma_size(c.width(), c.height());
    let mut out = Plane::alloc(cw, ch, 128);
    for y in 0..ch {
        let r0 = c.row(2 * y);
        let r1 = c.row((2 * y + 1).min(c.height() - 1));
        for (x, d) in out.row_mut(y).iter_mut().enumerate() {
            let x0 = 2 * x;
            let x1 = (x0 + 1).min(c.width() - 1);
            let s = r0[x0] as u32 + r0[x1] as u32 + r1[x0] as u32 + r1[x1] as u32;
            *d = ((s + 2) >> 2) as u8;
        }
    }
    out
}

/// I420 → RGB。
pub fn i420_to_rgb(f: &I420Frame, spec: YuvSpec) -> RgbImage {
    let c = Coeffs::new(spec);
    let (w, h) = (f.width(), f.height());
    let mut out = RgbImage::new(w, h).unwrap_or_else(|_| unreachable_image());
    let mut tmp = Vec::new();
    let mut ur = vec![0i32; w];
    let mut vr = vec![0i32; w];
    for y in 0..h {
        chroma_row_16x(f.u(), y, w, &mut tmp, &mut ur);
        chroma_row_16x(f.v(), y, w, &mut tmp, &mut vr);
        let yr = f.y().row(y);
        for (x, px) in out.row_mut(y).as_chunks_mut::<3>().0.iter_mut().enumerate() {
            px.copy_from_slice(&c.yuv_to_rgb_scaled(yr[x] as i32, ur[x], vr[x], 4));
        }
    }
    out
}

/// I420 → RGBA（アルファ 255）。
pub fn i420_to_rgba(f: &I420Frame, spec: YuvSpec) -> RgbaImage {
    i420_to_rgb(f, spec).to_rgba()
}

// I420Frame は検査済みの大きさなので RgbImage::new は失敗しない。万一のときは 1x1 を返す。
fn unreachable_image() -> RgbImage {
    RgbImage::from_vec(1, 1, vec![0; 3]).unwrap_or_else(|_| unreachable!())
}

/// RGB（3 または 4 バイト / 画素）の行から I420 を作る共通部。
fn packed_to_i420(w: usize, h: usize, bpp: usize, data: &[u8], spec: YuvSpec) -> I420Frame {
    let c = Coeffs::new(spec);
    let (cw, ch) = chroma_size(w, h);
    let mut yp = Plane::alloc(w, h, 0);
    let mut up = Plane::alloc(cw, ch, 128);
    let mut vp = Plane::alloc(cw, ch, 128);
    let px = |x: usize, y: usize| {
        let s = (y * w + x) * bpp;
        (data[s] as i32, data[s + 1] as i32, data[s + 2] as i32)
    };
    for y in 0..h {
        let row = yp.row_mut(y);
        for (x, d) in row.iter_mut().enumerate() {
            let (r, g, b) = px(x, y);
            *d = c.rgb_to_y(r, g, b);
        }
    }
    for cy in 0..ch {
        let y0 = 2 * cy;
        let y1 = (y0 + 1).min(h - 1);
        for cx in 0..cw {
            let x0 = 2 * cx;
            let x1 = (x0 + 1).min(w - 1);
            let mut s = (0, 0, 0);
            for (x, y) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
                let p = px(x, y);
                s = (s.0 + p.0, s.1 + p.1, s.2 + p.2);
            }
            let (u, v) = c.rgb_to_uv_scaled(s.0, s.1, s.2, 2);
            up.set(cx, cy, u);
            vp.set(cx, cy, v);
        }
    }
    I420Frame::from_planes(yp, up, vp).unwrap_or_else(|_| unreachable!())
}

/// RGB → I420。
pub fn rgb_to_i420(img: &RgbImage, spec: YuvSpec) -> I420Frame {
    packed_to_i420(img.width(), img.height(), 3, img.data(), spec)
}

/// RGBA → I420（アルファは捨てる。合成でアルファを使うときは `RgbaImage::alpha_plane`）。
pub fn rgba_to_i420(img: &RgbaImage, spec: YuvSpec) -> I420Frame {
    packed_to_i420(img.width(), img.height(), 4, img.data(), spec)
}

/// RGB → 輝度だけ。
pub fn rgb_to_gray(img: &RgbImage, spec: YuvSpec) -> Plane {
    let c = Coeffs::new(spec);
    let data = img
        .data()
        .as_chunks::<3>()
        .0
        .iter()
        .map(|p| c.rgb_to_y(p[0] as i32, p[1] as i32, p[2] as i32))
        .collect();
    Plane::from_vec(img.width(), img.height(), data).unwrap_or_else(|_| unreachable!())
}

/// 輝度だけ → RGB（無彩色）。
pub fn gray_to_rgb(p: &Plane, range: ColorRange) -> RgbImage {
    let c = Coeffs::new(YuvSpec {
        matrix: ColorMatrix::Bt709,
        range,
    });
    let mut out = RgbImage::new(p.width(), p.height()).unwrap_or_else(|_| unreachable_image());
    for y in 0..p.height() {
        let src = p.row(y);
        for (x, d) in out.row_mut(y).as_chunks_mut::<3>().0.iter_mut().enumerate() {
            d.copy_from_slice(&c.yuv_to_rgb(src[x] as i32, 128, 128));
        }
    }
    out
}

/// YUV の規格を変える（例: 映像の BT.709 限定範囲 → JPEG の BT.601 全範囲）。
/// 範囲だけの違いなら平面ごとの変換、行列が違えば RGB を経由する。
pub fn convert_i420(f: &I420Frame, from: YuvSpec, to: YuvSpec) -> I420Frame {
    if from == to {
        return f.clone();
    }
    if from.matrix == to.matrix {
        let (lut_y, lut_c) = range_luts(from.range, to.range);
        let mut out = f.clone();
        let (y, u, v) = out.planes_mut();
        y.map_in_place(|p| lut_y[p as usize]);
        u.map_in_place(|p| lut_c[p as usize]);
        v.map_in_place(|p| lut_c[p as usize]);
        return out;
    }
    rgb_to_i420(&i420_to_rgb(f, from), to)
}

/// 範囲の変換表（Y, C）。
fn range_luts(from: ColorRange, to: ColorRange) -> ([u8; 256], [u8; 256]) {
    let mut ly = [0u8; 256];
    let mut lc = [0u8; 256];
    for i in 0..256 {
        let v = i as f64;
        let (y, c) = match (from, to) {
            (ColorRange::Limited, ColorRange::Full) => (
                (v - 16.0) * 255.0 / 219.0,
                (v - 128.0) * 255.0 / 224.0 + 128.0,
            ),
            (ColorRange::Full, ColorRange::Limited) => (
                v * 219.0 / 255.0 + 16.0,
                (v - 128.0) * 224.0 / 255.0 + 128.0,
            ),
            _ => (v, v),
        };
        ly[i] = y.round().clamp(0.0, 255.0) as u8;
        lc[i] = c.round().clamp(0.0, 255.0) as u8;
    }
    (ly, lc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values_bt709_limited() {
        let s = YuvSpec::BT709_LIMITED;
        assert_eq!(s.rgb_to_yuv(0, 0, 0), YuvColor::new(16, 128, 128));
        assert_eq!(s.rgb_to_yuv(255, 255, 255), YuvColor::new(235, 128, 128));
        // 赤: Y = 16 + 219 * 0.2126 = 62.56、V = 128 + 112 = 240
        let r = s.rgb_to_yuv(255, 0, 0);
        assert_eq!(r.y, 63);
        assert_eq!(r.v, 240);
        assert_eq!(r.u, 102); // 128 - 112 * 0.2126 / 0.9278 = 102.33
    }

    #[test]
    fn known_values_jfif() {
        let s = YuvSpec::BT601_FULL;
        let c = s.rgb_to_yuv(255, 0, 0);
        assert_eq!((c.y, c.u, c.v), (76, 85, 255));
        assert_eq!(s.yuv_to_rgb(YuvColor::new(76, 85, 255)), [254, 0, 0]);
    }

    #[test]
    fn rgb_roundtrip_all_specs() {
        for spec in [
            YuvSpec::BT601_FULL,
            YuvSpec::BT601_LIMITED,
            YuvSpec::BT709_FULL,
            YuvSpec::BT709_LIMITED,
        ] {
            let mut worst = 0;
            for r in (0..=255).step_by(15) {
                for g in (0..=255).step_by(15) {
                    for b in (0..=255).step_by(15) {
                        let c = spec.rgb_to_yuv(r as u8, g as u8, b as u8);
                        let back = spec.yuv_to_rgb(c);
                        for (a, b) in back.iter().zip([r, g, b]) {
                            worst = worst.max((*a as i32 - b).abs());
                        }
                    }
                }
            }
            let tol = if spec.range == ColorRange::Full { 2 } else { 3 };
            assert!(worst <= tol, "{spec:?} worst {worst}");
        }
    }

    #[test]
    fn i420_flat_color_roundtrip() {
        let img = RgbImage::from_vec(6, 5, [200u8, 40, 90].repeat(30)).unwrap();
        let f = rgb_to_i420(&img, YuvSpec::BT709_LIMITED);
        let back = i420_to_rgb(&f, YuvSpec::BT709_LIMITED);
        for p in back.data().as_chunks::<3>().0 {
            assert!(
                (p[0] as i32 - 200).abs() <= 2
                    && (p[1] as i32 - 40).abs() <= 2
                    && (p[2] as i32 - 90).abs() <= 2,
                "{p:?}"
            );
        }
    }

    #[test]
    fn upsample_center_sited() {
        let c = Plane::from_vec(2, 1, vec![0, 160]).unwrap();
        let up = upsample_chroma_420(&c, 4, 2).unwrap();
        // x = 0: 0、x = 1: 3/4*0 + 1/4*160 = 40、x = 2: 120、x = 3: 160
        assert_eq!(up.row(0), &[0, 40, 120, 160]);
        let down = downsample_chroma_420(&up);
        assert_eq!(down.row(0), &[20, 140]);
    }

    #[test]
    fn range_conversion() {
        let f = I420Frame::new(4, 4, YuvColor::new(235, 240, 16)).unwrap();
        let g = convert_i420(&f, YuvSpec::BT709_LIMITED, YuvSpec::BT709_FULL);
        // V = 16 は (16 - 128) * 255 / 224 + 128 = 0.5 → 1
        assert_eq!(
            (g.y().get(0, 0), g.u().get(0, 0), g.v().get(0, 0)),
            (255, 255, 1)
        );
        let h = convert_i420(&f, YuvSpec::BT709_LIMITED, YuvSpec::BT601_FULL);
        let rgb1 = YuvSpec::BT709_LIMITED.yuv_to_rgb(YuvColor::new(235, 240, 16));
        let rgb2 = YuvSpec::BT601_FULL.yuv_to_rgb(YuvColor::new(
            h.y().get(0, 0),
            h.u().get(0, 0),
            h.v().get(0, 0),
        ));
        for i in 0..3 {
            assert!(
                (rgb1[i] as i32 - rgb2[i] as i32).abs() <= 3,
                "{rgb1:?} {rgb2:?}"
            );
        }
    }
}
