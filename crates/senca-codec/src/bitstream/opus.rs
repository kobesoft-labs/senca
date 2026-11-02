//! OpusHead（RFC 7845 5.1 の識別ヘッダー）。

use crate::error::{CodecError, Result};
use bytes::Bytes;

/// OpusHead の中身（チャンネル割り当て 0 だけを扱う: 1〜2 チャンネル）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusHead {
    pub channels: u8,
    pub pre_skip: u16,
    pub input_sample_rate: u32,
    pub output_gain: i16,
    pub mapping_family: u8,
}

pub fn build_opus_head(channels: u8, pre_skip: u16, input_sample_rate: u32) -> Bytes {
    let mut v = Vec::with_capacity(19);
    v.extend_from_slice(b"OpusHead");
    v.push(1);
    v.push(channels);
    v.extend_from_slice(&pre_skip.to_le_bytes());
    v.extend_from_slice(&input_sample_rate.to_le_bytes());
    v.extend_from_slice(&0i16.to_le_bytes());
    v.push(0);
    Bytes::from(v)
}

pub fn parse_opus_head(d: &[u8]) -> Result<OpusHead> {
    if d.len() < 19 || &d[..8] != b"OpusHead" || d[8] >> 4 != 0 {
        return Err(CodecError::invalid_input("malformed OpusHead"));
    }
    let h = OpusHead {
        channels: d[9],
        pre_skip: u16::from_le_bytes([d[10], d[11]]),
        input_sample_rate: u32::from_le_bytes([d[12], d[13], d[14], d[15]]),
        output_gain: i16::from_le_bytes([d[16], d[17]]),
        mapping_family: d[18],
    };
    if h.channels == 0 {
        return Err(CodecError::invalid_input("OpusHead with zero channels"));
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let b = build_opus_head(2, 312, 48000);
        assert_eq!(b.len(), 19);
        let h = parse_opus_head(&b).unwrap();
        assert_eq!(
            (
                h.channels,
                h.pre_skip,
                h.input_sample_rate,
                h.mapping_family
            ),
            (2, 312, 48000, 0)
        );
        assert!(parse_opus_head(b"OpusTags...........").is_err());
    }
}
