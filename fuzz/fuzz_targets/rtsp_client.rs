//! RTSP の client（ClientSession）に server 側のバイト列を与える。
//! OPTIONS → DESCRIBE → SETUP → PLAY と進め、応答・interleaved・リダイレクト・認証の扱いを試す。
#![no_main]
use std::time::{Duration, Instant};

use libfuzzer_sys::fuzz_target;
use senca_rtsp::{ClientConfig, ClientEvent, ClientSession, Credentials, TransportRequest};

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else { return };
    let mut cfg = ClientConfig::new("rtsp://user:pass@127.0.0.1:8554/live/cam");
    if sel & 1 != 0 {
        cfg.credentials = Some(Credentials::new("admin", "secret"));
    }
    cfg.allow_basic = sel & 2 != 0;
    cfg.cnonce = Some(Box::new(|| "0a4f113b".to_string()));
    let Ok(mut c) = ClientSession::new(cfg) else { return };
    let mut now = Instant::now();
    if c.start(now).is_err() {
        return;
    }
    let step = 1 + (sel >> 2) as usize * 9;
    for chunk in rest.chunks(step) {
        now += Duration::from_millis(5);
        if c.feed(now, chunk).is_err() {
            return;
        }
        while let Some(ev) = c.poll_event() {
            let r = match ev {
                ClientEvent::Options { .. } => c.describe(now),
                ClientEvent::Described { content_base, .. } => {
                    let url = c.resolve_control("trackID=0");
                    let _ = content_base;
                    c.setup(now, &url, TransportRequest::Interleaved { rtp_channel: 0, rtcp_channel: 1 })
                        .map(|_| ())
                }
                ClientEvent::SetupDone { .. } => c.play(now, None),
                ClientEvent::Reconnect { .. } => c.reconnected(now),
                ClientEvent::Playing { .. } => c.get_parameter(now),
                _ => Ok(()),
            };
            if r.is_err() {
                return;
            }
        }
        let _ = c.take_output();
        if let Some(t) = c.poll_timeout() {
            if t <= now {
                let _ = c.handle_timeout(now);
            }
        }
    }
});
