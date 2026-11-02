//! MoQ の結合試験: メモリ上の QUIC で publisher → relay → subscriber。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::{Bytes, BytesMut};
use senca_moq::auth::{AllowAll, Authorizer, NamespaceAcl, Rule, token_hash};
use senca_moq::params::{LocationFilter, Params, Token};
use senca_moq::*;
use senca_quic::{ClientConfig, Endpoint, EndpointConfig, ServerConfig};
use senca_tls::{CertifiedKey, RootStore, ServerVerifier};

const CA: &[u8] = include_bytes!("../../senca-tls/tests/data/ca.pem");
const CHAIN: &[u8] = include_bytes!("../../senca-tls/tests/data/server-ecdsa.pem");
const KEY: &[u8] = include_bytes!("../../senca-tls/tests/data/server-ecdsa.key");

fn server(auth: Arc<dyn Authorizer>) -> MoqServer {
    let mut tls = senca_tls::ServerConfig::new(Arc::new(CertifiedKey::from_pem(CHAIN, KEY).unwrap())).unwrap();
    tls.alpn_protocols = vec![ALPN.to_vec(), senca_h3::ALPN.to_vec()];
    let ep = Endpoint::new(EndpointConfig::new().unwrap(), Some(Arc::new(ServerConfig::new(Arc::new(tls)))));
    MoqServer::new(ep, Relay::new(RelayConfig::new(auth)), "/moq")
}

fn client_quic(alpn: &[u8]) -> ClientConfig {
    let mut r = RootStore::new();
    r.add_pem(CA).unwrap();
    let mut tls = senca_tls::ClientConfig::new(ServerVerifier::WebPki {
        roots: Arc::new(r),
        spki_pins: vec![],
    });
    tls.alpn_protocols = vec![alpn.to_vec()];
    ClientConfig::new(Arc::new(tls))
}

struct Flight {
    at: Instant,
    from: SocketAddr,
    to: SocketAddr,
    data: Vec<u8>,
}

struct World {
    now: Instant,
    server: MoqServer,
    saddr: SocketAddr,
    clients: Vec<(SocketAddr, MoqClient)>,
    flight: Vec<Flight>,
    loss_every: usize,
    counter: usize,
}

impl World {
    fn new(auth: Arc<dyn Authorizer>) -> Self {
        Self {
            now: Instant::now(),
            server: server(auth),
            saddr: "10.0.0.1:443".parse().unwrap(),
            clients: Vec::new(),
            flight: Vec::new(),
            loss_every: 0,
            counter: 0,
        }
    }

    fn add_native(&mut self, cfg: SessionConfig) -> usize {
        self.add_native_alpn(cfg, ALPN)
    }

    fn add_native_alpn(&mut self, cfg: SessionConfig, alpn: &[u8]) -> usize {
        let addr: SocketAddr = format!("10.0.1.{}:5000", self.clients.len() + 1).parse().unwrap();
        let ep = Endpoint::new(EndpointConfig::new().unwrap(), None);
        let c = MoqClient::connect_native(ep, client_quic(alpn), self.saddr, "localhost", cfg, self.now, SystemTime::now()).unwrap();
        self.clients.push((addr, c));
        self.clients.len() - 1
    }

    fn add_wt(&mut self, cfg: SessionConfig, path: &str) -> usize {
        let addr: SocketAddr = format!("10.0.1.{}:5000", self.clients.len() + 1).parse().unwrap();
        let ep = Endpoint::new(EndpointConfig::new().unwrap(), None);
        let c = MoqClient::connect_webtransport(
            ep,
            client_quic(senca_h3::ALPN),
            self.saddr,
            "localhost",
            "localhost",
            path,
            cfg,
            self.now,
            SystemTime::now(),
        )
        .unwrap();
        self.clients.push((addr, c));
        self.clients.len() - 1
    }

    fn c(&mut self, i: usize) -> &mut Session {
        &mut self.clients[i].1.session
    }

