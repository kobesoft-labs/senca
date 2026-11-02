//! client と server をメモリ上で繋ぐ試験。

mod common;

use std::sync::Arc;

use common::*;
use senca_tls::*;

#[test]
fn all_suites_groups_and_key_types() {
    for (chain, key) in [(ECDSA_CHAIN, ECDSA_KEY), (RSA_CHAIN, RSA_KEY), (ED_CHAIN, ED_KEY)] {
        for suite in CipherSuite::ALL {
            for group in NamedGroup::ALL {
                let mut cc = client_config();
                cc.cipher_suites = vec![suite];
                cc.groups = vec![group];
                let sc = server_config(chain, key);
                let (mut c, mut s) = connect(cc, sc, "localhost");
                let ci = c.info();
                assert_eq!(ci.cipher_suite, Some(suite));
                assert_eq!(ci.group, Some(group));
                assert!(!ci.hello_retry);
                assert_eq!(s.info().cipher_suite, Some(suite));
                assert_eq!(c.peer_certificates().len(), 2);
                exchange(&mut c, &mut s, 50_000);
                assert_eq!(
                    c.export_keying_material(b"EXPORTER-test", b"ctx", 32).unwrap(),
                    s.export_keying_material(b"EXPORTER-test", b"ctx", 32).unwrap()
                );
            }
        }
    }
}

#[test]
fn hello_retry_request_when_share_missing() {
    let mut cc = client_config();
    cc.groups = vec![NamedGroup::X25519, NamedGroup::Secp256r1];
    let mut sc = server_config(ECDSA_CHAIN, ECDSA_KEY);
    sc.groups = vec![NamedGroup::Secp256r1];
    let (mut c, mut s) = connect(cc, sc, "localhost");
    assert!(c.info().hello_retry);
    assert!(s.info().hello_retry);
    assert_eq!(c.info().group, Some(NamedGroup::Secp256r1));
    exchange(&mut c, &mut s, 100);
}

#[test]
fn no_common_group_fails() {
    let mut cc = client_config();
    cc.groups = vec![NamedGroup::X25519];
    let mut sc = server_config(ECDSA_CHAIN, ECDSA_KEY);
    sc.groups = vec![NamedGroup::Secp256r1];
    let mut c = Connection::client(Arc::new(cc), "localhost", now()).unwrap();
    let mut s = Connection::server(Arc::new(sc), now()).unwrap();
    let (rc, rs) = pump(&mut c, &mut s);
    assert_eq!(alert_of(&rs.unwrap_err()), Some(AlertDescription::HANDSHAKE_FAILURE));
    assert_eq!(rc.unwrap_err(), Error::PeerAlert(AlertDescription::HANDSHAKE_FAILURE));
}

#[test]
fn ticket_resumption() {
    let cc = Arc::new(client_config());
    let sc = Arc::new(server_config(ECDSA_CHAIN, ECDSA_KEY));
    let mut c = Connection::client(cc.clone(), "localhost", now()).unwrap();
    let mut s = Connection::server(sc.clone(), now()).unwrap();
    let (rc, rs) = pump(&mut c, &mut s);
    rc.unwrap();
    rs.unwrap();
    assert!(!c.info().resumed);
    exchange(&mut c, &mut s, 10);

    let mut c = Connection::client(cc.clone(), "localhost", now()).unwrap();
    let mut s = Connection::server(sc.clone(), now()).unwrap();
    let (rc, rs) = pump(&mut c, &mut s);
    rc.unwrap();
    rs.unwrap();
    assert!(c.info().resumed);
    assert!(s.info().resumed);
    exchange(&mut c, &mut s, 1000);

    // 別の server（チケット鍵が違う）では完全なハンドシェイクに戻る。
    let sc2 = Arc::new(server_config(ECDSA_CHAIN, ECDSA_KEY));
    let mut c = Connection::client(cc.clone(), "localhost", now()).unwrap();
    let mut s = Connection::server(sc2, now()).unwrap();
    let (rc, rs) = pump(&mut c, &mut s);
    rc.unwrap();
    rs.unwrap();
    assert!(!c.info().resumed);
}

#[test]
fn ticket_resumption_with_hrr_and_aes256() {
    let mut cc = client_config();
    cc.cipher_suites = vec![CipherSuite::Aes256GcmSha384];
    let cc = Arc::new(cc);
    let mut sc = server_config(RSA_CHAIN, RSA_KEY);
    sc.groups = vec![NamedGroup::Secp256r1];
    let sc = Arc::new(sc);
    for i in 0..3 {
        let mut c = Connection::client(cc.clone(), "localhost", now()).unwrap();
        let mut s = Connection::server(sc.clone(), now()).unwrap();
        let (rc, rs) = pump(&mut c, &mut s);
        rc.unwrap();
        rs.unwrap();
        assert!(c.info().hello_retry);
        assert_eq!(c.info().resumed, i > 0);
        exchange(&mut c, &mut s, 10);
    }
}

