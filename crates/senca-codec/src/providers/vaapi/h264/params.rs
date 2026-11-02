//! H.264 の SPS・PPS の解析（7.3.2.1・7.3.2.2）。復号の準備として VA-API に渡す値をすべて取り出す。
//!
//! 倍率表（スケーリングリスト）は、ビットストリームの順（ジグザグ順）のまま持つ（VA-API の `VAIQMatrixBufferH264` と同じ順）。

use crate::bitstream::nal::unescape_rbsp;
use crate::error::{CodecError, Result};
use senca_core::bits::BitReader;

pub const FLAT_4X4: [u8; 16] = [16; 16];
pub const FLAT_8X8: [u8; 64] = [16; 64];
/// Table 7-3（ジグザグ順）。
pub const DEFAULT_4X4_INTRA: [u8; 16] = [
    6, 13, 13, 20, 20, 20, 28, 28, 28, 28, 32, 32, 32, 37, 37, 42,
];
pub const DEFAULT_4X4_INTER: [u8; 16] = [
    10, 14, 14, 20, 20, 20, 24, 24, 24, 24, 27, 27, 27, 30, 30, 34,
];
/// Table 7-4（ジグザグ順）。
pub const DEFAULT_8X8_INTRA: [u8; 64] = [
    6, 10, 10, 13, 11, 13, 16, 16, 16, 16, 18, 18, 18, 18, 18, 23, 23, 23, 23, 23, 23, 25, 25, 25,
    25, 25, 25, 25, 27, 27, 27, 27, 27, 27, 27, 27, 29, 29, 29, 29, 29, 29, 29, 31, 31, 31, 31, 31,
    31, 33, 33, 33, 33, 33, 36, 36, 36, 36, 38, 38, 38, 40, 40, 42,
];
pub const DEFAULT_8X8_INTER: [u8; 64] = [
    9, 13, 13, 15, 13, 15, 17, 17, 17, 17, 19, 19, 19, 19, 19, 21, 21, 21, 21, 21, 21, 22, 22, 22,
    22, 22, 22, 22, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27, 27, 27,
    27, 28, 28, 28, 28, 28, 30, 30, 30, 30, 32, 32, 32, 33, 33, 35,
];

/// 倍率表一式（4x4 が 6 つ、8x8 が 6 つ。4:2:0 では 8x8 は先頭 2 つだけ使う）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalingLists {
    pub l4x4: [[u8; 16]; 6],
    pub l8x8: [[u8; 64]; 6],
}

impl ScalingLists {
    pub fn flat() -> Self {
        ScalingLists {
            l4x4: [FLAT_4X4; 6],
            l8x8: [FLAT_8X8; 6],
        }
    }
}

fn bad(what: &str) -> CodecError {
    CodecError::invalid_input(format!("malformed H.264 {what}"))
}

fn unsupported(what: &str) -> CodecError {
    CodecError::unsupported(format!(
        "H.264 {what} is not supported by the VA-API decoder"
    ))
}

/// scaling_list()。戻り値の bool は useDefaultScalingMatrixFlag。
fn read_scaling_list(r: &mut BitReader, out: &mut [u8]) -> Option<bool> {
    let mut last = 8i32;
    let mut next = 8i32;
    let mut use_default = false;
    for (j, v) in out.iter_mut().enumerate() {
        if next != 0 {
            let delta = r.read_se()?;
            if !(-128..=127).contains(&delta) {
                return None;
            }
            next = (last + delta + 256) % 256;
            use_default = j == 0 && next == 0;
        }
        *v = if next == 0 { last as u8 } else { next as u8 };
        last = *v as i32;
    }
    Some(use_default)
}

