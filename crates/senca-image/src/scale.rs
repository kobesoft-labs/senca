//! 拡大縮小: 分離型（横 → 縦）の重み付き和。
//!
//! - 座標は画素の中心を (i + 0.5) とする連続座標。元の範囲は小数の矩形（[`RectF`]）で指定でき、
//!   切り抜き＋拡大縮小を 1 回で行う。範囲外は端の画素で延長する。
//! - 縮小では核の幅を倍率ぶん広げる（エイリアスを抑える）。`Area` は面積平均（縮小時は厳密）。
//! - 重みは 14 bit の固定小数点、中間値は 4 bit の端数を持つ i32。
//! - [`PlaneScaler::scale_rows`] で出力の一部の行だけを作れる（合成の帯ごとの並列化に使う）。

use std::ops::Range;

use crate::error::{ImageError, Result};
use crate::frame::{I420Frame, Plane, RectF, check_dims, chroma_size};

/// 拡大縮小の核。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScaleFilter {
    /// 最近傍。
    Nearest,
    /// 双線形（三角核、縮小時は幅を広げる）。
    #[default]
    Bilinear,
    /// 双三次（Catmull-Rom、a = -0.5）。
    Bicubic,
    /// Lanczos（a = 3）。高画質。
    Lanczos3,
    /// 面積平均（縮小向け）。
    Area,
}

const WSHIFT: i32 = 14;
const WONE: i32 = 1 << WSHIFT;
/// 中間値の端数ビット。
const IBITS: i32 = 4;

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        let p = std::f64::consts::PI * x;
        p.sin() / p
    }
}

impl ScaleFilter {
    fn support(self) -> f64 {
        match self {
            Self::Nearest => 0.5,
            Self::Bilinear => 1.0,
            Self::Bicubic => 2.0,
            Self::Lanczos3 => 3.0,
            Self::Area => 0.5,
        }
    }