#[test]
fn tampered_record_is_rejected() {
    let (mut c, mut s) = connect(client_config(), server_config(ECDSA_CHAIN, ECDSA_KEY), "localhost");
    s.write_plaintext(b"hello").unwrap();
    let mut d = s.take_tls();
    let n = d.len();
    d[n - 3] ^= 0x01;
    let e = feed(&mut c, &d).unwrap_err();
    assert_eq!(e.alert(), Some(AlertDescription::BAD_RECORD_MAC));
    assert!(read_all(&mut c).is_empty());
    // 以後は同じエラーで止まり、encrypted な alert を一つ出している。
    assert!(c.write_plaintext(b"x").is_err());
    let alert = c.take_tls();
    assert_eq!(alert[0], 23);
    let e2 = feed(&mut s, &alert).unwrap_err();
    assert_eq!(e2, Error::PeerAlert(AlertDescription::BAD_RECORD_MAC));
}

#[test]
fn replayed_and_reordered_records_are_rejected() {
    let (mut c, mut s) = connect(client_config(), server_config(ECDSA_CHAIN, ECDSA_KEY), "localhost");
    s.write_plaintext(b"one").unwrap();
    let r1 = s.take_tls();
    s.write_plaintext(b"two").unwrap();
    let r2 = s.take_tls();
    feed(&mut c, &r1).unwrap();
    assert_eq!(read_all(&mut c), b"one");
    // 再送
    let e = feed(&mut c, &r1).unwrap_err();
    assert_eq!(e.alert(), Some(AlertDescription::BAD_RECORD_MAC));

    let (mut c, mut s) = connect(client_config(), server_config(ECDSA_CHAIN, ECDSA_KEY), "localhost");
    s.write_plaintext(b"one").unwrap();
    let _r1 = s.take_tls();
    s.write_plaintext(b"two").unwrap();
    let r2b = s.take_tls();
    let _ = r2;
    // 順序の入れ替え
    let e = feed(&mut c, &r2b).unwrap_err();
    assert_eq!(e.alert(), Some(AlertDescription::BAD_RECORD_MAC));
}

#[test]
fn tampered_handshake_is_rejected() {
    // server の最初の送信（SH + CCS + 暗号化された EE..Finished）の各バイトを順に壊す。
    let cc = Arc::new(client_config());
    let sc = Arc::new(server_config(ECDSA_CHAIN, ECDSA_KEY));
    let mut c0 = Connection::client(cc.clone(), "localhost", now()).unwrap();
    let ch = c0.take_tls();
    let mut s0 = Connection::server(sc.clone(), now()).unwrap();
    feed(&mut s0, &ch).unwrap();
    let flight = s0.take_tls();
    let mut rejected = 0;
    for i in (0..flight.len()).step_by(7) {
        // 同じ client の状態を作り直すため、ClientHello からやり直す。
        let mut c = Connection::client(cc.clone(), "localhost", now()).unwrap();
        let ch = c.take_tls();
        let mut s = Connection::server(sc.clone(), now()).unwrap();
        feed(&mut s, &ch).unwrap();
        let mut f = s.take_tls();
        f[i] ^= 0x40;
        match feed(&mut c, &f) {
            Err(_) => rejected += 1,
            Ok(()) => assert!(c.is_handshaking(), "壊れた送信でハンドシェイクが完了した (byte {i})"),
        }
    }
    assert!(rejected > 0);
}

#[test]
fn key_update_both_directions() {
    let (mut c, mut s) = connect(client_config(), server_config(ECDSA_CHAIN, ECDSA_KEY), "localhost");
    for i in 0..10 {
        if i % 2 == 0 {
            c.update_keys(true).unwrap();
        } else {
            s.update_keys(false).unwrap();
        }
        exchange(&mut c, &mut s, 3000);
    }
}

