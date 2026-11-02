//! 試験用の TURN サーバー（std の UDP ソケット）。相互接続試験（tools/interop/ice_turn.sh）で使う。
//!
//! ```text
//! turn_server --listen 127.0.0.1:3478 --relay-ip 127.0.0.1 --ports 46100-46199 \
//!     [--user u:p]... [--secret S] [--realm r] [--allow-peer CIDR]...
//! ```
//!
//! 準備ができたら標準出力に `LISTENING <addr>` を出す。止めるまで動く。

use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use senca_turn::server::{RateLimit, RelayIp, RestSecret, StaticUser};
use senca_turn::{PeerPolicy, TurnServer, TurnServerConfig, TurnServerEvent};

enum Input {
    Client(SocketAddr, Bytes),
    Peer(SocketAddr, SocketAddr, Bytes),
}

fn usage() -> ! {
    eprintln!(
        "usage: turn_server --listen ADDR --relay-ip IP --ports MIN-MAX [--user u:p]... [--secret S] [--realm R] [--allow-peer CIDR]..."
    );
    std::process::exit(2)
}

fn reader(
    sock: Arc<UdpSocket>,
    stop: Arc<AtomicBool>,
    tx: mpsc::Sender<Input>,
    relay: Option<SocketAddr>,
) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        while !stop.load(Ordering::Relaxed) {
            match sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let data = Bytes::copy_from_slice(&buf[..n]);
                    let msg = match relay {
                        Some(r) => Input::Peer(r, from, data),
                        None => Input::Client(from, data),
                    };
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => {}
            }
        }
    });
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut listen: Option<SocketAddr> = None;
    let mut relay_ip: Option<IpAddr> = None;
    let mut ports = (49152u16, 65535u16);
    let mut cfg = TurnServerConfig {
        software: Some("senca-turn example".into()),
        unauth_rate: RateLimit {
            burst: 1000,
            per_second: 1000,
            max_entries: 10_000,
        },
        peer_policy: PeerPolicy::strict(),
        ..Default::default()
    };
    while let Some(a) = args.next() {
        let v = args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--listen" => listen = Some(v.parse().unwrap_or_else(|_| usage())),
            "--relay-ip" => relay_ip = Some(v.parse().unwrap_or_else(|_| usage())),
            "--ports" => {
                let (a, b) = v.split_once('-').unwrap_or_else(|| usage());
                ports = (
                    a.parse().unwrap_or_else(|_| usage()),
                    b.parse().unwrap_or_else(|_| usage()),
                );
            }
            "--user" => {
                let (u, p) = v.split_once(':').unwrap_or_else(|| usage());
                cfg.users.push(StaticUser {
                    username: u.into(),
                    password: p.into(),
                    tenant: None,
                });
            }
            "--secret" => cfg.rest_secrets.push(RestSecret {
                secret: v.into_bytes(),
                tenant: None,
            }),
            "--realm" => cfg.realm = v,
            "--allow-peer" => cfg
                .peer_policy
                .allow
                .push(v.parse().unwrap_or_else(|_| usage())),
            _ => usage(),
        }
    }
    let (Some(listen), Some(relay_ip)) = (listen, relay_ip) else {
        usage()
    };
    cfg.port_min = ports.0;
    cfg.port_max = ports.1;
    let relay = RelayIp {
        local: relay_ip,
        external: None,
    };
    if relay_ip.is_ipv4() {
        cfg.relay_v4 = Some(relay);
    } else {
        cfg.relay_v6 = Some(relay);
    }
    let mut server = TurnServer::new(cfg).expect("random source");

    let (tx, rx) = mpsc::channel();
    let main_sock = Arc::new(UdpSocket::bind(listen).expect("bind listen"));
    main_sock
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("timeout");
    let listen = main_sock.local_addr().expect("local addr");
    reader(
        main_sock.clone(),
        Arc::new(AtomicBool::new(false)),
        tx.clone(),
        None,
    );
    let mut sockets: HashMap<SocketAddr, (Arc<UdpSocket>, Arc<AtomicBool>)> = HashMap::new();

    println!("LISTENING {listen}");
    std::io::stdout().flush().ok();

    loop {
        server.set_unix_time(unix_now());
        let now = Instant::now();
        let wait = server
            .poll_timeout()
            .map_or(Duration::from_millis(500), |t| {
                t.saturating_duration_since(now)
            });
        match rx.recv_timeout(wait.min(Duration::from_millis(500))) {
            Ok(Input::Client(from, data)) => {
                server.handle_client(Instant::now(), listen, from, data)
            }
            Ok(Input::Peer(relay, from, data)) => {
                server.handle_peer(Instant::now(), relay, from, data)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        server.handle_timeout(Instant::now());
        while let Some(ev) = server.poll_event() {
            match ev {
                TurnServerEvent::AllocationCreated { relay, .. } => match UdpSocket::bind(relay) {
                    Ok(s) => {
                        s.set_read_timeout(Some(Duration::from_millis(200))).ok();
                        let s = Arc::new(s);
                        let stop = Arc::new(AtomicBool::new(false));
                        reader(s.clone(), stop.clone(), tx.clone(), Some(relay));
                        sockets.insert(relay, (s, stop));
                    }
                    Err(e) => {
                        eprintln!("relay bind {relay}: {e}");
                        server.relay_unavailable(Instant::now(), relay);
                    }
                },
                TurnServerEvent::AllocationClosed { relay } => {
                    if let Some((_, stop)) = sockets.remove(&relay) {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        while let Some(t) = server.poll_transmit() {
            let sock = if t.local == listen {
                Some(&main_sock)
            } else {
                sockets.get(&t.local).map(|s| &s.0)
            };
            if let Some(s) = sock {
                let _ = s.send_to(&t.data, t.remote);
            }
        }
    }
}
