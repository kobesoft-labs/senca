//! シーケンスヘッダー OBU（AV1 仕様 5.5）とフレームヘッダーの先頭（5.9.2）。

use crate::error::{Error, Result};
use crate::obu::{ObuType, obus};
use senca_core::bits::BitReader;

struct R<'a>(BitReader<'a>);

impl R<'_> {
    fn u(&mut self, n: u32) -> Result<u32> {
        self.0
            .read_bits(n)
            .map(|v| v as u32)
            .ok_or(Error::Truncated)
    }
    fn u8(&mut self, n: u32) -> Result<u8> {
        self.u(n).map(|v| v as u8)
    }
    fn flag(&mut self) -> Result<bool> {
        self.0.read_bit().ok_or(Error::Truncated)
    }
    /// uvlc()（4.10.3）。
    fn uvlc(&mut self) -> Result<u32> {
        let mut zeros = 0u32;
        while !self.flag()? {
            zeros += 1;
        }
        if zeros >= 32 {
            return Ok(u32::MAX);
        }
        let v = self.u(zeros)? as u64;
        Ok(((1u64 << zeros) - 1 + v).min(u32::MAX as u64) as u32)
    }
}

/// timing_info。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingInfo {
    pub num_units_in_display_tick: u32,
    pub time_scale: u32,
    /// equal_picture_interval が 1 のときの num_ticks_per_picture_minus_1 + 1。
    pub num_ticks_per_picture: Option<u32>,
}

/// 動作点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatingPoint {
    pub idc: u16,
    pub seq_level_idx: u8,
    pub seq_tier: u8,
    /// initial_display_delay_minus_1 + 1（書かれていれば）。
    pub initial_display_delay: Option<u8>,
}

/// color_config。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorConfig {
    pub bit_depth: u8,
    pub mono_chrome: bool,
    pub color_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub color_range: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
    pub separate_uv_delta_q: bool,
}

/// シーケンスヘッダー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceHeader {
    pub seq_profile: u8,
    pub still_picture: bool,
    pub reduced_still_picture_header: bool,
    pub timing_info: Option<TimingInfo>,
    pub decoder_model_info_present: bool,
    pub operating_points: Vec<OperatingPoint>,
    pub max_frame_width: u32,
    pub max_frame_height: u32,
    pub frame_id_numbers_present: bool,
    pub use_128x128_superblock: bool,
    pub enable_order_hint: bool,
    pub enable_superres: bool,
    pub enable_cdef: bool,
    pub enable_restoration: bool,
    pub color: ColorConfig,
    pub film_grain_params_present: bool,
}

fn parse_color_config(r: &mut R<'_>, seq_profile: u8) -> Result<ColorConfig> {
    let high_bitdepth = r.flag()?;
    let bit_depth = if seq_profile == 2 && high_bitdepth {
        if r.flag()? { 12 } else { 10 }
    } else if high_bitdepth {
        10
    } else {
        8
    };
    let mono_chrome = if seq_profile == 1 { false } else { r.flag()? };
    let (color_primaries, transfer_characteristics, matrix_coefficients) = if r.flag()? {
        (r.u8(8)?, r.u8(8)?, r.u8(8)?)
    } else {
        (2, 2, 2)
    };
    let mut c = ColorConfig {
        bit_depth,
        mono_chrome,
        color_primaries,
        transfer_characteristics,
        matrix_coefficients,
        color_range: false,
        subsampling_x: true,
        subsampling_y: true,
        chroma_sample_position: 0,
        separate_uv_delta_q: false,
    };
    if mono_chrome {
        c.color_range = r.flag()?;
        return Ok(c);
    }
    if color_primaries == 1 && transfer_characteristics == 13 && matrix_coefficients == 0 {
        // sRGB
        c.color_range = true;
        c.subsampling_x = false;
        c.subsampling_y = false;
    } else {
        c.color_range = r.flag()?;
        match seq_profile {
            0 => {}
            1 => {
                c.subsampling_x = false;
                c.subsampling_y = false;
            }
            _ => {
                if bit_depth == 12 {
                    c.subsampling_x = r.flag()?;
                    c.subsampling_y = c.subsampling_x && r.flag()?;
                } else {
                    c.subsampling_x = true;
                    c.subsampling_y = false;
                }
            }
        }
        if c.subsampling_x && c.subsampling_y {
            c.chroma_sample_position = r.u8(2)?;
        }
    }
    c.separate_uv_delta_q = r.flag()?;
    Ok(c)
}

