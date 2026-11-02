//! ベースライン JPEG（ITU-T T.81）の符号化と復号。
//!
//! - 符号化: 4:4:4・4:2:0・グレー、品質 1〜100（IJG と同じ表の倍率）、Annex K の標準の量子化表と
//!   Huffman 表、浮動小数点の 8x8 DCT。JFIF（BT.601 全範囲の YCbCr）で書く。
//! - 復号: SOF0/SOF1（8 bit の Huffman 逐次）、1 または 3 成分、任意の標本化係数（1〜4）、
//!   インターリーブ・非インターリーブの scan、リスタート。プログレッシブ・算術符号・12 bit は未対応として拒否する。
//!   大きさは [`Limits`] で検査し、壊れた入力は `Err` を返す。

use crate::color::{YuvSpec, convert_i420, rgb_to_i420};
use crate::error::{ImageError, Result};
use crate::frame::{I420Frame, Limits, Plane, RectF, RgbImage, chroma_size};
use crate::scale::{ScaleFilter, scale_plane_rect};

/// ジグザグ順の k 番目の自然順の位置。
pub const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Annex K.1 の輝度の量子化表（自然順）。
const STD_LUMA_Q: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
/// Annex K.1 の色差の量子化表（自然順）。
const STD_CHROMA_Q: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMA_VALS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMA_VALS: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

/// DCT の基底（C[u][x] = c(u)/2 cos((2x+1)uπ/16)）。
fn dct_matrix() -> [[f32; 8]; 8] {
    let mut m = [[0f32; 8]; 8];
    for (u, row) in m.iter_mut().enumerate() {
        let c = if u == 0 {
            std::f64::consts::FRAC_1_SQRT_2
        } else {
            1.0
        };
        for (x, v) in row.iter_mut().enumerate() {
            *v = (c / 2.0 * ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos())
                as f32;
        }
    }
    m
}

/// 順 DCT（入力は 128 を引いた値、自然順）。
fn fdct(m: &[[f32; 8]; 8], blk: &[f32; 64], out: &mut [f32; 64]) {
    let mut tmp = [0f32; 64];
    // 行: tmp[y][u] = Σx m[u][x] f[y][x]
    for y in 0..8 {
        for u in 0..8 {
            let mut s = 0.0;
            for x in 0..8 {
                s += m[u][x] * blk[y * 8 + x];
            }
            tmp[y * 8 + u] = s;
        }
    }
    for v in 0..8 {
        for u in 0..8 {
            let mut s = 0.0;
            for y in 0..8 {
                s += m[v][y] * tmp[y * 8 + u];
            }
            out[v * 8 + u] = s;
        }
    }
}

/// 逆 DCT（自然順の係数 → 画素値 + 128、0〜255 に丸める）。
fn idct(m: &[[f32; 8]; 8], coef: &[f32; 64], out: &mut [u8; 64]) {
    let mut tmp = [0f32; 64];
    // tmp[v][x] = Σu m[u][x] F[v][u]
    for v in 0..8 {
        let row = &coef[v * 8..v * 8 + 8];
        if row.iter().all(|&c| c == 0.0) {
            continue;
        }
        for x in 0..8 {
            let mut s = 0.0;
            for u in 0..8 {
                s += m[u][x] * row[u];
            }
            tmp[v * 8 + x] = s;
        }
    }
    for y in 0..8 {
        for x in 0..8 {
            let mut s = 0.0;
            for v in 0..8 {
                s += m[v][y] * tmp[v * 8 + x];
            }
            out[y * 8 + x] = (s + 128.5).clamp(0.0, 255.0) as u8;
        }
    }
}

/// 品質（1〜100）から量子化表（ジグザグ順）を作る。
fn scaled_table(base: &[u16; 64], quality: u8) -> [u16; 64] {
    let q = quality.clamp(1, 100) as u32;
    let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
    let mut out = [0u16; 64];
    for k in 0..64 {
        let v = (base[ZIGZAG[k]] as u32 * scale + 50) / 100;
        out[k] = v.clamp(1, 255) as u16;
    }
    out
}

/// 色差の間引き方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JpegSubsampling {
    /// 4:4:4。
    S444,
    /// 4:2:0。
    #[default]
    S420,
}

struct HuffEnc {
    code: [u16; 256],
    size: [u8; 256],
}

impl HuffEnc {
    fn new(bits: &[u8; 16], vals: &[u8]) -> Self {
        let mut code = [0u16; 256];
        let mut size = [0u8; 256];
        let mut c = 0u16;
        let mut k = 0;
        for (l, &n) in bits.iter().enumerate() {
            for _ in 0..n {
                code[vals[k] as usize] = c;
                size[vals[k] as usize] = (l + 1) as u8;
                c += 1;
                k += 1;
            }
            c <<= 1;
        }
        Self { code, size }
    }
}

struct JBitWriter {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl JBitWriter {
    #[inline]
    fn put(&mut self, bits: u32, len: u32) {
        if len == 0 {
            return;
        }
        self.acc = (self.acc << len) | (bits & ((1 << len) - 1));
        self.n += len;
        while self.n >= 8 {
            let b = (self.acc >> (self.n - 8)) as u8;
            self.out.push(b);
            if b == 0xFF {
                self.out.push(0);
            }
            self.n -= 8;
        }
        self.acc &= (1 << self.n) - 1;
    }
    fn flush(&mut self) {
        if self.n > 0 {
            let pad = 8 - self.n;
            self.put((1 << pad) - 1, pad);
        }
    }
}

fn bit_len(v: i32) -> u32 {
    32 - v.unsigned_abs().leading_zeros()
}

struct EncComp<'a> {
    plane: &'a Plane,
    h: usize,
    v: usize,
    qt: usize,
    tbl: usize,
}

fn put_segment(out: &mut Vec<u8>, marker: u8, body: &[u8]) {
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(body);
}

