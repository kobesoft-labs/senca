//! RTCP の複合パケット・reduced-size（RFC 3550・4585・5104・5506）の解析。
#![no_main]
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let b = Bytes::copy_from_slice(data);
    let _ = senca_rtp::parse_compound(&b);
    let _ = senca_rtp::parse_reduced(&b);
});