impl SequenceHeader {
    /// シーケンスヘッダー OBU のペイロード（OBU ヘッダーと obu_size を除いたもの）を解析する。
    pub fn parse(payload: &[u8]) -> Result<SequenceHeader> {
        let mut r = R(BitReader::new(payload));
        let seq_profile = r.u8(3)?;
        if seq_profile > 2 {
            return Err(Error::Invalid("seq_profile"));
        }
        let still_picture = r.flag()?;
        let reduced_still_picture_header = r.flag()?;
        let mut timing_info = None;
        let mut decoder_model_info_present = false;
        let mut operating_points = Vec::new();
        if reduced_still_picture_header {
            if !still_picture {
                return Err(Error::Invalid(
                    "reduced_still_picture_header で still_picture が 0",
                ));
            }
            operating_points.push(OperatingPoint {
                idc: 0,
                seq_level_idx: r.u8(5)?,
                seq_tier: 0,
                initial_display_delay: None,
            });
        } else {
            if r.flag()? {
                let num_units_in_display_tick = r.u(32)?;
                let time_scale = r.u(32)?;
                let num_ticks_per_picture = if r.flag()? {
                    Some(r.uvlc()?.saturating_add(1))
                } else {
                    None
                };
                timing_info = Some(TimingInfo {
                    num_units_in_display_tick,
                    time_scale,
                    num_ticks_per_picture,
                });
            }
            let mut buffer_delay_length = 0;
            if timing_info.is_some() {
                decoder_model_info_present = r.flag()?;
                if decoder_model_info_present {
                    buffer_delay_length = r.u(5)? + 1;
                    r.u(32)?; // num_units_in_decoding_tick
                    r.u(5)?; // buffer_removal_time_length_minus_1
                    r.u(5)?; // frame_presentation_time_length_minus_1
                }
            }
            let initial_display_delay_present = r.flag()?;
            let count = r.u(5)? + 1;
            for _ in 0..count {
                let idc = r.u(12)? as u16;
                let seq_level_idx = r.u8(5)?;
                let seq_tier = if seq_level_idx > 7 { r.u8(1)? } else { 0 };
                if decoder_model_info_present && r.flag()? {
                    r.u(buffer_delay_length)?; // decoder_buffer_delay
                    r.u(buffer_delay_length)?; // encoder_buffer_delay
                    r.flag()?; // low_delay_mode_flag
                }
                let initial_display_delay = if initial_display_delay_present && r.flag()? {
                    Some(r.u8(4)? + 1)
                } else {
                    None
                };
                operating_points.push(OperatingPoint {
                    idc,
                    seq_level_idx,
                    seq_tier,
                    initial_display_delay,
                });
            }
        }
        let wbits = r.u(4)? + 1;
        let hbits = r.u(4)? + 1;
        let max_frame_width = r.u(wbits)? + 1;
        let max_frame_height = r.u(hbits)? + 1;
        let frame_id_numbers_present = if reduced_still_picture_header {
            false
        } else {
            r.flag()?
        };
        if frame_id_numbers_present {
            r.u(4 + 3)?;
        }
        let use_128x128_superblock = r.flag()?;
        r.u(2)?; // enable_filter_intra, enable_intra_edge_filter
        let mut enable_order_hint = false;
        if !reduced_still_picture_header {
            r.u(4)?; // interintra_compound, masked_compound, warped_motion, dual_filter
            enable_order_hint = r.flag()?;
            if enable_order_hint {
                r.u(2)?; // jnt_comp, ref_frame_mvs
            }
            let seq_force_screen_content_tools = if r.flag()? { 2 } else { r.u(1)? };
            if seq_force_screen_content_tools > 0 && !r.flag()? {
                r.u(1)?; // seq_force_integer_mv
            }
            if enable_order_hint {
                r.u(3)?; // order_hint_bits_minus_1
            }
        }
        let enable_superres = r.flag()?;
        let enable_cdef = r.flag()?;
        let enable_restoration = r.flag()?;
        let color = parse_color_config(&mut r, seq_profile)?;
        let film_grain_params_present = r.flag()?;
        Ok(SequenceHeader {
            seq_profile,
            still_picture,
            reduced_still_picture_header,
            timing_info,
            decoder_model_info_present,
            operating_points,
            max_frame_width,
            max_frame_height,
            frame_id_numbers_present,
            use_128x128_superblock,
            enable_order_hint,
            enable_superres,
            enable_cdef,
            enable_restoration,
            color,
            film_grain_params_present,
        })
    }

