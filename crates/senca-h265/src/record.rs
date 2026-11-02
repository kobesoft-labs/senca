//! HEVCDecoderConfigurationRecord（ISO/IEC 14496-15 8.3.3.1）と [`CodecConfig`] の組み立て。

use crate::error::{Error, Result};
use crate::nal::{NalUnitType, nal_unit_type};
use crate::ptl::ProfileTierLevel;
use crate::sps::Sps;
use bytes::Bytes;
use senca_core::media::{Codec, CodecConfig, VideoParams};

/// NAL の配列（同じ種類の NAL をまとめたもの）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NalArray {
    pub array_completeness: bool,
    pub nal_unit_type: NalUnitType,
    pub nalus: Vec<Bytes>,
}

/// HEVCDecoderConfigurationRecord。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcDecoderConfigurationRecord {
    pub ptl: ProfileTierLevel,
    pub min_spatial_segmentation_idc: u16,
    pub parallelism_type: u8,
    pub chroma_format_idc: u8,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    /// フレーム / 256 秒（0 は未指定）。
    pub avg_frame_rate: u16,
    pub constant_frame_rate: u8,
    pub num_temporal_layers: u8,
    pub temporal_id_nested: bool,
    /// NAL 長さのバイト数（1・2・4）。
    pub length_size: u8,
    pub arrays: Vec<NalArray>,
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8> {
        let b = *self.data.get(self.pos).ok_or(Error::Truncated)?;
        self.pos += 1;
        Ok(b)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes([self.u8()?, self.u8()?]))
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(Error::Truncated)?;
        let s = self.data.get(self.pos..end).ok_or(Error::Truncated)?;
        self.pos = end;
        Ok(s)
    }
}

/// 配列の総数・NAL の総数の上限（壊れた入力で大きな確保をしない）。
const MAX_NALUS: usize = 1024;

