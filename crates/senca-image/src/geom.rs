//! 幾何の操作: 切り抜き、余白付け（レターボックス）、90 度単位の回転、反転、貼り付け。

use crate::error::{ImageError, Result};
use crate::frame::{I420Frame, Plane, Rect, YuvColor, check_dims, chroma_size};
use crate::scale::{ScaleFilter, scale_i420};

/// 90 度単位の回転（時計回り）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    R0,
    R90,
    R180,
    R270,
}

/// 平面の切り抜き（複製）。
pub fn crop_plane(p: &Plane, r: Rect) -> Result<Plane> {
    if !r.fits_in(p.width(), p.height()) {
        return Err(ImageError::Mismatch("切り抜きの範囲が画像の外"));
    }
    let mut out = Plane::alloc(r.width, r.height, 0);
    for y in 0..r.height {
        out.row_mut(y)
            .copy_from_slice(&p.row(r.y + y)[r.x..r.x + r.width]);
    }
    Ok(out)
}

/// I420 の切り抜き。左上は偶数に切り下げる（色差の位置を保つため）。
pub fn crop_i420(f: &I420Frame, r: Rect) -> Result<I420Frame> {
    let r = Rect::new(r.x & !1, r.y & !1, r.width, r.height);
    if !r.fits_in(f.width(), f.height()) {
        return Err(ImageError::Mismatch("切り抜きの範囲が画像の外"));
    }
    let (cw, ch) = chroma_size(r.width, r.height);
    let cr = Rect::new(r.x / 2, r.y / 2, cw, ch);
    let cr = Rect::new(
        cr.x,
        cr.y,
        cw.min(f.u().width() - cr.x),
        ch.min(f.u().height() - cr.y),
    );
    let pad = |p: Plane| -> Plane {
        if p.width() == cw && p.height() == ch {
            return p;
        }
        let mut out = Plane::alloc(cw, ch, 128);
        for y in 0..ch {
            for x in 0..cw {
                out.set(x, y, p.get_clamped(x as isize, y as isize));
            }
        }
        out
    };
    I420Frame::from_planes(
        crop_plane(f.y(), r)?,
        pad(crop_plane(f.u(), cr)?),
        pad(crop_plane(f.v(), cr)?),
    )
}

/// 平面を回転する。
pub fn rotate_plane(p: &Plane, rot: Rotation) -> Plane {
    let (w, h) = (p.width(), p.height());
    match rot {
        Rotation::R0 => p.clone(),
        Rotation::R180 => {
            let mut out = Plane::alloc(w, h, 0);
            for y in 0..h {
                let s = p.row(h - 1 - y);
                for (d, &v) in out.row_mut(y).iter_mut().zip(s.iter().rev()) {
                    *d = v;
                }
            }
            out
        }
        Rotation::R90 => {
            // 出力 (X, Y) = 入力 (Y, h - 1 - X)
            let mut out = Plane::alloc(h, w, 0);
            for oy in 0..w {
                let row = out.row_mut(oy);
                for (ox, d) in row.iter_mut().enumerate() {
                    *d = p.get(oy, h - 1 - ox);
                }
            }
            out
        }
        Rotation::R270 => {
            // 出力 (X, Y) = 入力 (w - 1 - Y, X)
            let mut out = Plane::alloc(h, w, 0);
            for oy in 0..w {
                let row = out.row_mut(oy);
                for (ox, d) in row.iter_mut().enumerate() {
                    *d = p.get(w - 1 - oy, ox);
                }
            }
            out
        }
    }
}

/// I420 を回転する。
pub fn rotate_i420(f: &I420Frame, rot: Rotation) -> I420Frame {
    let (y, u, v) = (
        rotate_plane(f.y(), rot),
        rotate_plane(f.u(), rot),
        rotate_plane(f.v(), rot),
    );
    I420Frame::from_planes(y, u, v).unwrap_or_else(|_| unreachable!())
}