/// 倍率表を読み、規則 A（SPS）または B（PPS、`fallback` は SPS の表）で補う。
fn read_scaling_matrix(
    r: &mut BitReader,
    n8x8: usize,
    fallback: Option<&ScalingLists>,
) -> Option<ScalingLists> {
    let mut m = ScalingLists::flat();
    for i in 0..6 + n8x8 {
        let present = r.read_bit()?;
        if i < 6 {
            if present {
                let mut l = [0u8; 16];
                let def = read_scaling_list(r, &mut l)?;
                m.l4x4[i] = if def {
                    if i < 3 {
                        DEFAULT_4X4_INTRA
                    } else {
                        DEFAULT_4X4_INTER
                    }
                } else {
                    l
                };
            } else {
                m.l4x4[i] = match (i, fallback) {
                    (0, None) => DEFAULT_4X4_INTRA,
                    (3, None) => DEFAULT_4X4_INTER,
                    (0 | 3, Some(f)) => f.l4x4[i],
                    _ => m.l4x4[i - 1],
                };
            }
        } else {
            let k = i - 6;
            if present {
                let mut l = [0u8; 64];
                let def = read_scaling_list(r, &mut l)?;
                m.l8x8[k] = if def {
                    if k % 2 == 0 {
                        DEFAULT_8X8_INTRA
                    } else {
                        DEFAULT_8X8_INTER
                    }
                } else {
                    l
                };
            } else {
                m.l8x8[k] = match (k, fallback) {
                    (0, None) => DEFAULT_8X8_INTRA,
                    (1, None) => DEFAULT_8X8_INTER,
                    (0 | 1, Some(f)) => f.l8x8[k],
                    _ => m.l8x8[k - 2],
                };
            }
        }
    }
    Some(m)
}

/// SPS。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub sps_id: u32,
    pub chroma_format_idc: u32,
    pub separate_colour_plane: bool,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub qpprime_y_zero_transform_bypass: bool,
    pub seq_scaling_matrix_present: bool,
    pub scaling: ScalingLists,
    pub log2_max_frame_num: u32,
    pub poc_type: u32,
    pub log2_max_poc_lsb: u32,
    pub delta_pic_order_always_zero: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: Vec<i32>,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_allowed: bool,
    pub pic_width_in_mbs: u32,
    pub pic_height_in_map_units: u32,
    pub frame_mbs_only: bool,
    pub mb_adaptive_frame_field: bool,
    pub direct_8x8_inference: bool,
    /// 切り抜き（輝度の画素単位）: 左・右・上・下。
    pub crop: [u32; 4],
    pub max_num_reorder_frames: Option<u32>,
    pub max_dec_frame_buffering: Option<u32>,
}

impl Sps {
    pub fn max_frame_num(&self) -> u32 {
        1 << self.log2_max_frame_num
    }

    pub fn frame_height_in_mbs(&self) -> u32 {
        self.pic_height_in_map_units * if self.frame_mbs_only { 1 } else { 2 }
    }

    pub fn coded_width(&self) -> u32 {
        self.pic_width_in_mbs * 16
    }

    pub fn coded_height(&self) -> u32 {
        self.frame_height_in_mbs() * 16
    }

    /// 切り抜いた表示サイズ。
    pub fn display_size(&self) -> (u32, u32) {
        (
            self.coded_width() - self.crop[0] - self.crop[1],
            self.coded_height() - self.crop[2] - self.crop[3],
        )
    }

    /// ChromaArrayType。
    pub fn chroma_array_type(&self) -> u32 {
        if self.separate_colour_plane {
            0
        } else {
            self.chroma_format_idc
        }
    }

    /// DPB の大きさ（フレーム数、A.3.1 の MaxDpbMbs と VUI の max_dec_frame_buffering から）。
    pub fn dpb_size(&self) -> usize {
        let max_dpb_mbs: u32 = match (self.level_idc, self.constraint_flags & 0x10 != 0) {
            (9, _) | (10, _) | (11, true) => 396,
            (11, false) => 900,
            (12, _) | (13, _) | (20, _) => 2376,
            (21, _) => 4752,
            (22, _) | (30, _) => 8100,
            (31, _) => 18000,
            (32, _) => 20480,
            (40, _) | (41, _) => 32768,
            (42, _) => 34816,
            (50, _) => 110400,
            (51, _) | (52, _) => 184320,
            _ => 696320,
        };
        let mbs = (self.pic_width_in_mbs * self.frame_height_in_mbs()).max(1);
        let mut n = (max_dpb_mbs / mbs).clamp(1, 16) as usize;
        if let Some(m) = self.max_dec_frame_buffering {
            n = (m as usize).clamp(1, 16);
        }
        n.max(self.max_num_ref_frames as usize).clamp(1, 16)
    }

    /// 表示順の入れ替えに要るフレーム数（出力を待たせる枚数）。
    pub fn reorder_depth(&self) -> usize {
        if let Some(n) = self.max_num_reorder_frames {
            return (n as usize).min(self.dpb_size());
        }
        // Baseline・Constrained Baseline 系（B が無い）と intra 専用は入れ替えが無い。
        let constrained_intra = self.constraint_flags & 0x10 != 0;
        if self.profile_idc == 66
            || self.profile_idc == 83
            || (constrained_intra && matches!(self.profile_idc, 44 | 100 | 110 | 122 | 244))
        {
            return 0;
        }
        self.dpb_size()
    }
}

