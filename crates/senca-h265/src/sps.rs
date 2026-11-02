//! VPS・SPS・PPS（H.265 7.3.2）とスライスセグメントヘッダーの先頭の解析。

use crate::bits::Bits;
use crate::error::{Error, Result};
use crate::nal::{NalHeader, NalUnitType, remove_emulation_prevention};
use crate::ptl::ProfileTierLevel;

/// 幅・高さの上限（画素）。
const MAX_DIM: u32 = 32768;

/// 窓（conformance_window・default_display_window）。単位は SubWidthC/SubHeightC。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Window {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

/// 色の記述。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColourDescription {
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
}

/// video_signal_type。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSignal {
    pub video_format: u8,
    pub full_range: bool,
    pub colour: Option<ColourDescription>,
}

/// vui_timing_info。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub num_ticks_poc_diff_one: Option<u32>,
}

/// VUI（附属書 E）のうち配信で使う部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Vui {
    pub sample_aspect_ratio: Option<(u16, u16)>,
    pub video_signal: Option<VideoSignal>,
    pub field_seq: bool,
    pub default_display_window: Option<Window>,
    pub timing: Option<Timing>,
    /// bitstream_restriction の min_spatial_segmentation_idc（無ければ 0）。
    pub min_spatial_segmentation_idc: u16,
}

/// VPS の先頭部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vps {
    pub vps_video_parameter_set_id: u8,
    pub max_layers: u8,
    pub max_sub_layers: u8,
    pub temporal_id_nesting: bool,
    pub ptl: ProfileTierLevel,
}

/// SPS の解析結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    pub sps_video_parameter_set_id: u8,
    pub max_sub_layers: u8,
    pub temporal_id_nesting: bool,
    pub ptl: ProfileTierLevel,
    pub sps_seq_parameter_set_id: u8,
    pub chroma_format_idc: u8,
    pub separate_colour_plane: bool,
    pub pic_width_in_luma_samples: u32,
    pub pic_height_in_luma_samples: u32,
    pub conformance_window: Window,
    /// conformance window を適用した幅（画素）。
    pub width: u32,
    /// conformance window を適用した高さ（画素）。
    pub height: u32,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub log2_max_pic_order_cnt_lsb: u8,
    /// 最上位サブレイヤーの sps_max_dec_pic_buffering_minus1 + 1。
    pub max_dec_pic_buffering: u32,
    /// 最上位サブレイヤーの sps_max_num_reorder_pics。
    pub max_num_reorder_pics: u32,
    pub log2_min_luma_coding_block_size: u8,
    pub log2_ctb_size: u8,
    pub num_short_term_ref_pic_sets: u8,
    pub long_term_ref_pics_present: bool,
    pub temporal_mvp_enabled: bool,
    pub strong_intra_smoothing_enabled: bool,
    /// VUI。無い、または壊れているときは `None`。
    pub vui: Option<Vui>,
}

const SAR_TABLE: [(u16, u16); 16] = [
    (1, 1),
    (12, 11),
    (10, 11),
    (16, 11),
    (40, 33),
    (24, 11),
    (20, 11),
    (32, 11),
    (80, 33),
    (18, 11),
    (15, 11),
    (64, 33),
    (160, 99),
    (4, 3),
    (3, 2),
    (2, 1),
];

fn rbsp_of(nal: &[u8], ty: NalUnitType) -> Result<std::borrow::Cow<'_, [u8]>> {
    let h = NalHeader::parse(nal)?;
    if h.nal_unit_type != ty {
        return Err(Error::Invalid("NAL の種類が違う"));
    }
    if h.nuh_layer_id != 0 {
        return Err(Error::Unsupported(
            "nuh_layer_id が 0 でないパラメーターセット",
        ));
    }
    Ok(remove_emulation_prevention(&nal[2..]))
}

fn skip_scaling_list_data(r: &mut Bits<'_>) -> Result<()> {
    for size_id in 0..4u32 {
        let step = if size_id == 3 { 3 } else { 1 };
        let mut matrix_id = 0;
        while matrix_id < 6 {
            if !r.flag()? {
                r.ue_max(matrix_id / step, "scaling_list_pred_matrix_id_delta")?;
            } else {
                let coef_num = 64.min(1u32 << (4 + (size_id << 1)));
                if size_id > 1 {
                    let dc = r.se()?;
                    if !(-7..=247).contains(&dc) {
                        return Err(Error::Invalid("scaling_list_dc_coef_minus8"));
                    }
                }
                for _ in 0..coef_num {
                    let d = r.se()?;
                    if !(-128..=127).contains(&d) {
                        return Err(Error::Invalid("scaling_list_delta_coef"));
                    }
                }
            }
            matrix_id += step;
        }
    }
    Ok(())
}

