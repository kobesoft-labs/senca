//! H.265 のパラメーターセットと HEVCDecoderConfigurationRecord（ISO/IEC 14496-15 8.3.3）。

use super::nal::{split_annexb, unescape_rbsp};
use crate::error::{CodecError, Result};
use bytes::Bytes;
use senca_core::bits::BitReader;
use senca_core::media::VideoParams;

pub const NAL_IDR_W_RADL: u8 = 19;
pub const NAL_IDR_N_LP: u8 = 20;
pub const NAL_CRA: u8 = 21;
pub const NAL_VPS: u8 = 32;
pub const NAL_SPS: u8 = 33;
pub const NAL_PPS: u8 = 34;
pub const NAL_AUD: u8 = 35;

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| (b >> 1) & 0x3f)
}

/// IRAP（BLA・IDR・CRA、16..=23）か。
pub fn is_irap(nal: &[u8]) -> bool {
    (16..=23).contains(&nal_type(nal))
}

/// SPS から分かること。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpsInfo {
    /// general_profile_space(2) tier(1) profile_idc(5) の 1 バイト。
    pub profile_byte: u8,
    pub profile_idc: u8,
    pub tier: bool,
    pub compat_flags: u32,
    /// general_constraint_indicator_flags（48 bit）。
    pub constraint_flags: [u8; 6],
    pub level_idc: u8,
    pub max_sub_layers: u8,
    pub temporal_id_nested: bool,
    pub chroma_format_idc: u32,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub width: u32,
    pub height: u32,
}

/// SPS（2 バイトの NAL ヘッダーを含む）を解析する。
pub fn parse_sps(nal: &[u8]) -> Result<SpsInfo> {
    let bad = || CodecError::invalid_input("malformed H.265 SPS");
    if nal_type(nal) != NAL_SPS || nal.len() < 16 {
        return Err(bad());
    }
    let rbsp = unescape_rbsp(&nal[2..]);
    let mut r = BitReader::new(&rbsp);
    (|| -> Option<SpsInfo> {
        r.read_bits(4)?; // sps_video_parameter_set_id
        let max_sub_layers_minus1 = r.read_u8(3)?;
        if max_sub_layers_minus1 > 6 {
            return None;
        }
        let temporal_id_nested = r.read_bit()?;
        // profile_tier_level(1, max_sub_layers_minus1)
        let profile_byte = r.read_u8(8)?;
        let compat_flags = r.read_u32(32)?;
        let mut constraint_flags = [0u8; 6];
        for b in &mut constraint_flags {
            *b = r.read_u8(8)?;
        }
        let level_idc = r.read_u8(8)?;
        let mut prof = [false; 8];
        let mut lvl = [false; 8];
        for i in 0..max_sub_layers_minus1 as usize {
            prof[i] = r.read_bit()?;
            lvl[i] = r.read_bit()?;
        }
        if max_sub_layers_minus1 > 0 {
            for _ in max_sub_layers_minus1..8 {
                r.read_bits(2)?;
            }
        }
        for i in 0..max_sub_layers_minus1 as usize {
            if prof[i] {
                r.skip(88)?;
            }
            if lvl[i] {
                r.skip(8)?;
            }
        }
        let sps_id = r.read_ue()?;
        if sps_id > 15 {
            return None;
        }
        let chroma_format_idc = r.read_ue()?;
        if chroma_format_idc > 3 {
            return None;
        }
        let mut separate = false;
        if chroma_format_idc == 3 {
            separate = r.read_bit()?;
        }
        let mut width = r.read_ue()?;
        let mut height = r.read_ue()?;
        if r.read_bit()? {
            let (sw, sh) = match (chroma_format_idc, separate) {
                (1, _) => (2, 2),
                (2, _) => (2, 1),
                _ => (1, 1),
            };
            let l = r.read_ue()?;
            let rr = r.read_ue()?;
            let t = r.read_ue()?;
            let b = r.read_ue()?;
            width = width.checked_sub(l.checked_add(rr)?.checked_mul(sw)?)?;
            height = height.checked_sub(t.checked_add(b)?.checked_mul(sh)?)?;
        }
        let l = r.read_ue()?;
        let c = r.read_ue()?;
        if l > 8 || c > 8 || width == 0 || height == 0 || width > 16384 || height > 16384 {
            return None;
        }
        Some(SpsInfo {
            profile_byte,
            profile_idc: profile_byte & 0x1f,
            tier: profile_byte & 0x20 != 0,
            compat_flags,
            constraint_flags,
            level_idc,
            max_sub_layers: max_sub_layers_minus1 + 1,
            temporal_id_nested,
            chroma_format_idc,
            bit_depth_luma: 8 + l as u8,
            bit_depth_chroma: 8 + c as u8,
            width,
            height,
        })
    })()
    .ok_or_else(bad)
}

/// hvcC の中身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcRecord {
    pub length_size: usize,
    pub vps: Vec<Bytes>,
    pub sps: Vec<Bytes>,
    pub pps: Vec<Bytes>,
}

