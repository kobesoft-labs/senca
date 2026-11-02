//! client と server をメモリ上でつなぐ試験（損失・並べ替え・重複・分割・改変・replay）。

use std::time::{Duration, Instant};

use senca_dtls::{
    AlertDescription, CipherSuite, CookieDecision, CookieKey, DtlsCertificate, DtlsConfig,
    DtlsEndpoint, DtlsError, DtlsEvent, NamedGroup, SrtpProtectionProfile, check_client_hello,
};

/// 決定的な擬似乱数（試験の再現性のため）。
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

fn certs() -> (DtlsCertificate, DtlsCertificate) {
    (
        DtlsCertificate::generate_self_signed("client").unwrap(),
        DtlsCertificate::generate_self_signed("server").unwrap(),
    )
}

#[derive(Default)]
struct Net {
    loss: u64,
    dup: u64,
    reorder: u64,
}

struct Outcome {
    client_events: Vec<DtlsEvent>,
    server_events: Vec<DtlsEvent>,
    client_err: Option<DtlsError>,
    server_err: Option<DtlsError>,
    max_datagram: usize,
    datagrams: usize,
}

/// 2 つの endpoint を時刻を進めながらつなぐ。`mutate` は送られる datagram を書き換える。
fn run(
    c: &mut DtlsEndpoint,
    s: &mut DtlsEndpoint,
    net: Net,
    seed: u64,
    start: Instant,
    mut mutate: impl FnMut(bool, usize, &mut Vec<u8>),
) -> Outcome {
    let mut rng = Rng(seed | 1);
    let mut now = start;
    let mut o = Outcome {
        client_events: vec![],
        server_events: vec![],
        client_err: None,
        server_err: None,
        max_datagram: 0,
        datagrams: 0,
    };
    // (宛先が server か, datagram)
    let mut wire: Vec<(bool, Vec<u8>)> = Vec::new();
    let mut counter = 0usize;
    for _ in 0..2000 {
        for (to_server, ep) in [(true, &mut *c), (false, &mut *s)] {
            while let Some(mut d) = ep.poll_transmit() {
                o.max_datagram = o.max_datagram.max(d.len());
                o.datagrams += 1;
                mutate(to_server, counter, &mut d);
                counter += 1;
                if rng.chance(net.loss) {
                    continue;
                }
                if rng.chance(net.dup) {
                    wire.push((to_server, d.clone()));
                }
                wire.push((to_server, d));
            }
        }
        if rng.chance(net.reorder) && wire.len() > 1 {
            let i = (rng.next() as usize) % wire.len();
            let j = (rng.next() as usize) % wire.len();
            wire.swap(i, j);
        }
        if !wire.is_empty() {
            let (to_server, d) = wire.remove(0);
            let (ep, err) = if to_server {
                (&mut *s, &mut o.server_err)
            } else {
                (&mut *c, &mut o.client_err)
            };
            if let Err(e) = ep.handle_datagram(&d, now)
                && err.is_none()
            {
                *err = Some(e);
            }
        } else {
            // 何も飛んでいなければ時刻を次の timer まで進める
            let next = [c.poll_timeout(), s.poll_timeout()]
                .into_iter()
                .flatten()
                .min();
            match next {
                Some(t) => {
                    now = now.max(t);
                    for (ep, err) in [(&mut *c, &mut o.client_err), (&mut *s, &mut o.server_err)] {
                        if let Err(e) = ep.handle_timeout(now)
                            && err.is_none()
                        {
                            *err = Some(e);
                        }
                    }
                }
                None => break,
            }
        }
        while let Some(e) = c.poll_event() {
            o.client_events.push(e);
        }
        while let Some(e) = s.poll_event() {
            o.server_events.push(e);
        }
        if c.is_connected() && s.is_connected() && wire.is_empty() {
            // 残りを流し切る
            let mut quiet = true;
            for ep in [&mut *c, &mut *s] {
                if ep.poll_timeout().is_some() {
                    quiet = false;
                }
            }
            if quiet {
                break;
            }
        }
        if c.is_closed() && s.is_closed() {
            break;
        }
    }
    o
}