/// st_ref_pic_set(idx) を読み飛ばし、NumDeltaPocs[idx] を返す。
fn skip_st_ref_pic_set(r: &mut Bits<'_>, idx: usize, num_delta_pocs: &[u32]) -> Result<u32> {
    let inter = idx != 0 && r.flag()?;
    if inter {
        // SPS の中では delta_idx_minus1 は無く、参照先は直前の集合
        r.skip(1)?; // delta_rps_sign
        r.ue_max(32767, "abs_delta_rps_minus1")?;
        let ref_n = num_delta_pocs[idx - 1];
        let mut n = 0;
        for _ in 0..=ref_n {
            let used = r.flag()?;
            let use_delta = if used { true } else { r.flag()? };
            if use_delta {
                n += 1;
            }
        }
        if n > 32 {
            return Err(Error::Invalid("NumDeltaPocs"));
        }
        Ok(n)
    } else {
        let neg = r.ue_max(16, "num_negative_pics")?;
        let pos = r.ue_max(16, "num_positive_pics")?;
        for _ in 0..neg + pos {
            r.ue_max(32767, "delta_poc_minus1")?;
            r.skip(1)?;
        }
        Ok(neg + pos)
    }
}

fn skip_sub_layer_hrd(r: &mut Bits<'_>, cpb_cnt: u32, sub_pic: bool) -> Result<()> {
    for _ in 0..cpb_cnt {
        r.ue()?;
        r.ue()?;
        if sub_pic {
            r.ue()?;
            r.ue()?;
        }
        r.skip(1)?;
    }
    Ok(())
}

fn skip_hrd(r: &mut Bits<'_>, max_sub_layers_minus1: u8) -> Result<()> {
    let nal = r.flag()?;
    let vcl = r.flag()?;
    let mut sub_pic = false;
    if nal || vcl {
        sub_pic = r.flag()?;
        if sub_pic {
            r.skip(8 + 5 + 1 + 5)?;
        }
        r.skip(8)?; // bit_rate_scale, cpb_size_scale
        if sub_pic {
            r.skip(4)?;
        }
        r.skip(15)?;
    }
    for _ in 0..=max_sub_layers_minus1 {
        let fixed_general = r.flag()?;
        let fixed_within_cvs = if fixed_general { true } else { r.flag()? };
        let mut low_delay = false;
        if fixed_within_cvs {
            r.ue_max(2047, "elemental_duration_in_tc_minus1")?;
        } else {
            low_delay = r.flag()?;
        }
        let cpb_cnt = if low_delay {
            1
        } else {
            r.ue_max(31, "cpb_cnt_minus1")? + 1
        };
        if nal {
            skip_sub_layer_hrd(r, cpb_cnt, sub_pic)?;
        }
        if vcl {
            skip_sub_layer_hrd(r, cpb_cnt, sub_pic)?;
        }
    }
    Ok(())
}

