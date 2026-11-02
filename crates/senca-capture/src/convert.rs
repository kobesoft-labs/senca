//! 機器の生の形式から共通メディアモデル（8 bit I420）への変換（自前、仕様 2.1）。
//!
//! - YUYV・UYVY（4:2:2 の詰めた形式）→ I420: 輝度はそのまま、色差は縦の 2 行を平均（四捨五入）する。
//!   幅は偶数でなければならない（4:2:2 の 1 組が 2 画素）。高さが奇数なら最後の行の色差はその行だけ。
//! - NV12 → I420: 色差の並べ替え（`senca_codec::VideoFrame::convert`）。
//! - MJPEG → I420: `senca_image::jpeg` の復号器で復号し、JFIF の BT.601 全範囲から BT.601 限定範囲に直す
//!   （符号化器・再生側は限定範囲を前提にする）。UVC の MJPEG は Huffman 表（DHT）を省くことが多い
//!   （AVI1 の慣習、ITU-T T.81 Annex K.3 の標準の表を使う前提）ので、無ければ標準の DHT を補ってから復号する。
//! - YUYV・UYVY・NV12 はそのままの範囲（UVC・SDI は限定範囲）とみなす。
//!
//! 入力の長さ・stride はすべて検査し、足りなければ `InvalidInput` を返す（壊れたバッファで落ちない）。

use crate::error::{CaptureError, Result};
use crate::types::RawFormat;
use bytes::Bytes;
use senca_codec::{PixelFormat, VideoFrame};
use senca_core::Timebase;
use senca_image::jpeg::decode_jpeg;
use senca_image::{I420Frame, Limits, YuvSpec};

/// 詰めた I420 の長さ。
pub fn i420_len(width: u32, height: u32) -> usize {
    PixelFormat::I420.packed_size(width, height)
}

fn check_packed_422(data: &[u8], width: u32, height: u32, stride: usize) -> Result<()> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) {
        return Err(CaptureError::invalid_input(format!(
            "4:2:2 packed frame needs a non-zero even width ({width}x{height})"
        )));
    }
    let row = width as usize * 2;
    if stride < row {
        return Err(CaptureError::invalid_input("stride smaller than row"));
    }
    let need = stride
        .checked_mul(height as usize - 1)
        .and_then(|v| v.checked_add(row))
        .ok_or_else(|| CaptureError::invalid_input("frame size overflow"))?;
    if data.len() < need {
        return Err(CaptureError::invalid_input(format!(
            "frame too short: {} < {need}",
            data.len()
        )));
    }
    Ok(())
}

/// 4:2:2 の詰めた形式を I420 にする。`y0`・`u`・`y1`・`v` は 4 バイトの組の中の位置。
fn packed_422_to_i420(
    data: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    pos: [usize; 4],
) -> Result<Vec<u8>> {
    check_packed_422(data, width, height, stride)?;
    let (w, h) = (width as usize, height as usize);
    let (cw, ch) = (w / 2, h.div_ceil(2));
    let mut out = vec![0u8; i420_len(width, height)];
    let (yp, rest) = out.split_at_mut(w * h);
    let (up, vp) = rest.split_at_mut(cw * ch);
    let [py0, pu, py1, pv] = pos;
    for y in 0..h {
        let row = &data[y * stride..y * stride + w * 2];
        let yrow = &mut yp[y * w..(y + 1) * w];
        for (x, quad) in row.as_chunks::<4>().0.iter().enumerate() {
            yrow[2 * x] = quad[py0];
            yrow[2 * x + 1] = quad[py1];
        }
    }
    for cy in 0..ch {
        let r0 = &data[2 * cy * stride..2 * cy * stride + w * 2];
        let r1 = if 2 * cy + 1 < h {
            &data[(2 * cy + 1) * stride..(2 * cy + 1) * stride + w * 2]
        } else {
            r0
        };
        let urow = &mut up[cy * cw..(cy + 1) * cw];
        let vrow = &mut vp[cy * cw..(cy + 1) * cw];
        for x in 0..cw {
            let (a, b) = (&r0[4 * x..4 * x + 4], &r1[4 * x..4 * x + 4]);
            urow[x] = (a[pu] as u16 + b[pu] as u16).div_ceil(2) as u8;
            vrow[x] = (a[pv] as u16 + b[pv] as u16).div_ceil(2) as u8;
        }
    }
    Ok(out)
}

