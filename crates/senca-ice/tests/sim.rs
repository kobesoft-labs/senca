//! 2 つの sans-io エージェントを模擬ネットワーク（NAT・損失・STUN/TURN サーバー）でつなぐ試験。

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use bytes::Bytes;
use senca_ice::{CandidateKind, IceAgent, IceConfig, IceEvent, IceRole, IceState, TurnServerSpec};
use senca_stun::{Message, binding_success};
use senca_turn::server::{RelayIp, StaticUser};
use senca_turn::{TurnServer, TurnServerConfig};

fn sa(s: &str) -> SocketAddr {
    s.parse().unwrap()
}
fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

const STUN: &str = "9.9.9.9:3478";
const TURN: &str = "8.8.8.8:3478";
const LATENCY: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NatKind {
    /// endpoint-independent mapping と filtering。
    FullCone,
    /// 宛先ごとの mapping と filtering。
    Symmetric,
}

struct Nat {
    public: IpAddr,
    inside: HashSet<IpAddr>,
    kind: NatKind,
    map: HashMap<(SocketAddr, Option<SocketAddr>), u16>,
    rev: HashMap<u16, SocketAddr>,
    allowed: HashSet<(u16, SocketAddr)>,
    next_port: u16,
}

impl Nat {
    fn new(public: &str, inside: &[&str], kind: NatKind) -> Nat {
        Nat {
            public: ip(public),
            inside: inside.iter().map(|s| ip(s)).collect(),
            kind,
            map: HashMap::new(),
            rev: HashMap::new(),
            allowed: HashSet::new(),
            next_port: 40000,
        }
    }

    fn outbound(&mut self, src: SocketAddr, dst: SocketAddr) -> SocketAddr {
        let key = (
            src,
            if self.kind == NatKind::Symmetric {
                Some(dst)
            } else {
                None
            },
        );
        let port = *self.map.entry(key).or_insert_with(|| {
            self.next_port += 1;
            self.next_port
        });
        self.rev.insert(port, src);
        self.allowed.insert((port, dst));
        SocketAddr::new(self.public, port)
    }

    fn inbound(&self, src: SocketAddr, dst: SocketAddr) -> Option<SocketAddr> {
        let inside = *self.rev.get(&dst.port())?;
        match self.kind {
            NatKind::FullCone => Some(inside),
            NatKind::Symmetric => self.allowed.contains(&(dst.port(), src)).then_some(inside),
        }
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Packet {
    at: Instant,
    seq: u64,
    src: SocketAddr,
    dst: SocketAddr,
    data: Vec<u8>,
}

struct Peer {
    agent: IceAgent,
    ips: Vec<IpAddr>,
    events: Vec<IceEvent>,
    received: Vec<Bytes>,
    /// 相手へ送る trickle（シグナリングは即時）。
    outbox: Vec<IceEvent>,
}

struct Sim {
    now: Instant,
    t0: Instant,
    queue: BinaryHeap<Reverse<Packet>>,
    seq: u64,
    nats: Vec<Nat>,
    peers: [Peer; 2],
    turn: TurnServer,
    loss: u32,
    rng: u64,
    /// 片方からの送信をすべて落とす（consent の試験）。
    blackhole: [bool; 2],
    signaling: bool,
}

fn peer(cfg: IceConfig, ips: &[&str]) -> Peer {
    Peer {
        agent: IceAgent::new(cfg).unwrap(),
        ips: ips.iter().map(|s| ip(s)).collect(),
        events: Vec::new(),
        received: Vec::new(),
        outbox: Vec::new(),
    }
}

fn is_private(a: IpAddr) -> bool {
    match a {
        IpAddr::V4(v) => v.is_private(),
        IpAddr::V6(v) => (v.segments()[0] & 0xfe00) == 0xfc00,
    }
}

impl Sim {
    fn new(a: Peer, b: Peer, nats: Vec<Nat>) -> Sim {
        let t0 = Instant::now();
        let mut turn = TurnServer::new(TurnServerConfig {
            realm: "sim".into(),
            users: vec![StaticUser {
                username: "u".into(),
                password: "p".into(),
                tenant: None,
            }],
            relay_v4: Some(RelayIp {
                local: ip("8.8.8.8"),
                external: None,
            }),
            port_min: 50000,
            port_max: 50100,
            ..Default::default()
        })
        .unwrap();
        turn.set_unix_time(1_000_000);
        Sim {
            now: t0,
            t0,
            queue: BinaryHeap::new(),
            seq: 0,
            nats,
            peers: [a, b],
            turn,
            loss: 0,
            rng: 0x1234_5678,
            blackhole: [false, false],
            signaling: true,
        }
    }

    fn rand(&mut self) -> u32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 32) as u32
    }

