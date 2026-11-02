//! PNG の符号化・復号（自前の DEFLATE を使う）。
//!
//! - 符号化: グレー・RGB・RGBA の 8 bit。行ごとに 5 種のフィルターから「差の絶対値の和」が最小のものを選ぶ。
//! - 復号: 全ての色型（グレー・RGB・パレット・グレー＋α・RGBA）、1/2/4/8/16 bit、Adam7 インターレース、tRNS。
//!   出力は RGBA 8 bit（16 bit は上位バイト）。CRC・大きさの上限・チャンクの順序を厳格に検査する。

use crate::checksum::{crc32, crc32_update};
use crate::deflate::{zlib_compress, zlib_decompress};
use crate::error::{ImageError, Result};
use crate::frame::{Limits, Plane, RgbImage, RgbaImage};

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// 符号化する画素の並び。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PngColor {
    Gray,
    Rgb,
    Rgba,
}

impl PngColor {
    fn channels(self) -> usize {
        match self {
            Self::Gray => 1,
            Self::Rgb => 3,
            Self::Rgba => 4,
        }
    }
    fn color_type(self) -> u8 {
        match self {
            Self::Gray => 0,
            Self::Rgb => 2,
            Self::Rgba => 6,
        }
    }
}

fn put_chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    let c = crc32_update(crc32(ty), data);
    out.extend_from_slice(&c.to_be_bytes());
}

#[inline]
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let pa = (p - a as i16).abs();
    let pb = (p - b as i16).abs();
    let pc = (p - c as i16).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// 詰めて並んだ画素列を PNG にする。`level` は DEFLATE の段階（0〜9、既定 6）。
pub fn encode_png(
    width: usize,
    height: usize,
    color: PngColor,
    pixels: &[u8],
    level: u8,
) -> Result<Vec<u8>> {
    Limits::default().check(width, height)?;
    let bpp = color.channels();
    let stride = width * bpp;
    if pixels.len() != stride * height {
        return Err(ImageError::Mismatch("PNG の画素列の長さ"));
    }
    let mut raw = Vec::with_capacity((stride + 1) * height);
    let zero = vec![0u8; stride];
    let mut cand = vec![vec![0u8; stride]; 5];
    for y in 0..height {
        let cur = &pixels[y * stride..(y + 1) * stride];
        let up = if y > 0 {
            &pixels[(y - 1) * stride..y * stride]
        } else {
            &zero[..]
        };
        if level == 0 {
            raw.push(0);
            raw.extend_from_slice(cur);
            continue;
        }
        for i in 0..stride {
            let a = if i >= bpp { cur[i - bpp] } else { 0 };
            let b = up[i];
            let c = if i >= bpp { up[i - bpp] } else { 0 };
            let x = cur[i];
            cand[0][i] = x;
            cand[1][i] = x.wrapping_sub(a);
            cand[2][i] = x.wrapping_sub(b);
            cand[3][i] = x.wrapping_sub(((a as u16 + b as u16) / 2) as u8);
            cand[4][i] = x.wrapping_sub(paeth(a, b, c));
        }
        let cost = |v: &[u8]| {
            v.iter()
                .map(|&b| (b as i8).unsigned_abs() as u64)
                .sum::<u64>()
        };
        let best = (0..5).min_by_key(|&f| cost(&cand[f])).unwrap_or(0);
        raw.push(best as u8);
        raw.extend_from_slice(&cand[best]);
    }
    let mut out = SIGNATURE.to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, color.color_type(), 0, 0, 0]);
    put_chunk(&mut out, b"IHDR", &ihdr);
    let z = zlib_compress(&raw, level);
    for c in z.chunks(1 << 20) {
        put_chunk(&mut out, b"IDAT", c);
    }
    put_chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

/// グレーの平面を PNG にする。
pub fn encode_png_gray(p: &Plane, level: u8) -> Result<Vec<u8>> {
    encode_png(p.width(), p.height(), PngColor::Gray, &p.to_packed(), level)
}

/// RGB を PNG にする。
pub fn encode_png_rgb(img: &RgbImage, level: u8) -> Result<Vec<u8>> {
    encode_png(img.width(), img.height(), PngColor::Rgb, img.data(), level)
}

/// RGBA を PNG にする。
pub fn encode_png_rgba(img: &RgbaImage, level: u8) -> Result<Vec<u8>> {
    encode_png(img.width(), img.height(), PngColor::Rgba, img.data(), level)
}

/// 復号結果。
#[derive(Debug, Clone)]
pub struct PngDecoded {
    pub image: RgbaImage,
    /// 元の色型（0 グレー、2 RGB、3 パレット、4 グレー＋α、6 RGBA）。
    pub color_type: u8,
    pub bit_depth: u8,
    pub interlaced: bool,
}

