//! MoQ（draft-22）の相互接続試験用の relay・publisher・subscriber（std の同期 UDP、1 スレッド）。
//!
//! ```text
//! moq_interop relay     --listen 127.0.0.1:51020 --cert chain.pem --key key.pem [--wt-path /moq] [--allow-missing-wt-protocol]
//! moq_interop publish   --connect 127.0.0.1:51020 (--ca ca.pem | --pin-spki HEX) --namespace live/demo
//!                       [--fmp4 file.mp4] [--objects N] [--wt] [--path /moq] [--seconds S]
//! moq_interop subscribe --connect 127.0.0.1:51020 (--ca ca.pem | --pin-spki HEX) --namespace live/demo --track video
//!                       [--count N] [--out FILE] [--fetch G0:G1] [--wt] [--path /moq] [--seconds S]
//! ```
//!
//! publish は `catalog.json`（hang の catalog、CMAF）と `video` track を出す。`--fmp4` があればその
//! fragment を GOP ごとの group にして送り、無ければ `--objects` 個の合成の object を送る。
//! subscribe は受けた object の payload を順に `--out` に書き、統計を標準出力に JSON で出す。

use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::{Bytes, BytesMut};
use senca_moq::auth::AllowAll;
use senca_moq::mapping::{CATALOG_TRACK, GroupMapper, MediaKind, Rendition, catalog_json, split_fmp4};
use senca_moq::params::{GroupOrder, LocationFilter, Params};
use senca_moq::*;
use senca_quic::{ClientConfig, Endpoint, EndpointConfig, ServerConfig, Transmit};
use senca_tls::{CertifiedKey, RootStore, ServerVerifier};

fn arg(args: &[String], k: &str) -> Option<String> {
    args.iter().position(|a| a == k).and_then(|i| args.get(i + 1).cloned())
}
fn flag(args: &[String], k: &str) -> bool {
    args.iter().any(|a| a == k)
}
fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// 受信を一回まわす。
fn recv(sock: &UdpSocket, wait: Duration, buf: &mut [u8], mut f: impl FnMut(SocketAddr, &[u8])) {
    sock.set_read_timeout(Some(wait.clamp(Duration::from_micros(200), Duration::from_millis(20)))).ok();
    if let Ok((n, from)) = sock.recv_from(buf) {
        f(from, &buf[..n]);
        sock.set_nonblocking(true).ok();
        while let Ok((n, from)) = sock.recv_from(buf) {
            f(from, &buf[..n]);
        }
        sock.set_nonblocking(false).ok();
    }
}

fn send_all(sock: &UdpSocket, mut next: impl FnMut() -> Option<Transmit>) {
    while let Some(t) = next() {
        let _ = sock.send_to(&t.contents, t.destination);
    }
}

fn relay(args: &[String]) {
    let listen: SocketAddr = arg(args, "--listen").unwrap_or("127.0.0.1:51020".into()).parse().unwrap();
    let cert = CertifiedKey::from_pem_files(arg(args, "--cert").unwrap(), arg(args, "--key").unwrap()).unwrap();
    let mut tls = senca_tls::ServerConfig::new(Arc::new(cert)).unwrap();
    tls.alpn_protocols = vec![ALPN.to_vec(), senca_h3::ALPN.to_vec()];
    let ep = Endpoint::new(EndpointConfig::new().unwrap(), Some(Arc::new(ServerConfig::new(Arc::new(tls)))));
    let mut server = MoqServer::new(ep, Relay::new(RelayConfig::new(Arc::new(AllowAll))), &arg(args, "--wt-path").unwrap_or("/moq".into()));
    server.allow_missing_wt_protocol = flag(args, "--allow-missing-wt-protocol");
    let sock = UdpSocket::bind(listen).unwrap();
    eprintln!("moq relay (draft-22, ALPN moqt-22 / h3+WebTransport) on {listen}");
    let mut buf = vec![0u8; 65536];
    let mut last_stats = Instant::now();
    loop {
        let now = Instant::now();
        let wait = server.poll_timeout().map_or(Duration::from_millis(20), |t| t.saturating_duration_since(now));
        recv(&sock, wait, &mut buf, |from, d| {
            server.handle_datagram(Instant::now(), SystemTime::now(), from, BytesMut::from(d));
        });
        server.handle_timeout(Instant::now());
        send_all(&sock, || server.poll_transmit(Instant::now()));
        if last_stats.elapsed() > Duration::from_secs(2) {
            last_stats = Instant::now();
            eprintln!("stats {:?}", server.relay.stats());
        }
    }
}

