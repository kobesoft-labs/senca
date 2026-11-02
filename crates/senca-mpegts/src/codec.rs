//! 多重化・分離に要る最小限のビットストリーム処理。
//!
//! H.264/H.265 の NAL（Annex B と長さ前置の変換、SPS の最小限の解析、設定レコードの組み立てと読み取り）、
//! AAC の ADTS と AudioSpecificConfig、Opus の TOC と OpusHead を扱う。
//! 他の crate（`senca-h264` など）には依存せず、TS に要る範囲だけをここに持つ。

use senca_core::bits::BitReader;

// ---------------------------------------------------------------------------
// NAL の列挙

/// Annex B（開始コード 00 00 01 / 00 00 00 01）の NAL を列挙する。末尾の 0 は落とす。
pub(crate) fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &(_, s)) in starts.iter().enumerate() {
        let mut e = if k + 1 < starts.len() { starts[k + 1].0 } else { data.len() };
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out
}

/// 4 バイト長さ前置の NAL を列挙する。長さが合わなければ `None`。
pub(crate) fn avcc_nals(data: &[u8]) -> Option<Vec<&[u8]>> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p < data.len() {
        let len_bytes = data.get(p..p + 4)?;
        let len = u32::from_be_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;
        p += 4;
        let nal = data.get(p..p.checked_add(len)?)?;
        if !nal.is_empty() {
            out.push(nal);
        }
        p += len;
    }
    Some(out)
}

/// emulation_prevention_three_byte を取り除く。
pub(crate) fn unescape_rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

// ---------------------------------------------------------------------------
// H.264

pub(crate) mod h264_nal {
    pub const IDR: u8 = 5;
    pub const SPS: u8 = 7;
    pub const PPS: u8 = 8;
    pub const AUD: u8 = 9;
}

/// H.264 SPS から取り出す値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct H264Sps {
    pub id: u32,
    pub profile_idc: u8,
    pub constraint: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u32,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub width: u32,
    pub height: u32,
}

fn h264_high_profile(p: u8) -> bool {
    matches!(p, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135)
}

