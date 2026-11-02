//! AMF0 の復号（RTMP の command・FLV の script）。復号できたものは符号化し直して往復させる。
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(values) = senca_amf0::decode_all(data) {
        if let Ok(enc) = senca_amf0::encode_all(values.iter()) {
            // 符号化し直したものは必ず復号できる
            let again = senca_amf0::decode_all(&enc).expect("re-encoded AMF0 must decode");
            assert_eq!(again.len(), values.len());
        }
    }
});
