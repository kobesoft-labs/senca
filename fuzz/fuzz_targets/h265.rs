//! H.265 のビットストリーム解析: NAL 分割、VPS・SPS・PPS・スライスセグメントヘッダーの先頭、hvcC、codecs 文字列。
#![no_main]
use libfuzzer_sys::fuzz_target;
use senca_h265::{HevcDecoderConfigurationRecord, NalHeader, Pps, SliceSegmentHeaderStart, Sps, Vps};

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else { return };
    match sel % 4 {
        0 => {
            for nal in senca_h265::annexb_nals(rest).take(4096) {
                let Ok(h) = NalHeader::parse(nal) else { continue };
                match nal.first().map(|b| (b >> 1) & 0x3f) {
                    Some(32) => {
                        let _ = Vps::parse(nal);
                    }
                    Some(33) => {
                        let _ = Sps::parse(nal);
                    }
                    Some(34) => {
                        let _ = Pps::parse(nal);
                    }
                    _ => {
                        if h.nal_unit_type.is_vcl() {
                            let _ = SliceSegmentHeaderStart::parse(nal);
                        }
                    }
                }
            }
            let _ = senca_h265::is_keyframe_annexb(rest);
            let _ = senca_h265::remove_emulation_prevention(rest);
        }
        1 => {
            for ls in 1..=4u8 {
                let _ = senca_h265::is_keyframe_length_prefixed(rest, ls);
                let _ = senca_h265::length_prefixed_to_annexb(rest, ls);
            }
        }
        2 => {
            if let Ok(r) = HevcDecoderConfigurationRecord::parse(rest) {
                let _ = r.parse_sps();
                if let Ok(b) = r.to_bytes() {
                    HevcDecoderConfigurationRecord::parse(&b).expect("re-encoded hvcC must parse");
                }
            }
        }
        _ => {
            let _ = Sps::parse(rest);
            let _ = Vps::parse(rest);
            let _ = Pps::parse(rest);
            if let Ok(s) = std::str::from_utf8(rest) {
                let _ = senca_h265::parse_codecs_string(s);
            }
        }
    }
});