fn skip_scaling_list(r: &mut BitReader<'_>, size: usize) -> Option<()> {
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
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// SPS（NAL ヘッダーを含む）を解析する。
pub(crate) fn parse_h264_sps(nal: &[u8]) -> Option<H264Sps> {
    if nal.len() < 4 || nal[0] & 0x1F != h264_nal::SPS {
        return None;
    }
    let rbsp = unescape_rbsp(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.read_u8(8)?;
    let constraint = r.read_u8(8)?;
    let level_idc = r.read_u8(8)?;
    let id = r.read_ue()?;
    if id > 31 {
        return None;
    }
    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = false;
    let mut bit_depth_luma_minus8 = 0;
    let mut bit_depth_chroma_minus8 = 0;
    if h264_high_profile(profile_idc) {
        chroma_format_idc = r.read_ue()?;
        if chroma_format_idc > 3 {
            return None;
        }
        if chroma_format_idc == 3 {
            separate_colour_plane = r.read_bit()?;
        }
        bit_depth_luma_minus8 = r.read_ue()?;
        bit_depth_chroma_minus8 = r.read_ue()?;
        if bit_depth_luma_minus8 > 6 || bit_depth_chroma_minus8 > 6 {
            return None;
        }
        r.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.read_bit()? {
            let n = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..n {
                if r.read_bit()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let _log2_max_frame_num_minus4 = r.read_ue()?;
    let poc_type = r.read_ue()?;
    match poc_type {
        0 => {
            r.read_ue()?;
        }
        1 => {
            r.skip(1)?;
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
    let _max_num_ref_frames = r.read_ue()?;
    r.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    let w_mbs = r.read_ue()? as u64 + 1;
    let h_map = r.read_ue()? as u64 + 1;
    let frame_mbs_only = r.read_bit()?;
    if !frame_mbs_only {
        r.skip(1)?;
    }
    r.skip(1)?; // direct_8x8_inference_flag
    let (mut cl, mut cr, mut ct, mut cb) = (0u64, 0u64, 0u64, 0u64);
    if r.read_bit()? {
        cl = r.read_ue()? as u64;
        cr = r.read_ue()? as u64;
        ct = r.read_ue()? as u64;
        cb = r.read_ue()? as u64;
    }
    let chroma_array_type = if separate_colour_plane { 0 } else { chroma_format_idc };
    let (sub_w, sub_h) = match chroma_array_type {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let fm = if frame_mbs_only { 1 } else { 2 };
    let crop_x = sub_w;
    let crop_y = sub_h * fm;
    let width = (w_mbs * 16).checked_sub(crop_x * (cl + cr))?;
    let height = (fm * h_map * 16).checked_sub(crop_y * (ct + cb))?;
    if width == 0 || height == 0 || width > 65535 || height > 65535 {
        return None;
    }
    Some(H264Sps {
        id,
        profile_idc,
        constraint,
        level_idc,
        chroma_format_idc,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        width: width as u32,
        height: height as u32,
    })
}

/// PPS の pic_parameter_set_id。
pub(crate) fn parse_h264_pps_id(nal: &[u8]) -> Option<u32> {
    if nal.len() < 2 {
        return None;
    }
    let rbsp = unescape_rbsp(&nal[1..nal.len().min(16)]);
    let mut r = BitReader::new(&rbsp);
    let id = r.read_ue()?;
    (id <= 255).then_some(id)
}

/// AVCDecoderConfigurationRecord を組み立てる（lengthSizeMinusOne = 3）。
pub(crate) fn build_avcc(sps: &[&[u8]], pps: &[&[u8]], info: &H264Sps) -> Option<Vec<u8>> {
    if sps.is_empty() || pps.is_empty() || sps.len() > 31 || pps.len() > 255 {
        return None;
    }
    let mut v = vec![1, info.profile_idc, info.constraint, info.level_idc, 0xFF, 0xE0 | sps.len() as u8];
    for s in sps {
        v.extend_from_slice(&u16::try_from(s.len()).ok()?.to_be_bytes());
        v.extend_from_slice(s);
    }
    v.push(pps.len() as u8);
    for p in pps {
        v.extend_from_slice(&u16::try_from(p.len()).ok()?.to_be_bytes());
        v.extend_from_slice(p);
    }
    if h264_high_profile(info.profile_idc) {
        v.push(0xFC | (info.chroma_format_idc as u8 & 3));
        v.push(0xF8 | (info.bit_depth_luma_minus8 as u8 & 7));
        v.push(0xF8 | (info.bit_depth_chroma_minus8 as u8 & 7));
        v.push(0);
    }
    Some(v)
}

/// AVCDecoderConfigurationRecord から SPS・PPS・SPS 拡張を取り出す。
pub(crate) fn parse_avcc(rec: &[u8]) -> Option<Vec<&[u8]>> {
    if rec.len() < 7 || rec[0] != 1 {
        return None;
    }
    let mut out = Vec::new();
    let mut p = 5usize;
    let nsps = (rec[p] & 0x1F) as usize;
    p += 1;
    for _ in 0..nsps {
        let n = u16::from_be_bytes([*rec.get(p)?, *rec.get(p + 1)?]) as usize;
        out.push(rec.get(p + 2..p + 2 + n)?);
        p += 2 + n;
    }
    let npps = *rec.get(p)? as usize;
    p += 1;
    for _ in 0..npps {
        let n = u16::from_be_bytes([*rec.get(p)?, *rec.get(p + 1)?]) as usize;
        out.push(rec.get(p + 2..p + 2 + n)?);
        p += 2 + n;
    }
    // High 系の拡張（numOfSequenceParameterSetExt）。無くてもよい。
    if h264_high_profile(rec[1]) && rec.len() >= p + 4 {
        let next = rec[p + 3] as usize;
        p += 4;
        for _ in 0..next {
            let Some(hdr) = rec.get(p..p + 2) else { break };
            let n = u16::from_be_bytes([hdr[0], hdr[1]]) as usize;
            let Some(nal) = rec.get(p + 2..p + 2 + n) else { break };
            out.push(nal);
            p += 2 + n;
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// H.265

pub(crate) mod h265_nal {
    pub const VPS: u8 = 32;
    pub const SPS: u8 = 33;
    pub const PPS: u8 = 34;
    pub const AUD: u8 = 35;
}

pub(crate) fn h265_nal_type(nal: &[u8]) -> u8 {
    (nal[0] >> 1) & 0x3F
}

/// H.265 SPS から取り出す値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct H265Sps {
    pub id: u32,
    pub max_sub_layers_minus1: u8,
    pub temporal_id_nesting: bool,
    /// general_profile_space〜general_level_idc の 12 バイト。
    pub ptl: [u8; 12],
    pub chroma_format_idc: u32,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub width: u32,
    pub height: u32,
}

pub(crate) fn parse_h265_sps(nal: &[u8]) -> Option<H265Sps> {
    if nal.len() < 16 || h265_nal_type(nal) != h265_nal::SPS {
        return None;
    }
    let rbsp = unescape_rbsp(&nal[2..]);
    if rbsp.len() < 13 {
        return None;
    }
    let max_sub_layers_minus1 = (rbsp[0] >> 1) & 7;
    let temporal_id_nesting = rbsp[0] & 1 == 1;
    if max_sub_layers_minus1 > 6 {
        return None;
    }
    let mut ptl = [0u8; 12];
    ptl.copy_from_slice(&rbsp[1..13]);
    let mut r = BitReader::new(&rbsp[13..]);
    let n = max_sub_layers_minus1 as usize;
    let mut prof = [false; 8];
    let mut lvl = [false; 8];
    for i in 0..n {
        prof[i] = r.read_bit()?;
        lvl[i] = r.read_bit()?;
    }
    if n > 0 {
        for _ in n..8 {
            r.skip(2)?;
        }
    }
    for i in 0..n {
        if prof[i] {
            r.skip(88)?;
        }
        if lvl[i] {
            r.skip(8)?;
        }
    }
    let id = r.read_ue()?;
    if id > 15 {
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
    let w = r.read_ue()? as u64;
    let h = r.read_ue()? as u64;
    let (mut cl, mut cr, mut ct, mut cb) = (0u64, 0u64, 0u64, 0u64);
    if r.read_bit()? {
        cl = r.read_ue()? as u64;
        cr = r.read_ue()? as u64;
        ct = r.read_ue()? as u64;
        cb = r.read_ue()? as u64;
    }
    let bit_depth_luma_minus8 = r.read_ue()?;
    let bit_depth_chroma_minus8 = r.read_ue()?;
    if bit_depth_luma_minus8 > 8 || bit_depth_chroma_minus8 > 8 {
        return None;
    }
    let (sub_w, sub_h) = match (chroma_format_idc, separate) {
        (1, _) => (2, 2),
        (2, _) => (2, 1),
        _ => (1, 1),
    };
    let width = w.checked_sub(sub_w * (cl + cr))?;
    let height = h.checked_sub(sub_h * (ct + cb))?;
    if width == 0 || height == 0 || width > 65535 || height > 65535 {
        return None;
    }
    Some(H265Sps {
        id,
        max_sub_layers_minus1,
        temporal_id_nesting,
        ptl,
        chroma_format_idc,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        width: width as u32,
        height: height as u32,
    })
}

/// VPS の vps_video_parameter_set_id。
pub(crate) fn parse_h265_vps_id(nal: &[u8]) -> Option<u32> {
    nal.get(2).map(|b| (b >> 4) as u32)
}

/// PPS の pps_pic_parameter_set_id。
pub(crate) fn parse_h265_pps_id(nal: &[u8]) -> Option<u32> {
    if nal.len() < 3 {
        return None;
    }
    let rbsp = unescape_rbsp(&nal[2..nal.len().min(16)]);
    let id = BitReader::new(&rbsp).read_ue()?;
    (id <= 63).then_some(id)
}

/// HEVCDecoderConfigurationRecord を組み立てる（lengthSizeMinusOne = 3、配列は完全）。
pub(crate) fn build_hvcc(vps: &[&[u8]], sps: &[&[u8]], pps: &[&[u8]], info: &H265Sps) -> Option<Vec<u8>> {
    if vps.is_empty() || sps.is_empty() || pps.is_empty() {
        return None;
    }
    let mut v = Vec::with_capacity(64);
    v.push(1);
    v.extend_from_slice(&info.ptl);
    v.extend_from_slice(&[0xF0, 0x00]); // min_spatial_segmentation_idc = 0
    v.push(0xFC); // parallelismType = 0
    v.push(0xFC | (info.chroma_format_idc as u8 & 3));
    v.push(0xF8 | (info.bit_depth_luma_minus8 as u8 & 7));
    v.push(0xF8 | (info.bit_depth_chroma_minus8 as u8 & 7));
    v.extend_from_slice(&[0, 0]); // avgFrameRate
    v.push(((info.max_sub_layers_minus1 + 1) << 3) | ((info.temporal_id_nesting as u8) << 2) | 3);
    v.push(3);
    for (ty, list) in [(h265_nal::VPS, vps), (h265_nal::SPS, sps), (h265_nal::PPS, pps)] {
        v.push(0x80 | ty);
        v.extend_from_slice(&u16::try_from(list.len()).ok()?.to_be_bytes());
        for n in list {
            v.extend_from_slice(&u16::try_from(n.len()).ok()?.to_be_bytes());
            v.extend_from_slice(n);
        }
    }
    Some(v)
}

/// HEVCDecoderConfigurationRecord から VPS/SPS/PPS（と配列にある他の NAL）を取り出す。
pub(crate) fn parse_hvcc(rec: &[u8]) -> Option<Vec<&[u8]>> {
    if rec.len() < 23 || rec[0] != 1 {
        return None;
    }
    let narrays = rec[22] as usize;
    let mut p = 23usize;
    let mut out = Vec::new();
    for _ in 0..narrays {
        let ty = *rec.get(p)? & 0x3F;
        let cnt = u16::from_be_bytes([*rec.get(p + 1)?, *rec.get(p + 2)?]) as usize;
        p += 3;
        for _ in 0..cnt {
            let n = u16::from_be_bytes([*rec.get(p)?, *rec.get(p + 1)?]) as usize;
            let nal = rec.get(p + 2..p + 2 + n)?;
            if matches!(ty, h265_nal::VPS | h265_nal::SPS | h265_nal::PPS) {
                out.push(nal);
            }
            p += 2 + n;
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// AAC

pub(crate) const AAC_SAMPLE_RATES: [u32; 13] =
    [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdtsHeader {
    /// audio object type（profile + 1）。
    pub object_type: u8,
    pub sf_index: u8,
    pub channel_config: u8,
    pub frame_len: usize,
    pub header_len: usize,
    pub raw_blocks: u8,
}

/// ADTS ヘッダーを読む（7 バイト以上必要）。
pub(crate) fn parse_adts(b: &[u8]) -> Option<AdtsHeader> {
    if b.len() < 7 || b[0] != 0xFF || b[1] & 0xF6 != 0xF0 {
        return None;
    }
    let protection_absent = b[1] & 1 == 1;
    let profile = b[2] >> 6;
    let sf_index = (b[2] >> 2) & 0xF;
    let channel_config = ((b[2] & 1) << 2) | (b[3] >> 6);
    let frame_len = (((b[3] & 3) as usize) << 11) | ((b[4] as usize) << 3) | ((b[5] >> 5) as usize);
    let raw_blocks = b[6] & 3;
    let header_len = if protection_absent { 7 } else { 9 };
    if sf_index as usize >= AAC_SAMPLE_RATES.len() || frame_len < header_len {
        return None;
    }
    Some(AdtsHeader { object_type: profile + 1, sf_index, channel_config, frame_len, header_len, raw_blocks })
}

/// ADTS ヘッダー（CRC なし）を書く。
pub(crate) fn write_adts(out: &mut Vec<u8>, object_type: u8, sf_index: u8, channel_config: u8, payload_len: usize) {
    let len = payload_len + 7;
    let profile = object_type - 1;
    out.extend_from_slice(&[
        0xFF,
        0xF1,
        (profile << 6) | (sf_index << 2) | (channel_config >> 2),
        ((channel_config & 3) << 6) | ((len >> 11) as u8 & 3),
        (len >> 3) as u8,
        ((len as u8 & 7) << 5) | 0x1F,
        0xFC,
    ]);
}

/// AudioSpecificConfig から ADTS に要る値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Asc {
    /// ADTS に載せる object type（SBR/PS の明示指定は基本の型に戻す）。
    pub object_type: u8,
    pub sf_index: u8,
    pub sample_rate: u32,
    pub channel_config: u8,
}

pub(crate) fn parse_asc(b: &[u8]) -> Option<Asc> {
    let mut r = BitReader::new(b);
    let read_aot = |r: &mut BitReader<'_>| -> Option<u8> {
        let a = r.read_u8(5)?;
        if a == 31 { Some(32 + r.read_u8(6)?) } else { Some(a) }
    };
    let read_sf = |r: &mut BitReader<'_>| -> Option<(u8, u32)> {
        let i = r.read_u8(4)?;
        if i == 15 {
            let f = r.read_u32(24)?;
            let idx = AAC_SAMPLE_RATES.iter().position(|&x| x == f).map(|x| x as u8).unwrap_or(15);
            Some((idx, f))
        } else {
            Some((i, *AAC_SAMPLE_RATES.get(i as usize)?))
        }
    };
    let mut aot = read_aot(&mut r)?;
    let (sf_index, sample_rate) = read_sf(&mut r)?;
    let channel_config = r.read_u8(4)?;
    if aot == 5 || aot == 29 {
        read_sf(&mut r)?;
        aot = read_aot(&mut r)?;
    }
    Some(Asc { object_type: aot, sf_index, sample_rate, channel_config })
}

pub(crate) fn build_asc(object_type: u8, sf_index: u8, channel_config: u8) -> [u8; 2] {
    [(object_type << 3) | (sf_index >> 1), ((sf_index & 1) << 7) | (channel_config << 3)]
}

// ---------------------------------------------------------------------------
// Opus

/// Opus パケットの長さ（48 kHz のサンプル数）。RFC 6716 3.1。
pub(crate) fn opus_packet_samples(pkt: &[u8]) -> Option<u32> {
    let toc = *pkt.first()?;
    let config = toc >> 3;
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][(config % 4) as usize],
        12..=15 => [480, 960][(config % 2) as usize],
        _ => [120, 240, 480, 960][(config % 4) as usize],
    };
    let count = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => (*pkt.get(1)? & 0x3F) as u32,
    };
    let total = frame * count;
    (count > 0 && total <= 5760).then_some(total)
}

/// OpusHead（RFC 7845 5.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpusHead {
    pub channels: u8,
    pub pre_skip: u16,
    pub input_rate: u32,
    pub gain: i16,
    pub family: u8,
    pub streams: u8,
    pub coupled: u8,
    pub mapping: Vec<u8>,
}

pub(crate) fn parse_opus_head(b: &[u8]) -> Option<OpusHead> {
    if b.len() < 19 || &b[0..8] != b"OpusHead" || b[8] >> 4 != 0 {
        return None;
    }
    let channels = b[9];
    if channels == 0 {
        return None;
    }
    let pre_skip = u16::from_le_bytes([b[10], b[11]]);
    let input_rate = u32::from_le_bytes([b[12], b[13], b[14], b[15]]);
    let gain = i16::from_le_bytes([b[16], b[17]]);
    let family = b[18];
    let (streams, coupled, mapping) = if family == 0 {
        if channels > 2 {
            return None;
        }
        (1, channels - 1, Vec::new())
    } else {
        let s = *b.get(19)?;
        let c = *b.get(20)?;
        let m = b.get(21..21 + channels as usize)?.to_vec();
        (s, c, m)
    };
    Some(OpusHead { channels, pre_skip, input_rate, gain, family, streams, coupled, mapping })
}

pub(crate) fn build_opus_head(h: &OpusHead) -> Vec<u8> {
    let mut v = Vec::with_capacity(21 + h.mapping.len());
    v.extend_from_slice(b"OpusHead");
    v.push(1);
    v.push(h.channels);
    v.extend_from_slice(&h.pre_skip.to_le_bytes());
    v.extend_from_slice(&h.input_rate.to_le_bytes());
    v.extend_from_slice(&h.gain.to_le_bytes());
    v.push(h.family);
    if h.family != 0 {
        v.push(h.streams);
        v.push(h.coupled);
        v.extend_from_slice(&h.mapping);
    }
    v
}

/// Vorbis 順の既定の割り当て（channel_config_code 1〜8 用）。
pub(crate) fn opus_default_head(channels: u8) -> Option<OpusHead> {
    let (family, streams, coupled, mapping): (u8, u8, u8, &[u8]) = match channels {
        1 => (0, 1, 0, &[]),
        2 => (0, 1, 1, &[]),
        3 => (1, 2, 1, &[0, 2, 1]),
        4 => (1, 2, 2, &[0, 1, 2, 3]),
        5 => (1, 3, 2, &[0, 4, 1, 2, 3]),
        6 => (1, 4, 2, &[0, 4, 1, 2, 3, 5]),
        7 => (1, 4, 3, &[0, 4, 1, 2, 3, 5, 6]),
        8 => (1, 5, 3, &[0, 6, 1, 2, 3, 4, 5, 7]),
        _ => return None,
    };
    Some(OpusHead {
        channels,
        pre_skip: 0,
        input_rate: 48000,
        gain: 0,
        family,
        streams,
        coupled,
        mapping: mapping.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annexb_split() {
        let d = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 9, 0];
        let n = annexb_nals(&d);
        assert_eq!(n, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9][..]]);
        assert!(annexb_nals(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn avcc_split_rejects_overrun() {
        assert_eq!(avcc_nals(&[0, 0, 0, 2, 1, 2]).unwrap(), vec![&[1, 2][..]]);
        assert!(avcc_nals(&[0, 0, 0, 3, 1, 2]).is_none());
        assert!(avcc_nals(&[0, 0, 0]).is_none());
        assert!(avcc_nals(&[0xFF, 0xFF, 0xFF, 0xFF, 1]).is_none());
    }

    #[test]
    fn rbsp() {
        assert_eq!(unescape_rbsp(&[0, 0, 3, 1, 0, 0, 3]), vec![0, 0, 1, 0, 0]);
    }

    // x264 が出した 320x240 High の SPS
    const SPS_320X240: [u8; 25] = [
        0x67, 0x64, 0x00, 0x0d, 0xac, 0xd9, 0x41, 0x41, 0xfb, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00,
        0x00, 0x03, 0x03, 0xc0, 0xf1, 0x42, 0x99, 0x60,
    ];

    #[test]
    fn h264_sps() {
        let s = parse_h264_sps(&SPS_320X240).unwrap();
        assert_eq!((s.width, s.height, s.profile_idc, s.level_idc), (320, 240, 100, 13));
        let rec = build_avcc(&[&SPS_320X240], &[&[0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0]], &s).unwrap();
        let back = parse_avcc(&rec).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], &SPS_320X240[..]);
        // 切り詰めた SPS はパニックせず None
        for i in 0..SPS_320X240.len() {
            let _ = parse_h264_sps(&SPS_320X240[..i]);
        }
    }

    #[test]
    fn adts_roundtrip() {
        let mut v = Vec::new();
        write_adts(&mut v, 2, 3, 2, 100);
        let h = parse_adts(&v).unwrap();
        assert_eq!((h.object_type, h.sf_index, h.channel_config, h.frame_len, h.header_len), (2, 3, 2, 107, 7));
        let asc = build_asc(2, 3, 2);
        assert_eq!(asc, [0x11, 0x90]);
        let a = parse_asc(&asc).unwrap();
        assert_eq!((a.object_type, a.sample_rate, a.channel_config), (2, 48000, 2));
        // HE-AAC（明示 SBR）は基本の型へ
        let he = parse_asc(&[0x2B, 0x92, 0x08, 0x00]).unwrap();
        assert_eq!(he.object_type, 2);
    }

    #[test]
    fn opus_toc() {
        assert_eq!(opus_packet_samples(&[0xFC]), Some(960)); // CELT FB 20ms, 1 frame
        assert_eq!(opus_packet_samples(&[0xFD]), Some(1920));
        assert_eq!(opus_packet_samples(&[0xFF, 0x03]), Some(2880));
        assert_eq!(opus_packet_samples(&[0xFF, 0x00]), None);
        assert_eq!(opus_packet_samples(&[0xFF]), None);
        let h = opus_default_head(6).unwrap();
        assert_eq!(parse_opus_head(&build_opus_head(&h)).unwrap(), h);
    }
}