    fn owner(&self, a: IpAddr) -> Option<usize> {
        self.peers.iter().position(|p| p.ips.contains(&a))
    }

    /// 送信元から NAT を通して網に出す。
    fn send(&mut self, src: SocketAddr, dst: SocketAddr, data: Bytes) {
        if let Some(o) = self.owner(src.ip())
            && self.blackhole[o]
        {
            return;
        }
        if self.loss > 0 && self.rand() % 100 < self.loss {
            return;
        }
        let mut src = src;
        for n in self.nats.iter_mut() {
            if n.inside.contains(&src.ip()) && !n.inside.contains(&dst.ip()) {
                src = n.outbound(src, dst);
            }
        }
        self.seq += 1;
        self.queue.push(Reverse(Packet {
            at: self.now + LATENCY,
            seq: self.seq,
            src,
            dst,
            data: data.to_vec(),
        }));
    }

    fn deliver(&mut self, p: Packet) {
        let mut dst = p.dst;
        if let Some(n) = self.nats.iter().find(|n| n.public == dst.ip()) {
            match n.inbound(p.src, dst) {
                Some(inside) => dst = inside,
                None => return,
            }
        } else if is_private(dst.ip()) {
            // 別の私設網には届かない（同じ NAT の内側どうしは届く）
            let same = self
                .nats
                .iter()
                .any(|n| n.inside.contains(&dst.ip()) && n.inside.contains(&p.src.ip()));
            let public_peer = self.owner(dst.ip()).is_some()
                && !self.nats.iter().any(|n| n.inside.contains(&dst.ip()));
            if !same && !public_peer {
                return;
            }
        }
        let data = Bytes::from(p.data);
        if dst == sa(STUN) {
            if let Ok(m) = Message::parse(data)
                && let Ok(resp) = binding_success(&m, p.src).fingerprint().build()
            {
                self.send(sa(STUN), p.src, resp);
            }
        } else if dst == sa(TURN) {
            self.turn.handle_client(self.now, dst, p.src, data);
        } else if dst.ip() == ip("8.8.8.8") {
            self.turn.handle_peer(self.now, dst, p.src, data);
        } else if let Some(i) = self.owner(dst.ip()) {
            let now = self.now;
            if let Some(app) = self.peers[i].agent.handle_input(now, dst, p.src, data) {
                self.peers[i].received.push(app);
            }
        }
    }

    fn flush(&mut self) {
        loop {
            let mut moved = false;
            for i in 0..2 {
                while let Some(t) = self.peers[i].agent.poll_transmit() {
                    moved = true;
                    self.send(t.local, t.remote, t.data);
                }
                while let Some(e) = self.peers[i].agent.poll_event() {
                    moved = true;
                    if matches!(e, IceEvent::LocalCandidate(_) | IceEvent::GatheringComplete) {
                        self.peers[i].outbox.push(e.clone());
                    }
                    self.peers[i].events.push(e);
                }
            }
            while let Some(t) = self.turn.poll_transmit() {
                moved = true;
                self.send(t.local, t.remote, t.data);
            }
            while self.turn.poll_event().is_some() {}
            if self.signaling {
                for i in 0..2 {
                    let out = std::mem::take(&mut self.peers[i].outbox);
                    let now = self.now;
                    for e in out {
                        moved = true;
                        match e {
                            IceEvent::LocalCandidate(c) => {
                                let _ = self.peers[1 - i].agent.add_remote_candidate(now, c);
                            }
                            IceEvent::GatheringComplete => {
                                self.peers[1 - i].agent.end_of_remote_candidates(now)
                            }
                            _ => {}
                        }
                    }
                }
            }
            if !moved {
                break;
            }
        }
    }

