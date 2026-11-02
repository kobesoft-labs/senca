//! 厳格さの試験: 格下げ・CCS の誤用・大きすぎるメッセージ・壊れた入力。

mod common;

use std::sync::Arc;

use common::*;
use senca_tls::*;

fn record(typ: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![typ, 3, 3, (body.len() >> 8) as u8, body.len() as u8];
    v.extend_from_slice(body);
    v
}

fn hs(typ: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![typ, (body.len() >> 16) as u8, (body.len() >> 8) as u8, body.len() as u8];
    v.extend_from_slice(body);
    v
}

/// client の ClientHello から legacy_session_id を取り出す。
fn session_id(ch_record: &[u8]) -> Vec<u8> {
    let at = 5 + 4 + 2 + 32;
    let n = ch_record[at] as usize;
    ch_record[at + 1..at + 1 + n].to_vec()
}

fn tls12_server_hello(sid: &[u8], sentinel: bool) -> Vec<u8> {
    let mut b = vec![3, 3];
    let mut random = [0x11u8; 32];
    if sentinel {
        random[24..].copy_from_slice(b"DOWNGRD\x01");
    }
    b.extend_from_slice(&random);
    b.push(sid.len() as u8);
    b.extend_from_slice(sid);
    b.extend_from_slice(&[0xc0, 0x2f, 0]);
    b.extend_from_slice(&[0, 0]);
    record(22, &hs(2, &b))
}

#[test]
fn downgrade_is_rejected() {
    for (sentinel, want) in [
        (true, AlertDescription::ILLEGAL_PARAMETER),
        (false, AlertDescription::PROTOCOL_VERSION),
    ] {
        let mut c = Connection::client(Arc::new(client_config()), "localhost", now()).unwrap();
        let ch = c.take_tls();
        let e = feed(&mut c, &tls12_server_hello(&session_id(&ch), sentinel)).unwrap_err();
        assert_eq!(e.alert(), Some(want));
        // 平文の alert を返している
        assert_eq!(c.take_tls(), vec![21, 3, 3, 0, 2, 2, want.0]);
    }
}

#[test]
fn tls12_only_client_hello_is_rejected() {
    let mut b = vec![3, 3];
    b.extend_from_slice(&[0x22; 32]);
    b.push(0);
    b.extend_from_slice(&[0, 2, 0xc0, 0x2f, 1, 0, 0, 0]);
    let mut s = Connection::server(Arc::new(server_config(ECDSA_CHAIN, ECDSA_KEY)), now()).unwrap();
    let e = feed(&mut s, &record(22, &hs(1, &b))).unwrap_err();
    assert_eq!(e.alert(), Some(AlertDescription::PROTOCOL_VERSION));
}

#[test]
fn ccs_rules() {
    // ClientHello より前の CCS は不正。
    let mut s = Connection::server(Arc::new(server_config(ECDSA_CHAIN, ECDSA_KEY)), now()).unwrap();
    let e = feed(&mut s, &record(20, &[1])).unwrap_err();
    assert_eq!(e.alert(), Some(AlertDescription::UNEXPECTED_MESSAGE));

    // ハンドシェイク中の CCS（0x01）は捨てる。値が違えば不正。
    let mut c = Connection::client(Arc::new(client_config()), "localhost", now()).unwrap();
    let mut s = Connection::server(Arc::new(server_config(ECDSA_CHAIN, ECDSA_KEY)), now()).unwrap();
    feed(&mut s, &c.take_tls()).unwrap();
    feed(&mut s, &record(20, &[1])).unwrap();
    let e = feed(&mut s, &record(20, &[2])).unwrap_err();
    assert_eq!(e.alert(), Some(AlertDescription::UNEXPECTED_MESSAGE));

    // 完了後の CCS は不正。
    let (mut c, _s) = connect(client_config(), server_config(ECDSA_CHAIN, ECDSA_KEY), "localhost");
    let e = feed(&mut c, &record(20, &[1])).unwrap_err();
    assert_eq!(e.alert(), Some(AlertDescription::UNEXPECTED_MESSAGE));
}

