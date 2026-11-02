//! Association の試験。

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes};

use super::{Link, Sim, XorShift};
use crate::chunk::{self, Chunk, InitChunk, ct, pt};
use crate::{Association, AssociationState, SctpConfig, SctpError, SctpEvent, SendOptions};

type ASim = Sim<Association, Association>;

fn cfg() -> SctpConfig {
    SctpConfig::default()
}

fn pair(seed: u64, link: Link, c: SctpConfig) -> ASim {
    let now = Instant::now();
    let a = Association::new(c.clone(), now).unwrap();
    let b = Association::new(c, now).unwrap();
    Sim::new(a, b, now, seed, link)
}

fn established(s: &ASim) -> bool {
    s.a.state() == AssociationState::Established && s.b.state() == AssociationState::Established
}

fn connected_pair(seed: u64, link: Link, c: SctpConfig) -> ASim {
    let mut s = pair(seed, link, c);
    let now = s.now;
    s.a.connect(now);
    assert!(
        s.run_until(Duration::from_secs(60), established),
        "setup failed"
    );
    s
}

fn messages(ev: &[SctpEvent], stream: u16) -> Vec<Bytes> {
    ev.iter()
        .filter_map(|e| match e {
            SctpEvent::Message {
                stream: s, data, ..
            } if *s == stream => Some(data.clone()),
            _ => None,
        })
        .collect()
}

fn closed_reason(ev: &[SctpEvent]) -> Option<String> {
    ev.iter().find_map(|e| match e {
        SctpEvent::Closed { reason } => Some(reason.clone()),
        _ => None,
    })
}

fn numbered(i: u32, len: usize) -> Bytes {
    let mut v = Vec::with_capacity(len.max(4));
    v.put_u32(i);
    while v.len() < len {
        v.push((i as usize + v.len()) as u8);
    }
    Bytes::from(v)
}

fn number_of(b: &Bytes) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

#[test]
fn setup_one_side() {
    let s = connected_pair(1, Link::default(), cfg());
    assert!(s.a_events.contains(&SctpEvent::Connected));
    assert!(s.b_events.contains(&SctpEvent::Connected));
    assert_eq!(s.a.outbound_streams(), 65535);
    assert_eq!(s.b.inbound_streams(), 65535);
    assert!(s.a.rtt().is_some(), "COOKIE-ECHO の往復で RTT を測る");
}

#[test]
fn setup_with_loss() {
    for seed in 1..20 {
        let s = connected_pair(seed, Link::lossy(30, 10, 20), cfg());
        assert!(established(&s));
    }
}

#[test]
fn stream_limits_use_minimum() {
    let mut c = cfg();
    c.num_streams = 10;
    let now = Instant::now();
    let a = Association::new(c, now).unwrap();
    let b = Association::new(cfg(), now).unwrap();
    let mut s = Sim::new(a, b, now, 3, Link::default());
    s.a.connect(now);
    assert!(s.run_until(Duration::from_secs(10), established));
    assert_eq!(s.a.outbound_streams(), 10);
    assert_eq!(s.b.outbound_streams(), 10);
    let now = s.now;
    assert_eq!(
        s.b.send(
            now,
            10,
            53,
            Bytes::from_static(b"x"),
            SendOptions::default()
        ),
        Err(SctpError::InvalidStream)
    );
}

fn exchange_both_ways(s: &mut ASim) {
    let now = s.now;
    s.a.send(
        now,
        1,
        53,
        Bytes::from_static(b"from a"),
        SendOptions::default(),
    )
    .unwrap();
    s.b.send(
        now,
        2,
        53,
        Bytes::from_static(b"from b"),
        SendOptions::default(),
    )
    .unwrap();
    assert!(s.run_until(Duration::from_secs(30), |s| {
        !messages(&s.b_events, 1).is_empty() && !messages(&s.a_events, 2).is_empty()
    }));
}

#[test]
fn init_collision_simultaneous() {
    for seed in 1..10 {
        let mut s = pair(seed, Link::default(), cfg());
        let now = s.now;
        s.a.connect(now);
        s.b.connect(now);
        assert!(
            s.run_until(Duration::from_secs(30), established),
            "seed {seed}"
        );
        exchange_both_ways(&mut s);
        assert_eq!(
            s.a_events
                .iter()
                .filter(|e| **e == SctpEvent::Connected)
                .count(),
            1
        );
        assert_eq!(
            s.b_events
                .iter()
                .filter(|e| **e == SctpEvent::Connected)
                .count(),
            1
        );
    }
}

#[test]
fn init_collision_staggered() {
    // b が a の INIT に（状態を持たずに）答えた後で connect する場合など、時間をずらして試す。
    for off_ms in (0..100).step_by(3) {
        for (seed, link) in [
            (off_ms + 1, Link::default()),
            (off_ms + 7, Link::lossy(20, 10, 30)),
        ] {
            let mut s = pair(seed, link, cfg());
            let now = s.now;
            s.a.connect(now);
            s.run_for(Duration::from_millis(off_ms));
            let now = s.now;
            s.b.connect(now);
            assert!(
                s.run_until(Duration::from_secs(60), established),
                "offset {off_ms}"
            );
            exchange_both_ways(&mut s);
        }
    }
}

