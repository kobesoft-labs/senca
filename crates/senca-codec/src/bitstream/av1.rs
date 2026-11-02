//! AV1 の OBU（Low Overhead Bitstream Format）と AV1CodecConfigurationRecord（av1C）。
//!
//! 内部形式（ADR-0003）は「OBU 列（Temporal Delimiter を除く、各 OBU は obu_size 付き）」。

use crate::error::{CodecError, Result};
use bytes::Bytes;
use senca_core::bits::BitReader;
use senca_core::media::VideoParams;

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_FRAME_HEADER: u8 = 3;
pub const OBU_FRAME: u8 = 6;
pub const OBU_PADDING: u8 = 15;

/// 1 AU の OBU 数の上限。
pub const MAX_OBUS: usize = 4096;

/// OBU 1 つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obu<'a> {
    pub obu_type: u8,
    /// ヘッダー（拡張を含む）から末尾までの全体。
    pub raw: &'a [u8],
    /// ペイロード。
    pub payload: &'a [u8],
    pub temporal_id: u8,
    pub spatial_id: u8,
}

fn read_leb128(d: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for i in 0..8 {
        let b = *d.get(i)?;
        v |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

fn write_leb128(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// OBU 列を分ける。`obu_has_size_field` が 0 の OBU は最後の 1 つだけ許す（残り全部をペイロードとみなす）。
pub fn parse_obus(data: &[u8]) -> Result<Vec<Obu<'_>>> {
    let bad = |m: &str| CodecError::invalid_input(format!("malformed AV1 OBU: {m}"));
    let mut out = Vec::new();
    let mut p = 0;
    while p < data.len() {
        let h = data[p];
        if h & 0x80 != 0 {
            return Err(bad("forbidden bit"));
        }
        let obu_type = (h >> 3) & 0x0f;
        let ext = h & 0x04 != 0;
        let has_size = h & 0x02 != 0;
        let mut q = p + 1;
        let (mut tid, mut sid) = (0, 0);
        if ext {
            let e = *data.get(q).ok_or_else(|| bad("truncated extension"))?;
            tid = e >> 5;
            sid = (e >> 3) & 3;
            q += 1;
        }
        let size = if has_size {
            let (v, n) = read_leb128(&data[q.min(data.len())..]).ok_or_else(|| bad("bad size"))?;
            q += n;
            usize::try_from(v).map_err(|_| bad("size overflow"))?
        } else {
            data.len() - q.min(data.len())
        };
        if q > data.len() || size > data.len() - q {
            return Err(bad("truncated payload"));
        }
        out.push(Obu {
            obu_type,
            raw: &data[p..q + size],
            payload: &data[q..q + size],
            temporal_id: tid,
            spatial_id: sid,
        });
        p = q + size;
        if out.len() > MAX_OBUS {
            return Err(bad("too many OBUs"));
        }
    }
    Ok(out)
}

/// Temporal Delimiter（と Padding）を取り除き、どの OBU も obu_size 付きの形にそろえる。
pub fn to_internal(data: &[u8]) -> Result<Vec<u8>> {
    let obus = parse_obus(data)?;
    let mut out = Vec::with_capacity(data.len());
    for o in obus {
        if o.obu_type == OBU_TEMPORAL_DELIMITER || o.obu_type == OBU_PADDING {
            continue;
        }
        let header_len = o.raw.len() - o.payload.len();
        let has_size = o.raw[0] & 0x02 != 0;
        if has_size {
            out.extend_from_slice(o.raw);
        } else {
            let ext = o.raw[0] & 0x04 != 0;
            out.push(o.raw[0] | 0x02);
            if ext {
                out.push(o.raw[1]);
            }
            let _ = header_len;
            write_leb128(&mut out, o.payload.len() as u64);
            out.extend_from_slice(o.payload);
        }
    }
    Ok(out)
}

/// 内部形式の先頭に Temporal Delimiter を付ける（デコーダーが要求する場合に使う）。
pub fn with_temporal_delimiter(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 2);
    out.extend_from_slice(&[0x12, 0x00]);
    out.extend_from_slice(data);
    out
}

/// シーケンスヘッダーから分かること（av1C を作るのに要る分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceHeader {
    pub seq_profile: u8,
    pub seq_level_idx_0: u8,
    pub seq_tier_0: bool,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub monochrome: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
    pub max_width: u32,
    pub max_height: u32,
}

