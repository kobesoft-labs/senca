//! AVCDecoderConfigurationRecord（ISO/IEC 14496-15 5.3.3.1）と [`CodecConfig`] の組み立て。

use crate::error::{Error, Result};
use crate::nal::{NalUnitType, nal_unit_type};
use crate::sps::Sps;
use bytes::Bytes;
use senca_core::media::{Codec, CodecConfig, VideoParams};

/// High 系プロファイルの拡張部分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcConfigExt {
    pub chroma_format: u8,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub sps_ext: Vec<Bytes>,
}

/// AVCDecoderConfigurationRecord。パラメーターセットは NAL ヘッダー付きで持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcDecoderConfigurationRecord {
    pub profile_indication: u8,
    pub profile_compatibility: u8,
    pub level_indication: u8,
    /// NAL 長さのバイト数（1・2・4）。
    pub length_size: u8,
    pub sps: Vec<Bytes>,
    pub pps: Vec<Bytes>,
    /// High 系（profile_idc が 66・77・88 以外）のときの拡張。古い書き手は省くことがある。
    pub ext: Option<AvcConfigExt>,
}

/// 組み立て時に拡張部分を書くプロファイル（Baseline・Main・Extended 以外。FFmpeg と同じ）。
fn writes_ext(profile: u8) -> bool {
    !matches!(profile, 66 | 77 | 88)
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
    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
    fn nal_list(&mut self, count: usize, ty: Option<NalUnitType>) -> Result<Vec<Bytes>> {
        let mut v = Vec::with_capacity(count);
        for _ in 0..count {
            let len = self.u16()? as usize;
            if len == 0 {
                return Err(Error::Invalid("長さ 0 のパラメーターセット"));
            }
            let nal = self.take(len)?;
            if let Some(ty) = ty
                && nal_unit_type(nal) != Some(ty)
            {
                return Err(Error::Invalid("パラメーターセットの NAL の種類が違う"));
            }
            v.push(Bytes::copy_from_slice(nal));
        }
        Ok(v)
    }
}

fn write_nal_list(out: &mut Vec<u8>, nals: &[Bytes]) -> Result<()> {
    for n in nals {
        let len = u16::try_from(n.len())
            .map_err(|_| Error::Invalid("パラメーターセットが 65535 バイトを超える"))?;
        if len == 0 {
            return Err(Error::Invalid("長さ 0 のパラメーターセット"));
        }
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(n);
    }
    Ok(())
}

