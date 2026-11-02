//! TURN クライアントの相互接続試験（std の UDP）。
//!
//! `turn_relay_check <server> <username> <password> [count]`
//!
//! 1. サーバーに Allocate し、ローカルの peer ソケット（127.0.0.1 の別ソケット、受けたものをそのまま返す）
//!    への permission を作る。
//! 2. Send indication で `count` 個送り、peer が relay アドレスへ返したものを Data indication で受ける。
//! 3. ChannelBind して、ChannelData で同じことをする。
//! 4. Refresh(0) で解放する。
//!
//! 各段の結果を `ALLOCATED <relay>`・`SEND <受けた数>/<count>`・`CHANNEL <受けた数>/<count>`・`CLOSED` で出す。

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use bytes::Bytes;
use senca_turn::{TurnClient, TurnClientConfig, TurnClientEvent, TurnInput};

struct Driver {
    sock: UdpSocket,
    client: TurnClient,
    events: Vec<TurnClientEvent>,
    data: Vec<(SocketAddr, Bytes)>,
}

impl Driver {
    fn flush(&mut self) -> std::io::Result<()> {
        while let Some(t) = self.client.poll_transmit() {
            self.sock.send_to(&t.data, t.remote)?;
        }
        while let Some(e) = self.client.poll_event() {
            println!("EVENT {e:?}");
            self.events.push(e);
        }
        Ok(())
    }

    /// `cond` が真になるか `limit` が過ぎるまで回す。
    fn run(&mut self, limit: Duration, cond: impl Fn(&Driver) -> bool) -> std::io::Result<bool> {
        let deadline = Instant::now() + limit;
        let mut buf = vec![0u8; 65536];
        loop {
            self.flush()?;
            if cond(self) {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            let mut wake = deadline;
            if let Some(t) = self.client.poll_timeout() {
                wake = wake.min(t);
            }
            self.sock.set_read_timeout(Some(
                wake.saturating_duration_since(now)
                    .max(Duration::from_millis(1)),
            ))?;
            match self.sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if let TurnInput::Data { peer, data } = self.client.handle_input(
                        Instant::now(),
                        from,
                        Bytes::copy_from_slice(&buf[..n]),
                    ) {
                        self.data.push((peer, data));
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => return Err(e),
            }
            let now = Instant::now();
            if self.client.poll_timeout().is_some_and(|t| t <= now) {
                self.client.handle_timeout(now);
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        return Err("usage: turn_relay_check <server> <username> <password> [count]".into());
    }
    let server: SocketAddr = args[0].parse()?;
    let count: usize = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(20);
    let bind = if server.is_ipv4() {
        "127.0.0.1:0"
    } else {
        "[::1]:0"
    };
    let sock = UdpSocket::bind(bind)?;
    let local = sock.local_addr()?;

    // peer: 受けたものを送り元（relay アドレス）へ返す
    let peer_sock = UdpSocket::bind(bind)?;
    let peer = peer_sock.local_addr()?;
    std::thread::spawn(move || {
        let mut b = vec![0u8; 65536];
        while let Ok((n, from)) = peer_sock.recv_from(&mut b) {
            let _ = peer_sock.send_to(&b[..n], from);
        }
    });

    let mut cfg = TurnClientConfig::new(local, server, &args[1], &args[2]);
    cfg.software = Some("senca-turn".into());
    let mut d = Driver {
        sock,
        client: TurnClient::new(cfg),
        events: Vec::new(),
        data: Vec::new(),
    };
    d.client.allocate(Instant::now())?;
    let ok = d.run(Duration::from_secs(10), |d| {
        d.events.iter().any(|e| {
            matches!(
                e,
                TurnClientEvent::Allocated { .. } | TurnClientEvent::AllocationFailed { .. }
            )
        })
    })?;
    let Some(relayed) = d.client.relayed_address() else {
        println!("FAILED allocate ({ok})");
        std::process::exit(1);
    };
    println!("ALLOCATED {relayed}");

    d.client.create_permission(Instant::now(), peer.ip())?;
    d.run(Duration::from_secs(5), |d| {
        d.client.has_permission(peer.ip())
    })?;

    let mut results = Vec::new();
    for phase in ["SEND", "CHANNEL"] {
        if phase == "CHANNEL" {
            d.client.channel_bind(Instant::now(), peer)?;
            d.run(Duration::from_secs(5), |d| d.client.channel(peer).is_some())?;
        }
        d.data.clear();
        for i in 0..count {
            let msg = format!("{phase}-{i}");
            if let Some(t) = d.client.send(peer, msg.as_bytes()) {
                d.sock.send_to(&t.data, t.remote)?;
            }
        }
        d.run(Duration::from_secs(3), |d| d.data.len() >= count)?;
        let good = d
            .data
            .iter()
            .filter(|(p, b)| *p == peer && b.starts_with(phase.as_bytes()))
            .count();
        println!("{phase} {good}/{count}");
        results.push(good == count);
    }

    d.client.deallocate(Instant::now())?;
    if d.run(Duration::from_secs(5), |d| {
        d.events.contains(&TurnClientEvent::Closed)
    })? {
        println!("CLOSED");
    }
    if results.iter().all(|x| *x) {
        Ok(())
    } else {
        std::process::exit(1)
    }
}
