//! 分散合意のノード間メッセージの復号（枠の切り出しと本体）。復号できたものは符号化し直して一致を確かめる。
#![no_main]
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use senca_raft::codec::{FrameDecoder, decode_message_body, encode_message};

fuzz_target!(|data: &[u8]| {
    if let Ok(m) = decode_message_body(Bytes::copy_from_slice(data)) {
        let enc = encode_message(&m);
        let again = decode_message_body(Bytes::copy_from_slice(&enc[4..])).expect("re-encoded message must decode");
        assert_eq!(again, m);
    }
    let mut d = FrameDecoder::new(1 << 20);
    let step = 1 + data.first().copied().unwrap_or(0) as usize * 3;
    for chunk in data.chunks(step) {
        d.push(chunk);
        loop {
            match d.next_message() {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
    let _ = senca_raft::codec::decode_entry(Bytes::copy_from_slice(data));
    let _ = senca_raft::codec::decode_hard_state(Bytes::copy_from_slice(data));
    let _ = senca_raft::codec::decode_conf_state(&Bytes::copy_from_slice(data));
    let _ = senca_raft::codec::decode_snapshot_meta(Bytes::copy_from_slice(data));
});