    fn kernel(self, x: f64) -> f64 {
        let ax = x.abs();
        match self {
            Self::Nearest | Self::Area => {
                if ax < 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
            Self::Bilinear => (1.0 - ax).max(0.0),
            Self::Bicubic => {
                let a = -0.5;
                if ax < 1.0 {
                    (a + 2.0) * ax * ax * ax - (a + 3.0) * ax * ax + 1.0
                } else if ax < 2.0 {
                    a * ax * ax * ax - 5.0 * a * ax * ax + 8.0 * a * ax - 4.0 * a
                } else {
                    0.0
                }
            }
            Self::Lanczos3 => {
                if ax < 3.0 {
                    sinc(x) * sinc(x / 3.0)
                } else {
                    0.0
                }
            }
        }
    }
}

/// 1 軸ぶんの重み。出力 i は `starts[i]` から `taps` 個の入力を使う。
#[derive(Debug, Clone)]
struct AxisWeights {
    starts: Vec<usize>,
    taps: usize,
    weights: Vec<i32>,
}

impl AxisWeights {
    fn new(
        src_len: usize,
        start: f64,
        size: f64,
        dst_len: usize,
        filter: ScaleFilter,
    ) -> Result<Self> {
        if !(start.is_finite() && size.is_finite()) || size <= 0.0 || src_len == 0 || dst_len == 0 {
            return Err(ImageError::Mismatch("拡大縮小の範囲"));
        }
        let scale = size / dst_len as f64;
        let lo = (start.floor().max(0.0) as usize).min(src_len - 1);
        let hi_f = (start + size).ceil() - 1.0;
        let hi = if hi_f < 0.0 {
            0
        } else {
            (hi_f as usize).min(src_len - 1)
        }
        .max(lo);
        let mut per: Vec<(usize, Vec<f64>)> = Vec::with_capacity(dst_len);
        let mut taps = 1usize; // 後で 0 の重みを削って詰め直す
        for i in 0..dst_len {
            let center = start + (i as f64 + 0.5) * scale;
            let mut ws: Vec<(usize, f64)> = Vec::new();
            match filter {
                ScaleFilter::Nearest => {
                    let j = (center.floor().max(0.0) as usize).clamp(lo, hi);
                    ws.push((j, 1.0));
                }
                ScaleFilter::Area => {
                    let half = scale.max(1.0) / 2.0;
                    let (a, b) = (center - half, center + half);
                    let j0 = a.floor() as i64;
                    let j1 = b.ceil() as i64;
                    for j in j0..j1 {
                        let ov = (b.min(j as f64 + 1.0) - a.max(j as f64)).max(0.0);
                        if ov > 0.0 {
                            ws.push(((j.max(0) as usize).clamp(lo, hi), ov));
                        }
                    }
                }
                _ => {
                    let fscale = scale.max(1.0);
                    let support = filter.support() * fscale;
                    let pos = center - 0.5;
                    let j0 = (pos - support).floor() as i64;
                    let j1 = (pos + support).ceil() as i64;
                    for j in j0..=j1 {
                        let w = filter.kernel((j as f64 - pos) / fscale);
                        if w != 0.0 {
                            ws.push(((j.max(0) as usize).clamp(lo, hi), w));
                        }
                    }
                }
            }
            if ws.is_empty() {
                let j = (center.floor().max(0.0) as usize).clamp(lo, hi);
                ws.push((j, 1.0));
            }
            let first = ws.iter().map(|w| w.0).min().unwrap_or(lo);
            let last = ws.iter().map(|w| w.0).max().unwrap_or(lo);
            let mut dense = vec![0.0; last - first + 1];
            for (j, w) in ws {
                dense[j - first] += w;
            }
            taps = taps.max(dense.len());
            per.push((first, dense));
        }
        if taps > 4096 {
            return Err(ImageError::Unsupported("縮小率が大きすぎる"));
        }
        let mut starts = Vec::with_capacity(dst_len);
        let mut weights = vec![0i32; dst_len * taps];
        for (i, (first, dense)) in per.into_iter().enumerate() {
            // 端で taps に満たないときは開始位置を左へずらす（範囲内に収める）。
            let first_adj = first.min(src_len.saturating_sub(taps));
            let off = first - first_adj;
            let sum: f64 = dense.iter().sum();
            let sum = if sum.abs() < 1e-12 { 1.0 } else { sum };
            let row = &mut weights[i * taps..(i + 1) * taps];
            let mut total = 0;
            let mut best = (0usize, i32::MIN);
            for (k, &w) in dense.iter().enumerate() {
                let q = (w / sum * WONE as f64).round() as i32;
                row[k + off] = q;
                total += q;
                if q > best.1 {
                    best = (k + off, q);
                }
            }
            row[best.0] += WONE - total;
            starts.push(first_adj);
        }
        // 両端の 0 の重みを削る（等倍の双三次などは 1 タップになる）。
        let mut lo_k = taps;
        let mut hi_k = 0;
        for i in 0..dst_len {
            let row = &weights[i * taps..(i + 1) * taps];
            if let Some(f) = row.iter().position(|&w| w != 0) {
                lo_k = lo_k.min(f);
            }
            if let Some(l) = row.iter().rposition(|&w| w != 0) {
                hi_k = hi_k.max(l);
            }
        }
        if lo_k <= hi_k && (lo_k > 0 || hi_k + 1 < taps) {
            // 行ごとに最初の非 0 から詰め直す
            let mut nt = 1;
            for i in 0..dst_len {
                let row = &weights[i * taps..(i + 1) * taps];
                let f = row.iter().position(|&w| w != 0).unwrap_or(0);
                let l = row.iter().rposition(|&w| w != 0).unwrap_or(0);
                nt = nt.max(l + 1 - f);
            }
            if nt < taps {
                let mut w2 = vec![0i32; dst_len * nt];
                for i in 0..dst_len {
                    let row = &weights[i * taps..(i + 1) * taps];
                    let f = row.iter().position(|&w| w != 0).unwrap_or(0);
                    let s = (starts[i] + f).min(src_len.saturating_sub(nt));
                    for k in 0..taps {
                        if row[k] != 0 {
                            w2[i * nt + (starts[i] + k - s)] += row[k];
                        }
                    }
                    starts[i] = s;
                }
                weights = w2;
                taps = nt;
            }
        }
        // taps が入力長を超えるとき（例えば 1 画素の入力）、範囲外を読まないよう詰める。
        if taps > src_len {
            let mut w2 = vec![0i32; dst_len * src_len];
            for i in 0..dst_len {
                for k in 0..taps {
                    let j = (starts[i] + k).min(src_len - 1);
                    w2[i * src_len + j] += weights[i * taps + k];
                }
                starts[i] = 0;
            }
            return Ok(Self {
                starts,
                taps: src_len,
                weights: w2,
            });
        }
        Ok(Self {
            starts,
            taps,
            weights,
        })
    }

