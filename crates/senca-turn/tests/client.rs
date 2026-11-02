//! TURN クライアントの試験（sans-io のサーバーとつなぐ）。

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use senca_turn::server::RestSecret;
use senca_turn::server::{RelayIp, StaticUser};
use senca_turn::{
    Transmit, TurnClient, TurnClientConfig, TurnClientEvent, TurnInput, TurnServer,
    TurnServerConfig, TurnServerEvent, rest_api_credentials,
};

fn sa(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

const LISTEN: &str = "198.51.100.1:3478";
const CLIENT: &str = "1.1.1.1:5000";
const PEER: &str = "8.8.8.8:9000";

fn server() -> TurnServer {
    let mut s = TurnServer::new(TurnServerConfig {
        realm: "r".into(),
        users: vec![StaticUser {
            username: "u".into(),
            password: "p".into(),
            tenant: None,
        }],
        rest_secrets: vec![RestSecret {
            secret: b"sec".to_vec(),
            tenant: None,
        }],
        relay_v4: Some(RelayIp {
            local: "10.0.0.1".parse().unwrap(),
            external: Some("198.51.100.1".parse().unwrap()),
        }),
        port_min: 50000,
        port_max: 50010,
        ..Default::default()
    })
    .unwrap();
    s.set_unix_time(1_000_000);
    s
}

/// クライアントとサーバーの間でキューが空になるまで回す。peer 宛の送信は返す。
fn pump(
    now: Instant,
    c: &mut TurnClient,
    s: &mut TurnServer,
    drop_first: &mut usize,
) -> (Vec<Transmit>, Vec<(SocketAddr, Bytes)>) {
    let mut to_peer = Vec::new();
    let mut data = Vec::new();
    for _ in 0..100 {
        let mut moved = false;
        while let Some(t) = c.poll_transmit() {
            moved = true;
            if *drop_first > 0 {
                *drop_first -= 1;
                continue;
            }
            assert_eq!(t.remote, sa(LISTEN));
            s.handle_client(now, t.remote, t.local, t.data);
        }
        while let Some(t) = s.poll_transmit() {
            moved = true;
            if t.remote == sa(CLIENT) {
                match c.handle_input(now, t.local, t.data) {
                    TurnInput::Data { peer, data: d } => data.push((peer, d)),
                    TurnInput::Consumed => {}
                    TurnInput::NotMine => panic!("unexpected"),
                }
            } else {
                to_peer.push(t);
            }
        }
        if !moved {
            break;
        }
    }
    (to_peer, data)
}

fn events(c: &mut TurnClient) -> Vec<TurnClientEvent> {
    std::iter::from_fn(|| c.poll_event()).collect()
}

#[test]
fn allocate_permission_channel_and_data() {
    let t0 = Instant::now();
    let mut s = server();
    let mut c = TurnClient::new(TurnClientConfig::new(sa(CLIENT), sa(LISTEN), "u", "p"));
    c.allocate(t0).unwrap();
    let mut drop = 0;
    pump(t0, &mut c, &mut s, &mut drop);
    let ev = events(&mut c);
    let relayed = match &ev[..] {
        [
            TurnClientEvent::Allocated {
                relayed,
                mapped,
                lifetime,
            },
        ] => {
            assert_eq!(*mapped, Some(sa(CLIENT)));
            assert_eq!(*lifetime, Duration::from_secs(600));
            *relayed
        }
        e => panic!("{e:?}"),
    };
    assert_eq!(
        relayed.ip(),
        "198.51.100.1".parse::<std::net::IpAddr>().unwrap()
    );
    let relay_local = match s.poll_event() {
        Some(TurnServerEvent::AllocationCreated { relay, .. }) => relay,
        e => panic!("{e:?}"),
    };

    // permission 前の peer からのデータは届かない
    s.handle_peer(t0, relay_local, sa(PEER), Bytes::from_static(b"early"));
    let (_, d) = pump(t0, &mut c, &mut s, &mut drop);
    assert!(d.is_empty());

    c.create_permission(t0, sa(PEER).ip()).unwrap();
    pump(t0, &mut c, &mut s, &mut drop);
    assert_eq!(
        events(&mut c),
        vec![TurnClientEvent::PermissionCreated {
            peer: sa(PEER).ip()
        }]
    );

    // Send indication → peer
    let t = c.send(sa(PEER), b"hello").unwrap();
    s.handle_client(t0, t.remote, t.local, t.data);
    let (out, _) = pump(t0, &mut c, &mut s, &mut drop);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].remote, sa(PEER));
    assert_eq!(out[0].local, relay_local);
    assert_eq!(&out[0].data[..], b"hello");

    // peer → Data indication
    s.handle_peer(t0, relay_local, sa(PEER), Bytes::from_static(b"back"));
    let (_, d) = pump(t0, &mut c, &mut s, &mut drop);
    assert_eq!(d, vec![(sa(PEER), Bytes::from_static(b"back"))]);

    // channel
    c.channel_bind(t0, sa(PEER)).unwrap();
    pump(t0, &mut c, &mut s, &mut drop);
    assert_eq!(
        events(&mut c),
        vec![TurnClientEvent::ChannelBound {
            peer: sa(PEER),
            channel: 0x4000
        }]
    );
    let t = c.send(sa(PEER), b"via channel").unwrap();
    assert_eq!(t.data[0], 0x40);
    s.handle_client(t0, t.remote, t.local, t.data);
    let (out, _) = pump(t0, &mut c, &mut s, &mut drop);
    assert_eq!(&out[0].data[..], b"via channel");
    s.handle_peer(t0, relay_local, sa(PEER), Bytes::from_static(b"cd back"));
    let (_, d) = pump(t0, &mut c, &mut s, &mut drop);
    assert_eq!(d, vec![(sa(PEER), Bytes::from_static(b"cd back"))]);

    // 時間を進めると Refresh・permission・channel の更新が自動で行われ、allocation が保たれる
    let mut now = t0;
    for _ in 0..(1200 / 5) {
        now += Duration::from_secs(5);
        c.handle_timeout(now);
        s.handle_timeout(now);
        pump(now, &mut c, &mut s, &mut drop);
    }
    let ev = events(&mut c);
    assert!(
        ev.iter()
            .any(|e| matches!(e, TurnClientEvent::Refreshed { .. })),
        "{ev:?}"
    );
    assert!(
        !ev.iter()
            .any(|e| matches!(e, TurnClientEvent::AllocationLost))
    );
    assert!(c.is_allocated());
    s.handle_peer(now, relay_local, sa(PEER), Bytes::from_static(b"still"));
    let (_, d) = pump(now, &mut c, &mut s, &mut drop);
    assert_eq!(d, vec![(sa(PEER), Bytes::from_static(b"still"))]);

    // 解放
    c.deallocate(now).unwrap();
    pump(now, &mut c, &mut s, &mut drop);
    assert_eq!(events(&mut c), vec![TurnClientEvent::Closed]);
    assert_eq!(s.allocation_count(), 0);
}

