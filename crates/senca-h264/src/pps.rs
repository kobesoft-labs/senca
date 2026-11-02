//! PPS（pic_parameter_set_rbsp、H.264 7.3.2.2）の先頭部分の解析。

use crate::bits::Bits;
use crate::error::{Error, Result};
use crate::nal::{NalHeader, NalUnitType, remove_emulation_prevention};

/// PPS の先頭部分。スライスヘッダーの解析と SPS との対応付けに使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pps {
    pub pic_parameter_set_id: u8,
    pub seq_parameter_set_id: u8,
    pub entropy_coding_mode_flag: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_slice_groups: u32,
}

impl Pps {
    /// PPS の NAL（ヘッダー付き）を解析する。
    pub fn parse(nal: &[u8]) -> Result<Pps> {
        let h = NalHeader::parse(nal)?;
        if h.nal_unit_type != NalUnitType::Pps {
            return Err(Error::Invalid("PPS ではない NAL"));
        }
        // 先頭の数フィールドしか読まないので、先頭だけを取り出せば足りる
        let head = &nal[1..nal.len().min(33)];
        Self::parse_rbsp(&remove_emulation_prevention(head))
    }

    /// RBSP（先頭部分だけでもよい）を解析する。
    pub fn parse_rbsp(rbsp: &[u8]) -> Result<Pps> {
        let mut r = Bits::new(rbsp);
        let pic_parameter_set_id = r.ue_max(255, "pic_parameter_set_id")? as u8;
        let seq_parameter_set_id = r.ue_max(31, "seq_parameter_set_id")? as u8;
        let entropy_coding_mode_flag = r.flag()?;
        let bottom_field_pic_order_in_frame_present = r.flag()?;
        let num_slice_groups = r.ue_max(7, "num_slice_groups_minus1")? + 1;
        Ok(Pps {
            pic_parameter_set_id,
            seq_parameter_set_id,
            entropy_coding_mode_flag,
            bottom_field_pic_order_in_frame_present,
            num_slice_groups,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use senca_core::bits::BitWriter;

    fn build(pps_id: u32, sps_id: u32) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_ue(pps_id);
        w.write_ue(sps_id);
        w.write_bit(true);
        w.write_bit(false);
        w.write_ue(0);
        w.write_bits(0xFFFF, 16); // 以降のフィールドの代わり
        w.write_bit(true);
        let mut v = vec![0x68];
        v.extend(w.finish());
        v
    }

    #[test]
    fn parse_ids() {
        let p = Pps::parse(&build(3, 1)).unwrap();
        assert_eq!(p.pic_parameter_set_id, 3);
        assert_eq!(p.seq_parameter_set_id, 1);
        assert!(p.entropy_coding_mode_flag);
        assert_eq!(p.num_slice_groups, 1);
        assert!(matches!(Pps::parse(&build(256, 0)), Err(Error::Invalid(_))));
        assert!(matches!(Pps::parse(&build(0, 32)), Err(Error::Invalid(_))));
        assert_eq!(Pps::parse(&[0x68]), Err(Error::Truncated));
        assert!(Pps::parse(&[0x67, 0xFF]).is_err());
    }
}
