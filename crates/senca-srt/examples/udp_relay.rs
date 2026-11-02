//! 相互接続試験用：損失と遅延を入れる UDP 中継（1 つの相手だけ）。
//!
//! 使い方: udp_relay --listen PORT --to HOST:PORT [--loss PCT] [--delay MS] [--seed N] [--idle S]
//!
//! `--listen` に最初に届いた送信元を相手とし、`--to` との間で両方向に中継する。各データグラムを
//! `--loss` % の確率で捨て、`--delay` ms 遅らせる。`--idle` 秒なにも届かなければ件数を出して終わる。

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

struct Rng(u64);

impl Rng {
    fn f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = arg(&args, "--listen").expect("--listen").parse().expect("port");
    let upstream: SocketAddr = arg(&args, "--to").expect("--to").parse().expect("addr");
    let loss: f64 = arg(&args, "--loss").map(|v| v.parse().expect("loss")).unwrap_or(0.0) / 100.0;
    let delay = Duration::from_millis(arg(&args, "--delay").map(|v| v.parse().expect("delay")).unwrap_or(0));
    let idle = Duration::from_secs(arg(&args, "--idle").map(|v| v.parse().expect("idle")).unwrap_or(10));
    let mut rng = Rng(arg(&args, "--seed").map(|v| v.parse().expect("seed")).unwrap_or(1) | 1);

    let front = UdpSocket::bind(("127.0.0.1", port)).expect("bind front");
    let back = UdpSocket::bind("127.0.0.1:0").expect("bind back");
    front.set_nonblocking(true).ok();
    back.set_nonblocking(true).ok();
    println!("RELAY {port} -> {upstream}");

    let mut client: Option<SocketAddr> = None;
    // (時刻, 上り?, データ)
    let mut queue: VecDeque<(Instant, bool, Vec<u8>)> = VecDeque::new();
    let (mut fwd, mut dropped) = ([0u64; 2], [0u64; 2]);
    let mut last = Instant::now();
    let mut buf = [0u8; 2048];
    loop {
        let mut got = false;
        while let Ok((n, from)) = front.recv_from(&mut buf) {
            client.get_or_insert(from);
            got = true;
            if rng.f64() < loss {
                dropped[0] += 1;
            } else {
                queue.push_back((Instant::now() + delay, true, buf[..n].to_vec()));
            }
        }
        while let Ok((n, _)) = back.recv_from(&mut buf) {
            got = true;
            if rng.f64() < loss {
                dropped[1] += 1;
            } else {
                queue.push_back((Instant::now() + delay, false, buf[..n].to_vec()));
            }
        }
        let now = Instant::now();
        while queue.front().is_some_and(|(t, _, _)| *t <= now) {
            let (_, up, d) = queue.pop_front().unwrap_or((now, true, Vec::new()));
            if up {
                let _ = back.send_to(&d, upstream);
                fwd[0] += 1;
            } else if let Some(c) = client {
                let _ = front.send_to(&d, c);
                fwd[1] += 1;
            }
        }
        if got {
            last = now;
        } else if last.elapsed() > idle && queue.is_empty() {
            break;
        } else {
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    println!("RELAY_RESULT up_fwd={} up_drop={} down_fwd={} down_drop={}", fwd[0], dropped[0], fwd[1], dropped[1]);
}