/// 左右反転。
pub fn flip_h_plane(p: &Plane) -> Plane {
    let mut out = p.clone();
    for y in 0..out.height() {
        out.row_mut(y).reverse();
    }
    out
}

/// 上下反転。
pub fn flip_v_plane(p: &Plane) -> Plane {
    let mut out = Plane::alloc(p.width(), p.height(), 0);
    for y in 0..p.height() {
        out.row_mut(y).copy_from_slice(p.row(p.height() - 1 - y));
    }
    out
}

/// I420 の反転（`horizontal` が真なら左右、偽なら上下）。
pub fn flip_i420(f: &I420Frame, horizontal: bool) -> I420Frame {
    let g = if horizontal {
        flip_h_plane
    } else {
        flip_v_plane
    };
    I420Frame::from_planes(g(f.y()), g(f.u()), g(f.v())).unwrap_or_else(|_| unreachable!())
}

/// `src` を `dst` の (x, y) に貼る（はみ出しは切る）。
pub fn blit_plane(dst: &mut Plane, src: &Plane, x: isize, y: isize) {
    for sy in 0..src.height() {
        let dy = y + sy as isize;
        if dy < 0 || dy >= dst.height() as isize {
            continue;
        }
        let x0 = x.max(0);
        let x1 = (x + src.width() as isize).min(dst.width() as isize);
        if x1 <= x0 {
            continue;
        }
        let s0 = (x0 - x) as usize;
        let n = (x1 - x0) as usize;
        dst.row_mut(dy as usize)[x0 as usize..x0 as usize + n]
            .copy_from_slice(&src.row(sy)[s0..s0 + n]);
    }
}

/// 縦横比を保って `bw`x`bh` に収まる最大の矩形（中央寄せ、偶数に揃える）。
pub fn fit_rect(src_w: usize, src_h: usize, bw: usize, bh: usize) -> Rect {
    if src_w == 0 || src_h == 0 || bw == 0 || bh == 0 {
        return Rect::new(0, 0, bw, bh);
    }
    let (w, h) = if src_w as u128 * bh as u128 >= src_h as u128 * bw as u128 {
        (
            bw,
            ((bw as u128 * src_h as u128 + src_w as u128 / 2) / src_w as u128) as usize,
        )
    } else {
        (
            ((bh as u128 * src_w as u128 + src_h as u128 / 2) / src_h as u128) as usize,
            bh,
        )
    };
    let even = |v: usize, max: usize| {
        if max >= 2 {
            (v & !1).clamp(2, max & !1).max(1)
        } else {
            max
        }
    };
    let (w, h) = (even(w, bw), even(h, bh));
    Rect::new(((bw - w) / 2) & !1, ((bh - h) / 2) & !1, w, h)
}

/// 縦横比を保って `dst_w`x`dst_h` に縮小・拡大し、余白を `fill` で埋める。
pub fn letterbox_i420(
    f: &I420Frame,
    dst_w: usize,
    dst_h: usize,
    filter: ScaleFilter,
    fill: YuvColor,
) -> Result<I420Frame> {
    check_dims(dst_w, dst_h)?;
    let r = fit_rect(f.width(), f.height(), dst_w, dst_h);
    let inner = scale_i420(f, r.width, r.height, filter)?;
    let mut out = I420Frame::new(dst_w, dst_h, fill)?;
    paste_i420(&mut out, &inner, r.x, r.y);
    Ok(out)
}

/// 周りに余白を足す（左と上は偶数に切り上げる）。
pub fn pad_i420(
    f: &I420Frame,
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
    fill: YuvColor,
) -> Result<I420Frame> {
    let (left, top) = (left.next_multiple_of(2), top.next_multiple_of(2));
    let w = f
        .width()
        .checked_add(left)
        .and_then(|v| v.checked_add(right));
    let h = f
        .height()
        .checked_add(top)
        .and_then(|v| v.checked_add(bottom));
    let (Some(w), Some(h)) = (w, h) else {
        return Err(ImageError::TooLarge {
            width: usize::MAX,
            height: usize::MAX,
        });
    };
    let mut out = I420Frame::new(w, h, fill)?;
    paste_i420(&mut out, f, left, top);
    Ok(out)
}

