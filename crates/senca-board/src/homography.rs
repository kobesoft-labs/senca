//! 4 点の対応からのホモグラフィ（正規化 DLT）と、透視変換（台形補正）。

use senca_image::frame::chroma_size;
use senca_image::{I420Frame, ImageError, Plane, Result, YuvColor};

/// 平面上の点（画素の連続座標。画素 (i, j) の中心は (i + 0.5, j + 0.5)）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
    pub fn dist(self, o: Point) -> f64 {
        ((self.x - o.x).powi(2) + (self.y - o.y).powi(2)).sqrt()
    }
}

/// 3x3 の射影変換（行優先、h[8] = 1 に正規化）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Homography(pub [f64; 9]);

/// n 元の連立一次方程式を部分ピボット選択つきのガウスの消去法で解く（a は n x (n+1) の拡大係数行列）。
pub fn solve_linear<const N: usize>(a: &mut [[f64; N]], n: usize) -> Option<Vec<f64>> {
    for col in 0..n {
        let (piv, maxv) = (col..n)
            .map(|r| (r, a[r][col].abs()))
            .max_by(|x, y| x.1.total_cmp(&y.1))?;
        if maxv < 1e-12 || !maxv.is_finite() {
            return None;
        }
        a.swap(col, piv);
        let pivot_row = a[col];
        for row in a.iter_mut().take(n).skip(col + 1) {
            let f = row[col] / pivot_row[col];
            if f != 0.0 {
                for (v, &p) in row.iter_mut().zip(pivot_row.iter()).take(n + 1).skip(col) {
                    *v -= f * p;
                }
            }
        }
    }
    let mut x = vec![0.0; n];
    for r in (0..n).rev() {
        let mut s = a[r][n];
        for c in r + 1..n {
            s -= a[r][c] * x[c];
        }
        x[r] = s / a[r][r];
        if !x[r].is_finite() {
            return None;
        }
    }
    Some(x)
}

/// 点群の正規化（重心を原点、原点からの平均距離を √2）。(変換, 逆変換) を 3x3 で返す。
fn normalization(p: &[Point; 4]) -> ([f64; 9], [f64; 9]) {
    let cx = p.iter().map(|q| q.x).sum::<f64>() / 4.0;
    let cy = p.iter().map(|q| q.y).sum::<f64>() / 4.0;
    let md = p
        .iter()
        .map(|q| ((q.x - cx).powi(2) + (q.y - cy).powi(2)).sqrt())
        .sum::<f64>()
        / 4.0;
    let s = if md > 1e-12 {
        std::f64::consts::SQRT_2 / md
    } else {
        1.0
    };
    (
        [s, 0.0, -s * cx, 0.0, s, -s * cy, 0.0, 0.0, 1.0],
        [1.0 / s, 0.0, cx, 0.0, 1.0 / s, cy, 0.0, 0.0, 1.0],
    )
}

fn mul(a: &[f64; 9], b: &[f64; 9]) -> [f64; 9] {
    let mut r = [0.0; 9];
    for i in 0..3 {
        for j in 0..3 {
            r[i * 3 + j] = (0..3).map(|k| a[i * 3 + k] * b[k * 3 + j]).sum();
        }
    }
    r
}

impl Homography {
    pub const IDENTITY: Self = Self([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);

    /// `src[i]` を `dst[i]` に写す変換。3 点が一直線に並ぶなど退化していれば None。
    pub fn from_points(src: &[Point; 4], dst: &[Point; 4]) -> Option<Self> {
        if src
            .iter()
            .chain(dst)
            .any(|p| !(p.x.is_finite() && p.y.is_finite()))
        {
            return None;
        }
        let (ts, _) = normalization(src);
        let (td, td_inv) = normalization(dst);
        let ap = |t: &[f64; 9], p: Point| Point::new(t[0] * p.x + t[2], t[4] * p.y + t[5]);
        let mut a = [[0.0f64; 9]; 8];
        for i in 0..4 {
            let s = ap(&ts, src[i]);
            let d = ap(&td, dst[i]);
            a[2 * i] = [s.x, s.y, 1.0, 0.0, 0.0, 0.0, -d.x * s.x, -d.x * s.y, d.x];
            a[2 * i + 1] = [0.0, 0.0, 0.0, s.x, s.y, 1.0, -d.y * s.x, -d.y * s.y, d.y];
        }
        let h = solve_linear(&mut a, 8)?;
        let hn = [h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], 1.0];
        let m = mul(&td_inv, &mul(&hn, &ts));
        let hm = Self(m).normalized()?;
        // 退化の検査: 4 点がきちんと写ること。
        for i in 0..4 {
            let q = hm.apply(src[i])?;
            let tol = 1e-6 * (1.0 + dst[i].x.abs() + dst[i].y.abs());
            if (q.x - dst[i].x).abs() > tol || (q.y - dst[i].y).abs() > tol {
                return None;
            }
        }
        Some(hm)
    }

