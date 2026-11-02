//! SRT のパケット・ハンドシェイクの CIF・loss list・Stream ID の解析。
#![no_main]
use libfuzzer_sys::fuzz_target;
use senca_srt::packet::{Packet, decode_loss_list, parse};

fuzz_target!(|data: &[u8]| {
    let _ = senca_srt::packet::peek_dest(data);
    if let Ok(Packet::Control(_h, cif)) = parse(data) {
        let _ = senca_srt::handshake::Handshake::parse(cif);
        let _ = decode_loss_list(cif, 1024);
    }
    let _ = senca_srt::handshake::Handshake::parse(data);
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = senca_srt::StreamId::parse(s);
    }
});
