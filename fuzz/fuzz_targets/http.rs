//! HTTP/1.1 の要求と応答の解析（RFC 9112）。先頭のバイトで要求・応答を選ぶ。
#![no_main]
use libfuzzer_sys::fuzz_target;
use senca_http::{Event, Limits, Method, RequestParser, ResponseParser};

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else { return };
    let step = 1 + (sel >> 1) as usize * 5;
    if sel & 1 == 0 {
        let mut p = RequestParser::new(Limits::default());
        for chunk in rest.chunks(step) {
            if p.feed(chunk).is_err() {
                return;
            }
            loop {
                match p.poll() {
                    Ok(Event::NeedMore) | Ok(Event::Closed) => break,
                    Ok(Event::Head(h)) => {
                        let _ = (h.path(), h.query(), h.content_length(), h.has_body());
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        }
        p.feed_eof();
        for _ in 0..1024 {
            match p.poll() {
                Ok(Event::NeedMore) | Ok(Event::Closed) | Err(_) => break,
                Ok(_) => {}
            }
        }
    } else {
        let mut p = ResponseParser::new(Limits::default());
        for _ in 0..4 {
            p.push_request(Method::Get);
        }
        p.push_request(Method::Head);
        for chunk in rest.chunks(step) {
            if p.feed(chunk).is_err() {
                return;
            }
            loop {
                match p.poll() {
                    Ok(Event::NeedMore) | Ok(Event::Closed) => break,
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        }
        p.feed_eof();
        for _ in 0..1024 {
            match p.poll() {
                Ok(Event::NeedMore) | Ok(Event::Closed) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }
});
