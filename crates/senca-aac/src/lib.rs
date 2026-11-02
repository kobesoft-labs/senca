//! AAC の設定（AudioSpecificConfig）と ADTS の枠組み。sans-io。
//!
//! AAC の符号化・復号はしない（ADR-0004: AAC はパススルーのみ）。ここで扱うのは設定とフレームの境界だけ。
//!
//! - [`asc`]: AudioSpecificConfig の解析・組み立てと codecs 文字列（`mp4a.40.N`）
//! - [`adts`]: ADTS ヘッダーの解析・組み立て、ADTS 列から raw_data_block への分割

pub mod adts;
pub mod asc;
pub mod error;

pub use adts::{
    ADTS_HEADER_LEN, AdtsFrame, AdtsHeader, AdtsReader, adts_to_raw, same_config, split_adts,
    wrap_adts,
};
pub use asc::{
    AudioSpecificConfig, SAMPLING_FREQUENCIES, channels_of_configuration, sampling_frequency_index,
};
pub use error::{Error, Result};

use senca_core::media::{AudioParams, Codec, CodecConfig};

/// AudioSpecificConfig のバイト列から [`CodecConfig`] を作る。
///
/// `AudioParams.sample_rate` はコアの標本化周波数（1 フレーム = 1024/960 サンプルの基準）、
/// `object_type` は codecs 文字列と同じ最初に書かれた audioObjectType。
pub fn codec_config(asc_bytes: &[u8], generation: u32) -> Result<CodecConfig> {
    let asc = AudioSpecificConfig::parse(asc_bytes)?;
    Ok(CodecConfig {
        codec: Codec::Aac,
        generation,
        record: bytes::Bytes::copy_from_slice(asc_bytes),
        video: None,
        audio: Some(AudioParams {
            sample_rate: asc.sampling_frequency,
            channels: asc.channels,
            object_type: asc.signaled_object_type,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_config_from_asc() {
        let cc = codec_config(&[0x11, 0x90], 2).unwrap();
        assert_eq!(cc.codec, Codec::Aac);
        assert_eq!(&cc.record[..], &[0x11, 0x90]);
        let a = cc.audio.unwrap();
        assert_eq!((a.sample_rate, a.channels, a.object_type), (48000, 2, 2));
        assert!(codec_config(&[0x11], 0).is_err());
    }
}
