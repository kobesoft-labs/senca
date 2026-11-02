//! 2 つの ICE エージェントを本物の UDP ソケット（loopback）でつなぐ相互接続試験。
//!
//! `ice_pair [--stun <addr>] [--turn <addr> <user> <pass>] [--relay-only] [--lite-b]`
//!
//! - `--turn` を渡すと両方が TURN（coturn など）で relayed 候補を取る。
//! - `--relay-only` なら相手へ渡す候補を relayed だけにする（経路は TURN を必ず通る）。
//! - `--lite-b` なら B を ICE-lite にする。
//!
//! 繋がったら選んだ pair を `SELECTED <side> <local> -> <remote>` で出し、双方向にデータを送って
//! `DATA ok` を出す。

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use bytes::Bytes;
use senca_ice::{CandidateKind, IceAgent, IceConfig, IceEvent, IceRole, IceState, TurnServerSpec};

struct Side {
    name: &'static str,
    agent: IceAgent,
    sock: UdpSocket,
    received: usize,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cfg = IceConfig {
        software: Some("senca-ice".into()),
        ..Default::default()
    };
    let mut relay_only = false;
    let mut lite_b = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--stun" => {
                cfg.stun_servers.push(args[i + 1].parse()?);
                i += 1;
            }
            "--turn" => {
                cfg.turn_servers.push(TurnServerSpec {
                    server: args[i + 1].parse()?,
                    username: args[i + 2].clone(),
                    password: args[i + 3].clone(),
                });
                i += 3;
            }
            "--relay-only" => relay_only = true,
            "--lite-b" => lite_b = true,
            a => return Err(format!("unknown argument {a}").into()),
        }
        i += 1;
    }
    let now = Instant::now();
    let mut sides = Vec::new();
    for (name, role) in [("A", IceRole::Controlling), ("B", IceRole::Controlled)] {
        let mut c = cfg.clone();
        c.role = role;
        if name == "B" && lite_b {
            c.lite = true;
        }
        let sock = UdpSocket::bind("127.0.0.1:0")?;
        sock.set_nonblocking(true)?;
        let mut agent = IceAgent::new(c)?;
        agent.add_local_socket(now, sock.local_addr()?)?;
        sides.push(Side {
            name,
            agent,
            sock,
            received: 0,
        });
    }
    let ca = sides[0].agent.local_credentials().clone();
    let cb = sides[1].agent.local_credentials().clone();
    sides[0].agent.set_remote_credentials(&cb.ufrag, &cb.pwd)?;
    sides[1].agent.set_remote_credentials(&ca.ufrag, &ca.pwd)?;

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut buf = vec![0u8; 65536];
    let mut data_sent = false;
    let mut selected_printed = [false, false];
    loop {
        let now = Instant::now();
        if now >= deadline {
            println!("TIMEOUT {:?} {:?}", sides[0].agent, sides[1].agent);
            std::process::exit(1);
        }
        for s in 0..2 {
            // 受信
            loop {
                match sides[s].sock.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        let local = sides[s].sock.local_addr()?;
                        if let Some(app) = sides[s].agent.handle_input(
                            Instant::now(),
                            local,
                            from,
                            Bytes::copy_from_slice(&buf[..n]),
                        ) && app.starts_with(b"\x17hello")
                        {
                            sides[s].received += 1;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
            let now = Instant::now();
            if sides[s].agent.poll_timeout().is_some_and(|t| t <= now) {
                sides[s].agent.handle_timeout(now);
            }
            while let Some(t) = sides[s].agent.poll_transmit() {
                let _ = sides[s].sock.send_to(&t.data, t.remote);
            }
            while let Some(e) = sides[s].agent.poll_event() {
                match e {
                    IceEvent::LocalCandidate(c) => {
                        println!("CANDIDATE {} {c}", sides[s].name);
                        if relay_only && c.kind != CandidateKind::Relayed {
                            continue;
                        }
                        let o = 1 - s;
                        sides[o]
                            .agent
                            .add_remote_candidate_str(Instant::now(), &c.to_string())?;
                    }
                    IceEvent::GatheringComplete => {
                        sides[1 - s].agent.end_of_remote_candidates(Instant::now())
                    }
                    IceEvent::SelectedPair { local, remote } => {
                        println!(
                            "SELECTED {} {} {} -> {} {}",
                            sides[s].name,
                            local.kind.as_str(),
                            local.addr().unwrap(),
                            remote.kind.as_str(),
                            remote.addr().unwrap()
                        );
                        selected_printed[s] = true;
                    }
                    IceEvent::StateChanged(st) => println!("STATE {} {st:?}", sides[s].name),
                    IceEvent::RoleChanged(r) => println!("ROLE {} {r:?}", sides[s].name),
                }
            }
        }
        let connected = sides
            .iter()
            .all(|s| matches!(s.agent.state(), IceState::Connected | IceState::Completed));
        if connected && selected_printed.iter().all(|x| *x) && !data_sent {
            data_sent = true;
            for s in sides.iter_mut() {
                for _ in 0..5 {
                    if let Some(t) = s.agent.send(Bytes::from_static(b"\x17hello")) {
                        s.sock.send_to(&t.data, t.remote)?;
                    }
                }
            }
        }
        if data_sent && sides.iter().all(|s| s.received >= 1) {
            println!("DATA ok a={} b={}", sides[0].received, sides[1].received);
            let relayed = sides
                .iter()
                .map(|s| s.agent.selected_pair().map(|(l, _)| l.kind))
                .collect::<Vec<_>>();
            println!("LOCAL_KINDS {relayed:?}");
            let now = Instant::now();
            for s in sides.iter_mut() {
                s.agent.close(now);
                while let Some(t) = s.agent.poll_transmit() {
                    let _ = s.sock.send_to(&t.data, t.remote);
                }
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
