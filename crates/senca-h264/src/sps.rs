//! SPS（seq_parameter_set_rbsp、H.264 7.3.2.1）の解析。

use crate::bits::Bits;
use crate::error::{Error, Result};
use crate::nal::{NalHeader, NalUnitType, remove_emulation_prevention};

/// 幅・高さのマクロブロック数の上限（32768 画素）。巨大な値での桁あふれと無駄な計算を防ぐ。
const MAX_MBS_PER_DIM: u32 = 2048;

/// クロップ量（frame_crop_*_offset。単位は CropUnitX/CropUnitY）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Crop {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

/// 色の記述（colour_description）。
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

/// timing_info。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub fixed_frame_rate: bool,
}

/// bitstream_restriction の一部。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitstreamRestriction {
    pub max_num_reorder_frames: u32,
    pub max_dec_frame_buffering: u32,
}

/// VUI（附属書 E）のうち配信で使う部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Vui {
    /// 画素の縦横比（aspect_ratio_idc の表を引いた値。未指定なら `None`）。
    pub sample_aspect_ratio: Option<(u16, u16)>,
    pub video_signal: Option<VideoSignal>,
    pub timing: Option<Timing>,
    pub pic_struct_present: bool,
    pub bitstream_restriction: Option<BitstreamRestriction>,
}

/// SPS の解析結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    /// constraint_set0〜5 と reserved_zero_2bits を合わせた 1 バイト（codecs 文字列の CC）。
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub seq_parameter_set_id: u8,
    pub chroma_format_idc: u8,
    pub separate_colour_plane: bool,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub log2_max_frame_num: u8,
    pub pic_order_cnt_type: u8,
    /// pic_order_cnt_type が 0 のときだけ意味を持つ（それ以外は 0）。
    pub log2_max_pic_order_cnt_lsb: u8,
    pub delta_pic_order_always_zero: bool,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_allowed: bool,
    pub pic_width_in_mbs: u32,
    pub pic_height_in_map_units: u32,
    pub frame_mbs_only: bool,
    pub mb_adaptive_frame_field: bool,
    pub direct_8x8_inference: bool,
    pub crop: Crop,
    /// クロップ後の表示幅（画素）。
    pub width: u32,
    /// クロップ後の表示高さ（画素）。
    pub height: u32,
    /// VUI。無い、または壊れているときは `None`（幅・高さは VUI に依らないので SPS 自体は受け付ける）。
    pub vui: Option<Vui>,
}

/// chroma_format_idc などを持つ（High 系の）profile_idc。
fn has_chroma_info(profile_idc: u8) -> bool {
    matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    )
}