fn client(args: &[String]) -> MoqClient {
    let remote: SocketAddr = arg(args, "--connect").unwrap().parse().unwrap();
    let verifier = if let Some(p) = arg(args, "--pin-spki") {
        ServerVerifier::PinnedSpkiOnly(vec![hex(&p).try_into().unwrap()])
    } else {
        let mut r = RootStore::new();
        r.add_pem_file(arg(args, "--ca").unwrap()).unwrap();
        ServerVerifier::WebPki {
            roots: Arc::new(r),
            spki_pins: vec![],
        }
    };
    let wt = flag(args, "--wt");
    let mut tls = senca_tls::ClientConfig::new(verifier);
    tls.alpn_protocols = vec![if wt { senca_h3::ALPN.to_vec() } else { ALPN.to_vec() }];
    let quic = ClientConfig::new(Arc::new(tls));
    let ep = Endpoint::new(EndpointConfig::new().unwrap(), None);
    let name = arg(args, "--name").unwrap_or("localhost".into());
    let path = arg(args, "--path").unwrap_or(if wt { "/moq".into() } else { "/".into() });
    let mut cfg = SessionConfig::new(Role::Client);
    cfg.path = Some(path.clone());
    cfg.authority = Some(format!("{name}:{}", remote.port()));
    if wt {
        MoqClient::connect_webtransport(ep, quic, remote, &name, &format!("{name}:{}", remote.port()), &path, cfg, Instant::now(), SystemTime::now()).unwrap()
    } else {
        MoqClient::connect_native(ep, quic, remote, &name, cfg, Instant::now(), SystemTime::now()).unwrap()
    }
}

fn run_client(sock: &UdpSocket, c: &mut MoqClient, buf: &mut [u8]) {
    let now = Instant::now();
    let wait = c.poll_timeout().map_or(Duration::from_millis(10), |t| t.saturating_duration_since(now)).min(Duration::from_millis(10));
    recv(sock, wait, buf, |from, d| c.handle_datagram(Instant::now(), SystemTime::now(), from, BytesMut::from(d)));
    c.handle_timeout(Instant::now());
}