/// YUYV（Y0 U Y1 V）を詰めた I420 にする。
pub fn yuyv_to_i420(data: &[u8], width: u32, height: u32, stride: usize) -> Result<Vec<u8>> {
    packed_422_to_i420(data, width, height, stride, [0, 1, 2, 3])
}

/// UYVY（U Y0 V Y1）を詰めた I420 にする。
pub fn uyvy_to_i420(data: &[u8], width: u32, height: u32, stride: usize) -> Result<Vec<u8>> {
    packed_422_to_i420(data, width, height, stride, [1, 0, 3, 2])
}

fn i420_to_packed_422(i420: &[u8], width: u32, height: u32, pos: [usize; 4]) -> Result<Vec<u8>> {
    if !width.is_multiple_of(2) || i420.len() != i420_len(width, height) {
        return Err(CaptureError::invalid_input("bad I420 buffer"));
    }
    let (w, h) = (width as usize, height as usize);
    let cw = w / 2;
    let (yp, rest) = i420.split_at(w * h);
    let (up, vp) = rest.split_at(cw * h.div_ceil(2));
    let mut out = vec![0u8; w * h * 2];
    let [py0, pu, py1, pv] = pos;
    for y in 0..h {
        let cy = y / 2;
        for x in 0..cw {
            let o = &mut out[y * w * 2 + 4 * x..y * w * 2 + 4 * x + 4];
            o[py0] = yp[y * w + 2 * x];
            o[py1] = yp[y * w + 2 * x + 1];
            o[pu] = up[cy * cw + x];
            o[pv] = vp[cy * cw + x];
        }
    }
    Ok(out)
}

/// 詰めた I420 を YUYV にする（試験用の機器・試験で使う。色差は縦に複製）。
pub fn i420_to_yuyv(i420: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    i420_to_packed_422(i420, width, height, [0, 1, 2, 3])
}

/// 詰めた I420 を UYVY にする。
pub fn i420_to_uyvy(i420: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    i420_to_packed_422(i420, width, height, [1, 0, 3, 2])
}

/// stride 付きの I420（V4L2 `YU12`、色差の stride は輝度の半分）を詰める。
fn planar_i420(data: &[u8], width: u32, height: u32, stride: usize) -> Result<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let cs = stride.div_ceil(2);
    if stride < w {
        return Err(CaptureError::invalid_input("stride smaller than row"));
    }
    // 最後の色差の行は stride 分なくてもよい。
    let need = stride * h + cs * ch + cs * (ch - 1) + cw;
    if data.len() < need {
        return Err(CaptureError::invalid_input("I420 frame too short"));
    }
    let mut out = Vec::with_capacity(i420_len(width, height));
    for y in 0..h {
        out.extend_from_slice(&data[y * stride..y * stride + w]);
    }
    for p in 0..2 {
        let base = stride * h + p * cs * ch;
        for y in 0..ch {
            out.extend_from_slice(&data[base + y * cs..base + y * cs + cw]);
        }
    }
    Ok(out)
}

/// stride 付きの NV12 を詰めた I420 にする。
fn nv12_to_i420(data: &[u8], width: u32, height: u32, stride: usize) -> Result<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    if stride < w.next_multiple_of(2) {
        return Err(CaptureError::invalid_input("stride smaller than row"));
    }
    let need = stride * h + stride * (ch - 1) + cw * 2;
    if data.len() < need {
        return Err(CaptureError::invalid_input("NV12 frame too short"));
    }
    let mut out = Vec::with_capacity(i420_len(width, height));
    for y in 0..h {
        out.extend_from_slice(&data[y * stride..y * stride + w]);
    }
    for off in 0..2 {
        for y in 0..ch {
            let row = &data[stride * h + y * stride..stride * h + y * stride + cw * 2];
            out.extend(row.iter().skip(off).step_by(2));
        }
    }
    Ok(out)
}

