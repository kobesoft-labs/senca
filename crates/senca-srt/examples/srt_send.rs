//! 相互接続試験用：SRT caller として MPEG-TS ファイルを送る。
//!
//! 使い方: srt_send --to HOST:PORT --in FILE [--passphrase S] [--pbkeylen 16|24|32] [--gcm]
//!                  [--streamid SID] [--latency MS] [--duration S] [--km-refresh N]
//!
//! 1316 バイト（7×188）ずつ、`--duration` 秒で送り終わる一定の間隔で送る。送り終えたら閉じ、
//! 未確認の分を送り切ってから終わる。

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use senca_srt::{CipherMode, CloseReason, Config, Connection, Event, KeyLen, Passphrase};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let to: SocketAddr = arg(&args, "--to").expect("--to").parse().expect("addr");
    let data = std::fs::read(arg(&args, "--in").expect("--in")).expect("read input");
    let duration: f64 = arg(&args, "--duration").map(|v| v.parse().expect("duration")).unwrap_or(5.0);
    let key_len = match arg(&args, "--pbkeylen").as_deref() {
        Some("24") => KeyLen::Aes192,
        Some("32") => KeyLen::Aes256,
        _ => KeyLen::Aes128,
    };
    let cfg = Config {
        passphrase: arg(&args, "--passphrase").map(|p| Passphrase::new(p).expect("passphrase 10..79")),
        key_len,
        cipher: if args.iter().any(|a| a == "--gcm") { CipherMode::AesGcm } else { CipherMode::AesCtr },
        stream_id: arg(&args, "--streamid"),
        latency_ms: arg(&args, "--latency").map(|v| v.parse().expect("latency")).unwrap_or(120),
        km_refresh_rate: arg(&args, "--km-refresh").map(|v| v.parse().expect("km")).unwrap_or(1 << 24),
        km_preannounce: arg(&args, "--km-refresh").map(|v| v.parse::<u64>().expect("km") / 4).unwrap_or(1 << 12),
        linger_ms: 3000,
        ..Config::default()
    };
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let t0 = Instant::now();
    let now_us = || t0.elapsed().as_micros() as u64;
    let mut c = Connection::connect(cfg, to, now_us()).expect("connect");
    let chunks: Vec<&[u8]> = data.chunks(1316).collect();
    let interval = (duration * 1e6 / chunks.len().max(1) as f64) as u64;
    let mut next_idx = 0usize;
    let mut send_start = None;
    let mut closing = false;
    let mut result: Option<CloseReason> = None;
    let mut buf = [0u8; 2048];
    loop {
        let now = now_us();
        if c.is_connected() && !closing {
            let start = *send_start.get_or_insert(now);
            while next_idx < chunks.len() && start + next_idx as u64 * interval <= now {
                if let Err(e) = c.send(chunks[next_idx], now) {
                    eprintln!("send error: {e}");
                    break;
                }
                next_idx += 1;
            }
            if next_idx >= chunks.len() {
                c.close(now);
                closing = true;
            }
        }
        while let Some((dst, p)) = c.poll_transmit() {
            let _ = sock.send_to(&p, dst);
        }
        while let Some(ev) = c.poll_event() {
            match ev {
                Event::Connected(info) => eprintln!("connected: {info:?}"),
                Event::Data { .. } => {}
                Event::Closed { reason } => result = Some(reason),
            }
        }
        if let Some(r) = result {
            let s = c.stats();
            println!(
                "RESULT close={:?} sent_chunks={} pkts_sent={} retrans={} snd_loss={} snd_dropped={} rtt_us={}",
                r, next_idx, s.pkts_sent, s.pkts_retransmitted, s.pkts_snd_loss, s.pkts_snd_dropped, s.rtt_us
            );
            std::process::exit(if r == CloseReason::Local { 0 } else { 1 });
        }
        let now = now_us();
        let mut wait = c.poll_timeout().map(|t| t.saturating_sub(now)).unwrap_or(10_000);
        if c.is_connected() && !closing && send_start.is_some() {
            let due = send_start.unwrap_or(now) + next_idx as u64 * interval;
            wait = wait.min(due.saturating_sub(now));
        }
        sock.set_read_timeout(Some(Duration::from_micros(wait.clamp(200, 20_000)))).ok();
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if std::env::var("SRT_DEBUG").is_ok()
                    && let Ok(senca_srt::packet::Packet::Control(h, cif)) = senca_srt::packet::parse(&buf[..n])
                {
                    eprintln!("{} ctrl {:?} info={} len={} {:02x?}", now_us(), h.ctype, h.info, cif.len(), &cif[..cif.len().min(16)]);
                }
                c.handle_datagram(now_us(), from, &buf[..n])
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("recv: {e}"),
        }
        c.handle_timeout(now_us());
    }
}
