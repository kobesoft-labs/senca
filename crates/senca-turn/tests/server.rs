//! TURN サーバーの試験。要求は senca-stun の MessageBuilder で組み立てる。

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use senca_stun::attr::attr_type;
use senca_stun::{
    Attribute, Class, Message, MessageBuilder, Method, PasswordAlgorithm, TransactionId,
    long_term_key,
};
use senca_turn::server::{Quotas, RelayIp, RestSecret, StaticUser};
use senca_turn::{
    ChannelData, PeerPolicy, Transmit, TurnServer, TurnServerConfig, TurnServerEvent,
    rest_api_credentials,
};

const REALM: &str = "senca.test";

fn sa(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn listen() -> SocketAddr {
    sa("198.51.100.1:3478")
}

fn config() -> TurnServerConfig {
    TurnServerConfig {
        realm: REALM.into(),
        users: vec![
            StaticUser {
                username: "alice".into(),
                password: "pw".into(),
                tenant: Some("t1".into()),
            },
            StaticUser {
                username: "bob".into(),
                password: "pw2".into(),
                tenant: Some("t1".into()),
            },
        ],
        rest_secrets: vec![RestSecret {
            secret: b"s3cret".to_vec(),
            tenant: Some("t2".into()),
        }],
        relay_v4: Some(RelayIp {
            local: "10.0.0.1".parse().unwrap(),
            external: Some("198.51.100.1".parse().unwrap()),
        }),
        relay_v6: None,
        port_min: 50000,
        port_max: 50100,
        ..Default::default()
    }
}

struct Cli {
    addr: SocketAddr,
    user: String,
    key: Vec<u8>,
    nonce: String,
}

impl Cli {
    fn new(addr: &str, user: &str, pass: &str) -> Cli {
        Cli {
            addr: sa(addr),
            user: user.into(),
            key: long_term_key(user, REALM, pass, PasswordAlgorithm::Md5),
            nonce: String::new(),
        }
    }

    fn request(&self, method: Method, attrs: Vec<Attribute>) -> (TransactionId, Bytes) {
        let tid = TransactionId::random().unwrap();
        let b = MessageBuilder::new(Class::Request, method, tid)
            .attrs(attrs)
            .attr(Attribute::Username(self.user.clone()))
            .attr(Attribute::Realm(REALM.into()))
            .attr(Attribute::Nonce(self.nonce.clone()))
            .integrity(&self.key)
            .fingerprint()
            .build()
            .unwrap();
        (tid, b)
    }

    /// 送って、応答を 1 つ受ける（無ければ None）。
    fn call(
        &self,
        s: &mut TurnServer,
        now: Instant,
        method: Method,
        attrs: Vec<Attribute>,
    ) -> Option<Message> {
        let (_, b) = self.request(method, attrs);
        s.handle_client(now, listen(), self.addr, b);
        recv(s, self.addr)
    }

    /// 401 を受けて nonce を覚え、Allocate を通す。
    fn allocate(&mut self, s: &mut TurnServer, now: Instant) -> Message {
        self.challenge(s, now);
        self.call(
            s,
            now,
            Method::ALLOCATE,
            vec![Attribute::RequestedTransport(17)],
        )
        .unwrap()
    }

    fn challenge(&mut self, s: &mut TurnServer, now: Instant) {
        let b = MessageBuilder::new(
            Class::Request,
            Method::ALLOCATE,
            TransactionId::random().unwrap(),
        )
        .attr(Attribute::RequestedTransport(17))
        .build()
        .unwrap();
        s.handle_client(now, listen(), self.addr, b);
        let m = recv(s, self.addr).unwrap();
        assert_eq!(m.error_code().unwrap().0, 401);
        assert_eq!(m.realm(), Some(REALM));
        assert!(m.software().is_none());
        self.nonce = m.nonce().unwrap().to_string();
    }
}

fn drain(s: &mut TurnServer) -> Vec<Transmit> {
    std::iter::from_fn(|| s.poll_transmit()).collect()
}

fn recv(s: &mut TurnServer, to: SocketAddr) -> Option<Message> {
    let t = drain(s);
    let t: Vec<_> = t.into_iter().filter(|t| t.remote == to).collect();
    assert!(t.len() <= 1, "{t:?}");
    t.into_iter().next().map(|t| {
        assert_eq!(t.local, listen());
        Message::parse(t.data).unwrap()
    })
}

fn ok(m: &Message, key: &[u8]) {
    assert_eq!(m.class, Class::Success, "{m:?}");
    m.verify_integrity(key).unwrap();
}

fn err(m: &Message) -> u16 {
    assert_eq!(m.class, Class::Error, "{m:?}");
    m.error_code().unwrap().0
}

#[test]
fn allocation_lifecycle() {
    let mut s = TurnServer::new(config()).unwrap();
    let t0 = Instant::now();
    let mut c = Cli::new("203.0.113.5:4000", "alice", "pw");
    let m = c.allocate(&mut s, t0);
    ok(&m, &c.key);
    let relay = m.xor_relayed_address().unwrap();
    assert_eq!(relay.ip().to_string(), "198.51.100.1");
    assert!((50000..=50100).contains(&relay.port()));
    assert_eq!(m.mapped_address(), Some(c.addr));
    assert_eq!(m.lifetime(), Some(600));
    assert!(m.has_fingerprint());
    let ev = s.poll_event().unwrap();
    let TurnServerEvent::AllocationCreated {
        relay: local_relay,
        client,
        username,
        ..
    } = ev
    else {
        panic!()
    };
    assert_eq!(
        local_relay,
        SocketAddr::new("10.0.0.1".parse().unwrap(), relay.port())
    );
    assert_eq!(client, c.addr);
    assert_eq!(username, "alice");
    assert_eq!(s.allocation_count(), 1);

    // 別の Allocate は 437
    assert_eq!(
        err(&c
            .call(
                &mut s,
                t0,
                Method::ALLOCATE,
                vec![Attribute::RequestedTransport(17)]
            )
            .unwrap()),
        437
    );

    // Refresh（上限 3600 に丸める）
    let m = c
        .call(
            &mut s,
            t0,
            Method::REFRESH,
            vec![Attribute::Lifetime(99999)],
        )
        .unwrap();
    ok(&m, &c.key);
    assert_eq!(m.lifetime(), Some(3600));
    // 小さすぎる値は既定値
    let m = c
        .call(&mut s, t0, Method::REFRESH, vec![Attribute::Lifetime(10)])
        .unwrap();
    assert_eq!(m.lifetime(), Some(600));

    // 期限で消える
    assert_eq!(s.poll_timeout(), Some(t0 + Duration::from_secs(600)));
    s.handle_timeout(t0 + Duration::from_secs(599));
    assert_eq!(s.allocation_count(), 1);
    s.handle_timeout(t0 + Duration::from_secs(600));
    assert_eq!(s.allocation_count(), 0);
    assert_eq!(
        s.poll_event(),
        Some(TurnServerEvent::AllocationClosed { relay: local_relay })
    );
}

#[test]
fn refresh_zero_deletes() {
    let mut s = TurnServer::new(config()).unwrap();
    let t0 = Instant::now();
    let mut c = Cli::new("203.0.113.5:4000", "alice", "pw");
    ok(&c.allocate(&mut s, t0), &c.key);
    let m = c
        .call(&mut s, t0, Method::REFRESH, vec![Attribute::Lifetime(0)])
        .unwrap();
    ok(&m, &c.key);
    assert_eq!(m.lifetime(), Some(0));
    assert_eq!(s.allocation_count(), 0);
    assert_eq!(
        err(&c.call(&mut s, t0, Method::REFRESH, vec![]).unwrap()),
        437
    );
    // 消えた後は新たに作れる
    ok(
        &c.call(
            &mut s,
            t0,
            Method::ALLOCATE,
            vec![Attribute::RequestedTransport(17)],
        )
        .unwrap(),
        &c.key,
    );
}

#[test]
fn wrong_password_and_unknown_user() {
    let mut s = TurnServer::new(config()).unwrap();
    let t0 = Instant::now();
    let mut c = Cli::new("203.0.113.5:4000", "alice", "nope");
    c.challenge(&mut s, t0);
    let m = c
        .call(
            &mut s,
            t0,
            Method::ALLOCATE,
            vec![Attribute::RequestedTransport(17)],
        )
        .unwrap();
    assert_eq!(err(&m), 401);
    assert!(m.verify_integrity(&c.key).is_err());
    let mut c = Cli::new("203.0.113.5:4001", "mallory", "pw");
    c.challenge(&mut s, t0);
    assert_eq!(
        err(&c
            .call(
                &mut s,
                t0,
                Method::ALLOCATE,
                vec![Attribute::RequestedTransport(17)]
            )
            .unwrap()),
        401
    );
    assert_eq!(s.allocation_count(), 0);
}

#[test]
fn rest_credentials() {
    let mut s = TurnServer::new(config()).unwrap();
    let t0 = Instant::now();
    let (user, pass) = rest_api_credentials(b"s3cret", "carol", 2_000);
    // 壁時計が無ければ使えない（閉じた側に倒す）
    let mut c = Cli::new("203.0.113.5:4000", &user, &pass);
    c.challenge(&mut s, t0);
    assert_eq!(
        err(&c
            .call(
                &mut s,
                t0,
                Method::ALLOCATE,
                vec![Attribute::RequestedTransport(17)]
            )
            .unwrap()),
        401
    );
    // 期限内
    s.set_unix_time(1_000);
    ok(
        &c.call(
            &mut s,
            t0,
            Method::ALLOCATE,
            vec![Attribute::RequestedTransport(17)],
        )
        .unwrap(),
        &c.key,
    );
    // 期限切れ
    s.set_unix_time(2_001);
    let mut c2 = Cli::new("203.0.113.5:4001", &user, &pass);
    c2.challenge(&mut s, t0);
    assert_eq!(
        err(&c2
            .call(
                &mut s,
                t0,
                Method::ALLOCATE,
                vec![Attribute::RequestedTransport(17)]
            )
            .unwrap()),
        401
    );
    // 違う秘密
    s.set_unix_time(1_000);
    let (user, pass) = rest_api_credentials(b"other", "carol", 2_000);
    let mut c3 = Cli::new("203.0.113.5:4002", &user, &pass);
    c3.challenge(&mut s, t0);
    assert_eq!(
        err(&c3
            .call(
                &mut s,
                t0,
                Method::ALLOCATE,
                vec![Attribute::RequestedTransport(17)]
            )
            .unwrap()),
        401
    );
}

#[test]
fn stale_nonce() {
    let mut s = TurnServer::new(config()).unwrap();
    let t0 = Instant::now();
    let mut c = Cli::new("203.0.113.5:4000", "alice", "pw");
    ok(&c.allocate(&mut s, t0), &c.key);
    ok(
        &c.call(&mut s, t0, Method::REFRESH, vec![Attribute::Lifetime(3600)])
            .unwrap(),
        &c.key,
    );
    let later = t0 + Duration::from_secs(601);
    let m = c.call(&mut s, later, Method::REFRESH, vec![]).unwrap();
    assert_eq!(err(&m), 438);
    c.nonce = m.nonce().unwrap().to_string();
    ok(
        &c.call(&mut s, later, Method::REFRESH, vec![]).unwrap(),
        &c.key,
    );
    // 別の送信元アドレスに出した nonce は使えない
    let mut other = Cli::new("203.0.113.6:4000", "alice", "pw");
    other.nonce = c.nonce.clone();
    assert_eq!(
        err(&other
            .call(
                &mut s,
                later,
                Method::ALLOCATE,
                vec![Attribute::RequestedTransport(17)]
            )
            .unwrap()),
        438
    );
    // 改ざんした nonce
    c.nonce.replace_range(0..1, "f");
    assert_eq!(
        err(&c.call(&mut s, later, Method::REFRESH, vec![]).unwrap()),
        438
    );
}

#[test]
fn wrong_credentials_on_existing_allocation() {
    let mut s = TurnServer::new(config()).unwrap();
    let t0 = Instant::now();
    let mut c = Cli::new("203.0.113.5:4000", "alice", "pw");
    ok(&c.allocate(&mut s, t0), &c.key);
    let mut b = Cli::new("203.0.113.5:4000", "bob", "pw2");
    b.nonce = c.nonce.clone();
    assert_eq!(
        err(&b.call(&mut s, t0, Method::REFRESH, vec![]).unwrap()),
        441
    );
}

#[test]
fn retransmitted_allocate_gets_same_response() {
    let mut s = TurnServer::new(config()).unwrap();
    let t0 = Instant::now();
    let mut c = Cli::new("203.0.113.5:4000", "alice", "pw");
    c.challenge(&mut s, t0);
    let (_, b) = c.request(Method::ALLOCATE, vec![Attribute::RequestedTransport(17)]);
    s.handle_client(t0, listen(), c.addr, b.clone());
    let first = drain(&mut s);
    s.handle_client(t0 + Duration::from_millis(500), listen(), c.addr, b);
    let second = drain(&mut s);
    assert_eq!(first, second);
    assert_eq!(s.allocation_count(), 1);
}

/// 許可・channel・relay のための準備。
fn allocated() -> (TurnServer, Instant, Cli, SocketAddr) {
    let mut s = TurnServer::new(config()).unwrap();
    let t0 = Instant::now();
    let mut c = Cli::new("203.0.113.5:4000", "alice", "pw");
    let _ = c.allocate(&mut s, t0);
    let Some(TurnServerEvent::AllocationCreated { relay, .. }) = s.poll_event() else {
        panic!()
    };
    (s, t0, c, relay)
}

fn send_indication(peer: SocketAddr, data: &'static [u8]) -> Bytes {
    MessageBuilder::new(
        Class::Indication,
        Method::SEND,
        TransactionId::random().unwrap(),
    )
    .attr(Attribute::XorPeerAddress(peer))
    .attr(Attribute::Data(Bytes::from_static(data)))
    .build()
    .unwrap()
}

#[test]
fn permission_enforcement() {
    let (mut s, t0, c, relay) = allocated();
    let peer = sa("8.8.8.8:5000");
    // 許可なし: 相手からのデータも Send も捨てる
    s.handle_peer(t0, relay, peer, Bytes::from_static(b"hi"));
    assert!(drain(&mut s).is_empty());
    s.handle_client(t0, listen(), c.addr, send_indication(peer, b"x"));
    assert!(drain(&mut s).is_empty());

    ok(
        &c.call(
            &mut s,
            t0,
            Method::CREATE_PERMISSION,
            vec![Attribute::XorPeerAddress(peer)],
        )
        .unwrap(),
        &c.key,
    );
    // Send → relay から相手へ
    s.handle_client(t0, listen(), c.addr, send_indication(peer, b"x"));
    let t = drain(&mut s);
    assert_eq!(
        t,
        vec![Transmit {
            local: relay,
            remote: peer,
            data: Bytes::from_static(b"x")
        }]
    );
    // 相手から（別ポートでも IP が同じなら可）→ Data indication
    s.handle_peer(t0, relay, sa("8.8.8.8:5001"), Bytes::from_static(b"hi"));
    let m = recv(&mut s, c.addr).unwrap();
    assert_eq!((m.class, m.method), (Class::Indication, Method::DATA));
    assert_eq!(m.xor_peer_addresses().next(), Some(sa("8.8.8.8:5001")));
    assert_eq!(&m.data().unwrap()[..], b"hi");
    // 別の IP は捨てる
    s.handle_peer(t0, relay, sa("8.8.4.4:5000"), Bytes::from_static(b"hi"));
    assert!(drain(&mut s).is_empty());
    // 300 秒で切れる
    let later = t0 + Duration::from_secs(300);
    s.handle_timeout(later);
    s.handle_peer(later, relay, peer, Bytes::from_static(b"hi"));
    assert!(drain(&mut s).is_empty());
}

#[test]
fn denies_private_peers() {
    let (mut s, t0, c, _) = allocated();
    for p in [
        "127.0.0.1:80",
        "10.1.1.1:80",
        "192.168.0.1:80",
        "169.254.169.254:80",
    ] {
        let m = c
            .call(
                &mut s,
                t0,
                Method::CREATE_PERMISSION,
                vec![Attribute::XorPeerAddress(sa(p))],
            )
            .unwrap();
        assert_eq!(err(&m), 403, "{p}");
        let m = c
            .call(
                &mut s,
                t0,
                Method::CHANNEL_BIND,
                vec![
                    Attribute::ChannelNumber(0x4000),
                    Attribute::XorPeerAddress(sa(p)),
                ],
            )
            .unwrap();
        assert_eq!(err(&m), 403, "{p}");
    }
    // 1 つでも拒否があれば全体を拒否
    let m = c
        .call(
            &mut s,
            t0,
            Method::CREATE_PERMISSION,
            vec![
                Attribute::XorPeerAddress(sa("8.8.8.8:1")),
                Attribute::XorPeerAddress(sa("10.0.0.2:1")),
            ],
        )
        .unwrap();
    assert_eq!(err(&m), 403);
    s.handle_client(t0, listen(), c.addr, send_indication(sa("8.8.8.8:1"), b"x"));
    assert!(drain(&mut s).is_empty());
    // IPv6 の相手は IPv4 の relay では 443
    let m = c
        .call(
            &mut s,
            t0,
            Method::CREATE_PERMISSION,
            vec![Attribute::XorPeerAddress(sa("[2a00::1]:1"))],
        )
        .unwrap();
    assert_eq!(err(&m), 443);
}

#[test]
fn allowed_private_network() {
    let mut cfg = config();
    cfg.peer_policy = PeerPolicy {
        allow: vec!["10.0.0.0/8".parse().unwrap()],
        deny: vec![],
    };
    let mut s = TurnServer::new(cfg).unwrap();
    let t0 = Instant::now();
    let mut c = Cli::new("203.0.113.5:4000", "alice", "pw");
    let _ = c.allocate(&mut s, t0);
    ok(
        &c.call(
            &mut s,
            t0,
            Method::CREATE_PERMISSION,
            vec![Attribute::XorPeerAddress(sa("10.1.1.1:5"))],
        )
        .unwrap(),
        &c.key,
    );
    assert_eq!(
        err(&c
            .call(
                &mut s,
                t0,
                Method::CREATE_PERMISSION,
                vec![Attribute::XorPeerAddress(sa("127.0.0.1:5"))]
            )
            .unwrap()),
        403
    );
}

#[test]
fn channel_bind_and_channel_data() {
    let (mut s, t0, c, relay) = allocated();
    let peer = sa("8.8.8.8:5000");
    let bind =
        |n: u16, p: SocketAddr| vec![Attribute::ChannelNumber(n), Attribute::XorPeerAddress(p)];
    assert_eq!(
        err(&c
            .call(&mut s, t0, Method::CHANNEL_BIND, bind(0x3FFF, peer))
            .unwrap()),
        400
    );
    assert_eq!(
        err(&c
            .call(
                &mut s,
                t0,
                Method::CHANNEL_BIND,
                vec![Attribute::ChannelNumber(0x4000)]
            )
            .unwrap()),
        400
    );
    ok(
        &c.call(&mut s, t0, Method::CHANNEL_BIND, bind(0x4000, peer))
            .unwrap(),
        &c.key,
    );
    // 同じ組の再 bind は可、番号・相手の付け替えは 400
    ok(
        &c.call(&mut s, t0, Method::CHANNEL_BIND, bind(0x4000, peer))
            .unwrap(),
        &c.key,
    );
    assert_eq!(
        err(&c
            .call(
                &mut s,
                t0,
                Method::CHANNEL_BIND,
                bind(0x4000, sa("8.8.8.8:5001"))
            )
            .unwrap()),
        400
    );
    assert_eq!(
        err(&c
            .call(&mut s, t0, Method::CHANNEL_BIND, bind(0x4001, peer))
            .unwrap()),
        400
    );

    // client → peer
    s.handle_client(
        t0,
        listen(),
        c.addr,
        ChannelData::encode(0x4000, b"abc", true),
    );
    assert_eq!(
        drain(&mut s),
        vec![Transmit {
            local: relay,
            remote: peer,
            data: Bytes::from_static(b"abc")
        }]
    );
    // 知らない channel は捨てる
    s.handle_client(
        t0,
        listen(),
        c.addr,
        ChannelData::encode(0x4002, b"abc", true),
    );
    assert!(drain(&mut s).is_empty());
    // peer → client は ChannelData
    s.handle_peer(t0, relay, peer, Bytes::from_static(b"xyz"));
    let t = drain(&mut s);
    assert_eq!(t.len(), 1);
    assert_eq!((t[0].local, t[0].remote), (listen(), c.addr));
    let cd = ChannelData::parse(t[0].data.clone()).unwrap();
    assert_eq!((cd.number, &cd.data[..]), (0x4000, &b"xyz"[..]));
    // 同じ IP の別ポートは Data indication（permission は IP 単位）
    s.handle_peer(t0, relay, sa("8.8.8.8:6000"), Bytes::from_static(b"q"));
    assert_eq!(recv(&mut s, c.addr).unwrap().method, Method::DATA);

    // permission（300 秒）を ChannelBind で更新しなければ channel が生きていても届かない
    let t1 = t0 + Duration::from_secs(301);
    s.handle_peer(t1, relay, peer, Bytes::from_static(b"xyz"));
    assert!(drain(&mut s).is_empty());
    // channel は 600 秒、その後 300 秒は別の相手に使えない
    let t2 = t0 + Duration::from_secs(700);
    c.call(&mut s, t2, Method::REFRESH, vec![]); // nonce 期限切れ（438）でも構わない
    s.handle_timeout(t2);
}

#[test]
fn channel_cooldown() {
    let mut cfg = config();
    cfg.nonce_lifetime = Duration::from_secs(100_000);
    cfg.max_lifetime = Duration::from_secs(3600);
    let mut s = TurnServer::new(cfg).unwrap();
    let t0 = Instant::now();
    let mut c = Cli::new("203.0.113.5:4000", "alice", "pw");
    let _ = c.allocate(&mut s, t0);
    let _ = c.call(&mut s, t0, Method::REFRESH, vec![Attribute::Lifetime(3600)]);
    let bind = |n: u16, p: &str| {
        vec![
            Attribute::ChannelNumber(n),
            Attribute::XorPeerAddress(sa(p)),
        ]
    };
    ok(
        &c.call(&mut s, t0, Method::CHANNEL_BIND, bind(0x4000, "8.8.8.8:1"))
            .unwrap(),
        &c.key,
    );
    let t1 = t0 + Duration::from_secs(700); // channel は切れ、cooldown 中
    s.handle_timeout(t1);
    assert_eq!(
        err(&c
            .call(&mut s, t1, Method::CHANNEL_BIND, bind(0x4000, "8.8.8.8:2"))
            .unwrap()),
        400
    );
    let t2 = t0 + Duration::from_secs(901);
    s.handle_timeout(t2);
    ok(
        &c.call(&mut s, t2, Method::CHANNEL_BIND, bind(0x4000, "8.8.8.8:2"))
            .unwrap(),
        &c.key,
    );
}

#[test]
fn quotas() {
    let mut cfg = config();
    cfg.quotas = Quotas {
        per_user: 1,
        per_tenant: 2,
        ..Quotas::default()
    };
    cfg.users.push(StaticUser {
        username: "dave".into(),
        password: "pw3".into(),
        tenant: Some("t1".into()),
    });
    let mut s = TurnServer::new(cfg).unwrap();
    let t0 = Instant::now();
    let mut a1 = Cli::new("203.0.113.5:1", "alice", "pw");
    ok(&a1.allocate(&mut s, t0), &a1.key);
    // 同じ利用者の 2 つ目は 486
    let mut a2 = Cli::new("203.0.113.5:2", "alice", "pw");
    assert_eq!(err(&a2.allocate(&mut s, t0)), 486);
    // 同じテナントの 2 人目は可、3 人目は 486
    let mut b = Cli::new("203.0.113.5:3", "bob", "pw2");
    ok(&b.allocate(&mut s, t0), &b.key);
    let mut d = Cli::new("203.0.113.5:4", "dave", "pw3");
    assert_eq!(err(&d.allocate(&mut s, t0)), 486);
    // 1 つ消せば空く
    ok(
        &a1.call(&mut s, t0, Method::REFRESH, vec![Attribute::Lifetime(0)])
            .unwrap(),
        &a1.key,
    );
    ok(
        &d.call(
            &mut s,
            t0,
            Method::ALLOCATE,
            vec![Attribute::RequestedTransport(17)],
        )
        .unwrap(),
        &d.key,
    );
}

#[test]
fn permission_quota_and_port_exhaustion() {
    let mut cfg = config();
    cfg.quotas.permissions_per_allocation = 2;
    cfg.port_min = 50000;
    cfg.port_max = 50000;
    let mut s = TurnServer::new(cfg).unwrap();
    let t0 = Instant::now();
    let mut c = Cli::new("203.0.113.5:1", "alice", "pw");
    ok(&c.allocate(&mut s, t0), &c.key);
    for ip in ["8.8.8.1:1", "8.8.8.2:1"] {
        ok(
            &c.call(
                &mut s,
                t0,
                Method::CREATE_PERMISSION,
                vec![Attribute::XorPeerAddress(sa(ip))],
            )
            .unwrap(),
            &c.key,
        );
    }
    assert_eq!(
        err(&c
            .call(
                &mut s,
                t0,
                Method::CREATE_PERMISSION,
                vec![Attribute::XorPeerAddress(sa("8.8.8.3:1"))]
            )
            .unwrap()),
        508
    );
    // 既存の更新は可
    ok(
        &c.call(
            &mut s,
            t0,
            Method::CREATE_PERMISSION,
            vec![Attribute::XorPeerAddress(sa("8.8.8.1:9"))],
        )
        .unwrap(),
        &c.key,
    );
    // ポートが尽きたら 508
    let mut b = Cli::new("203.0.113.5:2", "bob", "pw2");
    assert_eq!(err(&b.allocate(&mut s, t0)), 508);
}

#[test]
fn allocate_attribute_checks() {
    let mut s = TurnServer::new(config()).unwrap();
    let t0 = Instant::now();
    let mut c = Cli::new("203.0.113.5:1", "alice", "pw");
    c.challenge(&mut s, t0);
    let mut call =
        |attrs: Vec<Attribute>| err(&c.call(&mut s, t0, Method::ALLOCATE, attrs).unwrap());
    assert_eq!(call(vec![]), 400);
    assert_eq!(call(vec![Attribute::RequestedTransport(6)]), 442);
    assert_eq!(
        call(vec![
            Attribute::RequestedTransport(17),
            Attribute::RequestedAddressFamily(senca_stun::AddressFamily::V6)
        ]),
        440
    );
    assert_eq!(
        call(vec![
            Attribute::RequestedTransport(17),
            Attribute::EvenPort(true)
        ]),
        508
    );
    assert_eq!(
        call(vec![
            Attribute::RequestedTransport(17),
            Attribute::ReservationToken([1; 8])
        ]),
        508
    );
    assert_eq!(
        call(vec![
            Attribute::RequestedTransport(17),
            Attribute::RequestedAddressFamily(senca_stun::AddressFamily::V4),
            Attribute::AdditionalAddressFamily(senca_stun::AddressFamily::V6)
        ]),
        400
    );
    let m = c
        .call(
            &mut s,
            t0,
            Method::ALLOCATE,
            vec![Attribute::RequestedTransport(17), Attribute::DontFragment],
        )
        .unwrap();
    assert_eq!(err(&m), 420);
    assert_eq!(
        m.get(attr_type::UNKNOWN_ATTRIBUTES),
        Some(&Attribute::UnknownAttributes(vec![
            attr_type::DONT_FRAGMENT
        ]))
    );
    let m = c
        .call(
            &mut s,
            t0,
            Method::ALLOCATE,
            vec![
                Attribute::RequestedTransport(17),
                Attribute::Unknown {
                    typ: 0x0077,
                    value: Bytes::from_static(b"zz"),
                },
            ],
        )
        .unwrap();
    assert_eq!(err(&m), 420);
    assert_eq!(
        m.get(attr_type::UNKNOWN_ATTRIBUTES),
        Some(&Attribute::UnknownAttributes(vec![0x0077]))
    );
    // EVEN-PORT（R=0）は偶数ポート
    let m = c
        .call(
            &mut s,
            t0,
            Method::ALLOCATE,
            vec![
                Attribute::RequestedTransport(17),
                Attribute::EvenPort(false),
            ],
        )
        .unwrap();
    ok(&m, &c.key);
    assert_eq!(m.xor_relayed_address().unwrap().port() % 2, 0);
}

#[test]
fn additional_family_partial() {
    let mut s = TurnServer::new(config()).unwrap();
    let t0 = Instant::now();
    let mut c = Cli::new("203.0.113.5:1", "alice", "pw");
    c.challenge(&mut s, t0);
    let m = c
        .call(
            &mut s,
            t0,
            Method::ALLOCATE,
            vec![
                Attribute::RequestedTransport(17),
                Attribute::AdditionalAddressFamily(senca_stun::AddressFamily::V6),
            ],
        )
        .unwrap();
    ok(&m, &c.key);
    assert!(matches!(
        m.get(attr_type::ADDRESS_ERROR_CODE),
        Some(Attribute::AddressErrorCode { code: 440, .. })
    ));
}

#[test]
fn binding_and_rate_limit() {
    let mut s = TurnServer::new(config()).unwrap();
    let t0 = Instant::now();
    let from = sa("203.0.113.9:7");
    let mut answered = 0;
    for _ in 0..100 {
        let b = MessageBuilder::new(
            Class::Request,
            Method::BINDING,
            TransactionId::random().unwrap(),
        )
        .fingerprint()
        .build()
        .unwrap();
        s.handle_client(t0, listen(), from, b);
        if let Some(m) = recv(&mut s, from) {
            assert_eq!(m.mapped_address(), Some(from));
            answered += 1;
        }
    }
    // 既定の burst 20
    assert_eq!(answered, 20);
    // 時間が経てば戻る
    let b = MessageBuilder::new(
        Class::Request,
        Method::BINDING,
        TransactionId::random().unwrap(),
    )
    .build()
    .unwrap();
    s.handle_client(t0 + Duration::from_secs(1), listen(), from, b);
    assert!(recv(&mut s, from).is_some());
    // 別の IP は影響を受けない
    let b = MessageBuilder::new(
        Class::Request,
        Method::BINDING,
        TransactionId::random().unwrap(),
    )
    .build()
    .unwrap();
    s.handle_client(t0, listen(), sa("203.0.113.10:7"), b);
    assert!(recv(&mut s, sa("203.0.113.10:7")).is_some());
}

#[test]
fn ignores_non_requests_and_garbage() {
    let mut s = TurnServer::new(config()).unwrap();
    let t0 = Instant::now();
    let from = sa("203.0.113.9:7");
    // 応答・Data indication・未知の indication には応えない
    for (class, method) in [
        (Class::Success, Method::BINDING),
        (Class::Indication, Method::DATA),
        (Class::Error, Method::ALLOCATE),
    ] {
        let b = MessageBuilder::new(class, method, TransactionId::random().unwrap())
            .build()
            .unwrap();
        s.handle_client(t0, listen(), from, b);
    }
    assert!(drain(&mut s).is_empty());
    // ランダムな入力でパニックしない
    let mut seed = 7u32;
    let base = Cli::new("203.0.113.5:1", "alice", "pw")
        .request(Method::ALLOCATE, vec![Attribute::RequestedTransport(17)])
        .1;
    for i in 0..20000 {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let mut v = if i % 3 == 0 {
            base.to_vec()
        } else {
            vec![0x40, 0x00, 0x00, 0x08, 1, 2, 3, 4, 5, 6, 7, 8]
        };
        let idx = (seed as usize >> 4) % v.len();
        v[idx] = (seed >> 16) as u8;
        if seed & 1 == 0 {
            v.truncate((seed as usize >> 8) % (v.len() + 1));
        }
        s.handle_client(t0, listen(), from, Bytes::from(v.clone()));
        s.handle_peer(t0, sa("10.0.0.1:50000"), sa("8.8.8.8:1"), Bytes::from(v));
        drain(&mut s);
    }
}

#[test]
fn relay_unavailable_removes_allocation() {
    let (mut s, t0, c, relay) = allocated();
    s.relay_unavailable(t0, relay);
    assert_eq!(s.allocation_count(), 0);
    assert_eq!(s.poll_event(), None);
    assert_eq!(
        err(&c.call(&mut s, t0, Method::REFRESH, vec![]).unwrap()),
        437
    );
}

#[test]
fn debug_hides_secrets() {
    let d = format!("{:?}", config());
    assert!(!d.contains("pw2"));
    assert!(!d.contains("s3cret") && !d.contains("115, 51"));
}
