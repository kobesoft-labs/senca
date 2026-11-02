//! フレームの型: 8 bit の平面（`Plane`）、I420・NV12、RGB・RGBA、f32 の平面。
//!
//! すべて「行の先頭からの stride」を持つ平面の組み合わせで表す。大きさは [`Limits`] で検査する。

use crate::error::{ImageError, Result};
use bytes::Bytes;

/// 一辺の上限の既定値。
pub const MAX_DIMENSION: usize = 16384;
/// 画素数の上限の既定値（約 6700 万画素。RGBA で 256 MiB）。
pub const MAX_PIXELS: usize = 1 << 26;

/// 大きさの上限。復号器と、利用者が大きさを指定する関数で使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_width: usize,
    pub max_height: usize,
    pub max_pixels: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_width: MAX_DIMENSION,
            max_height: MAX_DIMENSION,
            max_pixels: MAX_PIXELS,
        }
    }
}

impl Limits {
    /// 大きさが 0 でなく上限内か検査する。
    pub fn check(&self, width: usize, height: usize) -> Result<()> {
        if width == 0 || height == 0 {
            return Err(ImageError::InvalidDimensions { width, height });
        }
        if width > self.max_width
            || height > self.max_height
            || width
                .checked_mul(height)
                .is_none_or(|p| p > self.max_pixels)
        {
            return Err(ImageError::TooLarge { width, height });
        }
        Ok(())
    }
}

/// 既定の上限で大きさを検査する。
pub fn check_dims(width: usize, height: usize) -> Result<()> {
    Limits::default().check(width, height)
}

/// 画素の矩形（左上と大きさ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Rect {
    pub const fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
    /// `w`x`h` の画像の内側に収まるか。
    pub fn fits_in(&self, w: usize, h: usize) -> bool {
        self.width > 0
            && self.height > 0
            && self.x.checked_add(self.width).is_some_and(|r| r <= w)
            && self.y.checked_add(self.height).is_some_and(|b| b <= h)
    }
}

/// 小数の矩形（拡大縮小の元の範囲などに使う）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RectF {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl RectF {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

impl From<Rect> for RectF {
    fn from(r: Rect) -> Self {
        Self {
            x: r.x as f64,
            y: r.y as f64,
            width: r.width as f64,
            height: r.height as f64,
        }
    }
}

/// 8 bit の 1 平面。`stride >= width`、`data.len() >= stride * height`。
#[derive(Clone, PartialEq, Eq)]
pub struct Plane {
    width: usize,
    height: usize,
    stride: usize,
    data: Vec<u8>,
}

/// 輝度だけの画像（8 bit の 1 平面）。
pub type GrayImage = Plane;

impl std::fmt::Debug for Plane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Plane({}x{}, stride {})",
            self.width, self.height, self.stride
        )
    }
}

impl Plane {
    /// 0 で埋めた平面を作る（大きさを検査する）。
    pub fn new(width: usize, height: usize) -> Result<Self> {
        check_dims(width, height)?;
        Ok(Self::alloc(width, height, 0))
    }

    /// 指定の値で埋めた平面を作る。
    pub fn filled(width: usize, height: usize, value: u8) -> Result<Self> {
        check_dims(width, height)?;
        Ok(Self::alloc(width, height, value))
    }

    /// 検査済みの大きさで確保する（crate 内部用）。
    pub(crate) fn alloc(width: usize, height: usize, value: u8) -> Self {
        Self {
            width,
            height,
            stride: width,
            data: vec![value; width * height],
        }
    }

    /// 詰めて並んだ画素列から作る（`data.len() == width * height`）。
    pub fn from_vec(width: usize, height: usize, data: Vec<u8>) -> Result<Self> {
        Self::from_vec_with_stride(width, height, width, data)
    }

    /// stride 付きの画素列から作る。
    pub fn from_vec_with_stride(
        width: usize,
        height: usize,
        stride: usize,
        data: Vec<u8>,
    ) -> Result<Self> {
        check_dims(width, height)?;
        if stride < width {
            return Err(ImageError::Mismatch("stride が幅より小さい"));
        }
        let need = stride
            .checked_mul(height - 1)
            .and_then(|v| v.checked_add(width))
            .ok_or(ImageError::TooLarge { width, height })?;
        if data.len() < need {
            return Err(ImageError::Truncated);
        }
        Ok(Self {
            width,
            height,
            stride,
            data,
        })
    }

