//! AV1CodecConfigurationRecord（AV1 ISOBMFF 2.3.3）と [`CodecConfig`] の組み立て。

use crate::error::{Error, Result};
use crate::obu::{ObuType, obus};
use crate::seq::SequenceHeader;
use bytes::Bytes;
use senca_core::media::{Codec, CodecConfig, VideoParams};

/// AV1CodecConfigurationRecord（av1C）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Av1CodecConfigurationRecord {
    pub seq_profile: u8,
    pub seq_level_idx_0: u8,
    pub seq_tier_0: u8,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub monochrome: bool,
    pub chroma_subsampling_x: bool,
    pub chroma_subsampling_y: bool,
    pub chroma_sample_position: u8,
    /// initial_presentation_delay_minus_one + 1（書かれていれば）。
    pub initial_presentation_delay: Option<u8>,
    /// configOBUs（シーケンスヘッダー OBU など。obu_size 付き）。
    pub config_obus: Bytes,
}

impl Av1CodecConfigurationRecord {
    /// バイト列を解析する。
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 4 {
            return Err(Error::Truncated);
        }
        if data[0] != 0x81 {
            return Err(Error::Invalid("av1C の marker/version"));
        }
        let config_obus = &data[4..];
        for obu in obus(config_obus) {
            let obu = obu?;
            if !obu.header.has_size_field {
                return Err(Error::Invalid("configOBUs に obu_size がない"));
            }
        }
        Ok(Av1CodecConfigurationRecord {
            seq_profile: data[1] >> 5,
            seq_level_idx_0: data[1] & 0x1f,
            seq_tier_0: data[2] >> 7,
            high_bitdepth: data[2] & 0x40 != 0,
            twelve_bit: data[2] & 0x20 != 0,
            monochrome: data[2] & 0x10 != 0,
            chroma_subsampling_x: data[2] & 0x08 != 0,
            chroma_subsampling_y: data[2] & 0x04 != 0,
            chroma_sample_position: data[2] & 3,
            initial_presentation_delay: (data[3] & 0x10 != 0).then_some((data[3] & 0x0f) + 1),
            config_obus: Bytes::copy_from_slice(config_obus),
        })
    }

    /// シーケンスヘッダー OBU（OBU ヘッダーと obu_size を含むもの）から作る。
    pub fn from_sequence_header_obu(obu: &[u8]) -> Result<Self> {
        let mut it = obus(obu);
        let first = it.next().ok_or(Error::Truncated)??;
        if first.header.obu_type != ObuType::SequenceHeader {
            return Err(Error::Invalid("シーケンスヘッダー OBU ではない"));
        }
        if it.next().is_some() {
            return Err(Error::Invalid(
                "シーケンスヘッダー OBU の後ろに余分なデータ",
            ));
        }
        let sh = SequenceHeader::parse(first.payload)?;
        // obu_size のない形で渡されたら付け直す
        let config_obus = if first.header.has_size_field {
            Bytes::copy_from_slice(first.raw)
        } else {
            let mut v = Vec::with_capacity(first.raw.len() + 2);
            crate::obu::write_obu(
                ObuType::SequenceHeader,
                first.header.extension,
                first.payload,
                &mut v,
            )?;
            Bytes::from(v)
        };
        Ok(Self::from_sequence_header(&sh, config_obus))
    }

    /// 解析済みのシーケンスヘッダーと configOBUs から作る。
    pub fn from_sequence_header(sh: &SequenceHeader, config_obus: Bytes) -> Self {
        let c = &sh.color;
        Av1CodecConfigurationRecord {
            seq_profile: sh.seq_profile,
            seq_level_idx_0: sh.seq_level_idx_0(),
            seq_tier_0: sh.seq_tier_0(),
            high_bitdepth: c.bit_depth > 8,
            twelve_bit: c.bit_depth == 12,
            monochrome: c.mono_chrome,
            chroma_subsampling_x: c.subsampling_x,
            chroma_subsampling_y: c.subsampling_y,
            chroma_sample_position: c.chroma_sample_position,
            initial_presentation_delay: sh
                .operating_points
                .first()
                .and_then(|o| o.initial_display_delay),
            config_obus,
        }
    }

    /// バイト列にする。
    pub fn to_bytes(&self) -> Result<Bytes> {
        if self.seq_profile > 7
            || self.seq_level_idx_0 > 31
            || self.seq_tier_0 > 1
            || self.chroma_sample_position > 3
        {
            return Err(Error::Invalid("av1C の値が範囲外"));
        }
        let delay = match self.initial_presentation_delay {
            Some(d @ 1..=16) => 0x10 | (d - 1),
            Some(_) => return Err(Error::Invalid("initial_presentation_delay")),
            None => 0,
        };
        let mut out = Vec::with_capacity(4 + self.config_obus.len());
        out.push(0x81);
        out.push((self.seq_profile << 5) | self.seq_level_idx_0);
        out.push(
            (self.seq_tier_0 << 7)
                | ((self.high_bitdepth as u8) << 6)
                | ((self.twelve_bit as u8) << 5)
                | ((self.monochrome as u8) << 4)
                | ((self.chroma_subsampling_x as u8) << 3)
                | ((self.chroma_subsampling_y as u8) << 2)
                | self.chroma_sample_position,
        );
        out.push(delay);
        out.extend_from_slice(&self.config_obus);
        Ok(Bytes::from(out))
    }

    /// ビット深度。
    pub fn bit_depth(&self) -> u8 {
        match (self.high_bitdepth, self.twelve_bit) {
            (false, _) => 8,
            (true, false) => 10,
            (true, true) => 12,
        }
    }

    /// codecs 文字列の短い形（`av01.P.LLT.DD`）。
    pub fn codecs_string(&self) -> String {
        format!(
            "av01.{}.{:02}{}.{:02}",
            self.seq_profile,
            self.seq_level_idx_0,
            if self.seq_tier_0 == 1 { 'H' } else { 'M' },
            self.bit_depth()
        )
    }

    /// configOBUs の中のシーケンスヘッダーを解析する。
    pub fn sequence_header(&self) -> Result<SequenceHeader> {
        SequenceHeader::find_in(&self.config_obus)?
            .map(|(s, _)| s)
            .ok_or(Error::Invalid("configOBUs にシーケンスヘッダーがない"))
    }

    /// [`CodecConfig`] を作る。幅・高さは max_frame_width/height、`VideoParams` の
    /// profile = seq_profile、compat = seq_tier_0、level = seq_level_idx_0。
    pub fn codec_config(&self, generation: u32) -> Result<CodecConfig> {
        let sh = self.sequence_header()?;
        Ok(CodecConfig {
            codec: Codec::Av1,
            generation,
            record: self.to_bytes()?,
            video: Some(VideoParams {
                width: sh.max_frame_width,
                height: sh.max_frame_height,
                profile: self.seq_profile,
                compat: self.seq_tier_0,
                level: self.seq_level_idx_0,
            }),
            audio: None,
        })
    }
}