const ADAM7: [(usize, usize, usize, usize); 7] = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

/// PNG を復号する。
pub fn decode_png(data: &[u8], limits: &Limits) -> Result<PngDecoded> {
    if data.len() < 8 || data[..8] != SIGNATURE {
        return Err(ImageError::Format("PNG の署名"));
    }
    let mut pos = 8;
    let mut hdr: Option<(usize, usize, u8, u8, bool)> = None;
    let mut palette: Vec<[u8; 4]> = Vec::new();
    let mut trns: Option<Vec<u8>> = None;
    let mut idat: Vec<u8> = Vec::new();
    let mut idat_done = false;
    let mut seen_iend = false;
    let mut max_idat = 0usize;
    while pos < data.len() {
        if pos + 12 > data.len() {
            return Err(ImageError::Truncated);
        }
        let len =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        if len > 0x7FFF_FFFF {
            return Err(ImageError::Format("チャンクの長さ"));
        }
        let ty: [u8; 4] = [data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]];
        let body_end = pos.checked_add(8 + len).ok_or(ImageError::Truncated)?;
        if body_end + 4 > data.len() {
            return Err(ImageError::Truncated);
        }
        let body = &data[pos + 8..body_end];
        let crc = u32::from_be_bytes([
            data[body_end],
            data[body_end + 1],
            data[body_end + 2],
            data[body_end + 3],
        ]);
        if crc32_update(crc32(&ty), body) != crc {
            return Err(ImageError::Checksum("PNG のチャンク CRC"));
        }
        pos = body_end + 4;
        if hdr.is_none() && &ty != b"IHDR" {
            return Err(ImageError::Format("IHDR が先頭にない"));
        }
        if &ty != b"IDAT" && !idat.is_empty() {
            idat_done = true;
        }
        match &ty {
            b"IHDR" => {
                if hdr.is_some() || len != 13 {
                    return Err(ImageError::Format("IHDR"));
                }
                let w = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
                let h = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
                let (depth, ct, comp, filt, inter) =
                    (body[8], body[9], body[10], body[11], body[12]);
                let ok = matches!(
                    (ct, depth),
                    (0, 1 | 2 | 4 | 8 | 16)
                        | (2, 8 | 16)
                        | (3, 1 | 2 | 4 | 8)
                        | (4, 8 | 16)
                        | (6, 8 | 16)
                );
                if !ok || comp != 0 || filt != 0 || inter > 1 {
                    return Err(ImageError::Format("IHDR の値"));
                }
                limits.check(w, h)?;
                let ch = channels(ct);
                let bits = depth as usize * ch;
                let mut raw = 0usize;
                if inter == 1 {
                    for &(xs, ys, dx, dy) in &ADAM7 {
                        let pw = if w > xs { (w - xs).div_ceil(dx) } else { 0 };
                        let ph = if h > ys { (h - ys).div_ceil(dy) } else { 0 };
                        if pw > 0 && ph > 0 {
                            raw += ph * (1 + (pw * bits).div_ceil(8));
                        }
                    }
                } else {
                    raw = h * (1 + (w * bits).div_ceil(8));
                }
                // 圧縮後が生データの 2 倍＋1 MiB を超えることはまずない。超えたら拒否する。
                max_idat = raw.saturating_mul(2).saturating_add(1 << 20);
                hdr = Some((w, h, depth, ct, inter == 1));
            }
            b"PLTE" => {
                if !idat.is_empty()
                    || !len.is_multiple_of(3)
                    || len == 0
                    || len > 768
                    || !palette.is_empty()
                {
                    return Err(ImageError::Format("PLTE"));
                }
                palette = body
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|c| [c[0], c[1], c[2], 255])
                    .collect();
            }
            b"tRNS" => {
                if !idat.is_empty() {
                    return Err(ImageError::Format("tRNS の位置"));
                }
                trns = Some(body.to_vec());
            }
            b"IDAT" => {
                if idat_done {
                    return Err(ImageError::Format("IDAT が連続していない"));
                }
                if idat.len() + len > max_idat {
                    return Err(ImageError::OutputLimit);
                }
                idat.extend_from_slice(body);
            }
            b"IEND" => {
                seen_iend = true;
                break;
            }
            _ => {
                if ty[0] & 0x20 == 0 {
                    return Err(ImageError::Unsupported("未知の必須チャンク"));
                }
            }
        }
    }
    if !seen_iend {
        return Err(ImageError::Truncated);
    }
    let (w, h, depth, ct, interlaced) = hdr.ok_or(ImageError::Format("IHDR がない"))?;
    if ct == 3 && palette.is_empty() {
        return Err(ImageError::Format("パレットがない"));
    }
    if let Some(t) = &trns {
        match ct {
            0 if t.len() != 2 => return Err(ImageError::Format("tRNS（グレー）")),
            2 if t.len() != 6 => return Err(ImageError::Format("tRNS（RGB）")),
            3 => {
                if t.len() > palette.len() {
                    return Err(ImageError::Format("tRNS（パレット）"));
                }
                for (i, &a) in t.iter().enumerate() {
                    palette[i][3] = a;
                }
            }
            4 | 6 => return Err(ImageError::Format("α付きの色型に tRNS")),
            _ => {}
        }
    }
    let ch = channels(ct);
    let bits = depth as usize * ch;
    let fbpp = bits.div_ceil(8).max(1);
    // 期待する生データの大きさをちょうどに展開する。
    let mut passes: Vec<(usize, usize, usize, usize, usize, usize)> = Vec::new();
    if interlaced {
        for &(xs, ys, dx, dy) in &ADAM7 {
            let pw = if w > xs { (w - xs).div_ceil(dx) } else { 0 };
            let ph = if h > ys { (h - ys).div_ceil(dy) } else { 0 };
            if pw > 0 && ph > 0 {
                passes.push((xs, ys, dx, dy, pw, ph));
            }
        }
    } else {
        passes.push((0, 0, 1, 1, w, h));
    }
    let total: usize = passes
        .iter()
        .map(|p| p.5 * (1 + (p.4 * bits).div_ceil(8)))
        .sum();
    let raw = zlib_decompress(&idat, total)?;
    if raw.len() != total {
        return Err(ImageError::Format("画素データの長さ"));
    }
    let mut img = RgbaImage::new(w, h)?;
    let tr16 = |i: usize| {
        trns.as_ref()
            .map(|t| u16::from_be_bytes([t[2 * i], t[2 * i + 1]]))
    };
    let mut off = 0;
    for &(xs, ys, dx, dy, pw, ph) in &passes {
        let rb = (pw * bits).div_ceil(8);
        let mut prev = vec![0u8; rb];
        let mut cur = vec![0u8; rb];
        for py in 0..ph {
            let ft = raw[off];
            cur.copy_from_slice(&raw[off + 1..off + 1 + rb]);
            off += 1 + rb;
            unfilter(ft, &mut cur, &prev, fbpp)?;
            for px in 0..pw {
                let sample = |c: usize| -> u16 {
                    let idx = px * ch + c;
                    match depth {
                        8 => cur[idx] as u16,
                        16 => u16::from_be_bytes([cur[2 * idx], cur[2 * idx + 1]]),
                        d => {
                            let bit = idx * d as usize;
                            let byte = cur[bit / 8];
                            let shift = 8 - d as usize - (bit % 8);
                            ((byte >> shift) & ((1u8 << d) - 1)) as u16
                        }
                    }
                };
                let to8 = |v: u16| -> u8 {
                    match depth {
                        16 => (v >> 8) as u8,
                        8 => v as u8,
                        d => (v as u32 * 255 / ((1u32 << d) - 1)) as u8,
                    }
                };
                let rgba = match ct {
                    0 => {
                        let g = sample(0);
                        let a = if tr16(0) == Some(g) { 0 } else { 255 };
                        let g8 = to8(g);
                        [g8, g8, g8, a]
                    }
                    2 => {
                        let (r, g, b) = (sample(0), sample(1), sample(2));
                        let a = if trns.is_some()
                            && tr16(0) == Some(r)
                            && tr16(1) == Some(g)
                            && tr16(2) == Some(b)
                        {
                            0
                        } else {
                            255
                        };
                        [to8(r), to8(g), to8(b), a]
                    }
                    3 => {
                        let i = sample(0) as usize;
                        *palette
                            .get(i)
                            .ok_or(ImageError::Format("パレットの範囲外"))?
                    }
                    4 => {
                        let g = to8(sample(0));
                        [g, g, g, to8(sample(1))]
                    }
                    _ => [
                        to8(sample(0)),
                        to8(sample(1)),
                        to8(sample(2)),
                        to8(sample(3)),
                    ],
                };
                let (x, y) = (xs + px * dx, ys + py * dy);
                img.pixel_mut(x, y).copy_from_slice(&rgba);
            }
            std::mem::swap(&mut prev, &mut cur);
        }
    }
    Ok(PngDecoded {
        image: img,
        color_type: ct,
        bit_depth: depth,
        interlaced,
    })
}

