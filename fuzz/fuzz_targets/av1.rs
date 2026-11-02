//! AV1 の OBU 列・sequence header・frame header の先頭・av1C。
#![no_main]
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use senca_av1::{Av1CodecConfigurationRecord, ObuType, SequenceHeader};

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else { return };
    match sel % 3 {
        0 => {
            let mut seq = None;
            for obu in senca_av1::obus(rest).take(4096) {
                let Ok(obu) = obu else { break };
                match obu.header.obu_type {
                    ObuType::SequenceHeader => {
                        seq = SequenceHeader::parse(obu.payload).ok();
                        let _ = Av1CodecConfigurationRecord::from_sequence_header_obu(obu.raw);
                    }
                    ObuType::FrameHeader | ObuType::Frame => {
                        let reduced = seq.as_ref().is_some_and(|s| s.reduced_still_picture_header);
                        let _ = senca_av1::FrameHeaderStart::parse(obu.payload, reduced);
                    }
                    _ => {}
                }
            }
            let _ = senca_av1::is_keyframe(rest, seq.as_ref());
            let _ = senca_av1::strip_temporal_delimiters(&Bytes::copy_from_slice(rest));
        }
        1 => {
            let _ = Av1CodecConfigurationRecord::parse(rest);
            let _ = senca_av1::codec_config(rest, 1);
        }
        _ => {
            let _ = SequenceHeader::parse(rest);
            let _ = senca_av1::read_leb128(rest);
        }
    }
});