#[test]
fn large_message_fragmented() {
    for (seed, link) in [(5, Link::default()), (6, Link::lossy(10, 5, 10))] {
        let mut s = connected_pair(seed, link, cfg());
        let mut r = XorShift::new(seed);
        let big = Bytes::from(r.bytes(200 * 1024));
        let now = s.now;
        s.a.send(now, 3, 53, big.clone(), SendOptions::default())
            .unwrap();
        assert!(s.a.buffered_amount(3) == big.len());
        assert!(s.run_until(Duration::from_secs(120), |s| {
            !messages(&s.b_events, 3).is_empty()
        }));
        let got = messages(&s.b_events, 3);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], big);
        assert!(s.run_until(Duration::from_secs(30), |s| s.a.buffered_amount(3) == 0));
        assert!(
            s.a_events
                .contains(&SctpEvent::BufferedAmountLow { stream: 3 })
        );
    }
}

#[test]
fn too_large_to_send() {
    let mut s = connected_pair(1, Link::default(), cfg());
    let now = s.now;
    let big = Bytes::from(vec![0u8; 256 * 1024 + 1]);
    assert_eq!(
        s.a.send(now, 0, 53, big.clone(), SendOptions::default()),
        Err(SctpError::TooLarge)
    );
    s.a.set_max_send_message_size(0);
    s.a.send(now, 0, 53, big, SendOptions::default()).unwrap();
    // 送信待ちの上限
    let chunk = Bytes::from(vec![0u8; 1024 * 1024]);
    let mut full = false;
    for _ in 0..8 {
        match s.a.send(now, 0, 53, chunk.clone(), SendOptions::default()) {
            Ok(()) => {}
            Err(SctpError::BufferFull) => {
                full = true;
                break;
            }
            Err(e) => panic!("{e:?}"),
        }
    }
    assert!(full);
    assert!(s.a.stats().buffered <= 4 * 1024 * 1024);
    assert_eq!(
        s.a.send(now, 0, 53, Bytes::new(), SendOptions::default()),
        Err(SctpError::Malformed("empty message"))
    );
}

#[test]
fn receiver_rejects_oversized_message() {
    let now = Instant::now();
    let mut small = cfg();
    small.max_receive_message_size = 10_000;
    let a = Association::new(cfg(), now).unwrap();
    let b = Association::new(small, now).unwrap();
    let mut s = Sim::new(a, b, now, 9, Link::default());
    s.a.connect(now);
    assert!(s.run_until(Duration::from_secs(10), established));
    let now = s.now;
    s.a.send(
        now,
        0,
        53,
        Bytes::from(vec![1u8; 50_000]),
        SendOptions::default(),
    )
    .unwrap();
    assert!(s.run_until(Duration::from_secs(10), |s| {
        closed_reason(&s.b_events).is_some()
    }));
    assert!(s.run_until(Duration::from_secs(10), |s| {
        closed_reason(&s.a_events).is_some()
    }));
    assert_eq!(closed_reason(&s.a_events).as_deref(), Some("peer abort"));
}

#[test]
fn many_messages_under_loss() {
    for seed in [11u64, 12, 13] {
        let mut s = connected_pair(seed, Link::lossy(15, 5, 15), cfg());
        let mut r = XorShift::new(seed * 7);
        let n = 400u32;
        let now = s.now;
        for i in 0..n {
            let len = 4 + r.below(3000) as usize;
            s.a.send(now, 1, 53, numbered(i, len), SendOptions::default())
                .unwrap();
            let len = 4 + r.below(500) as usize;
            s.a.send(
                now,
                2,
                53,
                numbered(i, len),
                SendOptions {
                    unordered: true,
                    ..Default::default()
                },
            )
            .unwrap();
            s.b.send(now, 1, 51, numbered(i, 10), SendOptions::default())
                .unwrap();
        }
        assert!(
            s.run_until(Duration::from_secs(600), |s| {
                messages(&s.b_events, 1).len() == n as usize
                    && messages(&s.b_events, 2).len() == n as usize
                    && messages(&s.a_events, 1).len() == n as usize
            }),
            "seed {seed}: {} {} {}",
            messages(&s.b_events, 1).len(),
            messages(&s.b_events, 2).len(),
            messages(&s.a_events, 1).len()
        );
        let ordered: Vec<u32> = messages(&s.b_events, 1).iter().map(number_of).collect();
        assert_eq!(ordered, (0..n).collect::<Vec<_>>());
        let back: Vec<u32> = messages(&s.a_events, 1).iter().map(number_of).collect();
        assert_eq!(back, (0..n).collect::<Vec<_>>());
        let unordered: BTreeSet<u32> = messages(&s.b_events, 2).iter().map(number_of).collect();
        assert_eq!(unordered.len(), n as usize);
        let st = s.a.stats();
        assert!(st.retransmitted_chunks > 0);
        assert!(st.fast_retransmits > 0 || st.t3_expirations > 0);
        // 全部届いたら送信待ちは空になる。
        assert!(
            s.run_until(Duration::from_secs(60), |s| s.a.stats().outstanding_chunks
                == 0)
        );
        assert_eq!(s.a.stats().buffered, 0);
        assert_eq!(s.b.stats().rx_buffered, 0);
    }
}

