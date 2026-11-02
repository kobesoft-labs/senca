//! 画質の指標: PSNR・SSIM・鮮明さ（ラプラシアンの分散）・輝度の統計・黒／白／平坦の判定。

use crate::color::ColorRange;
use crate::error::{ImageError, Result};
use crate::filter::gaussian_blur_f32;
use crate::frame::{I420Frame, Plane, PlaneF32, RgbImage};

fn same_size(a: &Plane, b: &Plane) -> Result<()> {
    if a.width() != b.width() || a.height() != b.height() {
        return Err(ImageError::Mismatch("大きさの違う平面"));
    }
    Ok(())
}

fn sse(a: &Plane, b: &Plane) -> u64 {
    let mut s = 0u64;
    for y in 0..a.height() {
        for (&p, &q) in a.row(y).iter().zip(b.row(y)) {
            let d = p as i64 - q as i64;
            s += (d * d) as u64;
        }
    }
    s
}

fn mse_to_psnr(mse: f64) -> f64 {
    if mse <= 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// 平均二乗誤差。
pub fn mse(a: &Plane, b: &Plane) -> Result<f64> {
    same_size(a, b)?;
    Ok(sse(a, b) as f64 / (a.width() * a.height()) as f64)
}

/// PSNR（dB）。同一なら無限大。
pub fn psnr(a: &Plane, b: &Plane) -> Result<f64> {
    Ok(mse_to_psnr(mse(a, b)?))
}

/// I420 の PSNR（3 平面の全標本をまとめた MSE から）。
pub fn psnr_i420(a: &I420Frame, b: &I420Frame) -> Result<f64> {
    let mut s = 0u64;
    let mut n = 0usize;
    for (p, q) in [(a.y(), b.y()), (a.u(), b.u()), (a.v(), b.v())] {
        same_size(p, q)?;
        s += sse(p, q);
        n += p.width() * p.height();
    }
    Ok(mse_to_psnr(s as f64 / n as f64))
}

/// RGB の PSNR。
pub fn psnr_rgb(a: &RgbImage, b: &RgbImage) -> Result<f64> {
    if a.width() != b.width() || a.height() != b.height() {
        return Err(ImageError::Mismatch("大きさの違う画像"));
    }
    let s: u64 = a
        .data()
        .iter()
        .zip(b.data())
        .map(|(&p, &q)| {
            let d = p as i64 - q as i64;
            (d * d) as u64
        })
        .sum();
    Ok(mse_to_psnr(s as f64 / a.data().len() as f64))
}

/// SSIM（ガウス窓 σ = 1.5、K1 = 0.01、K2 = 0.03、平均を返す）。
pub fn ssim(a: &Plane, b: &Plane) -> Result<f64> {
    same_size(a, b)?;
    let fa = PlaneF32::from_plane(a);
    let fb = PlaneF32::from_plane(b);
    let mul = |x: &PlaneF32, y: &PlaneF32| PlaneF32 {
        width: x.width,
        height: x.height,
        data: x.data.iter().zip(&y.data).map(|(p, q)| p * q).collect(),
    };
    let s = 1.5;
    let ma = gaussian_blur_f32(&fa, s);
    let mb = gaussian_blur_f32(&fb, s);
    let saa = gaussian_blur_f32(&mul(&fa, &fa), s);
    let sbb = gaussian_blur_f32(&mul(&fb, &fb), s);
    let sab = gaussian_blur_f32(&mul(&fa, &fb), s);
    let c1 = (0.01f64 * 255.0).powi(2);
    let c2 = (0.03f64 * 255.0).powi(2);
    let mut total = 0.0f64;
    for i in 0..fa.data.len() {
        let (mx, my) = (ma.data[i] as f64, mb.data[i] as f64);
        let vx = saa.data[i] as f64 - mx * mx;
        let vy = sbb.data[i] as f64 - my * my;
        let cxy = sab.data[i] as f64 - mx * my;
        total +=
            ((2.0 * mx * my + c1) * (2.0 * cxy + c2)) / ((mx * mx + my * my + c1) * (vx + vy + c2));
    }
    Ok(total / fa.data.len() as f64)
}

/// 鮮明さ: 4 近傍ラプラシアンの分散（内側の画素だけ）。ぼけた画像ほど小さい。
pub fn laplacian_variance(p: &Plane) -> f64 {
    let (w, h) = (p.width(), p.height());
    if w < 3 || h < 3 {
        return 0.0;
    }
    let mut s = 0.0f64;
    let mut s2 = 0.0f64;
    for y in 1..h - 1 {
        let (a, c, b) = (p.row(y - 1), p.row(y), p.row(y + 1));
        for x in 1..w - 1 {
            let l = a[x] as i32 + b[x] as i32 + c[x - 1] as i32 + c[x + 1] as i32 - 4 * c[x] as i32;
            s += l as f64;
            s2 += (l * l) as f64;
        }
    }
    let n = ((w - 2) * (h - 2)) as f64;
    let m = s / n;
    s2 / n - m * m
}

/// 輝度の統計（0〜255 の値）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LumaStats {
    pub mean: f64,
    pub std: f64,
    pub min: u8,
    pub max: u8,
    /// 2 パーセンタイル。
    pub p02: u8,
    /// 98 パーセンタイル。
    pub p98: u8,
    /// ヒストグラムのエントロピー（bit）。
    pub entropy: f64,
}