fn publish(args: &[String]) -> i32 {
    let ns = Namespace::from_path(&arg(args, "--namespace").unwrap_or("live/demo".into()));
    let seconds: u64 = arg(args, "--seconds").unwrap_or("20".into()).parse().unwrap();
    // 送るもの
    let (init, frags) = match arg(args, "--fmp4") {
        Some(f) => {
            let data = std::fs::read(f).unwrap();
            split_fmp4(&data).unwrap()
        }
        None => {
            let n: usize = arg(args, "--objects").unwrap_or("30".into()).parse().unwrap();
            let frags = (0..n)
                .map(|i| (Bytes::from(format!("synthetic-object-{i:04}-").repeat(50)), i % 10 == 0))
                .collect();
            (Bytes::from_static(b"no-init"), frags)
        }
    };
    let catalog = catalog_json(&[Rendition {
        track: "video".into(),
        kind: MediaKind::Video {
            codec: arg(args, "--codec").unwrap_or("avc1.64001f".into()),
            width: 640,
            height: 360,
            framerate: Some(30.0),
        },
        init,
        bitrate: None,
    }]);
    let mut mapper = GroupMapper::new();
    let objects: Vec<Object> = frags.into_iter().filter_map(|(b, k)| mapper.push(b, k, 128)).collect();
    eprintln!("publishing {} objects in {} groups", objects.len(), mapper.current_group().map_or(0, |g| g + 1));
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut c = client(args);
    let mut buf = vec![0u8; 65536];
    let start = Instant::now();
    let mut announced = false;
    let mut subs: Vec<(u64, bool)> = Vec::new(); // (request, catalog?)
    while start.elapsed() < Duration::from_secs(seconds) {
        run_client(&sock, &mut c, &mut buf);
        let now = Instant::now();
        while let Some(e) = c.session.poll_event() {
            match e {
                SessionEvent::Ready { peer } => {
                    eprintln!("ready, peer implementation {:?}", peer.implementation);
                    let _ = c.session.publish_namespace(ns.clone(), Params::default());
                }
                SessionEvent::RequestOk { .. } if !announced => {
                    announced = true;
                    eprintln!("namespace announced");
                }
                SessionEvent::RequestError { code, reason, .. } => {
                    eprintln!("request error {code:#x} {reason}");
                    return 1;
                }
                SessionEvent::Subscribe { request_id, namespace, track, .. } => {
                    let name = String::from_utf8_lossy(&track).to_string();
                    eprintln!("subscribe {} {}", namespace.render(), name);
                    let is_catalog = name == CATALOG_TRACK;
                    if !is_catalog && name != "video" {
                        c.session.request_error(request_id, RequestErrorCode::DOES_NOT_EXIST, "no such track");
                        continue;
                    }
                    let largest = if is_catalog {
                        Some(Location::new(0, 0))
                    } else {
                        objects.last().map(|o| o.location())
                    };
                    let _ = c.session.subscribe_ok(request_id, largest, Bytes::new());
                    subs.push((request_id, is_catalog));
                    if is_catalog {
                        let o = Object {
                            group: 0,
                            subgroup: Some(0),
                            id: 0,
                            priority: 0,
                            status: ObjectStatus::Normal,
                            properties: Bytes::new(),
                            payload: Bytes::from(catalog.clone()),
                        };
                        let _ = c.session.send_object(request_id, &o, now, now);
                        c.session.end_subgroup(request_id, 0, 0, now);
                    } else {
                        let mut last_group = None;
                        for o in &objects {
                            if let Some(g) = last_group
                                && g != o.group
                            {
                                c.session.end_subgroup(request_id, g, 0, now);
                            }
                            last_group = Some(o.group);
                            let _ = c.session.send_object(request_id, o, now, now);
                        }
                        if let Some(g) = last_group {
                            c.session.end_subgroup(request_id, g, 0, now);
                        }
                    }
                }
                SessionEvent::Fetch { request_id, track, params, .. } => {
                    let name = String::from_utf8_lossy(&track).to_string();
                    let list: Vec<Object> = if name == CATALOG_TRACK {
                        vec![Object {
                            group: 0,
                            subgroup: Some(0),
                            id: 0,
                            priority: 0,
                            status: ObjectStatus::Normal,
                            properties: Bytes::new(),
                            payload: Bytes::from(catalog.clone()),
                        }]
                    } else if name == "video" {
                        let start = filter_start(params.location_filter.as_ref(), None);
                        let end = filter_end(params.location_filter.as_ref()).unwrap_or(Location::new(u64::MAX, u64::MAX));
                        objects.iter().filter(|o| o.location() >= start && o.location() <= end).cloned().collect()
                    } else {
                        c.session.request_error(request_id, RequestErrorCode::DOES_NOT_EXIST, "no such track");
                        continue;
                    };
                    let Some(end) = list.last().map(|o| o.location()) else {
                        c.session.request_error(request_id, RequestErrorCode::INVALID_RANGE, "empty range");
                        continue;
                    };
                    let _ = c.session.fetch_ok(request_id, false, end, Bytes::new());
                    for o in &list {
                        c.session.fetch_object(request_id, o);
                    }
                    c.session.fetch_finish(request_id);
                }
                SessionEvent::Closed { error } => {
                    eprintln!("closed {error:?}");
                    return if announced { 0 } else { 1 };
                }
                _ => {}
            }
        }
        c.drive(Instant::now());
        send_all(&sock, || c.poll_transmit(Instant::now()));
    }
    println!("{{\"announced\": {announced}, \"subscriptions\": {}}}", subs.len());
    if announced { 0 } else { 1 }
}