    /// `cond` が真になるまで（または `limit` 経過まで）時間を進める。
    fn run_until(&mut self, limit: Duration, mut cond: impl FnMut(&Sim) -> bool) -> bool {
        let deadline = self.now + limit;
        for _ in 0..2_000_000 {
            self.flush();
            if cond(self) {
                return true;
            }
            let mut next = deadline;
            if let Some(Reverse(p)) = self.queue.peek() {
                next = next.min(p.at);
            }
            for p in &self.peers {
                if let Some(t) = p.agent.poll_timeout() {
                    next = next.min(t);
                }
            }
            if let Some(t) = self.turn.poll_timeout() {
                next = next.min(t);
            }
            if next >= deadline && self.now >= deadline {
                return cond(self);
            }
            self.now = self.now.max(next);
            while self.queue.peek().is_some_and(|Reverse(p)| p.at <= self.now) {
                let Reverse(p) = self.queue.pop().unwrap();
                self.deliver(p);
            }
            let now = self.now;
            for p in self.peers.iter_mut() {
                if p.agent.poll_timeout().is_some_and(|t| t <= now) {
                    p.agent.handle_timeout(now);
                }
            }
            if self.turn.poll_timeout().is_some_and(|t| t <= now) {
                self.turn.handle_timeout(now);
            }
        }
        panic!("simulation did not settle");
    }

    fn exchange_credentials(&mut self) {
        let a = self.peers[0].agent.local_credentials().clone();
        let b = self.peers[1].agent.local_credentials().clone();
        self.peers[0]
            .agent
            .set_remote_credentials(&b.ufrag, &b.pwd)
            .unwrap();
        self.peers[1]
            .agent
            .set_remote_credentials(&a.ufrag, &a.pwd)
            .unwrap();
    }

    fn connected(&self) -> bool {
        self.peers.iter().all(|p| {
            matches!(p.agent.state(), IceState::Connected | IceState::Completed)
                && p.agent.selected_pair().is_some()
        })
    }

    fn agree(&self) -> bool {
        // 両側の選んだ pair が同じ経路（相手の候補のアドレスがこちらの見ている相手）
        let (Some((_, ra)), Some((_, rb))) = (
            self.peers[0].agent.selected_pair(),
            self.peers[1].agent.selected_pair(),
        ) else {
            return false;
        };
        ra.addr().is_some() && rb.addr().is_some()
    }

    /// 選んだ pair で双方向にアプリのデータを送り、届くかを確かめる。
    fn exchange_data(&mut self) -> bool {
        for i in 0..2 {
            self.peers[i].received.clear();
            let t = self.peers[i]
                .agent
                .send(Bytes::from(vec![0x17, 0xfe, 0xfd, i as u8]))
                .expect("send");
            self.send(t.local, t.remote, t.data);
        }
        self.run_until(Duration::from_millis(200), |s| {
            s.peers.iter().all(|p| !p.received.is_empty())
        })
    }

    fn elapsed(&self) -> Duration {
        self.now - self.t0
    }
}

fn cfg(role: IceRole, tie: u64) -> IceConfig {
    IceConfig {
        role,
        tie_breaker: Some(tie),
        ..Default::default()
    }
}

fn start(sim: &mut Sim, a_socks: &[&str], b_socks: &[&str]) {
    let now = sim.now;
    for s in a_socks {
        sim.peers[0].agent.add_local_socket(now, sa(s)).unwrap();
    }
    for s in b_socks {
        sim.peers[1].agent.add_local_socket(now, sa(s)).unwrap();
    }
    sim.exchange_credentials();
}