fn pair(cfg_c: DtlsConfig, cfg_s: DtlsConfig, now: Instant) -> (DtlsEndpoint, DtlsEndpoint) {
    (
        DtlsEndpoint::new(cfg_c, now).unwrap(),
        DtlsEndpoint::new(cfg_s, now).unwrap(),
    )
}

fn assert_same_keys(c: &DtlsEndpoint, s: &DtlsEndpoint) {
    let (pc, kc) = c.export_srtp_keying_material().unwrap();
    let (ps, ks) = s.export_srtp_keying_material().unwrap();
    assert_eq!(pc, ps);
    assert_eq!(kc.len(), pc.keying_material_len());
    assert_eq!(*kc, *ks);
    let a = c
        .export_keying_material(b"EXPERIMENTAL-test", Some(b"ctx"), 32)
        .unwrap();
    let b = s
        .export_keying_material(b"EXPERIMENTAL-test", Some(b"ctx"), 32)
        .unwrap();
    assert_eq!(*a, *b);
    let n = s
        .export_keying_material(b"EXPERIMENTAL-test", None, 32)
        .unwrap();
    assert_ne!(*a, *n);
}

#[test]
fn handshake_all_suites_groups_profiles() {
    let now = Instant::now();
    for suite in [
        CipherSuite::EcdheEcdsaAes128GcmSha256,
        CipherSuite::EcdheEcdsaAes256GcmSha384,
        CipherSuite::EcdheEcdsaChacha20Poly1305Sha256,
    ] {
        for group in [NamedGroup::X25519, NamedGroup::Secp256r1] {
            for profile in [
                SrtpProtectionProfile::AeadAes128Gcm,
                SrtpProtectionProfile::AeadAes256Gcm,
                SrtpProtectionProfile::Aes128CmHmacSha1_80,
                SrtpProtectionProfile::Aes128CmHmacSha1_32,
            ] {
                let (cc, sc) = certs();
                let (cfp, sfp) = (cc.fingerprint_sha256(), sc.fingerprint_sha256());
                let mut c_cfg = DtlsConfig::client(cc);
                c_cfg.cipher_suites = vec![suite];
                // client は secp256r1 を必ず示す。x25519 を選ばせるときは先頭に置く。
                c_cfg.groups = if group == NamedGroup::X25519 {
                    vec![group, NamedGroup::Secp256r1]
                } else {
                    vec![group]
                };
                c_cfg.srtp_profiles = vec![profile];
                c_cfg.expected_peer_fingerprint = Some(sfp);
                let mut s_cfg = DtlsConfig::server(sc);
                s_cfg.srtp_profiles = vec![
                    SrtpProtectionProfile::AeadAes128Gcm,
                    SrtpProtectionProfile::AeadAes256Gcm,
                    SrtpProtectionProfile::Aes128CmHmacSha1_80,
                    SrtpProtectionProfile::Aes128CmHmacSha1_32,
                ];
                let (mut c, mut s) = pair(c_cfg, s_cfg, now);
                let o = run(&mut c, &mut s, Net::default(), 1, now, |_, _, _| {});
                assert!(
                    o.client_err.is_none() && o.server_err.is_none(),
                    "{:?} {:?}",
                    o.client_err,
                    o.server_err
                );
                assert_eq!(o.client_events, vec![DtlsEvent::Connected]);
                assert_eq!(o.server_events, vec![DtlsEvent::Connected]);
                assert_eq!(c.cipher_suite(), Some(suite));
                assert_eq!(s.cipher_suite(), Some(suite));
                assert_eq!(c.named_group(), Some(group));
                assert_eq!(c.srtp_profile(), Some(profile));
                assert!(c.verify_peer_fingerprint(&sfp));
                assert!(s.verify_peer_fingerprint(&cfp));
                assert!(!s.verify_peer_fingerprint(&sfp));
                assert_same_keys(&c, &s);
                // application data を両方向に
                c.send(b"ping").unwrap();
                s.handle_datagram(&c.poll_transmit().unwrap(), now).unwrap();
                assert_eq!(
                    s.poll_event(),
                    Some(DtlsEvent::ApplicationData(b"ping"[..].into()))
                );
                s.send(b"pong").unwrap();
                c.handle_datagram(&s.poll_transmit().unwrap(), now).unwrap();
                assert_eq!(
                    c.poll_event(),
                    Some(DtlsEvent::ApplicationData(b"pong"[..].into()))
                );
            }
        }
    }
}

