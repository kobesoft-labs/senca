//! H.264 のビットストリーム解析: Annex B / 長さ前置の NAL 分割、SPS・PPS・スライスヘッダー、avcC。
#![no_main]
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use senca_h264::{AvcDecoderConfigurationRecord, NalHeader, Pps, SliceHeader, Sps};

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else { return };
    match sel % 4 {
        0 => {
            // Annex B の AU
            let mut sps = None;
            for nal in senca_h264::annexb_nals(rest).take(4096) {
                if let Ok(h) = NalHeader::parse(nal) {
                    let _ = h;
                }
                match nal.first().map(|b| b & 0x1f) {
                    Some(7) => sps = Sps::parse(nal).ok(),
                    Some(8) => {
                        let _ = Pps::parse(nal);
                    }
                    Some(1) | Some(5) => {
                        let _ = SliceHeader::parse(nal, sps.as_ref());
                    }
                    _ => {}
                }
            }
            let _ = senca_h264::annexb_to_length_prefixed(rest);
            let _ = senca_h264::annexb_nals_bytes(&Bytes::copy_from_slice(rest));
        }
        1 => {
            // 長さ前置（1〜4 バイト）
            for ls in 1..=4u8 {
                if let Ok(it) = senca_h264::length_prefixed_nals(rest, ls) {
                    for n in it.take(4096) {
                        let _ = n;
                    }
                }
                let _ = senca_h264::length_prefixed_to_annexb(rest, ls);
            }
        }
        2 => {
            if let Ok(r) = AvcDecoderConfigurationRecord::parse(rest) {
                let _ = r.codecs_string();
                if let Ok(s) = r.parse_sps() {
                    let _ = s;
                }
                let _ = r.codec_config(1);
                if let Ok(b) = r.to_bytes() {
                    AvcDecoderConfigurationRecord::parse(&b).expect("re-encoded avcC must parse");
                }
            }
        }
        _ => {
            let _ = Sps::parse(rest);
            let _ = Sps::parse_rbsp(rest);
            let _ = Pps::parse(rest);
            let _ = SliceHeader::parse(rest, None);
        }
    }
});