/// I420 を貼る（`x`・`y` は偶数に切り下げる）。
pub fn paste_i420(dst: &mut I420Frame, src: &I420Frame, x: usize, y: usize) {
    let (x, y) = (x & !1, y & !1);
    let (dy, du, dv) = dst.planes_mut();
    blit_plane(dy, src.y(), x as isize, y as isize);
    blit_plane(du, src.u(), (x / 2) as isize, (y / 2) as isize);
    blit_plane(dv, src.v(), (x / 2) as isize, (y / 2) as isize);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(w: usize, h: usize) -> Plane {
        Plane::from_fn(w, h, |x, y| (y * w + x) as u8).unwrap()
    }

    #[test]
    fn rotations() {
        let p = numbered(3, 2); // [0 1 2; 3 4 5]
        let r90 = rotate_plane(&p, Rotation::R90);
        assert_eq!((r90.width(), r90.height()), (2, 3));
        assert_eq!(r90.to_packed(), vec![3, 0, 4, 1, 5, 2]);
        let r270 = rotate_plane(&p, Rotation::R270);
        assert_eq!(r270.to_packed(), vec![2, 5, 1, 4, 0, 3]);
        assert_eq!(
            rotate_plane(&p, Rotation::R180).to_packed(),
            vec![5, 4, 3, 2, 1, 0]
        );
        assert_eq!(rotate_plane(&r90, Rotation::R270), p);
        assert_eq!(
            rotate_plane(&rotate_plane(&r90, Rotation::R90), Rotation::R90),
            rotate_plane(&p, Rotation::R270)
        );
    }

    #[test]
    fn flips() {
        let p = numbered(3, 2);
        assert_eq!(flip_h_plane(&p).to_packed(), vec![2, 1, 0, 5, 4, 3]);
        assert_eq!(flip_v_plane(&p).to_packed(), vec![3, 4, 5, 0, 1, 2]);
    }

    #[test]
    fn crop() {
        let p = numbered(4, 4);
        assert_eq!(
            crop_plane(&p, Rect::new(1, 2, 2, 2)).unwrap().to_packed(),
            vec![9, 10, 13, 14]
        );
        assert!(crop_plane(&p, Rect::new(3, 0, 2, 1)).is_err());
        assert!(crop_plane(&p, Rect::new(usize::MAX, 0, 2, 1)).is_err());
        let f = I420Frame::new(8, 8, YuvColor::new(1, 2, 3)).unwrap();
        let c = crop_i420(&f, Rect::new(3, 3, 5, 5)).unwrap();
        assert_eq!((c.width(), c.u().width()), (5, 3));
    }

    #[test]
    fn fit_and_letterbox() {
        assert_eq!(fit_rect(1920, 1080, 640, 640), Rect::new(0, 140, 640, 360));
        assert_eq!(fit_rect(1080, 1920, 640, 360), Rect::new(218, 0, 202, 360));
        let f = I420Frame::new(160, 90, YuvColor::new(200, 100, 150)).unwrap();
        let l =
            letterbox_i420(&f, 100, 100, ScaleFilter::Bilinear, YuvColor::BLACK_LIMITED).unwrap();
        assert_eq!(l.y().get(50, 0), 16);
        assert_eq!(l.y().get(50, 50), 200);
        assert_eq!(l.u().get(25, 25), 100);
    }

    #[test]
    fn pad() {
        let f = I420Frame::new(4, 4, YuvColor::new(200, 100, 150)).unwrap();
        let p = pad_i420(&f, 1, 2, 3, 0, YuvColor::BLACK_LIMITED).unwrap();
        assert_eq!((p.width(), p.height()), (9, 6));
        assert_eq!(p.y().get(2, 2), 200);
        assert_eq!(p.y().get(1, 2), 16);
    }
}