fn skip_hrd(r: &mut BitReader) -> Option<()> {
    let cnt = r.read_ue()?;
    if cnt > 31 {
        return None;
    }
    r.skip(8)?;
    for _ in 0..=cnt {
        r.read_ue()?;
        r.read_ue()?;
        r.skip(1)?;
    }
    r.skip(20)
}

/// VUI を読み、bitstream_restriction の (max_num_reorder_frames, max_dec_frame_buffering) を返す。
fn parse_vui(r: &mut BitReader) -> Option<(Option<u32>, Option<u32>)> {
    if r.read_bit()? && r.read_u8(8)? == 255 {
        r.skip(32)?;
    }
    if r.read_bit()? {
        r.skip(1)?;
    }
    if r.read_bit()? {
        r.skip(4)?;
        if r.read_bit()? {
            r.skip(24)?;
        }
    }
    if r.read_bit()? {
        r.read_ue()?;
        r.read_ue()?;
    }
    if r.read_bit()? {
        r.skip(65)?;
    }
    let nal_hrd = r.read_bit()?;
    if nal_hrd {
        skip_hrd(r)?;
    }
    let vcl_hrd = r.read_bit()?;
    if vcl_hrd {
        skip_hrd(r)?;
    }
    if nal_hrd || vcl_hrd {
        r.skip(1)?;
    }
    r.skip(1)?; // pic_struct_present_flag
    if r.read_bit()? {
        r.skip(1)?;
        for _ in 0..4 {
            r.read_ue()?;
        }
        let reorder = r.read_ue()?;
        let dec_buf = r.read_ue()?;
        if reorder > 16 || dec_buf > 16 {
            return None;
        }
        return Some((Some(reorder), Some(dec_buf)));
    }
    Some((None, None))
}

