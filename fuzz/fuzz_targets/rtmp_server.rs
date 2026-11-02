//! RTMP の server（ServerSession）にクライアント側のバイト列を与える。
//! connect・publish・play は全部受け入れて、chunk・command・media の奥まで進める。
#![no_main]
use libfuzzer_sys::fuzz_target;
use senca_rtmp::{ServerConfig, ServerEvent, ServerSession};

fuzz_target!(|data: &[u8]| {
    let mut s = ServerSession::new(ServerConfig::default());
    let step = 1 + data.first().copied().unwrap_or(0) as usize * 13;
    for chunk in data.chunks(step) {
        if s.feed(chunk).is_err() {
            break;
        }
        while let Some(ev) = s.poll_event() {
            let r = match ev {
                ServerEvent::Connect(_) => s.accept_connect(),
                ServerEvent::Publish { stream_id, .. } => s.accept_publish(stream_id),
                ServerEvent::Play { stream_id, .. } => s.accept_play(stream_id),
                _ => Ok(()),
            };
            if r.is_err() {
                return;
            }
        }
        let _ = s.take_output();
    }
});