    #[inline]
    fn row(&self, i: usize) -> (usize, &[i32]) {
        (
            self.starts[i],
            &self.weights[i * self.taps..(i + 1) * self.taps],
        )
    }
}

/// 1 平面の拡大縮小器（重みを前もって計算しておき、何度でも使う）。
#[derive(Debug, Clone)]
pub struct PlaneScaler {
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    xw: AxisWeights,
    yw: AxisWeights,
}

impl PlaneScaler {
    /// `src_w`x`src_h` の平面の `src` の範囲を `dst_w`x`dst_h` にする拡大縮小器。
    pub fn new(
        src_w: usize,
        src_h: usize,
        src: RectF,
        dst_w: usize,
        dst_h: usize,
        filter: ScaleFilter,
    ) -> Result<Self> {
        check_dims(src_w, src_h)?;
        check_dims(dst_w, dst_h)?;
        Ok(Self {
            src_w,
            src_h,
            dst_w,
            dst_h,
            xw: AxisWeights::new(src_w, src.x, src.width, dst_w, filter)?,
            yw: AxisWeights::new(src_h, src.y, src.height, dst_h, filter)?,
        })
    }

    pub fn dst_width(&self) -> usize {
        self.dst_w
    }
    pub fn dst_height(&self) -> usize {
        self.dst_h
    }

    /// 出力の `rows` の行を作り、行ごとに `emit(y, 行)` を呼ぶ。
    pub fn scale_rows(
        &self,
        src: &Plane,
        rows: Range<usize>,
        mut emit: impl FnMut(usize, &[u8]),
    ) -> Result<()> {
        if src.width() != self.src_w || src.height() != self.src_h {
            return Err(ImageError::Mismatch("拡大縮小器と入力の大きさ"));
        }
        let rows = rows.start.min(self.dst_h)..rows.end.min(self.dst_h);
        if rows.is_empty() {
            return Ok(());
        }
        let first_src = self.yw.starts[rows.start];
        let last_src = (self.yw.starts[rows.end - 1] + self.yw.taps).min(self.src_h);
        let n = last_src - first_src;
        let dw = self.dst_w;
        let mut tmp = vec![0i32; n * dw];
        let xt = self.xw.taps;
        for (r, sy) in (first_src..last_src).enumerate() {
            let srow = src.row(sy);
            let out = &mut tmp[r * dw..(r + 1) * dw];
            for (x, o) in out.iter_mut().enumerate() {
                let (s, w) = self.xw.row(x);
                let px = &srow[s..s + xt];
                let mut acc = 0i32;
                for k in 0..xt {
                    acc += w[k] * px[k] as i32;
                }
                *o = (acc + (1 << (WSHIFT - IBITS - 1))) >> (WSHIFT - IBITS);
            }
        }
        let yt = self.yw.taps;
        let mut line = vec![0u8; dw];
        let mut acc = vec![0i32; dw];
        let round = 1 << (WSHIFT + IBITS - 1);
        for y in rows {
            let (s, w) = self.yw.row(y);
            acc.fill(round);
            for (k, &wk) in w.iter().enumerate().take(yt) {
                if wk == 0 {
                    continue;
                }
                let r = s + k - first_src;
                let t = &tmp[r * dw..(r + 1) * dw];
                for (a, &v) in acc.iter_mut().zip(t) {
                    *a += wk * v;
                }
            }
            for (d, &a) in line.iter_mut().zip(&acc) {
                *d = (a >> (WSHIFT + IBITS)).clamp(0, 255) as u8;
            }
            emit(y, &line);
        }
        Ok(())
    }