#[test]
fn certificate_failures() {
    let check = |cc: ClientConfig, sc: ServerConfig, name: &str, want: AlertDescription| {
        let mut c = Connection::client(Arc::new(cc), name, now()).unwrap();
        let mut s = Connection::server(Arc::new(sc), now()).unwrap();
        let (rc, rs) = pump(&mut c, &mut s);
        let e = rc.unwrap_err();
        assert!(matches!(e, Error::Certificate { .. }), "{e:?}");
        assert_eq!(e.alert(), Some(want), "{e:?}");
        assert_eq!(rs.unwrap_err(), Error::PeerAlert(want));
    };
    // ホスト名違い
    check(client_config(), server_config(ECDSA_CHAIN, ECDSA_KEY), "wrong.example", AlertDescription::BAD_CERTIFICATE);
    // 信頼しない root
    let cc = ClientConfig::new(ServerVerifier::WebPki {
        roots: roots(OTHER_CA),
        spki_pins: Vec::new(),
    });
    check(cc, server_config(ECDSA_CHAIN, ECDSA_KEY), "localhost", AlertDescription::UNKNOWN_CA);
    // 期限切れ
    check(client_config(), server_config(EXPIRED, ECDSA_KEY), "localhost", AlertDescription::CERTIFICATE_EXPIRED);
    // 中間証明書を送らない
    let leaf_only: Vec<u8> = {
        let s = std::str::from_utf8(ECDSA_CHAIN).unwrap();
        let end = s.find("-----END CERTIFICATE-----").unwrap() + 25;
        s.as_bytes()[..end].to_vec()
    };
    check(client_config(), server_config(&leaf_only, ECDSA_KEY), "localhost", AlertDescription::UNKNOWN_CA);
    // pin 違い
    let cc = ClientConfig::new(ServerVerifier::WebPki {
        roots: roots(CA),
        spki_pins: vec![[0u8; 32]],
    });
    check(cc, server_config(ECDSA_CHAIN, ECDSA_KEY), "localhost", AlertDescription::BAD_CERTIFICATE);
}

#[test]
fn names_ip_wildcard_and_pins() {
    connect(client_config(), server_config(ECDSA_CHAIN, ECDSA_KEY), "127.0.0.1");
    connect(client_config(), server_config(ECDSA_CHAIN, ECDSA_KEY), "a.wild.test");
    connect(client_config(), server_config(ECDSA_CHAIN, ECDSA_KEY), "LOCALHOST.");
    let pin = certified(RSA_CHAIN, RSA_KEY).spki_sha256().unwrap();
    let cc = ClientConfig::new(ServerVerifier::WebPki {
        roots: roots(CA),
        spki_pins: vec![[1u8; 32], pin],
    });
    connect(cc, server_config(RSA_CHAIN, RSA_KEY), "localhost");
    // pin だけ（root もホスト名も見ない）
    let cc = ClientConfig::new(ServerVerifier::PinnedSpkiOnly(vec![pin]));
    connect(cc, server_config(RSA_CHAIN, RSA_KEY), "node7.internal");
}

#[test]
fn mutual_tls() {
    let mut sc = server_config(ECDSA_CHAIN, ECDSA_KEY);
    sc.client_auth = ClientAuth::Required(roots(CA));
    let mut cc = client_config();
    cc.client_cert = Some(certified(CLIENT_CHAIN, CLIENT_KEY));
    let (mut c, mut s) = connect(cc.clone(), sc.clone(), "localhost");
    assert_eq!(s.peer_certificates().len(), 1);
    exchange(&mut c, &mut s, 100);

    // 証明書なし → certificate_required
    let mut c = Connection::client(Arc::new(client_config()), "localhost", now()).unwrap();
    let mut s = Connection::server(Arc::new(sc.clone()), now()).unwrap();
    let (rc, rs) = pump(&mut c, &mut s);
    assert_eq!(rs.unwrap_err().alert(), Some(AlertDescription::CERTIFICATE_REQUIRED));
    assert_eq!(rc.unwrap_err(), Error::PeerAlert(AlertDescription::CERTIFICATE_REQUIRED));

    // 信頼しない CA の client 証明書 → unknown_ca
    let mut sc2 = sc.clone();
    sc2.client_auth = ClientAuth::Required(roots(OTHER_CA));
    let mut c = Connection::client(Arc::new(cc.clone()), "localhost", now()).unwrap();
    let mut s = Connection::server(Arc::new(sc2), now()).unwrap();
    let (_rc, rs) = pump(&mut c, &mut s);
    assert_eq!(rs.unwrap_err().alert(), Some(AlertDescription::UNKNOWN_CA));

    // サーバー証明書を client 証明書として使う → EKU 違い
    let mut cc3 = client_config();
    cc3.client_cert = Some(certified(ECDSA_CHAIN, ECDSA_KEY));
    let mut c = Connection::client(Arc::new(cc3), "localhost", now()).unwrap();
    let mut s = Connection::server(Arc::new(sc.clone()), now()).unwrap();
    let (_rc, rs) = pump(&mut c, &mut s);
    assert!(rs.is_err());

    // Optional: 出さなくてもよい
    let mut sc4 = sc.clone();
    sc4.client_auth = ClientAuth::Optional(roots(CA));
    let (_c, s) = connect(client_config(), sc4, "localhost");
    assert!(s.peer_certificates().is_empty());
}