#[test]
fn partial_reliability_max_retransmits() {
    for seed in [21u64, 22] {
        let mut s = connected_pair(seed, Link::lossy(20, 0, 10), cfg());
        let now = s.now;
        let n = 300u32;
        let pr = SendOptions {
            max_retransmits: Some(0),
            ..Default::default()
        };
        for i in 0..n {
            s.a.send(now, 4, 53, numbered(i, 100 + (i as usize % 3) * 1500), pr)
                .unwrap();
        }
        // 後から送る信頼のあるメッセージは必ず届く（止まらない）。
        let mut done = false;
        for i in n..n + 20 {
            let now = s.now;
            s.a.send(now, 4, 53, numbered(i, 50), SendOptions::default())
                .unwrap();
            s.run_for(Duration::from_millis(50));
        }
        if s.run_until(Duration::from_secs(300), |s| {
            messages(&s.b_events, 4)
                .iter()
                .filter(|m| number_of(m) >= n)
                .count()
                == 20
        }) {
            done = true;
        }
        assert!(
            done,
            "stalled: a={:#?} b={:#?} got={}",
            s.a.stats(),
            s.b.stats(),
            messages(&s.b_events, 4).len()
        );
        let got: Vec<u32> = messages(&s.b_events, 4).iter().map(number_of).collect();
        let mut sorted = got.clone();
        sorted.sort_unstable();
        assert_eq!(got, sorted, "ordered stream must stay in order");
        let pr_got = got.iter().filter(|&&x| x < n).count();
        assert!(
            pr_got < n as usize,
            "some PR messages should be dropped under loss"
        );
        assert!(s.a.stats().forward_tsn_sent > 0);
        assert!(s.b.stats().forward_tsn_received > 0);
        assert!(s.a.stats().abandoned_messages > 0);
        assert!(
            s.run_until(Duration::from_secs(60), |s| s.a.stats().outstanding_chunks
                == 0)
        );
        assert_eq!(s.a.stats().buffered, 0);
        assert_eq!(s.b.stats().rx_items, 0);
    }
}

#[test]
fn partial_reliability_unordered_under_loss() {
    let mut s = connected_pair(31, Link::lossy(20, 5, 20), cfg());
    let now = s.now;
    let pr = SendOptions {
        unordered: true,
        max_retransmits: Some(1),
        ..Default::default()
    };
    for i in 0..300u32 {
        s.a.send(now, 6, 53, numbered(i, 2000), pr).unwrap();
    }
    s.a.send(now, 6, 53, numbered(9999, 10), SendOptions::default())
        .unwrap();
    assert!(s.run_until(Duration::from_secs(300), |s| {
        messages(&s.b_events, 6)
            .iter()
            .any(|m| number_of(m) == 9999)
    }));
    assert!(
        s.run_until(Duration::from_secs(60), |s| s.a.stats().outstanding_chunks
            == 0)
    );
    assert_eq!(s.b.stats().rx_items, 0, "partial fragments must be dropped");
    assert_eq!(s.b.stats().rx_buffered, 0);
}

#[test]
fn partial_reliability_max_lifetime() {
    let mut s = connected_pair(41, Link::default(), cfg());
    s.block = [true, false];
    let now = s.now;
    let pr = SendOptions {
        max_lifetime: Some(Duration::from_millis(300)),
        ..Default::default()
    };
    for i in 0..5u32 {
        s.a.send(now, 7, 53, numbered(i, 3000), pr).unwrap();
    }
    s.run_for(Duration::from_secs(2));
    s.block = [false, false];
    let now = s.now;
    s.a.send(now, 7, 53, numbered(100, 10), SendOptions::default())
        .unwrap();
    assert!(s.run_until(Duration::from_secs(60), |s| {
        !messages(&s.b_events, 7).is_empty()
    }));
    let got: Vec<u32> = messages(&s.b_events, 7).iter().map(number_of).collect();
    assert_eq!(got, vec![100]);
    assert!(s.a.stats().forward_tsn_sent > 0);
    assert!(s.b.stats().forward_tsn_received > 0);
    // 送る前に期限が切れたものは送らずに捨てる。
    let now = s.now;
    s.a.send(
        now,
        7,
        53,
        numbered(200, 10),
        SendOptions {
            max_lifetime: Some(Duration::ZERO),
            ..Default::default()
        },
    )
    .unwrap();
    s.a.send(now, 7, 53, numbered(201, 10), SendOptions::default())
        .unwrap();
    assert!(
        s.run_until(Duration::from_secs(60), |s| messages(&s.b_events, 7).len()
            == 2)
    );
    assert_eq!(number_of(&messages(&s.b_events, 7)[1]), 201);
}

#[test]
fn stream_reset() {
    for (seed, link) in [
        (51u64, Link::default()),
        (52, Link::lossy(20, 5, 20)),
        (53, Link::lossy(30, 0, 30)),
    ] {
        let mut s = connected_pair(seed, link, cfg());
        let now = s.now;
        for i in 0..20u32 {
            s.a.send(now, 5, 53, numbered(i, 1500), SendOptions::default())
                .unwrap();
        }
        s.a.reset_streams(now, &[5]).unwrap();
        assert_eq!(
            s.a.send(now, 5, 53, numbered(0, 10), SendOptions::default()),
            Err(SctpError::InvalidStream)
        );
        assert!(s.run_until(Duration::from_secs(120), |s| {
            s.a_events
                .contains(&SctpEvent::OutgoingStreamsReset { streams: vec![5] })
        }));
        assert!(s.run_until(Duration::from_secs(60), |s| {
            s.b_events
                .contains(&SctpEvent::IncomingStreamsReset { streams: vec![5] })
        }));
        // リセットの前のメッセージはすべて先に届いている。
        let pos_reset = s
            .b_events
            .iter()
            .position(|e| matches!(e, SctpEvent::IncomingStreamsReset { .. }))
            .unwrap();
        let before: Vec<u32> = messages(&s.b_events[..pos_reset], 5)
            .iter()
            .map(number_of)
            .collect();
        assert_eq!(before, (0..20).collect::<Vec<_>>());
        // リセット後は SSN 0 から。
        let now = s.now;
        s.a.send(now, 5, 53, numbered(1000, 10), SendOptions::default())
            .unwrap();
        s.a.send(now, 5, 53, numbered(1001, 10), SendOptions::default())
            .unwrap();
        assert!(
            s.run_until(Duration::from_secs(60), |s| messages(&s.b_events, 5).len()
                == 22)
        );
        // 逆向きも。
        let now = s.now;
        s.b.reset_streams(now, &[5, 6]).unwrap();
        assert!(s.run_until(Duration::from_secs(60), |s| {
            s.b_events.contains(&SctpEvent::OutgoingStreamsReset {
                streams: vec![5, 6],
            }) && s.a_events.contains(&SctpEvent::IncomingStreamsReset {
                streams: vec![5, 6],
            })
        }));
    }
}