fn parse_vui(r: &mut Bits<'_>, max_sub_layers_minus1: u8) -> Result<Vui> {
    let mut vui = Vui::default();
    if r.flag()? {
        let idc = r.u8(8)?;
        vui.sample_aspect_ratio = match idc {
            1..=16 => Some(SAR_TABLE[idc as usize - 1]),
            255 => {
                let w = r.u(16)? as u16;
                let h = r.u(16)? as u16;
                (w != 0 && h != 0).then_some((w, h))
            }
            _ => None,
        };
    }
    if r.flag()? {
        r.skip(1)?;
    }
    if r.flag()? {
        let video_format = r.u8(3)?;
        let full_range = r.flag()?;
        let colour = if r.flag()? {
            Some(ColourDescription {
                colour_primaries: r.u8(8)?,
                transfer_characteristics: r.u8(8)?,
                matrix_coefficients: r.u8(8)?,
            })
        } else {
            None
        };
        vui.video_signal = Some(VideoSignal {
            video_format,
            full_range,
            colour,
        });
    }
    if r.flag()? {
        r.ue_max(5, "chroma_sample_loc_type_top_field")?;
        r.ue_max(5, "chroma_sample_loc_type_bottom_field")?;
    }
    r.skip(1)?; // neutral_chroma_indication_flag
    vui.field_seq = r.flag()?;
    r.skip(1)?; // frame_field_info_present_flag
    if r.flag()? {
        vui.default_display_window = Some(Window {
            left: r.ue()?,
            right: r.ue()?,
            top: r.ue()?,
            bottom: r.ue()?,
        });
    }
    if r.flag()? {
        let num_units_in_tick = r.u(32)?;
        let time_scale = r.u(32)?;
        let num_ticks_poc_diff_one = if r.flag()? {
            Some(r.ue_max(u32::MAX - 1, "num_ticks_poc_diff_one_minus1")? + 1)
        } else {
            None
        };
        vui.timing = Some(Timing {
            num_units_in_tick,
            time_scale,
            num_ticks_poc_diff_one,
        });
        if r.flag()? {
            skip_hrd(r, max_sub_layers_minus1)?;
        }
    }
    if r.flag()? {
        r.skip(3)?; // tiles_fixed_structure, motion_vectors_over_pic_boundaries, restricted_ref_pic_lists
        vui.min_spatial_segmentation_idc = r.ue_max(4095, "min_spatial_segmentation_idc")? as u16;
    }
    Ok(vui)
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

impl Vps {
    /// VPS の NAL（ヘッダー付き）を解析する。
    pub fn parse(nal: &[u8]) -> Result<Vps> {
        let rbsp = rbsp_of(nal, NalUnitType::Vps)?;
        let mut r = Bits::new(&rbsp);
        let vps_video_parameter_set_id = r.u8(4)?;
        r.skip(2)?; // vps_base_layer_internal_flag, vps_base_layer_available_flag
        let max_layers = r.u8(6)? + 1;
        let max_sub_layers_minus1 = r.u8(3)?;
        if max_sub_layers_minus1 > 6 {
            return Err(Error::Invalid("vps_max_sub_layers_minus1"));
        }
        let temporal_id_nesting = r.flag()?;
        r.skip(16)?; // vps_reserved_0xffff_16bits
        let ptl = ProfileTierLevel::parse(&mut r, max_sub_layers_minus1)?;
        Ok(Vps {
            vps_video_parameter_set_id,
            max_layers,
            max_sub_layers: max_sub_layers_minus1 + 1,
            temporal_id_nesting,
            ptl,
        })
    }
}

impl Sps {
    /// SPS の NAL（ヘッダー付き、emulation prevention 込み）を解析する。nuh_layer_id が 0 のものだけ。
    pub fn parse(nal: &[u8]) -> Result<Sps> {
        Self::parse_rbsp(&rbsp_of(nal, NalUnitType::Sps)?)
    }

    /// NAL ヘッダーを除き emulation prevention を取り除いた RBSP を解析する。
    pub fn parse_rbsp(rbsp: &[u8]) -> Result<Sps> {
        let mut r = Bits::new(rbsp);
        let sps_video_parameter_set_id = r.u8(4)?;
        let max_sub_layers_minus1 = r.u8(3)?;
        if max_sub_layers_minus1 > 6 {
            return Err(Error::Invalid("sps_max_sub_layers_minus1"));
        }
        let temporal_id_nesting = r.flag()?;
        let ptl = ProfileTierLevel::parse(&mut r, max_sub_layers_minus1)?;
        let sps_seq_parameter_set_id = r.ue_max(15, "sps_seq_parameter_set_id")? as u8;
        let chroma_format_idc = r.ue_max(3, "chroma_format_idc")? as u8;
        let separate_colour_plane = chroma_format_idc == 3 && r.flag()?;
        let pic_width_in_luma_samples = r.ue_max(MAX_DIM, "pic_width_in_luma_samples")?;
        let pic_height_in_luma_samples = r.ue_max(MAX_DIM, "pic_height_in_luma_samples")?;
        if pic_width_in_luma_samples == 0 || pic_height_in_luma_samples == 0 {
            return Err(Error::Invalid("幅・高さが 0"));
        }
        let mut conformance_window = Window::default();
        if r.flag()? {
            conformance_window = Window {
                left: r.ue()?,
                right: r.ue()?,
                top: r.ue()?,
                bottom: r.ue()?,
            };
        }
        let bit_depth_luma = r.ue_max(8, "bit_depth_luma_minus8")? as u8 + 8;
        let bit_depth_chroma = r.ue_max(8, "bit_depth_chroma_minus8")? as u8 + 8;
        let log2_max_pic_order_cnt_lsb =
            r.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")? as u8 + 4;
        let sub_layer_ordering_info_present = r.flag()?;
        let first = if sub_layer_ordering_info_present {
            0
        } else {
            max_sub_layers_minus1
        };
        let mut max_dec_pic_buffering = 0;
        let mut max_num_reorder_pics = 0;
        for _ in first..=max_sub_layers_minus1 {
            max_dec_pic_buffering = r.ue_max(15, "sps_max_dec_pic_buffering_minus1")? + 1;
            max_num_reorder_pics = r.ue_max(15, "sps_max_num_reorder_pics")?;
            r.ue()?; // sps_max_latency_increase_plus1
        }
        let log2_min_cb = r.ue_max(3, "log2_min_luma_coding_block_size_minus3")? as u8 + 3;
        let log2_ctb_size =
            log2_min_cb + r.ue_max(3, "log2_diff_max_min_luma_coding_block_size")? as u8;
        if log2_ctb_size > 6 {
            return Err(Error::Invalid("CTB の大きさ"));
        }
        let min_cb = 1u32 << log2_min_cb;
        if pic_width_in_luma_samples % min_cb != 0 || pic_height_in_luma_samples % min_cb != 0 {
            return Err(Error::Invalid("幅・高さが MinCbSizeY の倍数でない"));
        }
        r.ue_max(3, "log2_min_luma_transform_block_size_minus2")?;
        r.ue_max(3, "log2_diff_max_min_luma_transform_block_size")?;
        r.ue_max(4, "max_transform_hierarchy_depth_inter")?;
        r.ue_max(4, "max_transform_hierarchy_depth_intra")?;
        if r.flag()? && r.flag()? {
            // scaling_list_enabled_flag && sps_scaling_list_data_present_flag
            skip_scaling_list_data(&mut r)?;
        }
        r.skip(2)?; // amp_enabled_flag, sample_adaptive_offset_enabled_flag
        if r.flag()? {
            // pcm_enabled_flag
            r.skip(8)?;
            r.ue_max(2, "log2_min_pcm_luma_coding_block_size_minus3")?;
            r.ue_max(2, "log2_diff_max_min_pcm_luma_coding_block_size")?;
            r.skip(1)?;
        }
        let num_short_term_ref_pic_sets = r.ue_max(64, "num_short_term_ref_pic_sets")? as usize;
        let mut num_delta_pocs = [0u32; 64];
        for i in 0..num_short_term_ref_pic_sets {
            num_delta_pocs[i] = skip_st_ref_pic_set(&mut r, i, &num_delta_pocs)?;
        }
        let long_term_ref_pics_present = r.flag()?;
        if long_term_ref_pics_present {
            let n = r.ue_max(32, "num_long_term_ref_pics_sps")?;
            for _ in 0..n {
                r.skip(log2_max_pic_order_cnt_lsb as usize + 1)?;
            }
        }
        let temporal_mvp_enabled = r.flag()?;
        let strong_intra_smoothing_enabled = r.flag()?;
        let vui = if r.flag()? {
            parse_vui(&mut r, max_sub_layers_minus1).ok()
        } else {
            None
        };

        let chroma_array_type = if separate_colour_plane {
            0
        } else {
            chroma_format_idc
        };
        let (sub_w, sub_h) = match chroma_array_type {
            1 => (2u64, 2u64),
            2 => (2, 1),
            _ => (1, 1),
        };
        let cw = &conformance_window;
        let crop_w = sub_w * (cw.left as u64 + cw.right as u64);
        let crop_h = sub_h * (cw.top as u64 + cw.bottom as u64);
        if crop_w >= pic_width_in_luma_samples as u64 || crop_h >= pic_height_in_luma_samples as u64
        {
            return Err(Error::Invalid("conformance window が画面より大きい"));
        }
        let width = pic_width_in_luma_samples - crop_w as u32;
        let height = pic_height_in_luma_samples - crop_h as u32;

        Ok(Sps {
            sps_video_parameter_set_id,
            max_sub_layers: max_sub_layers_minus1 + 1,
            temporal_id_nesting,
            ptl,
            sps_seq_parameter_set_id,
            chroma_format_idc,
            separate_colour_plane,
            pic_width_in_luma_samples,
            pic_height_in_luma_samples,
            conformance_window,
            width,
            height,
            bit_depth_luma,
            bit_depth_chroma,
            log2_max_pic_order_cnt_lsb,
            max_dec_pic_buffering,
            max_num_reorder_pics,
            log2_min_luma_coding_block_size: log2_min_cb,
            log2_ctb_size,
            num_short_term_ref_pic_sets: num_short_term_ref_pic_sets as u8,
            long_term_ref_pics_present,
            temporal_mvp_enabled,
            strong_intra_smoothing_enabled,
            vui,
        })
    }

    /// フレームレート（分子, 分母）。VUI の timing_info があるときだけ（`time_scale / num_units_in_tick` を約分）。
    pub fn frame_rate(&self) -> Option<(u32, u32)> {
        let t = self.vui.as_ref()?.timing?;
        if t.num_units_in_tick == 0 || t.time_scale == 0 {
            return None;
        }
        let g = gcd(t.time_scale as u64, t.num_units_in_tick as u64);
        Some((
            (t.time_scale as u64 / g) as u32,
            (t.num_units_in_tick as u64 / g) as u32,
        ))
    }

    /// 表示順の並べ替え（B フレームなど）がありうるか。
    pub fn may_reorder(&self) -> bool {
        self.max_num_reorder_pics > 0
    }

    /// codecs 文字列（`hvc1.…`）。
    pub fn codecs_string(&self) -> String {
        self.ptl.codecs_string()
    }
}

/// PPS の先頭部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pps {
    pub pps_pic_parameter_set_id: u8,
    pub pps_seq_parameter_set_id: u8,
    pub dependent_slice_segments_enabled: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u8,
}