/// JPEG に DHT が無ければ、最初の SOS の前に標準の Huffman 表（T.81 Annex K.3）を挟む。
pub fn ensure_huffman_tables(jpeg: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>> {
    if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return Err(CaptureError::invalid_input("MJPEG frame without SOI"));
    }
    let mut pos = 2;
    loop {
        if pos + 4 > jpeg.len() || jpeg[pos] != 0xFF {
            return Err(CaptureError::invalid_input(
                "MJPEG frame: bad marker sequence",
            ));
        }
        let marker = jpeg[pos + 1];
        if marker == 0xFF {
            pos += 1; // 詰め物
            continue;
        }
        match marker {
            0xC4 => return Ok(std::borrow::Cow::Borrowed(jpeg)),
            0xDA => {
                let mut out = Vec::with_capacity(jpeg.len() + 432);
                out.extend_from_slice(&jpeg[..pos]);
                out.extend_from_slice(&standard_dht());
                out.extend_from_slice(&jpeg[pos..]);
                return Ok(std::borrow::Cow::Owned(out));
            }
            0xD8 | 0xD9 | 0x01 | 0xD0..=0xD7 => {
                return Err(CaptureError::invalid_input(
                    "MJPEG frame: unexpected marker",
                ));
            }
            _ => {
                let len = u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]) as usize;
                if len < 2 {
                    return Err(CaptureError::invalid_input(
                        "MJPEG frame: bad segment length",
                    ));
                }
                pos += 2 + len;
            }
        }
    }
}

/// 標準の 4 つの Huffman 表をまとめた DHT セグメント。
pub fn standard_dht() -> Vec<u8> {
    let tables: [(u8, &[u8; 16], &[u8]); 4] = [
        (0x00, &DC_LUMA_BITS, &DC_VALS),
        (0x10, &AC_LUMA_BITS, &AC_LUMA_VALS),
        (0x01, &DC_CHROMA_BITS, &DC_VALS),
        (0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALS),
    ];
    let body: usize = tables.iter().map(|(_, _, v)| 17 + v.len()).sum();
    let mut out = vec![0xFF, 0xC4];
    out.extend_from_slice(&((body + 2) as u16).to_be_bytes());
    for (tc_th, bits, vals) in tables {
        out.push(tc_th);
        out.extend_from_slice(bits);
        out.extend_from_slice(vals);
    }
    out
}

/// MJPEG の 1 フレームを詰めた I420（BT.601 限定範囲）にする。大きさが `width`x`height` でなければ `Err`。
pub fn mjpeg_to_i420(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let jpeg = ensure_huffman_tables(data)?;
    let limits = Limits {
        max_width: width as usize,
        max_height: height as usize,
        max_pixels: width as usize * height as usize,
    };
    let dec = decode_jpeg(&jpeg, &limits)?;
    if dec.width != width as usize || dec.height != height as usize {
        return Err(CaptureError::invalid_input(format!(
            "MJPEG frame is {}x{}, expected {width}x{height}",
            dec.width, dec.height
        )));
    }
    let full = dec.to_i420()?;
    let limited =
        senca_image::color::convert_i420(&full, YuvSpec::BT601_FULL, YuvSpec::BT601_LIMITED);
    Ok(limited.to_bytes().to_vec())
}

/// 詰めた I420（BT.601 限定範囲）を MJPEG（JFIF、4:2:0）にする（試験用の機器・試験で使う）。
pub fn i420_to_mjpeg(i420: &[u8], width: u32, height: u32, quality: u8) -> Result<Vec<u8>> {
    let f = I420Frame::from_packed(width as usize, height as usize, i420)?;
    Ok(senca_image::jpeg::encode_jpeg_i420(
        &f,
        YuvSpec::BT601_LIMITED,
        quality,
    )?)
}

/// 機器の生のフレームを I420 の [`VideoFrame`] にする。
///
/// `stride` は 1 行のバイト数（V4L2 の `bytesperline`、DeckLink の `GetRowBytes`。MJPEG では無視）。
pub fn decode_raw_video(
    format: RawFormat,
    width: u32,
    height: u32,
    stride: usize,
    data: &[u8],
    pts: i64,
    timebase: Timebase,
) -> Result<VideoFrame> {
    senca_codec::frame::check_dimensions(width, height)?;
    let packed = match format {
        RawFormat::I420 => planar_i420(data, width, height, stride)?,
        RawFormat::Nv12 => nv12_to_i420(data, width, height, stride)?,
        RawFormat::Yuyv => yuyv_to_i420(data, width, height, stride)?,
        RawFormat::Uyvy => uyvy_to_i420(data, width, height, stride)?,
        RawFormat::Mjpeg => mjpeg_to_i420(data, width, height)?,
        RawFormat::H264 | RawFormat::PcmS16 => {
            return Err(CaptureError::internal(format!(
                "{} is not a raw video format",
                format.as_str()
            )));
        }
    };
    Ok(VideoFrame::from_packed(
        PixelFormat::I420,
        width,
        height,
        Bytes::from(packed),
        pts,
        timebase,
    )?)
}