    /// 関数で各画素を決めて作る。
    pub fn from_fn(
        width: usize,
        height: usize,
        mut f: impl FnMut(usize, usize) -> u8,
    ) -> Result<Self> {
        let mut p = Self::new(width, height)?;
        for y in 0..height {
            for (x, v) in p.row_mut(y).iter_mut().enumerate() {
                *v = f(x, y);
            }
        }
        Ok(p)
    }

    pub fn width(&self) -> usize {
        self.width
    }
    pub fn height(&self) -> usize {
        self.height
    }
    pub fn stride(&self) -> usize {
        self.stride
    }
    /// stride 込みの生データ。
    pub fn data(&self) -> &[u8] {
        &self.data
    }
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// `y` 行目（幅ぶん）。
    #[inline]
    pub fn row(&self, y: usize) -> &[u8] {
        let s = y * self.stride;
        &self.data[s..s + self.width]
    }

    #[inline]
    pub fn row_mut(&mut self, y: usize) -> &mut [u8] {
        let s = y * self.stride;
        &mut self.data[s..s + self.width]
    }

    /// 画素の値。範囲外は端の値（clamp）。
    #[inline]
    pub fn get_clamped(&self, x: isize, y: isize) -> u8 {
        let x = x.clamp(0, self.width as isize - 1) as usize;
        let y = y.clamp(0, self.height as isize - 1) as usize;
        self.data[y * self.stride + x]
    }

    #[inline]
    pub fn get(&self, x: usize, y: usize) -> u8 {
        self.data[y * self.stride + x]
    }

    #[inline]
    pub fn set(&mut self, x: usize, y: usize, v: u8) {
        self.data[y * self.stride + x] = v;
    }

    /// 全画素を値で埋める。
    pub fn fill(&mut self, v: u8) {
        for y in 0..self.height {
            self.row_mut(y).fill(v);
        }
    }

    /// stride を詰めた画素列（width * height）。
    pub fn to_packed(&self) -> Vec<u8> {
        if self.stride == self.width {
            return self.data[..self.width * self.height].to_vec();
        }
        let mut out = Vec::with_capacity(self.width * self.height);
        for y in 0..self.height {
            out.extend_from_slice(self.row(y));
        }
        out
    }

    /// stride を幅に詰めた平面を返す。
    pub fn compact(self) -> Self {
        if self.stride == self.width && self.data.len() == self.width * self.height {
            return self;
        }
        let data = self.to_packed();
        Self {
            width: self.width,
            height: self.height,
            stride: self.width,
            data,
        }
    }

    /// 全画素に関数を当てる。
    pub fn map_in_place(&mut self, mut f: impl FnMut(u8) -> u8) {
        for y in 0..self.height {
            for v in self.row_mut(y) {
                *v = f(*v);
            }
        }
    }
}

/// f32 の 1 平面（stride = width）。中間計算用。
#[derive(Clone, PartialEq, Debug)]
pub struct PlaneF32 {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
}

impl PlaneF32 {
    /// 値で埋めた平面（大きさを検査する）。
    pub fn filled(width: usize, height: usize, v: f32) -> Result<Self> {
        check_dims(width, height)?;
        Ok(Self {
            width,
            height,
            data: vec![v; width * height],
        })
    }

    /// 8 bit の平面を f32（0〜255）にする。
    pub fn from_plane(p: &Plane) -> Self {
        let mut data = Vec::with_capacity(p.width() * p.height());
        for y in 0..p.height() {
            data.extend(p.row(y).iter().map(|&v| v as f32));
        }
        Self {
            width: p.width(),
            height: p.height(),
            data,
        }
    }

    /// 丸めて 0〜255 に収めた 8 bit の平面にする。
    pub fn to_plane(&self) -> Plane {
        let data = self.data.iter().map(|&v| clamp_u8(v)).collect();
        Plane {
            width: self.width,
            height: self.height,
            stride: self.width,
            data,
        }
    }