    fn normalized(self) -> Option<Self> {
        let s = self.0[8];
        if s.abs() < 1e-15 || !s.is_finite() {
            return None;
        }
        Some(Self(self.0.map(|v| v / s)))
    }

    /// 点を写す。無限遠に飛ぶときは None。
    #[inline]
    pub fn apply(&self, p: Point) -> Option<Point> {
        let h = &self.0;
        let w = h[6] * p.x + h[7] * p.y + h[8];
        if w.abs() < 1e-12 {
            return None;
        }
        Some(Point::new(
            (h[0] * p.x + h[1] * p.y + h[2]) / w,
            (h[3] * p.x + h[4] * p.y + h[5]) / w,
        ))
    }

    /// 逆変換。
    pub fn inverse(&self) -> Option<Self> {
        let m = &self.0;
        let det = m[0] * (m[4] * m[8] - m[5] * m[7]) - m[1] * (m[3] * m[8] - m[5] * m[6])
            + m[2] * (m[3] * m[7] - m[4] * m[6]);
        if det.abs() < 1e-15 || !det.is_finite() {
            return None;
        }
        let inv = [
            m[4] * m[8] - m[5] * m[7],
            m[2] * m[7] - m[1] * m[8],
            m[1] * m[5] - m[2] * m[4],
            m[5] * m[6] - m[3] * m[8],
            m[0] * m[8] - m[2] * m[6],
            m[2] * m[3] - m[0] * m[5],
            m[3] * m[7] - m[4] * m[6],
            m[1] * m[6] - m[0] * m[7],
            m[0] * m[4] - m[1] * m[3],
        ];
        Self(inv.map(|v| v / det)).normalized()
    }

    /// 入力と出力の座標をそれぞれ倍率で変える（色差の平面用: `out = S_out * H * S_in^-1`）。
    pub fn rescaled(&self, in_scale: f64, out_scale: f64) -> Self {
        let si = [
            1.0 / in_scale,
            0.0,
            0.0,
            0.0,
            1.0 / in_scale,
            0.0,
            0.0,
            0.0,
            1.0,
        ];
        let so = [out_scale, 0.0, 0.0, 0.0, out_scale, 0.0, 0.0, 0.0, 1.0];
        Self(mul(&so, &mul(&self.0, &si)))
            .normalized()
            .unwrap_or(*self)
    }
}

/// 出力の矩形（`out_w`x`out_h`）の 4 隅（左上・右上・右下・左下）。
pub fn rect_corners(out_w: usize, out_h: usize) -> [Point; 4] {
    let (w, h) = (out_w as f64, out_h as f64);
    [
        Point::new(0.0, 0.0),
        Point::new(w, 0.0),
        Point::new(w, h),
        Point::new(0.0, h),
    ]
}

/// 出力の画素から入力の位置への変換（`quad` は入力の板の 4 隅: 左上・右上・右下・左下）。
pub fn rectify_homography(quad: &[Point; 4], out_w: usize, out_h: usize) -> Option<Homography> {
    Homography::from_points(&rect_corners(out_w, out_h), quad)
}

