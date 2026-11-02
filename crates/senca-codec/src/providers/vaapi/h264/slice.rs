//! H.264 のスライスヘッダーの解析（7.3.3）。フレーム（プログレッシブ）だけを扱う。

use super::params::{Pps, Sps};
use crate::bitstream::nal::unescape_rbsp;
use crate::error::{CodecError, Result};
use senca_core::bits::BitReader;

pub const SLICE_P: u8 = 0;
pub const SLICE_B: u8 = 1;
pub const SLICE_I: u8 = 2;

/// 参照リストの並べ替え 1 つ（modification_of_pic_nums_idc と値）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefListMod {
    pub idc: u32,
    pub value: u32,
}

/// 参照ピクチャーの印付けの操作（MMCO）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mmco {
    /// 1: 短期を「参照しない」に。
    UnrefShort { difference_of_pic_nums_minus1: u32 },
    /// 2: 長期を「参照しない」に。
    UnrefLong { long_term_pic_num: u32 },
    /// 3: 短期を長期に。
    ShortToLong {
        difference_of_pic_nums_minus1: u32,
        long_term_frame_idx: u32,
    },
    /// 4: 長期の番号の上限。
    MaxLongTermIdx { max_long_term_frame_idx_plus1: u32 },
    /// 5: すべて「参照しない」に。
    UnrefAll,
    /// 6: 現在のピクチャーを長期に。
    CurrentToLong { long_term_frame_idx: u32 },
}

/// 重み付き予測の表（明示されなかった項目は既定値で埋める）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PredWeightTable {
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    /// リスト 0・1 ごとに「明示があったか」。
    pub luma_weight_flag: [bool; 2],
    pub chroma_weight_flag: [bool; 2],
    pub luma_weight: [[i16; 32]; 2],
    pub luma_offset: [[i16; 32]; 2],
    pub chroma_weight: [[[i16; 2]; 32]; 2],
    pub chroma_offset: [[[i16; 2]; 32]; 2],
}

/// スライスヘッダー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHeader {
    pub nal_ref_idc: u8,
    pub nal_unit_type: u8,
    pub first_mb_in_slice: u32,
    /// 0=P, 1=B, 2=I（5 の剰余）。
    pub slice_type: u8,
    pub pps_id: u32,
    pub frame_num: u32,
    pub idr_pic_id: u32,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub redundant_pic_cnt: u32,
    pub direct_spatial_mv_pred: bool,
    pub num_ref_idx_l0_active_minus1: u32,
    pub num_ref_idx_l1_active_minus1: u32,
    pub ref_list_mods: [Vec<RefListMod>; 2],
    pub pred_weight: Option<PredWeightTable>,
    pub no_output_of_prior_pics: bool,
    pub long_term_reference_flag: bool,
    /// adaptive_ref_pic_marking_mode_flag が 1 のときの操作列。`None` はスライディングウィンドウ。
    pub mmco: Option<Vec<Mmco>>,
    pub cabac_init_idc: u32,
    pub slice_qp_delta: i32,
    pub disable_deblocking_filter_idc: u32,
    pub slice_alpha_c0_offset_div2: i32,
    pub slice_beta_offset_div2: i32,
    /// NAL の先頭（NAL ヘッダーの 1 バイトを含む）からスライスデータまでのビット数。
    /// エミュレーション防止バイトを除いた（RBSP の）数え方（VA-API の `slice_data_bit_offset`）。
    pub header_bits: u32,
}

impl SliceHeader {
    pub fn is_idr(&self) -> bool {
        self.nal_unit_type == 5
    }
}

fn bad() -> CodecError {
    CodecError::invalid_input("malformed H.264 slice header")
}