    #[inline]
    pub fn get(&self, x: usize, y: usize) -> f32 {
        self.data[y * self.width + x]
    }
    #[inline]
    pub fn set(&mut self, x: usize, y: usize, v: f32) {
        self.data[y * self.width + x] = v;
    }
    #[inline]
    pub fn row(&self, y: usize) -> &[f32] {
        &self.data[y * self.width..(y + 1) * self.width]
    }
    #[inline]
    pub fn row_mut(&mut self, y: usize) -> &mut [f32] {
        &mut self.data[y * self.width..(y + 1) * self.width]
    }
}

/// f32 を丸めて 0〜255 に収める。NaN は 0。
#[inline]
pub fn clamp_u8(v: f32) -> u8 {
    if v.is_nan() {
        0
    } else {
        (v + 0.5).clamp(0.0, 255.0) as u8
    }
}

/// 4:2:0 の色差平面の大きさ。
#[inline]
pub const fn chroma_size(width: usize, height: usize) -> (usize, usize) {
    (width.div_ceil(2), height.div_ceil(2))
}

/// YUV の 1 色（8 bit）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YuvColor {
    pub y: u8,
    pub u: u8,
    pub v: u8,
}

impl YuvColor {
    pub const fn new(y: u8, u: u8, v: u8) -> Self {
        Self { y, u, v }
    }
    /// 限定範囲の黒。
    pub const BLACK_LIMITED: Self = Self::new(16, 128, 128);
    /// 全範囲の黒。
    pub const BLACK_FULL: Self = Self::new(0, 128, 128);
    /// 限定範囲の白。
    pub const WHITE_LIMITED: Self = Self::new(235, 128, 128);
}

/// 4:2:0 の planar（Y・U・V の 3 平面）。奇数の大きさも扱う（色差は切り上げ）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct I420Frame {
    width: usize,
    height: usize,
    y: Plane,
    u: Plane,
    v: Plane,
}

impl I420Frame {
    /// 指定の色で埋めたフレーム。
    pub fn new(width: usize, height: usize, fill: YuvColor) -> Result<Self> {
        check_dims(width, height)?;
        let (cw, ch) = chroma_size(width, height);
        Ok(Self {
            width,
            height,
            y: Plane::alloc(width, height, fill.y),
            u: Plane::alloc(cw, ch, fill.u),
            v: Plane::alloc(cw, ch, fill.v),
        })
    }

    /// 3 平面から作る（大きさを検査する）。
    pub fn from_planes(y: Plane, u: Plane, v: Plane) -> Result<Self> {
        let (cw, ch) = chroma_size(y.width(), y.height());
        if u.width() != cw || u.height() != ch || v.width() != cw || v.height() != ch {
            return Err(ImageError::Mismatch("I420 の色差平面の大きさ"));
        }
        Ok(Self {
            width: y.width(),
            height: y.height(),
            y,
            u,
            v,
        })
    }

    /// 詰めて並んだ I420（Y, U, V の順）から作る。FFmpeg の `-pix_fmt yuv420p` の rawvideo と同じ並び。
    pub fn from_packed(width: usize, height: usize, data: &[u8]) -> Result<Self> {
        check_dims(width, height)?;
        let (cw, ch) = chroma_size(width, height);
        let ys = width * height;
        let cs = cw * ch;
        if data.len() != ys + 2 * cs {
            return Err(ImageError::Mismatch("I420 の長さ"));
        }
        Self::from_planes(
            Plane::from_vec(width, height, data[..ys].to_vec())?,
            Plane::from_vec(cw, ch, data[ys..ys + cs].to_vec())?,
            Plane::from_vec(cw, ch, data[ys + cs..].to_vec())?,
        )
    }

    /// 詰めた I420 の並びを `Bytes` で返す（受け渡し用）。
    pub fn to_bytes(&self) -> Bytes {
        let mut out = Vec::with_capacity(self.packed_len());
        for p in [&self.y, &self.u, &self.v] {
            for r in 0..p.height() {
                out.extend_from_slice(p.row(r));
            }
        }
        Bytes::from(out)
    }

    /// 詰めた I420 の長さ。
    pub fn packed_len(&self) -> usize {
        let (cw, ch) = chroma_size(self.width, self.height);
        self.width * self.height + 2 * cw * ch
    }