fn channels(ct: u8) -> usize {
    match ct {
        0 | 3 => 1,
        2 => 3,
        4 => 2,
        _ => 4,
    }
}

fn unfilter(ft: u8, cur: &mut [u8], prev: &[u8], bpp: usize) -> Result<()> {
    match ft {
        0 => {}
        1 => {
            for i in bpp..cur.len() {
                cur[i] = cur[i].wrapping_add(cur[i - bpp]);
            }
        }
        2 => {
            for (c, &p) in cur.iter_mut().zip(prev) {
                *c = c.wrapping_add(p);
            }
        }
        3 => {
            for i in 0..cur.len() {
                let a = if i >= bpp { cur[i - bpp] } else { 0 };
                cur[i] = cur[i].wrapping_add(((a as u16 + prev[i] as u16) / 2) as u8);
            }
        }
        4 => {
            for i in 0..cur.len() {
                let (a, c) = if i >= bpp {
                    (cur[i - bpp], prev[i - bpp])
                } else {
                    (0, 0)
                };
                cur[i] = cur[i].wrapping_add(paeth(a, prev[i], c));
            }
        }
        _ => return Err(ImageError::Format("PNG のフィルター種別")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_rgba(w: usize, h: usize) -> RgbaImage {
        let mut img = RgbaImage::new(w, h).unwrap();
        for y in 0..h {
            for x in 0..w {
                let p = img.pixel_mut(x, y);
                p.copy_from_slice(&[
                    (x * 7) as u8,
                    (y * 5) as u8,
                    ((x ^ y) * 3) as u8,
                    (x + y) as u8,
                ]);
            }
        }
        img
    }

    #[test]
    fn roundtrip_all_colors() {
        let img = test_rgba(67, 41);
        for level in [0, 1, 6, 9] {
            let d = decode_png(&encode_png_rgba(&img, level).unwrap(), &Limits::default()).unwrap();
            assert_eq!(d.image, img);
            let rgb = img.to_rgb();
            let d = decode_png(&encode_png_rgb(&rgb, level).unwrap(), &Limits::default()).unwrap();
            assert_eq!(d.image.to_rgb(), rgb);
            let g = img.alpha_plane();
            let d = decode_png(&encode_png_gray(&g, level).unwrap(), &Limits::default()).unwrap();
            assert_eq!(
                d.image
                    .to_rgb()
                    .data()
                    .iter()
                    .step_by(3)
                    .copied()
                    .collect::<Vec<_>>(),
                g.to_packed()
            );
        }
    }

    #[test]
    fn limits_and_corruption() {
        let img = test_rgba(40, 30);
        let png = encode_png_rgba(&img, 6).unwrap();
        let small = Limits {
            max_width: 39,
            ..Limits::default()
        };
        assert!(matches!(
            decode_png(&png, &small),
            Err(ImageError::TooLarge { .. })
        ));
        for cut in 0..png.len() {
            assert!(decode_png(&png[..cut], &Limits::default()).is_err());
        }
        let mut s = 5u32;
        for _ in 0..3000 {
            let mut m = png.clone();
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            let i = (s >> 8) as usize % m.len();
            m[i] ^= 1 << (s % 8);
            let _ = decode_png(&m, &Limits::default());
        }
    }

    /// 手で組んだ 2 bit パレット・Adam7 の画像。
    #[test]
    fn palette_interlaced_and_low_depth() {
        let (w, h) = (5usize, 3usize);
        // パレット 4 色、画素値 = (x + y) % 4
        let pal = [10u8, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
        let mut raw = Vec::new();
        for &(xs, ys, dx, dy) in &ADAM7 {
            let pw = if w > xs { (w - xs).div_ceil(dx) } else { 0 };
            let ph = if h > ys { (h - ys).div_ceil(dy) } else { 0 };
            if pw == 0 || ph == 0 {
                continue;
            }
            for py in 0..ph {
                raw.push(0);
                let mut row = vec![0u8; (pw * 2).div_ceil(8)];
                for px in 0..pw {
                    let v = (((xs + px * dx) + (ys + py * dy)) % 4) as u8;
                    row[px / 4] |= v << (6 - 2 * (px % 4));
                }
                raw.extend_from_slice(&row);
            }
        }
        let mut f = SIGNATURE.to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&(w as u32).to_be_bytes());
        ihdr.extend_from_slice(&(h as u32).to_be_bytes());
        ihdr.extend_from_slice(&[2, 3, 0, 0, 1]);
        put_chunk(&mut f, b"IHDR", &ihdr);
        put_chunk(&mut f, b"PLTE", &pal);
        put_chunk(&mut f, b"tRNS", &[255, 128]);
        put_chunk(&mut f, b"IDAT", &zlib_compress(&raw, 6));
        put_chunk(&mut f, b"IEND", &[]);
        let d = decode_png(&f, &Limits::default()).unwrap();
        assert!(d.interlaced);
        for y in 0..h {
            for x in 0..w {
                let v = (x + y) % 4;
                let a = if v == 1 { 128 } else { 255 };
                assert_eq!(
                    d.image.pixel(x, y),
                    &[pal[v * 3], pal[v * 3 + 1], pal[v * 3 + 2], a]
                );
            }
        }
    }
}