/// 成分の平面（BT.601 全範囲の YCbCr、またはグレー 1 成分）を JPEG にする。
/// 4:2:0 なら色差は `chroma_size(width, height)` の大きさ。
fn encode_components(
    width: usize,
    height: usize,
    comps: &[EncComp],
    quality: u8,
    restart: u16,
) -> Result<Vec<u8>> {
    Limits {
        max_width: 65535,
        max_height: 65535,
        ..Limits::default()
    }
    .check(width, height)?;
    let qts = [
        scaled_table(&STD_LUMA_Q, quality),
        scaled_table(&STD_CHROMA_Q, quality),
    ];
    let mut out = vec![0xFF, 0xD8];
    put_segment(
        &mut out,
        0xE0,
        &[b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0],
    );
    let mut dqt = Vec::new();
    let nq = if comps.len() > 1 { 2 } else { 1 };
    for (i, qt) in qts.iter().enumerate().take(nq) {
        dqt.push(i as u8);
        dqt.extend(qt.iter().map(|&v| v as u8));
    }
    put_segment(&mut out, 0xDB, &dqt);
    let mut sof = vec![8];
    sof.extend_from_slice(&(height as u16).to_be_bytes());
    sof.extend_from_slice(&(width as u16).to_be_bytes());
    sof.push(comps.len() as u8);
    for (i, c) in comps.iter().enumerate() {
        sof.extend_from_slice(&[(i + 1) as u8, ((c.h as u8) << 4) | c.v as u8, c.qt as u8]);
    }
    put_segment(&mut out, 0xC0, &sof);
    let mut dht = Vec::new();
    let tables: [(u8, &[u8; 16], &[u8]); 4] = [
        (0x00, &DC_LUMA_BITS, &DC_VALS),
        (0x10, &AC_LUMA_BITS, &AC_LUMA_VALS),
        (0x01, &DC_CHROMA_BITS, &DC_VALS),
        (0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALS),
    ];
    for (tc, bits, vals) in tables.iter().take(if comps.len() > 1 { 4 } else { 2 }) {
        dht.push(*tc);
        dht.extend_from_slice(*bits);
        dht.extend_from_slice(vals);
    }
    put_segment(&mut out, 0xC4, &dht);
    if restart > 0 {
        put_segment(&mut out, 0xDD, &restart.to_be_bytes());
    }
    let mut sos = vec![comps.len() as u8];
    for (i, c) in comps.iter().enumerate() {
        sos.extend_from_slice(&[(i + 1) as u8, ((c.tbl as u8) << 4) | c.tbl as u8]);
    }
    sos.extend_from_slice(&[0, 63, 0]);
    put_segment(&mut out, 0xDA, &sos);

    let dc = [
        HuffEnc::new(&DC_LUMA_BITS, &DC_VALS),
        HuffEnc::new(&DC_CHROMA_BITS, &DC_VALS),
    ];
    let ac = [
        HuffEnc::new(&AC_LUMA_BITS, &AC_LUMA_VALS),
        HuffEnc::new(&AC_CHROMA_BITS, &AC_CHROMA_VALS),
    ];
    let hmax = comps.iter().map(|c| c.h).max().unwrap_or(1);
    let vmax = comps.iter().map(|c| c.v).max().unwrap_or(1);
    let mcux = width.div_ceil(8 * hmax);
    let mcuy = height.div_ceil(8 * vmax);
    let m = dct_matrix();
    // 1/q を前もって計算（ジグザグ順）。
    let inv: Vec<[f32; 64]> = qts
        .iter()
        .map(|q| std::array::from_fn(|k| 1.0 / q[k] as f32))
        .collect();
    let mut bw = JBitWriter { out, acc: 0, n: 0 };
    let mut pred = vec![0i32; comps.len()];
    let mut blk = [0f32; 64];
    let mut coef = [0f32; 64];
    let mut rst = 0u8;
    for my in 0..mcuy {
        for mx in 0..mcux {
            let n = my * mcux + mx;
            if restart > 0 && n > 0 && n.is_multiple_of(restart as usize) {
                bw.flush();
                bw.out.extend_from_slice(&[0xFF, 0xD0 + rst]);
                rst = (rst + 1) & 7;
                pred.iter_mut().for_each(|p| *p = 0);
            }
            for (ci, c) in comps.iter().enumerate() {
                for by in 0..c.v {
                    for bx in 0..c.h {
                        let x0 = ((mx * c.h + bx) * 8) as isize;
                        let y0 = ((my * c.v + by) * 8) as isize;
                        let p = c.plane;
                        let inside =
                            (x0 as usize + 8) <= p.width() && (y0 as usize + 8) <= p.height();
                        for y in 0..8 {
                            if inside {
                                let r = &p.row(y0 as usize + y)[x0 as usize..x0 as usize + 8];
                                for x in 0..8 {
                                    blk[y * 8 + x] = r[x] as f32 - 128.0;
                                }
                            } else {
                                for x in 0..8 {
                                    blk[y * 8 + x] = p.get_clamped(x0 + x as isize, y0 + y as isize)
                                        as f32
                                        - 128.0;
                                }
                            }
                        }
                        fdct(&m, &blk, &mut coef);
                        let iq = &inv[c.qt];
                        let mut zz = [0i32; 64];
                        for k in 0..64 {
                            zz[k] = (coef[ZIGZAG[k]] * iq[k]).round() as i32;
                        }
                        // DC
                        let diff = zz[0] - pred[ci];
                        pred[ci] = zz[0];
                        let t = &dc[c.tbl];
                        let nb = bit_len(diff);
                        bw.put(t.code[nb as usize] as u32, t.size[nb as usize] as u32);
                        let vbits = if diff < 0 {
                            (diff - 1) as u32
                        } else {
                            diff as u32
                        };
                        bw.put(vbits, nb);
                        // AC
                        let t = &ac[c.tbl];
                        let mut run = 0u32;
                        for &v in zz.iter().skip(1) {
                            if v == 0 {
                                run += 1;
                                continue;
                            }
                            while run > 15 {
                                bw.put(t.code[0xF0] as u32, t.size[0xF0] as u32);
                                run -= 16;
                            }
                            let nb = bit_len(v).min(10);
                            let sym = ((run << 4) | nb) as usize;
                            bw.put(t.code[sym] as u32, t.size[sym] as u32);
                            let vbits = if v < 0 { (v - 1) as u32 } else { v as u32 };
                            bw.put(vbits, nb);
                            run = 0;
                        }
                        if run > 0 {
                            bw.put(t.code[0] as u32, t.size[0] as u32);
                        }
                    }
                }
            }
        }
    }
    bw.flush();
    let mut out = bw.out;
    out.extend_from_slice(&[0xFF, 0xD9]);
    Ok(out)
}

