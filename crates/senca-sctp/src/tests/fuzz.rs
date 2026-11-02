//! 壊れた・でたらめなパケットを各状態の端点に入れる（パニックしない・量が抑えられている）。

use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes};

use super::{Endpoint, Link, Sim, XorShift};
use crate::chunk::{self, Chunk, DataChunk, ReconfigParam, SackChunk};
use crate::{Association, ChannelOptions, DataChannels, SctpConfig, SendOptions, dcep, ppid};

/// 種（`SENCA_SCTP_SEED` でずらせる）。
fn seed(base: u64) -> u64 {
    base + std::env::var("SENCA_SCTP_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn fix_crc(v: &mut [u8]) {
    if v.len() >= 12 {
        let c = crate::crc32c::packet_checksum(v);
        v[8..12].copy_from_slice(&c.to_le_bytes());
    }
}

/// 種のパケットを壊す。
fn mutate(r: &mut XorShift, corpus: &[Bytes], tags: &[u32]) -> Vec<u8> {
    let base = &corpus[r.below(corpus.len() as u64) as usize];
    let mut v = base.to_vec();
    match r.below(8) {
        0 => {
            for _ in 0..1 + r.below(8) {
                let i = r.below(v.len() as u64) as usize;
                v[i] ^= 1 << r.below(8);
            }
        }
        1 => {
            let n = r.below(v.len() as u64 + 1) as usize;
            v.truncate(n.max(12));
        }
        2 => {
            let extra = {
                let n = r.below(64) as usize;
                r.bytes(n)
            };
            v.extend_from_slice(&extra);
        }
        3 => {
            // チャンクの長さ欄を壊す。
            if v.len() >= 16 {
                let l = r.next() as u16;
                v[14..16].copy_from_slice(&l.to_be_bytes());
            }
        }
        4 => {
            // 別のパケットのチャンクをつなぐ。
            let other = &corpus[r.below(corpus.len() as u64) as usize];
            if other.len() > 12 {
                v.extend_from_slice(&other[12..]);
            }
        }
        5 => {
            // 値の範囲をでたらめにする（TSN・SSN・ストリーム番号など）。
            for _ in 0..1 + r.below(4) {
                if v.len() > 16 {
                    let i = 16 + r.below((v.len() - 16) as u64) as usize;
                    v[i] = r.next() as u8;
                }
            }
        }
        6 => {
            let n = 12 + r.below(200) as usize;
            v = r.bytes(n);
            v[0..2].copy_from_slice(&5000u16.to_be_bytes());
            v[2..4].copy_from_slice(&5000u16.to_be_bytes());
        }
        _ => {}
    }
    if v.len() >= 8 && !tags.is_empty() && r.chance(70) {
        let t = tags[r.below(tags.len() as u64) as usize];
        v[4..8].copy_from_slice(&t.to_be_bytes());
    }
    if r.chance(90) {
        fix_crc(&mut v);
    }
    v
}

/// 形は正しいが値がでたらめなチャンク。
fn random_chunk(r: &mut XorShift, near_tsn: u32) -> Chunk {
    let tsn = near_tsn.wrapping_add((r.below(200) as u32).wrapping_sub(100));
    match r.below(10) {
        0 | 1 => Chunk::Data(DataChunk {
            tsn,
            stream: r.below(8) as u16,
            ssn: r.below(8) as u16,
            ppid: [50, 51, 53, 56, 57, 52][r.below(6) as usize],
            unordered: r.chance(30),
            begin: r.chance(60),
            end: r.chance(60),
            immediate: false,
            payload: Bytes::from({
                let n = 1 + r.below(300) as usize;
                r.bytes(n)
            }),
        }),
        2 => Chunk::Sack(SackChunk {
            cum_tsn_ack: tsn,
            a_rwnd: r.next() as u32,
            gap_blocks: (0..r.below(20))
                .map(|_| (r.next() as u16, r.next() as u16))
                .collect(),
            dup_tsns: (0..r.below(5)).map(|_| r.next() as u32).collect(),
        }),
        3 => Chunk::ForwardTsn {
            new_cum_tsn: tsn,
            streams: (0..r.below(10))
                .map(|_| (r.below(10) as u16, r.next() as u16))
                .collect(),
        },
        4 => Chunk::Reconfig {
            params: (0..1 + r.below(2))
                .map(|_| match r.below(4) {
                    0 => ReconfigParam::OutgoingReset {
                        req_seq: r.next() as u32,
                        resp_seq: r.next() as u32,
                        last_tsn: tsn,
                        streams: (0..r.below(5)).map(|_| r.below(10) as u16).collect(),
                    },
                    1 => ReconfigParam::Response {
                        resp_seq: r.next() as u32,
                        result: r.below(8) as u32,
                        next_tsns: None,
                    },
                    2 => ReconfigParam::IncomingReset {
                        req_seq: r.next() as u32,
                        streams: vec![1],
                    },
                    _ => ReconfigParam::AddOutgoing {
                        req_seq: r.next() as u32,
                        count: 3,
                    },
                })
                .collect(),
        },
        5 => Chunk::Heartbeat {
            info: Bytes::from({
                let n = r.below(40) as usize;
                r.bytes(n)
            }),
        },
        6 => Chunk::HeartbeatAck {
            info: Bytes::from(r.bytes(12)),
        },
        7 => Chunk::Shutdown { cum_tsn_ack: tsn },
        8 => Chunk::Error {
            causes: Bytes::from(r.bytes(8)),
        },
        _ => Chunk::Unknown {
            chunk_type: r.next() as u8 | 0x80,
            raw: {
                let mut v = vec![0u8; 4 + r.below(16) as usize];
                v[0] = 0xC5;
                let l = v.len() as u16;
                v[2..4].copy_from_slice(&l.to_be_bytes());
                Bytes::from(v)
            },
        },
    }
}

/// パケットの中の TSN（DATA）の目安を拾う。
fn some_tsn(corpus: &[Bytes]) -> u32 {
    for p in corpus {
        if let Ok(pk) = chunk::parse_packet(p.clone()) {
            for c in pk.chunks {
                if let Chunk::Data(d) = c {
                    return d.tsn;
                }
            }
        }
    }
    0
}

fn tags_of(corpus: &[Bytes]) -> Vec<u32> {
    let mut t: Vec<u32> = corpus
        .iter()
        .filter(|p| p.len() >= 8)
        .map(|p| u32::from_be_bytes([p[4], p[5], p[6], p[7]]))
        .collect();
    t.sort_unstable();
    t.dedup();
    t
}

fn hammer<E: Endpoint>(
    ep: &mut E,
    now: &mut Instant,
    r: &mut XorShift,
    corpus: &[Bytes],
    iters: usize,
    check: &mut dyn FnMut(&E),
) {
    let tags = tags_of(corpus);
    let tsn = some_tsn(corpus);
    for i in 0..iters {
        let p: Vec<u8> = if r.chance(30) && !tags.is_empty() {
            let n = 1 + r.below(4) as usize;
            let chunks: Vec<Chunk> = (0..n).map(|_| random_chunk(r, tsn)).collect();
            let t = tags[r.below(tags.len() as u64) as usize];
            chunk::serialize_packet(5000, 5000, t, &chunks).to_vec()
        } else {
            mutate(r, corpus, &tags)
        };
        let _ = ep.ep_handle_packet(*now, &p);
        let mut n = 0;
        while ep.ep_poll_transmit().is_some() {
            n += 1;
            assert!(n < 100_000, "unbounded output");
        }
        let mut n = 0;
        while ep.ep_poll_event().is_some() {
            n += 1;
            assert!(n < 1_000_000, "unbounded events");
        }
        if i % 16 == 0 {
            *now += Duration::from_millis(r.below(3000));
            if ep.ep_poll_timeout().is_some_and(|t| t <= *now) {
                ep.ep_handle_timeout(*now);
            }
        }
        check(ep);
    }
}

/// 種のパケットを集める（いろいろな種類のチャンクを含む通信を記録する）。
fn collect_corpus(seed: u64) -> (Sim<Association, Association>, Vec<Bytes>) {
    let now = Instant::now();
    let a = Association::new(SctpConfig::default(), now).unwrap();
    let b = Association::new(SctpConfig::default(), now).unwrap();
    let mut s = Sim::new(a, b, now, seed, Link::lossy(15, 5, 15));
    s.record = true;
    s.a.connect(now);
    s.b.connect(now);
    assert!(s.run_until(Duration::from_secs(30), |s| {
        s.a.state() == crate::AssociationState::Established
            && s.b.state() == crate::AssociationState::Established
    }));
    let now = s.now;
    for i in 0..40u32 {
        let o = SendOptions {
            unordered: i % 3 == 0,
            max_retransmits: if i % 4 == 0 { Some(0) } else { None },
            ..Default::default()
        };
        s.a.send(
            now,
            (i % 5) as u16,
            53,
            Bytes::from(vec![i as u8; 100 + 900 * (i as usize % 4)]),
            o,
        )
        .unwrap();
        s.b.send(now, (i % 3) as u16, 51, Bytes::from(vec![i as u8; 50]), o)
            .unwrap();
    }
    s.a.reset_streams(now, &[1, 2]).unwrap();
    s.run_for(Duration::from_secs(10));
    let corpus = s.sent.iter().map(|(_, p)| p.clone()).collect();
    (s, corpus)
}

#[test]
fn fuzz_association_all_states() {
    let mut r = XorShift::new(seed(0xF00D));
    let (_, corpus) = collect_corpus(1);
    assert!(corpus.len() > 50);
    let window = SctpConfig::default().receive_window as usize;
    let mut check = |a: &Association| {
        let st = a.stats();
        assert!(
            st.rx_buffered <= 2 * window,
            "rx_buffered {}",
            st.rx_buffered
        );
        assert!(st.rx_items <= 3 * 16384 + 10, "rx_items {}", st.rx_items);
        assert!(st.outstanding_chunks <= 16384);
        assert!(st.buffered <= SctpConfig::default().max_send_buffer);
    };
    // つながった状態（タグが合うので深く入る）。時間を進めると相手がいないので閉じるため、
    // 何度か作り直す。
    let mut deep = 0;
    for round in 0..12u64 {
        let (mut s, corpus) = collect_corpus(seed(10 + round));
        let mut now = s.now;
        let before = s.b.stats().packets_received;
        hammer(&mut s.b, &mut now, &mut r, &corpus, 400, &mut check);
        deep += s.b.stats().packets_received - before;
        hammer(&mut s.a, &mut now, &mut r, &corpus, 400, &mut check);
    }
    // 多くのパケットが checksum とタグを通って中まで届いている。
    assert!(deep > 500, "deep {deep}");

    // 開設前の各状態。
    let now0 = Instant::now();
    for state in 0..4 {
        let mut a = Association::new(SctpConfig::default(), now0).unwrap();
        let mut b = Association::new(SctpConfig::default(), now0).unwrap();
        match state {
            0 => {}
            1 => a.connect(now0),
            2 | 3 => {
                a.connect(now0);
                let init = a.poll_transmit().unwrap();
                b.handle_packet(now0, &init).unwrap();
                let ia = b.poll_transmit().unwrap();
                a.handle_packet(now0, &ia).unwrap();
                if state == 3 {
                    a.shutdown(now0);
                }
            }
            _ => unreachable!(),
        }
        let mut now = now0;
        hammer(&mut a, &mut now, &mut r, &corpus, 2500, &mut check);
    }

    // SHUTDOWN の途中。
    let (mut s, corpus) = collect_corpus(2);
    let now = s.now;
    s.block = [true, true];
    s.a.shutdown(now);
    s.run_for(Duration::from_millis(1));
    let mut now = s.now;
    hammer(&mut s.a, &mut now, &mut r, &corpus, 3000, &mut check);
    hammer(&mut s.b, &mut now, &mut r, &corpus, 3000, &mut check);
}

#[test]
fn fuzz_random_bytes() {
    let mut r = XorShift::new(seed(77));
    let now = Instant::now();
    let mut a = Association::new(SctpConfig::default(), now).unwrap();
    let mut d = DataChannels::new(SctpConfig::default(), true, now).unwrap();
    d.connect(now);
    for _ in 0..20000 {
        let n = r.below(300) as usize;
        let mut v = r.bytes(n);
        if r.chance(50) {
            fix_crc(&mut v);
        }
        let _ = a.handle_packet(now, &v);
        let _ = d.handle_packet(now, &v);
        let _ = chunk::parse_packet(Bytes::from(v.clone()));
        let _ = dcep::parse(&v);
    }
    while a.poll_transmit().is_some() {}
    while d.poll_transmit().is_some() {}
}

#[test]
fn fuzz_datachannels() {
    let now = Instant::now();
    let a = DataChannels::new(SctpConfig::default(), true, now).unwrap();
    let b = DataChannels::new(SctpConfig::default(), false, now).unwrap();
    let mut s = Sim::new(a, b, now, 5, Link::lossy(10, 5, 10));
    s.record = true;
    s.a.connect(now);
    s.b.connect(now);
    assert!(s.run_until(Duration::from_secs(30), |s| s.a.is_connected()
        && s.b.is_connected()));
    let now = s.now;
    let id = s.a.open(now, ChannelOptions::default()).unwrap();
    let id2 =
        s.b.open(
            now,
            ChannelOptions {
                ordered: false,
                ..Default::default()
            },
        )
        .unwrap();
    s.run_for(Duration::from_secs(2));
    let now = s.now;
    for i in 0..20 {
        let _ = s.a.send_text(now, id, &format!("hello {i}"));
        let _ = s.b.send_binary(now, id2, Bytes::from(vec![i as u8; 2000]));
    }
    s.run_for(Duration::from_secs(2));
    let now = s.now;
    let _ = s.a.close(now, id);
    s.run_for(Duration::from_secs(5));
    let corpus: Vec<Bytes> = s.sent.iter().map(|(_, p)| p.clone()).collect();
    let mut r = XorShift::new(seed(99));
    let mut check = |d: &DataChannels| {
        assert!(d.channels().len() <= crate::datachannel::MAX_CHANNELS);
        assert!(d.association().stats().rx_buffered <= 2 * 1024 * 1024);
    };
    let mut now = s.now;
    hammer(&mut s.a, &mut now, &mut r, &corpus, 4000, &mut check);
    hammer(&mut s.b, &mut now, &mut r, &corpus, 4000, &mut check);
}

#[test]
fn fuzz_dcep_payloads() {
    // 生の Association から DataChannels に、でたらめな DCEP を送る。
    let now = Instant::now();
    let a = Association::new(SctpConfig::default(), now).unwrap();
    let d = DataChannels::new(SctpConfig::default(), true, now).unwrap();
    let mut s = Sim::new(a, d, now, 17, Link::default());
    s.a.connect(now);
    assert!(s.run_until(Duration::from_secs(10), |s| s.b.is_connected()));
    let mut r = XorShift::new(seed(1234));
    for i in 0..3000u32 {
        let now = s.now;
        let stream = r.below(64) as u16;
        let payload: Vec<u8> = match r.below(4) {
            0 => {
                let n = 1 + r.below(40) as usize;
                r.bytes(n)
            }
            1 => {
                // 形の近い OPEN。
                let ll = r.below(20) as u16;
                let pl = r.below(20) as u16;
                let mut v = vec![0x03, [0, 0x80, 1, 0x81, 2, 0x82, 0x7f][r.below(7) as usize]];
                v.put_u16(r.next() as u16);
                v.put_u32(r.next() as u32);
                v.put_u16(ll);
                v.put_u16(pl);
                let body = {
                    let n = (ll + pl) as usize + r.below(3) as usize;
                    r.bytes(n)
                };
                v.extend_from_slice(&body);
                v
            }
            2 => vec![0x02],
            _ => {
                let mut v = vec![0x03, 0];
                v.put_u16(0);
                v.put_u32(0);
                v.put_u16(3);
                v.put_u16(0);
                v.extend_from_slice(b"abc");
                v
            }
        };
        let p = if r.chance(85) {
            ppid::DCEP
        } else {
            [51, 53, 56, 57, 52, 54, 99][r.below(7) as usize]
        };
        let _ =
            s.a.send(now, stream, p, Bytes::from(payload), SendOptions::default());
        if i % 10 == 0 {
            s.run_for(Duration::from_millis(100));
        }
    }
    s.run_for(Duration::from_secs(5));
    assert!(s.b.is_connected());
    // 相手（奇数）の番号のチャネルだけが開く。
    for c in s.b.channels() {
        assert_eq!(c.id % 2, 1);
        assert!(c.open);
    }
}