// T.81 Annex K.3 の標準の Huffman 表（senca-image の符号化器と同じ値）。
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 決まった模様の I420（輝度は斜めの勾配、色差は横・縦の勾配）。
    fn pattern(w: u32, h: u32) -> Vec<u8> {
        let (wu, hu) = (w as usize, h as usize);
        let (cw, ch) = (wu.div_ceil(2), hu.div_ceil(2));
        let mut v = Vec::new();
        for y in 0..hu {
            for x in 0..wu {
                v.push((16 + (x * 3 + y * 5) % 220) as u8);
            }
        }
        for y in 0..ch {
            for x in 0..cw {
                v.push((16 + (x * 7) % 224) as u8 + (y % 2) as u8);
            }
        }
        for y in 0..ch {
            for x in 0..cw {
                v.push((240 - (y * 9) % 224) as u8 - (x % 2) as u8);
            }
        }
        v
    }

    #[test]
    fn yuyv_uyvy_roundtrip_exact() {
        // 色差を縦に複製してから平均するので、元に戻る。
        for (w, h) in [(16, 8), (6, 4), (2, 2)] {
            let src = pattern(w, h);
            let yuyv = i420_to_yuyv(&src, w, h).unwrap();
            assert_eq!(yuyv_to_i420(&yuyv, w, h, w as usize * 2).unwrap(), src);
            let uyvy = i420_to_uyvy(&src, w, h).unwrap();
            assert_eq!(uyvy_to_i420(&uyvy, w, h, w as usize * 2).unwrap(), src);
            assert_ne!(yuyv, uyvy);
        }
    }

    #[test]
    fn yuyv_known_values_and_averaging() {
        // 2x2: Y0 U Y1 V / Y2 U' Y3 V'
        let data = [10, 100, 20, 200, 30, 101, 40, 203];
        let out = yuyv_to_i420(&data, 2, 2, 4).unwrap();
        assert_eq!(out, vec![10, 20, 30, 40, 101, 202]); // (100+101+1)/2=101, (200+203+1)/2=202
        let data = [100, 10, 200, 20, 101, 30, 203, 40];
        assert_eq!(
            uyvy_to_i420(&data, 2, 2, 4).unwrap(),
            vec![10, 20, 30, 40, 101, 202]
        );
    }

    #[test]
    fn stride_and_odd_height() {
        // stride に余り、高さ 3（最後の色差は 1 行だけ）
        let (w, h, stride) = (4u32, 3u32, 12usize);
        let mut data = vec![0xEEu8; stride * 3];
        for y in 0..3 {
            for x in 0..2 {
                let o = y * stride + 4 * x;
                data[o..o + 4].copy_from_slice(&[
                    (y * 10 + x) as u8,
                    50 + y as u8,
                    99,
                    150 + y as u8,
                ]);
            }
        }
        let out = yuyv_to_i420(&data, w, h, stride).unwrap();
        assert_eq!(out.len(), 12 + 4 + 4);
        assert_eq!(&out[..4], &[0, 99, 1, 99]);
        // 色差: 行 0 と 1 の平均、行 2 はそのまま
        assert_eq!(&out[12..16], &[51, 51, 52, 52]);
        assert_eq!(&out[16..20], &[151, 151, 152, 152]);
        // 最後の行は stride 分なくてもよい
        assert!(yuyv_to_i420(&data[..stride * 2 + 8], w, h, stride).is_ok());
        assert!(yuyv_to_i420(&data[..stride * 2 + 7], w, h, stride).is_err());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(yuyv_to_i420(&[0; 12], 3, 2, 6).is_err());
        assert!(yuyv_to_i420(&[0; 8], 2, 2, 2).is_err());
        assert!(yuyv_to_i420(&[0; 7], 2, 2, 4).is_err());
        assert!(decode_raw_video(RawFormat::H264, 2, 2, 4, &[0; 8], 0, Timebase::NANOS).is_err());
        assert!(decode_raw_video(RawFormat::Yuyv, 0, 2, 4, &[0; 8], 0, Timebase::NANOS).is_err());
        assert!(mjpeg_to_i420(b"not a jpeg", 16, 16).is_err());
        assert!(mjpeg_to_i420(&[0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x01], 16, 16).is_err());
    }

    #[test]
    fn nv12_and_planar_with_stride() {
        let (w, h) = (6u32, 4u32);
        let src = pattern(w, h);
        let f = VideoFrame::from_packed(
            PixelFormat::I420,
            w,
            h,
            Bytes::from(src.clone()),
            0,
            Timebase::NANOS,
        )
        .unwrap();
        let nv12 = f.convert(PixelFormat::Nv12).unwrap().to_packed().unwrap();
        // stride 8 で並べ直す
        let stride = 8usize;
        let mut padded = Vec::new();
        for y in 0..h as usize {
            padded.extend_from_slice(&nv12[y * 6..y * 6 + 6]);
            padded.extend_from_slice(&[0, 0]);
        }
        for y in 0..2 {
            padded.extend_from_slice(&nv12[24 + y * 6..24 + y * 6 + 6]);
            padded.extend_from_slice(&[0, 0]);
        }
        let out =
            decode_raw_video(RawFormat::Nv12, w, h, stride, &padded, 5, Timebase::NANOS).unwrap();
        assert_eq!(out.to_packed().unwrap(), src);
        assert_eq!(out.pts, 5);
        // 詰めた I420（stride = 幅）
        let out = decode_raw_video(RawFormat::I420, w, h, 6, &src, 0, Timebase::NANOS).unwrap();
        assert_eq!(out.to_packed().unwrap(), src);
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let se: f64 = a
            .iter()
            .zip(b)
            .map(|(x, y)| (*x as f64 - *y as f64).powi(2))
            .sum();
        let mse = se / a.len() as f64;
        if mse == 0.0 {
            99.0
        } else {
            10.0 * (255.0f64 * 255.0 / mse).log10()
        }
    }

    /// 滑らかな模様（JPEG の誤差を見るため）。
    fn smooth(w: u32, h: u32) -> Vec<u8> {
        let (wu, hu) = (w as usize, h as usize);
        let mut v = Vec::new();
        for y in 0..hu {
            for x in 0..wu {
                v.push((16 + (x * 200 / wu + y * 19 / hu) as u32) as u8);
            }
        }
        for p in 0..2 {
            for y in 0..hu.div_ceil(2) {
                for x in 0..wu.div_ceil(2) {
                    v.push((64 + p * 64 + x * 60 / wu + y * 40 / hu) as u8);
                }
            }
        }
        v
    }

    #[test]
    fn mjpeg_roundtrip_via_senca_image() {
        let (w, h) = (64u32, 48u32);
        let src = smooth(w, h);
        let jpeg = i420_to_mjpeg(&src, w, h, 95).unwrap();
        let out = mjpeg_to_i420(&jpeg, w, h).unwrap();
        let p = psnr(&src, &out);
        assert!(p > 38.0, "psnr {p}");
        // 大きさが違えば拒否
        assert!(mjpeg_to_i420(&jpeg, 32, 48).is_err());
    }

    /// DHT を抜いた JPEG（UVC の MJPEG の形）も復号できる。
    #[test]
    fn mjpeg_without_dht() {
        let (w, h) = (32u32, 16u32);
        let src = smooth(w, h);
        let jpeg = i420_to_mjpeg(&src, w, h, 90).unwrap();
        let with = mjpeg_to_i420(&jpeg, w, h).unwrap();
        // DHT のセグメントを取り除く
        let mut stripped = jpeg[..2].to_vec();
        let mut pos = 2;
        loop {
            let marker = jpeg[pos + 1];
            if marker == 0xDA {
                stripped.extend_from_slice(&jpeg[pos..]);
                break;
            }
            let len = u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]) as usize;
            if marker != 0xC4 {
                stripped.extend_from_slice(&jpeg[pos..pos + 2 + len]);
            }
            pos += 2 + len;
        }
        assert!(stripped.len() < jpeg.len());
        assert!(matches!(
            ensure_huffman_tables(&stripped).unwrap(),
            std::borrow::Cow::Owned(_)
        ));
        assert!(matches!(
            ensure_huffman_tables(&jpeg).unwrap(),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(mjpeg_to_i420(&stripped, w, h).unwrap(), with);
    }
}
