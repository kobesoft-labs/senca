//! RTSP のメッセージの切り出し（要求・応答・interleaved）とヘッダーの解析。
#![no_main]
use libfuzzer_sys::fuzz_target;
use senca_rtsp::{Decoder, Frame, Limits};

fuzz_target!(|data: &[u8]| {
    let mut d = Decoder::new(Limits::default());
    let step = 1 + data.first().copied().unwrap_or(0) as usize * 7;
    for chunk in data.chunks(step) {
        if d.feed(chunk).is_err() {
            return;
        }
        loop {
            match d.decode() {
                Ok(Some(Frame::Request(r))) => {
                    for (k, v) in r.headers.iter() {
                        let _ = (k, v);
                    }
                    if let Some(t) = r.headers.get("Transport") {
                        let _ = senca_rtsp::Transport::parse_list(t);
                    }
                }
                Ok(Some(Frame::Response(r))) => {
                    if let Some(t) = r.headers.get("Transport") {
                        let _ = senca_rtsp::Transport::parse_list(t);
                    }
                    if let Some(t) = r.headers.get("Range") {
                        let _ = senca_rtsp::Range::parse(t);
                    }
                    if let Some(t) = r.headers.get("RTP-Info") {
                        let _ = senca_rtsp::headers::parse_rtp_info(t);
                    }
                    if let Some(t) = r.headers.get("Public") {
                        let _ = senca_rtsp::headers::parse_public(t);
                    }
                    if let Some(t) = r.headers.get("WWW-Authenticate") {
                        let _ = t.len();
                    }
                    if let Some(t) = r.headers.get("Session") {
                        let _ = senca_rtsp::SessionHeader::parse(t);
                    }
                }
                Ok(Some(Frame::Interleaved { .. })) => {}
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
});