/// 透視変換で平面を作り直す。`out_to_src` は出力の連続座標 → 入力の連続座標。
/// 双線形で標本化し、縮小が大きいところは格子状に複数点を平均する（最大 4x4）。範囲外は `fill`。
pub fn warp_plane(
    src: &Plane,
    out_to_src: &Homography,
    out_w: usize,
    out_h: usize,
    fill: u8,
) -> Result<Plane> {
    let mut out = Plane::filled(out_w, out_h, fill)?;
    let (sw, sh) = (src.width() as f64, src.height() as f64);
    // 中央付近の縮小率から標本数を決める。
    let c = Point::new(out_w as f64 / 2.0, out_h as f64 / 2.0);
    let ss = match (
        out_to_src.apply(c),
        out_to_src.apply(Point::new(c.x + 1.0, c.y)),
        out_to_src.apply(Point::new(c.x, c.y + 1.0)),
    ) {
        (Some(a), Some(b), Some(d)) => a.dist(b).max(a.dist(d)).ceil().clamp(1.0, 4.0) as usize,
        _ => 1,
    };
    let inv = 1.0 / ss as f64;
    for y in 0..out_h {
        let row = out.row_mut(y);
        for (x, d) in row.iter_mut().enumerate() {
            let mut acc = 0.0;
            let mut n = 0;
            for sy in 0..ss {
                for sx in 0..ss {
                    let p = Point::new(
                        x as f64 + (sx as f64 + 0.5) * inv,
                        y as f64 + (sy as f64 + 0.5) * inv,
                    );
                    let Some(q) = out_to_src.apply(p) else {
                        continue;
                    };
                    if q.x < 0.0 || q.y < 0.0 || q.x > sw || q.y > sh {
                        continue;
                    }
                    acc += bilinear(src, q.x - 0.5, q.y - 0.5);
                    n += 1;
                }
            }
            if n > 0 {
                *d = (acc / n as f64 + 0.5).clamp(0.0, 255.0) as u8;
            }
        }
    }
    Ok(out)
}

/// 双線形の標本化（画素の添字の座標、端は延長）。
#[inline]
pub fn bilinear(p: &Plane, x: f64, y: f64) -> f64 {
    let x0 = x.floor();
    let y0 = y.floor();
    let (fx, fy) = (x - x0, y - y0);
    let (xi, yi) = (x0 as isize, y0 as isize);
    let a = p.get_clamped(xi, yi) as f64;
    let b = p.get_clamped(xi + 1, yi) as f64;
    let c = p.get_clamped(xi, yi + 1) as f64;
    let d = p.get_clamped(xi + 1, yi + 1) as f64;
    (a * (1.0 - fx) + b * fx) * (1.0 - fy) + (c * (1.0 - fx) + d * fx) * fy
}

/// I420 の台形補正: 入力の `quad`（左上・右上・右下・左下、輝度の座標）を `out_w`x`out_h` の長方形にする。
pub fn warp_i420(
    f: &I420Frame,
    quad: &[Point; 4],
    out_w: usize,
    out_h: usize,
    fill: YuvColor,
) -> Result<I420Frame> {
    let h =
        rectify_homography(quad, out_w, out_h).ok_or(ImageError::Mismatch("4 隅が退化している"))?;
    let (cw, ch) = chroma_size(out_w, out_h);
    // 色差: 出力の色差座標 * 2 → 輝度の出力 → 輝度の入力 → / 2
    let hc = h.rescaled(0.5, 0.5);
    I420Frame::from_planes(
        warp_plane(f.y(), &h, out_w, out_h, fill.y)?,
        warp_plane(f.u(), &hc, cw, ch, fill.u)?,
        warp_plane(f.v(), &hc, cw, ch, fill.v)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_corners_exactly() {
        let src = [
            Point::new(10.0, 20.0),
            Point::new(300.0, 5.0),
            Point::new(320.0, 250.0),
            Point::new(-5.0, 200.0),
        ];
        let dst = rect_corners(400, 300);
        let h = Homography::from_points(&src, &dst).unwrap();
        for i in 0..4 {
            let q = h.apply(src[i]).unwrap();
            assert!(q.dist(dst[i]) < 1e-9);
        }
        let inv = h.inverse().unwrap();
        let q = inv.apply(Point::new(200.0, 150.0)).unwrap();
        let back = h.apply(q).unwrap();
        assert!(back.dist(Point::new(200.0, 150.0)) < 1e-9);
    }

    #[test]
    fn degenerate_rejected() {
        let src = [
            Point::new(0.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(2.0, 2.0),
            Point::new(5.0, 0.0),
        ];
        assert!(Homography::from_points(&src, &rect_corners(10, 10)).is_none());
        let nan = [Point::new(f64::NAN, 0.0); 4];
        assert!(Homography::from_points(&nan, &rect_corners(10, 10)).is_none());
    }

    #[test]
    fn identity_warp_is_exact() {
        let p = Plane::from_fn(40, 30, |x, y| (x * 5 + y * 3) as u8).unwrap();
        let q = warp_plane(&p, &Homography::IDENTITY, 40, 30, 0).unwrap();
        assert_eq!(q, p);
    }
}