/// グレーの平面を JPEG（1 成分）にする。値は全範囲として扱う。
pub fn encode_jpeg_gray(p: &Plane, quality: u8) -> Result<Vec<u8>> {
    encode_components(
        p.width(),
        p.height(),
        &[EncComp {
            plane: p,
            h: 1,
            v: 1,
            qt: 0,
            tbl: 0,
        }],
        quality,
        0,
    )
}

/// RGB を JPEG にする。
pub fn encode_jpeg_rgb(img: &RgbImage, quality: u8, sub: JpegSubsampling) -> Result<Vec<u8>> {
    match sub {
        JpegSubsampling::S420 => {
            let f = rgb_to_i420(img, YuvSpec::BT601_FULL);
            encode_jpeg_i420(&f, YuvSpec::BT601_FULL, quality)
        }
        JpegSubsampling::S444 => {
            let (w, h) = (img.width(), img.height());
            let spec = YuvSpec::BT601_FULL;
            let mut y = Plane::alloc(w, h, 0);
            let mut cb = Plane::alloc(w, h, 128);
            let mut cr = Plane::alloc(w, h, 128);
            for yy in 0..h {
                for (x, p) in img.row(yy).as_chunks::<3>().0.iter().enumerate() {
                    let c = spec.rgb_to_yuv(p[0], p[1], p[2]);
                    y.set(x, yy, c.y);
                    cb.set(x, yy, c.u);
                    cr.set(x, yy, c.v);
                }
            }
            encode_yuv444(&y, &cb, &cr, quality)
        }
    }
}

/// 3 平面（同じ大きさ、BT.601 全範囲）を 4:4:4 の JPEG にする。
pub fn encode_yuv444(y: &Plane, cb: &Plane, cr: &Plane, quality: u8) -> Result<Vec<u8>> {
    if cb.width() != y.width()
        || cr.width() != y.width()
        || cb.height() != y.height()
        || cr.height() != y.height()
    {
        return Err(ImageError::Mismatch("4:4:4 の平面の大きさ"));
    }
    let comps = [
        EncComp {
            plane: y,
            h: 1,
            v: 1,
            qt: 0,
            tbl: 0,
        },
        EncComp {
            plane: cb,
            h: 1,
            v: 1,
            qt: 1,
            tbl: 1,
        },
        EncComp {
            plane: cr,
            h: 1,
            v: 1,
            qt: 1,
            tbl: 1,
        },
    ];
    encode_components(y.width(), y.height(), &comps, quality, 0)
}

/// I420 を 4:2:0 の JPEG にする。`spec` は入力の規格（JPEG の BT.601 全範囲へ変換する）。
pub fn encode_jpeg_i420(f: &I420Frame, spec: YuvSpec, quality: u8) -> Result<Vec<u8>> {
    encode_jpeg_i420_with_restart(f, spec, quality, 0)
}

/// I420 を 4:2:0 の JPEG にする（`restart_mcus` 個の MCU ごとにリスタートマーカー、0 なら無し）。
pub fn encode_jpeg_i420_with_restart(
    f: &I420Frame,
    spec: YuvSpec,
    quality: u8,
    restart_mcus: u16,
) -> Result<Vec<u8>> {
    let conv;
    let f = if spec == YuvSpec::BT601_FULL {
        f
    } else {
        conv = convert_i420(f, spec, YuvSpec::BT601_FULL);
        &conv
    };
    let comps = [
        EncComp {
            plane: f.y(),
            h: 2,
            v: 2,
            qt: 0,
            tbl: 0,
        },
        EncComp {
            plane: f.u(),
            h: 1,
            v: 1,
            qt: 1,
            tbl: 1,
        },
        EncComp {
            plane: f.v(),
            h: 1,
            v: 1,
            qt: 1,
            tbl: 1,
        },
    ];
    encode_components(f.width(), f.height(), &comps, quality, restart_mcus)
}

// ---------------------------------------------------------------- 復号

/// 復号した成分の色の意味。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JpegColor {
    Gray,
    YCbCr,
    Rgb,
}

/// 復号した 1 成分。
#[derive(Debug, Clone)]
pub struct JpegComponent {
    pub id: u8,
    pub h: usize,
    pub v: usize,
    /// 成分の大きさ（ceil(X * h / hmax) x ceil(Y * v / vmax)）の平面。
    pub plane: Plane,
}

/// 復号結果。
#[derive(Debug, Clone)]
pub struct JpegDecoded {
    pub width: usize,
    pub height: usize,
    pub color: JpegColor,
    pub components: Vec<JpegComponent>,
}

impl JpegDecoded {
    fn hv_max(&self) -> (usize, usize) {
        (
            self.components.iter().map(|c| c.h).max().unwrap_or(1),
            self.components.iter().map(|c| c.v).max().unwrap_or(1),
        )
    }