    /// 全体を拡大縮小する。
    pub fn scale(&self, src: &Plane) -> Result<Plane> {
        let mut out = Plane::alloc(self.dst_w, self.dst_h, 0);
        self.scale_rows(src, 0..self.dst_h, |y, row| {
            out.row_mut(y).copy_from_slice(row)
        })?;
        Ok(out)
    }
}

/// 平面全体を拡大縮小する。
pub fn scale_plane(src: &Plane, dst_w: usize, dst_h: usize, filter: ScaleFilter) -> Result<Plane> {
    let r = RectF::new(0.0, 0.0, src.width() as f64, src.height() as f64);
    scale_plane_rect(src, r, dst_w, dst_h, filter)
}

/// 平面の一部（小数の矩形）を切り抜いて拡大縮小する。
pub fn scale_plane_rect(
    src: &Plane,
    rect: RectF,
    dst_w: usize,
    dst_h: usize,
    filter: ScaleFilter,
) -> Result<Plane> {
    PlaneScaler::new(src.width(), src.height(), rect, dst_w, dst_h, filter)?.scale(src)
}

/// I420 の 3 平面の拡大縮小器。輝度の座標の矩形から色差の矩形を求める（中央配置）。
#[derive(Debug, Clone)]
pub struct I420Scaler {
    pub y: PlaneScaler,
    pub c: PlaneScaler,
}

impl I420Scaler {
    /// `src` は輝度の座標の範囲。
    pub fn new(
        src_w: usize,
        src_h: usize,
        src: RectF,
        dst_w: usize,
        dst_h: usize,
        filter: ScaleFilter,
    ) -> Result<Self> {
        let y = PlaneScaler::new(src_w, src_h, src, dst_w, dst_h, filter)?;
        let (scw, sch) = chroma_size(src_w, src_h);
        let (dcw, dch) = chroma_size(dst_w, dst_h);
        let sx = src.width / dst_w as f64;
        let sy = src.height / dst_h as f64;
        let crect = RectF::new(src.x / 2.0, src.y / 2.0, dcw as f64 * sx, dch as f64 * sy);
        let c = PlaneScaler::new(scw, sch, crect, dcw, dch, filter)?;
        Ok(Self { y, c })
    }