    fn step(&mut self) {
        let now = self.now;
        for (_, c) in &mut self.clients {
            c.drive(now);
        }
        self.server.drive(now);
        for (addr, c) in &mut self.clients {
            while let Some(t) = c.poll_transmit(now) {
                self.counter += 1;
                if self.loss_every > 0 && self.counter.is_multiple_of(self.loss_every) {
                    continue;
                }
                self.flight.push(Flight {
                    at: now + Duration::from_millis(5),
                    from: *addr,
                    to: t.destination,
                    data: t.contents,
                });
            }
        }
        while let Some(t) = self.server.poll_transmit(now) {
            self.counter += 1;
            if self.loss_every > 0 && self.counter.is_multiple_of(self.loss_every) {
                continue;
            }
            self.flight.push(Flight {
                at: now + Duration::from_millis(5),
                from: self.saddr,
                to: t.destination,
                data: t.contents,
            });
        }
        self.now += Duration::from_millis(1);
        let now = self.now;
        let due: Vec<Flight> = {
            let (d, k): (Vec<Flight>, Vec<Flight>) = std::mem::take(&mut self.flight).into_iter().partition(|f| f.at <= now);
            self.flight = k;
            d
        };
        for f in due {
            if f.to == self.saddr {
                self.server.handle_datagram(now, SystemTime::now(), f.from, BytesMut::from(&f.data[..]));
            } else if let Some((_, c)) = self.clients.iter_mut().find(|(a, _)| *a == f.to) {
                c.handle_datagram(now, SystemTime::now(), f.from, BytesMut::from(&f.data[..]));
            }
        }
        self.server.handle_timeout(now);
        for (_, c) in &mut self.clients {
            c.handle_timeout(now);
        }
    }

    /// 進めながら、各 client のイベントを集める。
    fn run(&mut self, steps: usize, mut f: impl FnMut(&mut World, usize, SessionEvent)) {
        for _ in 0..steps {
            self.step();
            for i in 0..self.clients.len() {
                while let Some(e) = self.clients[i].1.session.poll_event() {
                    f(self, i, e);
                }
            }
        }
    }
}

fn obj(group: u64, id: u64, payload: &[u8]) -> Object {
    Object {
        group,
        subgroup: Some(0),
        id,
        priority: 100,
        status: ObjectStatus::Normal,
        properties: Bytes::new(),
        payload: Bytes::copy_from_slice(payload),
    }
}

fn ns() -> Namespace {
    Namespace::from_path("live/room1")
}

/// publisher が PUBLISH_NAMESPACE し、SUBSCRIBE に応えて object を送る。
#[derive(Default)]
struct Pub {
    sub_req: Option<u64>,
    announced: bool,
}

fn publisher_logic(w: &mut World, i: usize, e: &SessionEvent, p: &mut Pub, tokens: Vec<Token>) {
    match e {
        SessionEvent::Ready { .. } => {
            let params = Params {
                auth_tokens: tokens,
                ..Default::default()
            };
            w.c(i).publish_namespace(ns(), params).unwrap();
        }
        SessionEvent::RequestOk { .. } => p.announced = true,
        SessionEvent::Subscribe { request_id, track, .. } => {
            assert_eq!(&track[..], b"video");
            w.c(i).subscribe_ok(*request_id, None, Bytes::new()).unwrap();
            p.sub_req = Some(*request_id);
        }
        _ => {}
    }
}