#[test]
fn heartbeat_ack() {
    let mut c = cfg();
    c.heartbeat_interval = Some(Duration::from_secs(1));
    let mut s = connected_pair(61, Link::default(), c);
    s.run_for(Duration::from_secs(5));
    assert!(s.a.stats().heartbeats_acked >= 2);
    assert!(s.b.stats().heartbeats_acked >= 2);
    let rtt = s.b.rtt().unwrap();
    assert!(
        rtt >= Duration::from_millis(39) && rtt <= Duration::from_millis(41),
        "{rtt:?}"
    );
    assert!(established(&s));
}

#[test]
fn heartbeat_failure_closes() {
    let mut c = cfg();
    c.heartbeat_interval = Some(Duration::from_secs(1));
    c.max_retransmits = 3;
    let mut s = connected_pair(62, Link::default(), c);
    s.block = [true, true];
    assert!(s.run_until(Duration::from_secs(600), |s| {
        closed_reason(&s.a_events).is_some()
    }));
    assert_eq!(
        closed_reason(&s.a_events).as_deref(),
        Some("retransmission limit")
    );
}

#[test]
fn shutdown_graceful() {
    let mut s = connected_pair(71, Link::lossy(10, 5, 10), cfg());
    let now = s.now;
    for i in 0..50u32 {
        s.a.send(now, 1, 53, numbered(i, 2000), SendOptions::default())
            .unwrap();
    }
    s.a.shutdown(now);
    assert_eq!(s.a.state(), AssociationState::ShutdownPending);
    assert_eq!(
        s.a.send(now, 1, 53, numbered(0, 10), SendOptions::default()),
        Err(SctpError::Closed)
    );
    assert!(s.run_until(Duration::from_secs(300), |s| {
        closed_reason(&s.a_events).is_some() && closed_reason(&s.b_events).is_some()
    }));
    assert_eq!(closed_reason(&s.a_events).as_deref(), Some("shutdown"));
    assert_eq!(closed_reason(&s.b_events).as_deref(), Some("shutdown"));
    assert_eq!(messages(&s.b_events, 1).len(), 50);
    assert_eq!(s.a.poll_timeout(), None);
    assert_eq!(s.b.poll_timeout(), None);
}

#[test]
fn abort_both_sides_close() {
    let mut s = connected_pair(81, Link::default(), cfg());
    let now = s.now;
    s.a.abort(now);
    assert_eq!(closed_reason(&s.a_events), None);
    s.flush();
    assert_eq!(closed_reason(&s.a_events).as_deref(), Some("local abort"));
    assert!(s.run_until(Duration::from_secs(5), |s| {
        closed_reason(&s.b_events).is_some()
    }));
    assert_eq!(closed_reason(&s.b_events).as_deref(), Some("peer abort"));
    assert!(s.a.is_terminated());
    assert_eq!(
        s.a.send(now, 0, 53, Bytes::from_static(b"x"), SendOptions::default()),
        Err(SctpError::Closed)
    );
}

#[test]
fn retransmit_limit_closes() {
    let mut c = cfg();
    c.max_retransmits = 4;
    c.rto_max = Duration::from_secs(2);
    let mut s = connected_pair(91, Link::default(), c);
    s.block = [true, true];
    let now = s.now;
    s.a.send(
        now,
        0,
        53,
        Bytes::from_static(b"lost"),
        SendOptions::default(),
    )
    .unwrap();
    assert!(s.run_until(Duration::from_secs(120), |s| {
        closed_reason(&s.a_events).is_some()
    }));
    assert_eq!(
        closed_reason(&s.a_events).as_deref(),
        Some("retransmission limit")
    );
    assert_eq!(s.a.stats().t3_expirations, 5);
}

#[test]
fn init_retransmit_limit() {
    let mut c = cfg();
    c.max_init_retransmits = 3;
    let mut s = pair(92, Link::default(), c);
    s.block = [true, true];
    s.record = true;
    let now = s.now;
    s.a.connect(now);
    assert!(s.run_until(Duration::from_secs(600), |s| {
        closed_reason(&s.a_events).is_some()
    }));
    assert_eq!(closed_reason(&s.a_events).as_deref(), Some("init timeout"));
    assert_eq!(s.sent.iter().filter(|(to_b, _)| *to_b).count(), 4);
    // 間隔は倍々に伸びる（1 s, 2 s, 4 s, 8 s で閉じる）。
}

/// a と b の間の開設を 1 パケットずつ手で進める。COOKIE-ECHO のパケットを返す。
fn manual_until_cookie_echo(a: &mut Association, b: &mut Association, now: Instant) -> Bytes {
    a.connect(now);
    let init = a.poll_transmit().unwrap();
    b.handle_packet(now, &init).unwrap();
    assert_eq!(b.state(), AssociationState::Closed, "server keeps no state");
    assert_eq!(b.poll_timeout(), None);
    let init_ack = b.poll_transmit().unwrap();
    a.handle_packet(now, &init_ack).unwrap();
    assert_eq!(a.state(), AssociationState::CookieEchoed);
    a.poll_transmit().unwrap()
}