impl AvcDecoderConfigurationRecord {
    /// バイト列を解析する。
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut r = Reader { data, pos: 0 };
        if r.u8()? != 1 {
            return Err(Error::Invalid("configurationVersion が 1 でない"));
        }
        let profile_indication = r.u8()?;
        let profile_compatibility = r.u8()?;
        let level_indication = r.u8()?;
        let length_size = (r.u8()? & 3) + 1;
        if length_size == 3 {
            return Err(Error::Invalid("lengthSizeMinusOne が 2"));
        }
        let num_sps = (r.u8()? & 0x1f) as usize;
        let sps = r.nal_list(num_sps, Some(NalUnitType::Sps))?;
        let num_pps = r.u8()? as usize;
        let pps = r.nal_list(num_pps, Some(NalUnitType::Pps))?;
        let ext = if !matches!(profile_indication, 66 | 77 | 88) && r.remaining() >= 4 {
            let chroma_format = r.u8()? & 3;
            let bit_depth_luma = (r.u8()? & 7) + 8;
            let bit_depth_chroma = (r.u8()? & 7) + 8;
            let n = r.u8()? as usize;
            let sps_ext = r.nal_list(n, Some(NalUnitType::SpsExtension))?;
            Some(AvcConfigExt {
                chroma_format,
                bit_depth_luma,
                bit_depth_chroma,
                sps_ext,
            })
        } else {
            None
        };
        Ok(AvcDecoderConfigurationRecord {
            profile_indication,
            profile_compatibility,
            level_indication,
            length_size,
            sps,
            pps,
            ext,
        })
    }

    /// SPS・PPS の NAL（ヘッダー付き）から作る。プロファイル等は最初の SPS から取る。NAL 長さは 4 バイト。
    pub fn from_parameter_sets(sps: &[&[u8]], pps: &[&[u8]]) -> Result<Self> {
        let first = sps.first().ok_or(Error::Invalid("SPS がない"))?;
        let s = Sps::parse(first)?;
        for p in pps {
            if nal_unit_type(p) != Some(NalUnitType::Pps) {
                return Err(Error::Invalid("PPS ではない NAL"));
            }
        }
        for extra in &sps[1..] {
            if nal_unit_type(extra) != Some(NalUnitType::Sps) {
                return Err(Error::Invalid("SPS ではない NAL"));
            }
        }
        let ext = writes_ext(s.profile_idc).then(|| AvcConfigExt {
            chroma_format: s.chroma_format_idc,
            bit_depth_luma: s.bit_depth_luma,
            bit_depth_chroma: s.bit_depth_chroma,
            sps_ext: Vec::new(),
        });
        Ok(AvcDecoderConfigurationRecord {
            profile_indication: s.profile_idc,
            profile_compatibility: s.constraint_flags,
            level_indication: s.level_idc,
            length_size: 4,
            sps: sps.iter().map(|n| Bytes::copy_from_slice(n)).collect(),
            pps: pps.iter().map(|n| Bytes::copy_from_slice(n)).collect(),
            ext,
        })
    }

    /// バイト列にする。
    pub fn to_bytes(&self) -> Result<Bytes> {
        if !matches!(self.length_size, 1 | 2 | 4) {
            return Err(Error::Invalid("NAL 長さのバイト数は 1・2・4 のどれか"));
        }
        if self.sps.len() > 31 {
            return Err(Error::Invalid("SPS が 31 個を超える"));
        }
        if self.pps.len() > 255 {
            return Err(Error::Invalid("PPS が 255 個を超える"));
        }
        let mut out = vec![
            1,
            self.profile_indication,
            self.profile_compatibility,
            self.level_indication,
            0xFC | (self.length_size - 1),
            0xE0 | self.sps.len() as u8,
        ];
        write_nal_list(&mut out, &self.sps)?;
        out.push(self.pps.len() as u8);
        write_nal_list(&mut out, &self.pps)?;
        if let Some(ext) = &self.ext {
            if ext.sps_ext.len() > 255 {
                return Err(Error::Invalid("SPS 拡張が 255 個を超える"));
            }
            if !(8..=15).contains(&ext.bit_depth_luma) || !(8..=15).contains(&ext.bit_depth_chroma)
            {
                return Err(Error::Invalid("ビット深度"));
            }
            out.push(0xFC | (ext.chroma_format & 3));
            out.push(0xF8 | (ext.bit_depth_luma - 8));
            out.push(0xF8 | (ext.bit_depth_chroma - 8));
            out.push(ext.sps_ext.len() as u8);
            write_nal_list(&mut out, &ext.sps_ext)?;
        }
        Ok(Bytes::from(out))
    }

    /// `avc1.PPCCLL`。
    pub fn codecs_string(&self) -> String {
        crate::sps::codecs_string(
            self.profile_indication,
            self.profile_compatibility,
            self.level_indication,
        )
    }

    /// 最初の SPS を解析する。
    pub fn parse_sps(&self) -> Result<Sps> {
        Sps::parse(self.sps.first().ok_or(Error::Invalid("SPS がない"))?)
    }

    /// [`CodecConfig`] を作る（幅・高さは最初の SPS から）。
    pub fn codec_config(&self, generation: u32) -> Result<CodecConfig> {
        let sps = self.parse_sps()?;
        Ok(CodecConfig {
            codec: Codec::H264,
            generation,
            record: self.to_bytes()?,
            video: Some(VideoParams {
                width: sps.width,
                height: sps.height,
                profile: self.profile_indication,
                compat: self.profile_compatibility,
                level: self.level_indication,
            }),
            audio: None,
        })
    }
}

