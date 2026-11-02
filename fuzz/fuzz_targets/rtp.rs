//! RTP のヘッダー・拡張ヘッダー（RFC 3550・8285）の解析と書き出し。
#![no_main]
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use senca_rtp::RtpPacket;

fuzz_target!(|data: &[u8]| {
    let _ = senca_rtp::is_rtcp(data);
    if let Ok(p) = RtpPacket::parse(Bytes::copy_from_slice(data)) {
        if let Some(ext) = &p.header.extension {
            let _ = ext.elements();
            let _ = ext.get(1);
        }
        let _ = p.header_len();
        if let Ok(b) = p.to_bytes() {
            let q = RtpPacket::parse(b).expect("re-encoded RTP must parse");
            assert_eq!(q.payload, p.payload);
        }
    }
});
