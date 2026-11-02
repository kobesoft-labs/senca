//! NAL ユニットのヘッダーと emulation prevention。

use crate::annexb::{annexb_nals, length_prefixed_nals};
use crate::error::{Error, Result};
use std::borrow::Cow;

/// nal_unit_type（H.264 表 7-1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NalUnitType {
    /// 1: IDR でないピクチャのスライス。
    NonIdrSlice,
    /// 2: データ分割 A。
    SliceDataA,
    /// 3: データ分割 B。
    SliceDataB,
    /// 4: データ分割 C。
    SliceDataC,
    /// 5: IDR ピクチャのスライス。
    IdrSlice,
    /// 6
    Sei,
    /// 7
    Sps,
    /// 8
    Pps,
    /// 9: アクセスユニット区切り。
    Aud,
    /// 10
    EndOfSequence,
    /// 11
    EndOfStream,
    /// 12
    FillerData,
    /// 13
    SpsExtension,
    /// 14
    PrefixNal,
    /// 15
    SubsetSps,
    /// 16
    DepthParameterSet,
    /// 19: 補助ピクチャのスライス。
    AuxiliarySlice,
    /// 20: SVC/MVC の拡張スライス。
    SliceExtension,
    /// 21: 3D-AVC の深度スライス。
    SliceExtensionDepth,
    /// 上記以外（0・17・18・22〜31）。
    Other(u8),
}

impl NalUnitType {
    /// 下位 5 ビットを nal_unit_type として解釈する。
    pub fn from_u8(v: u8) -> Self {
        match v & 0x1f {
            1 => Self::NonIdrSlice,
            2 => Self::SliceDataA,
            3 => Self::SliceDataB,
            4 => Self::SliceDataC,
            5 => Self::IdrSlice,
            6 => Self::Sei,
            7 => Self::Sps,
            8 => Self::Pps,
            9 => Self::Aud,
            10 => Self::EndOfSequence,
            11 => Self::EndOfStream,
            12 => Self::FillerData,
            13 => Self::SpsExtension,
            14 => Self::PrefixNal,
            15 => Self::SubsetSps,
            16 => Self::DepthParameterSet,
            19 => Self::AuxiliarySlice,
            20 => Self::SliceExtension,
            21 => Self::SliceExtensionDepth,
            v => Self::Other(v),
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::NonIdrSlice => 1,
            Self::SliceDataA => 2,
            Self::SliceDataB => 3,
            Self::SliceDataC => 4,
            Self::IdrSlice => 5,
            Self::Sei => 6,
            Self::Sps => 7,
            Self::Pps => 8,
            Self::Aud => 9,
            Self::EndOfSequence => 10,
            Self::EndOfStream => 11,
            Self::FillerData => 12,
            Self::SpsExtension => 13,
            Self::PrefixNal => 14,
            Self::SubsetSps => 15,
            Self::DepthParameterSet => 16,
            Self::AuxiliarySlice => 19,
            Self::SliceExtension => 20,
            Self::SliceExtensionDepth => 21,
            Self::Other(v) => v & 0x1f,
        }
    }

    /// 主ピクチャの VCL NAL（1〜5）か。
    pub fn is_vcl(self) -> bool {
        matches!(self.as_u8(), 1..=5)
    }

    /// SPS・PPS・SPS 拡張・subset SPS か。
    pub fn is_parameter_set(self) -> bool {
        matches!(
            self,
            Self::Sps | Self::Pps | Self::SpsExtension | Self::SubsetSps
        )
    }
}

/// NAL ヘッダー（1 バイト）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalHeader {
    pub nal_ref_idc: u8,
    pub nal_unit_type: NalUnitType,
}

impl NalHeader {
    /// NAL の先頭バイトを読む。forbidden_zero_bit が 1 なら `Err`。
    pub fn parse(nal: &[u8]) -> Result<NalHeader> {
        let b = *nal.first().ok_or(Error::Truncated)?;
        if b & 0x80 != 0 {
            return Err(Error::Invalid("forbidden_zero_bit"));
        }
        Ok(NalHeader {
            nal_ref_idc: (b >> 5) & 3,
            nal_unit_type: NalUnitType::from_u8(b),
        })
    }

    pub fn to_byte(self) -> u8 {
        ((self.nal_ref_idc & 3) << 5) | self.nal_unit_type.as_u8()
    }
}

