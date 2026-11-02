//! NAL ユニットのヘッダー（2 バイト）と emulation prevention、IRAP の判定。

use crate::annexb::{annexb_nals, length_prefixed_nals};
use crate::error::{Error, Result};
use std::borrow::Cow;

/// nal_unit_type（H.265 表 7-1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NalUnitType {
    TrailN,
    TrailR,
    TsaN,
    TsaR,
    StsaN,
    StsaR,
    RadlN,
    RadlR,
    RaslN,
    RaslR,
    BlaWLp,
    BlaWRadl,
    BlaNLp,
    IdrWRadl,
    IdrNLp,
    CraNut,
    /// 32
    Vps,
    /// 33
    Sps,
    /// 34
    Pps,
    /// 35: アクセスユニット区切り。
    Aud,
    /// 36
    EndOfSequence,
    /// 37
    EndOfBitstream,
    /// 38
    FillerData,
    /// 39
    PrefixSei,
    /// 40
    SuffixSei,
    /// 上記以外（予約・未指定）。
    Other(u8),
}

impl NalUnitType {
    /// 6 ビットの値から作る（上位ビットは無視）。
    pub fn from_u8(v: u8) -> Self {
        match v & 0x3f {
            0 => Self::TrailN,
            1 => Self::TrailR,
            2 => Self::TsaN,
            3 => Self::TsaR,
            4 => Self::StsaN,
            5 => Self::StsaR,
            6 => Self::RadlN,
            7 => Self::RadlR,
            8 => Self::RaslN,
            9 => Self::RaslR,
            16 => Self::BlaWLp,
            17 => Self::BlaWRadl,
            18 => Self::BlaNLp,
            19 => Self::IdrWRadl,
            20 => Self::IdrNLp,
            21 => Self::CraNut,
            32 => Self::Vps,
            33 => Self::Sps,
            34 => Self::Pps,
            35 => Self::Aud,
            36 => Self::EndOfSequence,
            37 => Self::EndOfBitstream,
            38 => Self::FillerData,
            39 => Self::PrefixSei,
            40 => Self::SuffixSei,
            v => Self::Other(v),
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::TrailN => 0,
            Self::TrailR => 1,
            Self::TsaN => 2,
            Self::TsaR => 3,
            Self::StsaN => 4,
            Self::StsaR => 5,
            Self::RadlN => 6,
            Self::RadlR => 7,
            Self::RaslN => 8,
            Self::RaslR => 9,
            Self::BlaWLp => 16,
            Self::BlaWRadl => 17,
            Self::BlaNLp => 18,
            Self::IdrWRadl => 19,
            Self::IdrNLp => 20,
            Self::CraNut => 21,
            Self::Vps => 32,
            Self::Sps => 33,
            Self::Pps => 34,
            Self::Aud => 35,
            Self::EndOfSequence => 36,
            Self::EndOfBitstream => 37,
            Self::FillerData => 38,
            Self::PrefixSei => 39,
            Self::SuffixSei => 40,
            Self::Other(v) => v & 0x3f,
        }
    }

    /// VCL NAL（0〜31）か。
    pub fn is_vcl(self) -> bool {
        self.as_u8() < 32
    }

    /// IRAP（16〜23。BLA・IDR・CRA と予約の IRAP）か。ランダムアクセスできる位置。
    pub fn is_irap(self) -> bool {
        (16..=23).contains(&self.as_u8())
    }

    /// IDR（19・20）か。
    pub fn is_idr(self) -> bool {
        matches!(self, Self::IdrWRadl | Self::IdrNLp)
    }

    /// VPS・SPS・PPS か。
    pub fn is_parameter_set(self) -> bool {
        matches!(self, Self::Vps | Self::Sps | Self::Pps)
    }
}

/// NAL ヘッダー（2 バイト）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalHeader {
    pub nal_unit_type: NalUnitType,
    pub nuh_layer_id: u8,
    /// TemporalId（nuh_temporal_id_plus1 - 1）。
    pub temporal_id: u8,
}

impl NalHeader {
    /// 先頭 2 バイトを読む。forbidden_zero_bit が 1、nuh_temporal_id_plus1 が 0 なら `Err`。
    pub fn parse(nal: &[u8]) -> Result<NalHeader> {
        let [a, b, ..] = *nal else {
            return Err(Error::Truncated);
        };
        if a & 0x80 != 0 {
            return Err(Error::Invalid("forbidden_zero_bit"));
        }
        let tid_plus1 = b & 7;
        if tid_plus1 == 0 {
            return Err(Error::Invalid("nuh_temporal_id_plus1 が 0"));
        }
        Ok(NalHeader {
            nal_unit_type: NalUnitType::from_u8(a >> 1),
            nuh_layer_id: ((a & 1) << 5) | (b >> 3),
            temporal_id: tid_plus1 - 1,
        })
    }

