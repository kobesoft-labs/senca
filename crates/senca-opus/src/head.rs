//! OpusHead（RFC 7845 5.1 の識別ヘッダー）。

use crate::error::{Error, Result};
use bytes::Bytes;
use senca_core::media::{AudioParams, Codec, CodecConfig};

const MAGIC: &[u8; 8] = b"OpusHead";

/// チャンネル対応表（mapping family が 0 以外のとき）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelMapping {
    pub stream_count: u8,
    pub coupled_count: u8,
    /// 出力チャンネルごとの復号チャンネル番号（255 は無音）。
    pub mapping: Vec<u8>,
}

/// OpusHead。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusHead {
    /// version（上位 4 ビットが 0 のものだけ受け付ける）。
    pub version: u8,
    pub channels: u8,
    /// 復号の始めに捨てるサンプル数（48 kHz）。
    pub pre_skip: u16,
    /// 元の入力の標本化周波数（参考値。0 は不明）。復号は常に 48 kHz。
    pub input_sample_rate: u32,
    /// 出力の利得（Q7.8 dB）。
    pub output_gain: i16,
    pub mapping_family: u8,
    pub mapping: Option<ChannelMapping>,
}

impl OpusHead {
    /// mapping family 0（モノラル・ステレオ）の OpusHead を作る。
    pub fn new(channels: u8, pre_skip: u16, input_sample_rate: u32) -> Result<Self> {
        if !(1..=2).contains(&channels) {
            return Err(Error::Invalid("mapping family 0 のチャンネル数は 1 か 2"));
        }
        Ok(OpusHead {
            version: 1,
            channels,
            pre_skip,
            input_sample_rate,
            output_gain: 0,
            mapping_family: 0,
            mapping: None,
        })
    }