    pub fn width(&self) -> usize {
        self.width
    }
    pub fn height(&self) -> usize {
        self.height
    }
    pub fn y(&self) -> &Plane {
        &self.y
    }
    pub fn u(&self) -> &Plane {
        &self.u
    }
    pub fn v(&self) -> &Plane {
        &self.v
    }
    pub fn y_mut(&mut self) -> &mut Plane {
        &mut self.y
    }
    pub fn u_mut(&mut self) -> &mut Plane {
        &mut self.u
    }
    pub fn v_mut(&mut self) -> &mut Plane {
        &mut self.v
    }
    /// 3 平面を同時に可変で借りる。
    pub fn planes_mut(&mut self) -> (&mut Plane, &mut Plane, &mut Plane) {
        (&mut self.y, &mut self.u, &mut self.v)
    }
    /// 3 平面に分解する。
    pub fn into_planes(self) -> (Plane, Plane, Plane) {
        (self.y, self.u, self.v)
    }
    /// 全体を 1 色で埋める。
    pub fn fill(&mut self, c: YuvColor) {
        self.y.fill(c.y);
        self.u.fill(c.u);
        self.v.fill(c.v);
    }
}

/// NV12（Y 平面 + U/V を交互に並べた平面）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Nv12Frame {
    width: usize,
    height: usize,
    y: Plane,
    uv: Plane,
}

impl Nv12Frame {
    /// 平面から作る。`uv` は幅 `2 * ceil(w/2)`、高さ `ceil(h/2)`。
    pub fn from_planes(y: Plane, uv: Plane) -> Result<Self> {
        let (cw, ch) = chroma_size(y.width(), y.height());
        if uv.width() != cw * 2 || uv.height() != ch {
            return Err(ImageError::Mismatch("NV12 の UV 平面の大きさ"));
        }
        Ok(Self {
            width: y.width(),
            height: y.height(),
            y,
            uv,
        })
    }
    pub fn width(&self) -> usize {
        self.width
    }
    pub fn height(&self) -> usize {
        self.height
    }
    pub fn y(&self) -> &Plane {
        &self.y
    }
    pub fn uv(&self) -> &Plane {
        &self.uv
    }

    /// I420 から作る（色差を交互に並べ直すだけ）。
    pub fn from_i420(f: &I420Frame) -> Self {
        let (cw, ch) = chroma_size(f.width, f.height);
        let mut uv = Plane::alloc(cw * 2, ch, 128);
        for r in 0..ch {
            let (us, vs) = (f.u.row(r), f.v.row(r));
            for (i, d) in uv.row_mut(r).as_chunks_mut::<2>().0.iter_mut().enumerate() {
                d[0] = us[i];
                d[1] = vs[i];
            }
        }
        Self {
            width: f.width,
            height: f.height,
            y: f.y.clone(),
            uv,
        }
    }

    /// I420 にする。
    pub fn to_i420(&self) -> I420Frame {
        let (cw, ch) = chroma_size(self.width, self.height);
        let mut u = Plane::alloc(cw, ch, 128);
        let mut v = Plane::alloc(cw, ch, 128);
        for r in 0..ch {
            let s = self.uv.row(r);
            for (i, d) in u.row_mut(r).iter_mut().enumerate() {
                *d = s[2 * i];
            }
            for (i, d) in v.row_mut(r).iter_mut().enumerate() {
                *d = s[2 * i + 1];
            }
        }
        I420Frame {
            width: self.width,
            height: self.height,
            y: self.y.clone(),
            u,
            v,
        }
    }
}