fn fix_crc(v: &mut [u8]) {
    let c = crate::crc32c::packet_checksum(v);
    v[8..12].copy_from_slice(&c.to_le_bytes());
}

#[test]
fn cookie_tampering_rejected() {
    let now = Instant::now();
    let mut a = Association::new(cfg(), now).unwrap();
    let mut b = Association::new(cfg(), now).unwrap();
    let echo = manual_until_cookie_echo(&mut a, &mut b, now);
    // cookie の中身（ヘッダ 12 + チャンクヘッダ 4 の後）を 1 byte ずつ壊す。
    for i in 16..16 + crate::cookie::COOKIE_LEN {
        let mut v = echo.to_vec();
        v[i] ^= 0x40;
        fix_crc(&mut v);
        assert!(b.handle_packet(now, &v).is_err(), "byte {i}");
        assert_eq!(b.state(), AssociationState::Closed);
        assert!(b.poll_transmit().is_none());
    }
    // checksum が合わないもの。
    let mut v = echo.to_vec();
    v[20] ^= 1;
    assert_eq!(
        b.handle_packet(now, &v),
        Err(SctpError::Malformed("checksum"))
    );
    // 期限切れの cookie。
    let late = now + Duration::from_secs(61);
    assert_eq!(
        b.handle_packet(late, &echo),
        Err(SctpError::Malformed("stale cookie"))
    );
    // 正しいもの。
    b.handle_packet(now + Duration::from_secs(1), &echo)
        .unwrap();
    assert_eq!(b.state(), AssociationState::Established);
    assert_eq!(b.poll_event(), Some(SctpEvent::Connected));
    let ack = b.poll_transmit().unwrap();
    a.handle_packet(now, &ack).unwrap();
    assert_eq!(a.state(), AssociationState::Established);
    // 重複した COOKIE-ECHO には COOKIE-ACK を返し直す。
    b.handle_packet(now, &echo).unwrap();
    let p = chunk::parse_packet(b.poll_transmit().unwrap()).unwrap();
    assert_eq!(p.chunks, vec![Chunk::CookieAck]);
    assert_eq!(b.poll_event(), None);
}

#[test]
fn verification_tag_and_ports_checked() {
    let now = Instant::now();
    let mut a = Association::new(cfg(), now).unwrap();
    let mut b = Association::new(cfg(), now).unwrap();
    let echo = manual_until_cookie_echo(&mut a, &mut b, now);
    b.handle_packet(now, &echo).unwrap();
    let ack = b.poll_transmit().unwrap();
    // 違うタグ。
    let mut v = ack.to_vec();
    v[4] ^= 1;
    fix_crc(&mut v);
    assert_eq!(
        a.handle_packet(now, &v),
        Err(SctpError::Malformed("verification tag"))
    );
    // 違うポート。
    let mut v = ack.to_vec();
    v[1] ^= 1;
    fix_crc(&mut v);
    assert_eq!(
        a.handle_packet(now, &v),
        Err(SctpError::Malformed("port mismatch"))
    );
    assert_eq!(a.state(), AssociationState::CookieEchoed);
    a.handle_packet(now, &ack).unwrap();
    assert_eq!(a.state(), AssociationState::Established);
    // INIT にタグが付いている・ほかのチャンクと一緒。
    let init = chunk::serialize_packet(
        5000,
        5000,
        7,
        &[Chunk::Init(InitChunk {
            initiate_tag: 1,
            a_rwnd: 1000,
            num_outbound_streams: 1,
            num_inbound_streams: 1,
            initial_tsn: 1,
            ..Default::default()
        })],
    );
    assert!(a.handle_packet(now, &init).is_err());
}

#[test]
fn cwnd_slow_start_and_timeout() {
    let mut s = connected_pair(101, Link::default(), cfg());
    let initial = s.a.stats().cwnd;
    assert_eq!(initial, 4380);
    let now = s.now;
    for i in 0..100u32 {
        s.a.send(now, 1, 53, numbered(i, 1100), SendOptions::default())
            .unwrap();
    }
    s.run_for(Duration::from_millis(500));
    let grown = s.a.stats().cwnd;
    assert!(grown > initial * 2, "cwnd {grown}");
    assert!(
        s.run_until(Duration::from_secs(30), |s| messages(&s.b_events, 1).len()
            == 100)
    );
    // 回線を切って T3 を起こす。
    s.block = [true, true];
    let now = s.now;
    s.a.send(now, 1, 53, numbered(1000, 1100), SendOptions::default())
        .unwrap();
    let t3_before = s.a.stats().t3_expirations;
    assert!(
        s.run_until(Duration::from_secs(30), |s| s.a.stats().t3_expirations
            > t3_before)
    );
    let st = s.a.stats();
    assert_eq!(st.cwnd, 1200);
    assert!(st.ssthresh >= 4 * 1200);
    assert!(st.rto >= Duration::from_millis(400));
    s.block = [false, false];
    assert!(
        s.run_until(Duration::from_secs(60), |s| messages(&s.b_events, 1).len()
            == 101)
    );
    assert!(
        s.run_until(Duration::from_secs(60), |s| s.a.stats().outstanding_chunks
            == 0)
    );
    assert_eq!(s.a.stats().error_count, 0);
}