impl SequenceHeader {
    pub fn bit_depth(&self) -> u8 {
        if self.twelve_bit {
            12
        } else if self.high_bitdepth {
            10
        } else {
            8
        }
    }
}

fn uvlc(r: &mut BitReader) -> Option<u32> {
    let mut lz = 0;
    while !r.read_bit()? {
        lz += 1;
        if lz >= 32 {
            return None;
        }
    }
    let v = r.read_bits(lz)? as u32;
    Some(v + ((1u32 << lz) - 1))
}

/// sequence_header_obu のペイロードを解析する（AV1 仕様 5.5）。
pub fn parse_sequence_header(payload: &[u8]) -> Result<SequenceHeader> {
    let mut r = BitReader::new(payload);
    (|| -> Option<SequenceHeader> {
        let seq_profile = r.read_u8(3)?;
        if seq_profile > 2 {
            return None;
        }
        r.read_bit()?; // still_picture
        let reduced = r.read_bit()?;
        let seq_level_idx_0;
        let mut seq_tier_0 = false;
        if reduced {
            seq_level_idx_0 = r.read_u8(5)?;
        } else {
            let timing_info_present = r.read_bit()?;
            let mut decoder_model_info_present = false;
            let mut buffer_delay_length = 0;
            if timing_info_present {
                r.skip(64)?;
                if r.read_bit()? {
                    uvlc(&mut r)?;
                }
                decoder_model_info_present = r.read_bit()?;
                if decoder_model_info_present {
                    buffer_delay_length = r.read_u32(5)? + 1;
                    r.skip(32)?;
                    r.skip(10)?;
                }
            }
            let initial_display_delay_present = r.read_bit()?;
            let cnt = r.read_u8(5)? as usize + 1;
            let mut first = None;
            for _ in 0..cnt {
                r.skip(12)?; // operating_point_idc
                let lvl = r.read_u8(5)?;
                let tier = if lvl > 7 { r.read_bit()? } else { false };
                if first.is_none() {
                    first = Some((lvl, tier));
                }
                if decoder_model_info_present && r.read_bit()? {
                    r.skip(2 * buffer_delay_length as usize + 1)?;
                }
                if initial_display_delay_present && r.read_bit()? {
                    r.skip(4)?;
                }
            }
            let (l, t) = first?;
            seq_level_idx_0 = l;
            seq_tier_0 = t;
        }
        let wb = r.read_u32(4)? + 1;
        let hb = r.read_u32(4)? + 1;
        let max_width = r.read_u32(wb)? + 1;
        let max_height = r.read_u32(hb)? + 1;
        if !reduced && r.read_bit()? {
            r.skip(7)?; // delta_frame_id_length_minus_2, additional_frame_id_length_minus_1
        }
        r.skip(3)?; // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
        if !reduced {
            r.skip(4)?; // interintra, masked, warped, dual_filter
            let order_hint = r.read_bit()?;
            if order_hint {
                r.skip(2)?;
            }
            let force_sct = if r.read_bit()? { 2 } else { r.read_u8(1)? };
            if force_sct > 0 && !r.read_bit()? {
                r.skip(1)?;
            }
            if order_hint {
                r.skip(3)?;
            }
        }
        r.skip(3)?; // superres, cdef, restoration
        // color_config
        let high_bitdepth = r.read_bit()?;
        let twelve_bit = seq_profile == 2 && high_bitdepth && r.read_bit()?;
        let monochrome = seq_profile != 1 && r.read_bit()?;
        let (mut cp, mut tc, mut mc) = (2, 2, 2);
        if r.read_bit()? {
            cp = r.read_u8(8)?;
            tc = r.read_u8(8)?;
            mc = r.read_u8(8)?;
        }
        let (ssx, ssy, mut csp) = if monochrome {
            r.skip(1)?;
            (true, true, 0)
        } else if cp == 1 && tc == 13 && mc == 0 {
            (false, false, 0)
        } else {
            r.skip(1)?; // color_range
            let (x, y) = match seq_profile {
                0 => (true, true),
                1 => (false, false),
                _ => {
                    if twelve_bit {
                        let x = r.read_bit()?;
                        let y = if x { r.read_bit()? } else { false };
                        (x, y)
                    } else {
                        (true, false)
                    }
                }
            };
            (x, y, 0)
        };
        if !monochrome && ssx && ssy {
            csp = r.read_u8(2)?;
        }
        Some(SequenceHeader {
            seq_profile,
            seq_level_idx_0,
            seq_tier_0,
            high_bitdepth,
            twelve_bit,
            monochrome,
            subsampling_x: ssx,
            subsampling_y: ssy,
            chroma_sample_position: csp,
            max_width,
            max_height,
        })
    })()
    .ok_or_else(|| CodecError::invalid_input("malformed AV1 sequence header"))
}