macro_rules! packed_image {
    ($name:ident, $bpp:expr, $doc:expr) => {
        #[doc = $doc]
        #[derive(Clone, PartialEq, Eq)]
        pub struct $name {
            width: usize,
            height: usize,
            data: Vec<u8>,
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}({}x{})", stringify!($name), self.width, self.height)
            }
        }

        impl $name {
            /// 1 画素のバイト数。
            pub const BPP: usize = $bpp;

            /// 0 で埋めた画像。
            pub fn new(width: usize, height: usize) -> Result<Self> {
                check_dims(width, height)?;
                Ok(Self {
                    width,
                    height,
                    data: vec![0; width * height * $bpp],
                })
            }
            /// 詰めた画素列から作る。
            pub fn from_vec(width: usize, height: usize, data: Vec<u8>) -> Result<Self> {
                check_dims(width, height)?;
                if data.len() != width * height * $bpp {
                    return Err(ImageError::Mismatch("画素列の長さ"));
                }
                Ok(Self {
                    width,
                    height,
                    data,
                })
            }
            pub fn width(&self) -> usize {
                self.width
            }
            pub fn height(&self) -> usize {
                self.height
            }
            pub fn data(&self) -> &[u8] {
                &self.data
            }
            pub fn data_mut(&mut self) -> &mut [u8] {
                &mut self.data
            }
            pub fn into_vec(self) -> Vec<u8> {
                self.data
            }
            #[inline]
            pub fn row(&self, y: usize) -> &[u8] {
                let s = y * self.width * $bpp;
                &self.data[s..s + self.width * $bpp]
            }
            #[inline]
            pub fn row_mut(&mut self, y: usize) -> &mut [u8] {
                let s = y * self.width * $bpp;
                &mut self.data[s..s + self.width * $bpp]
            }
            /// 画素（`BPP` バイト）。
            #[inline]
            pub fn pixel(&self, x: usize, y: usize) -> &[u8] {
                let s = (y * self.width + x) * $bpp;
                &self.data[s..s + $bpp]
            }
            #[inline]
            pub fn pixel_mut(&mut self, x: usize, y: usize) -> &mut [u8] {
                let s = (y * self.width + x) * $bpp;
                &mut self.data[s..s + $bpp]
            }
        }
    };
}

packed_image!(RgbImage, 3, "RGB 8 bit（R, G, B の順に詰めて並べる）。");
packed_image!(RgbaImage, 4, "RGBA 8 bit（straight alpha）。");

impl RgbImage {
    /// アルファ 255 の RGBA にする。
    pub fn to_rgba(&self) -> RgbaImage {
        let mut data = Vec::with_capacity(self.width * self.height * 4);
        for p in self.data.as_chunks::<3>().0 {
            data.extend_from_slice(&[p[0], p[1], p[2], 255]);
        }
        RgbaImage {
            width: self.width,
            height: self.height,
            data,
        }
    }
}

impl RgbaImage {
    /// アルファを捨てて RGB にする。
    pub fn to_rgb(&self) -> RgbImage {
        let mut data = Vec::with_capacity(self.width * self.height * 3);
        for p in self.data.as_chunks::<4>().0 {
            data.extend_from_slice(&p[..3]);
        }
        RgbImage {
            width: self.width,
            height: self.height,
            data,
        }
    }
    /// アルファの平面。
    pub fn alpha_plane(&self) -> Plane {
        let data = self.data.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
        Plane {
            width: self.width,
            height: self.height,
            stride: self.width,
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits() {
        assert!(Plane::new(0, 4).is_err());
        assert!(Plane::new(16385, 1).is_err());
        assert!(Plane::new(16384, 16384).is_err());
        assert!(Limits::default().check(8192, 8192).is_ok());
        assert!(Limits::default().check(8192, 8193).is_err());
        assert!(Plane::new(4, 4).is_ok());
    }

    #[test]
    fn i420_packed_roundtrip() {
        let w = 5;
        let h = 3;
        let data: Vec<u8> = (0..(15 + 2 * 3 * 2)).map(|v| v as u8).collect();
        let f = I420Frame::from_packed(w, h, &data).unwrap();
        assert_eq!(f.u().width(), 3);
        assert_eq!(f.u().height(), 2);
        assert_eq!(&f.to_bytes()[..], &data[..]);
        assert!(I420Frame::from_packed(w, h, &data[1..]).is_err());
    }

    #[test]
    fn nv12_roundtrip() {
        let data: Vec<u8> = (0..(16 + 8)).map(|v| (v * 7) as u8).collect();
        let f = I420Frame::from_packed(4, 4, &data).unwrap();
        let n = Nv12Frame::from_i420(&f);
        assert_eq!(n.uv().row(0), &[data[16], data[20], data[17], data[21]]);
        assert_eq!(n.to_i420(), f);
    }

    #[test]
    fn stride_plane() {
        let p = Plane::from_vec_with_stride(2, 2, 3, vec![1, 2, 9, 3, 4]).unwrap();
        assert_eq!(p.to_packed(), vec![1, 2, 3, 4]);
        assert!(Plane::from_vec_with_stride(2, 2, 3, vec![1, 2, 9, 3]).is_err());
        assert!(Plane::from_vec_with_stride(3, 2, 2, vec![0; 10]).is_err());
    }
}