fn read_pred_weight(r: &mut BitReader, sps: &Sps, h: &SliceHeader) -> Option<PredWeightTable> {
    let mut t = PredWeightTable {
        luma_log2_weight_denom: r.read_ue().filter(|v| *v <= 7)? as u8,
        ..Default::default()
    };
    let chroma = sps.chroma_array_type() != 0;
    if chroma {
        t.chroma_log2_weight_denom = r.read_ue().filter(|v| *v <= 7)? as u8;
    }
    let lists = if h.slice_type == SLICE_B { 2 } else { 1 };
    for l in 0..lists {
        let n = if l == 0 {
            h.num_ref_idx_l0_active_minus1
        } else {
            h.num_ref_idx_l1_active_minus1
        } as usize
            + 1;
        for i in 0..n {
            t.luma_weight[l][i] = 1 << t.luma_log2_weight_denom;
            if r.read_bit()? {
                t.luma_weight_flag[l] = true;
                t.luma_weight[l][i] = r.read_se().filter(|v| (-128..=127).contains(v))? as i16;
                t.luma_offset[l][i] = r.read_se().filter(|v| (-128..=127).contains(v))? as i16;
            }
            if chroma {
                for j in 0..2 {
                    t.chroma_weight[l][i][j] = 1 << t.chroma_log2_weight_denom;
                }
                if r.read_bit()? {
                    t.chroma_weight_flag[l] = true;
                    for j in 0..2 {
                        t.chroma_weight[l][i][j] =
                            r.read_se().filter(|v| (-128..=127).contains(v))? as i16;
                        t.chroma_offset[l][i][j] =
                            r.read_se().filter(|v| (-128..=127).contains(v))? as i16;
                    }
                }
            }
        }
    }
    Some(t)
}

fn read_mods(r: &mut BitReader) -> Option<Vec<RefListMod>> {
    let mut v = Vec::new();
    if !r.read_bit()? {
        return Some(v);
    }
    loop {
        let idc = r.read_ue()?;
        match idc {
            3 => return Some(v),
            0..=2 => {
                let value = r.read_ue()?;
                v.push(RefListMod { idc, value });
                if v.len() > 33 {
                    return None;
                }
            }
            _ => return None,
        }
    }
}