#[test]
fn delayed_sack_timer() {
    let mut s = connected_pair(111, Link::default(), cfg());
    s.record = true;
    let now = s.now;
    s.a.send(
        now,
        1,
        53,
        Bytes::from_static(b"one"),
        SendOptions::default(),
    )
    .unwrap();
    s.flush();
    // a の DATA が届くのは 20 ms 後。SACK は 200 ms 遅れて出る。
    s.run_for(Duration::from_millis(100));
    let sacks = |s: &ASim| {
        s.sent
            .iter()
            .filter(|(to_b, p)| {
                !*to_b
                    && chunk::parse_packet(p.clone())
                        .unwrap()
                        .chunks
                        .iter()
                        .any(|c| matches!(c, Chunk::Sack(_)))
            })
            .count()
    };
    assert_eq!(sacks(&s), 0);
    s.run_for(Duration::from_millis(200));
    assert_eq!(sacks(&s), 1);
}

/// Chrome（dcsctp）らしい INIT を手で作る。
fn chrome_init(tag: u32, tsn: u32, extra_unknown: bool) -> Bytes {
    let mut v = Vec::new();
    let param = |v: &mut Vec<u8>, ty: u16, val: &[u8]| {
        v.put_u16(ty);
        v.put_u16((4 + val.len()) as u16);
        v.put_slice(val);
        while !v.len().is_multiple_of(4) {
            v.push(0);
        }
    };
    v.put_u8(ct::INIT);
    v.put_u8(0);
    v.put_u16(0);
    v.put_u32(tag);
    v.put_u32(131072);
    v.put_u16(65535);
    v.put_u16(65535);
    v.put_u32(tsn);
    // Supported Extensions: FORWARD-TSN・RE-CONFIG・I-DATA・AUTH(?)・ASCONF など
    param(&mut v, pt::SUPPORTED_EXTENSIONS, &[0xC0, 0x82, 0x40, 0x0F]);
    param(&mut v, pt::FORWARD_TSN_SUPPORTED, &[]);
    param(&mut v, pt::ECN_CAPABLE, &[]);
    param(&mut v, pt::RANDOM, &[0x5a; 32]);
    param(&mut v, pt::CHUNK_LIST, &[0x00, 0x03]);
    param(&mut v, pt::HMAC_ALGO, &[0x00, 0x01]);
    param(&mut v, pt::ZERO_CHECKSUM_ACCEPTABLE, &[0, 0, 0, 1]);
    if extra_unknown {
        param(&mut v, 0xC123, &[1, 2, 3, 4]);
        param(&mut v, 0x8124, &[9]);
    }
    let len = v.len() as u16;
    v[2..4].copy_from_slice(&len.to_be_bytes());
    let mut p = Vec::new();
    p.put_u16(5000);
    p.put_u16(5000);
    p.put_u32(0);
    p.put_u32(0);
    p.extend_from_slice(&v);
    fix_crc(&mut p);
    Bytes::from(p)
}