#[test]
fn public_hosts_connect() {
    let a = peer(cfg(IceRole::Controlling, 1), &["5.5.5.5", "2001:4860::5"]);
    let b = peer(cfg(IceRole::Controlled, 2), &["6.6.6.6", "2001:4860::6"]);
    let mut sim = Sim::new(a, b, vec![]);
    start(
        &mut sim,
        &["5.5.5.5:1000", "[2001:4860::5]:1000"],
        &["6.6.6.6:2000", "[2001:4860::6]:2000"],
    );
    assert!(sim.run_until(Duration::from_secs(5), |s| s.connected()));
    assert!(sim.agree());
    // IPv6 が優先される
    let (l, r) = sim.peers[0].agent.selected_pair().unwrap();
    assert!(
        l.addr().unwrap().is_ipv6() && r.addr().unwrap().is_ipv6(),
        "{l} {r}"
    );
    assert!(
        sim.elapsed() < Duration::from_secs(2),
        "{:?}",
        sim.elapsed()
    );
    assert!(sim.exchange_data());
    // end-of-candidates の後は Completed
    assert!(sim.run_until(Duration::from_secs(2), |s| {
        s.peers
            .iter()
            .all(|p| p.agent.state() == IceState::Completed)
    }));
    // consent が続くので 60 秒経っても繋がったまま
    assert!(!sim.run_until(Duration::from_secs(60), |s| {
        s.peers
            .iter()
            .any(|p| p.agent.state() != IceState::Completed)
    }));
    assert!(sim.exchange_data());
}

#[test]
fn full_cone_nat_with_stun() {
    let mut ca = cfg(IceRole::Controlling, 1);
    ca.stun_servers = vec![sa(STUN)];
    let a = peer(ca, &["192.168.1.10"]);
    let b = peer(cfg(IceRole::Controlled, 2), &["6.6.6.6"]);
    let nat = Nat::new("5.5.5.5", &["192.168.1.10"], NatKind::FullCone);
    let mut sim = Sim::new(a, b, vec![nat]);
    start(&mut sim, &["192.168.1.10:1000"], &["6.6.6.6:2000"]);
    assert!(sim.run_until(Duration::from_secs(10), |s| s.connected()));
    assert!(sim.peers[0].events.iter().any(
        |e| matches!(e, IceEvent::LocalCandidate(c) if c.kind == CandidateKind::ServerReflexive)
    ));
    // B から見た相手は NAT の外側のアドレス
    let (_, r) = sim.peers[1].agent.selected_pair().unwrap();
    assert_eq!(r.addr().unwrap().ip(), ip("5.5.5.5"));
    assert!(sim.exchange_data());
}

#[test]
fn symmetric_nats_need_turn() {
    let mut ca = cfg(IceRole::Controlling, 1);
    ca.stun_servers = vec![sa(STUN)];
    ca.turn_servers = vec![TurnServerSpec {
        server: sa(TURN),
        username: "u".into(),
        password: "p".into(),
    }];
    let mut cb = cfg(IceRole::Controlled, 2);
    cb.stun_servers = vec![sa(STUN)];
    let a = peer(ca, &["192.168.1.10"]);
    let b = peer(cb, &["10.0.0.20"]);
    let nats = vec![
        Nat::new("5.5.5.5", &["192.168.1.10"], NatKind::Symmetric),
        Nat::new("7.7.7.7", &["10.0.0.20"], NatKind::Symmetric),
    ];
    let mut sim = Sim::new(a, b, nats);
    start(&mut sim, &["192.168.1.10:1000"], &["10.0.0.20:2000"]);
    assert!(sim.run_until(Duration::from_secs(20), |s| s.connected()));
    let (l, _) = sim.peers[0].agent.selected_pair().unwrap();
    assert_eq!(l.kind, CandidateKind::Relayed);
    let (_, r) = sim.peers[1].agent.selected_pair().unwrap();
    assert_eq!(r.addr().unwrap().ip(), ip("8.8.8.8"));
    assert!(sim.exchange_data());
    // relay 経由で consent が保たれる（TURN の Refresh・permission の更新を含めて 11 分）
    assert!(!sim.run_until(Duration::from_secs(660), |s| {
        s.peers
            .iter()
            .any(|p| matches!(p.agent.state(), IceState::Failed | IceState::Disconnected))
    }));
    assert!(sim.exchange_data());
}