/// スライス（NAL ヘッダーを含む）のスライスヘッダーを解析する。
/// `pps_of`・`sps_of` は ID からパラメーターセットを引く。
pub fn parse_slice_header<'a>(
    nal: &[u8],
    pps_of: impl Fn(u32) -> Option<&'a Pps>,
    sps_of: impl Fn(u32) -> Option<&'a Sps>,
) -> Result<SliceHeader> {
    let nal_unit_type = nal.first().map(|b| b & 0x1f).ok_or_else(bad)?;
    if !matches!(nal_unit_type, 1 | 5) {
        return Err(bad());
    }
    let nal_ref_idc = (nal[0] >> 5) & 3;
    // ヘッダーだけ読めればよいので先頭の一部だけ戻す（巨大なスライスでも複製は小さい）。
    let head = &nal[1..nal.len().min(1 + 4096)];
    let rbsp = unescape_rbsp(head);
    let mut r = BitReader::new(&rbsp);
    let r = &mut r;
    let first_mb_in_slice = r.read_ue().ok_or_else(bad)?;
    let slice_type_raw = r.read_ue().filter(|v| *v <= 9).ok_or_else(bad)?;
    let slice_type = (slice_type_raw % 5) as u8;
    if slice_type > SLICE_I {
        return Err(CodecError::unsupported(
            "H.264 SP/SI slices are not supported",
        ));
    }
    if nal_unit_type == 5 && slice_type != SLICE_I {
        return Err(bad());
    }
    let pps_id = r.read_ue().filter(|v| *v <= 255).ok_or_else(bad)?;
    let pps = pps_of(pps_id)
        .ok_or_else(|| CodecError::invalid_input("H.264 slice refers to an unknown PPS"))?;
    let sps = sps_of(pps.sps_id)
        .ok_or_else(|| CodecError::invalid_input("H.264 slice refers to an unknown SPS"))?;
    if !sps.frame_mbs_only {
        return Err(CodecError::unsupported(
            "interlaced H.264 (field/MBAFF) is not supported by the VA-API decoder",
        ));
    }
    let mut h = SliceHeader {
        nal_ref_idc,
        nal_unit_type,
        first_mb_in_slice,
        slice_type,
        pps_id,
        frame_num: 0,
        idr_pic_id: 0,
        pic_order_cnt_lsb: 0,
        delta_pic_order_cnt_bottom: 0,
        delta_pic_order_cnt: [0; 2],
        redundant_pic_cnt: 0,
        direct_spatial_mv_pred: false,
        num_ref_idx_l0_active_minus1: pps.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_active_minus1: pps.num_ref_idx_l1_default_active_minus1,
        ref_list_mods: [Vec::new(), Vec::new()],
        pred_weight: None,
        no_output_of_prior_pics: false,
        long_term_reference_flag: false,
        mmco: None,
        cabac_init_idc: 0,
        slice_qp_delta: 0,
        disable_deblocking_filter_idc: 0,
        slice_alpha_c0_offset_div2: 0,
        slice_beta_offset_div2: 0,
        header_bits: 0,
    };
    (|| -> Option<()> {
        if sps.separate_colour_plane {
            r.skip(2)?;
        }
        h.frame_num = r.read_u32(sps.log2_max_frame_num)?;
        // frame_mbs_only なので field_pic_flag は無い。
        if nal_unit_type == 5 {
            h.idr_pic_id = r.read_ue().filter(|v| *v <= 65535)?;
        }
        if sps.poc_type == 0 {
            h.pic_order_cnt_lsb = r.read_u32(sps.log2_max_poc_lsb)?;
            if pps.bottom_field_pic_order_in_frame_present {
                h.delta_pic_order_cnt_bottom = r.read_se()?;
            }
        }
        if sps.poc_type == 1 && !sps.delta_pic_order_always_zero {
            h.delta_pic_order_cnt[0] = r.read_se()?;
            if pps.bottom_field_pic_order_in_frame_present {
                h.delta_pic_order_cnt[1] = r.read_se()?;
            }
        }
        if pps.redundant_pic_cnt_present {
            h.redundant_pic_cnt = r.read_ue().filter(|v| *v <= 127)?;
        }
        if slice_type == SLICE_B {
            h.direct_spatial_mv_pred = r.read_bit()?;
        }
        if slice_type == SLICE_P || slice_type == SLICE_B {
            if r.read_bit()? {
                h.num_ref_idx_l0_active_minus1 = r.read_ue().filter(|v| *v <= 31)?;
                if slice_type == SLICE_B {
                    h.num_ref_idx_l1_active_minus1 = r.read_ue().filter(|v| *v <= 31)?;
                }
            }
            // フレームでは 0..=15 まで。
            if h.num_ref_idx_l0_active_minus1 > 15 || h.num_ref_idx_l1_active_minus1 > 15 {
                return None;
            }
        }
        if slice_type != SLICE_I {
            h.ref_list_mods[0] = read_mods(r)?;
        }
        if slice_type == SLICE_B {
            h.ref_list_mods[1] = read_mods(r)?;
        }
        if (pps.weighted_pred && slice_type == SLICE_P)
            || (pps.weighted_bipred_idc == 1 && slice_type == SLICE_B)
        {
            h.pred_weight = Some(read_pred_weight(r, sps, &h)?);
        }
        if nal_ref_idc != 0 {
            if nal_unit_type == 5 {
                h.no_output_of_prior_pics = r.read_bit()?;
                h.long_term_reference_flag = r.read_bit()?;
            } else if r.read_bit()? {
                let mut ops = Vec::new();
                loop {
                    let op = r.read_ue()?;
                    let m = match op {
                        0 => break,
                        1 => Mmco::UnrefShort {
                            difference_of_pic_nums_minus1: r.read_ue()?,
                        },
                        2 => Mmco::UnrefLong {
                            long_term_pic_num: r.read_ue()?,
                        },
                        3 => Mmco::ShortToLong {
                            difference_of_pic_nums_minus1: r.read_ue()?,
                            long_term_frame_idx: r.read_ue().filter(|v| *v <= 15)?,
                        },
                        4 => Mmco::MaxLongTermIdx {
                            max_long_term_frame_idx_plus1: r.read_ue().filter(|v| *v <= 16)?,
                        },
                        5 => Mmco::UnrefAll,
                        6 => Mmco::CurrentToLong {
                            long_term_frame_idx: r.read_ue().filter(|v| *v <= 15)?,
                        },
                        _ => return None,
                    };
                    ops.push(m);
                    if ops.len() > 66 {
                        return None;
                    }
                }
                h.mmco = Some(ops);
            }
        }
        if pps.entropy_coding_mode && slice_type != SLICE_I {
            h.cabac_init_idc = r.read_ue().filter(|v| *v <= 2)?;
        }
        h.slice_qp_delta = r.read_se().filter(|v| (-87..=87).contains(v))?;
        if pps.deblocking_filter_control_present {
            h.disable_deblocking_filter_idc = r.read_ue().filter(|v| *v <= 2)?;
            if h.disable_deblocking_filter_idc != 1 {
                h.slice_alpha_c0_offset_div2 = r.read_se().filter(|v| (-6..=6).contains(v))?;
                h.slice_beta_offset_div2 = r.read_se().filter(|v| (-6..=6).contains(v))?;
            }
        }
        h.header_bits = 8 + u32::try_from(r.position()).ok()?;
        Some(())
    })()
    .ok_or_else(bad)?;
    let total_mbs = sps.pic_width_in_mbs * sps.frame_height_in_mbs();
    if h.first_mb_in_slice >= total_mbs {
        return Err(bad());
    }
    Ok(h)
}