#[test]
fn oversized_and_malformed_messages() {
    // 64 KiB を超える ClientHello の宣言
    let mut s = Connection::server(Arc::new(server_config(ECDSA_CHAIN, ECDSA_KEY)), now()).unwrap();
    let e = feed(&mut s, &record(22, &[1, 0x01, 0x00, 0x01])).unwrap_err();
    assert_eq!(e.alert(), Some(AlertDescription::ILLEGAL_PARAMETER));
    // 長すぎる記録
    let mut s = Connection::server(Arc::new(server_config(ECDSA_CHAIN, ECDSA_KEY)), now()).unwrap();
    let e = feed(&mut s, &[22, 3, 3, 0x40, 0x01]).unwrap_err();
    assert_eq!(e.alert(), Some(AlertDescription::RECORD_OVERFLOW));
    // 平文の app data
    let mut s = Connection::server(Arc::new(server_config(ECDSA_CHAIN, ECDSA_KEY)), now()).unwrap();
    let e = feed(&mut s, &record(23, b"GET /")).unwrap_err();
    assert_eq!(e.alert(), Some(AlertDescription::UNEXPECTED_MESSAGE));
    // 空のハンドシェイク記録
    let mut s = Connection::server(Arc::new(server_config(ECDSA_CHAIN, ECDSA_KEY)), now()).unwrap();
    let e = feed(&mut s, &record(22, &[])).unwrap_err();
    assert_eq!(e.alert(), Some(AlertDescription::UNEXPECTED_MESSAGE));
    // 知らない記録の種類
    let mut s = Connection::server(Arc::new(server_config(ECDSA_CHAIN, ECDSA_KEY)), now()).unwrap();
    assert!(feed(&mut s, &record(24, &[0])).is_err());
}

#[test]
fn plaintext_handshake_after_encryption_is_rejected() {
    // 暗号化の開始後に平文の ClientHello（再ネゴシエーションの試み）→ unexpected_message
    let (_c, mut s) = connect(client_config(), server_config(ECDSA_CHAIN, ECDSA_KEY), "localhost");
    let e = feed(&mut s, &record(22, &hs(1, &[3, 3]))).unwrap_err();
    assert_eq!(e.alert(), Some(AlertDescription::UNEXPECTED_MESSAGE));
}

#[test]
fn renegotiation_is_rejected_quic() {
    // 完了後の 1-RTT 段階の ClientHello は unexpected_message（再ネゴシエーションはない）。
    let mut cc = client_config();
    cc.alpn_protocols = vec![b"h3".to_vec()];
    let mut sc = server_config(ECDSA_CHAIN, ECDSA_KEY);
    sc.alpn_protocols = vec![b"h3".to_vec()];
    let cc = Arc::new(cc);
    let mut c = QuicConnection::client(cc.clone(), "localhost", vec![], now()).unwrap();
    let mut s = QuicConnection::server(Arc::new(sc), vec![], now()).unwrap();
    let mut ch = Vec::new();
    for _ in 0..8 {
        while let Some(o) = c.poll_output() {
            if let QuicOutput::Data { level, data } = o {
                if ch.is_empty() {
                    ch = data.clone();
                }
                s.read_handshake(level, &data).unwrap();
            }
        }
        while let Some(o) = s.poll_output() {
            if let QuicOutput::Data { level, data } = o {
                c.read_handshake(level, &data).unwrap();
            }
        }
    }
    assert!(!s.is_handshaking());
    let e = s.read_handshake(Level::OneRtt, &ch).unwrap_err();
    assert_eq!(e.alert(), Some(AlertDescription::UNEXPECTED_MESSAGE));
}

/// 決まった種からの xorshift。
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn mutated_inputs_never_panic() {
    let cc = Arc::new(client_config());
    let sc = Arc::new(server_config(ECDSA_CHAIN, ECDSA_KEY));
    let mut c = Connection::client(cc.clone(), "localhost", now()).unwrap();
    let ch = c.take_tls();
    let mut s = Connection::server(sc.clone(), now()).unwrap();
    feed(&mut s, &ch).unwrap();
    let flight = s.take_tls();
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for i in 0..800 {
        let mut m = if i % 2 == 0 { ch.clone() } else { flight.clone() };
        let flips = 1 + (rng.next() % 4) as usize;
        for _ in 0..flips {
            let at = (rng.next() as usize) % m.len();
            m[at] = rng.next() as u8;
        }
        if rng.next().is_multiple_of(5) {
            let cut = (rng.next() as usize) % m.len();
            m.truncate(cut);
        }
        if i % 2 == 0 {
            let mut s = Connection::server(sc.clone(), now()).unwrap();
            let _ = feed(&mut s, &m);
        } else {
            let mut c = Connection::client(cc.clone(), "localhost", now()).unwrap();
            let _ = c.take_tls();
            // セッション ID・乱数が違うので多くは不一致で落ちる。解析の道筋を通すことが目的。
            let _ = feed(&mut c, &m);
        }
    }
}
