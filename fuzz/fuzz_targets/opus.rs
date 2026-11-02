//! Opus のパケット（RFC 6716 3.1 の TOC と code 0〜3）と OpusHead。
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = senca_opus::parse_packet(data);
    let _ = senca_opus::packet_samples(data);
    let _ = senca_opus::frame_count(data);
    let _ = senca_opus::OpusHead::parse(data);
});