#[test]
fn client_must_offer_p256() {
    let (cc, _) = certs();
    let mut cfg = DtlsConfig::client(cc);
    cfg.groups = vec![NamedGroup::X25519];
    assert!(matches!(
        DtlsEndpoint::new(cfg, Instant::now()),
        Err(DtlsError::InvalidConfig(_))
    ));
}

#[test]
fn server_preference_and_srtp_choice() {
    let now = Instant::now();
    let (cc, sc) = certs();
    let mut c_cfg = DtlsConfig::client(cc);
    c_cfg.srtp_profiles = vec![
        SrtpProtectionProfile::Aes128CmHmacSha1_80,
        SrtpProtectionProfile::AeadAes128Gcm,
    ];
    c_cfg.cipher_suites = vec![
        CipherSuite::EcdheEcdsaChacha20Poly1305Sha256,
        CipherSuite::EcdheEcdsaAes128GcmSha256,
    ];
    let s_cfg = DtlsConfig::server(sc);
    let (mut c, mut s) = pair(c_cfg, s_cfg, now);
    let o = run(&mut c, &mut s, Net::default(), 1, now, |_, _, _| {});
    assert!(o.client_err.is_none() && o.server_err.is_none());
    // server の優先順（AEAD_AES_128_GCM、AES_128_GCM_SHA256）が勝つ
    assert_eq!(s.srtp_profile(), Some(SrtpProtectionProfile::AeadAes128Gcm));
    assert_eq!(
        c.cipher_suite(),
        Some(CipherSuite::EcdheEcdsaAes128GcmSha256)
    );
    assert_eq!(c.named_group(), Some(NamedGroup::X25519));
}

#[test]
fn no_common_srtp_profile_means_no_srtp() {
    let now = Instant::now();
    let (cc, sc) = certs();
    let mut c_cfg = DtlsConfig::client(cc);
    c_cfg.srtp_profiles = vec![SrtpProtectionProfile::AeadAes256Gcm];
    let (mut c, mut s) = pair(c_cfg, DtlsConfig::server(sc), now);
    let o = run(&mut c, &mut s, Net::default(), 1, now, |_, _, _| {});
    assert!(o.client_err.is_none() && o.server_err.is_none());
    assert_eq!(c.srtp_profile(), None);
    assert_eq!(
        c.export_srtp_keying_material().err(),
        Some(DtlsError::InvalidExporter)
    );
}