fn skip_scaling_list(r: &mut Bits<'_>, size: usize) -> Result<()> {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            let delta = r.se()?;
            if !(-128..=127).contains(&delta) {
                return Err(Error::Invalid("delta_scale"));
            }
            next = (last + delta + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Ok(())
}

fn skip_hrd(r: &mut Bits<'_>) -> Result<()> {
    let cpb_cnt = r.ue_max(31, "cpb_cnt_minus1")? + 1;
    r.skip(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..cpb_cnt {
        r.ue()?; // bit_rate_value_minus1
        r.ue()?; // cpb_size_value_minus1
        r.skip(1)?; // cbr_flag
    }
    r.skip(20) // 4 つの長さ（各 5 ビット）
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

fn parse_vui(r: &mut Bits<'_>) -> Result<Vui> {
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
        r.skip(1)?; // overscan_appropriate_flag
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
    if r.flag()? {
        let num_units_in_tick = r.u(32)?;
        let time_scale = r.u(32)?;
        let fixed_frame_rate = r.flag()?;
        vui.timing = Some(Timing {
            num_units_in_tick,
            time_scale,
            fixed_frame_rate,
        });
    }
    let nal_hrd = r.flag()?;
    if nal_hrd {
        skip_hrd(r)?;
    }
    let vcl_hrd = r.flag()?;
    if vcl_hrd {
        skip_hrd(r)?;
    }
    if nal_hrd || vcl_hrd {
        r.skip(1)?; // low_delay_hrd_flag
    }
    vui.pic_struct_present = r.flag()?;
    if r.flag()? {
        r.skip(1)?; // motion_vectors_over_pic_boundaries_flag
        r.ue()?; // max_bytes_per_pic_denom
        r.ue()?; // max_bits_per_mb_denom
        r.ue()?; // log2_max_mv_length_horizontal
        r.ue()?; // log2_max_mv_length_vertical
        let max_num_reorder_frames = r.ue_max(16, "max_num_reorder_frames")?;
        let max_dec_frame_buffering = r.ue_max(16, "max_dec_frame_buffering")?;
        vui.bitstream_restriction = Some(BitstreamRestriction {
            max_num_reorder_frames,
            max_dec_frame_buffering,
        });
    }
    Ok(vui)
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

impl Sps {
    /// SPS の NAL（ヘッダー付き、emulation prevention 込み）を解析する。
    pub fn parse(nal: &[u8]) -> Result<Sps> {
        let h = NalHeader::parse(nal)?;
        if h.nal_unit_type != NalUnitType::Sps {
            return Err(Error::Invalid("SPS ではない NAL"));
        }
        Self::parse_rbsp(&remove_emulation_prevention(&nal[1..]))
    }

    /// NAL ヘッダーを除き emulation prevention を取り除いた RBSP を解析する。
    pub fn parse_rbsp(rbsp: &[u8]) -> Result<Sps> {
        let mut r = Bits::new(rbsp);
        let profile_idc = r.u8(8)?;
        let constraint_flags = r.u8(8)?;
        let level_idc = r.u8(8)?;
        let seq_parameter_set_id = r.ue_max(31, "seq_parameter_set_id")? as u8;

        let mut chroma_format_idc = 1u8;
        let mut separate_colour_plane = false;
        let mut bit_depth_luma = 8u8;
        let mut bit_depth_chroma = 8u8;
        if has_chroma_info(profile_idc) {
            chroma_format_idc = r.ue_max(3, "chroma_format_idc")? as u8;
            if chroma_format_idc == 3 {
                separate_colour_plane = r.flag()?;
            }
            bit_depth_luma = r.ue_max(6, "bit_depth_luma_minus8")? as u8 + 8;
            bit_depth_chroma = r.ue_max(6, "bit_depth_chroma_minus8")? as u8 + 8;
            r.skip(1)?; // qpprime_y_zero_transform_bypass_flag
            if r.flag()? {
                // seq_scaling_matrix_present_flag
                let n = if chroma_format_idc == 3 { 12 } else { 8 };
                for i in 0..n {
                    if r.flag()? {
                        skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                    }
                }
            }
        }

        let log2_max_frame_num = r.ue_max(12, "log2_max_frame_num_minus4")? as u8 + 4;
        let pic_order_cnt_type = r.ue_max(2, "pic_order_cnt_type")? as u8;
        let mut log2_max_pic_order_cnt_lsb = 0;
        let mut delta_pic_order_always_zero = false;
        match pic_order_cnt_type {
            0 => {
                log2_max_pic_order_cnt_lsb =
                    r.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")? as u8 + 4;
            }
            1 => {
                delta_pic_order_always_zero = r.flag()?;
                r.se()?; // offset_for_non_ref_pic
                r.se()?; // offset_for_top_to_bottom_field
                let n = r.ue_max(255, "num_ref_frames_in_pic_order_cnt_cycle")?;
                for _ in 0..n {
                    r.se()?;
                }
            }
            _ => {}
        }
        let max_num_ref_frames = r.ue_max(16, "max_num_ref_frames")?;
        let gaps_in_frame_num_allowed = r.flag()?;
        let pic_width_in_mbs = r.ue_max(MAX_MBS_PER_DIM - 1, "pic_width_in_mbs_minus1")? + 1;
        let pic_height_in_map_units =
            r.ue_max(MAX_MBS_PER_DIM - 1, "pic_height_in_map_units_minus1")? + 1;
        let frame_mbs_only = r.flag()?;
        let mb_adaptive_frame_field = if frame_mbs_only { false } else { r.flag()? };
        let direct_8x8_inference = r.flag()?;
        let mut crop = Crop::default();
        if r.flag()? {
            crop.left = r.ue()?;
            crop.right = r.ue()?;
            crop.top = r.ue()?;
            crop.bottom = r.ue()?;
        }
        let vui_present = r.flag()?;
        let vui = if vui_present {
            parse_vui(&mut r).ok()
        } else {
            None
        };

        // 幅・高さ（7.4.2.1.1 の CropUnitX/Y）
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
        let field_factor = if frame_mbs_only { 1u64 } else { 2 };
        let crop_unit_x = sub_w;
        let crop_unit_y = sub_h * field_factor;
        let full_w = pic_width_in_mbs as u64 * 16;
        let full_h = pic_height_in_map_units as u64 * 16 * field_factor;
        let crop_w = crop_unit_x * (crop.left as u64 + crop.right as u64);
        let crop_h = crop_unit_y * (crop.top as u64 + crop.bottom as u64);
        if crop_w >= full_w || crop_h >= full_h {
            return Err(Error::Invalid("frame_crop_offset が画面より大きい"));
        }
        // 上限付きなので u32 に収まる
        let width = (full_w - crop_w) as u32;
        let height = (full_h - crop_h) as u32;

        Ok(Sps {
            profile_idc,
            constraint_flags,
            level_idc,
            seq_parameter_set_id,
            chroma_format_idc,
            separate_colour_plane,
            bit_depth_luma,
            bit_depth_chroma,
            log2_max_frame_num,
            pic_order_cnt_type,
            log2_max_pic_order_cnt_lsb,
            delta_pic_order_always_zero,
            max_num_ref_frames,
            gaps_in_frame_num_allowed,
            pic_width_in_mbs,
            pic_height_in_map_units,
            frame_mbs_only,
            mb_adaptive_frame_field,
            direct_8x8_inference,
            crop,
            width,
            height,
            vui,
        })
    }

    /// RFC 6381 の codecs 文字列（`avc1.PPCCLL`）。
    pub fn codecs_string(&self) -> String {
        codecs_string(self.profile_idc, self.constraint_flags, self.level_idc)
    }

    /// constraint_set3_flag。
    pub fn constraint_set3(&self) -> bool {
        self.constraint_flags & 0x10 != 0
    }

    /// フレームレート（分子, 分母）。VUI の timing_info があるときだけ。
    ///
    /// H.264 の tick はフィールド単位なので `time_scale / (2 * num_units_in_tick)`。約分して返す。
    pub fn frame_rate(&self) -> Option<(u32, u32)> {
        let t = self.vui.as_ref()?.timing?;
        if t.num_units_in_tick == 0 || t.time_scale == 0 {
            return None;
        }
        let num = t.time_scale as u64;
        let den = 2 * t.num_units_in_tick as u64;
        let g = gcd(num, den);
        Some((u32::try_from(num / g).ok()?, u32::try_from(den / g).ok()?))
    }

    /// max_num_reorder_frames。VUI にあればその値、intra 専用プロファイルなら 0（7.4.2.1.1 の推定）、
    /// それ以外は分からないので `None`。
    pub fn max_num_reorder_frames(&self) -> Option<u32> {
        if let Some(br) = self.vui.as_ref().and_then(|v| v.bitstream_restriction) {
            return Some(br.max_num_reorder_frames);
        }
        if self.constraint_set3() && matches!(self.profile_idc, 44 | 86 | 100 | 110 | 122 | 244) {
            return Some(0);
        }
        None
    }

    /// 表示順の並べ替え（B フレームなど）がありうるか。分からないときは Baseline 以外を「ありうる」とする。
    pub fn may_reorder(&self) -> bool {
        match self.max_num_reorder_frames() {
            Some(n) => n > 0,
            None => self.profile_idc != 66,
        }
    }
}

/// `avc1.PPCCLL`（16 進大文字）。
pub fn codecs_string(profile_idc: u8, constraint_flags: u8, level_idc: u8) -> String {
    format!("avc1.{profile_idc:02X}{constraint_flags:02X}{level_idc:02X}")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::nal::add_emulation_prevention;
    use senca_core::bits::BitWriter;

    /// 試験用の SPS を組み立てる。
    pub(crate) struct SpsSpec {
        pub profile_idc: u8,
        pub constraint: u8,
        pub level: u8,
        pub chroma_format_idc: u32,
        pub bit_depth_minus8: u32,
        pub poc_type: u32,
        pub frame_mbs_only: bool,
        pub width_mbs: u32,
        pub height_map_units: u32,
        pub crop: Option<(u32, u32, u32, u32)>,
        pub timing: Option<(u32, u32)>,
        pub reorder: Option<u32>,
        pub scaling: bool,
    }

    impl Default for SpsSpec {
        fn default() -> Self {
            SpsSpec {
                profile_idc: 100,
                constraint: 0,
                level: 31,
                chroma_format_idc: 1,
                bit_depth_minus8: 0,
                poc_type: 0,
                frame_mbs_only: true,
                width_mbs: 80,
                height_map_units: 45,
                crop: None,
                timing: None,
                reorder: None,
                scaling: false,
            }
        }
    }

    pub(crate) fn build_sps(s: &SpsSpec) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_bits(s.profile_idc as u64, 8);
        w.write_bits(s.constraint as u64, 8);
        w.write_bits(s.level as u64, 8);
        w.write_ue(0);
        if has_chroma_info(s.profile_idc) {
            w.write_ue(s.chroma_format_idc);
            if s.chroma_format_idc == 3 {
                w.write_bit(false);
            }
            w.write_ue(s.bit_depth_minus8);
            w.write_ue(s.bit_depth_minus8);
            w.write_bit(false);
            w.write_bit(s.scaling);
            if s.scaling {
                let n = if s.chroma_format_idc == 3 { 12 } else { 8 };
                for i in 0..n {
                    w.write_bit(i == 0 || i == 6);
                    if i == 0 {
                        for _ in 0..16 {
                            w.write_se(1);
                        }
                    }
                    if i == 6 {
                        w.write_se(-8); // next = 0 で打ち切り
                    }
                }
            }
        }
        w.write_ue(0); // log2_max_frame_num_minus4
        w.write_ue(s.poc_type);
        match s.poc_type {
            0 => w.write_ue(2),
            1 => {
                w.write_bit(false);
                w.write_se(-1);
                w.write_se(2);
                w.write_ue(2);
                w.write_se(3);
                w.write_se(-3);
            }
            _ => {}
        }
        w.write_ue(4); // max_num_ref_frames
        w.write_bit(false);
        w.write_ue(s.width_mbs - 1);
        w.write_ue(s.height_map_units - 1);
        w.write_bit(s.frame_mbs_only);
        if !s.frame_mbs_only {
            w.write_bit(true);
        }
        w.write_bit(true);
        match s.crop {
            Some((l, r, t, b)) => {
                w.write_bit(true);
                w.write_ue(l);
                w.write_ue(r);
                w.write_ue(t);
                w.write_ue(b);
            }
            None => w.write_bit(false),
        }
        let vui = s.timing.is_some() || s.reorder.is_some();
        w.write_bit(vui);
        if vui {
            w.write_bit(true); // aspect_ratio_info_present
            w.write_bits(255, 8);
            w.write_bits(4, 16);
            w.write_bits(3, 16);
            w.write_bit(false); // overscan
            w.write_bit(true); // video_signal_type
            w.write_bits(5, 3);
            w.write_bit(true);
            w.write_bit(true);
            w.write_bits(1, 8);
            w.write_bits(1, 8);
            w.write_bits(1, 8);
            w.write_bit(false); // chroma_loc
            match s.timing {
                Some((n, t)) => {
                    w.write_bit(true);
                    w.write_bits(n as u64, 32);
                    w.write_bits(t as u64, 32);
                    w.write_bit(true);
                }
                None => w.write_bit(false),
            }
            w.write_bit(true); // nal_hrd
            w.write_ue(0);
            w.write_bits(0, 8);
            w.write_ue(1000);
            w.write_ue(2000);
            w.write_bit(false);
            w.write_bits(0, 20);
            w.write_bit(false); // vcl_hrd
            w.write_bit(false); // low_delay
            w.write_bit(false); // pic_struct
            match s.reorder {
                Some(n) => {
                    w.write_bit(true);
                    w.write_bit(true);
                    w.write_ue(2);
                    w.write_ue(1);
                    w.write_ue(16);
                    w.write_ue(16);
                    w.write_ue(n);
                    w.write_ue(4);
                }
                None => w.write_bit(false),
            }
        }
        w.write_bit(true); // rbsp_stop_one_bit
        let mut nal = vec![0x67];
        nal.extend(add_emulation_prevention(&w.finish()));
        nal
    }

    #[test]
    fn basic_1080p_crop() {
        let nal = build_sps(&SpsSpec {
            width_mbs: 120,
            height_map_units: 68,
            crop: Some((0, 0, 0, 4)),
            timing: Some((1001, 60000)),
            reorder: Some(2),
            ..Default::default()
        });
        let s = Sps::parse(&nal).unwrap();
        assert_eq!((s.width, s.height), (1920, 1080));
        assert_eq!(s.frame_rate(), Some((30000, 1001)));
        assert_eq!(s.max_num_reorder_frames(), Some(2));
        assert!(s.may_reorder());
        assert_eq!(s.codecs_string(), "avc1.64001F");
        let vui = s.vui.unwrap();
        assert_eq!(vui.sample_aspect_ratio, Some((4, 3)));
        assert_eq!(
            vui.video_signal.unwrap().colour.unwrap().colour_primaries,
            1
        );
        assert!(vui.video_signal.unwrap().full_range);
    }

    #[test]
    fn interlaced_and_chroma_variants() {
        let s = Sps::parse(&build_sps(&SpsSpec {
            frame_mbs_only: false,
            width_mbs: 45,
            height_map_units: 18,
            crop: Some((0, 0, 0, 2)),
            ..Default::default()
        }))
        .unwrap();
        // 4:2:0・フィールド: CropUnitY = 2 * 2
        assert_eq!((s.width, s.height), (720, 576 - 8));
        assert!(s.mb_adaptive_frame_field);

        let s = Sps::parse(&build_sps(&SpsSpec {
            profile_idc: 244,
            chroma_format_idc: 3,
            bit_depth_minus8: 2,
            crop: Some((1, 1, 1, 1)),
            scaling: true,
            ..Default::default()
        }))
        .unwrap();
        assert_eq!(s.chroma_format_idc, 3);
        assert_eq!(s.bit_depth_luma, 10);
        assert_eq!((s.width, s.height), (1278, 718));

        let s = Sps::parse(&build_sps(&SpsSpec {
            profile_idc: 122,
            chroma_format_idc: 2,
            crop: Some((1, 1, 1, 1)),
            ..Default::default()
        }))
        .unwrap();
        assert_eq!((s.width, s.height), (1276, 718));

        let s = Sps::parse(&build_sps(&SpsSpec {
            profile_idc: 100,
            chroma_format_idc: 0,
            crop: Some((1, 1, 1, 1)),
            ..Default::default()
        }))
        .unwrap();
        assert_eq!((s.width, s.height), (1278, 718));
    }

    #[test]
    fn baseline_and_poc_types() {
        let s = Sps::parse(&build_sps(&SpsSpec {
            profile_idc: 66,
            constraint: 0xC0,
            level: 30,
            poc_type: 2,
            ..Default::default()
        }))
        .unwrap();
        assert_eq!(s.chroma_format_idc, 1);
        assert_eq!(s.pic_order_cnt_type, 2);
        assert_eq!(s.max_num_reorder_frames(), None);
        assert!(!s.may_reorder());
        assert_eq!(s.codecs_string(), "avc1.42C01E");

        let s = Sps::parse(&build_sps(&SpsSpec {
            poc_type: 1,
            ..Default::default()
        }))
        .unwrap();
        assert_eq!(s.pic_order_cnt_type, 1);
        assert!(s.may_reorder());

        // High 10 Intra（constraint_set3）
        let s = Sps::parse(&build_sps(&SpsSpec {
            profile_idc: 110,
            constraint: 0x10,
            ..Default::default()
        }))
        .unwrap();
        assert_eq!(s.max_num_reorder_frames(), Some(0));
    }

    #[test]
    fn rejects_malformed() {
        let good = build_sps(&SpsSpec {
            timing: Some((1, 50)),
            reorder: Some(0),
            ..Default::default()
        });
        assert!(Sps::parse(&good).is_ok());
        // 途中で切った入力はすべて Err か（VUI の途中なら）VUI なしで受理。パニックしない。
        for n in 0..good.len() {
            let _ = Sps::parse(&good[..n]);
        }
        assert_eq!(Sps::parse(&good[..4]), Err(Error::Truncated));
        assert!(Sps::parse(&[0x68, 0x64]).is_err());
        assert!(Sps::parse(&[]).is_err());
        // クロップが画面より大きい
        let bad = build_sps(&SpsSpec {
            width_mbs: 1,
            height_map_units: 1,
            crop: Some((4, 4, 0, 0)),
            ..Default::default()
        });
        assert!(matches!(Sps::parse(&bad), Err(Error::Invalid(_))));
        // 巨大な幅
        let bad = build_sps(&SpsSpec {
            width_mbs: 5000,
            ..Default::default()
        });
        assert!(matches!(Sps::parse(&bad), Err(Error::Invalid(_))));
        // ue が長すぎる（0 が 32 個以上）
        let mut bad = vec![0x67, 100, 0, 31];
        bad.extend([0u8; 5]);
        bad.push(0x80);
        assert!(Sps::parse(&bad).is_err());
    }

    #[test]
    fn frame_rate_edge_cases() {
        let s = Sps::parse(&build_sps(&SpsSpec {
            timing: Some((0, 50)),
            ..Default::default()
        }))
        .unwrap();
        assert_eq!(s.frame_rate(), None);
        let s = Sps::parse(&build_sps(&SpsSpec {
            timing: Some((u32::MAX, u32::MAX)),
            ..Default::default()
        }))
        .unwrap();
        assert_eq!(s.frame_rate(), Some((1, 2)));
        let s = Sps::parse(&build_sps(&SpsSpec {
            timing: Some((3, u32::MAX)),
            ..Default::default()
        }))
        .unwrap();
        assert_eq!(s.frame_rate(), Some((u32::MAX / 3, 2)));
    }
}
