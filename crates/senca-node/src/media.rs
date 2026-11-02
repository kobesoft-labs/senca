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
        _ => {}
    }
}