#[test]
fn cookie_exchange() {
    let now = Instant::now();
    let (cc, sc) = certs();
    let key = CookieKey::random().unwrap();
    let mut s_cfg = DtlsConfig::server(sc);
    s_cfg.cookie_key = Some(key.clone());
    s_cfg.peer_transport_id = b"192.0.2.1:5000".to_vec();
    let mut c = DtlsEndpoint::new(DtlsConfig::client(cc), now).unwrap();
    // 状態を持たない入口で検査する
    let ch1 = c.poll_transmit().unwrap();
    let hvr = match check_client_hello(&key, b"192.0.2.1:5000", &ch1) {
        CookieDecision::HelloVerifyRequest(d) => d,
        other => panic!("{other:?}"),
    };
    assert!(hvr.len() < ch1.len(), "増幅しない");
    c.handle_datagram(&hvr, now).unwrap();
    let ch2 = c.poll_transmit().unwrap();
    assert_eq!(
        check_client_hello(&key, b"192.0.2.1:5000", &ch2),
        CookieDecision::Accept
    );
    // 別の相手からの同じ cookie は通らない
    assert!(matches!(
        check_client_hello(&key, b"192.0.2.9:5000", &ch2),
        CookieDecision::HelloVerifyRequest(_)
    ));
    assert_eq!(
        check_client_hello(&key, b"x", b"garbage"),
        CookieDecision::Drop
    );
    let mut s = DtlsEndpoint::new(s_cfg, now).unwrap();
    s.handle_datagram(&ch2, now).unwrap();
    let o = run(&mut c, &mut s, Net::default(), 3, now, |_, _, _| {});
    assert!(
        o.client_err.is_none() && o.server_err.is_none(),
        "{:?} {:?}",
        o.client_err,
        o.server_err
    );
    assert!(c.is_connected() && s.is_connected());
    assert_same_keys(&c, &s);
}

#[test]
fn cookie_exchange_inside_endpoint_with_loss() {
    let now = Instant::now();
    for seed in 1..200u64 {
        let (cc, sc) = certs();
        let mut s_cfg = DtlsConfig::server(sc);
        s_cfg.cookie_key = Some(CookieKey::random().unwrap());
        s_cfg.peer_transport_id = b"peer".to_vec();
        s_cfg.max_retransmissions = 12;
        s_cfg.handshake_timeout = Duration::from_secs(600);
        let mut c_cfg = DtlsConfig::client(cc);
        c_cfg.max_retransmissions = 12;
        c_cfg.handshake_timeout = Duration::from_secs(600);
        let (mut c, mut s) = pair(c_cfg, s_cfg, now);
        let net = Net {
            loss: 25,
            dup: 10,
            reorder: 30,
        };
        let o = run(&mut c, &mut s, net, seed, now, |_, _, _| {});
        assert!(
            c.is_connected() && s.is_connected(),
            "seed {seed}: {:?} {:?}",
            o.client_err,
            o.server_err
        );
        assert_same_keys(&c, &s);
    }
}

#[test]
fn cookie_key_rotation() {
    let now = Instant::now();
    let (cc, _) = certs();
    let mut key = CookieKey::random().unwrap();
    let mut c = DtlsEndpoint::new(DtlsConfig::client(cc), now).unwrap();
    let ch1 = c.poll_transmit().unwrap();
    let CookieDecision::HelloVerifyRequest(hvr) = check_client_hello(&key, b"p", &ch1) else {
        panic!()
    };
    c.handle_datagram(&hvr, now).unwrap();
    let ch2 = c.poll_transmit().unwrap();
    key.rotate().unwrap();
    assert_eq!(check_client_hello(&key, b"p", &ch2), CookieDecision::Accept);
    key.rotate().unwrap();
    assert!(matches!(
        check_client_hello(&key, b"p", &ch2),
        CookieDecision::HelloVerifyRequest(_)
    ));
}

#[test]
fn loss_reorder_duplication() {
    let now = Instant::now();
    for seed in 1..80u64 {
        let (cc, sc) = certs();
        let mut c_cfg = DtlsConfig::client(cc);
        c_cfg.max_retransmissions = 12;
        c_cfg.handshake_timeout = Duration::from_secs(600);
        let mut s_cfg = DtlsConfig::server(sc);
        s_cfg.max_retransmissions = 12;
        s_cfg.handshake_timeout = Duration::from_secs(600);
        let (mut c, mut s) = pair(c_cfg, s_cfg, now);
        let net = Net {
            loss: 30,
            dup: 20,
            reorder: 40,
        };
        let o = run(&mut c, &mut s, net, seed, now, |_, _, _| {});
        assert!(
            c.is_connected() && s.is_connected(),
            "seed {seed}: {:?} {:?}",
            o.client_err,
            o.server_err
        );
        assert_same_keys(&c, &s);
        assert_eq!(
            o.client_events
                .iter()
                .filter(|e| **e == DtlsEvent::Connected)
                .count(),
            1
        );
        assert_eq!(
            o.server_events
                .iter()
                .filter(|e| **e == DtlsEvent::Connected)
                .count(),
            1
        );
    }
}

