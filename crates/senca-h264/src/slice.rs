//! スライスヘッダー（H.264 7.3.3）の先頭部分。

use crate::bits::Bits;
use crate::error::{Error, Result};
use crate::nal::{NalHeader, NalUnitType, remove_emulation_prevention};
use crate::sps::Sps;

/// slice_type（7.4.3。5〜9 は 0〜4 と同じ種類）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
    Sp,
    Si,
}

impl SliceType {
    fn from_ue(v: u32) -> Result<SliceType> {
        Ok(match v % 5 {
            0 => SliceType::P,
            1 => SliceType::B,
            2 => SliceType::I,
            3 => SliceType::Sp,
            _ => SliceType::Si,
        })
    }

    /// I・SI（他のピクチャを参照しない）か。
    pub fn is_intra(self) -> bool {
        matches!(self, SliceType::I | SliceType::Si)
    }
}

/// スライスヘッダーの先頭。SPS が分からないときは `frame_num` 以降が `None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceHeader {
    pub nal_unit_type: NalUnitType,
    pub nal_ref_idc: u8,
    pub first_mb_in_slice: u32,
    pub slice_type: SliceType,
    /// slice_type が 5〜9（ピクチャ内のすべてのスライスが同じ種類）。
    pub slice_type_fixed: bool,
    pub pic_parameter_set_id: u8,
    pub colour_plane_id: Option<u8>,
    pub frame_num: Option<u32>,
    pub field_pic: Option<bool>,
    pub bottom_field: Option<bool>,
    pub idr_pic_id: Option<u32>,
    pub pic_order_cnt_lsb: Option<u32>,
}

impl SliceHeader {
    /// スライス NAL（種類 1 または 5）のヘッダーを解析する。
    ///
    /// `sps` は PPS が参照する SPS（呼び出し側が pic_parameter_set_id から引く）。`None` なら pps_id までを読む。
    pub fn parse(nal: &[u8], sps: Option<&Sps>) -> Result<SliceHeader> {
        let h = NalHeader::parse(nal)?;
        if !matches!(
            h.nal_unit_type,
            NalUnitType::NonIdrSlice | NalUnitType::IdrSlice
        ) {
            return Err(Error::Unsupported("種類 1・5 以外のスライス"));
        }
        let idr = h.nal_unit_type == NalUnitType::IdrSlice;
        // 先頭のフィールドだけを読む。emulation prevention の除去は先頭に対しても正しく働く。
        let head = &nal[1..nal.len().min(65)];
        let rbsp = remove_emulation_prevention(head);
        let mut r = Bits::new(&rbsp);
        let first_mb_in_slice = r.ue_max(MAX_MBS, "first_mb_in_slice")?;
        let raw_type = r.ue_max(9, "slice_type")?;
        let slice_type = SliceType::from_ue(raw_type)?;
        if idr && !slice_type.is_intra() {
            return Err(Error::Invalid("IDR の slice_type が I/SI でない"));
        }
        let pic_parameter_set_id = r.ue_max(255, "pic_parameter_set_id")? as u8;
        let mut sh = SliceHeader {
            nal_unit_type: h.nal_unit_type,
            nal_ref_idc: h.nal_ref_idc,
            first_mb_in_slice,
            slice_type,
            slice_type_fixed: raw_type >= 5,
            pic_parameter_set_id,
            colour_plane_id: None,
            frame_num: None,
            field_pic: None,
            bottom_field: None,
            idr_pic_id: None,
            pic_order_cnt_lsb: None,
        };
        let Some(sps) = sps else {
            return Ok(sh);
        };
        if sps.separate_colour_plane {
            sh.colour_plane_id = Some(r.u8(2)?);
        }
        sh.frame_num = Some(r.u(sps.log2_max_frame_num as u32)?);
        if sps.frame_mbs_only {
            sh.field_pic = Some(false);
        } else {
            let field = r.flag()?;
            sh.field_pic = Some(field);
            sh.bottom_field = Some(if field { r.flag()? } else { false });
        }
        if idr {
            sh.idr_pic_id = Some(r.ue_max(65535, "idr_pic_id")?);
        }
        if sps.pic_order_cnt_type == 0 {
            sh.pic_order_cnt_lsb = Some(r.u(sps.log2_max_pic_order_cnt_lsb as u32)?);
        }
        Ok(sh)
    }

    /// 新しいピクチャの最初のスライスか（first_mb_in_slice が 0）。ASO（任意順スライス）は考えない。
    pub fn is_first_slice(&self) -> bool {
        self.first_mb_in_slice == 0
    }
}

/// first_mb_in_slice の上限（2048×2048 マクロブロック）。
const MAX_MBS: u32 = 2048 * 2048;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sps::tests::{SpsSpec, build_sps};
    use senca_core::bits::BitWriter;

    fn slice(nal_byte: u8, first_mb: u32, ty: u32, sps_frame_bits: u32, frame_num: u32) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_ue(first_mb);
        w.write_ue(ty);
        w.write_ue(0);
        w.write_bits(frame_num as u64, sps_frame_bits);
        if nal_byte & 0x1f == 5 {
            w.write_ue(7);
        }
        w.write_bits(10, 6); // pic_order_cnt_lsb（log2 = 6）
        w.write_bits(0xAAAA, 16);
        let mut v = vec![nal_byte];
        v.extend(w.finish());
        v
    }

    #[test]
    fn parse_with_and_without_sps() {
        let sps = Sps::parse(&build_sps(&SpsSpec::default())).unwrap();
        assert_eq!(sps.log2_max_frame_num, 4);
        assert_eq!(sps.log2_max_pic_order_cnt_lsb, 6);
        let nal = slice(0x65, 0, 7, 4, 0);
        let sh = SliceHeader::parse(&nal, Some(&sps)).unwrap();
        assert_eq!(sh.slice_type, SliceType::I);
        assert!(sh.slice_type_fixed);
        assert!(sh.is_first_slice());
        assert_eq!(sh.idr_pic_id, Some(7));
        assert_eq!(sh.frame_num, Some(0));
        assert_eq!(sh.pic_order_cnt_lsb, Some(10));

        let nal = slice(0x41, 30, 1, 4, 9);
        let sh = SliceHeader::parse(&nal, Some(&sps)).unwrap();
        assert_eq!(sh.slice_type, SliceType::B);
        assert_eq!(sh.first_mb_in_slice, 30);
        assert_eq!(sh.frame_num, Some(9));
        assert_eq!(sh.idr_pic_id, None);
        let sh = SliceHeader::parse(&nal, None).unwrap();
        assert_eq!(sh.frame_num, None);
    }

    #[test]
    fn rejects_malformed() {
        assert!(SliceHeader::parse(&slice(0x65, 0, 0, 4, 0), None).is_err()); // IDR で P
        assert!(SliceHeader::parse(&[0x67, 0x80], None).is_err());
        assert_eq!(SliceHeader::parse(&[0x65], None), Err(Error::Truncated));
        assert!(SliceHeader::parse(&[0x41, 0x00, 0x00, 0x00, 0x00, 0x01], None).is_err());
        let sps = Sps::parse(&build_sps(&SpsSpec::default())).unwrap();
        // frame_num の途中で終わる
        assert_eq!(
            SliceHeader::parse(&[0x41, 0b1010_0000], Some(&sps)),
            Err(Error::Truncated)
        );
    }
}