impl Pps {
    /// PPS の NAL（ヘッダー付き）を解析する。
    pub fn parse(nal: &[u8]) -> Result<Pps> {
        let h = NalHeader::parse(nal)?;
        if h.nal_unit_type != NalUnitType::Pps {
            return Err(Error::Invalid("PPS ではない NAL"));
        }
        let head = &nal[2..nal.len().min(34)];
        let rbsp = remove_emulation_prevention(head);
        let mut r = Bits::new(&rbsp);
        Ok(Pps {
            pps_pic_parameter_set_id: r.ue_max(63, "pps_pic_parameter_set_id")? as u8,
            pps_seq_parameter_set_id: r.ue_max(15, "pps_seq_parameter_set_id")? as u8,
            dependent_slice_segments_enabled: r.flag()?,
            output_flag_present: r.flag()?,
            num_extra_slice_header_bits: r.u8(3)?,
        })
    }
}

/// スライスセグメントヘッダーの先頭（PPS なしで読める部分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceSegmentHeaderStart {
    pub nal_unit_type: NalUnitType,
    /// ピクチャの最初のスライスセグメントか（アクセスユニットの境界の判定に使う）。
    pub first_slice_segment_in_pic: bool,
    pub no_output_of_prior_pics: Option<bool>,
    pub slice_pic_parameter_set_id: u8,
}