#[test]
fn final_flight_lost_is_recovered() {
    // server の最後の flight（CCS + Finished）だけを 2 回落とす。client の再送で server が送り直す。
    let now = Instant::now();
    let (cc, sc) = certs();
    let (mut c, mut s) = pair(DtlsConfig::client(cc), DtlsConfig::server(sc), now);
    let mut dropped = 0;
    let mut server_sent = 0;
    let o = run(&mut c, &mut s, Net::default(), 5, now, |to_server, _, d| {
        if !to_server {
            server_sent += 1;
            // server の 2 つ目以降の flight（F6）は 0x14（CCS）で始まる
            if d[0] == 20 && dropped < 2 {
                dropped += 1;
                d.clear();
            }
        }
    });
    assert!(dropped == 2 && server_sent >= 3);
    assert!(
        c.is_connected() && s.is_connected(),
        "{:?} {:?}",
        o.client_err,
        o.server_err
    );
    assert_same_keys(&c, &s);
}

#[test]
fn small_mtu_fragmentation() {
    let now = Instant::now();
    for mtu in [256usize, 300, 577] {
        for seed in 1..10u64 {
            let (cc, sc) = certs();
            let mut c_cfg = DtlsConfig::client(cc);
            c_cfg.mtu = mtu;
            let mut s_cfg = DtlsConfig::server(sc);
            s_cfg.mtu = mtu;
            let (mut c, mut s) = pair(c_cfg, s_cfg, now);
            let net = Net {
                loss: 15,
                dup: 10,
                reorder: 40,
            };
            let o = run(&mut c, &mut s, net, seed, now, |_, _, _| {});
            assert!(
                c.is_connected() && s.is_connected(),
                "mtu {mtu} seed {seed}: {:?} {:?}",
                o.client_err,
                o.server_err
            );
            assert!(o.max_datagram <= mtu, "{} > {mtu}", o.max_datagram);
            assert_same_keys(&c, &s);
        }
    }
}

#[test]
fn tampering_never_yields_mismatched_session() {
    // 1 つの datagram の 1 bit を反転する。handshake は失敗するか、改変を捨てて再送で完了する。
    // 完了した場合、両者の鍵は必ず一致する。
    let now = Instant::now();
    let mut failures = 0;
    let mut completions = 0;
    for target in 0..8usize {
        for pos in (0..1200usize).step_by(37) {
            let (cc, sc) = certs();
            let (mut c, mut s) = pair(DtlsConfig::client(cc), DtlsConfig::server(sc), now);
            let o = run(&mut c, &mut s, Net::default(), 9, now, |_, idx, d| {
                if idx == target && !d.is_empty() {
                    let p = pos % d.len();
                    d[p] ^= 0x10;
                }
            });
            if c.is_connected() && s.is_connected() {
                completions += 1;
                assert_same_keys(&c, &s);
            } else {
                failures += 1;
                assert!(
                    o.client_err.is_some()
                        || o.server_err.is_some()
                        || !c.is_connected()
                        || !s.is_connected()
                );
            }
        }
    }
    assert!(failures > 0 && completions > 0, "{failures} {completions}");
}