impl HevcDecoderConfigurationRecord {
    /// バイト列を解析する。
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut r = Reader { data, pos: 0 };
        if r.u8()? != 1 {
            return Err(Error::Invalid("configurationVersion が 1 でない"));
        }
        let b = r.u8()?;
        let compat = u32::from_be_bytes(r.take(4)?.try_into().map_err(|_| Error::Truncated)?);
        let c = r.take(6)?;
        let constraint = c.iter().fold(0u64, |acc, &x| (acc << 8) | x as u64);
        let level = r.u8()?;
        let ptl = ProfileTierLevel {
            general_profile_space: b >> 6,
            general_tier_flag: b & 0x20 != 0,
            general_profile_idc: b & 0x1f,
            general_profile_compatibility_flags: compat,
            general_constraint_indicator_flags: constraint,
            general_level_idc: level,
        };
        let min_spatial_segmentation_idc = r.u16()? & 0x0fff;
        let parallelism_type = r.u8()? & 3;
        let chroma_format_idc = r.u8()? & 3;
        let bit_depth_luma = (r.u8()? & 7) + 8;
        let bit_depth_chroma = (r.u8()? & 7) + 8;
        let avg_frame_rate = r.u16()?;
        let b = r.u8()?;
        let constant_frame_rate = b >> 6;
        let num_temporal_layers = (b >> 3) & 7;
        let temporal_id_nested = b & 4 != 0;
        let length_size = (b & 3) + 1;
        if length_size == 3 {
            return Err(Error::Invalid("lengthSizeMinusOne が 2"));
        }
        let num_arrays = r.u8()? as usize;
        let mut arrays = Vec::with_capacity(num_arrays);
        let mut total = 0usize;
        for _ in 0..num_arrays {
            let b = r.u8()?;
            let ty = NalUnitType::from_u8(b & 0x3f);
            let n = r.u16()? as usize;
            total += n;
            if total > MAX_NALUS {
                return Err(Error::Invalid("NAL が多すぎる"));
            }
            let mut nalus = Vec::with_capacity(n.min(16));
            for _ in 0..n {
                let len = r.u16()? as usize;
                let nal = r.take(len)?;
                if nal_unit_type(nal) != Some(ty) {
                    return Err(Error::Invalid("配列の NAL の種類が違う"));
                }
                nalus.push(Bytes::copy_from_slice(nal));
            }
            arrays.push(NalArray {
                array_completeness: b & 0x80 != 0,
                nal_unit_type: ty,
                nalus,
            });
        }
        Ok(HevcDecoderConfigurationRecord {
            ptl,
            min_spatial_segmentation_idc,
            parallelism_type,
            chroma_format_idc,
            bit_depth_luma,
            bit_depth_chroma,
            avg_frame_rate,
            constant_frame_rate,
            num_temporal_layers,
            temporal_id_nested,
            length_size,
            arrays,
        })
    }

    /// VPS・SPS・PPS（と任意の SEI）の NAL から作る。値は最初の SPS から取る。NAL 長さは 4 バイト。
    ///
    /// `extra` には prefix SEI など配列に入れたい他の NAL を渡す（種類ごとにまとめる）。
    pub fn from_parameter_sets(
        vps: &[&[u8]],
        sps: &[&[u8]],
        pps: &[&[u8]],
        extra: &[&[u8]],
    ) -> Result<Self> {
        let first = sps.first().ok_or(Error::Invalid("SPS がない"))?;
        let s = Sps::parse(first)?;
        if vps.is_empty() || pps.is_empty() {
            return Err(Error::Invalid("VPS・PPS がない"));
        }
        let mut arrays: Vec<NalArray> = Vec::new();
        let groups = [
            (NalUnitType::Vps, vps),
            (NalUnitType::Sps, sps),
            (NalUnitType::Pps, pps),
        ];
        for (ty, list) in groups {
            for n in list {
                if nal_unit_type(n) != Some(ty) {
                    return Err(Error::Invalid("パラメーターセットの NAL の種類が違う"));
                }
            }
            arrays.push(NalArray {
                array_completeness: true,
                nal_unit_type: ty,
                nalus: list.iter().map(|n| Bytes::copy_from_slice(n)).collect(),
            });
        }
        for n in extra {
            let ty = nal_unit_type(n).ok_or(Error::Truncated)?;
            match arrays.iter_mut().find(|a| a.nal_unit_type == ty) {
                Some(a) => a.nalus.push(Bytes::copy_from_slice(n)),
                None => arrays.push(NalArray {
                    array_completeness: true,
                    nal_unit_type: ty,
                    nalus: vec![Bytes::copy_from_slice(n)],
                }),
            }
        }
        Ok(HevcDecoderConfigurationRecord {
            ptl: s.ptl,
            min_spatial_segmentation_idc: s.vui.map_or(0, |v| v.min_spatial_segmentation_idc),
            parallelism_type: 0,
            chroma_format_idc: s.chroma_format_idc,
            bit_depth_luma: s.bit_depth_luma,
            bit_depth_chroma: s.bit_depth_chroma,
            avg_frame_rate: 0,
            constant_frame_rate: 0,
            num_temporal_layers: s.max_sub_layers,
            temporal_id_nested: s.temporal_id_nesting,
            length_size: 4,
            arrays,
        })
    }

    /// バイト列にする。
    pub fn to_bytes(&self) -> Result<Bytes> {
        if !matches!(self.length_size, 1 | 2 | 4) {
            return Err(Error::Invalid("NAL 長さのバイト数は 1・2・4 のどれか"));
        }
        if self.arrays.len() > 255 {
            return Err(Error::Invalid("配列が 255 個を超える"));
        }
        if !(8..=15).contains(&self.bit_depth_luma) || !(8..=15).contains(&self.bit_depth_chroma) {
            return Err(Error::Invalid("ビット深度"));
        }
        let p = &self.ptl;
        let mut out = Vec::with_capacity(64);
        out.push(1);
        out.push(
            ((p.general_profile_space & 3) << 6)
                | ((p.general_tier_flag as u8) << 5)
                | (p.general_profile_idc & 0x1f),
        );
        out.extend_from_slice(&p.general_profile_compatibility_flags.to_be_bytes());
        out.extend_from_slice(&p.constraint_bytes());
        out.push(p.general_level_idc);
        out.extend_from_slice(
            &(0xF000 | (self.min_spatial_segmentation_idc & 0x0fff)).to_be_bytes(),
        );
        out.push(0xFC | (self.parallelism_type & 3));
        out.push(0xFC | (self.chroma_format_idc & 3));
        out.push(0xF8 | (self.bit_depth_luma - 8));
        out.push(0xF8 | (self.bit_depth_chroma - 8));
        out.extend_from_slice(&self.avg_frame_rate.to_be_bytes());
        out.push(
            ((self.constant_frame_rate & 3) << 6)
                | ((self.num_temporal_layers & 7) << 3)
                | ((self.temporal_id_nested as u8) << 2)
                | (self.length_size - 1),
        );
        out.push(self.arrays.len() as u8);
        for a in &self.arrays {
            out.push(((a.array_completeness as u8) << 7) | a.nal_unit_type.as_u8());
            let n = u16::try_from(a.nalus.len()).map_err(|_| Error::Invalid("NAL が多すぎる"))?;
            out.extend_from_slice(&n.to_be_bytes());
            for nal in &a.nalus {
                let len = u16::try_from(nal.len())
                    .map_err(|_| Error::Invalid("NAL が 65535 バイトを超える"))?;
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(nal);
            }
        }
        Ok(Bytes::from(out))
    }

    /// 指定した種類の NAL を順に返す。
    pub fn nalus(&self, ty: NalUnitType) -> impl Iterator<Item = &Bytes> {
        self.arrays
            .iter()
            .filter(move |a| a.nal_unit_type == ty)
            .flat_map(|a| a.nalus.iter())
    }

    /// 最初の SPS を解析する。
    pub fn parse_sps(&self) -> Result<Sps> {
        Sps::parse(
            self.nalus(NalUnitType::Sps)
                .next()
                .ok_or(Error::Invalid("SPS がない"))?,
        )
    }

    /// codecs 文字列（`hvc1.…`）。
    pub fn codecs_string(&self) -> String {
        self.ptl.codecs_string()
    }

    /// [`CodecConfig`] を作る。`VideoParams` は profile = general_profile_idc、
    /// compat = general_tier_flag（0/1）、level = general_level_idc。
    pub fn codec_config(&self, generation: u32) -> Result<CodecConfig> {
        let sps = self.parse_sps()?;
        Ok(CodecConfig {
            codec: Codec::H265,
            generation,
            record: self.to_bytes()?,
            video: Some(VideoParams {
                width: sps.width,
                height: sps.height,
                profile: self.ptl.general_profile_idc,
                compat: self.ptl.general_tier_flag as u8,
                level: self.ptl.general_level_idc,
            }),
            audio: None,
        })
    }
}