/// OBU 列からシーケンスヘッダー OBU（全体）を探す。
pub fn find_sequence_header(data: &[u8]) -> Result<Option<&[u8]>> {
    Ok(parse_obus(data)?
        .into_iter()
        .find(|o| o.obu_type == OBU_SEQUENCE_HEADER)
        .map(|o| o.raw))
}

/// シーケンスヘッダー OBU（obu_size 付き）から av1C を作る。
pub fn build_av1c(seq_obu: &[u8]) -> Result<(Bytes, SequenceHeader)> {
    let obus = parse_obus(seq_obu)?;
    let o = obus
        .first()
        .filter(|o| o.obu_type == OBU_SEQUENCE_HEADER)
        .ok_or_else(|| CodecError::invalid_input("not a sequence header OBU"))?;
    let sh = parse_sequence_header(o.payload)?;
    let mut out = vec![
        0x81,
        (sh.seq_profile << 5) | (sh.seq_level_idx_0 & 0x1f),
        ((sh.seq_tier_0 as u8) << 7)
            | ((sh.high_bitdepth as u8) << 6)
            | ((sh.twelve_bit as u8) << 5)
            | ((sh.monochrome as u8) << 4)
            | ((sh.subsampling_x as u8) << 3)
            | ((sh.subsampling_y as u8) << 2)
            | (sh.chroma_sample_position & 3),
        0,
    ];
    let internal = to_internal(o.raw)?;
    out.extend_from_slice(&internal);
    Ok((Bytes::from(out), sh))
}

/// av1C から configOBUs（シーケンスヘッダー）を取り出す。
pub fn parse_av1c(rec: &[u8]) -> Result<&[u8]> {
    if rec.len() < 4 || rec[0] != 0x81 {
        return Err(CodecError::invalid_input("malformed av1C"));
    }
    parse_obus(&rec[4..])?;
    Ok(&rec[4..])
}

pub fn video_params(sh: &SequenceHeader) -> VideoParams {
    VideoParams {
        width: sh.max_width,
        height: sh.max_height,
        profile: sh.seq_profile,
        compat: sh.seq_tier_0 as u8,
        level: sh.seq_level_idx_0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb128() {
        let mut v = Vec::new();
        write_leb128(&mut v, 300);
        assert_eq!(v, vec![0xac, 0x02]);
        assert_eq!(read_leb128(&v), Some((300, 2)));
        assert_eq!(read_leb128(&[0x80; 9]), None);
    }

    #[test]
    fn strips_td_and_adds_sizes() {
        // TD（size 0）、size 無しの padding ではなく frame（type 6）
        let d = [0x12, 0x00, 0x30, 0xaa, 0xbb];
        let out = to_internal(&d).unwrap();
        assert_eq!(out, vec![0x32, 0x02, 0xaa, 0xbb]);
        assert!(parse_obus(&[0x32, 0x05, 1]).is_err());
        assert!(parse_obus(&[0x80]).is_err());
        assert_eq!(with_temporal_delimiter(&out)[..2], [0x12, 0]);
    }
}