/// SPS（NAL ヘッダーを含む）を解析する。
pub fn parse_sps(nal: &[u8]) -> Result<Sps> {
    if nal.len() < 4 || nal[0] & 0x1f != 7 {
        return Err(bad("SPS"));
    }
    let rbsp = unescape_rbsp(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let r = &mut r;
    let mut s = (|| -> Option<Sps> {
        let profile_idc = r.read_u8(8)?;
        let constraint_flags = r.read_u8(8)?;
        let level_idc = r.read_u8(8)?;
        let sps_id = r.read_ue()?;
        if sps_id > 31 {
            return None;
        }
        let mut s = Sps {
            profile_idc,
            constraint_flags,
            level_idc,
            sps_id,
            chroma_format_idc: 1,
            separate_colour_plane: false,
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
            qpprime_y_zero_transform_bypass: false,
            seq_scaling_matrix_present: false,
            scaling: ScalingLists::flat(),
            log2_max_frame_num: 4,
            poc_type: 0,
            log2_max_poc_lsb: 4,
            delta_pic_order_always_zero: false,
            offset_for_non_ref_pic: 0,
            offset_for_top_to_bottom_field: 0,
            offset_for_ref_frame: Vec::new(),
            max_num_ref_frames: 0,
            gaps_in_frame_num_allowed: false,
            pic_width_in_mbs: 0,
            pic_height_in_map_units: 0,
            frame_mbs_only: true,
            mb_adaptive_frame_field: false,
            direct_8x8_inference: false,
            crop: [0; 4],
            max_num_reorder_frames: None,
            max_dec_frame_buffering: None,
        };
        if matches!(
            profile_idc,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        ) {
            s.chroma_format_idc = r.read_ue()?;
            if s.chroma_format_idc > 3 {
                return None;
            }
            if s.chroma_format_idc == 3 {
                s.separate_colour_plane = r.read_bit()?;
            }
            let l = r.read_ue()?;
            let c = r.read_ue()?;
            if l > 6 || c > 6 {
                return None;
            }
            s.bit_depth_luma = 8 + l as u8;
            s.bit_depth_chroma = 8 + c as u8;
            s.qpprime_y_zero_transform_bypass = r.read_bit()?;
            s.seq_scaling_matrix_present = r.read_bit()?;
            if s.seq_scaling_matrix_present {
                let n8 = if s.chroma_format_idc == 3 { 6 } else { 2 };
                s.scaling = read_scaling_matrix(r, n8, None)?;
            }
        }
        let l = r.read_ue()?;
        if l > 12 {
            return None;
        }
        s.log2_max_frame_num = l + 4;
        s.poc_type = r.read_ue()?;
        match s.poc_type {
            0 => {
                let l = r.read_ue()?;
                if l > 12 {
                    return None;
                }
                s.log2_max_poc_lsb = l + 4;
            }
            1 => {
                s.delta_pic_order_always_zero = r.read_bit()?;
                s.offset_for_non_ref_pic = r.read_se()?;
                s.offset_for_top_to_bottom_field = r.read_se()?;
                let n = r.read_ue()?;
                if n > 255 {
                    return None;
                }
                for _ in 0..n {
                    s.offset_for_ref_frame.push(r.read_se()?);
                }
            }
            2 => {}
            _ => return None,
        }
        s.max_num_ref_frames = r.read_ue()?;
        if s.max_num_ref_frames > 16 {
            return None;
        }
        s.gaps_in_frame_num_allowed = r.read_bit()?;
        s.pic_width_in_mbs = r.read_ue()?.checked_add(1)?;
        s.pic_height_in_map_units = r.read_ue()?.checked_add(1)?;
        if s.pic_width_in_mbs > 1024 || s.pic_height_in_map_units > 1024 {
            return None;
        }
        s.frame_mbs_only = r.read_bit()?;
        if !s.frame_mbs_only {
            s.mb_adaptive_frame_field = r.read_bit()?;
        }
        s.direct_8x8_inference = r.read_bit()?;
        if r.read_bit()? {
            let (cx, cy) = match s.chroma_array_type() {
                0 => (1, 1),
                1 => (2, 2),
                2 => (2, 1),
                _ => (1, 1),
            };
            let cy = cy * if s.frame_mbs_only { 1 } else { 2 };
            let mut c = [0u32; 4];
            for v in &mut c {
                *v = r.read_ue()?;
                if *v > 8192 {
                    return None;
                }
            }
            s.crop = [c[0] * cx, c[1] * cx, c[2] * cy, c[3] * cy];
            if s.crop[0] + s.crop[1] >= s.coded_width() || s.crop[2] + s.crop[3] >= s.coded_height()
            {
                return None;
            }
        }
        if r.read_bit()? {
            // VUI が壊れていても SPS の本体は使えるので、読めなければ制約なしとして扱う。
            let mut rr = r.clone();
            if let Some((reorder, dec)) = parse_vui(&mut rr) {
                s.max_num_reorder_frames = reorder;
                s.max_dec_frame_buffering = dec;
            }
        }
        Some(s)
    })()
    .ok_or_else(|| bad("SPS"))?;
    if let Some(d) = s.max_dec_frame_buffering
        && d < s.max_num_ref_frames
    {
        s.max_dec_frame_buffering = Some(s.max_num_ref_frames);
    }
    Ok(s)
}

/// PPS。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pps {
    pub pps_id: u32,
    pub sps_id: u32,
    pub entropy_coding_mode: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_slice_groups_minus1: u32,
    pub num_ref_idx_l0_default_active_minus1: u32,
    pub num_ref_idx_l1_default_active_minus1: u32,
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp_minus26: i32,
    pub pic_init_qs_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present: bool,
    pub constrained_intra_pred: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8_mode: bool,
    pub pic_scaling_matrix_present: bool,
    /// 実際に使う倍率表（PPS に無ければ SPS のもの）。
    pub scaling: ScalingLists,
    pub second_chroma_qp_index_offset: i32,
}