#[test]
fn publish_subscribe_through_relay_native_and_webtransport() {
    let mut w = World::new(Arc::new(AllowAll));
    let publisher = w.add_native(SessionConfig::new(Role::Client));
    let sub_native = w.add_native(SessionConfig::new(Role::Client));
    let sub_wt = w.add_wt(SessionConfig::new(Role::Client), "/moq");
    let mut p = Pub::default();
    let mut ready = [false; 3];
    let mut subscribed = [false; 3];
    let mut got: [Vec<(u64, u64, Bytes, Delivery)>; 3] = Default::default();
    let mut sent = false;
    let mut payloads = Vec::new();
    for g in 0..3u64 {
        for o in 0..4u64 {
            payloads.push((g, o, Bytes::from(format!("g{g}o{o}-").repeat(200 + (g * 4 + o) as usize))));
        }
    }
    w.run(1500, |w, i, e| {
        if i == publisher {
            publisher_logic(w, i, &e, &mut p, vec![]);
            return;
        }
        match e {
            SessionEvent::Ready { .. } => {
                ready[i] = true;
            }
            SessionEvent::SubscribeOk { .. } => subscribed[i] = true,
            SessionEvent::Object { object, delivery, .. } => got[i].push((object.group, object.id, object.payload, delivery)),
            SessionEvent::RequestError { code, reason, .. } => panic!("subscriber {i} got error {code:#x} {reason}"),
            _ => {}
        }
    });
    assert!(p.announced, "namespace not announced");
    assert!(ready[sub_native] && ready[sub_wt]);
    // 購読（WebTransport の client も同じ relay へ）
    for i in [sub_native, sub_wt] {
        w.c(i)
            .subscribe(SubscribeArgs {
                namespace: ns(),
                track: Bytes::from_static(b"video"),
                params: Params::default(),
            })
            .unwrap();
    }
    w.run(1500, |w, i, e| {
        if i == publisher {
            publisher_logic(w, i, &e, &mut p, vec![]);
            if let (Some(req), false) = (p.sub_req, sent) {
                // 両方の購読が上流で一つにまとめられ、確立したら送る
                sent = true;
                let now = w.now;
                for (g, o, d) in &payloads {
                    w.c(publisher).send_object(req, &obj(*g, *o, d), now, now).unwrap();
                    if *o == 3 {
                        w.c(publisher).end_subgroup(req, *g, 0, now);
                    }
                }
                // datagram の object も
                let mut dg = obj(9, 0, b"datagram-object");
                dg.subgroup = None;
                w.c(publisher).send_object_datagram(req, &dg, now);
            }
            return;
        }
        match e {
            SessionEvent::SubscribeOk { .. } => subscribed[i] = true,
            SessionEvent::Object { object, delivery, .. } => got[i].push((object.group, object.id, object.payload, delivery)),
            SessionEvent::RequestError { code, reason, .. } => panic!("subscriber {i} got error {code:#x} {reason}"),
            _ => {}
        }
    });
    assert!(subscribed[sub_native] && subscribed[sub_wt]);
    for i in [sub_native, sub_wt] {
        let stream: Vec<_> = got[i].iter().filter(|x| x.3 == Delivery::Subgroup).cloned().collect();
        assert_eq!(stream.len(), payloads.len(), "subscriber {i} got {} objects", stream.len());
        for ((g, o, d), (gg, oo, dd, _)) in payloads.iter().zip(stream.iter()) {
            assert_eq!((g, o), (gg, oo));
            assert_eq!(d, dd, "payload mismatch at {g}/{o}");
        }
        assert!(got[i].iter().any(|x| x.3 == Delivery::Datagram && &x.2[..] == b"datagram-object"));
    }
    let st = w.server.relay.stats().clone();
    assert!(st.objects_in >= payloads.len() as u64);
    // 二つの購読者へ出したので、出した数は受けた数の二倍以上
    assert!(st.objects_out >= 2 * payloads.len() as u64);

    // 後から来た購読者は今の group の先頭から（cache）
    let late = w.add_native(SessionConfig::new(Role::Client));
    let mut late_got = Vec::new();
    let mut asked = false;
    w.run(800, |w, i, e| {
        if i == publisher {
            publisher_logic(w, i, &e, &mut p, vec![]);
            return;
        }
        if i == late {
            match e {
                SessionEvent::Ready { .. } if !asked => {
                    asked = true;
                    w.c(late)
                        .subscribe(SubscribeArgs {
                            namespace: ns(),
                            track: Bytes::from_static(b"video"),
                            params: Params {
                                location_filter: Some(LocationFilter::RelativeStart(1)),
                                ..Default::default()
                            },
                        })
                        .unwrap();
                }
                SessionEvent::Object { object, .. } => late_got.push((object.group, object.id, object.payload)),
                _ => {}
            }
        }
    });
    // 最新の group（datagram の group 9）の object が cache から届く
    assert!(late_got.iter().any(|x| x.0 == 9), "late joiner got {:?}", late_got.iter().map(|x| (x.0, x.1)).collect::<Vec<_>>());

    // fetch: relay の cache から group 1 を取り出す
    let mut fetched = Vec::new();
    let mut fetch_ok = false;
    let mut complete = false;
    w.c(sub_native)
        .fetch(
            ns(),
            Bytes::from_static(b"video"),
            Params {
                location_filter: Some(LocationFilter::AbsoluteRange {
                    start: Location::new(1, 0),
                    end_group: 1,
                    end_object: None,
                }),
                ..Default::default()
            },
        )
        .unwrap();
    w.run(500, |w, i, e| {
        if i == publisher {
            publisher_logic(w, i, &e, &mut p, vec![]);
            return;
        }
        if i == sub_native {
            match e {
                SessionEvent::FetchOk { .. } => fetch_ok = true,
                SessionEvent::Object {
                    object,
                    delivery: Delivery::Fetch,
                    ..
                } => fetched.push((object.group, object.id, object.payload)),
                SessionEvent::FetchComplete { .. } => complete = true,
                _ => {}
            }
        }
    });
    assert!(fetch_ok && complete);
    let want: Vec<_> = payloads.iter().filter(|x| x.0 == 1).cloned().collect();
    assert_eq!(fetched, want);
    assert!(w.server.relay.stats().fetches_from_cache >= 1);
}

