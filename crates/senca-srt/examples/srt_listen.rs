//! 相互接続試験用：SRT listener で受けた MPEG-TS をファイルへ書く。
//!
//! 使い方: srt_listen --port P --out FILE [--passphrase S] [--latency MS] [--resource R] [--timeout S]
//!
//! `--resource` を付けると、Stream ID が `#!::r=R,m=publish`（または平文の R）のときだけ受け入れ、
//! それ以外は SRT_REJX_FORBIDDEN(1403) で拒否する。最初の接続が閉じたら統計を出して終わる。

use std::io::Write;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

use senca_srt::{
    AcceptDecision, AcceptParams, Config, Event, Listener, ListenerConfig, Passphrase, RejectReason,
};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = arg(&args, "--port").expect("--port").parse().expect("port");
    let out_path = arg(&args, "--out").expect("--out");
    let pass = arg(&args, "--passphrase").map(|p| Passphrase::new(p).expect("passphrase 10..79"));
    let latency: u16 = arg(&args, "--latency").map(|v| v.parse().expect("latency")).unwrap_or(120);
    let resource = arg(&args, "--resource");
    let timeout: u64 = arg(&args, "--timeout").map(|v| v.parse().expect("timeout")).unwrap_or(30);

    let sock = UdpSocket::bind(("127.0.0.1", port)).expect("bind");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&out_path).expect("create out"));
    let cfg = ListenerConfig { conn: Config { latency_ms: latency, ..Config::default() }, max_connections: 4 };
    let mut l = Listener::new(cfg, move |req| {
        if let Some(r) = &resource {
            let ok = req.parsed.is_some_and(|p| p.resource.as_deref() == Some(r.as_str()) && (p.mode.is_none() || p.is_publish()));
            if !ok {
                eprintln!("reject stream id {:?}", req.stream_id);
                return AcceptDecision::Reject(RejectReason::X_FORBIDDEN);
            }
        }
        AcceptDecision::Accept(AcceptParams { passphrase: pass.clone(), ..Default::default() })
    })
    .expect("listener");
    println!("LISTENING {port}");
    std::io::stdout().flush().ok();

    let t0 = Instant::now();
    let now_us = || t0.elapsed().as_micros() as u64;
    let mut buf = [0u8; 2048];
    let mut bytes = 0u64;
    let mut last_activity = Instant::now();
    let mut last_stats = None;
    let mut connected_info = None;
    let mut close_reason = None;
    loop {
        let now = now_us();
        let wait = l.poll_timeout().map(|t| t.saturating_sub(now)).unwrap_or(50_000).clamp(200, 50_000);
        sock.set_read_timeout(Some(Duration::from_micros(wait))).ok();
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                last_activity = Instant::now();
                l.handle_datagram(now_us(), from, &buf[..n]);
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("recv: {e}"),
        }
        l.handle_timeout(now_us());
        while let Some((to, p)) = l.poll_transmit() {
            let _ = sock.send_to(&p, to);
        }
        while let Some(r) = l.poll_rejection() {
            eprintln!("REJECT {} {:?} {}", r.peer, r.stream_id, r.reason);
        }
        for (id, c) in l.connections() {
            let _ = id;
            last_stats = Some(c.stats());
        }
        while let Some((_, ev)) = l.poll_event() {
            match ev {
                Event::Connected(info) => {
                    eprintln!("connected: {info:?}");
                    connected_info = Some(info);
                }
                Event::Data { payload, .. } => {
                    bytes += payload.len() as u64;
                    out.write_all(&payload).expect("write");
                }
                Event::Closed { reason } => close_reason = Some(reason),
            }
        }
        if close_reason.is_some() {
            break;
        }
        if last_activity.elapsed() > Duration::from_secs(timeout) {
            break;
        }
    }
    out.flush().ok();
    let s = last_stats.unwrap_or_default();
    println!(
        "RESULT connected={} rejected={} close={:?} bytes={} encryption={:?} recv={} loss={} retrans_recv={} dropped={} undecrypted={} rtt_us={}",
        connected_info.is_some(),
        l.rejected,
        close_reason,
        bytes,
        connected_info.as_ref().and_then(|i| i.encryption),
        s.pkts_recv,
        s.pkts_rcv_loss,
        s.pkts_rcv_retransmitted,
        s.pkts_rcv_dropped,
        s.pkts_rcv_undecrypted,
        s.rtt_us
    );
}
