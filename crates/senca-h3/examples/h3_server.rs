//! 相互接続試験用の HTTP/3・WebTransport server（std の同期 UDP、1 スレッド）。
//!
//! ```text
//! h3_server --listen 127.0.0.1:51010 --cert chain.pem --key key.pem
//! ```
//!
//! - `GET /` … 短い HTML、`GET /bytes/N` … N バイト、`POST /echo` … body をそのまま返す。
//! - WebTransport `/wt/echo` … 双方向 stream は FIN までの中身を返し、単方向 stream は新しい単方向
//!   stream で返し、datagram はそのまま返す。session を閉じられたら標準エラーに出す。

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::{Bytes, BytesMut};
use senca_h3::*;
use senca_quic::{ConnectionHandle, Endpoint, EndpointConfig, EndpointEvent, ServerConfig, Side, StreamId};
use senca_tls::CertifiedKey;

fn arg(args: &[String], k: &str) -> Option<String> {
    args.iter().position(|a| a == k).and_then(|i| args.get(i + 1).cloned())
}

#[derive(Default)]
struct App {
    /// 要求の body（POST /echo）。
    bodies: HashMap<StreamId, (String, Vec<u8>)>,
    /// WebTransport の stream ごとの受信（FIN で返す）。
    wt_in: HashMap<StreamId, Vec<u8>>,
    /// 送りきれていない応答。
    pending: Vec<(StreamId, Bytes)>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let listen: SocketAddr = arg(&args, "--listen").unwrap_or("127.0.0.1:51010".into()).parse().unwrap();
    let cert = CertifiedKey::from_pem_files(arg(&args, "--cert").unwrap(), arg(&args, "--key").unwrap()).unwrap();
    let mut tls = senca_tls::ServerConfig::new(Arc::new(cert)).unwrap();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut ep = Endpoint::new(EndpointConfig::new().unwrap(), Some(Arc::new(ServerConfig::new(Arc::new(tls)))));
    let sock = UdpSocket::bind(listen).unwrap();
    eprintln!("h3 listening on {listen}");
    let mut buf = vec![0u8; 65536];
    let mut conns: HashMap<ConnectionHandle, (H3Connection, App)> = HashMap::new();
    loop {
        let now = Instant::now();
        let wait = ep
            .poll_timeout()
            .map(|t| t.saturating_duration_since(now))
            .unwrap_or(Duration::from_millis(20))
            .clamp(Duration::from_micros(200), Duration::from_millis(20));
        sock.set_read_timeout(Some(wait)).ok();
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            ep.handle_datagram(Instant::now(), SystemTime::now(), from, BytesMut::from(&buf[..n]));
            sock.set_nonblocking(true).ok();
            while let Ok((n, from)) = sock.recv_from(&mut buf) {
                ep.handle_datagram(Instant::now(), SystemTime::now(), from, BytesMut::from(&buf[..n]));
            }
            sock.set_nonblocking(false).ok();
        }
        let now = Instant::now();
        ep.handle_timeout(now);
        let hs: Vec<_> = ep.connections().collect();
        for h in hs {
            let q = ep.connection_mut(h).unwrap();
            let (h3, app) = conns
                .entry(h)
                .or_insert_with(|| (H3Connection::new(Side::Server, H3Config::default()), App::default()));
            h3.process(q, now);
            while let Some(e) = h3.poll_event() {
                if std::env::var("H3_LOG").is_ok() {
                    eprintln!("{h:?} event {e:?}");
                }
                handle(h3, q, app, e);
            }
            // 送りきれていない応答
            let pend = std::mem::take(&mut app.pending);
            for (s, d) in pend {
                let n = h3.send_data(q, s, d.clone(), true).unwrap_or(0);
                if n < d.len() {
                    app.pending.push((s, d.slice(n..)));
                }
            }
            h3.process(q, now);
        }
        while let Some(t) = ep.poll_transmit(Instant::now()) {
            let _ = sock.send_to(&t.contents, t.destination);
        }
        while let Some(e) = ep.poll_event() {
            if let EndpointEvent::Drained(h) = e {
                conns.remove(&h);
                eprintln!("connection {h:?} drained");
            }
        }
    }
}

fn respond(h3: &mut H3Connection, q: &mut senca_quic::Connection, app: &mut App, s: StreamId, status: &str, ctype: &str, body: Vec<u8>) {
    let hd = Headers::new()
        .with(":status", status)
        .with("content-type", ctype)
        .with("content-length", body.len().to_string());
    let _ = h3.send_response(q, s, hd, false);
    let b = Bytes::from(body);
    let n = h3.send_data(q, s, b.clone(), true).unwrap_or(0);
    if n < b.len() {
        app.pending.push((s, b.slice(n..)));
    }
}

fn handle(h3: &mut H3Connection, q: &mut senca_quic::Connection, app: &mut App, e: H3Event) {
    match e {
        H3Event::Request { stream, headers } => {
            let method = headers.get_str(":method").unwrap_or("").to_string();
            let path = headers.get_str(":path").unwrap_or("").to_string();
            eprintln!("request {method} {path}");
            app.bodies.insert(stream, (format!("{method} {path}"), Vec::new()));
        }
        H3Event::Data { stream } => {
            while let Some(b) = h3.recv_body(q, stream, 65536) {
                if let Some((_, v)) = app.bodies.get_mut(&stream) {
                    v.extend_from_slice(&b);
                }
            }
        }
        H3Event::Finished { stream } => {
            let Some((req, body)) = app.bodies.remove(&stream) else { return };
            if req == "GET /" {
                respond(h3, q, app, stream, "200", "text/html; charset=utf-8", b"<!doctype html><title>senca h3</title><p>hello from senca h3</p>".to_vec());
            } else if let Some(n) = req.strip_prefix("GET /bytes/") {
                let n: usize = n.parse().unwrap_or(0).min(50 << 20);
                respond(h3, q, app, stream, "200", "application/octet-stream", (0..n).map(|i| (i % 251) as u8).collect());
            } else if req == "POST /echo" {
                respond(h3, q, app, stream, "200", "application/octet-stream", body);
            } else {
                respond(h3, q, app, stream, "404", "text/plain", b"not found".to_vec());
            }
        }
        H3Event::WebTransportRequest { session, headers } => {
            let path = headers.get_str(":path").unwrap_or("");
            eprintln!("webtransport request {path} origin={:?}", headers.get_str("origin"));
            if path == "/wt/echo" {
                let _ = h3.wt_accept(q, session, Headers::new());
            } else {
                let _ = h3.wt_reject(q, session, 404);
            }
        }
        H3Event::WebTransportReadable { session, stream } => loop {
            let Ok(c) = h3.wt_recv(q, stream, 65536) else { break };
            app.wt_in.entry(stream).or_default().extend_from_slice(&c.data);
            if c.fin {
                let data = Bytes::from(app.wt_in.remove(&stream).unwrap_or_default());
                if stream.dir() == senca_quic::Dir::Bi {
                    let _ = h3.wt_send(q, stream, data);
                    let _ = h3.wt_finish(q, stream);
                } else if let Ok(u) = h3.wt_open(q, session, WtStreamKind::Uni) {
                    let _ = h3.wt_send(q, u, data);
                    let _ = h3.wt_finish(q, u);
                }
                break;
            }
            if c.data.is_empty() {
                break;
            }
        },
        H3Event::WebTransportDatagram { session, data } => {
            let _ = h3.wt_send_datagram(q, session, &data);
        }
        H3Event::WebTransportClosed { session, code, reason } => {
            eprintln!("webtransport session {session:?} closed code={code} reason={reason:?}");
        }
        _ => {}
    }
}