/// 輝度の統計を求める。
pub fn luma_stats(p: &Plane) -> LumaStats {
    let mut hist = [0u64; 256];
    for y in 0..p.height() {
        for &v in p.row(y) {
            hist[v as usize] += 1;
        }
    }
    let n = (p.width() * p.height()) as f64;
    let mut sum = 0.0;
    let mut sum2 = 0.0;
    for (v, &c) in hist.iter().enumerate() {
        sum += v as f64 * c as f64;
        sum2 += (v * v) as f64 * c as f64;
    }
    let mean = sum / n;
    let pct = |q: f64| -> u8 {
        let target = (q * n).ceil().max(1.0) as u64;
        let mut acc = 0u64;
        for (v, &c) in hist.iter().enumerate() {
            acc += c;
            if acc >= target {
                return v as u8;
            }
        }
        255
    };
    let entropy = hist
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let pr = c as f64 / n;
            -pr * pr.log2()
        })
        .sum();
    LumaStats {
        mean,
        std: (sum2 / n - mean * mean).max(0.0).sqrt(),
        min: hist.iter().position(|&c| c > 0).unwrap_or(0) as u8,
        max: hist.iter().rposition(|&c| c > 0).unwrap_or(0) as u8,
        p02: pct(0.02),
        p98: pct(0.98),
        entropy,
    }
}

/// 画面の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameClass {
    /// ほぼ黒（暗転・信号なし）。
    Black,
    /// ほぼ白（白飛び・白の画面）。
    White,
    /// 一様（単色・カラーバーではない平坦な画面）。
    Flat,
    /// 普通の画面。
    Normal,
}

/// 判定のしきい値（輝度は範囲で 0〜1 に正規化した値）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClassifyParams {
    /// 98 パーセンタイルがこれ以下なら黒。
    pub black_level: f32,
    /// 2 パーセンタイルがこれ以上なら白。
    pub white_level: f32,
    /// 標準偏差がこれ以下なら平坦。
    pub flat_std: f32,
}

impl Default for ClassifyParams {
    fn default() -> Self {
        Self {
            black_level: 0.10,
            white_level: 0.92,
            flat_std: 0.02,
        }
    }
}

/// 黒・白・平坦・普通を判定する。
pub fn classify(p: &Plane, range: ColorRange, params: &ClassifyParams) -> FrameClass {
    classify_stats(&luma_stats(p), range, params)
}

/// 統計から判定する。
pub fn classify_stats(s: &LumaStats, range: ColorRange, params: &ClassifyParams) -> FrameClass {
    let n = |v: f64| range.normalize_luma(v as f32);
    if n(s.p98 as f64) <= params.black_level {
        FrameClass::Black
    } else if n(s.p02 as f64) >= params.white_level {
        FrameClass::White
    } else if (s.std as f32) / (range.white() as f32 - range.black() as f32) <= params.flat_std {
        FrameClass::Flat
    } else {
        FrameClass::Normal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(w: usize, h: usize, seed: u32) -> Plane {
        let mut s = seed;
        Plane::from_fn(w, h, |_, _| {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s >> 24) as u8
        })
        .unwrap()
    }

    #[test]
    fn psnr_values() {
        let a = Plane::filled(10, 10, 100).unwrap();
        let mut b = a.clone();
        assert_eq!(psnr(&a, &b).unwrap(), f64::INFINITY);
        b.set(0, 0, 110); // MSE = 1
        assert!((psnr(&a, &b).unwrap() - 48.1308).abs() < 1e-3);
        assert!(psnr(&a, &Plane::filled(9, 10, 0).unwrap()).is_err());
    }

    #[test]
    fn ssim_behaviour() {
        let a = noise(64, 48, 1);
        assert!((ssim(&a, &a).unwrap() - 1.0).abs() < 1e-6);
        let b = noise(64, 48, 2);
        assert!(ssim(&a, &b).unwrap() < 0.1);
        let mut c = a.clone();
        c.map_in_place(|v| v.saturating_add(3));
        let s = ssim(&a, &c).unwrap();
        assert!(s > 0.95, "{s}");
    }

    #[test]
    fn sharpness_drops_with_blur() {
        let a = noise(64, 64, 3);
        let b = crate::filter::box_blur(&a, 2);
        assert!(laplacian_variance(&a) > 10.0 * laplacian_variance(&b));
    }

    #[test]
    fn classification() {
        let p = ClassifyParams::default();
        let r = ColorRange::Limited;
        assert_eq!(
            classify(&Plane::filled(32, 32, 16).unwrap(), r, &p),
            FrameClass::Black
        );
        assert_eq!(
            classify(&Plane::filled(32, 32, 235).unwrap(), r, &p),
            FrameClass::White
        );
        assert_eq!(
            classify(&Plane::filled(32, 32, 120).unwrap(), r, &p),
            FrameClass::Flat
        );
        assert_eq!(classify(&noise(32, 32, 9), r, &p), FrameClass::Normal);
        let s = luma_stats(&Plane::from_fn(10, 10, |x, y| (x + 10 * y) as u8).unwrap());
        assert_eq!((s.min, s.max, s.p02, s.p98), (0, 99, 1, 97));
        assert!((s.mean - 49.5).abs() < 1e-9);
    }
}
