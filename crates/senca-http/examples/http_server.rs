//! 相互運用試験用の小さなブロッキング HTTP サーバー（std::net、接続ごとにスレッド）。
//!
//! 使い方: `http_server <port>`（127.0.0.1 で待ち受ける）
//!
//! - `GET /hello` → `hello\n`
//! - `POST|PUT /echo` → 受け取った body をそのまま返す（上限 1 MiB）
//! - `POST /upload` → 受け取ったバイト数（上限 16 MiB）
//! - `GET /chunked` → chunked の応答
//! - `GET /events` → SSE を 3 つ送って終わる
//! - `GET /streams/{id}` → `{"id":"..."}`

use bytes::Bytes;
use senca_http::router::{RouteMatch, Router};
use senca_http::sse::SseEvent;
use senca_http::{CONTINUE_100, Event, Framing, HttpError, Limits, Method, RequestHead, RequestParser, Response, Version, chunked};
use std::io::{IoSlice, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Hello,
    Echo,
    Upload,
    Chunked,
    Events,
    Stream,
}

fn write_all_vectored(s: &mut TcpStream, parts: &[Bytes]) -> std::io::Result<()> {
    let mut slices: Vec<IoSlice<'_>> = parts.iter().filter(|b| !b.is_empty()).map(|b| IoSlice::new(b)).collect();
    let mut bufs = &mut slices[..];
    while !bufs.is_empty() {
        let n = s.write_vectored(bufs)?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        IoSlice::advance_slices(&mut bufs, n);
    }
    Ok(())
}

fn send_error(s: &mut TcpStream, e: &HttpError) {
    let body = Bytes::from(format!("{e}\n"));
    if let Ok(r) = Response::new(e.status()).header("Content-Type", "text/plain").and_then(|r| r.encode(body, Version::Http11, false, false)) {
        let _ = write_all_vectored(s, &[r.head, r.body]);
    }
    // 送信側を閉じてから残りを読み捨てる（未読のまま閉じると RST で応答が届かないことがある）
    let _ = s.shutdown(std::net::Shutdown::Write);
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let mut sink = [0u8; 4096];
    let mut total = 0usize;
    while let Ok(n) = s.read(&mut sink) {
        total += n;
        if n == 0 || total > (4 << 20) {
            break;
        }
    }
}

fn simple(s: &mut TcpStream, head: &RequestHead, status: u16, ctype: &str, body: Bytes) -> std::io::Result<()> {
    let r = Response::new(status)
        .header("Content-Type", ctype.to_owned())
        .and_then(|r| r.encode(body, head.version, head.keep_alive, head.method == Method::Head))
        .map_err(std::io::Error::other)?;
    write_all_vectored(s, &[r.head, r.body])
}

