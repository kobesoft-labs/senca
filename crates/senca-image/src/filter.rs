//! 近傍の処理: 箱型ぼかし、ガウスぼかし、最小値・最大値フィルター（モルフォロジー）、Sobel。

use crate::frame::{Plane, PlaneF32};

/// 1 行の箱型平均（半径 r、端は延長）。
fn box_line(src: &[f32], r: usize, out: &mut [f32]) {
    let n = src.len();
    if n == 0 {
        return;
    }
    let at = |i: isize| src[i.clamp(0, n as isize - 1) as usize];
    let win = (2 * r + 1) as f32;
    let mut sum: f32 = (-(r as isize)..=r as isize).map(at).sum();
    for (i, o) in out.iter_mut().enumerate().take(n) {
        *o = sum / win;
        sum += at(i as isize + r as isize + 1) - at(i as isize - r as isize);
    }
}

/// 箱型ぼかし（f32、分離型、端は延長）。累積和の誤差を避けるため行ごとに計算する。
pub fn box_blur_f32(p: &PlaneF32, r: usize) -> PlaneF32 {
    if r == 0 {
        return p.clone();
    }
    let (w, h) = (p.width, p.height);
    let mut tmp = p.clone();
    for y in 0..h {
        box_line(p.row(y), r, tmp.row_mut(y));
    }
    let mut out = tmp.clone();
    let mut col = vec![0.0; h];
    let mut res = vec![0.0; h];
    for x in 0..w {
        for (y, c) in col.iter_mut().enumerate() {
            *c = tmp.data[y * w + x];
        }
        box_line(&col, r, &mut res);
        for (y, &v) in res.iter().enumerate() {
            out.data[y * w + x] = v;
        }
    }
    out
}

/// 8 bit の箱型ぼかし。
pub fn box_blur(p: &Plane, r: usize) -> Plane {
    box_blur_f32(&PlaneF32::from_plane(p), r).to_plane()
}

/// ガウスの核（正規化済み、半径 ceil(3σ)）。
pub fn gaussian_kernel(sigma: f32) -> Vec<f32> {
    let sigma = sigma.max(0.01);
    let r = (3.0 * sigma).ceil() as usize;
    let mut k: Vec<f32> = (0..=2 * r)
        .map(|i| {
            let d = i as f32 - r as f32;
            (-d * d / (2.0 * sigma * sigma)).exp()
        })
        .collect();
    let s: f32 = k.iter().sum();
    k.iter_mut().for_each(|v| *v /= s);
    k
}

/// 分離型の畳み込み（端は延長）。
pub fn convolve_separable(p: &PlaneF32, k: &[f32]) -> PlaneF32 {
    let (w, h) = (p.width, p.height);
    let r = (k.len() / 2) as isize;
    let mut tmp = p.clone();
    for y in 0..h {
        let src = p.row(y);
        let dst = tmp.row_mut(y);
        for (x, d) in dst.iter_mut().enumerate() {
            let mut acc = 0.0;
            for (i, &kv) in k.iter().enumerate() {
                let sx = (x as isize + i as isize - r).clamp(0, w as isize - 1) as usize;
                acc += kv * src[sx];
            }
            *d = acc;
        }
    }
    let mut out = tmp.clone();
    for y in 0..h {
        let dst = out.row_mut(y);
        for (i, &kv) in k.iter().enumerate() {
            let sy = (y as isize + i as isize - r).clamp(0, h as isize - 1) as usize;
            let src = &tmp.data[sy * w..(sy + 1) * w];
            if i == 0 {
                for (d, &s) in dst.iter_mut().zip(src) {
                    *d = kv * s;
                }
            } else {
                for (d, &s) in dst.iter_mut().zip(src) {
                    *d += kv * s;
                }
            }
        }
    }
    out
}

/// ガウスぼかし（f32）。
pub fn gaussian_blur_f32(p: &PlaneF32, sigma: f32) -> PlaneF32 {
    convolve_separable(p, &gaussian_kernel(sigma))
}

/// 1 行の最小値・最大値（van Herk / Gil-Werman、1 画素あたり定数回の比較）。
fn minmax_line(src: &[u8], r: usize, is_max: bool, out: &mut [u8]) {
    let n = src.len();
    if r == 0 || n == 0 {
        out[..n].copy_from_slice(src);
        return;
    }
    let pick = |a: u8, b: u8| if is_max { a.max(b) } else { a.min(b) };
    let k = 2 * r + 1;
    // 両端を延長した列（長さ n + 2r）。
    let ext_len = n + 2 * r;
    let ext = |i: usize| src[(i as isize - r as isize).clamp(0, n as isize - 1) as usize];
    let mut g = vec![0u8; ext_len];
    let mut hh = vec![0u8; ext_len];
    for i in 0..ext_len {
        g[i] = if i % k == 0 {
            ext(i)
        } else {
            pick(g[i - 1], ext(i))
        };
    }
    for i in (0..ext_len).rev() {
        hh[i] = if i % k == k - 1 || i == ext_len - 1 {
            ext(i)
        } else {
            pick(hh[i + 1], ext(i))
        };
    }
    for (i, o) in out.iter_mut().enumerate().take(n) {
        // 窓は ext の [i, i + 2r]
        *o = pick(hh[i], g[i + 2 * r]);
    }
}