#[test]
fn app_data_replay_and_tamper_rejected() {
    let now = Instant::now();
    let (cc, sc) = certs();
    let (mut c, mut s) = pair(DtlsConfig::client(cc), DtlsConfig::server(sc), now);
    run(&mut c, &mut s, Net::default(), 1, now, |_, _, _| {});
    while s.poll_event().is_some() {}
    c.send(b"once").unwrap();
    let d = c.poll_transmit().unwrap();
    s.handle_datagram(&d, now).unwrap();
    assert_eq!(
        s.poll_event(),
        Some(DtlsEvent::ApplicationData(b"once"[..].into()))
    );
    s.handle_datagram(&d, now).unwrap();
    assert_eq!(s.poll_event(), None, "replay は受けない");
    c.send(b"twice").unwrap();
    let d = c.poll_transmit().unwrap();
    for i in 0..d.len() {
        let mut bad = d.clone();
        bad[i] ^= 0x01;
        s.handle_datagram(&bad, now).unwrap();
        assert_eq!(s.poll_event(), None, "byte {i}");
    }
    // epoch 0 の application data（平文）は受けない
    let mut plain = d[..13].to_vec();
    plain[3] = 0;
    plain[4] = 0;
    plain[11] = 0;
    plain[12] = 5;
    plain.extend_from_slice(b"hello");
    s.handle_datagram(&plain, now).unwrap();
    assert_eq!(s.poll_event(), None);
    // 平文の fatal alert で接続を切れない
    let alert = [21u8, 0xfe, 0xfd, 0, 0, 0, 0, 0, 0, 0, 99, 0, 2, 2, 40];
    s.handle_datagram(&alert, now).unwrap();
    assert!(s.is_connected());
    s.handle_datagram(&d, now).unwrap();
    assert_eq!(
        s.poll_event(),
        Some(DtlsEvent::ApplicationData(b"twice"[..].into()))
    );
    // 並べ替え（window 内）は受ける
    let ds: Vec<_> = (0..10u8)
        .map(|i| {
            c.send(&[i]).unwrap();
            c.poll_transmit().unwrap()
        })
        .collect();
    for d in ds.iter().rev() {
        s.handle_datagram(d, now).unwrap();
    }
    let got: Vec<_> = std::iter::from_fn(|| s.poll_event()).collect();
    assert_eq!(got.len(), 10);
}

#[test]
fn fingerprint_mismatch_aborts() {
    let now = Instant::now();
    let (cc, sc) = certs();
    let mut c_cfg = DtlsConfig::client(cc);
    c_cfg.expected_peer_fingerprint = Some([0xAA; 32]);
    let (mut c, mut s) = pair(c_cfg, DtlsConfig::server(sc), now);
    let o = run(&mut c, &mut s, Net::default(), 1, now, |_, _, _| {});
    assert_eq!(
        o.client_err,
        Some(DtlsError::Fatal {
            alert: AlertDescription::BadCertificate,
            reason: "fingerprint が一致しない"
        })
    );
    assert_eq!(
        o.server_err,
        Some(DtlsError::PeerAlert(AlertDescription::BadCertificate))
    );
    assert!(!c.is_connected() && !s.is_connected());
}

#[test]
fn server_fingerprint_check_on_client_cert() {
    let now = Instant::now();
    let (cc, sc) = certs();
    let mut s_cfg = DtlsConfig::server(sc);
    s_cfg.expected_peer_fingerprint = Some(cc.fingerprint_sha256());
    let (mut c, mut s) = pair(DtlsConfig::client(cc), s_cfg, now);
    run(&mut c, &mut s, Net::default(), 1, now, |_, _, _| {});
    assert!(c.is_connected() && s.is_connected());
}

#[test]
fn no_common_suite_fails() {
    let now = Instant::now();
    let (cc, sc) = certs();
    let mut c_cfg = DtlsConfig::client(cc);
    c_cfg.cipher_suites = vec![CipherSuite::EcdheEcdsaChacha20Poly1305Sha256];
    let mut s_cfg = DtlsConfig::server(sc);
    s_cfg.cipher_suites = vec![CipherSuite::EcdheEcdsaAes128GcmSha256];
    let (mut c, mut s) = pair(c_cfg, s_cfg, now);
    let o = run(&mut c, &mut s, Net::default(), 1, now, |_, _, _| {});
    assert!(matches!(
        o.server_err,
        Some(DtlsError::Fatal {
            alert: AlertDescription::HandshakeFailure,
            ..
        })
    ));
    assert_eq!(
        o.client_err,
        Some(DtlsError::PeerAlert(AlertDescription::HandshakeFailure))
    );
}

