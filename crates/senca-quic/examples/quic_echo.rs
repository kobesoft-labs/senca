//! 相互接続試験用の QUIC echo（std の同期 UDP、1 スレッド）。
//!
//! ```text
//! quic_echo server --listen 127.0.0.1:51000 --cert chain.pem --key key.pem [--alpn senca-echo] [--retry]
//! quic_echo client --connect 127.0.0.1:51000 --ca ca.pem [--pin-spki HEX] [--name localhost] [--alpn senca-echo]
//!                  [--bytes N] [--key-update] [--migrate]
//! ```
//!
//! server: 相手が開いた bidi stream のデータをそのまま返し、FIN で FIN を返す。統計を標準エラーへ。
//! client: N バイトを送り、同じものが返るか確かめる（終了コード 0 なら一致）。

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::BytesMut;
use senca_quic::*;
use senca_tls::{CertifiedKey, RootStore, ServerVerifier};

fn arg(args: &[String], k: &str) -> Option<String> {
    args.iter().position(|a| a == k).and_then(|i| args.get(i + 1).cloned())
}
fn flag(args: &[String], k: &str) -> bool {
    args.iter().any(|a| a == k)
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// UDP の送受信を一回まわす。
fn pump(sock: &UdpSocket, ep: &mut Endpoint, buf: &mut [u8]) {
    let now = Instant::now();
    let wait = ep
        .poll_timeout()
        .map(|t| t.saturating_duration_since(now))
        .unwrap_or(Duration::from_millis(50))
        .clamp(Duration::from_micros(200), Duration::from_millis(50));
    sock.set_read_timeout(Some(wait)).ok();
    if let Ok((n, from)) = sock.recv_from(buf) {
        ep.handle_datagram(Instant::now(), SystemTime::now(), from, BytesMut::from(&buf[..n]));
        // 溜まっている分も読む
        sock.set_nonblocking(true).ok();
        while let Ok((n, from)) = sock.recv_from(buf) {
            ep.handle_datagram(Instant::now(), SystemTime::now(), from, BytesMut::from(&buf[..n]));
        }
        sock.set_nonblocking(false).ok();
    }
    ep.handle_timeout(Instant::now());
}

fn flush(sock: &UdpSocket, ep: &mut Endpoint) {
    while let Some(t) = ep.poll_transmit(Instant::now()) {
        let _ = sock.send_to(&t.contents, t.destination);
    }
}

fn server(args: &[String]) {
    let listen: SocketAddr = arg(args, "--listen").unwrap_or("127.0.0.1:51000".into()).parse().unwrap();
    let cert = CertifiedKey::from_pem_files(arg(args, "--cert").unwrap(), arg(args, "--key").unwrap()).unwrap();
    let mut tls = senca_tls::ServerConfig::new(Arc::new(cert)).unwrap();
    tls.alpn_protocols = arg(args, "--alpn")
        .unwrap_or("senca-echo".into())
        .split(',')
        .map(|s| s.as_bytes().to_vec())
        .collect();
    let mut sc = ServerConfig::new(Arc::new(tls));
    sc.retry = flag(args, "--retry");
    let mut ep = Endpoint::new(EndpointConfig::new().unwrap(), Some(Arc::new(sc)));
    let sock = UdpSocket::bind(listen).unwrap();
    eprintln!("listening on {listen}");
    let mut buf = vec![0u8; 65536];
    let mut pending: HashMap<(ConnectionHandle, StreamId), Vec<u8>> = HashMap::new();
    loop {
        pump(&sock, &mut ep, &mut buf);
        let hs: Vec<_> = ep.connections().collect();
        for h in hs {
            let c = ep.connection_mut(h).unwrap();
            while let Some(e) = c.poll_event() {
                match e {
                    Event::StreamOpened { id } | Event::StreamReadable { id } => {
                        while let Ok(ch) = c.stream_recv(id, 65536) {
                            pending.entry((h, id)).or_default().extend_from_slice(&ch.data);
                            if ch.fin {
                                pending.insert((h, StreamId(id.0 | 1 << 62)), Vec::new()); // FIN の印
                                break;
                            }
                            if ch.data.is_empty() {
                                break;
                            }
                        }
                    }
                    Event::ConnectionLost(r) => eprintln!("connection {h:?} closed: {r}"),
                    Event::PathMigrated { remote } => eprintln!("connection {h:?} migrated to {remote}"),
                    Event::KeyUpdated => eprintln!("connection {h:?} key update"),
                    Event::HandshakeCompleted => eprintln!(
                        "connection {h:?} established alpn={:?} suite={:?}",
                        c.alpn().map(String::from_utf8_lossy),
                        c.tls().cipher_suite()
                    ),
                    _ => {}
                }
            }
            // echo（返せた分だけ消す）
            let keys: Vec<_> = pending.keys().filter(|(hh, id)| *hh == h && id.0 >> 62 == 0).copied().collect();
            for k in keys {
                let data = pending.get_mut(&k).unwrap();
                if !data.is_empty()
                    && let Ok(n) = c.stream_send(k.1, data)
                {
                    data.drain(..n);
                }
                let fin_key = (h, StreamId(k.1.0 | 1 << 62));
                if data.is_empty() && pending.contains_key(&fin_key) {
                    let _ = c.stream_finish(k.1);
                    pending.remove(&fin_key);
                    pending.remove(&k);
                }
            }
        }
        flush(&sock, &mut ep);
        while let Some(e) = ep.poll_event() {
            if let EndpointEvent::Drained(h) = e {
                pending.retain(|(hh, _), _| *hh != h);
            }
        }
    }
}

fn client(args: &[String]) -> i32 {
    let remote: SocketAddr = arg(args, "--connect").unwrap().parse().unwrap();
    let verifier = if let Some(p) = arg(args, "--pin-spki") {
        ServerVerifier::PinnedSpkiOnly(vec![hex(&p).try_into().unwrap()])
    } else {
        let mut r = RootStore::new();
        r.add_pem_file(arg(args, "--ca").unwrap()).unwrap();
        ServerVerifier::WebPki {
            roots: Arc::new(r),
            spki_pins: vec![],
        }
    };
    let mut tls = senca_tls::ClientConfig::new(verifier);
    tls.alpn_protocols = vec![arg(args, "--alpn").unwrap_or("senca-echo".into()).into_bytes()];
    let cc = ClientConfig::new(Arc::new(tls));
    let mut ep = Endpoint::new(EndpointConfig::new().unwrap(), None);
    let mut sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let name = arg(args, "--name").unwrap_or("localhost".into());
    let h = ep.connect(Instant::now(), SystemTime::now(), cc, remote, &name).unwrap();
    let total: usize = arg(args, "--bytes").unwrap_or("1000000".into()).parse().unwrap();
    let data: Vec<u8> = (0..total).map(|i| (i * 7 % 251) as u8).collect();
    let mut buf = vec![0u8; 65536];
    let start = Instant::now();
    let mut stream = None;
    let mut sent = 0;
    let mut got = Vec::new();
    let mut did_key_update = false;
    let mut did_migrate = false;
    loop {
        if start.elapsed() > Duration::from_secs(30) {
            eprintln!("timeout: sent {sent} got {}", got.len());
            return 1;
        }
        pump(&sock, &mut ep, &mut buf);
        let Some(c) = ep.connection_mut(h) else {
            eprintln!("connection gone");
            return 1;
        };
        while let Some(e) = c.poll_event() {
            match e {
                Event::HandshakeCompleted => eprintln!(
                    "established alpn={:?} suite={:?}",
                    c.alpn().map(String::from_utf8_lossy),
                    c.tls().cipher_suite()
                ),
                Event::ConnectionLost(r) => {
                    eprintln!("closed: {r}");
                    return 1;
                }
                Event::KeyUpdated => eprintln!("key update"),
                _ => {}
            }
        }
        if c.is_established() && stream.is_none() {
            stream = c.open_stream(Dir::Bi);
        }
        if let Some(id) = stream {
            if sent < data.len() {
                sent += c.stream_send(id, &data[sent..]).unwrap_or(0);
                if sent == data.len() {
                    c.stream_finish(id).unwrap();
                }
            }
            loop {
                match c.stream_recv(id, 65536) {
                    Ok(ch) => {
                        got.extend_from_slice(&ch.data);
                        if ch.fin {
                            let ok = got == data;
                            eprintln!(
                                "echo {} bytes in {:?}: {} stats={:?}",
                                got.len(),
                                start.elapsed(),
                                if ok { "match" } else { "MISMATCH" },
                                c.stats()
                            );
                            c.close(Instant::now(), 0, b"done");
                            flush(&sock, &mut ep);
                            return if ok { 0 } else { 1 };
                        }
                        if ch.data.is_empty() {
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("stream error {e}");
                        return 1;
                    }
                }
            }
            if flag(args, "--key-update") && !did_key_update && got.len() > total / 4 {
                did_key_update = c.initiate_key_update(Instant::now());
                if did_key_update {
                    eprintln!("initiated key update");
                }
            }
            if flag(args, "--migrate") && !did_migrate && got.len() > total / 2 && c.migrate(Instant::now()) {
                did_migrate = true;
                let new = UdpSocket::bind("127.0.0.1:0").unwrap();
                eprintln!("migrating to {}", new.local_addr().unwrap());
                sock = new;
            }
        }
        flush(&sock, &mut ep);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("server") => server(&args),
        Some("client") => std::process::exit(client(&args)),
        _ => {
            eprintln!("usage: quic_echo server|client ...");
            std::process::exit(2);
        }
    }
}