#[test]
fn chrome_style_init() {
    let now = Instant::now();
    let mut b = Association::new(cfg(), now).unwrap();
    let init = chrome_init(0xdead_beef, 12345, false);
    b.handle_packet(now, &init).unwrap();
    assert_eq!(b.state(), AssociationState::Closed);
    assert_eq!(b.poll_timeout(), None);
    let ack = chunk::parse_packet(b.poll_transmit().unwrap()).unwrap();
    assert!(b.poll_transmit().is_none());
    assert_eq!(ack.vtag, 0xdead_beef);
    assert_eq!((ack.src_port, ack.dst_port), (5000, 5000));
    assert_eq!(ack.chunks.len(), 1);
    let Chunk::InitAck(ia) = &ack.chunks[0] else {
        panic!("not INIT-ACK: {:?}", ack.chunks)
    };
    assert_ne!(ia.initiate_tag, 0);
    assert_eq!(ia.a_rwnd, 1024 * 1024);
    assert_eq!(ia.num_outbound_streams, 65535);
    assert_eq!(ia.num_inbound_streams, 65535);
    assert!(ia.forward_tsn_supported);
    assert_eq!(ia.supported_extensions, vec![0xC0, 0x82]);
    assert!(ia.unrecognized.is_empty());
    let cookie = ia.state_cookie.clone().unwrap();
    // 未知の型でも、上位 2 bit が 11 のものは Unrecognized Parameter で返す。
    let mut b2 = Association::new(cfg(), now).unwrap();
    b2.handle_packet(now, &chrome_init(1, 1, true)).unwrap();
    let raw2 = b2.poll_transmit().unwrap();
    let needle = [0x00, 0x08, 0x00, 0x0c, 0xC1, 0x23, 0x00, 0x08, 1, 2, 3, 4];
    assert!(
        raw2.windows(needle.len()).any(|w| w == needle),
        "Unrecognized Parameter"
    );
    assert!(
        !raw2.windows(2).any(|w| w == [0x81, 0x24]),
        "10 は報告しない"
    );

    // Chrome の COOKIE-ECHO（と DATA を一緒に）。
    let my_tag = ia.initiate_tag;
    let data = Chunk::Data(chunk::DataChunk {
        tsn: 12345,
        stream: 1,
        ssn: 0,
        ppid: 50,
        unordered: false,
        begin: true,
        end: true,
        immediate: false,
        payload: Bytes::from_static(&[0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    });
    let echo = chunk::serialize_packet(5000, 5000, my_tag, &[Chunk::CookieEcho { cookie }, data]);
    b.handle_packet(now, &echo).unwrap();
    assert_eq!(b.state(), AssociationState::Established);
    assert_eq!(b.poll_event(), Some(SctpEvent::Connected));
    assert!(matches!(
        b.poll_event(),
        Some(SctpEvent::Message {
            stream: 1,
            ppid: 50,
            ..
        })
    ));
    let out = chunk::parse_packet(b.poll_transmit().unwrap()).unwrap();
    assert_eq!(out.vtag, 0xdead_beef);
    assert_eq!(out.chunks[0], Chunk::CookieAck);
    // SACK は遅延（1 パケット目）か一緒。
    b.handle_timeout(now + Duration::from_millis(250));
    let sack = chunk::parse_packet(b.poll_transmit().unwrap()).unwrap();
    let Chunk::Sack(s) = &sack.chunks[0] else {
        panic!("{:?}", sack.chunks)
    };
    assert_eq!(s.cum_tsn_ack, 12345);
    assert!(s.gap_blocks.is_empty());
    assert_eq!(s.a_rwnd, 1024 * 1024);
    assert_eq!(b.stats().outbound_streams, 65535);
}

#[test]
fn tsn_wraparound() {
    // 初期 TSN を 2^32 の直前にして、TSN・要求番号・SSN の折り返しをまたぐ。
    for (seed, link) in [(201u64, Link::default()), (202, Link::lossy(15, 5, 15))] {
        let mut s = pair(seed, link, cfg());
        s.a.force_initial_tsn(u32::MAX - 50);
        s.b.force_initial_tsn(u32::MAX - 3);
        let now = s.now;
        s.a.connect(now);
        assert!(s.run_until(Duration::from_secs(30), established));
        let now = s.now;
        for i in 0..300u32 {
            let o = SendOptions {
                unordered: i % 5 == 0,
                max_retransmits: if i % 7 == 0 { Some(0) } else { None },
                ..Default::default()
            };
            s.a.send(now, 0, 53, numbered(i, 100 + (i as usize % 4) * 900), o)
                .unwrap();
            s.b.send(now, 0, 53, numbered(i, 50), SendOptions::default())
                .unwrap();
        }
        s.a.reset_streams(now, &[0]).unwrap();
        assert!(s.run_until(Duration::from_secs(300), |s| {
            messages(&s.a_events, 0).len() == 300
                && s.a_events
                    .contains(&SctpEvent::OutgoingStreamsReset { streams: vec![0] })
                && s.a.stats().outstanding_chunks == 0
        }));
        let reliable: Vec<u32> = messages(&s.b_events, 0)
            .iter()
            .map(number_of)
            .filter(|i| i % 5 != 0 && i % 7 != 0)
            .collect();
        let want: Vec<u32> = (0..300).filter(|i| i % 5 != 0 && i % 7 != 0).collect();
        assert_eq!(reliable, want);
        assert_eq!(s.b.stats().rx_items, 0);
    }
}

#[test]
fn ssn_wraparound() {
    // 1 つのストリームで 65536 を超える数のメッセージを送り、SSN の折り返しをまたぐ。
    let mut s = connected_pair(203, Link::lossy(5, 0, 10), cfg());
    let mut next = 0u32;
    while next < 70_000 {
        let now = s.now;
        let start = next;
        for _ in 0..2000 {
            s.a.send(now, 9, 53, numbered(next, 4), SendOptions::default())
                .unwrap();
            next += 1;
        }
        assert!(
            s.run_until(Duration::from_secs(120), |s| messages(&s.b_events, 9).len()
                == 2000)
        );
        let got: Vec<u32> = messages(&s.b_events, 9).iter().map(number_of).collect();
        assert_eq!(got, (start..next).collect::<Vec<_>>());
        // 受け取った分は捨てて覚えすぎない。
        s.b_events.clear();
    }
}

/// いろいろな回線・送り方を混ぜた長めの試験（種を変えて繰り返す）。
#[test]
fn randomized_mixed_traffic() {
    let seeds: u64 = std::env::var("SENCA_SCTP_STRESS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12);
    let start: u64 = std::env::var("SENCA_SCTP_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    for seed in start..start + seeds {
        let mut r = XorShift::new(seed + 1000);
        let link = Link::lossy(r.below(30) as u32, r.below(10) as u32, r.below(40) as u32);
        let mut s = pair(seed + 5000, link, cfg());
        s.record = std::env::var("SENCA_SCTP_DEBUG").is_ok();
        let now = s.now;
        s.a.connect(now);
        if r.chance(50) {
            s.run_for(Duration::from_millis(r.below(80)));
            let now = s.now;
            s.b.connect(now);
        }
        assert!(
            s.run_until(Duration::from_secs(120), established),
            "seed {seed} setup"
        );
        let mut expect_ordered: Vec<Vec<u32>> = vec![Vec::new(); 4];
        let mut expect_unordered: BTreeSet<u32> = BTreeSet::new();
        let mut id = 0u32;
        let mut resets: BTreeSet<u16> = BTreeSet::new();
        for round in 0..30 {
            let now = s.now;
            for _ in 0..r.below(20) {
                id += 1;
                let stream = r.below(4) as u16;
                let len = if r.chance(5) {
                    20_000 + r.below(60_000) as usize
                } else {
                    4 + r.below(2500) as usize
                };
                let kind = r.below(10);
                let opts = match kind {
                    0 => SendOptions {
                        max_retransmits: Some(r.below(3) as u32),
                        ..Default::default()
                    },
                    1 => SendOptions {
                        max_lifetime: Some(Duration::from_millis(r.below(500))),
                        ..Default::default()
                    },
                    2 => SendOptions {
                        unordered: true,
                        max_retransmits: Some(0),
                        ..Default::default()
                    },
                    3 | 4 => SendOptions {
                        unordered: true,
                        ..Default::default()
                    },
                    _ => SendOptions::default(),
                };
                match s.a.send(now, stream, 53, numbered(id, len), opts) {
                    Ok(()) => {
                        if kind >= 5 {
                            expect_ordered[stream as usize].push(id);
                        } else if kind >= 3 {
                            expect_unordered.insert(id);
                        }
                    }
                    Err(SctpError::InvalidStream) => {}
                    Err(e) => panic!("seed {seed}: {e:?}"),
                }
            }
            if round % 7 == 3 {
                let st = r.below(4) as u16;
                if s.a.reset_streams(now, &[st]).is_ok() {
                    resets.insert(st);
                }
            }
            s.run_for(Duration::from_millis(r.below(300)));
        }
        let ok = s.run_until(Duration::from_secs(900), |s| {
            s.a.stats().outstanding_chunks == 0 && s.a.stats().queued_messages == 0
        });
        assert!(ok, "seed {seed}: drain {:#?}", s.a.stats());
        for st in 0..4u16 {
            let got: Vec<u32> = messages(&s.b_events, st)
                .iter()
                .map(number_of)
                .filter(|x| expect_ordered[st as usize].contains(x))
                .collect();
            if got != expect_ordered[st as usize] && s.record {
                for (to_b, p) in &s.sent {
                    let Ok(pk) = chunk::parse_packet(p.clone()) else {
                        continue;
                    };
                    for c in pk.chunks {
                        match &c {
                            Chunk::Data(d) if d.stream == st => eprintln!(
                                "{} DATA tsn={} ssn={} u={} b={} e={} id={}",
                                if *to_b { ">" } else { "<" },
                                d.tsn,
                                d.ssn,
                                d.unordered,
                                d.begin,
                                d.end,
                                if d.begin { number_of(&d.payload) } else { 0 }
                            ),
                            Chunk::ForwardTsn { .. } | Chunk::Reconfig { .. } => {
                                eprintln!("{} {:?}", if *to_b { ">" } else { "<" }, c)
                            }
                            Chunk::Sack(sk) => eprintln!(
                                "{} SACK cum={} gaps={:?}",
                                if *to_b { ">" } else { "<" },
                                sk.cum_tsn_ack,
                                sk.gap_blocks
                            ),
                            _ => {}
                        }
                    }
                }
                eprintln!("B rx: {}", s.b.debug_rx_stream(st));
                eprintln!(
                    "A: {:#?} errors={} a_events={:?}",
                    s.a.stats(),
                    s.errors,
                    s.a_events
                        .iter()
                        .filter(|e| !matches!(e, SctpEvent::Message { .. }))
                        .collect::<Vec<_>>()
                );
                for e in &s.b_events {
                    if let SctpEvent::Message { stream, data, .. } = e {
                        if *stream == st {
                            eprintln!("B got {}", number_of(data));
                        }
                    } else {
                        eprintln!("B ev {e:?}");
                    }
                }
            }
            assert_eq!(got, expect_ordered[st as usize], "seed {seed} stream {st}");
            // 順序付きのメッセージ（PR を含む）は番号順。
            let all: Vec<u32> = s
                .b_events
                .iter()
                .filter_map(|e| match e {
                    SctpEvent::Message {
                        stream,
                        data,
                        unordered: false,
                        ..
                    } if *stream == st => Some(number_of(data)),
                    _ => None,
                })
                .collect();
            assert!(
                all.windows(2).all(|w| w[0] < w[1]),
                "seed {seed} stream {st} order"
            );
        }
        let got_u: BTreeSet<u32> = s
            .b_events
            .iter()
            .filter_map(|e| match e {
                SctpEvent::Message {
                    data,
                    unordered: true,
                    ..
                } => Some(number_of(data)),
                _ => None,
            })
            .collect();
        if !expect_unordered.is_subset(&got_u) && s.record {
            for (to_b, p) in s.sent.iter().rev().take(60).rev() {
                let Ok(pk) = chunk::parse_packet(p.clone()) else {
                    continue;
                };
                let v: Vec<String> = pk
                    .chunks
                    .iter()
                    .map(|c| match c {
                        Chunk::Data(d) => format!(
                            "D{} s{} u{} b{} e{}",
                            d.tsn, d.stream, d.unordered as u8, d.begin as u8, d.end as u8
                        ),
                        Chunk::Sack(k) => format!(
                            "SACK {} {:?} rwnd={}",
                            k.cum_tsn_ack, k.gap_blocks, k.a_rwnd
                        ),
                        o => format!("{o:?}").chars().take(80).collect(),
                    })
                    .collect();
                eprintln!("{} {:?}", if *to_b { ">" } else { "<" }, v);
            }
        }
        assert!(
            expect_unordered.is_subset(&got_u),
            "seed {seed} unordered: missing {:?} a={:#?} a_ev={:?}",
            expect_unordered.difference(&got_u).collect::<Vec<_>>(),
            s.a.stats(),
            s.a_events
                .iter()
                .filter(|e| !matches!(e, SctpEvent::Message { .. }))
                .collect::<Vec<_>>()
        );
        let reset_done = |s: &ASim| -> BTreeSet<u16> {
            s.a_events
                .iter()
                .filter_map(|e| match e {
                    SctpEvent::OutgoingStreamsReset { streams } => Some(streams.clone()),
                    _ => None,
                })
                .flatten()
                .collect()
        };
        assert!(
            s.run_until(Duration::from_secs(300), |s| reset_done(s) == resets),
            "seed {seed} resets {:?}/{resets:?}",
            reset_done(&s)
        );
        assert_eq!(s.b.stats().rx_items, 0, "seed {seed}");
        let now = s.now;
        s.a.shutdown(now);
        assert!(
            s.run_until(Duration::from_secs(600), |s| {
                closed_reason(&s.a_events).is_some() && closed_reason(&s.b_events).is_some()
            }),
            "seed {seed} shutdown"
        );
        assert_eq!(
            closed_reason(&s.a_events).as_deref(),
            Some("shutdown"),
            "seed {seed}"
        );
    }
}
