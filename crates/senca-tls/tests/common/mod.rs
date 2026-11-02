//! 試験の共通部品（メモリ上で client と server を繋ぐ）。
#![allow(dead_code)]

use std::sync::Arc;
use std::time::SystemTime;

use senca_tls::*;

pub const CA: &[u8] = include_bytes!("../data/ca.pem");
pub const OTHER_CA: &[u8] = include_bytes!("../data/other-ca.pem");
pub const ECDSA_CHAIN: &[u8] = include_bytes!("../data/server-ecdsa.pem");
pub const ECDSA_KEY: &[u8] = include_bytes!("../data/server-ecdsa.key");
pub const RSA_CHAIN: &[u8] = include_bytes!("../data/server-rsa.pem");
pub const RSA_KEY: &[u8] = include_bytes!("../data/server-rsa.key");
pub const ED_CHAIN: &[u8] = include_bytes!("../data/server-ed25519.pem");
pub const ED_KEY: &[u8] = include_bytes!("../data/server-ed25519.key");
pub const CLIENT_CHAIN: &[u8] = include_bytes!("../data/client.pem");
pub const CLIENT_KEY: &[u8] = include_bytes!("../data/client.key");
pub const EXPIRED: &[u8] = include_bytes!("../data/expired.pem");

pub fn now() -> SystemTime {
    SystemTime::now()
}

pub fn roots(pem: &[u8]) -> Arc<RootStore> {
    let mut r = RootStore::new();
    r.add_pem(pem).unwrap();
    Arc::new(r)
}

pub fn certified(chain: &[u8], key: &[u8]) -> Arc<CertifiedKey> {
    Arc::new(CertifiedKey::from_pem(chain, key).unwrap())
}

pub fn server_config(chain: &[u8], key: &[u8]) -> ServerConfig {
    ServerConfig::new(certified(chain, key)).unwrap()
}

pub fn client_config() -> ClientConfig {
    ClientConfig::new(ServerVerifier::WebPki {
        roots: roots(CA),
        spki_pins: Vec::new(),
    })
}

/// 片方の出力をもう片方へ渡す。どちらも出力がなくなるまで繰り返す。
pub fn pump(c: &mut Connection, s: &mut Connection) -> (Result<(), Error>, Result<(), Error>) {
    let mut rc = Ok(());
    let mut rs = Ok(());
    for _ in 0..64 {
        let mut moved = false;
        if c.wants_write() {
            let d = c.take_tls();
            moved = true;
            if rs.is_ok() {
                rs = feed(s, &d);
            }
        }
        if s.wants_write() {
            let d = s.take_tls();
            moved = true;
            if rc.is_ok() {
                rc = feed(c, &d);
            }
        }
        if !moved {
            break;
        }
    }
    (rc, rs)
}

pub fn feed(conn: &mut Connection, mut d: &[u8]) -> Result<(), Error> {
    while !d.is_empty() {
        let n = conn.read_tls(d)?;
        d = &d[n..];
        if n == 0 {
            // 平文が溜まりすぎ: 試験では捨てる。
            while conn.take_plaintext().is_some() {}
        }
    }
    Ok(())
}

pub fn connect(cc: ClientConfig, sc: ServerConfig, name: &str) -> (Connection, Connection) {
    let mut c = Connection::client(Arc::new(cc), name, now()).unwrap();
    let mut s = Connection::server(Arc::new(sc), now()).unwrap();
    let (rc, rs) = pump(&mut c, &mut s);
    rc.unwrap();
    rs.unwrap();
    assert!(!c.is_handshaking() && !s.is_handshaking());
    (c, s)
}

pub fn read_all(conn: &mut Connection) -> Vec<u8> {
    let mut v = Vec::new();
    while let Some(p) = conn.take_plaintext() {
        v.extend_from_slice(&p);
    }
    v
}

/// 双方向に平文を流して届くことを確かめる。
pub fn exchange(c: &mut Connection, s: &mut Connection, size: usize) {
    let msg: Vec<u8> = (0..size).map(|i| (i * 7 + 3) as u8).collect();
    assert_eq!(c.write_plaintext(&msg).unwrap(), size);
    let (rc, rs) = pump(c, s);
    rc.unwrap();
    rs.unwrap();
    assert_eq!(read_all(s), msg);
    assert_eq!(s.write_plaintext(&msg).unwrap(), size);
    let (rc, rs) = pump(c, s);
    rc.unwrap();
    rs.unwrap();
    assert_eq!(read_all(c), msg);
}

pub fn alert_of(e: &Error) -> Option<AlertDescription> {
    match e {
        Error::PeerAlert(a) => Some(*a),
        other => other.alert(),
    }
}