    /// OBU 列の中から最初のシーケンスヘッダーを探して解析する。戻り値の 2 番目はその OBU 全体のバイト列。
    pub fn find_in(data: &[u8]) -> Result<Option<(SequenceHeader, &[u8])>> {
        for obu in obus(data) {
            let obu = obu?;
            if obu.header.obu_type == ObuType::SequenceHeader {
                return Ok(Some((Self::parse(obu.payload)?, obu.raw)));
            }
        }
        Ok(None)
    }

    pub fn seq_level_idx_0(&self) -> u8 {
        self.operating_points.first().map_or(0, |o| o.seq_level_idx)
    }

    pub fn seq_tier_0(&self) -> u8 {
        self.operating_points.first().map_or(0, |o| o.seq_tier)
    }

    /// codecs 文字列の短い形（`av01.P.LLT.DD`、AV1 ISOBMFF 付録 A）。
    pub fn codecs_string(&self) -> String {
        format!(
            "av01.{}.{:02}{}.{:02}",
            self.seq_profile,
            self.seq_level_idx_0(),
            if self.seq_tier_0() == 1 { 'H' } else { 'M' },
            self.color.bit_depth
        )
    }

    /// codecs 文字列の長い形（`av01.P.LLT.DD.M.CCC.cp.tc.mc.F`）。
    pub fn codecs_string_full(&self) -> String {
        let c = &self.color;
        format!(
            "{}.{}.{}{}{}.{:02}.{:02}.{:02}.{}",
            self.codecs_string(),
            c.mono_chrome as u8,
            c.subsampling_x as u8,
            c.subsampling_y as u8,
            if c.subsampling_x && c.subsampling_y {
                c.chroma_sample_position
            } else {
                0
            },
            c.color_primaries,
            c.transfer_characteristics,
            c.matrix_coefficients,
            c.color_range as u8
        )
    }

    /// フレームレート（分子, 分母）。timing_info に equal_picture_interval があるときだけ。
    pub fn frame_rate(&self) -> Option<(u32, u32)> {
        let t = self.timing_info?;
        let den =
            (t.num_units_in_display_tick as u64).checked_mul(t.num_ticks_per_picture? as u64)?;
        if den == 0 || t.time_scale == 0 {
            return None;
        }
        let mut a = t.time_scale as u64;
        let mut b = den;
        while b != 0 {
            (a, b) = (b, a % b);
        }
        Some((
            u32::try_from(t.time_scale as u64 / a).ok()?,
            u32::try_from(den / a).ok()?,
        ))
    }
}

/// frame_type。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Key,
    Inter,
    IntraOnly,
    Switch,
}

/// フレームヘッダーの先頭。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeaderStart {
    pub show_existing_frame: bool,
    /// show_existing_frame が 1 なら `None`。
    pub frame_type: Option<FrameType>,
    pub show_frame: bool,
}

