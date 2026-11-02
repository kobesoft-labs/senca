//! H.264 のパラメーターセットと AVCDecoderConfigurationRecord（ISO/IEC 14496-15 5.3.3）。
//!
//! これはコーデック（符号化・復号）ではなく、ビットストリームの解析・形式変換（ADR-0004 の 2）。

use super::nal::{split_annexb, unescape_rbsp};
use crate::error::{CodecError, Result};
use bytes::Bytes;
use senca_core::bits::BitReader;
use senca_core::media::VideoParams;

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

/// SPS から分かること。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpsInfo {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub sps_id: u32,
    pub chroma_format_idc: u32,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub width: u32,
    pub height: u32,
}

fn high_profile(p: u8) -> bool {
    matches!(
        p,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    )
}

fn skip_scaling_list(r: &mut BitReader, size: usize) -> Option<()> {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            let delta = r.read_se()?;
            if !(-128..=127).contains(&delta) {
                return None;
            }
            next = (last + delta + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Some(())
}

/// SPS（NAL ヘッダーを含む）を解析する。
pub fn parse_sps(nal: &[u8]) -> Result<SpsInfo> {
    let bad = || CodecError::invalid_input("malformed H.264 SPS");
    if nal_type(nal) != NAL_SPS || nal.len() < 4 {
        return Err(bad());
    }
    let rbsp = unescape_rbsp(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    (|| -> Option<SpsInfo> {
        let profile_idc = r.read_u8(8)?;
        let constraint_flags = r.read_u8(8)?;
        let level_idc = r.read_u8(8)?;
        let sps_id = r.read_ue()?;
        if sps_id > 31 {
            return None;
        }
        let mut chroma_format_idc = 1;
        let mut bit_depth_luma = 8;
        let mut bit_depth_chroma = 8;
        let mut separate_colour_plane = false;
        if high_profile(profile_idc) {
            chroma_format_idc = r.read_ue()?;
            if chroma_format_idc > 3 {
                return None;
            }
            if chroma_format_idc == 3 {
                separate_colour_plane = r.read_bit()?;
            }
            let l = r.read_ue()?;
            let c = r.read_ue()?;
            if l > 6 || c > 6 {
                return None;
            }
            bit_depth_luma = 8 + l as u8;
            bit_depth_chroma = 8 + c as u8;
            r.read_bit()?; // qpprime_y_zero_transform_bypass_flag
            if r.read_bit()? {
                let n = if chroma_format_idc == 3 { 12 } else { 8 };
                for i in 0..n {
                    if r.read_bit()? {
                        skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                    }
                }
            }
        }
        let log2_max_frame_num = r.read_ue()?;
        if log2_max_frame_num > 12 {
            return None;
        }
        let poc_type = r.read_ue()?;
        match poc_type {
            0 => {
                if r.read_ue()? > 12 {
                    return None;
                }
            }
            1 => {
                r.read_bit()?;
                r.read_se()?;
                r.read_se()?;
                let n = r.read_ue()?;
                if n > 255 {
                    return None;
                }
                for _ in 0..n {
                    r.read_se()?;
                }
            }
            2 => {}
            _ => return None,
        }
        r.read_ue()?; // max_num_ref_frames
        r.read_bit()?; // gaps_in_frame_num_value_allowed_flag
        let w_mbs = r.read_ue()?.checked_add(1)?;
        let h_map = r.read_ue()?.checked_add(1)?;
        let frame_mbs_only = r.read_bit()?;
        if !frame_mbs_only {
            r.read_bit()?; // mb_adaptive_frame_field_flag
        }
        r.read_bit()?; // direct_8x8_inference_flag
        let h_mbs = h_map.checked_mul(if frame_mbs_only { 1 } else { 2 })?;
        let mut width = w_mbs.checked_mul(16)?;
        let mut height = h_mbs.checked_mul(16)?;
        if r.read_bit()? {
            let l = r.read_ue()?;
            let rr = r.read_ue()?;
            let t = r.read_ue()?;
            let b = r.read_ue()?;
            let (cx, cy) = match (chroma_format_idc, separate_colour_plane) {
                (0, _) | (3, true) => (1, if frame_mbs_only { 1 } else { 2 }),
                (1, _) => (2, 2 * if frame_mbs_only { 1 } else { 2 }),
                (2, _) => (2, if frame_mbs_only { 1 } else { 2 }),
                _ => (1, if frame_mbs_only { 1 } else { 2 }),
            };
            let cw = l.checked_add(rr)?.checked_mul(cx)?;
            let ch = t.checked_add(b)?.checked_mul(cy)?;
            width = width.checked_sub(cw)?;
            height = height.checked_sub(ch)?;
        }
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            return None;
        }
        Some(SpsInfo {
            profile_idc,
            constraint_flags,
            level_idc,
            sps_id,
            chroma_format_idc,
            bit_depth_luma,
            bit_depth_chroma,
            width,
            height,
        })
    })()
    .ok_or_else(bad)
}

/// avcC の中身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcRecord {
    pub profile_idc: u8,
    pub compat: u8,
    pub level_idc: u8,
    /// NAL の長さのバイト数（1, 2, 4）。
    pub length_size: usize,
    pub sps: Vec<Bytes>,
    pub pps: Vec<Bytes>,
}

