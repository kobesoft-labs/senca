//! 相互接続試験用の HTTPS server（std の同期 I/O、接続ごとにスレッド）。
//!
//! ```text
//! tls_server --port 48443 --cert chain.pem --key key.pem [--sni example.test=chain.pem:key.pem]
//!            [--suites aes128,aes256,chacha] [--groups x25519,p256] [--alpn http/1.1,h3]
//!            [--client-ca ca.pem | --client-ca-optional ca.pem] [--key-update] [--record-size-limit N]
//! ```
//!
//! どんな要求にも 200 と交渉の結果（suite・group・resumed・hrr・alpn・sni・client_cert）を返す。

#[path = "common/args.rs"]
mod args;
#[path = "common/blocking.rs"]
mod blocking;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::SystemTime;

use args::*;
use blocking::TlsStream;
use senca_tls::*;

fn main() {
    let a = Args(std::env::args().skip(1).collect());
    let port: u16 = a.get("--port").unwrap_or("48443").parse().unwrap();
    let cert = Arc::new(CertifiedKey::from_pem_files(a.get("--cert").unwrap(), a.get("--key").unwrap()).unwrap());
    let mut cfg = ServerConfig::new(cert).unwrap();
    for s in a.all("--sni") {
        let (name, files) = s.split_once('=').unwrap();
        let (c, k) = files.split_once(':').unwrap();
        cfg.add_sni_cert(name, Arc::new(CertifiedKey::from_pem_files(c, k).unwrap()));
    }
    if let Some(s) = a.get("--suites") {
        cfg.cipher_suites = parse_suites(s);
    }
    if let Some(s) = a.get("--groups") {
        cfg.groups = parse_groups(s);
    }
    if let Some(s) = a.get("--alpn") {
        cfg.alpn_protocols = parse_alpn(s);
    }
    if let Some(s) = a.get("--record-size-limit") {
        cfg.record_size_limit = Some(s.parse().unwrap());
    }
    if let Some(f) = a.get("--client-ca") {
        let mut r = RootStore::new();
        r.add_pem_file(f).unwrap();
        cfg.client_auth = ClientAuth::Required(Arc::new(r));
    }
    if let Some(f) = a.get("--client-ca-optional") {
        let mut r = RootStore::new();
        r.add_pem_file(f).unwrap();
        cfg.client_auth = ClientAuth::Optional(Arc::new(r));
    }
    cfg.tickets_per_handshake = 2;
    let key_update = a.has("--key-update");
    let cfg = Arc::new(cfg);
    let l = TcpListener::bind(("127.0.0.1", port)).unwrap();
    eprintln!("listening on 127.0.0.1:{port}");
    for sock in l.incoming() {
        let Ok(sock) = sock else { continue };
        let cfg = cfg.clone();
        std::thread::spawn(move || {
            let conn = match Connection::server(cfg, SystemTime::now()) {
                Ok(c) => c,
                Err(e) => return eprintln!("error: {e}"),
            };
            let mut s = TlsStream::new(conn, sock);
            if let Err(e) = serve(&mut s, key_update) {
                eprintln!("error: {e} ({:?})", s.conn.error());
            }
        });
    }
}

fn serve(s: &mut TlsStream<Connection, std::net::TcpStream>, key_update: bool) -> std::io::Result<()> {
    s.complete_handshake()?;
    let info = s.conn.info();
    let summary = format!(
        "suite={} group={:?} resumed={} hrr={} alpn={} sni={} client_cert={}",
        info.cipher_suite.map(|c| c.name()).unwrap_or("-"),
        info.group,
        info.resumed,
        info.hello_retry,
        info.alpn.as_deref().map(|p| String::from_utf8_lossy(p).into_owned()).unwrap_or("-".into()),
        info.server_name.as_deref().unwrap_or("-"),
        if s.conn.peer_certificates().is_empty() { "no" } else { "yes" },
    );
    eprintln!("handshake ok: {summary}");
    // 要求の終わり（空行）まで読む。
    let mut req = Vec::new();
    let mut buf = [0u8; 4096];
    while !req.windows(4).any(|w| w == b"\r\n\r\n") && !req.windows(2).any(|w| w == b"\n\n") {
        let n = s.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        req.extend_from_slice(&buf[..n]);
        if req.len() > 64 * 1024 {
            return Ok(());
        }
    }
    if key_update {
        s.conn.update_keys(true).map_err(std::io::Error::other)?;
    }
    let body = format!("{summary}\n");
    write!(
        s,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    s.close()?;
    // 相手の close_notify を少し待つ。
    let _ = s.read(&mut buf);
    Ok(())
}