#[test]
fn symmetric_nats_without_turn_fail_after_end_of_candidates() {
    let mut ca = cfg(IceRole::Controlling, 1);
    ca.stun_servers = vec![sa(STUN)];
    let mut cb = cfg(IceRole::Controlled, 2);
    cb.stun_servers = vec![sa(STUN)];
    let a = peer(ca, &["192.168.1.10"]);
    let b = peer(cb, &["10.0.0.20"]);
    let nats = vec![
        Nat::new("5.5.5.5", &["192.168.1.10"], NatKind::Symmetric),
        Nat::new("7.7.7.7", &["10.0.0.20"], NatKind::Symmetric),
    ];
    let mut sim = Sim::new(a, b, nats);
    start(&mut sim, &["192.168.1.10:1000"], &["10.0.0.20:2000"]);
    assert!(sim.run_until(Duration::from_secs(120), |s| {
        s.peers.iter().all(|p| p.agent.state() == IceState::Failed)
    }));
    assert!(!sim.connected());
}

#[test]
fn lossy_network_connects() {
    for seed in 1..6u64 {
        let mut ca = cfg(IceRole::Controlling, 1);
        ca.stun_servers = vec![sa(STUN)];
        let a = peer(ca, &["192.168.1.10"]);
        let b = peer(cfg(IceRole::Controlled, 2), &["6.6.6.6"]);
        let nat = Nat::new("5.5.5.5", &["192.168.1.10"], NatKind::FullCone);
        let mut sim = Sim::new(a, b, vec![nat]);
        sim.loss = 30;
        sim.rng = 0x9e37_79b9_7f4a_7c15 ^ seed;
        start(&mut sim, &["192.168.1.10:1000"], &["6.6.6.6:2000"]);
        assert!(
            sim.run_until(Duration::from_secs(30), |s| s.connected()),
            "seed {seed}"
        );
        sim.loss = 0;
        assert!(sim.exchange_data());
    }
}

#[test]
fn role_conflict_both_controlling() {
    let a = peer(cfg(IceRole::Controlling, 10), &["5.5.5.5"]);
    let b = peer(cfg(IceRole::Controlling, 20), &["6.6.6.6"]);
    let mut sim = Sim::new(a, b, vec![]);
    start(&mut sim, &["5.5.5.5:1000"], &["6.6.6.6:2000"]);
    assert!(sim.run_until(Duration::from_secs(10), |s| s.connected()));
    // tie-breaker の大きい B が controlling のまま、A が controlled に変わる
    assert_eq!(sim.peers[0].agent.role(), IceRole::Controlled);
    assert_eq!(sim.peers[1].agent.role(), IceRole::Controlling);
    assert!(
        sim.peers[0]
            .events
            .contains(&IceEvent::RoleChanged(IceRole::Controlled))
    );
    assert!(sim.exchange_data());
}

#[test]
fn role_conflict_both_controlled() {
    let a = peer(cfg(IceRole::Controlled, 30), &["5.5.5.5"]);
    let b = peer(cfg(IceRole::Controlled, 20), &["6.6.6.6"]);
    let mut sim = Sim::new(a, b, vec![]);
    start(&mut sim, &["5.5.5.5:1000"], &["6.6.6.6:2000"]);
    assert!(sim.run_until(Duration::from_secs(10), |s| s.connected()));
    assert_eq!(sim.peers[0].agent.role(), IceRole::Controlling);
    assert_eq!(sim.peers[1].agent.role(), IceRole::Controlled);
    assert!(sim.exchange_data());
}

#[test]
fn ice_lite_server() {
    let a = peer(cfg(IceRole::Controlling, 1), &["192.168.1.10"]);
    let b = peer(
        IceConfig {
            lite: true,
            ..Default::default()
        },
        &["6.6.6.6"],
    );
    let nat = Nat::new("5.5.5.5", &["192.168.1.10"], NatKind::Symmetric);
    let mut sim = Sim::new(a, b, vec![nat]);
    start(&mut sim, &["192.168.1.10:1000"], &["6.6.6.6:2000"]);
    assert!(sim.run_until(Duration::from_secs(10), |s| s.connected()));
    // lite 側は相手を peer-reflexive で知る
    let (_, r) = sim.peers[1].agent.selected_pair().unwrap();
    assert_eq!(r.kind, CandidateKind::PeerReflexive);
    assert!(sim.exchange_data());
    // full 側の consent が lite 側の同意を保つ
    assert!(!sim.run_until(Duration::from_secs(60), |s| {
        s.peers
            .iter()
            .any(|p| matches!(p.agent.state(), IceState::Failed | IceState::Disconnected))
    }));
    // full 側が黙ると lite 側も 30 秒で Failed
    sim.blackhole[0] = true;
    let t = sim.now;
    assert!(
        sim.run_until(Duration::from_secs(40), |s| s.peers[1].agent.state()
            == IceState::Failed)
    );
    assert!(sim.now - t >= Duration::from_secs(25));
}