/// SPS・PPS から AVCDecoderConfigurationRecord を作る（長さは 4 バイト）。
pub fn build_avcc(sps: &[&[u8]], pps: &[&[u8]]) -> Result<Bytes> {
    let first = sps
        .first()
        .ok_or_else(|| CodecError::invalid_input("avcC needs an SPS"))?;
    if pps.is_empty() || sps.len() > 31 || pps.len() > 255 {
        return Err(CodecError::invalid_input(
            "avcC needs 1..31 SPS and 1..255 PPS",
        ));
    }
    let info = parse_sps(first)?;
    let mut out = vec![
        1,
        first[1],
        first[2],
        first[3],
        0xfc | 3,
        0xe0 | sps.len() as u8,
    ];
    for s in sps {
        let n = u16::try_from(s.len()).map_err(|_| CodecError::invalid_input("SPS too long"))?;
        out.extend_from_slice(&n.to_be_bytes());
        out.extend_from_slice(s);
    }
    out.push(pps.len() as u8);
    for p in pps {
        let n = u16::try_from(p.len()).map_err(|_| CodecError::invalid_input("PPS too long"))?;
        out.extend_from_slice(&n.to_be_bytes());
        out.extend_from_slice(p);
    }
    if matches!(info.profile_idc, 100 | 110 | 122 | 144) {
        out.push(0xfc | (info.chroma_format_idc as u8 & 3));
        out.push(0xf8 | (info.bit_depth_luma - 8));
        out.push(0xf8 | (info.bit_depth_chroma - 8));
        out.push(0);
    }
    Ok(Bytes::from(out))
}

/// AVCDecoderConfigurationRecord を読む。
pub fn parse_avcc(rec: &[u8]) -> Result<AvcRecord> {
    let bad = || CodecError::invalid_input("malformed avcC");
    if rec.len() < 7 || rec[0] != 1 {
        return Err(bad());
    }
    let length_size = (rec[4] & 3) as usize + 1;
    if length_size == 3 {
        return Err(bad());
    }
    let mut p = 5;
    let read_sets = |count: usize, p: &mut usize| -> Result<Vec<Bytes>> {
        let mut v = Vec::with_capacity(count);
        for _ in 0..count {
            if rec.len() < *p + 2 {
                return Err(bad());
            }
            let n = u16::from_be_bytes([rec[*p], rec[*p + 1]]) as usize;
            *p += 2;
            if n == 0 || rec.len() < *p + n {
                return Err(bad());
            }
            v.push(Bytes::copy_from_slice(&rec[*p..*p + n]));
            *p += n;
        }
        Ok(v)
    };
    let nsps = (rec[p] & 0x1f) as usize;
    p += 1;
    let sps = read_sets(nsps, &mut p)?;
    if rec.len() < p + 1 {
        return Err(bad());
    }
    let npps = rec[p] as usize;
    p += 1;
    let pps = read_sets(npps, &mut p)?;
    Ok(AvcRecord {
        profile_idc: rec[1],
        compat: rec[2],
        level_idc: rec[3],
        length_size,
        sps,
        pps,
    })
}