impl FrameHeaderStart {
    /// フレームヘッダー OBU・フレーム OBU のペイロードの先頭を読む。
    pub fn parse(payload: &[u8], reduced_still_picture_header: bool) -> Result<Self> {
        if reduced_still_picture_header {
            return Ok(FrameHeaderStart {
                show_existing_frame: false,
                frame_type: Some(FrameType::Key),
                show_frame: true,
            });
        }
        let mut r = R(BitReader::new(payload));
        if r.flag()? {
            return Ok(FrameHeaderStart {
                show_existing_frame: true,
                frame_type: None,
                show_frame: true,
            });
        }
        let frame_type = match r.u(2)? {
            0 => FrameType::Key,
            1 => FrameType::Inter,
            2 => FrameType::IntraOnly,
            _ => FrameType::Switch,
        };
        Ok(FrameHeaderStart {
            show_existing_frame: false,
            frame_type: Some(frame_type),
            show_frame: r.flag()?,
        })
    }
}

/// 時間単位（TU）がキーフレームか。最初のフレームヘッダーが表示される KEY_FRAME（show_existing_frame = 0）なら真。
///
/// `seq` は直前までに受け取ったシーケンスヘッダー。TU の中にシーケンスヘッダーがあればそちらを使う。
pub fn is_keyframe(tu: &[u8], seq: Option<&SequenceHeader>) -> Result<bool> {
    let mut reduced = seq.is_some_and(|s| s.reduced_still_picture_header);
    for obu in obus(tu) {
        let obu = obu?;
        match obu.header.obu_type {
            ObuType::SequenceHeader => {
                reduced = SequenceHeader::parse(obu.payload)?.reduced_still_picture_header;
            }
            ObuType::FrameHeader | ObuType::Frame => {
                let f = FrameHeaderStart::parse(obu.payload, reduced)?;
                return Ok(!f.show_existing_frame
                    && f.show_frame
                    && f.frame_type == Some(FrameType::Key));
            }
            _ => {}
        }
    }
    Ok(false)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use senca_core::bits::BitWriter;

    pub(crate) struct Spec {
        pub profile: u8,
        pub level: u8,
        pub tier: bool,
        pub high_bitdepth: bool,
        pub twelve: bool,
        pub mono: bool,
        pub width: u32,
        pub height: u32,
        pub timing: Option<(u32, u32, u32)>,
        pub decoder_model: bool,
        pub ops: u32,
        pub reduced: bool,
        pub srgb: bool,
        pub initial_display_delay: Option<u8>,
    }

    impl Default for Spec {
        fn default() -> Self {
            Spec {
                profile: 0,
                level: 8,
                tier: false,
                high_bitdepth: false,
                twelve: false,
                mono: false,
                width: 1920,
                height: 1080,
                timing: None,
                decoder_model: false,
                ops: 1,
                reduced: false,
                srgb: false,
                initial_display_delay: None,
            }
        }
    }

    pub(crate) fn build(s: &Spec) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_bits(s.profile as u64, 3);
        w.write_bit(s.reduced);
        w.write_bit(s.reduced);
        if s.reduced {
            w.write_bits(s.level as u64, 5);
        } else {
            w.write_bit(s.timing.is_some());
            if let Some((n, t, ticks)) = s.timing {
                w.write_bits(n as u64, 32);
                w.write_bits(t as u64, 32);
                w.write_bit(true);
                // uvlc(ticks - 1)
                let v = ticks as u64;
                let len = 64 - v.leading_zeros();
                w.write_bits(0, len - 1);
                w.write_bits(v, len);
                w.write_bit(s.decoder_model);
                if s.decoder_model {
                    w.write_bits(9, 5);
                    w.write_bits(1, 32);
                    w.write_bits(5, 5);
                    w.write_bits(5, 5);
                }
            }
            w.write_bit(s.initial_display_delay.is_some());
            w.write_bits((s.ops - 1) as u64, 5);
            for i in 0..s.ops {
                w.write_bits(if i == 0 { 0 } else { 0x101 }, 12);
                w.write_bits(s.level as u64, 5);
                if s.level > 7 {
                    w.write_bit(s.tier);
                }
                if s.decoder_model {
                    w.write_bit(true);
                    w.write_bits(0x3FF, 10);
                    w.write_bits(0, 10);
                    w.write_bit(false);
                }
                if let Some(d) = s.initial_display_delay {
                    w.write_bit(true);
                    w.write_bits((d - 1) as u64, 4);
                }
            }
        }
        w.write_bits(15, 4);
        w.write_bits(15, 4);
        w.write_bits((s.width - 1) as u64, 16);
        w.write_bits((s.height - 1) as u64, 16);
        if !s.reduced {
            w.write_bit(true); // frame_id_numbers_present
            w.write_bits(0x55, 7);
        }
        w.write_bit(false);
        w.write_bits(3, 2);
        if !s.reduced {
            w.write_bits(0xF, 4);
            w.write_bit(true);
            w.write_bits(3, 2);
            w.write_bit(true); // seq_choose_screen_content_tools
            w.write_bit(true); // seq_choose_integer_mv
            w.write_bits(6, 3);
        }
        w.write_bits(0b011, 3);
        // color_config
        w.write_bit(s.high_bitdepth);
        if s.profile == 2 && s.high_bitdepth {
            w.write_bit(s.twelve);
        }
        if s.profile != 1 {
            w.write_bit(s.mono);
        }
        w.write_bit(true);
        if s.srgb {
            w.write_bits(1, 8);
            w.write_bits(13, 8);
            w.write_bits(0, 8);
        } else {
            w.write_bits(9, 8);
            w.write_bits(16, 8);
            w.write_bits(9, 8);
        }
        if s.mono {
            w.write_bit(true);
        } else if !s.srgb {
            w.write_bit(false);
            if s.profile == 2 && s.twelve {
                w.write_bit(true);
                w.write_bit(false);
            }
            let (sx, sy) = match s.profile {
                0 => (true, true),
                1 => (false, false),
                _ => (true, false),
            };
            if sx && sy {
                w.write_bits(1, 2);
            }
            w.write_bit(false);
        } else {
            w.write_bit(false);
        }
        w.write_bit(true); // film_grain
        w.write_bit(true); // trailing
        w.finish()
    }

    #[test]
    fn main_profile_1080p() {
        let sh = SequenceHeader::parse(&build(&Spec::default())).unwrap();
        assert_eq!(sh.seq_profile, 0);
        assert_eq!((sh.max_frame_width, sh.max_frame_height), (1920, 1080));
        assert_eq!(sh.seq_level_idx_0(), 8);
        assert_eq!(sh.color.bit_depth, 8);
        assert!(sh.color.subsampling_x && sh.color.subsampling_y);
        assert_eq!(sh.color.chroma_sample_position, 1);
        assert!(sh.film_grain_params_present);
        assert!(sh.frame_id_numbers_present);
        assert_eq!(sh.codecs_string(), "av01.0.08M.08");
        assert_eq!(sh.codecs_string_full(), "av01.0.08M.08.0.111.09.16.09.0");
        assert_eq!(sh.frame_rate(), None);
    }

    #[test]
    fn variants() {
        let sh = SequenceHeader::parse(&build(&Spec {
            profile: 2,
            high_bitdepth: true,
            twelve: true,
            level: 13,
            tier: true,
            timing: Some((1001, 60000, 1)),
            decoder_model: true,
            ops: 3,
            initial_display_delay: Some(4),
            ..Default::default()
        }))
        .unwrap();
        assert_eq!(sh.color.bit_depth, 12);
        assert!(sh.color.subsampling_x && !sh.color.subsampling_y);
        assert_eq!(sh.operating_points.len(), 3);
        assert_eq!(sh.operating_points[0].initial_display_delay, Some(4));
        assert_eq!(sh.seq_tier_0(), 1);
        assert_eq!(sh.codecs_string(), "av01.2.13H.12");
        assert_eq!(sh.frame_rate(), Some((60000, 1001)));

        let sh = SequenceHeader::parse(&build(&Spec {
            profile: 1,
            high_bitdepth: true,
            ..Default::default()
        }))
        .unwrap();
        assert_eq!(sh.color.bit_depth, 10);
        assert!(!sh.color.subsampling_x);
        assert_eq!(sh.codecs_string(), "av01.1.08M.10");

        let sh = SequenceHeader::parse(&build(&Spec {
            mono: true,
            ..Default::default()
        }))
        .unwrap();
        assert!(sh.color.mono_chrome);
        assert!(sh.color.color_range);

        let sh = SequenceHeader::parse(&build(&Spec {
            srgb: true,
            profile: 1,
            ..Default::default()
        }))
        .unwrap();
        assert!(!sh.color.subsampling_x);
        assert!(sh.color.color_range);

        let sh = SequenceHeader::parse(&build(&Spec {
            reduced: true,
            level: 5,
            ..Default::default()
        }))
        .unwrap();
        assert!(sh.reduced_still_picture_header);
        assert_eq!(sh.seq_level_idx_0(), 5);
        assert!(FrameHeaderStart::parse(&[], true).unwrap().frame_type == Some(FrameType::Key));
    }

    #[test]
    fn rejects_malformed() {
        let good = build(&Spec {
            timing: Some((1, 30, 1)),
            decoder_model: true,
            ..Default::default()
        });
        assert!(SequenceHeader::parse(&good).is_ok());
        for n in 0..good.len() - 1 {
            assert!(SequenceHeader::parse(&good[..n]).is_err(), "len {n}");
        }
        assert!(SequenceHeader::parse(&[0xE0, 0, 0]).is_err()); // profile 7
        assert!(SequenceHeader::parse(&[0x08, 0, 0, 0]).is_err()); // reduced で still_picture 0
        // uvlc が長い（0 が 32 個以上）
        let mut w = BitWriter::new();
        w.write_bits(0, 3);
        w.write_bits(0b001, 3);
        w.write_bits(1, 32);
        w.write_bits(1, 32);
        w.write_bit(true);
        w.write_bits(0, 40);
        let b = w.finish();
        assert!(SequenceHeader::parse(&b).is_err());
    }

    #[test]
    fn frame_header_and_keyframe() {
        // show_existing_frame = 0、KEY_FRAME、show_frame = 1 → 0b0001_xxxx
        let f = FrameHeaderStart::parse(&[0b0001_0000], false).unwrap();
        assert_eq!(f.frame_type, Some(FrameType::Key));
        assert!(f.show_frame);
        let f = FrameHeaderStart::parse(&[0b1000_0000], false).unwrap();
        assert!(f.show_existing_frame);
        let f = FrameHeaderStart::parse(&[0b0011_0000], false).unwrap();
        assert_eq!(f.frame_type, Some(FrameType::Inter));
        assert_eq!(FrameHeaderStart::parse(&[], false), Err(Error::Truncated));

        let key_tu = [0x12, 0x00, 0x32, 0x01, 0b0001_0000];
        let inter_tu = [0x12, 0x00, 0x32, 0x01, 0b0011_0000];
        let hidden_key = [0x12, 0x00, 0x32, 0x01, 0b0000_0000];
        assert!(is_keyframe(&key_tu, None).unwrap());
        assert!(!is_keyframe(&inter_tu, None).unwrap());
        assert!(!is_keyframe(&hidden_key, None).unwrap());
        assert!(!is_keyframe(&[0x12, 0x00], None).unwrap());
        assert!(is_keyframe(&[0x32, 0x05, 0], None).is_err());
        // reduced_still_picture_header のシーケンスヘッダーを含む TU
        let sh = build(&Spec {
            reduced: true,
            ..Default::default()
        });
        let mut tu = vec![0x0A, sh.len() as u8];
        tu.extend(&sh);
        tu.extend([0x32, 0x01, 0xFF]);
        assert!(is_keyframe(&tu, None).unwrap());
        let parsed = SequenceHeader::parse(&sh).unwrap();
        assert!(is_keyframe(&[0x32, 0x01, 0xFF], Some(&parsed)).unwrap());
        let (found, raw) = SequenceHeader::find_in(&tu).unwrap().unwrap();
        assert_eq!(found, parsed);
        assert_eq!(raw, &tu[..2 + sh.len()]);
        assert_eq!(SequenceHeader::find_in(&[0x12, 0x00]).unwrap(), None);
    }
}