#[test]
fn relay_rejects_conflicting_object_and_keeps_cache() {
    let mut w = World::new(Arc::new(AllowAll));
    let publisher = w.add_native(SessionConfig::new(Role::Client));
    let sub = w.add_native(SessionConfig::new(Role::Client));
    let mut p = Pub::default();
    w.run(800, |w, i, e| {
        if i == publisher {
            publisher_logic(w, i, &e, &mut p, vec![]);
        }
    });
    assert!(p.announced && w.c(sub).peer_setup().is_some());
    w.c(sub)
        .subscribe(SubscribeArgs {
            namespace: ns(),
            track: Bytes::from_static(b"video"),
            params: Params::default(),
        })
        .unwrap();
    let mut done_status = None;
    let mut stage = 0;
    let mut got = Vec::new();
    w.run(3000, |w, i, e| {
        if i == publisher {
            publisher_logic(w, i, &e, &mut p, vec![]);
            if let (Some(req), 0) = (p.sub_req, stage) {
                stage = 1;
                let now = w.now;
                w.c(publisher).send_object(req, &obj(0, 0, b"original"), now, now).unwrap();
                w.c(publisher).send_object(req, &obj(0, 1, b"next"), now, now).unwrap();
            }
            return;
        }
        match e {
            SessionEvent::PublishDone { status, .. } => done_status = Some(status),
            SessionEvent::Object { object, .. } => {
                got.push(object.payload);
                if got.len() == 2
                    && let Some(req) = p.sub_req
                {
                    // 下流に届いた後で、同じ ID に違う中身を送る（stream を開き直して）
                    let now = w.now;
                    w.c(publisher).send_object(req, &obj(0, 0, b"tampered"), now, now).unwrap();
                }
            }
            _ => {}
        }
    });
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(done_status, Some(publish_done::MALFORMED_TRACK));
    assert_eq!(w.server.relay.stats().cache_mismatches, 1);
    // 書き換えられた object は下流へ出ていない
    assert!(got.iter().all(|p| &p[..] != b"tampered"), "{got:?}");
    let cache = w
        .server
        .relay
        .cache(&FullTrackName::new(ns(), Bytes::from_static(b"video")))
        .unwrap();
    let first = cache.range(Location::new(0, 0), Location::new(0, 0)).next().unwrap();
    assert_eq!(&first.object.payload[..], b"original");
    assert!(cache.is_malformed());
}

