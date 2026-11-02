//! SDP の解析（RFC 8866）。解析できたものは書き出して読み直す。
#![no_main]
use libfuzzer_sys::fuzz_target;
use senca_sdp::SessionDescription;

fuzz_target!(|data: &[u8]| {
    if let Ok(sd) = SessionDescription::parse_bytes(data) {
        let s = sd.to_string();
        let _ = SessionDescription::parse(&s);
    }
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = senca_sdp::RtpMap::parse(s);
        let _ = senca_sdp::Fmtp::parse(s);
        let _ = senca_sdp::RtcpFb::parse(s);
    }
});