impl SliceSegmentHeaderStart {
    /// VCL NAL（ヘッダー付き）の先頭を読む。
    pub fn parse(nal: &[u8]) -> Result<Self> {
        let h = NalHeader::parse(nal)?;
        if !h.nal_unit_type.is_vcl() {
            return Err(Error::Invalid("VCL ではない NAL"));
        }
        let head = &nal[2..nal.len().min(16)];
        let rbsp = remove_emulation_prevention(head);
        let mut r = Bits::new(&rbsp);
        let first_slice_segment_in_pic = r.flag()?;
        let no_output_of_prior_pics = if h.nal_unit_type.is_irap() {
            Some(r.flag()?)
        } else {
            None
        };
        let slice_pic_parameter_set_id = r.ue_max(63, "slice_pic_parameter_set_id")? as u8;
        Ok(SliceSegmentHeaderStart {
            nal_unit_type: h.nal_unit_type,
            first_slice_segment_in_pic,
            no_output_of_prior_pics,
            slice_pic_parameter_set_id,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::nal::add_emulation_prevention;
    use senca_core::bits::BitWriter;

    pub(crate) struct SpsSpec {
        pub profile_idc: u8,
        pub compat: u32,
        pub level: u8,
        pub tier: bool,
        pub sub_layers_minus1: u8,
        pub chroma: u32,
        pub width: u32,
        pub height: u32,
        pub window: Option<(u32, u32, u32, u32)>,
        pub bit_depth_minus8: u32,
        pub reorder: u32,
        pub scaling: bool,
        pub st_rps: bool,
        pub timing: Option<(u32, u32)>,
        pub hrd: bool,
    }

    impl Default for SpsSpec {
        fn default() -> Self {
            SpsSpec {
                profile_idc: 1,
                compat: 0x6000_0000,
                level: 93,
                tier: false,
                sub_layers_minus1: 0,
                chroma: 1,
                width: 1920,
                height: 1088,
                window: Some((0, 0, 0, 4)),
                bit_depth_minus8: 0,
                reorder: 2,
                scaling: false,
                st_rps: false,
                timing: None,
                hrd: false,
            }
        }
    }

    pub(crate) fn write_ptl(w: &mut BitWriter, s: &SpsSpec) {
        w.write_bits(0, 2);
        w.write_bit(s.tier);
        w.write_bits(s.profile_idc as u64, 5);
        w.write_bits(s.compat as u64, 32);
        w.write_bits(0x9000_0000_0000, 48);
        w.write_bits(s.level as u64, 8);
        let n = s.sub_layers_minus1 as usize;
        for i in 0..n {
            w.write_bit(i == 0);
            w.write_bit(true);
        }
        if n > 0 {
            w.write_bits(0, 2 * (8 - n) as u32);
        }
        for i in 0..n {
            if i == 0 {
                w.write_bits(0x1234, 64);
                w.write_bits(0, 24);
            }
            w.write_bits(90, 8);
        }
    }

    pub(crate) fn build_vps(s: &SpsSpec) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_bits(0, 4);
        w.write_bits(3, 2);
        w.write_bits(0, 6);
        w.write_bits(s.sub_layers_minus1 as u64, 3);
        w.write_bit(true);
        w.write_bits(0xFFFF, 16);
        write_ptl(&mut w, s);
        w.write_bit(true);
        let mut v = vec![0x40, 0x01];
        v.extend(add_emulation_prevention(&w.finish()));
        v
    }

    pub(crate) fn build_sps(s: &SpsSpec) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_bits(0, 4);
        w.write_bits(s.sub_layers_minus1 as u64, 3);
        w.write_bit(true);
        write_ptl(&mut w, s);
        w.write_ue(0);
        w.write_ue(s.chroma);
        if s.chroma == 3 {
            w.write_bit(false);
        }
        w.write_ue(s.width);
        w.write_ue(s.height);
        match s.window {
            Some((l, r, t, b)) => {
                w.write_bit(true);
                for v in [l, r, t, b] {
                    w.write_ue(v);
                }
            }
            None => w.write_bit(false),
        }
        w.write_ue(s.bit_depth_minus8);
        w.write_ue(s.bit_depth_minus8);
        w.write_ue(4); // log2_max_pic_order_cnt_lsb_minus4
        w.write_bit(true); // sub_layer_ordering_info_present
        for _ in 0..=s.sub_layers_minus1 {
            w.write_ue(4);
            w.write_ue(s.reorder);
            w.write_ue(0);
        }
        w.write_ue(0); // log2_min_cb - 3
        w.write_ue(3); // diff → CTB 64
        w.write_ue(0);
        w.write_ue(3);
        w.write_ue(1);
        w.write_ue(1);
        w.write_bit(s.scaling);
        if s.scaling {
            w.write_bit(true);
            for size_id in 0..4 {
                let step = if size_id == 3 { 3 } else { 1 };
                let mut m = 0;
                while m < 6 {
                    if m % 2 == 0 {
                        w.write_bit(true);
                        if size_id > 1 {
                            w.write_se(8);
                        }
                        let n = 64.min(1 << (4 + (size_id << 1)));
                        for _ in 0..n {
                            w.write_se(-1);
                        }
                    } else {
                        w.write_bit(false);
                        w.write_ue(1);
                    }
                    m += step;
                }
            }
        }
        w.write_bit(true); // amp
        w.write_bit(true); // sao
        w.write_bit(true); // pcm
        w.write_bits(0x77, 8);
        w.write_ue(0);
        w.write_ue(1);
        w.write_bit(false);
        if s.st_rps {
            w.write_ue(3);
            // 0: 明示（負 2、正 1）
            w.write_ue(2);
            w.write_ue(1);
            for _ in 0..3 {
                w.write_ue(0);
                w.write_bit(true);
            }
            // 1: 予測（参照の NumDeltaPocs = 3 → 4 組）、used/use_delta の組で 3 個
            w.write_bit(true);
            w.write_bit(false);
            w.write_ue(0);
            w.write_bit(true);
            w.write_bit(false);
            w.write_bit(true);
            w.write_bit(false);
            w.write_bit(false);
            w.write_bit(true);
            // 2: 予測（参照の NumDeltaPocs = 3）
            w.write_bit(true);
            w.write_bit(true);
            w.write_ue(1);
            for _ in 0..4 {
                w.write_bit(true);
            }
        } else {
            w.write_ue(0);
        }
        w.write_bit(true); // long_term_ref_pics_present
        w.write_ue(1);
        w.write_bits(5, 8);
        w.write_bit(true);
        w.write_bit(true); // temporal_mvp
        w.write_bit(true); // strong_intra
        w.write_bit(true); // vui
        {
            w.write_bit(true);
            w.write_bits(1, 8);
            w.write_bit(false);
            w.write_bit(true);
            w.write_bits(5, 3);
            w.write_bit(false);
            w.write_bit(true);
            w.write_bits(9, 8);
            w.write_bits(16, 8);
            w.write_bits(9, 8);
            w.write_bit(false);
            w.write_bit(false);
            w.write_bit(false);
            w.write_bit(false);
            w.write_bit(false); // default_display_window
            match s.timing {
                Some((n, t)) => {
                    w.write_bit(true);
                    w.write_bits(n as u64, 32);
                    w.write_bits(t as u64, 32);
                    w.write_bit(true);
                    w.write_ue(0);
                    w.write_bit(s.hrd);
                    if s.hrd {
                        w.write_bit(true); // nal
                        w.write_bit(false); // vcl
                        w.write_bit(false); // sub_pic
                        w.write_bits(0, 8);
                        w.write_bits(0, 15);
                        for _ in 0..=s.sub_layers_minus1 {
                            w.write_bit(false);
                            w.write_bit(false);
                            w.write_bit(false); // low_delay
                            w.write_ue(1);
                            for _ in 0..2 {
                                w.write_ue(100);
                                w.write_ue(200);
                                w.write_bit(false);
                            }
                        }
                    }
                }
                None => w.write_bit(false),
            }
            w.write_bit(true); // bitstream_restriction
            w.write_bits(0, 3);
            w.write_ue(12);
            w.write_ue(0);
            w.write_ue(0);
            w.write_ue(15);
            w.write_ue(15);
        }
        w.write_bit(false); // sps_extension
        w.write_bit(true);
        let mut v = vec![0x42, 0x01];
        v.extend(add_emulation_prevention(&w.finish()));
        v
    }

    #[test]
    fn sps_1080p() {
        let s = Sps::parse(&build_sps(&SpsSpec {
            timing: Some((1001, 60000)),
            ..Default::default()
        }))
        .unwrap();
        assert_eq!((s.width, s.height), (1920, 1080));
        assert_eq!(s.ptl.general_profile_idc, 1);
        assert_eq!(s.ptl.general_level_idc, 93);
        assert_eq!(s.codecs_string(), "hvc1.1.6.L93.90");
        assert_eq!(s.frame_rate(), Some((60000, 1001)));
        assert!(s.may_reorder());
        assert_eq!(s.log2_ctb_size, 6);
        let vui = s.vui.unwrap();
        assert_eq!(vui.min_spatial_segmentation_idc, 12);
        assert_eq!(
            vui.video_signal
                .unwrap()
                .colour
                .unwrap()
                .transfer_characteristics,
            16
        );
        assert_eq!(vui.sample_aspect_ratio, Some((1, 1)));
    }

    #[test]
    fn sps_variants() {
        let s = Sps::parse(&build_sps(&SpsSpec {
            profile_idc: 4,
            chroma: 3,
            bit_depth_minus8: 4,
            width: 1280,
            height: 720,
            window: Some((1, 1, 1, 1)),
            sub_layers_minus1: 2,
            scaling: true,
            st_rps: true,
            timing: Some((1, 50)),
            hrd: true,
            tier: true,
            reorder: 0,
            ..Default::default()
        }))
        .unwrap();
        assert_eq!((s.width, s.height), (1278, 718));
        assert_eq!(s.bit_depth_luma, 12);
        assert_eq!(s.max_sub_layers, 3);
        assert_eq!(s.num_short_term_ref_pic_sets, 3);
        assert_eq!(s.frame_rate(), Some((50, 1)));
        assert!(!s.may_reorder());
        assert!(s.ptl.general_tier_flag);
        assert_eq!(s.vui.unwrap().min_spatial_segmentation_idc, 12);

        let s = Sps::parse(&build_sps(&SpsSpec {
            chroma: 2,
            width: 720,
            height: 480,
            window: Some((1, 1, 1, 1)),
            ..Default::default()
        }))
        .unwrap();
        assert_eq!((s.width, s.height), (716, 478));
    }

    #[test]
    fn vps_and_pps() {
        let spec = SpsSpec {
            sub_layers_minus1: 1,
            ..Default::default()
        };
        let v = Vps::parse(&build_vps(&spec)).unwrap();
        assert_eq!(v.max_sub_layers, 2);
        assert_eq!(v.ptl.general_level_idc, 93);
        assert!(Vps::parse(&build_sps(&spec)).is_err());
        // PPS: id 2、sps 0、dependent 1、output 0、extra bits 2
        let mut w = BitWriter::new();
        w.write_ue(2);
        w.write_ue(0);
        w.write_bit(true);
        w.write_bit(false);
        w.write_bits(2, 3);
        w.write_bit(true);
        let mut nal = vec![0x44, 0x01];
        nal.extend(w.finish());
        let p = Pps::parse(&nal).unwrap();
        assert_eq!(p.pps_pic_parameter_set_id, 2);
        assert!(p.dependent_slice_segments_enabled);
        assert_eq!(p.num_extra_slice_header_bits, 2);
        assert_eq!(Pps::parse(&[0x44, 0x01]), Err(Error::Truncated));
    }

    #[test]
    fn slice_start() {
        // IDR_W_RADL、first=1、no_output=0、pps=0
        let s = SliceSegmentHeaderStart::parse(&[0x26, 0x01, 0b1010_0000]).unwrap();
        assert!(s.first_slice_segment_in_pic);
        assert_eq!(s.no_output_of_prior_pics, Some(false));
        let s = SliceSegmentHeaderStart::parse(&[0x02, 0x01, 0b0010_0000]).unwrap();
        assert!(!s.first_slice_segment_in_pic);
        assert_eq!(s.slice_pic_parameter_set_id, 1);
        assert!(SliceSegmentHeaderStart::parse(&[0x40, 0x01, 0x80]).is_err());
        assert_eq!(
            SliceSegmentHeaderStart::parse(&[0x02, 0x01]),
            Err(Error::Truncated)
        );
    }

    #[test]
    fn sps_rejects_malformed() {
        let good = build_sps(&SpsSpec {
            st_rps: true,
            scaling: true,
            timing: Some((1, 30)),
            hrd: true,
            ..Default::default()
        });
        assert!(Sps::parse(&good).is_ok());
        for n in 0..good.len() {
            let _ = Sps::parse(&good[..n]);
        }
        assert!(Sps::parse(&good[..20]).is_err());
        let bad = build_sps(&SpsSpec {
            window: Some((0, 0, 0, 600)),
            ..Default::default()
        });
        assert!(matches!(Sps::parse(&bad), Err(Error::Invalid(_))));
        let bad = build_sps(&SpsSpec {
            width: 1921,
            ..Default::default()
        });
        assert!(matches!(Sps::parse(&bad), Err(Error::Invalid(_))));
        let bad = build_sps(&SpsSpec {
            width: 0,
            ..Default::default()
        });
        assert!(Sps::parse(&bad).is_err());
        let bad = build_sps(&SpsSpec {
            sub_layers_minus1: 7,
            ..Default::default()
        });
        assert!(Sps::parse(&bad).is_err());
        // nuh_layer_id != 0
        let mut l1 = good.clone();
        l1[1] = 0x09;
        assert!(matches!(Sps::parse(&l1), Err(Error::Unsupported(_))));
    }
}