fn subscribe(args: &[String]) -> i32 {
    let ns = Namespace::from_path(&arg(args, "--namespace").unwrap_or("live/demo".into()));
    let track = arg(args, "--track").unwrap_or("video".into());
    let count: usize = arg(args, "--count").unwrap_or("0".into()).parse().unwrap();
    let seconds: u64 = arg(args, "--seconds").unwrap_or("20".into()).parse().unwrap();
    let fetch = arg(args, "--fetch");
    let mut out = arg(args, "--out").map(|p| std::fs::File::create(p).unwrap());
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut c = client(args);
    let mut buf = vec![0u8; 65536];
    let start = Instant::now();
    let mut got = 0usize;
    let mut bytes = 0usize;
    let mut groups = std::collections::BTreeSet::new();
    let mut ok = false;
    let mut done = false;
    let mut err = None;
    let mut impl_name = None;
    while start.elapsed() < Duration::from_secs(seconds) && !done {
        run_client(&sock, &mut c, &mut buf);
        while let Some(e) = c.session.poll_event() {
            match e {
                SessionEvent::Ready { peer } => {
                    impl_name = peer.implementation.clone();
                    let r = match &fetch {
                        Some(range) => {
                            let (a, b) = range.split_once(':').unwrap_or((range, range));
                            let (a, b): (u64, u64) = (a.parse().unwrap(), b.parse().unwrap());
                            c.session.fetch(
                                ns.clone(),
                                Bytes::copy_from_slice(track.as_bytes()),
                                Params {
                                    location_filter: Some(LocationFilter::AbsoluteRange {
                                        start: Location::new(a, 0),
                                        end_group: b,
                                        end_object: None,
                                    }),
                                    group_order: Some(GroupOrder::Ascending),
                                    ..Default::default()
                                },
                            )
                        }
                        None => c.session.subscribe(SubscribeArgs {
                            namespace: ns.clone(),
                            track: Bytes::copy_from_slice(track.as_bytes()),
                            params: Params {
                                location_filter: Some(LocationFilter::AbsoluteStart(Location::new(0, 0))),
                                ..Default::default()
                            },
                        }),
                    };
                    if let Err(e) = r {
                        eprintln!("request failed: {e}");
                    }
                }
                SessionEvent::SubscribeOk { .. } | SessionEvent::FetchOk { .. } => ok = true,
                SessionEvent::RequestError { code, reason, .. } => {
                    err = Some(format!("{code:#x} {reason}"));
                    done = true;
                }
                SessionEvent::Object { object, delivery, .. } => {
                    if flag(args, "--verbose") {
                        eprintln!(
                            "object {}/{:?}/{} status={:?} len={} props={} via {:?}",
                            object.group,
                            object.subgroup,
                            object.id,
                            object.status,
                            object.payload.len(),
                            object.properties.len(),
                            delivery
                        );
                    }
                    got += 1;
                    bytes += object.payload.len();
                    groups.insert(object.group);
                    if let Some(f) = out.as_mut() {
                        f.write_all(&object.payload).unwrap();
                    }
                    if count > 0 && got >= count {
                        done = true;
                    }
                }
                SessionEvent::FetchComplete { .. } | SessionEvent::PublishDone { .. } => done = true,
                SessionEvent::Closed { error } => {
                    err = Some(format!("closed {error:?}"));
                    done = true;
                }
                _ => {}
            }
        }
        c.drive(Instant::now());
        send_all(&sock, || c.poll_transmit(Instant::now()));
    }
    // 少し流して ACK を返す
    for _ in 0..5 {
        run_client(&sock, &mut c, &mut buf);
        send_all(&sock, || c.poll_transmit(Instant::now()));
    }
    println!(
        "{{\"ok\": {ok}, \"objects\": {got}, \"bytes\": {bytes}, \"groups\": {}, \"peer\": {:?}, \"error\": {:?}}}",
        groups.len(),
        impl_name.unwrap_or_default(),
        err.unwrap_or_default()
    );
    if ok && (count == 0 || got >= count) { 0 } else { 1 }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("relay") => {
            relay(&args);
            0
        }
        Some("publish") => publish(&args),
        Some("subscribe") => subscribe(&args),
        _ => {
            eprintln!("usage: moq_interop relay|publish|subscribe ...");
            2
        }
    };
    std::process::exit(code);
}