#[test]
fn authorization_per_namespace_and_action() {
    let acl = NamespaceAcl {
        rules: vec![Rule {
            prefix: Namespace::from_path("tenantA"),
            publish: vec![token_hash(b"pub-secret")],
            subscribe: vec![token_hash(b"sub-secret")],
            public_subscribe: false,
        }],
    };
    let mut w = World::new(Arc::new(acl));
    let good = w.add_native(SessionConfig::new(Role::Client));
    let _bad = w.add_native(SessionConfig::new(Role::Client));
    let tok = |v: &'static [u8]| {
        vec![Token {
            token_type: 0,
            value: Bytes::from_static(v),
        }]
    };
    let mut results: Vec<(usize, &'static str, Result<(), u64>)> = Vec::new();
    let mut started = [false; 2];
    let mut pending: Vec<(usize, u64, &'static str)> = Vec::new();
    w.run(1500, |w, i, e| match e {
        SessionEvent::Ready { .. } if !started[i] => {
            started[i] = true;
            if i == good {
                let r = w.c(i).publish_namespace(Namespace::from_path("tenantA/live"), Params { auth_tokens: tok(b"pub-secret"), ..Default::default() }).unwrap();
                pending.push((i, r, "good publish tenantA"));
                // 文字列の前方一致では通らない
                let r = w.c(i).publish_namespace(Namespace::from_path("tenantAB/live"), Params { auth_tokens: tok(b"pub-secret"), ..Default::default() }).unwrap();
                pending.push((i, r, "publish tenantAB"));
            } else {
                // subscribe のトークンで publish はできない
                let r = w.c(i).publish_namespace(Namespace::from_path("tenantA/x"), Params { auth_tokens: tok(b"sub-secret"), ..Default::default() }).unwrap();
                pending.push((i, r, "publish with subscribe token"));
                let r = w.c(i).subscribe_namespace(Namespace::from_path("tenantA"), Params { auth_tokens: tok(b"sub-secret"), ..Default::default() }).unwrap();
                pending.push((i, r, "discover with subscribe token"));
                let r = w
                    .c(i)
                    .subscribe(SubscribeArgs {
                        namespace: Namespace::from_path("tenantA/live"),
                        track: Bytes::from_static(b"v"),
                        params: Params::default(),
                    })
                    .unwrap();
                pending.push((i, r, "subscribe without token"));
            }
        }
        SessionEvent::RequestOk { request_id, .. } => {
            if let Some(p) = pending.iter().find(|p| p.0 == i && p.1 == request_id) {
                results.push((i, p.2, Ok(())));
            }
        }
        SessionEvent::RequestError { request_id, code, .. } => {
            if let Some(p) = pending.iter().find(|p| p.0 == i && p.1 == request_id) {
                results.push((i, p.2, Err(code)));
            }
        }
        _ => {}
    });
    let get = |name: &str| results.iter().find(|r| r.1 == name).map(|r| r.2).unwrap_or_else(|| panic!("no result for {name}: {results:?}"));
    assert_eq!(get("good publish tenantA"), Ok(()));
    assert_eq!(get("publish tenantAB"), Err(RequestErrorCode::UNAUTHORIZED.0));
    assert_eq!(get("publish with subscribe token"), Err(RequestErrorCode::UNAUTHORIZED.0));
    assert_eq!(get("discover with subscribe token"), Ok(()));
    assert_eq!(get("subscribe without token"), Err(RequestErrorCode::UNAUTHORIZED.0));
    assert!(w.server.relay.stats().unauthorized >= 3);
}

#[test]
fn unknown_version_is_rejected() {
    let mut w = World::new(Arc::new(AllowAll));
    let old = w.add_native_alpn(SessionConfig::new(Role::Client), b"moqt-21");
    let ok = w.add_native(SessionConfig::new(Role::Client));
    let mut ready = [false; 2];
    w.run(500, |_, i, e| {
        if let SessionEvent::Ready { .. } = e {
            ready[i] = true;
        }
    });
    assert!(ready[ok]);
    assert!(!ready[old]);
    // TLS の no_application_protocol（CRYPTO_ERROR 0x178）で閉じられる
    let c = &w.clients[old].1;
    let closed = c.endpoint.connection(c.conn).is_none_or(|c| {
        matches!(
            c.close_reason(),
            Some(senca_quic::ConnectionError::ConnectionClosed { code, .. }) if code.0 == 0x100 + 120
        )
    });
    assert!(closed);
    // WebTransport で未知の版だけを出す
    let wt = w.add_wt(SessionConfig::new(Role::Client), "/moq");
    // client の wt-available-protocols を書き換えられないので、パスを間違えたときの拒否で代用しない:
    // server 側の関数で直接確かめる
    let h = senca_h3::Headers::new().with("wt-available-protocols", "\"moqt-21\", \"moq-lite-03\"");
    assert_eq!(offers_protocol(&h, WT_PROTOCOL), Some(false));
    let h = senca_h3::Headers::new().with("wt-available-protocols", "\"moqt-21\", \"moqt-22\"");
    assert_eq!(offers_protocol(&h, WT_PROTOCOL), Some(true));
    let mut wt_ready = false;
    w.run(500, |_, i, e| {
        if i == wt && matches!(e, SessionEvent::Ready { .. }) {
            wt_ready = true;
        }
    });
    assert!(wt_ready);
    let wrong = w.add_wt(SessionConfig::new(Role::Client), "/other");
    w.run(300, |_, _, _| {});
    assert_eq!(w.clients[wrong].1.rejected(), Some(404));
}

#[test]
fn relay_delivery_survives_packet_loss() {
    let mut w = World::new(Arc::new(AllowAll));
    w.loss_every = 13;
    let publisher = w.add_native(SessionConfig::new(Role::Client));
    let sub = w.add_native(SessionConfig::new(Role::Client));
    let mut p = Pub::default();
    w.run(2000, |w, i, e| {
        if i == publisher {
            publisher_logic(w, i, &e, &mut p, vec![]);
        }
    });
    assert!(p.announced && w.c(sub).peer_setup().is_some());
    w.c(sub)
        .subscribe(SubscribeArgs {
            namespace: ns(),
            track: Bytes::from_static(b"video"),
            params: Params::default(),
        })
        .unwrap();
    let mut got = Vec::new();
    let mut sent = false;
    w.run(6000, |w, i, e| {
        if i == publisher {
            publisher_logic(w, i, &e, &mut p, vec![]);
            if let (Some(req), false) = (p.sub_req, sent) {
                sent = true;
                let now = w.now;
                for g in 0..5u64 {
                    for o in 0..5u64 {
                        w.c(publisher).send_object(req, &obj(g, o, &vec![(g * 5 + o) as u8; 3000]), now, now).unwrap();
                    }
                    w.c(publisher).end_subgroup(req, g, 0, now);
                }
            }
            return;
        }
        if let SessionEvent::Object { object, .. } = e {
            got.push((object.group, object.id, object.payload.len()));
        }
    });
    assert_eq!(got.len(), 25, "{got:?}");
    assert!(got.iter().all(|x| x.2 == 3000));
}

#[test]
fn subscribe_to_missing_namespace_fails_and_namespace_discovery_works() {
    let mut w = World::new(Arc::new(AllowAll));
    let publisher = w.add_native(SessionConfig::new(Role::Client));
    let watcher = w.add_native(SessionConfig::new(Role::Client));
    let mut p = Pub::default();
    let mut errs = Vec::new();
    let mut seen = Vec::new();
    let mut started = false;
    w.run(1500, |w, i, e| {
        if i == publisher {
            publisher_logic(w, i, &e, &mut p, vec![]);
            return;
        }
        match e {
            SessionEvent::Ready { .. } if !started => {
                started = true;
                w.c(i).subscribe_namespace(Namespace::from_path("live"), Params::default()).unwrap();
                w.c(i)
                    .subscribe(SubscribeArgs {
                        namespace: Namespace::from_path("nobody/here"),
                        track: Bytes::from_static(b"x"),
                        params: Params::default(),
                    })
                    .unwrap();
            }
            SessionEvent::RequestError { code, .. } => errs.push(code),
            SessionEvent::Namespace { suffix, .. } => seen.push(suffix),
            _ => {}
        }
    });
    assert_eq!(errs, vec![RequestErrorCode::DOES_NOT_EXIST.0]);
    assert_eq!(seen, vec![Namespace::from_path("room1")]);
    let _ = watcher;
}

#[test]
fn fuzz_session_inputs_do_not_panic() {
    let mut x: u64 = 0xdead_beef_1234_5678;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let now = Instant::now();
    for round in 0..3000 {
        let mut s = Session::new(SessionConfig::new(if round % 2 == 0 { Role::Server } else { Role::Client }), now);
        // 正しい SETUP を先に入れることもある
        if round % 3 == 0 {
            let setup = message::Message::Setup(params::SetupOptions::default()).encode().unwrap();
            s.on_stream_data(2, false, Bytes::from(setup), false, now);
        }
        for _ in 0..4 {
            let len = (rnd() % 80) as usize;
            let mut d: Vec<u8> = (0..len).map(|_| rnd() as u8).collect();
            // それらしい先頭を混ぜる
            let prefix: &[u8] = match rnd() % 5 {
                0 => &[0x10, 0x01, 0x00],
                1 => &[0x05, 0x00],
                2 => &[0x03, 0x00, 0x20],
                _ => &[],
            };
            d.splice(0..0, prefix.iter().copied());
            let stream = rnd() % 16;
            let bidi = stream % 4 < 2;
            s.on_stream_data(stream, bidi, Bytes::from(d.clone()), rnd() % 3 == 0, now);
            s.on_datagram(Bytes::from(d), now);
        }
        while s.poll_event().is_some() {}
    }
    for _ in 0..20000 {
        let len = (rnd() % 64) as usize;
        let d: Vec<u8> = (0..len).map(|_| rnd() as u8).collect();
        let _ = message::Message::decode_body(rnd() % 0x60, Bytes::from(d.clone()), rnd() % 2 == 0);
        let mut r = data::DataStreamReader::new(1 << 16);
        r.push(&d);
        for _ in 0..8 {
            if r.next_item().is_err() {
                break;
            }
        }
        let _ = data::decode_datagram(Bytes::from(d.clone()));
        let _ = params::parse_kvps(&Bytes::from(d.clone()));
        let _ = mapping::split_fmp4(&d);
        let _ = mapping::parse_catalog(&d);
    }
}