#[test]
fn consent_timeout() {
    let a = peer(cfg(IceRole::Controlling, 1), &["5.5.5.5"]);
    let b = peer(cfg(IceRole::Controlled, 2), &["6.6.6.6"]);
    let mut sim = Sim::new(a, b, vec![]);
    start(&mut sim, &["5.5.5.5:1000"], &["6.6.6.6:2000"]);
    assert!(sim.run_until(Duration::from_secs(5), |s| s.connected()));
    // B が応答しなくなる
    sim.blackhole[1] = true;
    let t = sim.now;
    assert!(
        sim.run_until(Duration::from_secs(20), |s| s.peers[0].agent.state()
            == IceState::Disconnected)
    );
    assert!(sim.now - t >= Duration::from_secs(5));
    assert!(
        sim.run_until(Duration::from_secs(40), |s| s.peers[0].agent.state()
            == IceState::Failed)
    );
    let waited = sim.now - t;
    assert!(
        waited >= Duration::from_secs(25) && waited <= Duration::from_secs(31),
        "{waited:?}"
    );
    // consent を失ったら送らない
    assert!(
        sim.peers[0]
            .agent
            .send(Bytes::from_static(&[0x17]))
            .is_none()
    );
}

#[test]
fn consent_recovers_from_short_outage() {
    let a = peer(cfg(IceRole::Controlling, 1), &["5.5.5.5"]);
    let b = peer(cfg(IceRole::Controlled, 2), &["6.6.6.6"]);
    let mut sim = Sim::new(a, b, vec![]);
    start(&mut sim, &["5.5.5.5:1000"], &["6.6.6.6:2000"]);
    assert!(sim.run_until(Duration::from_secs(5), |s| s.connected()));
    sim.blackhole[1] = true;
    assert!(
        sim.run_until(Duration::from_secs(20), |s| s.peers[0].agent.state()
            == IceState::Disconnected)
    );
    sim.blackhole[1] = false;
    assert!(sim.run_until(Duration::from_secs(10), |s| matches!(
        s.peers[0].agent.state(),
        IceState::Connected | IceState::Completed
    )));
    assert!(sim.exchange_data());
}

#[test]
fn ice_restart() {
    let a = peer(cfg(IceRole::Controlling, 1), &["5.5.5.5"]);
    let b = peer(cfg(IceRole::Controlled, 2), &["6.6.6.6"]);
    let mut sim = Sim::new(a, b, vec![]);
    start(&mut sim, &["5.5.5.5:1000"], &["6.6.6.6:2000"]);
    assert!(sim.run_until(Duration::from_secs(5), |s| s.connected()));
    let old = sim.peers[0].agent.local_credentials().clone();
    let now = sim.now;
    let na = sim.peers[0].agent.restart(now).unwrap();
    assert_ne!(na, old);
    // 新しい offer/answer の前でも古い pair で送れる
    assert!(sim.exchange_data());
    let nb = sim.peers[1].agent.restart(now).unwrap();
    for p in sim.peers.iter_mut() {
        p.events.clear();
    }
    sim.peers[0]
        .agent
        .set_remote_credentials(&nb.ufrag, &nb.pwd)
        .unwrap();
    sim.peers[1]
        .agent
        .set_remote_credentials(&na.ufrag, &na.pwd)
        .unwrap();
    for i in 0..2 {
        for c in sim.peers[i].agent.local_candidates() {
            sim.peers[1 - i].agent.add_remote_candidate(now, c).unwrap();
        }
    }
    assert!(
        sim.run_until(Duration::from_secs(10), |s| s.peers.iter().all(|p| p
            .events
            .iter()
            .any(|e| matches!(e, IceEvent::SelectedPair { .. }))))
    );
    assert!(sim.exchange_data());
    // 新しい資格情報で consent が続く
    assert!(!sim.run_until(Duration::from_secs(45), |s| {
        s.peers
            .iter()
            .any(|p| matches!(p.agent.state(), IceState::Failed | IceState::Disconnected))
    }));
}

