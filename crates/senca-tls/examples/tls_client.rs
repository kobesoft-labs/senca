//! 相互接続試験用の HTTPS client（std の同期 I/O）。
//!
//! ```text
//! tls_client --connect 127.0.0.1:4433 --name localhost --ca ca.pem [--suites ..] [--groups ..]
//!            [--alpn http/1.1] [--cert c.pem --key c.key] [--pin <sha256 hex>] [--count N]
//!            [--key-update] [--record-size-limit N]
//! ```
//!
//! `GET /` を送り、応答を標準出力へ、交渉の結果を標準エラーへ書く。`--count N` なら同じ設定で
//! N 回繋ぐ（2 回目以降はチケットで再開する）。

#[path = "common/args.rs"]
mod args;
#[path = "common/blocking.rs"]
mod blocking;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::SystemTime;

use args::*;
use blocking::TlsStream;
use senca_tls::*;

fn main() {
    let a = Args(std::env::args().skip(1).collect());
    let addr = a.get("--connect").unwrap();
    let name = a.get("--name").unwrap_or("localhost");
    let pins: Vec<[u8; 32]> = a
        .all("--pin")
        .iter()
        .map(|p| unhex(p).try_into().expect("pin は 32 バイト"))
        .collect();
    let verifier = match a.get("--ca") {
        Some(f) => {
            let mut r = RootStore::new();
            r.add_pem_file(f).unwrap();
            ServerVerifier::WebPki {
                roots: Arc::new(r),
                spki_pins: pins,
            }
        }
        None if !pins.is_empty() => ServerVerifier::PinnedSpkiOnly(pins),
        None => ServerVerifier::WebPki {
            roots: Arc::new(RootStore::system().unwrap()),
            spki_pins: pins,
        },
    };
    let mut cfg = ClientConfig::new(verifier);
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
    if let (Some(c), Some(k)) = (a.get("--cert"), a.get("--key")) {
        cfg.client_cert = Some(Arc::new(CertifiedKey::from_pem_files(c, k).unwrap()));
    }
    let count: usize = a.get("--count").unwrap_or("1").parse().unwrap();
    let cfg = Arc::new(cfg);
    let mut failed = false;
    for i in 0..count {
        match fetch(&cfg, addr, name, a.has("--key-update")) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("connection {i}: error: {e}");
                failed = true;
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}

fn fetch(cfg: &Arc<ClientConfig>, addr: &str, name: &str, key_update: bool) -> std::io::Result<()> {
    let sock = TcpStream::connect(addr)?;
    let conn = Connection::client(cfg.clone(), name, SystemTime::now()).map_err(std::io::Error::other)?;
    let mut s = TlsStream::new(conn, sock);
    s.complete_handshake()?;
    let info = s.conn.info();
    eprintln!(
        "handshake ok: suite={} group={:?} resumed={} hrr={} alpn={}",
        info.cipher_suite.map(|c| c.name()).unwrap_or("-"),
        info.group,
        info.resumed,
        info.hello_retry,
        info.alpn.as_deref().map(|p| String::from_utf8_lossy(p).into_owned()).unwrap_or("-".into()),
    );
    if key_update {
        s.conn.update_keys(true).map_err(std::io::Error::other)?;
    }
    write!(s, "GET / HTTP/1.0\r\nHost: {name}\r\n\r\n")?;
    let mut resp = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => resp.extend_from_slice(&buf[..n]),
            Err(e) => {
                if resp.is_empty() {
                    return Err(e);
                }
                break;
            }
        }
    }
    let _ = s.close();
    std::io::stdout().write_all(&resp)?;
    eprintln!("close_notify received: {}", s.conn.peer_closed());
    Ok(())
}