/// VPS・SPS・PPS から HEVCDecoderConfigurationRecord を作る（長さは 4 バイト）。
pub fn build_hvcc(vps: &[&[u8]], sps: &[&[u8]], pps: &[&[u8]]) -> Result<Bytes> {
    let first = sps
        .first()
        .ok_or_else(|| CodecError::invalid_input("hvcC needs an SPS"))?;
    if vps.is_empty() || pps.is_empty() {
        return Err(CodecError::invalid_input("hvcC needs VPS, SPS and PPS"));
    }
    let i = parse_sps(first)?;
    let mut out = Vec::new();
    out.push(1);
    out.push(i.profile_byte);
    out.extend_from_slice(&i.compat_flags.to_be_bytes());
    out.extend_from_slice(&i.constraint_flags);
    out.push(i.level_idc);
    out.extend_from_slice(&0xf000u16.to_be_bytes()); // min_spatial_segmentation_idc = 0
    out.push(0xfc); // parallelismType = 0（不明）
    out.push(0xfc | (i.chroma_format_idc as u8 & 3));
    out.push(0xf8 | (i.bit_depth_luma - 8));
    out.push(0xf8 | (i.bit_depth_chroma - 8));
    out.extend_from_slice(&[0, 0]); // avgFrameRate
    // constantFrameRate=0, numTemporalLayers, temporalIdNested, lengthSizeMinusOne=3
    out.push(((i.max_sub_layers & 7) << 3) | ((i.temporal_id_nested as u8) << 2) | 3);
    out.push(3);
    for (t, sets) in [(NAL_VPS, vps), (NAL_SPS, sps), (NAL_PPS, pps)] {
        out.push(0x80 | t); // array_completeness = 1
        let n = u16::try_from(sets.len())
            .map_err(|_| CodecError::invalid_input("too many parameter sets"))?;
        out.extend_from_slice(&n.to_be_bytes());
        for s in sets {
            let n = u16::try_from(s.len())
                .map_err(|_| CodecError::invalid_input("parameter set too long"))?;
            out.extend_from_slice(&n.to_be_bytes());
            out.extend_from_slice(s);
        }
    }
    Ok(Bytes::from(out))
}

/// HEVCDecoderConfigurationRecord を読む。
pub fn parse_hvcc(rec: &[u8]) -> Result<HevcRecord> {
    let bad = || CodecError::invalid_input("malformed hvcC");
    if rec.len() < 23 || rec[0] != 1 {
        return Err(bad());
    }
    let length_size = (rec[21] & 3) as usize + 1;
    if length_size == 3 {
        return Err(bad());
    }
    let narrays = rec[22] as usize;
    let mut p = 23;
    let mut out = HevcRecord {
        length_size,
        vps: vec![],
        sps: vec![],
        pps: vec![],
    };
    for _ in 0..narrays {
        if rec.len() < p + 3 {
            return Err(bad());
        }
        let t = rec[p] & 0x3f;
        let n = u16::from_be_bytes([rec[p + 1], rec[p + 2]]) as usize;
        p += 3;
        for _ in 0..n {
            if rec.len() < p + 2 {
                return Err(bad());
            }
            let len = u16::from_be_bytes([rec[p], rec[p + 1]]) as usize;
            p += 2;
            if len == 0 || rec.len() < p + len {
                return Err(bad());
            }
            let b = Bytes::copy_from_slice(&rec[p..p + len]);
            p += len;
            match t {
                NAL_VPS => out.vps.push(b),
                NAL_SPS => out.sps.push(b),
                NAL_PPS => out.pps.push(b),
                _ => {}
            }
        }
    }
    Ok(out)
}

/// Annex B から VPS・SPS・PPS と残りを分ける（AUD は捨てる）。
pub fn split_parameter_sets(annexb: &[u8]) -> ParameterSets<'_> {
    let mut ps = ParameterSets::default();
    for n in split_annexb(annexb) {
        match nal_type(n) {
            NAL_VPS => ps.vps.push(n),
            NAL_SPS => ps.sps.push(n),
            NAL_PPS => ps.pps.push(n),
            NAL_AUD => {}
            _ => ps.rest.push(n),
        }
    }
    ps
}

#[derive(Debug, Default)]
pub struct ParameterSets<'a> {
    pub vps: Vec<&'a [u8]>,
    pub sps: Vec<&'a [u8]>,
    pub pps: Vec<&'a [u8]>,
    pub rest: Vec<&'a [u8]>,
}

pub fn video_params(sps: &[u8]) -> Result<VideoParams> {
    let i = parse_sps(sps)?;
    Ok(VideoParams {
        width: i.width,
        height: i.height,
        profile: i.profile_idc,
        compat: 0,
        level: i.level_idc,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn rejects_garbage() {
        assert!(parse_sps(&[0x42, 0x01]).is_err());
        assert!(parse_hvcc(&[1; 10]).is_err());
        assert!(build_hvcc(&[], &[], &[]).is_err());
    }
}