/// SPS・PPS（NAL ヘッダー付き）から [`CodecConfig`] を作る。
pub fn codec_config(sps: &[u8], pps: &[u8], generation: u32) -> Result<CodecConfig> {
    AvcDecoderConfigurationRecord::from_parameter_sets(&[sps], &[pps])?.codec_config(generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sps::tests::{SpsSpec, build_sps};

    #[test]
    fn build_parse_roundtrip_high() {
        let sps = build_sps(&SpsSpec {
            profile_idc: 100,
            level: 40,
            ..Default::default()
        });
        let pps = [0x68u8, 0xEB, 0xE3, 0xCB, 0x22, 0xC0];
        let rec = AvcDecoderConfigurationRecord::from_parameter_sets(&[&sps], &[&pps]).unwrap();
        let bytes = rec.to_bytes().unwrap();
        assert_eq!(&bytes[..6], &[1, 100, 0, 40, 0xFF, 0xE1]);
        assert_eq!(&bytes[bytes.len() - 4..], &[0xFD, 0xF8, 0xF8, 0]);
        let back = AvcDecoderConfigurationRecord::parse(&bytes).unwrap();
        assert_eq!(back, rec);
        assert_eq!(back.codecs_string(), "avc1.640028");
        let cc = back.codec_config(3).unwrap();
        assert_eq!(cc.codec, Codec::H264);
        assert_eq!(cc.generation, 3);
        assert_eq!(cc.record, bytes);
        let v = cc.video.unwrap();
        assert_eq!(
            (v.width, v.height, v.profile, v.level),
            (1280, 720, 100, 40)
        );
        assert_eq!(codec_config(&sps, &pps, 0).unwrap().record, bytes);
    }

    #[test]
    fn baseline_has_no_ext_and_old_high_records() {
        let sps = build_sps(&SpsSpec {
            profile_idc: 66,
            ..Default::default()
        });
        let rec =
            AvcDecoderConfigurationRecord::from_parameter_sets(&[&sps], &[&[0x68, 0xCE]]).unwrap();
        assert!(rec.ext.is_none());
        let b = rec.to_bytes().unwrap();
        assert_eq!(AvcDecoderConfigurationRecord::parse(&b).unwrap(), rec);

        // High でも拡張を省いた古い形式を受け付ける
        let sps = build_sps(&SpsSpec::default());
        let mut rec =
            AvcDecoderConfigurationRecord::from_parameter_sets(&[&sps], &[&[0x68, 0xCE]]).unwrap();
        rec.ext = None;
        rec.length_size = 2;
        let b = rec.to_bytes().unwrap();
        let back = AvcDecoderConfigurationRecord::parse(&b).unwrap();
        assert_eq!(back.length_size, 2);
        assert!(back.ext.is_none());
    }

    #[test]
    fn rejects_malformed() {
        let sps = build_sps(&SpsSpec::default());
        let rec =
            AvcDecoderConfigurationRecord::from_parameter_sets(&[&sps], &[&[0x68, 0xCE]]).unwrap();
        let b = rec.to_bytes().unwrap();
        // 途中で切ったもの（拡張の手前で切ったものは拡張なしとして受理されうる）
        for n in 0..b.len() {
            let r = AvcDecoderConfigurationRecord::parse(&b[..n]);
            if n < b.len() - 4 {
                assert!(r.is_err(), "len {n}");
            }
        }
        let mut bad = b.to_vec();
        bad[0] = 0;
        assert!(AvcDecoderConfigurationRecord::parse(&bad).is_err());
        let mut bad = b.to_vec();
        bad[4] = 0xFE; // lengthSizeMinusOne = 2
        assert!(AvcDecoderConfigurationRecord::parse(&bad).is_err());
        let mut bad = b.to_vec();
        bad[8] = 0x68; // SPS の位置に PPS
        assert!(AvcDecoderConfigurationRecord::parse(&bad).is_err());
        assert!(AvcDecoderConfigurationRecord::from_parameter_sets(&[], &[]).is_err());
        assert!(AvcDecoderConfigurationRecord::from_parameter_sets(&[&sps], &[&[0x67]]).is_err());
        let mut r2 = rec.clone();
        r2.pps.push(Bytes::new());
        assert!(r2.to_bytes().is_err());
        let mut r2 = rec.clone();
        r2.length_size = 3;
        assert!(r2.to_bytes().is_err());
        let mut r2 = rec;
        r2.pps.push(Bytes::from(vec![0x68; 70000]));
        assert!(r2.to_bytes().is_err());
    }
}