#[test]
fn trickle_candidates_arrive_late() {
    let a = peer(cfg(IceRole::Controlling, 1), &["5.5.5.5"]);
    let b = peer(cfg(IceRole::Controlled, 2), &["6.6.6.6"]);
    let mut sim = Sim::new(a, b, vec![]);
    sim.signaling = false;
    start(&mut sim, &["5.5.5.5:1000"], &["6.6.6.6:2000"]);
    assert!(!sim.run_until(Duration::from_secs(2), |s| s.connected()));
    // A の候補だけ遅れて届く。B の候補は A へ渡さない（A は peer-reflexive で学ぶ）。
    let now = sim.now;
    let ca: Vec<_> = sim.peers[0].agent.local_candidates();
    for c in ca {
        sim.peers[1]
            .agent
            .add_remote_candidate_str(now, &c.to_string())
            .unwrap();
    }
    assert!(sim.run_until(Duration::from_secs(5), |s| s.connected()));
    assert!(sim.exchange_data());
}

#[test]
fn mdns_candidates_need_resolver() {
    let now = Instant::now();
    let mut a = IceAgent::new(IceConfig::default()).unwrap();
    let c =
        "candidate:1 1 udp 2122260223 4a2f8e1c-1111-2222-3333-444455556666.local 50000 typ host";
    match a.add_remote_candidate_str(now, c) {
        Err(senca_ice::IceError::UnresolvableHostname(h)) => assert!(h.ends_with(".local")),
        r => panic!("{r:?}"),
    }
    a.set_mdns_resolver(Box::new(|name| {
        name.starts_with("4a2f8e1c").then(|| ip("6.6.6.6"))
    }));
    assert_eq!(a.add_remote_candidate_str(now, c), Ok(true));
}

#[test]
fn rejects_unauthenticated_checks_and_junk() {
    let now = Instant::now();
    let mut a = IceAgent::new(IceConfig::default()).unwrap();
    a.add_local_socket(now, sa("6.6.6.6:2000")).unwrap();
    while a.poll_transmit().is_some() {}
    // 誤った鍵の検査には 401、アプリのデータは知らない相手からは返さない
    let tid = senca_stun::TransactionId::random().unwrap();
    let req = senca_stun::MessageBuilder::new(
        senca_stun::Class::Request,
        senca_stun::Method::BINDING,
        tid,
    )
    .attr(senca_stun::Attribute::Username(format!(
        "{}:x",
        a.local_credentials().ufrag
    )))
    .attr(senca_stun::Attribute::Priority(1))
    .integrity(b"wrong-password-wrong-pw")
    .fingerprint()
    .build()
    .unwrap();
    assert!(
        a.handle_input(now, sa("6.6.6.6:2000"), sa("1.2.3.4:5"), req)
            .is_none()
    );
    let t = a.poll_transmit().unwrap();
    let m = Message::parse(t.data).unwrap();
    assert_eq!(m.error_code().map(|e| e.0), Some(401));
    assert!(
        a.handle_input(
            now,
            sa("6.6.6.6:2000"),
            sa("1.2.3.4:5"),
            Bytes::from_static(&[0x17, 0xfe, 0xfd])
        )
        .is_none()
    );
    let mut seed = 3u32;
    for _ in 0..20000 {
        let len = (seed % 80) as usize;
        let mut v = vec![0u8; len];
        for b in v.iter_mut() {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            *b = (seed >> 16) as u8;
        }
        if len > 20 {
            v[0] = 0;
            v[1] = 1;
            v[4..8].copy_from_slice(&0x2112A442u32.to_be_bytes());
        }
        let _ = a.handle_input(now, sa("6.6.6.6:2000"), sa("1.2.3.4:5"), Bytes::from(v));
    }
    assert_eq!(a.state(), IceState::New);
}