fn minmax_filter(p: &Plane, r: usize, is_max: bool) -> Plane {
    let (w, h) = (p.width(), p.height());
    let mut tmp = Plane::alloc(w, h, 0);
    for y in 0..h {
        minmax_line(p.row(y), r, is_max, tmp.row_mut(y));
    }
    let mut out = Plane::alloc(w, h, 0);
    let mut col = vec![0u8; h];
    let mut res = vec![0u8; h];
    for x in 0..w {
        for (y, c) in col.iter_mut().enumerate() {
            *c = tmp.get(x, y);
        }
        minmax_line(&col, r, is_max, &mut res);
        for (y, &v) in res.iter().enumerate() {
            out.set(x, y, v);
        }
    }
    out
}

/// 最小値フィルター（正方形の窓、半径 r）＝ 収縮（erode）。
pub fn min_filter(p: &Plane, r: usize) -> Plane {
    minmax_filter(p, r, false)
}

/// 最大値フィルター ＝ 膨張（dilate）。
pub fn max_filter(p: &Plane, r: usize) -> Plane {
    minmax_filter(p, r, true)
}

/// クロージング（膨張 → 収縮）。明るい背景の上の暗く細いもの（ホワイトボードの字）を消す。
pub fn closing(p: &Plane, r: usize) -> Plane {
    min_filter(&max_filter(p, r), r)
}

/// オープニング（収縮 → 膨張）。暗い背景の上の明るく細いもの（チョークの字）を消す。
pub fn opening(p: &Plane, r: usize) -> Plane {
    max_filter(&min_filter(p, r), r)
}

/// Sobel の勾配（gx, gy）。f32 の平面 2 つを返す。
pub fn sobel(p: &PlaneF32) -> (PlaneF32, PlaneF32) {
    let (w, h) = (p.width, p.height);
    let mut gx = PlaneF32 {
        width: w,
        height: h,
        data: vec![0.0; w * h],
    };
    let mut gy = gx.clone();
    let at = |x: isize, y: isize| {
        p.data[(y.clamp(0, h as isize - 1) as usize) * w + x.clamp(0, w as isize - 1) as usize]
    };
    for y in 0..h as isize {
        for x in 0..w as isize {
            let a = at(x - 1, y - 1);
            let b = at(x, y - 1);
            let c = at(x + 1, y - 1);
            let d = at(x - 1, y);
            let f = at(x + 1, y);
            let g = at(x - 1, y + 1);
            let hh = at(x, y + 1);
            let i = at(x + 1, y + 1);
            let idx = y as usize * w + x as usize;
            gx.data[idx] = (c + 2.0 * f + i) - (a + 2.0 * d + g);
            gy.data[idx] = (g + 2.0 * hh + i) - (a + 2.0 * b + c);
        }
    }
    (gx, gy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minmax_matches_naive() {
        let p = Plane::from_fn(23, 17, |x, y| ((x * 37 + y * 91) % 251) as u8).unwrap();
        for r in [1, 2, 5, 30] {
            let mx = max_filter(&p, r);
            let mn = min_filter(&p, r);
            for y in 0..17 {
                for x in 0..23 {
                    let mut a = 0u8;
                    let mut b = 255u8;
                    for dy in -(r as isize)..=r as isize {
                        for dx in -(r as isize)..=r as isize {
                            let v = p.get_clamped(x as isize + dx, y as isize + dy);
                            a = a.max(v);
                            b = b.min(v);
                        }
                    }
                    assert_eq!(mx.get(x, y), a, "max r{r} {x},{y}");
                    assert_eq!(mn.get(x, y), b, "min r{r} {x},{y}");
                }
            }
        }
    }

    #[test]
    fn closing_removes_thin_dark_line() {
        let p = Plane::from_fn(20, 20, |x, _| if x == 10 { 20 } else { 200 }).unwrap();
        let c = closing(&p, 2);
        assert!(c.to_packed().iter().all(|&v| v == 200));
    }

    #[test]
    fn blur_preserves_mean_of_flat() {
        let p = Plane::filled(9, 7, 123).unwrap();
        assert_eq!(box_blur(&p, 3), p);
        let g = gaussian_blur_f32(&PlaneF32::from_plane(&p), 1.5);
        assert!(g.data.iter().all(|v| (v - 123.0).abs() < 1e-3));
    }
}