#[test]
fn retransmits_lost_requests() {
    let t0 = Instant::now();
    let mut s = server();
    let mut c = TurnClient::new(TurnClientConfig::new(sa(CLIENT), sa(LISTEN), "u", "p"));
    c.allocate(t0).unwrap();
    let mut drop = 1; // 最初の Allocate を落とす
    pump(t0, &mut c, &mut s, &mut drop);
    assert!(events(&mut c).is_empty());
    let next = c.poll_timeout().unwrap();
    assert_eq!(next - t0, Duration::from_millis(500));
    c.handle_timeout(next);
    pump(next, &mut c, &mut s, &mut drop);
    assert!(matches!(
        events(&mut c)[..],
        [TurnClientEvent::Allocated { .. }]
    ));
}

#[test]
fn wrong_password_fails_and_timeout_fails() {
    let t0 = Instant::now();
    let mut s = server();
    let mut c = TurnClient::new(TurnClientConfig::new(sa(CLIENT), sa(LISTEN), "u", "wrong"));
    c.allocate(t0).unwrap();
    let mut drop = 0;
    pump(t0, &mut c, &mut s, &mut drop);
    assert!(matches!(
        events(&mut c)[..],
        [TurnClientEvent::AllocationFailed {
            code: Some(401),
            ..
        }]
    ));

    // 応答が無ければ 39.5 秒で失敗
    let mut c = TurnClient::new(TurnClientConfig::new(sa(CLIENT), sa(LISTEN), "u", "p"));
    c.allocate(t0).unwrap();
    while c.poll_transmit().is_some() {}
    let mut now = t0;
    while let Some(t) = c.poll_timeout() {
        now = t;
        c.handle_timeout(now);
        while c.poll_transmit().is_some() {}
    }
    assert_eq!(now - t0, Duration::from_millis(39_500));
    assert!(matches!(
        events(&mut c)[..],
        [TurnClientEvent::AllocationFailed { code: None, .. }]
    ));
}

#[test]
fn rest_credentials_and_denied_peer() {
    let t0 = Instant::now();
    let mut s = server();
    let (u, p) = rest_api_credentials(b"sec", "carol", 2_000_000);
    let mut c = TurnClient::new(TurnClientConfig::new(sa(CLIENT), sa(LISTEN), &u, &p));
    c.allocate(t0).unwrap();
    let mut drop = 0;
    pump(t0, &mut c, &mut s, &mut drop);
    assert!(matches!(
        events(&mut c)[..],
        [TurnClientEvent::Allocated { .. }]
    ));
    c.create_permission(t0, "192.168.1.1".parse().unwrap())
        .unwrap();
    pump(t0, &mut c, &mut s, &mut drop);
    assert_eq!(
        events(&mut c),
        vec![TurnClientEvent::PermissionFailed {
            peer: "192.168.1.1".parse().unwrap(),
            code: Some(403)
        }]
    );
}

#[test]
fn ignores_foreign_and_forged_input() {
    let t0 = Instant::now();
    let mut c = TurnClient::new(TurnClientConfig::new(sa(CLIENT), sa(LISTEN), "u", "p"));
    c.allocate(t0).unwrap();
    assert_eq!(
        c.handle_input(t0, sa(PEER), Bytes::from_static(b"x")),
        TurnInput::NotMine
    );
    assert_eq!(
        c.handle_input(t0, sa(LISTEN), Bytes::from_static(&[0x40, 0, 0, 9])),
        TurnInput::Consumed
    );
    assert_eq!(
        c.handle_input(t0, sa(LISTEN), Bytes::from_static(&[0x80, 0, 0, 0])),
        TurnInput::NotMine
    );
    let mut seed = 7u32;
    for _ in 0..5000 {
        let mut v = vec![0u8; (seed % 64) as usize];
        for b in v.iter_mut() {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            *b = (seed >> 16) as u8;
        }
        if v.len() > 8 {
            v[0] &= 0x3f;
            v[4..8].copy_from_slice(&0x2112A442u32.to_be_bytes());
        }
        let _ = c.handle_input(t0, sa(LISTEN), Bytes::from(v));
    }
}