    pub fn to_bytes(self) -> [u8; 2] {
        let t = self.nal_unit_type.as_u8();
        let l = self.nuh_layer_id & 0x3f;
        [
            (t << 1) | (l >> 5),
            ((l & 0x1f) << 3) | (self.temporal_id.wrapping_add(1) & 7),
        ]
    }
}

/// NAL の種類だけを素早く取る（2 バイト未満なら `None`）。
pub fn nal_unit_type(nal: &[u8]) -> Option<NalUnitType> {
    if nal.len() < 2 {
        return None;
    }
    Some(NalUnitType::from_u8(nal[0] >> 1))
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

/// RBSP に emulation_prevention_three_byte を入れる（7.4.2）。
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

/// NAL 列に IRAP（BLA・IDR・CRA）の VCL NAL が含まれるか。
pub fn contains_irap<'a, I>(nals: I) -> bool
where
    I: IntoIterator<Item = &'a [u8]>,
{
    nals.into_iter()
        .any(|n| nal_unit_type(n).is_some_and(NalUnitType::is_irap))
}

/// 長さ前置の AU がキーフレーム（IRAP を含む）か。
pub fn is_keyframe_length_prefixed(au: &[u8], length_size: u8) -> Result<bool> {
    for nal in length_prefixed_nals(au, length_size)? {
        if nal_unit_type(nal?).is_some_and(NalUnitType::is_irap) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Annex B の AU がキーフレーム（IRAP を含む）か。
pub fn is_keyframe_annexb(au: &[u8]) -> bool {
    contains_irap(annexb_nals(au))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_roundtrip() {
        for v in 0u8..64 {
            assert_eq!(NalUnitType::from_u8(v).as_u8(), v);
        }
        assert!(NalUnitType::CraNut.is_irap());
        assert!(NalUnitType::BlaWLp.is_irap());
        assert!(NalUnitType::Other(22).is_irap());
        assert!(!NalUnitType::RaslN.is_irap());
        assert!(NalUnitType::IdrNLp.is_idr());
        assert!(!NalUnitType::CraNut.is_idr());
        assert!(NalUnitType::TrailR.is_vcl());
        assert!(!NalUnitType::Vps.is_vcl());
    }

    #[test]
    fn header() {
        let h = NalHeader::parse(&[0x40, 0x01]).unwrap();
        assert_eq!(h.nal_unit_type, NalUnitType::Vps);
        assert_eq!((h.nuh_layer_id, h.temporal_id), (0, 0));
        assert_eq!(h.to_bytes(), [0x40, 0x01]);
        let h = NalHeader {
            nal_unit_type: NalUnitType::TrailR,
            nuh_layer_id: 63,
            temporal_id: 6,
        };
        assert_eq!(NalHeader::parse(&h.to_bytes()).unwrap(), h);
        assert_eq!(NalHeader::parse(&[0x40]), Err(Error::Truncated));
        assert!(NalHeader::parse(&[0xC0, 0x01]).is_err());
        assert!(NalHeader::parse(&[0x40, 0x00]).is_err());
        assert_eq!(nal_unit_type(&[0x26]), None);
    }

    #[test]
    fn emulation_prevention_roundtrip() {
        let rbsp = [0u8, 0, 1, 0, 0, 0, 0, 0, 2, 0xFF, 0, 0];
        let nal = add_emulation_prevention(&rbsp);
        assert_eq!(&*remove_emulation_prevention(&nal), &rbsp[..]);
        assert!(matches!(
            remove_emulation_prevention(&[1, 0, 0, 4]),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn keyframe() {
        // IDR_W_RADL = 19 → 0x26 0x01、TRAIL_R = 1 → 0x02 0x01
        assert!(is_keyframe_length_prefixed(&[0, 0, 0, 3, 0x26, 0x01, 0xAF], 4).unwrap());
        assert!(!is_keyframe_length_prefixed(&[0, 0, 0, 3, 0x02, 0x01, 0xAF], 4).unwrap());
        assert!(is_keyframe_length_prefixed(&[0, 0, 0, 3, 0x02], 4).is_err());
        assert!(is_keyframe_annexb(&[0, 0, 1, 0x2A, 0x01, 0xAF])); // CRA
        assert!(!is_keyframe_annexb(&[0, 0, 1, 0x02, 0x01, 0xAF]));
    }
}
