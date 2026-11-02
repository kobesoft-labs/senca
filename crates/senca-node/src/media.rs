//! 設定の補い（解像度など）。FLV の取り込みは幅・高さを入れないので、SPS から読む。

use senca_core::{Codec, CodecConfig};

pub fn enrich(c: &mut CodecConfig) {
    match c.codec {
        Codec::H264 => {
            if let Ok(rec) = senca_h264::AvcDecoderConfigurationRecord::parse(&c.record)
                && let Some(sps) = rec.sps.first()
                && let Ok(s) = senca_h264::Sps::parse(sps)
            {
                let v = c.video.get_or_insert_with(Default::default);
                v.width = s.width;
                v.height = s.height;
            }
        }
        Codec::H265 => {
            if let Ok(rec) = senca_h265::HevcDecoderConfigurationRecord::parse(&c.record)
                && let Some(sps) = rec.nalus(senca_h265::NalUnitType::Sps).next()
                && let Ok(s) = senca_h265::Sps::parse(sps)
            {
                let v = c.video.get_or_insert_with(Default::default);
                v.width = s.width;
                v.height = s.height;
            }
        }
        Codec::Av1 => {
            // av1C の configOBUs のシーケンスヘッダーから最大フレームサイズを読む（fMP4 の av01 に要る）
            if let Ok(rec) = senca_av1::Av1CodecConfigurationRecord::parse(&c.record)
                && let Ok(Some((sh, _))) = senca_av1::SequenceHeader::find_in(&rec.config_obus)
            {
                let v = c.video.get_or_insert_with(Default::default);
                v.width = sh.max_frame_width;
                v.height = sh.max_frame_height;
            }
        }
        _ => {}
    }
}

/// VP8 / VP9 のキーフレームから幅・高さを読む（設定レコードに寸法が無いので）。
pub fn vpx_keyframe_dims(codec: Codec, frame: &[u8]) -> Option<(u32, u32)> {
    match codec {
        Codec::Vp8 => {
            // RFC 6386 9.1: 3 バイトの frame tag（bit0 = 0 がキーフレーム）、開始コード 9d 01 2a、幅・高さ 14 bit
            if frame.len() < 10 || frame[0] & 1 != 0 || frame[3..6] != [0x9d, 0x01, 0x2a] {
                return None;
            }
            let w = u16::from_le_bytes([frame[6], frame[7]]) & 0x3fff;
            let h = u16::from_le_bytes([frame[8], frame[9]]) & 0x3fff;
            Some((w as u32, h as u32))
        }
        Codec::Vp9 => {
            let mut r = senca_core::bits::BitReader::new(frame);
            if r.read_bits(2)? != 2 {
                return None; // frame_marker
            }
            let lo = r.read_bits(1)?;
            let hi = r.read_bits(1)?;
            let profile = (hi << 1) | lo;
            if profile == 3 {
                r.read_bits(1)?;
            }
            if r.read_bits(1)? == 1 {
                return None; // show_existing_frame
            }
            if r.read_bits(1)? != 0 {
                return None; // キーフレームではない
            }
            r.read_bits(2)?; // show_frame, error_resilient_mode
            if r.read_bits(24)? != 0x49_83_42 {
                return None;
            }
            // color_config
            if profile >= 2 {
                r.read_bits(1)?;
            }
            let cs = r.read_bits(3)?;
            if cs != 7 {
                r.read_bits(1)?;
                if profile == 1 || profile == 3 {
                    r.read_bits(3)?;
                }
            } else if profile == 1 || profile == 3 {
                r.read_bits(1)?;
            }
            let w = r.read_bits(16)? + 1;
            let h = r.read_bits(16)? + 1;
            Some((w as u32, h as u32))
        }
        _ => None,
    }
}