fn respond(s: &mut TcpStream, router: &Router<Route>, head: &RequestHead, body: Vec<u8>) -> std::io::Result<()> {
    match router.route(&head.method, head.path()) {
        RouteMatch::Found { handler, params } => match handler {
            Route::Hello => simple(s, head, 200, "text/plain", Bytes::from_static(b"hello\n")),
            Route::Echo => simple(s, head, 200, "application/octet-stream", Bytes::from(body)),
            Route::Upload => simple(s, head, 200, "text/plain", Bytes::from(format!("{}\n", body.len()))),
            Route::Stream => {
                let id = params.get("id").unwrap_or_default();
                let json = format!("{{\"id\":\"{}\"}}", id.replace(['"', '\\'], ""));
                simple(s, head, 200, "application/json", Bytes::from(json))
            }
            Route::Chunked | Route::Events => {
                let events = *handler == Route::Events;
                let ctype = if events { "text/event-stream" } else { "text/plain" };
                let framing = if head.version == Version::Http10 { Framing::Close } else { Framing::Chunked };
                let r = Response::new(200).header("Content-Type", ctype).map_err(std::io::Error::other)?;
                let h = r.encode_head(framing, head.version, head.keep_alive).map_err(std::io::Error::other)?;
                write_all_vectored(s, &[h])?;
                for i in 0..3 {
                    let data = if events {
                        let d = format!("{{\"n\":{i}}}");
                        SseEvent { event: Some("tick"), id: Some(&i.to_string()), retry: None, data: &d }.encode().map_err(std::io::Error::other)?
                    } else {
                        Bytes::from(format!("part{i}\n"))
                    };
                    if framing == Framing::Chunked {
                        if let Some(c) = chunked::encode_chunk(data) {
                            write_all_vectored(s, &c)?;
                        }
                    } else {
                        write_all_vectored(s, &[data])?;
                    }
                }
                if framing == Framing::Chunked {
                    write_all_vectored(s, &[chunked::last_chunk(None)])?;
                }
                Ok(())
            }
        },
        RouteMatch::MethodNotAllowed { allow } => {
            let allow: Vec<&str> = allow.iter().map(|m| m.as_str()).collect();
            let r = Response::new(405)
                .header("Allow", allow.join(", "))
                .and_then(|r| r.encode(Bytes::new(), head.version, head.keep_alive, false))
                .map_err(std::io::Error::other)?;
            write_all_vectored(s, &[r.head, r.body])
        }
        RouteMatch::NotFound => simple(s, head, 404, "text/plain", Bytes::from_static(b"not found\n")),
        RouteMatch::BadPath(_) => simple(s, head, 400, "text/plain", Bytes::from_static(b"bad path\n")),
    }
}

fn serve(mut s: TcpStream, router: &Router<Route>) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let mut p = RequestParser::new(Limits::DEFAULT);
    let mut buf = vec![0u8; 16 * 1024];
    let mut cur: Option<RequestHead> = None;
    let mut body = Vec::new();
    loop {
        let ev = match p.poll() {
            Ok(ev) => ev,
            Err(e) => {
                send_error(&mut s, &e);
                return;
            }
        };
        match ev {
            Event::NeedMore => match s.read(&mut buf) {
                Ok(0) => p.feed_eof(),
                Ok(n) => {
                    if let Err(e) = p.feed(&buf[..n]) {
                        send_error(&mut s, &e);
                        return;
                    }
                }
                Err(_) => return,
            },
            Event::Head(h) => {
                let limit = match router.route(&h.method, h.path()) {
                    RouteMatch::Found { handler: Route::Upload, .. } => 16 << 20,
                    _ => Limits::DEFAULT.max_body,
                };
                p.set_body_limit(limit);
                if h.content_length().is_some_and(|n| n > limit) {
                    send_error(&mut s, &HttpError::BodyTooLarge);
                    return;
                }
                if h.expect_continue && h.has_body() && s.write_all(CONTINUE_100).is_err() {
                    return;
                }
                body.clear();
                cur = Some(h);
            }
            Event::Data(d) => body.extend_from_slice(&d),
            Event::End(_) => {
                let Some(h) = cur.take() else { return };
                if respond(&mut s, router, &h, std::mem::take(&mut body)).is_err() || !h.keep_alive {
                    return;
                }
                if matches!(router.route(&h.method, h.path()), RouteMatch::Found { handler: Route::Chunked | Route::Events, .. })
                    && h.version == Version::Http10
                {
                    return;
                }
            }
            Event::Closed => return,
        }
    }
}

fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(42080);
    let mut router = Router::new();
    router.add(Method::Get, "/hello", Route::Hello).unwrap();
    router.add(Method::Post, "/echo", Route::Echo).unwrap();
    router.add(Method::Put, "/echo", Route::Echo).unwrap();
    router.add(Method::Post, "/upload", Route::Upload).unwrap();
    router.add(Method::Get, "/chunked", Route::Chunked).unwrap();
    router.add(Method::Get, "/events", Route::Events).unwrap();
    router.add(Method::Get, "/streams/{id}", Route::Stream).unwrap();
    let router = std::sync::Arc::new(router);
    let l = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    eprintln!("listening on 127.0.0.1:{port}");
    for s in l.incoming().flatten() {
        let router = router.clone();
        std::thread::spawn(move || serve(s, &router));
    }
}
