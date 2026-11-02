//! 2 つの sans-io 端点を、損失・遅延・順序入れ替えのある模擬回線でつなぐ試験。
//! 乱数は固定の種から作るので結果は毎回同じ。

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::net::SocketAddr;

use crate::*;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn f64(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

#[derive(Clone, Copy)]
struct Link {
    loss: f64,
    delay: u64,
    jitter: u64,
}

const LA: &str = "10.0.0.1:9000";
const CA: &str = "10.0.0.2:5000";

type Flight = BinaryHeap<Reverse<(u64, u64, bool, Vec<u8>)>>;

struct Sim {
    now: u64,
    l: Listener,
    c: Connection,
    la: SocketAddr,
    ca: SocketAddr,
    flight: Flight,
    n: u64,
    rng: Rng,
    up: Link,
    down: Link,
    blackhole: bool,
    lid: Option<ConnectionId>,
    l_events: Vec<Event>,
    c_events: Vec<Event>,
    l_closed: Option<CloseReason>,
}

impl Sim {
    fn new(lcfg: ListenerConfig, params: AcceptParams, ccfg: Config, up: Link, down: Link, seed: u64) -> Sim {
        Self::with_accept(lcfg, move |_| AcceptDecision::Accept(params.clone()), ccfg, up, down, seed)
    }

    fn with_accept(
        lcfg: ListenerConfig,
        accept: impl FnMut(&AcceptRequest<'_>) -> AcceptDecision + Send + 'static,
        ccfg: Config,
        up: Link,
        down: Link,
        seed: u64,
    ) -> Sim {
        let la: SocketAddr = LA.parse().unwrap();
        let ca: SocketAddr = CA.parse().unwrap();
        let now = 10_000_000_000;
        Sim {
            now,
            l: Listener::new(lcfg, accept).unwrap(),
            c: Connection::connect(ccfg, la, now).unwrap(),
            la,
            ca,
            flight: BinaryHeap::new(),
            n: 0,
            rng: Rng(seed | 1),
            up,
            down,
            blackhole: false,
            lid: None,
            l_events: Vec::new(),
            c_events: Vec::new(),
            l_closed: None,
        }
    }

    fn enqueue(&mut self, to_listener: bool, b: Vec<u8>) {
        if self.blackhole {
            return;
        }
        let link = if to_listener { self.up } else { self.down };
        let lost = self.rng.f64() < link.loss;
        if let Ok(w) = std::env::var("SIMTRACE") {
            let w: u64 = w.parse().unwrap();
            match crate::packet::parse(&b) {
                Ok(crate::packet::Packet::Data(h, body)) if body.len() >= 8 => {
                    let i = u64::from_be_bytes(body[0..8].try_into().unwrap());
                    if i == w {
                        eprintln!("{} data i={i} seq={} R={} lost={lost} up={to_listener}", self.now, h.seq, h.retransmitted);
                    }
                }
                Ok(crate::packet::Packet::Control(h, cif)) if h.ctype == crate::packet::ControlType::Nak => {
                    eprintln!("{} NAK {:?} lost={lost}", self.now, crate::packet::decode_loss_list(cif, 100));
                }
                _ => {}
            }
        }
        if lost {
            return;
        }
        let at = self.now + link.delay + self.rng.below(link.jitter + 1);
        self.n += 1;
        self.flight.push(Reverse((at, self.n, to_listener, b)));
    }

    fn pump(&mut self) {
        for _ in 0..100_000 {
            let mut any = false;
            if let Some((to, b)) = self.c.poll_transmit() {
                assert_eq!(to, self.la);
                self.enqueue(true, b.to_vec());
                any = true;
            }
            if let Some((to, b)) = self.l.poll_transmit() {
                assert_eq!(to, self.ca);
                self.enqueue(false, b.to_vec());
                any = true;
            }
            if !any {
                break;
            }
        }
        while let Some(e) = self.c.poll_event() {
            self.c_events.push(e);
        }
        while let Some((id, e)) = self.l.poll_event() {
            if matches!(e, Event::Connected(_)) {
                self.lid = Some(id);
            }
            if let Event::Closed { reason } = e {
                self.l_closed = Some(reason);
            }
            self.l_events.push(e);
        }
    }

    /// 次の出来事まで（最大 `limit` まで）時刻を進める。
    fn step(&mut self, limit: u64) {
        let mut t = limit;
        if let Some(Reverse((at, ..))) = self.flight.peek() {
            t = t.min(*at);
        }
        if let Some(x) = self.c.poll_timeout() {
            t = t.min(x);
        }
        if let Some(x) = self.l.poll_timeout() {
            t = t.min(x);
        }
        self.now = self.now.max(t);
        while let Some(Reverse((at, ..))) = self.flight.peek() {
            if *at > self.now {
                break;
            }
            let Reverse((_, _, to_l, b)) = self.flight.pop().unwrap();
            if to_l {
                self.l.handle_datagram(self.now, self.ca, &b);
            } else {
                self.c.handle_datagram(self.now, self.la, &b);
            }
        }
        self.c.handle_timeout(self.now);
        self.l.handle_timeout(self.now);
        self.pump();
    }

    fn run_until(&mut self, end: u64) {
        while self.now < end {
            self.step(end);
        }
    }

    fn connect(&mut self) {
        self.pump();
        let end = self.now + 2_000_000;
        while self.now < end && !self.c.is_connected() && !self.c.is_closed() {
            self.step(end);
        }
    }
}

fn msg(i: u64, sent: u64, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    v.extend_from_slice(&i.to_be_bytes());
    v.extend_from_slice(&sent.to_be_bytes());
    while v.len() < len {
        v.push((i as u8).wrapping_mul(31).wrapping_add(v.len() as u8));
    }
    v
}

fn check_msg(p: &[u8]) -> (u64, u64) {
    let i = u64::from_be_bytes(p[0..8].try_into().unwrap());
    let sent = u64::from_be_bytes(p[8..16].try_into().unwrap());
    assert_eq!(p, &msg(i, sent, p.len())[..], "payload corrupted");
    (i, sent)
}

struct Report {
    sent: u64,
    got: Vec<u64>,
    max_late: i64,
    min_late: i64,
}

/// 受け取ったメッセージを検査する：順序どおり・重複なし・中身が正しい・渡した時刻が送信 + 遅延の近く。
fn verify(events: &[Event], sent: u64, latency_us: u64, max_owd: u64) -> Report {
    // 遅延は受信側の合意値（max(自分の受信遅延, 相手の peer 遅延)）
    let latency_us = events
        .iter()
        .find_map(|e| if let Event::Connected(i) = e { Some(i.latency_ms as u64 * 1000) } else { None })
        .unwrap_or(latency_us);
    let mut got = Vec::new();
    let mut max_late = i64::MIN;
    let mut min_late = i64::MAX;
    for e in events {
        if let Event::Data { payload, deliver_at, .. } = e {
            let (i, s) = check_msg(payload);
            if let Some(&last) = got.last() {
                assert!(i > last, "out of order: {i} after {last}");
            }
            got.push(i);
            let late = *deliver_at as i64 - s as i64 - latency_us as i64;
            if late < -1000 && std::env::var("SIMDBG").is_ok() {
                eprintln!("early i={i} sent={s} deliver_at={deliver_at} late={late}");
            }
            max_late = max_late.max(late);
            min_late = min_late.min(late);
        }
    }
    if !got.is_empty() {
        assert!(min_late >= -1_000, "delivered too early: {min_late}");
        assert!(max_late <= max_owd as i64 + 2_000, "delivered too late: {max_late}");
    }
    Report { sent, got, max_late, min_late }
}

struct Run {
    sim: Sim,
    c_sent: u64,
    l_sent: u64,
}

/// `dur` µs のあいだ、両方向（`bidir`）に `interval` µs ごとに 1316 バイトを送る。
fn transfer(mut sim: Sim, dur: u64, interval: u64, bidir: bool) -> Run {
    sim.connect();
    assert!(sim.c.is_connected(), "not connected: {:?}", sim.c_events);
    let lid = sim.lid.expect("listener connection");
    let start = sim.now;
    let end = start + dur;
    let mut next = start;
    let (mut c_sent, mut l_sent) = (0u64, 0u64);
    while sim.now < end {
        if sim.now >= next {
            sim.c.send(&msg(c_sent, sim.now, 1316), sim.now).unwrap();
            c_sent += 1;
            if bidir {
                sim.l.send(lid, &msg(l_sent, sim.now, 1316), sim.now).unwrap();
                l_sent += 1;
            }
            next += interval;
            sim.pump();
        }
        sim.step(next.min(end));
    }
    // 送り終えたら遅延と再送が落ち着くまで回す
    let drain = sim.now + 2_000_000;
    sim.run_until(drain);
    Run { sim, c_sent, l_sent }
}

fn lossy(loss: f64) -> Link {
    Link { loss, delay: 20_000, jitter: if loss > 0.0 { 10_000 } else { 0 } }
}

/// 両方向の受信遅延をそろえた開始側の設定。
fn ccfg(latency_ms: u16) -> Config {
    Config { latency_ms, ..Config::default() }
}

fn lcfg(latency_ms: u16) -> ListenerConfig {
    ListenerConfig { conn: Config { latency_ms, ..Config::default() }, max_connections: 8 }
}

fn pass(s: &str) -> Option<Passphrase> {
    Some(Passphrase::new(s).unwrap())
}

#[test]
fn clean_link_delivers_everything_in_order() {
    let sim = Sim::new(
        lcfg(120),
        AcceptParams::default(),
        Config { stream_id: Some("#!::r=live/a,m=publish".into()), ..Config::default() },
        lossy(0.0),
        lossy(0.0),
        1,
    );
    let r = transfer(sim, 2_000_000, 1_000, true);
    let up = verify(&r.sim.l_events, r.c_sent, 120_000, 30_000);
    assert_eq!(up.got.len() as u64, up.sent);
    let down = verify(&r.sim.c_events, r.l_sent, 120_000, 30_000);
    assert_eq!(down.got.len() as u64, down.sent);
    match &r.sim.l_events[0] {
        Event::Connected(i) => {
            assert_eq!(i.stream_id.as_deref(), Some("#!::r=live/a,m=publish"));
            assert_eq!(i.encryption, None);
            assert_eq!(i.latency_ms, 120);
        }
        e => panic!("{e:?}"),
    }
    let st = r.sim.c.stats();
    assert!(st.rtt_us > 30_000 && st.rtt_us < 80_000, "rtt {}", st.rtt_us);
    assert_eq!(st.pkts_retransmitted, 0);
}

#[test]
fn lossy_links_recover_within_latency() {
    for (seed, loss) in [(7u64, 0.01), (11, 0.03), (13, 0.05)] {
        let sim = Sim::new(
            lcfg(300),
            AcceptParams { passphrase: pass("lossy-secret-123"), ..Default::default() },
            Config { passphrase: pass("lossy-secret-123"), ..ccfg(300) },
            lossy(loss),
            lossy(loss),
            seed,
        );
        let r = transfer(sim, 3_000_000, 1_000, true);
        let up = verify(&r.sim.l_events, r.c_sent, 300_000, 30_000);
        let down = verify(&r.sim.c_events, r.l_sent, 300_000, 30_000);
        let cs = r.sim.c.stats();
        let ls = r.sim.l.stats(r.sim.lid.unwrap()).unwrap();
        // 300 ms の遅延なら 1〜5% の損失はすべて再送で埋まる
        assert_eq!(up.got.len() as u64, up.sent, "loss {loss}: up missing, stats {ls:?}");
        assert_eq!(down.got.len() as u64, down.sent, "loss {loss}: down missing, stats {cs:?}");
        assert!(cs.pkts_retransmitted > 0 && ls.pkts_retransmitted > 0);
        assert!(ls.pkts_rcv_loss > 0);
        assert_eq!(ls.pkts_rcv_dropped, 0);
        let _ = (up.max_late, up.min_late);
    }
}

#[test]
fn heavy_loss_drops_too_late_but_keeps_order() {
    let sim = Sim::new(
        lcfg(60),
        AcceptParams::default(),
        Config::default(),
        Link { loss: 0.25, delay: 30_000, jitter: 20_000 },
        Link { loss: 0.25, delay: 30_000, jitter: 20_000 },
        99,
    );
    let r = transfer(sim, 2_000_000, 1_000, false);
    let up = verify(&r.sim.l_events, r.c_sent, 60_000, 50_000);
    let ls = r.sim.l.stats(r.sim.lid.unwrap()).unwrap();
    assert!(up.got.len() as u64 > up.sent / 2);
    assert!(ls.pkts_rcv_dropped > 0, "{ls:?}");
    // 渡した数 + 捨てた数 = 送った数
    assert_eq!(up.got.len() as u64 + ls.pkts_rcv_dropped, up.sent, "{ls:?}");
}

#[test]
fn encryption_all_key_lengths_and_gcm_both_directions() {
    for (kl, mode) in [
        (KeyLen::Aes128, CipherMode::AesCtr),
        (KeyLen::Aes192, CipherMode::AesCtr),
        (KeyLen::Aes256, CipherMode::AesCtr),
        (KeyLen::Aes128, CipherMode::AesGcm),
        (KeyLen::Aes256, CipherMode::AesGcm),
    ] {
        let sim = Sim::new(
            lcfg(200),
            AcceptParams { passphrase: pass("correct horse battery"), ..Default::default() },
            Config { passphrase: pass("correct horse battery"), key_len: kl, cipher: mode, ..ccfg(200) },
            lossy(0.02),
            lossy(0.02),
            kl.bytes() as u64,
        );
        let r = transfer(sim, 1_000_000, 2_000, true);
        let up = verify(&r.sim.l_events, r.c_sent, 200_000, 30_000);
        let down = verify(&r.sim.c_events, r.l_sent, 200_000, 30_000);
        assert_eq!(up.got.len() as u64, up.sent);
        assert_eq!(down.got.len() as u64, down.sent);
        assert_eq!(r.sim.c.encryption(), Some((mode, kl)));
        match &r.sim.l_events[0] {
            Event::Connected(i) => assert_eq!(i.encryption, Some((mode, kl))),
            e => panic!("{e:?}"),
        }
        let ls = r.sim.l.stats(r.sim.lid.unwrap()).unwrap();
        assert_eq!(ls.pkts_rcv_undecrypted, 0);
    }
}

#[test]
fn key_rotation_under_loss() {
    let cfg = Config {
        passphrase: pass("rotate-me-please"),
        km_refresh_rate: 300,
        km_preannounce: 50,
        ..ccfg(250)
    };
    let sim = Sim::new(
        ListenerConfig { conn: Config { latency_ms: 250, km_refresh_rate: 400, km_preannounce: 60, ..cfg.clone() }, max_connections: 4 },
        AcceptParams { passphrase: pass("rotate-me-please"), ..Default::default() },
        cfg,
        lossy(0.03),
        lossy(0.03),
        5,
    );
    let r = transfer(sim, 3_000_000, 1_000, true);
    let up = verify(&r.sim.l_events, r.c_sent, 250_000, 30_000);
    let down = verify(&r.sim.c_events, r.l_sent, 250_000, 30_000);
    // 3000 パケットで 300 ごとに切り替え → 約 10 回
    assert_eq!(up.got.len() as u64, up.sent);
    assert_eq!(down.got.len() as u64, down.sent);
    assert_eq!(r.sim.l.stats(r.sim.lid.unwrap()).unwrap().pkts_rcv_undecrypted, 0);
    assert_eq!(r.sim.c.stats().pkts_rcv_undecrypted, 0);
}

fn rejected_with(sim: &Sim) -> Option<RejectReason> {
    sim.c_events.iter().find_map(|e| match e {
        Event::Closed { reason: CloseReason::Rejected(r) } => Some(*r),
        _ => None,
    })
}

#[test]
fn handshake_rejections() {
    let clean = Link { loss: 0.0, delay: 5_000, jitter: 0 };
    // 誤ったパスフレーズ
    let mut s = Sim::new(
        lcfg(120),
        AcceptParams { passphrase: pass("server-secret-1"), ..Default::default() },
        Config { passphrase: pass("client-secret-2"), ..Config::default() },
        clean,
        clean,
        1,
    );
    s.connect();
    assert_eq!(rejected_with(&s), Some(RejectReason::BADSECRET));
    assert!(s.l.is_empty());
    // 片側だけ暗号：平文へ降格せず拒否
    for (lp, cp) in [(pass("server-secret-1"), None), (None, pass("client-secret-2"))] {
        let mut s = Sim::new(
            lcfg(120),
            AcceptParams { passphrase: lp, ..Default::default() },
            Config { passphrase: cp, ..Config::default() },
            clean,
            clean,
            2,
        );
        s.connect();
        assert_eq!(rejected_with(&s), Some(RejectReason::UNSECURE));
        assert!(s.l.is_empty());
    }
    // GCM を要求する listener に CTR で来た
    let mut s = Sim::new(
        lcfg(120),
        AcceptParams { passphrase: pass("server-secret-1"), cipher: Some(CipherMode::AesGcm), ..Default::default() },
        Config { passphrase: pass("server-secret-1"), ..Config::default() },
        clean,
        clean,
        3,
    );
    s.connect();
    assert_eq!(rejected_with(&s), Some(RejectReason::CRYPTO));
    // Stream ID による拒否（SRT_REJX_FORBIDDEN）
    let mut s = Sim::with_accept(
        lcfg(120),
        |req| {
            let ok = req.parsed.is_some_and(|p| p.resource.as_deref() == Some("live/ok") && p.is_publish());
            if ok {
                AcceptDecision::Accept(AcceptParams::default())
            } else {
                AcceptDecision::Reject(RejectReason::X_FORBIDDEN)
            }
        },
        Config { stream_id: Some("#!::r=live/bad,m=publish".into()), ..Config::default() },
        clean,
        clean,
        4,
    );
    s.connect();
    assert_eq!(rejected_with(&s), Some(RejectReason::X_FORBIDDEN));
    // 結論の要求が失われても再送で確立する
    let mut s = Sim::new(
        lcfg(120),
        AcceptParams::default(),
        Config::default(),
        Link { loss: 0.5, delay: 5_000, jitter: 0 },
        Link { loss: 0.5, delay: 5_000, jitter: 0 },
        8,
    );
    s.connect();
    assert!(s.c.is_connected());
    assert_eq!(s.l.len(), 1);
}

#[test]
fn connect_timeout_and_peer_idle() {
    let clean = Link { loss: 0.0, delay: 5_000, jitter: 0 };
    let mut s = Sim::new(lcfg(120), AcceptParams::default(), Config::default(), clean, clean, 1);
    s.blackhole = true;
    s.connect();
    let end = s.now + 4_000_000;
    s.run_until(end);
    assert!(s.c_events.contains(&Event::Closed { reason: CloseReason::ConnectTimeout }));

    let mut s = Sim::new(lcfg(120), AcceptParams::default(), Config::default(), clean, clean, 1);
    s.connect();
    assert!(s.c.is_connected());
    s.blackhole = true;
    let end = s.now + 6_000_000;
    s.run_until(end);
    assert!(s.c_events.contains(&Event::Closed { reason: CloseReason::PeerIdle }));
    assert_eq!(s.l_closed, Some(CloseReason::PeerIdle));
    assert!(s.l.is_empty(), "closed connection must be removed");
}

#[test]
fn graceful_close_delivers_tail() {
    let sim = Sim::new(lcfg(200), AcceptParams::default(), ccfg(200), lossy(0.05), lossy(0.05), 21);
    let mut r = transfer(sim, 500_000, 1_000, false);
    if std::env::var("SIMTRACE").is_ok() {
        eprintln!("L {:?}\nC {:?}", r.sim.l.stats(r.sim.lid.unwrap()), r.sim.c.stats());
    }
    // 送った直後に閉じても、未確認の分は送り切ってから SHUTDOWN を送る
    for i in 0..50 {
        r.sim.c.send(&msg(r.c_sent + i, r.sim.now, 1316), r.sim.now).unwrap();
    }
    r.c_sent += 50;
    r.sim.c.close(r.sim.now);
    let end = r.sim.now + 3_000_000;
    r.sim.run_until(end);
    assert!(r.sim.c_events.contains(&Event::Closed { reason: CloseReason::Local }));
    assert_eq!(r.sim.l_closed, Some(CloseReason::PeerShutdown));
    let got: Vec<u64> = r
        .sim
        .l_events
        .iter()
        .filter_map(|e| if let Event::Data { payload, .. } = e { Some(check_msg(payload).0) } else { None })
        .collect();
    assert_eq!(got, (0..r.c_sent).collect::<Vec<_>>());
}

#[test]
fn sequence_and_timestamp_wrap() {
    let mut sim = Sim::new(lcfg(150), AcceptParams::default(), Config::default(), lossy(0.02), lossy(0.02), 31);
    // 2 秒後にタイムスタンプ（32 ビット µs）が巻き戻り、約 1000 パケットで番号（31 ビット）が巻き戻る
    let now = sim.now;
    sim.c.force_start_isn(now.wrapping_sub((1u64 << 32) - 2_000_000), crate::packet::SEQ_MASK - 1000);
    let r = transfer(sim, 4_000_000, 1_000, false);
    let up = verify(&r.sim.l_events, r.c_sent, 150_000, 30_000);
    assert_eq!(up.got.len() as u64, up.sent);
}

#[test]
fn pacing_with_fixed_bandwidth() {
    let cfg = Config { max_bw: MaxBandwidth::Fixed(500_000), ..Config::default() };
    let sim = Sim::new(lcfg(500), AcceptParams::default(), cfg, lossy(0.0), lossy(0.0), 3);
    // 約 2.6 Mbit/s の入力を 4 Mbit/s で抑えても全部届く
    let r = transfer(sim, 1_000_000, 4_000, false);
    let up = verify(&r.sim.l_events, r.c_sent, 500_000, 30_000);
    assert_eq!(up.got.len() as u64, up.sent);
}

/// 壊れた・切り詰めた・でたらめなデータグラムを両端へ入れてもパニックしない。
#[test]
fn fuzz_malformed_datagrams() {
    let mut sim = Sim::new(
        lcfg(120),
        AcceptParams { passphrase: pass("fuzz-secret-xyz"), ..Default::default() },
        Config { passphrase: pass("fuzz-secret-xyz"), stream_id: Some("abc".into()), ..Config::default() },
        lossy(0.0),
        lossy(0.0),
        77,
    );
    // 正規のパケットを集める
    let mut samples: Vec<Vec<u8>> = Vec::new();
    sim.pump();
    for Reverse((_, _, _, b)) in sim.flight.iter() {
        samples.push(b.clone());
    }
    sim.connect();
    let lid = sim.lid.unwrap();
    for i in 0..200 {
        sim.c.send(&msg(i, sim.now, 1316), sim.now).unwrap();
        sim.l.send(lid, &msg(i, sim.now, 188), sim.now).unwrap();
        while let Some((_, b)) = sim.c.poll_transmit() {
            samples.push(b.to_vec());
        }
        while let Some((_, b)) = sim.l.poll_transmit() {
            samples.push(b.to_vec());
        }
        let t = sim.now + 1_000;
        sim.c.handle_timeout(t);
        sim.l.handle_timeout(t);
        sim.now = t;
    }
    let mut rng = Rng(12345);
    let la = sim.la;
    let ca = sim.ca;
    for round in 0..20_000u64 {
        let mut b = samples[rng.below(samples.len() as u64) as usize].clone();
        match round % 5 {
            0 => b.truncate(rng.below(b.len() as u64 + 1) as usize),
            1 => {
                for _ in 0..1 + rng.below(4) {
                    let i = rng.below(b.len() as u64) as usize;
                    b[i] ^= 1 << rng.below(8);
                }
            }
            2 => {
                let n = rng.below(200) as usize;
                b = (0..n).map(|_| rng.next() as u8).collect();
            }
            3 => {
                // 宛先をつなぎ直し、本体をでたらめにする
                if b.len() >= 16 {
                    let id = if rng.below(2) == 0 { lid.0 } else { sim.c.local_id() };
                    b[12..16].copy_from_slice(&id.to_be_bytes());
                    for x in b.iter_mut().skip(16) {
                        *x = rng.next() as u8;
                    }
                }
            }
            _ => {
                let i = rng.below(b.len() as u64) as usize;
                b.splice(i..i, (0..rng.below(64)).map(|_| rng.next() as u8));
            }
        }
        sim.now += rng.below(500);
        sim.l.handle_datagram(sim.now, ca, &b);
        sim.c.handle_datagram(sim.now, la, &b);
        if round % 64 == 0 {
            sim.l.handle_timeout(sim.now);
            sim.c.handle_timeout(sim.now);
            while sim.l.poll_transmit().is_some() {}
            while sim.c.poll_transmit().is_some() {}
            while sim.l.poll_event().is_some() {}
            while sim.c.poll_event().is_some() {}
        }
    }
    // 異常な入力のあとも出力は有界
    let mut n = 0;
    while sim.c.poll_transmit().is_some() {
        n += 1;
        assert!(n < 100_000);
    }
}