    /// 全体を拡大縮小する。
    pub fn scale(&self, f: &I420Frame) -> Result<I420Frame> {
        I420Frame::from_planes(
            self.y.scale(f.y())?,
            self.c.scale(f.u())?,
            self.c.scale(f.v())?,
        )
    }
}

/// I420 全体を拡大縮小する。
pub fn scale_i420(
    f: &I420Frame,
    dst_w: usize,
    dst_h: usize,
    filter: ScaleFilter,
) -> Result<I420Frame> {
    let r = RectF::new(0.0, 0.0, f.width() as f64, f.height() as f64);
    scale_i420_rect(f, r, dst_w, dst_h, filter)
}

/// I420 の一部（輝度の座標）を切り抜いて拡大縮小する。
pub fn scale_i420_rect(
    f: &I420Frame,
    rect: RectF,
    dst_w: usize,
    dst_h: usize,
    filter: ScaleFilter,
) -> Result<I420Frame> {
    I420Scaler::new(f.width(), f.height(), rect, dst_w, dst_h, filter)?.scale(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(w: usize, h: usize) -> Plane {
        Plane::from_fn(w, h, |x, y| {
            ((x * 255) / (w - 1) / 2 + (y * 255) / (h - 1) / 2) as u8
        })
        .unwrap()
    }

    #[test]
    fn identity_is_exact() {
        let p = ramp(37, 23);
        for f in [
            ScaleFilter::Nearest,
            ScaleFilter::Bilinear,
            ScaleFilter::Bicubic,
            ScaleFilter::Lanczos3,
            ScaleFilter::Area,
        ] {
            let q = scale_plane(&p, 37, 23, f).unwrap();
            assert_eq!(q, p, "{f:?}");
        }
    }

    #[test]
    fn flat_stays_flat() {
        let p = Plane::filled(50, 30, 77).unwrap();
        for f in [
            ScaleFilter::Bilinear,
            ScaleFilter::Bicubic,
            ScaleFilter::Lanczos3,
            ScaleFilter::Area,
        ] {
            for (w, h) in [(13, 7), (200, 90), (1, 1), (50, 3)] {
                let q = scale_plane(&p, w, h, f).unwrap();
                assert!(q.to_packed().iter().all(|&v| v == 77), "{f:?} {w}x{h}");
            }
        }
    }

    #[test]
    fn downscale_by_two_area_is_average() {
        let p = Plane::from_fn(4, 2, |x, _| (x * 10) as u8).unwrap();
        let q = scale_plane(&p, 2, 1, ScaleFilter::Area).unwrap();
        assert_eq!(q.row(0), &[5, 25]);
    }

    #[test]
    fn bilinear_upscale_center_sited() {
        let p = Plane::from_vec(2, 1, vec![0, 160]).unwrap();
        let q = scale_plane(&p, 4, 1, ScaleFilter::Bilinear).unwrap();
        assert_eq!(q.row(0), &[0, 40, 120, 160]);
    }

    #[test]
    fn ramp_preserved_by_all_filters() {
        let p = Plane::from_fn(64, 8, |x, _| (x * 4) as u8).unwrap();
        for f in [
            ScaleFilter::Bilinear,
            ScaleFilter::Bicubic,
            ScaleFilter::Lanczos3,
            ScaleFilter::Area,
        ] {
            let q = scale_plane(&p, 32, 8, f).unwrap();
            for x in 4..28 {
                let expect = (2 * x) as f64 * 4.0 + 2.0;
                assert!(
                    (q.get(x, 3) as f64 - expect).abs() <= 1.0,
                    "{f:?} x {x}: {}",
                    q.get(x, 3)
                );
            }
        }
    }

    #[test]
    fn rows_subset_matches_full() {
        let p = ramp(97, 61);
        let s = PlaneScaler::new(
            97,
            61,
            RectF::new(3.5, 2.0, 80.0, 50.0),
            40,
            33,
            ScaleFilter::Lanczos3,
        )
        .unwrap();
        let full = s.scale(&p).unwrap();
        let mut got = Plane::new(40, 33).unwrap();
        for band in [0..5, 5..17, 17..33] {
            s.scale_rows(&p, band, |y, r| got.row_mut(y).copy_from_slice(r))
                .unwrap();
        }
        assert_eq!(got, full);
    }

    #[test]
    fn tiny_sources() {
        let p = Plane::filled(1, 1, 200).unwrap();
        let q = scale_plane(&p, 9, 5, ScaleFilter::Lanczos3).unwrap();
        assert!(q.to_packed().iter().all(|&v| v == 200));
        let q = scale_plane(&ramp(3, 2), 1, 1, ScaleFilter::Bicubic).unwrap();
        assert_eq!(q.width(), 1);
    }

    #[test]
    fn rejects_bad_args() {
        let p = ramp(8, 8);
        assert!(scale_plane(&p, 0, 4, ScaleFilter::Bilinear).is_err());
        assert!(
            scale_plane_rect(
                &p,
                RectF::new(0.0, 0.0, f64::NAN, 3.0),
                4,
                4,
                ScaleFilter::Bilinear
            )
            .is_err()
        );
        let s = PlaneScaler::new(
            8,
            8,
            RectF::new(0.0, 0.0, 8.0, 8.0),
            4,
            4,
            ScaleFilter::Bilinear,
        )
        .unwrap();
        assert!(s.scale(&ramp(9, 8)).is_err());
    }

    #[test]
    fn i420_scale_sizes() {
        let f = I420Frame::new(33, 17, crate::frame::YuvColor::new(100, 90, 160)).unwrap();
        let g = scale_i420(&f, 21, 11, ScaleFilter::Lanczos3).unwrap();
        assert_eq!((g.u().width(), g.u().height()), (11, 6));
        assert!(g.u().to_packed().iter().all(|&v| v == 90));
        assert!(g.v().to_packed().iter().all(|&v| v == 160));
    }
}