/// PPS（NAL ヘッダーを含む）を解析する。参照する SPS が要る（倍率表の補い方とチャンネル数のため）。
pub fn parse_pps(nal: &[u8], sps_of: impl Fn(u32) -> Option<Sps>) -> Result<Pps> {
    if nal.len() < 2 || nal[0] & 0x1f != 8 {
        return Err(bad("PPS"));
    }
    let rbsp = unescape_rbsp(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let r = &mut r;
    let pps_id = r
        .read_ue()
        .filter(|v| *v <= 255)
        .ok_or_else(|| bad("PPS"))?;
    let sps_id = r.read_ue().filter(|v| *v <= 31).ok_or_else(|| bad("PPS"))?;
    let sps = sps_of(sps_id)
        .ok_or_else(|| CodecError::invalid_input("H.264 PPS refers to an unknown SPS"))?;
    let mut p = (|| -> Option<Pps> {
        let entropy_coding_mode = r.read_bit()?;
        let bottom_field_pic_order_in_frame_present = r.read_bit()?;
        let num_slice_groups_minus1 = r.read_ue()?;
        if num_slice_groups_minus1 > 7 {
            return None;
        }
        Some(Pps {
            pps_id,
            sps_id,
            entropy_coding_mode,
            bottom_field_pic_order_in_frame_present,
            num_slice_groups_minus1,
            num_ref_idx_l0_default_active_minus1: 0,
            num_ref_idx_l1_default_active_minus1: 0,
            weighted_pred: false,
            weighted_bipred_idc: 0,
            pic_init_qp_minus26: 0,
            pic_init_qs_minus26: 0,
            chroma_qp_index_offset: 0,
            deblocking_filter_control_present: false,
            constrained_intra_pred: false,
            redundant_pic_cnt_present: false,
            transform_8x8_mode: false,
            pic_scaling_matrix_present: false,
            scaling: sps.scaling.clone(),
            second_chroma_qp_index_offset: 0,
        })
    })()
    .ok_or_else(|| bad("PPS"))?;
    if p.num_slice_groups_minus1 > 0 {
        // FMO（Baseline の任意機能）。VA-API のドライバーも多くは対応しない。
        return Err(unsupported("FMO (slice groups)"));
    }
    (|| -> Option<()> {
        p.num_ref_idx_l0_default_active_minus1 = r.read_ue().filter(|v| *v <= 31)?;
        p.num_ref_idx_l1_default_active_minus1 = r.read_ue().filter(|v| *v <= 31)?;
        p.weighted_pred = r.read_bit()?;
        p.weighted_bipred_idc = r.read_u32(2)?;
        if p.weighted_bipred_idc > 2 {
            return None;
        }
        p.pic_init_qp_minus26 = r.read_se().filter(|v| (-26..=25).contains(v))?;
        p.pic_init_qs_minus26 = r.read_se().filter(|v| (-26..=25).contains(v))?;
        p.chroma_qp_index_offset = r.read_se().filter(|v| (-12..=12).contains(v))?;
        p.deblocking_filter_control_present = r.read_bit()?;
        p.constrained_intra_pred = r.read_bit()?;
        p.redundant_pic_cnt_present = r.read_bit()?;
        p.second_chroma_qp_index_offset = p.chroma_qp_index_offset;
        if r.more_rbsp_data() {
            p.transform_8x8_mode = r.read_bit()?;
            p.pic_scaling_matrix_present = r.read_bit()?;
            if p.pic_scaling_matrix_present {
                let n8 = if p.transform_8x8_mode {
                    if sps.chroma_format_idc == 3 { 6 } else { 2 }
                } else {
                    0
                };
                let fallback = sps.scaling.clone();
                let fallback = if sps.seq_scaling_matrix_present {
                    fallback
                } else {
                    ScalingLists::flat()
                };
                let mut m = read_scaling_matrix(r, n8, Some(&fallback))?;
                if n8 == 0 {
                    m.l8x8 = fallback.l8x8;
                }
                p.scaling = m;
            }
            p.second_chroma_qp_index_offset = r.read_se().filter(|v| (-12..=12).contains(v))?;
        }
        Some(())
    })()
    .ok_or_else(|| bad("PPS"))?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_list_default_flag() {
        // delta = -8 → next = 0 at j=0 → useDefault
        let mut w = senca_core::bits::BitWriter::new();
        w.write_se(-8);
        let d = w.finish();
        let mut r = BitReader::new(&d);
        let mut l = [0u8; 16];
        assert_eq!(read_scaling_list(&mut r, &mut l), Some(true));
    }

    #[test]
    fn scaling_list_values() {
        // 1 つ目 +2 → 10、以後 0 → 繰り返し
        let mut w = senca_core::bits::BitWriter::new();
        w.write_se(2);
        for _ in 1..16 {
            w.write_se(0);
        }
        let d = w.finish();
        let mut r = BitReader::new(&d);
        let mut l = [0u8; 16];
        assert_eq!(read_scaling_list(&mut r, &mut l), Some(false));
        assert_eq!(l, [10; 16]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_sps(&[0x67, 0x64]).is_err());
        assert!(parse_sps(&[0x68, 0x64, 0, 0, 0]).is_err());
        assert!(parse_pps(&[0x68, 0x80], |_| None).is_err());
    }
}
