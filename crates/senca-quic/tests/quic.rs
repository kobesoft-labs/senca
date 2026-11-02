//! QUIC の結合試験（メモリ上の client・server）。

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use senca_quic::*;

const ALPN: &[u8] = b"test";

fn payload(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

/// client が bidi stream で送り、server が読んで同じものを返す。
fn echo(net: &mut Net, data: &[u8], limit: Duration) {
    let id = net.c().open_stream(Dir::Bi).unwrap();
    let mut off = 0;
    let mut finished = false;
    let mut server_got = Vec::new();
    let mut server_sent = 0usize;
    let mut server_fin = false;
    let mut client_got = Vec::new();
    let ok = net.run_until(limit, |n| {
        if off < data.len() {
            off += n.c().stream_send(id, &data[off..]).unwrap();
        }
        if off == data.len() && !finished {
            n.c().stream_finish(id).unwrap();
            finished = true;
        }
        if n.sh.is_some() && n.server.connection(n.sh.unwrap()).is_some() {
            if !server_fin {
                server_fin = read_all(n.s(), id, &mut server_got);
            }
            if server_sent < server_got.len() {
                let chunk = server_got[server_sent..].to_vec();
                server_sent += n.s().stream_send(id, &chunk).unwrap();
            }
            if server_fin && server_sent == server_got.len() && server_sent == data.len() {
                let _ = n.s().stream_finish(id);
            }
        }
        read_all(n.c(), id, &mut client_got)
    });
    assert!(ok, "echo did not finish: sent {off}, server got {}, client got {}", server_got.len(), client_got.len());
    assert_eq!(client_got, data);
}

#[test]
fn handshake_and_echo_all_suites() {
    for suite in senca_tls::CipherSuite::ALL {
        let mut tls = (*tls_client(&[ALPN])).clone();
        tls.cipher_suites = vec![suite];
        let s = ServerConfig::new(tls_server(ECDSA_CHAIN, ECDSA_KEY, &[ALPN]));
        let mut net = Net::new(s, ClientConfig::new(Arc::new(tls)));
        net.handshake();
        assert_eq!(net.c().alpn(), Some(ALPN));
        assert_eq!(net.c().tls().cipher_suite(), Some(suite));
        echo(&mut net, &payload(50_000), Duration::from_secs(10));
        let ev = net.drain_events(true);
        assert!(ev.contains(&Event::HandshakeCompleted));
    }
}

#[test]
fn bulk_transfer_with_loss_and_reordering() {
    let mut net = Net::default_pair(ALPN);
    net.loss = 0.05;
    net.reorder = 0.2;
    net.handshake();
    let data = payload(2_000_000);
    echo(&mut net, &data, Duration::from_secs(120));
    let st = net.c().stats();
    assert!(st.lost_packets > 0, "loss was not exercised: {st:?}");
}

#[test]
fn handshake_survives_heavy_loss() {
    for seed in 1..6u64 {
        let mut net = Net::default_pair(ALPN);
        net.rng = Rng(seed * 7919);
        net.loss = 0.3;
        net.handshake();
        echo(&mut net, b"hello under loss", Duration::from_secs(60));
    }
}

#[test]
fn many_uni_streams_respect_stream_limit() {
    let tc = TransportConfig {
        max_concurrent_uni_streams: 4,
        ..Default::default()
    };
    let mut s = ServerConfig::new(tls_server(ECDSA_CHAIN, ECDSA_KEY, &[ALPN]));
    s.transport = Arc::new(tc);
    let mut net = Net::new(s, ClientConfig::new(tls_client(&[ALPN])));
    net.handshake();
    let total = 40;
    let mut opened = 0;
    let mut received = 0;
    let ok = net.run_until(Duration::from_secs(20), |n| {
        while opened < total {
            let Some(id) = n.c().open_stream(Dir::Uni) else { break };
            n.c().stream_send(id, format!("msg{opened}").as_bytes()).unwrap();
            n.c().stream_finish(id).unwrap();
            opened += 1;
        }
        if n.sh.is_some() {
            while let Some(e) = n.s().poll_event() {
                if let Event::StreamOpened { id } | Event::StreamReadable { id } = e {
                    let mut v = Vec::new();
                    if read_all(n.s(), id, &mut v) {
                        received += 1;
                    }
                }
            }
        }
        received == total
    });
    assert!(ok, "opened {opened} received {received}");
}

#[test]
fn datagrams_roundtrip() {
    let mut net = Net::default_pair(ALPN);
    net.handshake();
    let max = net.c().max_datagram_size().unwrap();
    assert!(max > 1000 && max < 1200, "{max}");
    for i in 0..10u8 {
        net.c().send_datagram(bytes::Bytes::from(vec![i; 100])).unwrap();
    }
    assert_eq!(
        net.c().send_datagram(bytes::Bytes::from(vec![0; max + 1])),
        Err(DatagramError::TooLarge)
    );
    let mut got = Vec::new();
    net.run_until(Duration::from_secs(2), |n| {
        while let Some(d) = n.s().recv_datagram() {
            got.push(d);
        }
        got.len() == 10
    });
    assert_eq!(got.len(), 10);
    assert_eq!(got[3], vec![3u8; 100]);
}

#[test]
fn key_update_both_directions() {
    let mut net = Net::default_pair(ALPN);
    net.handshake();
    echo(&mut net, b"before", Duration::from_secs(5));
    assert_eq!(net.c().key_phase(), Some(false));
    assert!({ let now = net.now; net.c().initiate_key_update(now) });
    // ACK が返るまで二度目は始められない
    assert!(!{ let now = net.now; net.c().initiate_key_update(now) });
    echo(&mut net, &payload(100_000), Duration::from_secs(10));
    assert_eq!(net.c().key_phase(), Some(true));
    assert_eq!(net.s().key_phase(), Some(true));
    // server からも
    assert!({ let now = net.now; net.s().initiate_key_update(now) });
    echo(&mut net, &payload(100_000), Duration::from_secs(10));
    assert_eq!(net.c().key_phase(), Some(false));
    assert!(net.c().stats().key_updates >= 2);
    assert!(net.s().stats().key_updates >= 2);
}

#[test]
fn automatic_key_update_by_packet_count() {
    let tc = TransportConfig {
        key_update_interval: 50,
        ..Default::default()
    };
    let mut cc = ClientConfig::new(tls_client(&[ALPN]));
    cc.transport = Arc::new(tc);
    let s = ServerConfig::new(tls_server(ECDSA_CHAIN, ECDSA_KEY, &[ALPN]));
    let mut net = Net::new(s, cc);
    net.loss = 0.02;
    net.handshake();
    echo(&mut net, &payload(400_000), Duration::from_secs(30));
    assert!(net.c().stats().key_updates >= 3, "{:?}", net.c().stats());
}

#[test]
fn client_migration_with_new_cid() {
    let mut net = Net::default_pair(ALPN);
    net.handshake();
    echo(&mut net, b"before migration", Duration::from_secs(5));
    // 予備の connection ID が届くまで待つ
    net.run_until(Duration::from_secs(1), |_| false);
    assert!({ let now = net.now; net.c().migrate(now) });
    net.client_addr = addr("10.0.0.99:40000");
    echo(&mut net, &payload(200_000), Duration::from_secs(20));
    let ev = net.drain_events(false);
    assert!(
        ev.iter().any(|e| matches!(e, Event::PathMigrated { remote } if *remote == addr("10.0.0.99:40000"))),
        "{ev:?}"
    );
    assert_eq!(net.s().remote_address(), addr("10.0.0.99:40000"));
}

#[test]
fn nat_rebinding_without_client_action() {
    let mut net = Net::default_pair(ALPN);
    net.handshake();
    echo(&mut net, b"x", Duration::from_secs(5));
    net.client_addr = addr("10.0.0.2:50001");
    echo(&mut net, &payload(100_000), Duration::from_secs(20));
    assert_eq!(net.s().remote_address(), addr("10.0.0.2:50001"));
    assert!(net.s().stats().migrations >= 1);
}

#[test]
fn retry_then_handshake() {
    let mut s = ServerConfig::new(tls_server(ECDSA_CHAIN, ECDSA_KEY, &[ALPN]));
    s.retry = true;
    let mut net = Net::new(s, ClientConfig::new(tls_client(&[ALPN])));
    net.handshake();
    echo(&mut net, b"after retry", Duration::from_secs(5));
}

#[test]
fn forged_retry_token_is_rejected() {
    let mut s = ServerConfig::new(tls_server(ECDSA_CHAIN, ECDSA_KEY, &[ALPN]));
    s.retry = true;
    let mut net = Net::new(s, ClientConfig::new(tls_client(&[ALPN])));
    // server の Retry のトークンを書き換える（tag の直前のバイト）。client は tag で検出して無視するので、
    // ここでは tag も壊れる → client は Retry を捨て、接続は確立しない。
    net.s2c_filter = Some(Box::new(|d: &mut Vec<u8>| {
        if d[0] & 0xf0 == 0xf0 {
            let n = d.len();
            d[n - 17] ^= 1;
        }
        true
    }));
    let ok = net.run_until(Duration::from_secs(3), |n| n.c().is_established());
    assert!(!ok);
}

#[test]
fn version_negotiation_for_unknown_version() {
    let mut net = Net::default_pair(ALPN);
    // client の最初の datagram の版を書き換える
    net.c2s_filter = Some(Box::new(|d: &mut Vec<u8>| {
        if d[0] & 0x80 != 0 {
            d[1..5].copy_from_slice(&0x1a2a_3a4au32.to_be_bytes());
        }
        true
    }));
    // server は VN を返し、接続を作らない。client は v1 を含まない VN として閉じる…はずだが、
    // server の VN は v1 を含むので client は無視する（ダウングレード防止）。
    let saw_vn = std::rc::Rc::new(std::cell::Cell::new(false));
    let seen = saw_vn.clone();
    net.s2c_filter = Some(Box::new(move |d: &mut Vec<u8>| {
        if d.len() > 5 && d[0] & 0x80 != 0 && d[1..5] == [0, 0, 0, 0] {
            seen.set(true);
            assert!(d.windows(4).any(|w| w == [0, 0, 0, 1]));
        }
        true
    }));
    net.run_until(Duration::from_secs(1), |_| false);
    assert!(saw_vn.get(), "server did not send Version Negotiation");
    assert!(net.sh.is_none());
    assert!(net.server.is_empty());
    assert!(!net.c().is_established());
}

#[test]
fn client_rejects_version_negotiation_without_v1() {
    let mut net = Net::default_pair(ALPN);
    // server の応答を、v1 を含まない VN に差し替える
    net.c2s_filter = Some(Box::new(|_d: &mut Vec<u8>| true));
    net.step();
    // client の最初のパケットから CID を取り出して偽の VN を作る
    let mut first = None;
    net.c2s_filter = Some(Box::new(|_| true));
    let t = {
        // 送信を一つ作る（再送 timer を進める）
        let now = net.now + Duration::from_secs(2);
        net.client.handle_timeout(now);
        net.client.poll_transmit(now)
    };
    if let Some(t) = t {
        first = Some(t.contents);
    }
    let d = first.expect("client initial");
    let dl = d[5] as usize;
    let dcid = &d[6..6 + dl];
    let sl = d[6 + dl] as usize;
    let scid = &d[7 + dl..7 + dl + sl];
    let mut vn = vec![0x80, 0, 0, 0, 0, sl as u8];
    vn.extend_from_slice(scid);
    vn.push(dl as u8);
    vn.extend_from_slice(dcid);
    vn.extend_from_slice(&0xff00_001du32.to_be_bytes());
    net.client
        .handle_datagram(net.now, std::time::SystemTime::now(), net.server_addr, bytes::BytesMut::from(&vn[..]));
    let ev = net.drain_events(true);
    assert!(ev.contains(&Event::ConnectionLost(ConnectionError::VersionMismatch)), "{ev:?}");
}

#[test]
fn anti_amplification_limit_before_validation() {
    // 大きい証明書（RSA）で、client からの 2 個目以降を届けない
    let s = ServerConfig::new(tls_server(RSA_CHAIN, RSA_KEY, &[ALPN]));
    let mut net = Net::new(s, ClientConfig::new(tls_client(&[ALPN])));
    let mut sent = 0;
    net.c2s_filter = Some(Box::new(move |_d: &mut Vec<u8>| {
        sent += 1;
        sent == 1
    }));
    net.run_until(Duration::from_secs(5), |_| false);
    assert!(net.c2s_bytes >= 1200);
    assert!(net.s2c_bytes > 0);
    assert!(net.s2c_bytes <= 3 * 1200, "server sent {} bytes for 1200 received", net.s2c_bytes);
}

#[test]
fn idle_timeout_closes_both() {
    let tc = TransportConfig {
        max_idle_timeout: Duration::from_secs(3),
        ..Default::default()
    };
    let mut s = ServerConfig::new(tls_server(ECDSA_CHAIN, ECDSA_KEY, &[ALPN]));
    s.transport = Arc::new(tc);
    let mut net = Net::new(s, ClientConfig::new(tls_client(&[ALPN])));
    net.handshake();
    net.blackhole = true;
    net.run_until(Duration::from_secs(20), |n| n.client.is_empty() && n.server.is_empty());
    assert!(net.client.is_empty() && net.server.is_empty());
}

#[test]
fn application_close_is_reported() {
    let mut net = Net::default_pair(ALPN);
    net.handshake();
    let now = net.now;
    net.c().close(now, 0x42, b"bye");
    let mut got = None;
    net.run_until(Duration::from_secs(2), |n| {
        if let Some(h) = n.sh
            && let Some(c) = n.server.connection_mut(h)
        {
            while let Some(e) = c.poll_event() {
                if let Event::ConnectionLost(r) = e {
                    got = Some(r);
                }
            }
        }
        got.is_some()
    });
    assert_eq!(
        got,
        Some(ConnectionError::ApplicationClosed {
            code: 0x42,
            reason: bytes::Bytes::from_static(b"bye")
        })
    );
}

#[test]
fn flow_control_blocks_until_read() {
    let tc = TransportConfig {
        stream_receive_window: 10_000,
        receive_window: 20_000,
        ..Default::default()
    };
    let mut s = ServerConfig::new(tls_server(ECDSA_CHAIN, ECDSA_KEY, &[ALPN]));
    s.transport = Arc::new(tc);
    let mut net = Net::new(s, ClientConfig::new(tls_client(&[ALPN])));
    net.handshake();
    let id = net.c().open_stream(Dir::Uni).unwrap();
    let data = payload(100_000);
    assert_eq!(net.c().stream_send(id, &data).unwrap(), data.len());
    net.c().stream_finish(id).unwrap();
    // 読まないと 10_000 で止まる
    net.run_until(Duration::from_secs(2), |_| false);
    let buffered = net.c().stream_send_buffered(id).unwrap();
    assert!(buffered >= 90_000, "{buffered}");
    let mut got = Vec::new();
    let ok = net.run_until(Duration::from_secs(20), |n| read_all(n.s(), id, &mut got));
    assert!(ok);
    assert_eq!(got, data);
}

#[test]
fn stream_reset_and_stop_sending() {
    let mut net = Net::default_pair(ALPN);
    net.handshake();
    let id = net.c().open_stream(Dir::Bi).unwrap();
    net.c().stream_send(id, b"partial").unwrap();
    net.run_until(Duration::from_millis(200), |_| false);
    net.c().stream_reset(id, 7).unwrap();
    let ok = net.run_until(Duration::from_secs(2), |n| {
        matches!(n.s().stream_recv(id, 100), Err(StreamError::Reset(7)))
            || matches!(n.s().stream_recv(id, 100), Err(StreamError::UnknownStream))
    });
    assert!(ok);
    // STOP_SENDING
    let id2 = net.c().open_stream(Dir::Bi).unwrap();
    net.c().stream_send(id2, b"x").unwrap();
    net.run_until(Duration::from_millis(200), |_| false);
    net.s().stream_stop(id2, 9).unwrap();
    let ok = net.run_until(Duration::from_secs(2), |n| matches!(n.c().stream_send(id2, b"more"), Err(StreamError::Stopped(9))));
    assert!(ok);
}

#[test]
fn stateless_reset_after_server_state_loss() {
    let mut net = Net::default_pair(ALPN);
    net.handshake();
    echo(&mut net, b"x", Duration::from_secs(2));
    // server の状態を失った（同じ鍵の endpoint に置き換える）: 鍵が同じなら reset token も同じ
    // ここでは server 側の接続だけ消して、未知の CID への reset を返させる
    let key_cfg = EndpointConfig::new().unwrap();
    let _ = key_cfg;
    let h = net.sh.unwrap();
    let now = net.now;
    net.server.connection_mut(h).unwrap().close(now, 0, b"");
    // 閉じた通知を client へ届けない
    net.blackhole = true;
    net.run_until(Duration::from_secs(5), |n| n.server.is_empty());
    net.blackhole = false;
    // client が送ると server endpoint は未知の CID として stateless reset を返す
    let id = net.c().open_stream(Dir::Uni).unwrap();
    net.c().stream_send(id, &payload(200)).unwrap();
    let mut lost = None;
    net.run_until(Duration::from_secs(3), |n| {
        if let Some(c) = n.client.connection_mut(n.ch) {
            while let Some(e) = c.poll_event() {
                if let Event::ConnectionLost(r) = e {
                    lost = Some(r);
                }
            }
        }
        lost.is_some()
    });
    assert_eq!(lost, Some(ConnectionError::Reset));
}

#[test]
fn alpn_mismatch_fails_with_crypto_error() {
    let s = ServerConfig::new(tls_server(ECDSA_CHAIN, ECDSA_KEY, &[b"moqt-22"]));
    let mut net = Net::new(s, ClientConfig::new(tls_client(&[b"moqt-99"])));
    let mut lost = None;
    net.run_until(Duration::from_secs(3), |n| {
        if let Some(c) = n.client.connection_mut(n.ch) {
            while let Some(e) = c.poll_event() {
                if let Event::ConnectionLost(r) = e {
                    lost = Some(r);
                }
            }
        }
        lost.is_some()
    });
    match lost {
        Some(ConnectionError::ConnectionClosed { code, .. }) => assert_eq!(code, TransportErrorCode::crypto(120)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn fuzz_datagrams_do_not_panic() {
    let mut rng = Rng(0x5eed_1234_abcd_0001);
    // 1) でたらめな datagram を server endpoint へ
    let mut net = Net::default_pair(ALPN);
    for _ in 0..20_000 {
        let len = (rng.next() % 1400) as usize;
        let mut d: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if !d.is_empty() && rng.chance(0.5) {
            // それらしい long header
            d[0] = 0xc0 | (d[0] & 0x3f);
            if d.len() > 5 {
                d[1..5].copy_from_slice(&1u32.to_be_bytes());
            }
        }
        let now = net.now;
        net.server
            .handle_datagram(now, std::time::SystemTime::now(), addr("10.9.9.9:9"), bytes::BytesMut::from(&d[..]));
        while net.server.poll_transmit(now).is_some() {}
    }
    // 2) 確立した接続へ、本物のパケットを少し壊して入れる（認証で落ちるか、正しく処理される）
    let mut net = Net::default_pair(ALPN);
    net.handshake();
    let mut captured = Vec::new();
    let id = net.c().open_stream(Dir::Bi).unwrap();
    net.c().stream_send(id, &payload(30_000)).unwrap();
    let now = net.now;
    while let Some(t) = net.client.poll_transmit(now) {
        captured.push(t.contents);
    }
    for _ in 0..20_000 {
        let mut d = captured[(rng.next() as usize) % captured.len()].clone();
        let flips = 1 + rng.next() % 4;
        for _ in 0..flips {
            let i = (rng.next() as usize) % d.len();
            d[i] ^= 1 << (rng.next() % 8);
        }
        if rng.chance(0.2) {
            d.truncate((rng.next() as usize) % d.len().max(1));
        }
        let now = net.now;
        let ca = net.client_addr;
        net.server
            .handle_datagram(now, std::time::SystemTime::now(), ca, bytes::BytesMut::from(&d[..]));
    }
    // 接続はまだ生きている（改ざんは AEAD で落ちる）
    echo(&mut net, b"still alive", Duration::from_secs(5));
}