    /// 成分を全画素の大きさに戻す（中央配置の双線形）。
    fn full_res(&self, i: usize) -> Result<Plane> {
        let c = &self.components[i];
        let (hm, vm) = self.hv_max();
        if c.h == hm && c.v == vm {
            return Ok(c.plane.clone());
        }
        let rect = RectF::new(
            0.0,
            0.0,
            self.width as f64 * c.h as f64 / hm as f64,
            self.height as f64 * c.v as f64 / vm as f64,
        );
        scale_plane_rect(
            &c.plane,
            rect,
            self.width,
            self.height,
            ScaleFilter::Bilinear,
        )
    }

    /// 輝度（グレー）。RGB の JPEG なら BT.601 の輝度を計算する。
    pub fn to_gray(&self) -> Result<Plane> {
        match self.color {
            JpegColor::Gray | JpegColor::YCbCr => self.full_res(0),
            JpegColor::Rgb => Ok(crate::color::rgb_to_gray(
                &self.to_rgb()?,
                YuvSpec::BT601_FULL,
            )),
        }
    }

    /// RGB にする。
    pub fn to_rgb(&self) -> Result<RgbImage> {
        let mut out = RgbImage::new(self.width, self.height)?;
        match self.color {
            JpegColor::Gray => {
                let y = self.full_res(0)?;
                for r in 0..self.height {
                    for (x, p) in out.row_mut(r).as_chunks_mut::<3>().0.iter_mut().enumerate() {
                        let v = y.get(x, r);
                        p.copy_from_slice(&[v, v, v]);
                    }
                }
            }
            JpegColor::YCbCr | JpegColor::Rgb => {
                let (hm, vm) = self.hv_max();
                let fancy = self.color == JpegColor::YCbCr
                    && hm == 2
                    && vm == 2
                    && self.components[0].h == 2
                    && self.components[0].v == 2
                    && self.components[1..].iter().all(|c| c.h == 1 && c.v == 1);
                if fancy {
                    let f = self.to_i420()?;
                    return Ok(crate::color::i420_to_rgb(&f, YuvSpec::BT601_FULL));
                }
                let a = self.full_res(0)?;
                let b = self.full_res(1)?;
                let c = self.full_res(2)?;
                let spec = YuvSpec::BT601_FULL;
                for r in 0..self.height {
                    for (x, p) in out.row_mut(r).as_chunks_mut::<3>().0.iter_mut().enumerate() {
                        let (v0, v1, v2) = (a.get(x, r), b.get(x, r), c.get(x, r));
                        if self.color == JpegColor::Rgb {
                            p.copy_from_slice(&[v0, v1, v2]);
                        } else {
                            p.copy_from_slice(
                                &spec.yuv_to_rgb(crate::frame::YuvColor::new(v0, v1, v2)),
                            );
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// I420（BT.601 全範囲）にする。4:2:0 の JPEG なら平面をそのまま使う。
    pub fn to_i420(&self) -> Result<I420Frame> {
        let (cw, ch) = chroma_size(self.width, self.height);
        match self.color {
            JpegColor::Gray => {
                let y = self.full_res(0)?;
                I420Frame::from_planes(y, Plane::filled(cw, ch, 128)?, Plane::filled(cw, ch, 128)?)
            }
            JpegColor::Rgb => Ok(rgb_to_i420(&self.to_rgb()?, YuvSpec::BT601_FULL)),
            JpegColor::YCbCr => {
                let direct = self.components[1].plane.width() == cw
                    && self.components[1].plane.height() == ch
                    && self.components[2].plane.width() == cw
                    && self.components[2].plane.height() == ch
                    && self.components[0].plane.width() == self.width;
                if direct {
                    return I420Frame::from_planes(
                        self.components[0].plane.clone(),
                        self.components[1].plane.clone(),
                        self.components[2].plane.clone(),
                    );
                }
                let y = self.full_res(0)?;
                let u = crate::color::downsample_chroma_420(&self.full_res(1)?);
                let v = crate::color::downsample_chroma_420(&self.full_res(2)?);
                I420Frame::from_planes(y, u, v)
            }
        }
    }
}

struct HuffDec {
    /// (長さ << 8) | 値。長さ 0 は早引き不可。
    fast: [u16; 1 << 9],
    maxcode: [i32; 18],
    valptr: [i32; 17],
    mincode: [i32; 17],
    vals: Vec<u8>,
}

impl HuffDec {
    fn new(bits: &[u8; 16], vals: &[u8]) -> Result<Self> {
        let total: usize = bits.iter().map(|&b| b as usize).sum();
        if total > 256 || total != vals.len() {
            return Err(ImageError::Format("DHT の個数"));
        }
        let mut fast = [0u16; 1 << 9];
        let mut maxcode = [-1i32; 18];
        let mut valptr = [0i32; 17];
        let mut mincode = [0i32; 17];
        let mut code = 0i32;
        let mut k = 0usize;
        for l in 1..=16 {
            let n = bits[l - 1] as i32;
            valptr[l] = k as i32;
            mincode[l] = code;
            for _ in 0..n {
                if code >= (1 << l) {
                    return Err(ImageError::Format("Huffman 表が過剰"));
                }
                if l <= 9 {
                    let shift = 9 - l;
                    let base = (code as usize) << shift;
                    for i in 0..(1 << shift) {
                        fast[base + i] = ((l as u16) << 8) | vals[k] as u16;
                    }
                }
                code += 1;
                k += 1;
            }
            maxcode[l] = if n > 0 { code - 1 } else { -1 };
            if code > (1 << l) {
                return Err(ImageError::Format("Huffman 表が過剰"));
            }
            code <<= 1;
        }
        maxcode[17] = i32::MAX;
        Ok(Self {
            fast,
            maxcode,
            valptr,
            mincode,
            vals: vals.to_vec(),
        })
    }
}

struct EntropyReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    cnt: u32,
    /// 出会ったマーカー（そこで読み止める）。
    marker: Option<u8>,
    /// マーカーの後に詰めた 0 のビット数。
    padded: u32,
}

impl<'a> EntropyReader<'a> {
    fn fill(&mut self) {
        while self.cnt <= 56 {
            let byte = if self.marker.is_some() {
                self.padded += 8;
                0
            } else if self.pos >= self.data.len() {
                self.marker = Some(0xD9);
                self.padded += 8;
                0
            } else {
                let b = self.data[self.pos];
                if b == 0xFF {
                    let mut p = self.pos + 1;
                    while p < self.data.len() && self.data[p] == 0xFF {
                        p += 1;
                    }
                    if p < self.data.len() && self.data[p] == 0 {
                        self.pos = p + 1;
                        0xFF
                    } else {
                        // マーカー: pos はマーカーの 0xFF を指したまま止める。
                        self.marker = Some(if p < self.data.len() {
                            self.data[p]
                        } else {
                            0xD9
                        });
                        self.padded += 8;
                        0
                    }
                } else {
                    self.pos += 1;
                    b
                }
            };
            self.buf |= (byte as u64) << (56 - self.cnt);
            self.cnt += 8;
        }
    }
    /// マーカー後の詰め物を実際に読んだか（壊れた・途切れたデータ）。
    fn overran(&self) -> bool {
        self.padded > self.cnt
    }
    #[inline]
    fn bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        if self.cnt < n {
            self.fill();
        }
        let v = (self.buf >> (64 - n)) as u32;
        self.buf <<= n;
        self.cnt -= n;
        v
    }
    fn decode(&mut self, h: &HuffDec) -> Result<u8> {
        if self.cnt < 16 {
            self.fill();
        }
        let e = h.fast[(self.buf >> (64 - 9)) as usize];
        if e != 0 {
            let l = (e >> 8) as u32;
            self.buf <<= l;
            self.cnt -= l;
            return Ok(e as u8);
        }
        let mut code = 0i32;
        for l in 1..=16 {
            code = (code << 1) | self.bits(1) as i32;
            if code <= h.maxcode[l] {
                let idx = h.valptr[l] + code - h.mincode[l];
                return h
                    .vals
                    .get(idx as usize)
                    .copied()
                    .ok_or(ImageError::Format("Huffman 符号"));
            }
        }
        Err(ImageError::Format("Huffman 符号"))
    }
    fn receive_extend(&mut self, s: u32) -> i32 {
        if s == 0 {
            return 0;
        }
        let v = self.bits(s) as i32;
        if v < (1 << (s - 1)) {
            v - (1 << s) + 1
        } else {
            v
        }
    }
    /// リスタートのためビットを捨て、RSTn を読む。
    fn restart(&mut self, expect: u8) -> Result<()> {
        self.buf = 0;
        self.cnt = 0;
        self.padded = 0;
        // pos はマーカーの 0xFF を指しているはず（先読みが届いていなくても、残りのデータはない）。
        let m = match self.marker.take() {
            Some(m) => m,
            None => {
                if self.data.get(self.pos) != Some(&0xFF) {
                    return Err(ImageError::Format("リスタートマーカーがない"));
                }
                let mut p = self.pos;
                while p < self.data.len() && self.data[p] == 0xFF {
                    p += 1;
                }
                *self.data.get(p).ok_or(ImageError::Truncated)?
            }
        };
        if m != 0xD0 + expect {
            return Err(ImageError::Format("リスタートマーカーが違う"));
        }
        let mut p = self.pos;
        while p < self.data.len() && self.data[p] == 0xFF {
            p += 1;
        }
        self.pos = p + 1;
        Ok(())
    }
}

struct FrameComp {
    id: u8,
    h: usize,
    v: usize,
    tq: usize,
    plane: Plane,
    pred: i32,
    done: bool,
}

/// JPEG を復号する（ベースライン・拡張逐次の 8 bit Huffman）。
pub fn decode_jpeg(data: &[u8], limits: &Limits) -> Result<JpegDecoded> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return Err(ImageError::Format("JPEG の SOI"));
    }
    let mut pos = 2;
    let mut qt: [Option<[u16; 64]>; 4] = [None; 4];
    let mut dc: [Option<HuffDec>; 4] = [None, None, None, None];
    let mut ac: [Option<HuffDec>; 4] = [None, None, None, None];
    let mut frame: Option<(usize, usize, Vec<FrameComp>)> = None;
    let mut restart = 0usize;
    let mut adobe: Option<u8> = None;
    let mut jfif = false;
    let mut scans = 0;
    let m = dct_matrix();
    loop {
        if pos + 2 > data.len() {
            return Err(ImageError::Truncated);
        }
        if data[pos] != 0xFF {
            return Err(ImageError::Format("マーカーがない"));
        }
        while pos < data.len() && data[pos] == 0xFF {
            pos += 1;
        }
        if pos >= data.len() {
            return Err(ImageError::Truncated);
        }
        let marker = data[pos];
        pos += 1;
        match marker {
            0xD9 => break,
            0xD8 => return Err(ImageError::Format("SOI の重複")),
            0xD0..=0xD7 | 0x01 => continue,
            _ => {}
        }
        if pos + 2 > data.len() {
            return Err(ImageError::Truncated);
        }
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        if len < 2 || pos + len > data.len() {
            return Err(ImageError::Truncated);
        }
        let seg = &data[pos + 2..pos + len];
        pos += len;
        match marker {
            0xDB => {
                let mut s = seg;
                while !s.is_empty() {
                    let (pq, tq) = ((s[0] >> 4) as usize, (s[0] & 15) as usize);
                    let n = if pq == 0 { 64 } else { 128 };
                    if pq > 1 || tq > 3 || s.len() < 1 + n {
                        return Err(ImageError::Format("DQT"));
                    }
                    let mut t = [0u16; 64];
                    for (k, v) in t.iter_mut().enumerate() {
                        *v = if pq == 0 {
                            s[1 + k] as u16
                        } else {
                            u16::from_be_bytes([s[1 + 2 * k], s[2 + 2 * k]])
                        };
                        if *v == 0 {
                            return Err(ImageError::Format("量子化値 0"));
                        }
                    }
                    qt[tq] = Some(t);
                    s = &s[1 + n..];
                }
            }
            0xC4 => {
                let mut s = seg;
                while !s.is_empty() {
                    if s.len() < 17 {
                        return Err(ImageError::Format("DHT"));
                    }
                    let (tc, th) = (s[0] >> 4, (s[0] & 15) as usize);
                    if tc > 1 || th > 3 {
                        return Err(ImageError::Format("DHT の番号"));
                    }
                    let mut bits = [0u8; 16];
                    bits.copy_from_slice(&s[1..17]);
                    let n: usize = bits.iter().map(|&b| b as usize).sum();
                    if s.len() < 17 + n {
                        return Err(ImageError::Format("DHT の長さ"));
                    }
                    let h = HuffDec::new(&bits, &s[17..17 + n])?;
                    if tc == 0 {
                        dc[th] = Some(h);
                    } else {
                        ac[th] = Some(h);
                    }
                    s = &s[17 + n..];
                }
            }
            0xDD => {
                if seg.len() != 2 {
                    return Err(ImageError::Format("DRI"));
                }
                restart = u16::from_be_bytes([seg[0], seg[1]]) as usize;
            }
            0xC0 | 0xC1 => {
                if frame.is_some() {
                    return Err(ImageError::Format("SOF の重複"));
                }
                if seg.len() < 6 {
                    return Err(ImageError::Format("SOF"));
                }
                if seg[0] != 8 {
                    return Err(ImageError::Unsupported("8 bit 以外の JPEG"));
                }
                let h = u16::from_be_bytes([seg[1], seg[2]]) as usize;
                let w = u16::from_be_bytes([seg[3], seg[4]]) as usize;
                let nf = seg[5] as usize;
                if h == 0 {
                    return Err(ImageError::Unsupported("DNL（高さ 0）"));
                }
                limits.check(w, h)?;
                if !(nf == 1 || nf == 3) {
                    return Err(ImageError::Unsupported("成分の数（1 か 3 のみ）"));
                }
                if seg.len() != 6 + 3 * nf {
                    return Err(ImageError::Format("SOF の長さ"));
                }
                let mut comps = Vec::new();
                for i in 0..nf {
                    let c = &seg[6 + 3 * i..9 + 3 * i];
                    let (hh, vv) = ((c[1] >> 4) as usize, (c[1] & 15) as usize);
                    if !(1..=4).contains(&hh) || !(1..=4).contains(&vv) || c[2] > 3 {
                        return Err(ImageError::Format("SOF の成分"));
                    }
                    if comps.iter().any(|x: &FrameComp| x.id == c[0]) {
                        return Err(ImageError::Format("成分の番号の重複"));
                    }
                    comps.push(FrameComp {
                        id: c[0],
                        h: hh,
                        v: vv,
                        tq: c[2] as usize,
                        plane: Plane::alloc(1, 1, 0),
                        pred: 0,
                        done: false,
                    });
                }
                let hm = comps.iter().map(|c| c.h).max().unwrap_or(1);
                let vm = comps.iter().map(|c| c.v).max().unwrap_or(1);
                let mcux = w.div_ceil(8 * hm);
                let mcuy = h.div_ceil(8 * vm);
                for c in comps.iter_mut() {
                    let (pw, ph) = (mcux * c.h * 8, mcuy * c.v * 8);
                    limits.check(pw.min(limits.max_width), ph.min(limits.max_height))?;
                    c.plane = Plane::alloc(pw, ph, 0);
                }
                frame = Some((w, h, comps));
            }
            0xC2 | 0xC6 | 0xCA | 0xCE => {
                return Err(ImageError::Unsupported("プログレッシブ JPEG"));
            }
            0xC3 | 0xC5 | 0xC7 | 0xC9 | 0xCB | 0xCD | 0xCF => {
                return Err(ImageError::Unsupported("可逆・階層・算術符号の JPEG"));
            }
            0xE0 => {
                if seg.starts_with(b"JFIF\0") {
                    jfif = true;
                }
            }
            0xEE => {
                if seg.len() >= 12 && seg.starts_with(b"Adobe") {
                    adobe = Some(seg[11]);
                }
            }
            0xDA => {
                let Some((w, h, comps)) = frame.as_mut() else {
                    return Err(ImageError::Format("SOF より前の SOS"));
                };
                let ns = *seg.first().ok_or(ImageError::Format("SOS"))? as usize;
                if ns == 0 || ns > 4 || seg.len() != 4 + 2 * ns {
                    return Err(ImageError::Format("SOS の長さ"));
                }
                let mut sel: Vec<(usize, usize, usize)> = Vec::new();
                for i in 0..ns {
                    let cid = seg[1 + 2 * i];
                    let t = seg[2 + 2 * i];
                    let ci = comps
                        .iter()
                        .position(|c| c.id == cid)
                        .ok_or(ImageError::Format("SOS の成分"))?;
                    if sel.iter().any(|s| s.0 == ci) || comps[ci].done {
                        return Err(ImageError::Format("SOS の成分の重複"));
                    }
                    let (td, ta) = ((t >> 4) as usize, (t & 15) as usize);
                    if td > 3 || ta > 3 || dc[td].is_none() || ac[ta].is_none() {
                        return Err(ImageError::Format("SOS の Huffman 表"));
                    }
                    if qt[comps[ci].tq].is_none() {
                        return Err(ImageError::Format("量子化表がない"));
                    }
                    sel.push((ci, td, ta));
                }
                let tail = &seg[1 + 2 * ns..];
                if tail != [0, 63, 0] {
                    return Err(ImageError::Unsupported("ベースライン以外の scan"));
                }
                let hm = comps.iter().map(|c| c.h).max().unwrap_or(1);
                let vm = comps.iter().map(|c| c.v).max().unwrap_or(1);
                let blocks_per_mcu: usize = sel.iter().map(|s| comps[s.0].h * comps[s.0].v).sum();
                if ns > 1 && blocks_per_mcu > 10 {
                    return Err(ImageError::Format("MCU のブロック数が 10 を超える"));
                }
                // (mcu の数 x, y)
                let (mx, my) = if ns == 1 {
                    let c = &comps[sel[0].0];
                    (
                        (*w * c.h).div_ceil(hm).div_ceil(8),
                        (*h * c.v).div_ceil(vm).div_ceil(8),
                    )
                } else {
                    (w.div_ceil(8 * hm), h.div_ceil(8 * vm))
                };
                for s in &sel {
                    comps[s.0].pred = 0;
                }
                let mut rd = EntropyReader {
                    data,
                    pos,
                    buf: 0,
                    cnt: 0,
                    marker: None,
                    padded: 0,
                };
                let total = mx * my;
                let mut rst = 0u8;
                let mut coef = [0f32; 64];
                let mut px = [0u8; 64];
                for n in 0..total {
                    if restart > 0 && n > 0 && n % restart == 0 {
                        if rd.overran() {
                            return Err(ImageError::Truncated);
                        }
                        rd.restart(rst)?;
                        rst = (rst + 1) & 7;
                        for s in &sel {
                            comps[s.0].pred = 0;
                        }
                    }
                    let (mcx, mcy) = (n % mx, n / mx);
                    for &(ci, td, ta) in &sel {
                        let (ch, cv) = if ns == 1 {
                            (1, 1)
                        } else {
                            (comps[ci].h, comps[ci].v)
                        };
                        for by in 0..cv {
                            for bx in 0..ch {
                                let q = qt[comps[ci].tq]
                                    .as_ref()
                                    .ok_or(ImageError::Format("量子化表"))?;
                                let dct = dc[td].as_ref().ok_or(ImageError::Format("DC 表"))?;
                                let act = ac[ta].as_ref().ok_or(ImageError::Format("AC 表"))?;
                                coef.fill(0.0);
                                let t = rd.decode(dct)? as u32;
                                if t > 11 {
                                    return Err(ImageError::Format("DC の大きさ"));
                                }
                                let diff = rd.receive_extend(t);
                                let c = &mut comps[ci];
                                c.pred = c.pred.wrapping_add(diff);
                                if c.pred.abs() > 1 << 16 {
                                    return Err(ImageError::Format("DC の値"));
                                }
                                coef[0] = c.pred as f32 * q[0] as f32;
                                let mut k = 1;
                                while k < 64 {
                                    let rs = rd.decode(act)?;
                                    let (r, s) = ((rs >> 4) as usize, (rs & 15) as u32);
                                    if s == 0 {
                                        if r == 15 {
                                            k += 16;
                                            continue;
                                        }
                                        break;
                                    }
                                    k += r;
                                    if k > 63 || s > 10 {
                                        return Err(ImageError::Format("AC の位置"));
                                    }
                                    coef[ZIGZAG[k]] = rd.receive_extend(s) as f32 * q[k] as f32;
                                    k += 1;
                                }
                                idct(&m, &coef, &mut px);
                                let (x0, y0) = if ns == 1 {
                                    (mcx * 8, mcy * 8)
                                } else {
                                    ((mcx * ch + bx) * 8, (mcy * cv + by) * 8)
                                };
                                if x0 + 8 <= c.plane.width() && y0 + 8 <= c.plane.height() {
                                    for yy in 0..8 {
                                        c.plane.row_mut(y0 + yy)[x0..x0 + 8]
                                            .copy_from_slice(&px[yy * 8..yy * 8 + 8]);
                                    }
                                }
                            }
                        }
                    }
                }
                if rd.overran() {
                    return Err(ImageError::Truncated);
                }
                for s in &sel {
                    comps[s.0].done = true;
                }
                scans += 1;
                // 次のマーカーまで進める（rd.pos はマーカーの 0xFF か末尾）。
                pos = rd.pos;
                if rd.marker.is_none() {
                    // ビットバッファに読み込んだ分の後ろにマーカーがあるはず。探す。
                    while pos + 1 < data.len()
                        && !(data[pos] == 0xFF
                            && data[pos + 1] != 0
                            && !(0xD0..=0xD7).contains(&data[pos + 1]))
                    {
                        pos += 1;
                    }
                }
                // RST が余分に続く場合を飛ばす
                while pos + 1 < data.len()
                    && data[pos] == 0xFF
                    && (0xD0..=0xD7).contains(&data[pos + 1])
                {
                    pos += 2;
                }
            }
            _ => {}
        }
    }
    let (w, h, comps) = frame.ok_or(ImageError::Format("SOF がない"))?;
    if scans == 0 || comps.iter().any(|c| !c.done) {
        return Err(ImageError::Format("復号されていない成分がある"));
    }
    let hm = comps.iter().map(|c| c.h).max().unwrap_or(1);
    let vm = comps.iter().map(|c| c.v).max().unwrap_or(1);
    let color = if comps.len() == 1 {
        JpegColor::Gray
    } else if adobe == Some(0)
        || (adobe.is_none() && !jfif && comps.iter().map(|c| c.id).eq(*b"RGB"))
    {
        JpegColor::Rgb
    } else {
        JpegColor::YCbCr
    };
    let mut out = Vec::new();
    for c in comps {
        let cw = (w * c.h).div_ceil(hm);
        let chh = (h * c.v).div_ceil(vm);
        let plane = crate::geom::crop_plane(&c.plane, crate::frame::Rect::new(0, 0, cw, chh))?;
        out.push(JpegComponent {
            id: c.id,
            h: c.h,
            v: c.v,
            plane,
        });
    }
    Ok(JpegDecoded {
        width: w,
        height: h,
        color,
        components: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{psnr, psnr_rgb};

    pub(crate) fn photo(w: usize, h: usize) -> RgbImage {
        let mut img = RgbImage::new(w, h).unwrap();
        for y in 0..h {
            for x in 0..w {
                let fx = x as f32 / w as f32;
                let fy = y as f32 / h as f32;
                let r = 128.0 + 100.0 * (fx * 6.0).sin() * (fy * 3.0).cos();
                let g = 40.0 + 180.0 * fy;
                let b = 128.0 + 90.0 * ((fx + fy) * 9.0).cos();
                img.pixel_mut(x, y)
                    .copy_from_slice(&[r as u8, g as u8, b as u8]);
            }
        }
        img
    }

    #[test]
    fn tables_consistent() {
        assert_eq!(AC_LUMA_BITS.iter().map(|&b| b as usize).sum::<usize>(), 162);
        assert_eq!(
            AC_CHROMA_BITS.iter().map(|&b| b as usize).sum::<usize>(),
            162
        );
        assert_eq!(DC_LUMA_BITS.iter().map(|&b| b as usize).sum::<usize>(), 12);
        assert_eq!(
            DC_CHROMA_BITS.iter().map(|&b| b as usize).sum::<usize>(),
            12
        );
        let mut seen = [false; 64];
        for &z in &ZIGZAG {
            seen[z] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }

    #[test]
    fn dct_roundtrip() {
        let m = dct_matrix();
        let blk: [f32; 64] = std::array::from_fn(|i| ((i * 37) % 255) as f32 - 128.0);
        let mut c = [0f32; 64];
        fdct(&m, &blk, &mut c);
        let mut px = [0u8; 64];
        idct(&m, &c, &mut px);
        for i in 0..64 {
            assert!((px[i] as f32 - 128.0 - blk[i]).abs() <= 0.51, "{i}");
        }
    }

    #[test]
    fn roundtrip_quality() {
        let img = photo(101, 67);
        for (sub, q, min_psnr) in [
            (JpegSubsampling::S444, 95, 40.0),
            (JpegSubsampling::S420, 95, 34.0),
            (JpegSubsampling::S444, 50, 32.0),
        ] {
            let j = encode_jpeg_rgb(&img, q, sub).unwrap();
            let d = decode_jpeg(&j, &Limits::default()).unwrap();
            assert_eq!((d.width, d.height, d.color), (101, 67, JpegColor::YCbCr));
            let back = d.to_rgb().unwrap();
            let p = psnr_rgb(&img, &back).unwrap();
            assert!(p >= min_psnr, "{sub:?} q{q}: {p}");
        }
        let g = crate::color::rgb_to_gray(&img, YuvSpec::BT601_FULL);
        let j = encode_jpeg_gray(&g, 90).unwrap();
        let d = decode_jpeg(&j, &Limits::default()).unwrap();
        assert_eq!(d.color, JpegColor::Gray);
        assert!(psnr(&g, &d.to_gray().unwrap()).unwrap() > 40.0);
    }

    #[test]
    fn i420_direct() {
        let img = photo(64, 48);
        let f = rgb_to_i420(&img, YuvSpec::BT709_LIMITED);
        let j = encode_jpeg_i420(&f, YuvSpec::BT709_LIMITED, 95).unwrap();
        let d = decode_jpeg(&j, &Limits::default()).unwrap();
        let back = convert_i420(
            &d.to_i420().unwrap(),
            YuvSpec::BT601_FULL,
            YuvSpec::BT709_LIMITED,
        );
        assert!(crate::metrics::psnr_i420(&f, &back).unwrap() > 38.0);
    }

    #[test]
    fn restart_markers_roundtrip() {
        let img = photo(100, 70);
        let f = rgb_to_i420(&img, YuvSpec::BT601_FULL);
        let plain = decode_jpeg(
            &encode_jpeg_i420(&f, YuvSpec::BT601_FULL, 90).unwrap(),
            &Limits::default(),
        )
        .unwrap();
        for r in [1u16, 3, 7] {
            let j = encode_jpeg_i420_with_restart(&f, YuvSpec::BT601_FULL, 90, r).unwrap();
            assert!(j.windows(2).any(|w| w == [0xFF, 0xD0]));
            let d = decode_jpeg(&j, &Limits::default()).unwrap();
            assert_eq!(d.components[0].plane, plain.components[0].plane, "r {r}");
            // RST を 1 つ壊すと誤り
            let mut bad = j.clone();
            let i = bad.windows(2).position(|w| w == [0xFF, 0xD1]).unwrap();
            bad[i + 1] = 0xD3;
            assert!(decode_jpeg(&bad, &Limits::default()).is_err());
        }
    }

    #[test]
    fn rejects_garbage_without_panic() {
        let img = photo(48, 40);
        let j = encode_jpeg_rgb(&img, 80, JpegSubsampling::S420).unwrap();
        for cut in 0..j.len() - 2 {
            assert!(
                decode_jpeg(&j[..cut], &Limits::default()).is_err(),
                "cut {cut}"
            );
        }
        let mut s = 11u32;
        for _ in 0..3000 {
            let mut m = j.clone();
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            let i = (s >> 8) as usize % m.len();
            m[i] ^= 1 << (s % 8);
            let _ = decode_jpeg(&m, &Limits::default());
        }
        let small = Limits {
            max_width: 40,
            ..Limits::default()
        };
        assert!(decode_jpeg(&j, &small).is_err());
        // プログレッシブは未対応として拒否
        let mut p = j.clone();
        let i = p.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        p[i + 1] = 0xC2;
        assert_eq!(
            decode_jpeg(&p, &Limits::default()).unwrap_err(),
            ImageError::Unsupported("プログレッシブ JPEG")
        );
    }
}
