//! TLS 1.3 の server（Connection::server）に client 側の記録層のバイト列を与える。
//! 先頭 1 バイトで証明書（ECDSA・RSA・Ed25519）と区切りの大きさを選ぶ。
#![no_main]
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use libfuzzer_sys::fuzz_target;
use senca_tls::{CertifiedKey, Connection, ServerConfig, TlsSession};

const ECDSA_CHAIN: &[u8] = include_bytes!("../../crates/senca-tls/tests/data/server-ecdsa.pem");
const ECDSA_KEY: &[u8] = include_bytes!("../../crates/senca-tls/tests/data/server-ecdsa.key");
const RSA_CHAIN: &[u8] = include_bytes!("../../crates/senca-tls/tests/data/server-rsa.pem");
const RSA_KEY: &[u8] = include_bytes!("../../crates/senca-tls/tests/data/server-rsa.key");
const ED_CHAIN: &[u8] = include_bytes!("../../crates/senca-tls/tests/data/server-ed25519.pem");
const ED_KEY: &[u8] = include_bytes!("../../crates/senca-tls/tests/data/server-ed25519.key");

fn configs() -> &'static [Arc<ServerConfig>; 3] {
    static C: OnceLock<[Arc<ServerConfig>; 3]> = OnceLock::new();
    C.get_or_init(|| {
        let mk = |c: &[u8], k: &[u8]| {
            let ck = Arc::new(CertifiedKey::from_pem(c, k).expect("cert"));
            let mut cfg = ServerConfig::new(ck).expect("config");
            cfg.alpn_protocols = vec![b"http/1.1".to_vec(), b"h3".to_vec()];
            Arc::new(cfg)
        };
        [mk(ECDSA_CHAIN, ECDSA_KEY), mk(RSA_CHAIN, RSA_KEY), mk(ED_CHAIN, ED_KEY)]
    })
}

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else { return };
    let cfg = configs()[(sel % 3) as usize].clone();
    let Ok(mut c) = Connection::server(cfg, SystemTime::now()) else { return };
    let step = 1 + (sel >> 2) as usize * 17;
    for chunk in rest.chunks(step) {
        let mut off = 0;
        while off < chunk.len() {
            match c.read_tls(&chunk[off..]) {
                Ok(0) => {
                    // 平文が溜まっている。取り出してからもう一度。
                    if c.take_plaintext().is_none() {
                        break;
                    }
                }
                Ok(n) => off += n,
                Err(_) => {
                    let _ = c.take_tls();
                    return;
                }
            }
        }
        while c.take_plaintext().is_some() {}
        if c.wants_write() {
            let _ = c.take_tls();
        }
    }
    if !c.is_handshaking() {
        let _ = c.write_plaintext(b"HTTP/1.1 200 OK\r\n\r\n");
        let _ = c.take_tls();
    }
});