/// VPS・SPS・PPS（NAL ヘッダー付き）から [`CodecConfig`] を作る。
pub fn codec_config(vps: &[u8], sps: &[u8], pps: &[u8], generation: u32) -> Result<CodecConfig> {
    HevcDecoderConfigurationRecord::from_parameter_sets(&[vps], &[sps], &[pps], &[])?
        .codec_config(generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sps::tests::{SpsSpec, build_sps, build_vps};

    const PPS: [u8; 7] = [0x44, 0x01, 0xC1, 0x72, 0xB4, 0x62, 0x40];

    #[test]
    fn roundtrip() {
        let spec = SpsSpec::default();
        let vps = build_vps(&spec);
        let sps = build_sps(&spec);
        let sei = [0x4E, 0x01, 0x05, 0x01, 0x00, 0x80];
        let rec =
            HevcDecoderConfigurationRecord::from_parameter_sets(&[&vps], &[&sps], &[&PPS], &[&sei])
                .unwrap();
        assert_eq!(rec.arrays.len(), 4);
        assert_eq!(rec.min_spatial_segmentation_idc, 12);
        let b = rec.to_bytes().unwrap();
        assert_eq!(b[0], 1);
        assert_eq!(b[1], 0x01);
        assert_eq!(&b[2..6], &[0x60, 0, 0, 0]);
        assert_eq!(&b[6..12], &[0x90, 0, 0, 0, 0, 0]);
        assert_eq!(b[12], 93);
        assert_eq!(&b[13..15], &[0xF0, 12]);
        assert_eq!(b[21], 0x0F);
        let back = HevcDecoderConfigurationRecord::parse(&b).unwrap();
        assert_eq!(back, rec);
        assert_eq!(back.codecs_string(), "hvc1.1.6.L93.90");
        let cc = back.codec_config(7).unwrap();
        let v = cc.video.unwrap();
        assert_eq!(
            (v.width, v.height, v.profile, v.compat, v.level),
            (1920, 1080, 1, 0, 93)
        );
        assert_eq!(cc.codec, Codec::H265);
        assert_eq!(codec_config(&vps, &sps, &PPS, 0).unwrap().video, cc.video);
        assert_eq!(back.nalus(NalUnitType::PrefixSei).count(), 1);
    }

    #[test]
    fn rejects_malformed() {
        let spec = SpsSpec::default();
        let vps = build_vps(&spec);
        let sps = build_sps(&spec);
        let rec =
            HevcDecoderConfigurationRecord::from_parameter_sets(&[&vps], &[&sps], &[&PPS], &[])
                .unwrap();
        let b = rec.to_bytes().unwrap();
        for n in 0..b.len() {
            assert!(
                HevcDecoderConfigurationRecord::parse(&b[..n]).is_err(),
                "len {n}"
            );
        }
        let mut bad = b.to_vec();
        bad[0] = 2;
        assert!(HevcDecoderConfigurationRecord::parse(&bad).is_err());
        let mut bad = b.to_vec();
        bad[21] = 0x0E;
        assert!(HevcDecoderConfigurationRecord::parse(&bad).is_err());
        let mut bad = b.to_vec();
        bad[23] = 0xA1; // VPS の配列に SPS の種類
        assert!(HevcDecoderConfigurationRecord::parse(&bad).is_err());
        assert!(
            HevcDecoderConfigurationRecord::from_parameter_sets(&[], &[&sps], &[&PPS], &[])
                .is_err()
        );
        assert!(
            HevcDecoderConfigurationRecord::from_parameter_sets(&[&vps], &[], &[&PPS], &[])
                .is_err()
        );
        assert!(
            HevcDecoderConfigurationRecord::from_parameter_sets(&[&sps], &[&sps], &[&PPS], &[])
                .is_err()
        );
        let mut r = rec.clone();
        r.length_size = 3;
        assert!(r.to_bytes().is_err());
        let mut r = rec;
        r.arrays[0].nalus.push(Bytes::from(vec![0x40; 70000]));
        assert!(r.to_bytes().is_err());
        // NAL 数が多すぎる
        let mut big = b[..22].to_vec();
        big.push(1);
        big.extend([0xA0, 0xFF, 0xFF]);
        assert!(matches!(
            HevcDecoderConfigurationRecord::parse(&big),
            Err(Error::Invalid(_))
        ));
    }
}