/// NAL の種類だけを素早く取る（空なら `None`）。
pub fn nal_unit_type(nal: &[u8]) -> Option<NalUnitType> {
    nal.first().map(|&b| NalUnitType::from_u8(b))
}

/// emulation_prevention_three_byte（`00 00 03` の `03`）を取り除く。含まれていなければコピーしない。
pub fn remove_emulation_prevention(data: &[u8]) -> Cow<'_, [u8]> {
    let has = data.windows(3).any(|w| w == [0, 0, 3]);
    if !has {
        return Cow::Borrowed(data);
    }
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0usize;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    Cow::Owned(out)
}

/// RBSP に emulation_prevention_three_byte を入れて NAL のペイロードにする（7.4.1）。
pub fn add_emulation_prevention(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64 + 1);
    let mut zeros = 0usize;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    if rbsp.last() == Some(&0) {
        out.push(3);
    }
    out
}

/// NAL 列に IDR スライスが含まれるか。
pub fn contains_idr<'a, I>(nals: I) -> bool
where
    I: IntoIterator<Item = &'a [u8]>,
{
    nals.into_iter()
        .any(|n| nal_unit_type(n) == Some(NalUnitType::IdrSlice))
}

/// 長さ前置の AU（ADR-0003 の payload）がキーフレーム（IDR を含む）か。
pub fn is_keyframe_length_prefixed(au: &[u8], length_size: u8) -> Result<bool> {
    for nal in length_prefixed_nals(au, length_size)? {
        if nal_unit_type(nal?) == Some(NalUnitType::IdrSlice) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Annex B の AU がキーフレーム（IDR を含む）か。
pub fn is_keyframe_annexb(au: &[u8]) -> bool {
    contains_idr(annexb_nals(au))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_types_roundtrip() {
        for v in 0u8..32 {
            assert_eq!(NalUnitType::from_u8(v).as_u8(), v);
        }
        assert!(NalUnitType::IdrSlice.is_vcl());
        assert!(!NalUnitType::Sei.is_vcl());
        assert!(NalUnitType::Pps.is_parameter_set());
    }

    #[test]
    fn header() {
        let h = NalHeader::parse(&[0x67]).unwrap();
        assert_eq!(h.nal_ref_idc, 3);
        assert_eq!(h.nal_unit_type, NalUnitType::Sps);
        assert_eq!(h.to_byte(), 0x67);
        assert_eq!(
            NalHeader::parse(&[0xE7]),
            Err(Error::Invalid("forbidden_zero_bit"))
        );
        assert_eq!(NalHeader::parse(&[]), Err(Error::Truncated));
        assert_eq!(nal_unit_type(&[]), None);
    }

    #[test]
    fn emulation_prevention() {
        let rbsp = [0, 0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 4, 0, 0];
        let nal = add_emulation_prevention(&rbsp);
        assert_eq!(
            nal,
            vec![
                0, 0, 3, 0, 0, 3, 0, 1, 0, 0, 3, 2, 0, 0, 3, 3, 0, 0, 4, 0, 0, 3
            ]
        );
        assert!(!nal.windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2));
        assert_eq!(&*remove_emulation_prevention(&nal), &rbsp[..]);
        assert!(matches!(
            remove_emulation_prevention(&[1, 2, 0, 0, 4]),
            Cow::Borrowed(_)
        ));
        assert_eq!(add_emulation_prevention(&[]), Vec::<u8>::new());
    }

    #[test]
    fn keyframe_helpers() {
        let au = [0, 0, 0, 2, 0x09, 0xF0, 0, 0, 0, 1, 0x65];
        assert!(is_keyframe_length_prefixed(&au, 4).unwrap());
        let au = [0, 0, 0, 2, 0x09, 0xF0, 0, 0, 0, 1, 0x41];
        assert!(!is_keyframe_length_prefixed(&au, 4).unwrap());
        assert!(is_keyframe_length_prefixed(&[0, 0, 0, 5, 0x65], 4).is_err());
        assert!(is_keyframe_annexb(&[0, 0, 1, 0x67, 0, 0, 1, 0x65, 0x88]));
        assert!(!is_keyframe_annexb(&[0, 0, 1, 0x41, 0x88]));
    }
}