#[test]
fn alpn_negotiation() {
    let mut sc = server_config(ECDSA_CHAIN, ECDSA_KEY);
    sc.alpn_protocols = vec![b"h3".to_vec(), b"http/1.1".to_vec()];
    let mut cc = client_config();
    cc.alpn_protocols = vec![b"http/1.1".to_vec(), b"h3".to_vec()];
    let (c, s) = connect(cc, sc.clone(), "localhost");
    assert_eq!(c.alpn_protocol(), Some(&b"h3"[..]));
    assert_eq!(s.alpn_protocol(), Some(&b"h3"[..]));

    let mut cc = client_config();
    cc.alpn_protocols = vec![b"moq-00".to_vec()];
    let mut c = Connection::client(Arc::new(cc), "localhost", now()).unwrap();
    let mut s = Connection::server(Arc::new(sc), now()).unwrap();
    let (rc, rs) = pump(&mut c, &mut s);
    assert_eq!(rs.unwrap_err().alert(), Some(AlertDescription::NO_APPLICATION_PROTOCOL));
    assert_eq!(rc.unwrap_err(), Error::PeerAlert(AlertDescription::NO_APPLICATION_PROTOCOL));
}

#[test]
fn sni_selects_certificate() {
    let mut sc = server_config(RSA_CHAIN, RSA_KEY);
    sc.add_sni_cert("example.test", certified(ED_CHAIN, ED_KEY));
    let ed_leaf = certified(ED_CHAIN, ED_KEY).chain()[0].clone();
    let rsa_leaf = certified(RSA_CHAIN, RSA_KEY).chain()[0].clone();
    let (c, s) = connect(client_config(), sc.clone(), "example.test");
    assert_eq!(c.peer_certificates()[0], ed_leaf);
    assert_eq!(s.info().server_name.as_deref(), Some("example.test"));
    let (c, _) = connect(client_config(), sc, "localhost");
    assert_eq!(c.peer_certificates()[0], rsa_leaf);
}

#[test]
fn record_size_limit_is_honoured() {
    let mut cc = client_config();
    cc.record_size_limit = Some(256);
    let (mut c, mut s) = connect(cc, server_config(ECDSA_CHAIN, ECDSA_KEY), "localhost");
    s.write_plaintext(&[7u8; 5000]).unwrap();
    let d = s.take_tls();
    let mut off = 0;
    let mut n = 0;
    while off < d.len() {
        let len = u16::from_be_bytes([d[off + 3], d[off + 4]]) as usize;
        assert!(len <= 256 + 16, "記録が長すぎる: {len}");
        off += 5 + len;
        n += 1;
    }
    assert!(n >= 20);
    feed(&mut c, &d).unwrap();
    assert_eq!(read_all(&mut c), vec![7u8; 5000]);
}

#[test]
fn close_notify() {
    let (mut c, mut s) = connect(client_config(), server_config(ECDSA_CHAIN, ECDSA_KEY), "localhost");
    c.write_plaintext(b"bye").unwrap();
    c.send_close_notify();
    assert!(c.write_plaintext(b"more").is_err());
    let (rc, rs) = pump(&mut c, &mut s);
    rc.unwrap();
    rs.unwrap();
    assert_eq!(read_all(&mut s), b"bye");
    assert!(s.peer_closed());
}

#[test]
fn write_before_handshake_is_buffered() {
    let mut c = Connection::client(Arc::new(client_config()), "localhost", now()).unwrap();
    let mut s = Connection::server(Arc::new(server_config(ECDSA_CHAIN, ECDSA_KEY)), now()).unwrap();
    c.write_plaintext(b"GET / HTTP/1.1\r\n\r\n").unwrap();
    let (rc, rs) = pump(&mut c, &mut s);
    rc.unwrap();
    rs.unwrap();
    assert_eq!(read_all(&mut s), b"GET / HTTP/1.1\r\n\r\n");
}

#[test]
fn byte_by_byte_delivery() {
    let mut c = Connection::client(Arc::new(client_config()), "localhost", now()).unwrap();
    let mut s = Connection::server(Arc::new(server_config(RSA_CHAIN, RSA_KEY)), now()).unwrap();
    for _ in 0..10 {
        let d = c.take_tls();
        for b in d.chunks(1) {
            feed(&mut s, b).unwrap();
        }
        let d = s.take_tls();
        for b in d.chunks(3) {
            feed(&mut c, b).unwrap();
        }
    }
    assert!(!c.is_handshaking() && !s.is_handshaking());
}