    /// バイト列を解析する（後ろの余分なバイトは無視する）。
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 19 {
            return Err(Error::Truncated);
        }
        if &data[..8] != MAGIC {
            return Err(Error::Invalid("OpusHead の magic"));
        }
        let version = data[8];
        if version >> 4 != 0 {
            return Err(Error::Unsupported("OpusHead の主版数が 0 でない"));
        }
        let channels = data[9];
        let head = OpusHead {
            version,
            channels,
            pre_skip: u16::from_le_bytes([data[10], data[11]]),
            input_sample_rate: u32::from_le_bytes([data[12], data[13], data[14], data[15]]),
            output_gain: i16::from_le_bytes([data[16], data[17]]),
            mapping_family: data[18],
            mapping: if data[18] == 0 {
                None
            } else {
                let table = data
                    .get(19..21 + channels as usize)
                    .ok_or(Error::Truncated)?;
                Some(ChannelMapping {
                    stream_count: table[0],
                    coupled_count: table[1],
                    mapping: table[2..].to_vec(),
                })
            },
        };
        head.validate()?;
        Ok(head)
    }

    /// 値の整合を確かめる（RFC 7845 5.1.1）。
    pub fn validate(&self) -> Result<()> {
        if self.version >> 4 != 0 {
            return Err(Error::Unsupported("OpusHead の主版数が 0 でない"));
        }
        if self.channels == 0 {
            return Err(Error::Invalid("チャンネル数が 0"));
        }
        match (self.mapping_family, &self.mapping) {
            (0, None) => {
                if self.channels > 2 {
                    return Err(Error::Invalid("mapping family 0 のチャンネル数は 1 か 2"));
                }
            }
            (0, Some(_)) => return Err(Error::Invalid("mapping family 0 に対応表がある")),
            (_, None) => return Err(Error::Invalid("対応表がない")),
            (family, Some(m)) => {
                if family == 1 && self.channels > 8 {
                    return Err(Error::Invalid("mapping family 1 のチャンネル数は 8 まで"));
                }
                if m.mapping.len() != self.channels as usize {
                    return Err(Error::Invalid("対応表の長さがチャンネル数と違う"));
                }
                if m.stream_count == 0 || m.coupled_count > m.stream_count {
                    return Err(Error::Invalid("stream_count・coupled_count"));
                }
                let total = m.stream_count as u16 + m.coupled_count as u16;
                if total > 255 {
                    return Err(Error::Invalid(
                        "stream_count + coupled_count が 255 を超える",
                    ));
                }
                if m.mapping.iter().any(|&i| i != 255 && i as u16 >= total) {
                    return Err(Error::Invalid("対応表の番号が範囲外"));
                }
            }
        }
        Ok(())
    }

    /// バイト列にする。
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut out = Vec::with_capacity(21 + self.channels as usize);
        out.extend_from_slice(MAGIC);
        out.push(self.version);
        out.push(self.channels);
        out.extend_from_slice(&self.pre_skip.to_le_bytes());
        out.extend_from_slice(&self.input_sample_rate.to_le_bytes());
        out.extend_from_slice(&self.output_gain.to_le_bytes());
        out.push(self.mapping_family);
        if let Some(m) = &self.mapping {
            out.push(m.stream_count);
            out.push(m.coupled_count);
            out.extend_from_slice(&m.mapping);
        }
        Ok(out)
    }

    /// [`CodecConfig`] を作る。`AudioParams.sample_rate` は復号の周波数 48000。
    pub fn codec_config(&self, generation: u32) -> Result<CodecConfig> {
        Ok(CodecConfig {
            codec: Codec::Opus,
            generation,
            record: Bytes::from(self.to_bytes()?),
            video: None,
            audio: Some(AudioParams {
                sample_rate: 48000,
                channels: self.channels,
                object_type: 0,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_roundtrip() {
        let h = OpusHead::new(2, 312, 48000).unwrap();
        let b = h.to_bytes().unwrap();
        assert_eq!(b.len(), 19);
        assert_eq!(&b[8..], &[1, 2, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 0]);
        assert_eq!(OpusHead::parse(&b).unwrap(), h);
        let cc = h.codec_config(1).unwrap();
        assert_eq!(cc.audio.unwrap().channels, 2);
        assert_eq!(cc.audio.unwrap().sample_rate, 48000);
        assert_eq!(cc.codec, Codec::Opus);
    }

    #[test]
    fn surround_mapping() {
        let h = OpusHead {
            version: 1,
            channels: 6,
            pre_skip: 312,
            input_sample_rate: 44100,
            output_gain: -256,
            mapping_family: 1,
            mapping: Some(ChannelMapping {
                stream_count: 4,
                coupled_count: 2,
                mapping: vec![0, 4, 1, 2, 3, 5],
            }),
        };
        let b = h.to_bytes().unwrap();
        assert_eq!(b.len(), 27);
        assert_eq!(OpusHead::parse(&b).unwrap(), h);
        // 余分なバイトは無視
        let mut more = b.clone();
        more.push(0xAA);
        assert_eq!(OpusHead::parse(&more).unwrap(), h);
    }

    #[test]
    fn rejects_malformed() {
        let good = OpusHead::new(1, 0, 0).unwrap().to_bytes().unwrap();
        for n in 0..good.len() {
            assert_eq!(OpusHead::parse(&good[..n]), Err(Error::Truncated));
        }
        let mut b = good.clone();
        b[0] = b'X';
        assert!(OpusHead::parse(&b).is_err());
        let mut b = good.clone();
        b[8] = 0x10;
        assert!(matches!(OpusHead::parse(&b), Err(Error::Unsupported(_))));
        let mut b = good.clone();
        b[9] = 0;
        assert!(OpusHead::parse(&b).is_err());
        let mut b = good.clone();
        b[9] = 3;
        assert!(OpusHead::parse(&b).is_err());
        // family 1 で対応表が足りない
        let mut b = good.clone();
        b[18] = 1;
        b.extend([1, 0]);
        assert_eq!(OpusHead::parse(&b), Err(Error::Truncated));
        b.push(1); // 番号 1 は stream_count + coupled_count = 1 以上
        assert!(OpusHead::parse(&b).is_err());
        let n = b.len();
        b[n - 1] = 255; // 無音は許される
        assert!(OpusHead::parse(&b).is_ok());
        let n = b.len();
        b[n - 3] = 0; // stream_count 0
        assert!(OpusHead::parse(&b).is_err());
        assert!(OpusHead::new(3, 0, 0).is_err());
        let mut h = OpusHead::new(2, 0, 0).unwrap();
        h.mapping_family = 1;
        assert!(h.to_bytes().is_err());
    }
}
