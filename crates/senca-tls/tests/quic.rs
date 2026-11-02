//! QUIC モードの試験: 段階ごとにハンドシェイクのバイト列を渡し、両者の secret が一致するか。

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::*;
use senca_tls::*;

#[derive(Default)]
struct Seen {
    read: HashMap<Level, Secret>,
    write: HashMap<Level, Secret>,
    complete: bool,
}

fn drain(from: &mut QuicConnection, seen: &mut Seen, to: &mut QuicConnection) -> Result<bool, Error> {
    let mut moved = false;
    while let Some(o) = from.poll_output() {
        moved = true;
        match o {
            QuicOutput::Data { level, data } => to.read_handshake(level, &data)?,
            QuicOutput::ReadSecret { level, secret, .. } => {
                seen.read.insert(level, secret);
            }
            QuicOutput::WriteSecret { level, secret, .. } => {
                seen.write.insert(level, secret);
            }
            QuicOutput::HandshakeComplete => seen.complete = true,
        }
    }
    Ok(moved)
}

fn run(c: &mut QuicConnection, s: &mut QuicConnection) -> Result<(Seen, Seen), Error> {
    let mut cs = Seen::default();
    let mut ss = Seen::default();
    for _ in 0..16 {
        let a = drain(c, &mut cs, s)?;
        let b = drain(s, &mut ss, c)?;
        if !a && !b {
            break;
        }
    }
    Ok((cs, ss))
}

fn quic_configs() -> (ClientConfig, ServerConfig) {
    let mut cc = client_config();
    cc.alpn_protocols = vec![ALPN_H3.to_vec()];
    let mut sc = server_config(ECDSA_CHAIN, ECDSA_KEY);
    sc.alpn_protocols = vec![ALPN_MOQ00.to_vec(), ALPN_H3.to_vec()];
    (cc, sc)
}

fn check_secrets(cs: &Seen, ss: &Seen) {
    for level in [Level::Handshake, Level::OneRtt] {
        assert_eq!(cs.write[&level], ss.read[&level], "{level:?}");
        assert_eq!(cs.read[&level], ss.write[&level], "{level:?}");
        assert_ne!(cs.write[&level], cs.read[&level]);
    }
    assert!(cs.complete && ss.complete);
}

#[test]
fn quic_handshake_secrets_match() {
    for suite in CipherSuite::ALL {
        let (mut cc, sc) = quic_configs();
        cc.cipher_suites = vec![suite];
        let mut c = QuicConnection::client(Arc::new(cc), "localhost", b"client-tp".to_vec(), now()).unwrap();
        let mut s = QuicConnection::server(Arc::new(sc), b"server-tp".to_vec(), now()).unwrap();
        let (cs, ss) = run(&mut c, &mut s).unwrap();
        check_secrets(&cs, &ss);
        assert_eq!(c.peer_transport_parameters(), Some(&b"server-tp"[..]));
        assert_eq!(s.peer_transport_parameters(), Some(&b"client-tp"[..]));
        assert_eq!(c.alpn_protocol(), Some(ALPN_H3));
        assert_eq!(c.cipher_suite(), Some(suite));
        // 鍵の導出
        let k = quic::packet_keys(suite, &cs.write[&Level::OneRtt]).unwrap();
        assert_eq!(k.key.as_bytes().len(), suite.key_len());
        let n1 = quic::next_secret(suite, &cs.write[&Level::OneRtt]).unwrap();
        let n2 = quic::next_secret(suite, &ss.read[&Level::OneRtt]).unwrap();
        assert_eq!(n1, n2);
        assert_eq!(
            c.export_keying_material(b"x", b"", 16).unwrap(),
            s.export_keying_material(b"x", b"", 16).unwrap()
        );
    }
}

#[test]
fn quic_hrr_mtls_and_resumption() {
    let (mut cc, mut sc) = quic_configs();
    sc.groups = vec![NamedGroup::Secp256r1];
    cc.client_cert = Some(certified(CLIENT_CHAIN, CLIENT_KEY));
    sc.client_auth = ClientAuth::Required(roots(CA));
    let (cc, sc) = (Arc::new(cc), Arc::new(sc));
    let mut c = QuicConnection::client(cc.clone(), "localhost", vec![1], now()).unwrap();
    let mut s = QuicConnection::server(sc.clone(), vec![2], now()).unwrap();
    let (cs, ss) = run(&mut c, &mut s).unwrap();
    check_secrets(&cs, &ss);
    assert_eq!(s.peer_certificates().len(), 1);

    // 再開（mTLS なしの設定で）
    let (cc, sc) = quic_configs();
    let (cc, sc) = (Arc::new(cc), Arc::new(sc));
    for i in 0..2 {
        let mut c = QuicConnection::client(cc.clone(), "localhost", vec![1], now()).unwrap();
        let mut s = QuicConnection::server(sc.clone(), vec![2], now()).unwrap();
        let (cs, ss) = run(&mut c, &mut s).unwrap();
        check_secrets(&cs, &ss);
        assert_eq!(c.resumed(), i == 1);
        assert_eq!(s.resumed(), i == 1);
    }
}

#[test]
fn quic_rejects_wrong_level_and_missing_parameters() {
    let (cc, sc) = quic_configs();
    let mut c = QuicConnection::client(Arc::new(cc.clone()), "localhost", vec![1], now()).unwrap();
    let Some(QuicOutput::Data { data, .. }) = c.poll_output() else { panic!() };
    let mut s = QuicConnection::server(Arc::new(sc.clone()), vec![2], now()).unwrap();
    let e = s.read_handshake(Level::Handshake, &data).unwrap_err();
    assert_eq!(e.alert(), Some(AlertDescription::UNEXPECTED_MESSAGE));

    // TCP 用の ClientHello（transport parameters なし）
    let mut tcp = Connection::client(Arc::new(cc.clone()), "localhost", now()).unwrap();
    let rec = tcp.take_tls();
    let mut s = QuicConnection::server(Arc::new(sc.clone()), vec![2], now()).unwrap();
    let e = s.read_handshake(Level::Initial, &rec[5..]).unwrap_err();
    assert_eq!(e.alert(), Some(AlertDescription::MISSING_EXTENSION));

    // ALPN のない QUIC は作れない
    let mut cc2 = cc.clone();
    cc2.alpn_protocols.clear();
    assert!(matches!(
        QuicConnection::client(Arc::new(cc2), "localhost", vec![], now()),
        Err(Error::Config(_))
    ));
}