/// シーケンスヘッダー OBU（obu_size 付き）から [`CodecConfig`] を作る。
pub fn codec_config(sequence_header_obu: &[u8], generation: u32) -> Result<CodecConfig> {
    Av1CodecConfigurationRecord::from_sequence_header_obu(sequence_header_obu)?
        .codec_config(generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obu::write_obu;
    use crate::seq::tests::{Spec, build};

    fn sh_obu(s: &Spec) -> Vec<u8> {
        let mut v = Vec::new();
        write_obu(ObuType::SequenceHeader, None, &build(s), &mut v).unwrap();
        v
    }

    #[test]
    fn roundtrip() {
        let obu = sh_obu(&Spec {
            profile: 2,
            high_bitdepth: true,
            twelve: true,
            level: 12,
            tier: true,
            initial_display_delay: Some(3),
            ..Default::default()
        });
        let rec = Av1CodecConfigurationRecord::from_sequence_header_obu(&obu).unwrap();
        let b = rec.to_bytes().unwrap();
        assert_eq!(&b[..4], &[0x81, 0x4C, 0xE8, 0x12]);
        assert_eq!(&b[4..], &obu[..]);
        let back = Av1CodecConfigurationRecord::parse(&b).unwrap();
        assert_eq!(back, rec);
        assert_eq!(back.codecs_string(), "av01.2.12H.12");
        let cc = back.codec_config(5).unwrap();
        let v = cc.video.unwrap();
        assert_eq!(
            (v.width, v.height, v.profile, v.compat, v.level),
            (1920, 1080, 2, 1, 12)
        );
        assert_eq!(codec_config(&obu, 5).unwrap(), cc);

        // obu_size のない OBU から作っても obu_size 付きになる
        let payload = build(&Spec::default());
        let mut no_size = vec![0x08];
        no_size.extend(&payload);
        let rec = Av1CodecConfigurationRecord::from_sequence_header_obu(&no_size).unwrap();
        assert_eq!(&rec.config_obus[..], &sh_obu(&Spec::default())[..]);
    }

    #[test]
    fn rejects_malformed() {
        let obu = sh_obu(&Spec::default());
        let b = Av1CodecConfigurationRecord::from_sequence_header_obu(&obu)
            .unwrap()
            .to_bytes()
            .unwrap();
        for n in 0..4 {
            assert!(Av1CodecConfigurationRecord::parse(&b[..n]).is_err());
        }
        for n in 5..b.len() {
            assert!(
                Av1CodecConfigurationRecord::parse(&b[..n]).is_err(),
                "len {n}"
            );
        }
        let mut bad = b.to_vec();
        bad[0] = 0x01;
        assert!(Av1CodecConfigurationRecord::parse(&bad).is_err());
        assert!(Av1CodecConfigurationRecord::from_sequence_header_obu(&[0x12, 0x00]).is_err());
        assert!(Av1CodecConfigurationRecord::from_sequence_header_obu(&[]).is_err());
        let mut extra = obu.clone();
        extra.extend([0x12, 0x00]);
        assert!(Av1CodecConfigurationRecord::from_sequence_header_obu(&extra).is_err());
        // configOBUs が空でも解析はできるが CodecConfig は作れない
        let empty = Av1CodecConfigurationRecord::parse(&[0x81, 0, 0x0C, 0]).unwrap();
        assert!(empty.codec_config(0).is_err());
        let mut r = empty;
        r.initial_presentation_delay = Some(17);
        assert!(r.to_bytes().is_err());
    }
}
