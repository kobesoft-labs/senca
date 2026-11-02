//! AAC の AudioSpecificConfig と ADTS（分割・逐次読み込み・生の AU への変換）。
#![no_main]
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use senca_aac::{AdtsHeader, AdtsReader, AudioSpecificConfig};

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else { return };
    if sel & 1 == 0 {
        if let Ok(asc) = AudioSpecificConfig::parse(rest) {
            let _ = senca_aac::wrap_adts(&asc, &[0u8; 16]);
        }
        let _ = senca_aac::codec_config(rest, 1);
    } else {
        let b = Bytes::copy_from_slice(rest);
        let _ = senca_aac::split_adts(&b);
        let _ = senca_aac::adts_to_raw(&b);
        if let Ok(h) = AdtsHeader::parse(rest) {
            let _ = (h.header_len(), h.payload_len(), h.sampling_frequency(), h.audio_specific_config());
            let _ = h.to_bytes();
        }
        let mut r = AdtsReader::new();
        let step = 1 + (sel >> 1) as usize * 3;
        for chunk in rest.chunks(step) {
            if r.push(chunk).is_err() {
                return;
            }
            loop {
                match r.next_frame() {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => return,
                }
            }
        }
    }
});