#[test]
fn retransmission_backoff_and_timeout() {
    let start = Instant::now();
    let (cc, _) = certs();
    let mut cfg = DtlsConfig::client(cc);
    cfg.max_retransmissions = 4;
    let mut c = DtlsEndpoint::new(cfg, start).unwrap();
    let first = c.poll_transmit().unwrap();
    let mut times = Vec::new();
    loop {
        let t = c.poll_timeout().unwrap();
        match c.handle_timeout(t) {
            Ok(()) => {
                let d = c.poll_transmit().unwrap();
                // 同じ ClientHello（record sequence number だけが違う）
                assert_eq!(d[13..], first[13..]);
                assert_ne!(d[5..11], first[5..11]);
                times.push(t.duration_since(start).as_millis());
            }
            Err(e) => {
                assert_eq!(e, DtlsError::Timeout);
                break;
            }
        }
    }
    assert_eq!(times, vec![1000, 3000, 7000, 15000]);
    assert!(c.is_closed());
    assert_eq!(c.handle_datagram(&first, start), Err(DtlsError::Closed));
}

#[test]
fn close_notify() {
    let now = Instant::now();
    let (cc, sc) = certs();
    let (mut c, mut s) = pair(DtlsConfig::client(cc), DtlsConfig::server(sc), now);
    run(&mut c, &mut s, Net::default(), 1, now, |_, _, _| {});
    while s.poll_event().is_some() {}
    c.close();
    assert!(c.is_closed());
    assert_eq!(c.send(b"x"), Err(DtlsError::Closed));
    s.handle_datagram(&c.poll_transmit().unwrap(), now).unwrap();
    assert_eq!(s.poll_event(), Some(DtlsEvent::Closed));
    assert!(s.is_closed());
    assert!(s.poll_transmit().is_some(), "close_notify を返す");
    assert!(
        c.export_keying_material(b"EXTRACTOR-dtls_srtp", None, 60)
            .is_err()
    );
}

#[test]
fn exporter_rejects_reserved_labels() {
    let now = Instant::now();
    let (cc, sc) = certs();
    let (mut c, mut s) = pair(DtlsConfig::client(cc), DtlsConfig::server(sc), now);
    assert_eq!(
        c.export_keying_material(b"x", None, 10).err(),
        Some(DtlsError::NotConnected)
    );
    run(&mut c, &mut s, Net::default(), 1, now, |_, _, _| {});
    for l in [
        &b"key expansion"[..],
        b"master secret",
        b"client finished",
        b"extended master secret",
    ] {
        assert_eq!(
            c.export_keying_material(l, None, 10).err(),
            Some(DtlsError::InvalidExporter)
        );
    }
    assert_eq!(
        c.export_keying_material(b"x", None, 0).err(),
        Some(DtlsError::InvalidExporter)
    );
}

#[test]
fn garbage_input_never_panics() {
    let now = Instant::now();
    let (cc, sc) = certs();
    let (mut c, mut s) = pair(DtlsConfig::client(cc), DtlsConfig::server(sc), now);
    let mut rng = Rng(12345);
    let ch = c.poll_transmit().unwrap();
    for i in 0..3000 {
        let mut d = ch.clone();
        let n = 1 + rng.next() as usize % 4;
        for _ in 0..n {
            let p = rng.next() as usize % d.len();
            d[p] = rng.next() as u8;
        }
        if i % 7 == 0 {
            d.truncate(rng.next() as usize % d.len());
        }
        let mut fresh = DtlsEndpoint::new(
            DtlsConfig::server(DtlsCertificate::generate_self_signed("s").unwrap()),
            now,
        )
        .unwrap();
        let _ = fresh.handle_datagram(&d, now);
        let _ = s.handle_datagram(&d, now);
        let _ = c.handle_datagram(&d, now);
    }
}