/// Annex B のエンコーダー出力から、パラメーターセット（SPS/PPS）と残りの NAL を分ける。
/// AUD は捨てる（内部形式に入れない）。
/// （SPS, PPS, その他）。
pub type SplitSets<'a> = (Vec<&'a [u8]>, Vec<&'a [u8]>, Vec<&'a [u8]>);

pub fn split_parameter_sets(annexb: &[u8]) -> SplitSets<'_> {
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    let mut rest = Vec::new();
    for n in split_annexb(annexb) {
        match nal_type(n) {
            NAL_SPS => sps.push(n),
            NAL_PPS => pps.push(n),
            NAL_AUD => {}
            _ => rest.push(n),
        }
    }
    (sps, pps, rest)
}

/// SPS から `VideoParams` を作る。
pub fn video_params(sps: &[u8]) -> Result<VideoParams> {
    let i = parse_sps(sps)?;
    Ok(VideoParams {
        width: i.width,
        height: i.height,
        profile: i.profile_idc,
        compat: i.constraint_flags,
        level: i.level_idc,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// x264 の 320x240 High の SPS（参考用の既知の値）。
    pub const SPS_320X240_HIGH: &[u8] = &[
        0x67, 0x64, 0x00, 0x0d, 0xac, 0xd9, 0x41, 0x41, 0xfb, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00,
        0x10, 0x00, 0x00, 0x03, 0x03, 0xc0, 0xf1, 0x42, 0x99, 0x60,
    ];
    pub const PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0];

    #[test]
    fn sps_parse() {
        let i = parse_sps(SPS_320X240_HIGH).unwrap();
        assert_eq!(
            (i.profile_idc, i.level_idc, i.width, i.height),
            (100, 13, 320, 240)
        );
        assert_eq!((i.chroma_format_idc, i.bit_depth_luma), (1, 8));
    }

    #[test]
    fn avcc_roundtrip() {
        let rec = build_avcc(&[SPS_320X240_HIGH], &[PPS]).unwrap();
        assert_eq!(&rec[..6], &[1, 0x64, 0x00, 0x0d, 0xff, 0xe1]);
        let r = parse_avcc(&rec).unwrap();
        assert_eq!(r.length_size, 4);
        assert_eq!(r.sps[0].as_ref(), SPS_320X240_HIGH);
        assert_eq!(r.pps[0].as_ref(), PPS);
        // High の拡張（4:2:0、8 bit）
        assert_eq!(&rec[rec.len() - 4..], &[0xfd, 0xf8, 0xf8, 0]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_sps(&[0x67]).is_err());
        assert!(parse_sps(&[0x67, 0x42, 0, 0x1e]).is_err());
        assert!(parse_sps(&[0x67, 0x42, 0, 0x1e, 0x00, 0x00]).is_err());
        assert!(parse_avcc(&[1, 2, 3]).is_err());
        assert!(parse_avcc(&[1, 0x64, 0, 0x0d, 0xff, 0xe1, 0, 50, 1]).is_err());
        assert!(build_avcc(&[], &[PPS]).is_err());
    }

    #[test]
    fn split_sets() {
        let mut d = vec![0, 0, 0, 1, 9, 0xf0, 0, 0, 0, 1];
        d.extend_from_slice(SPS_320X240_HIGH);
        d.extend_from_slice(&[0, 0, 1]);
        d.extend_from_slice(PPS);
        d.extend_from_slice(&[0, 0, 1, 0x65, 0x88]);
        let (s, p, r) = split_parameter_sets(&d);
        assert_eq!((s.len(), p.len(), r), (1, 1, vec![&[0x65, 0x88][..]]));
    }
}
