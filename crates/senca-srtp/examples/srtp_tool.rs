//! 相互接続試験用の SRTP 道具（std の UDP）。tools/interop/dtls_srtp.sh から使う。
//!
//! srtp_tool capture PORT COUNT TIMEOUT_MS OUT           UDP を受けて 1 行 1 packet の hex で書く
//! srtp_tool unprotect rtp|rtcp SUITE B64 IN OUT         SRTP(SRTCP) を RTP(RTCP) に戻す
//! srtp_tool protect rtp|rtcp SUITE B64 IN OUT           RTP(RTCP) を SRTP(SRTCP) にする
//! srtp_tool send IN HOST:PORT INTERVAL_MS               hex の行を datagram として送る
//!
//! SUITE: AES_CM_128_HMAC_SHA1_80 | AES_CM_128_HMAC_SHA1_32 | AEAD_AES_128_GCM | AEAD_AES_256_GCM。
//! B64: master key || master salt の base64（SDES の inline と同じ）。

use std::io::Write;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

use base64::Engine;
use senca_srtp::{Direction, MasterKey, SrtpContext, SrtpProfile};

fn die(msg: &str) -> ! {
    eprintln!("srtp_tool: {msg}");
    std::process::exit(2)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or_else(|_| die("hex")))
        .collect()
}

fn read_lines(path: &str) -> Vec<Vec<u8>> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|_| die("read"))
        .lines()
        .filter(|l| !l.is_empty())
        .map(unhex)
        .collect()
}

fn context(suite: &str, b64: &str, dir: Direction) -> SrtpContext {
    let profile = match suite {
        "AES_CM_128_HMAC_SHA1_80" => SrtpProfile::Aes128CmHmacSha1_80,
        // SRTP は 32 bit、SRTCP は 80 bit の tag（RFC 5764 4.1.2）。
        "AES_CM_128_HMAC_SHA1_32" => SrtpProfile::Aes128CmHmacSha1_32,
        "AEAD_AES_128_GCM" => SrtpProfile::AeadAes128Gcm,
        "AEAD_AES_256_GCM" => SrtpProfile::AeadAes256Gcm,
        _ => die("suite"),
    };
    let km = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap_or_else(|_| die("base64"));
    let mk = MasterKey::from_concatenated(profile, &km).unwrap_or_else(|| die("key length"));
    SrtpContext::new(profile, &mk, dir).unwrap_or_else(|e| die(&e.to_string()))
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    match a.first().map(String::as_str) {
        Some("capture") if a.len() == 5 => {
            let sock =
                UdpSocket::bind(format!("127.0.0.1:{}", a[1])).unwrap_or_else(|_| die("bind"));
            let count: usize = a[2].parse().unwrap_or_else(|_| die("count"));
            let deadline = Instant::now()
                + Duration::from_millis(a[3].parse().unwrap_or_else(|_| die("timeout")));
            let mut out = std::fs::File::create(&a[4]).unwrap_or_else(|_| die("create"));
            let mut buf = [0u8; 65536];
            let mut n = 0;
            while n < count {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                sock.set_read_timeout(Some(deadline - now)).ok();
                if let Ok(len) = sock.recv(&mut buf) {
                    writeln!(out, "{}", hex(&buf[..len])).ok();
                    n += 1;
                }
            }
            eprintln!("captured {n}");
        }
        Some(op @ ("unprotect" | "protect")) if a.len() == 6 => {
            let rtcp = match a[1].as_str() {
                "rtp" => false,
                "rtcp" => true,
                _ => die("rtp|rtcp"),
            };
            let dir = if op == "protect" {
                Direction::Outbound
            } else {
                Direction::Inbound
            };
            let mut ctx = context(&a[2], &a[3], dir);
            let mut out = std::fs::File::create(&a[5]).unwrap_or_else(|_| die("create"));
            let (mut ok, mut bad) = (0, 0);
            for p in read_lines(&a[4]) {
                let r = match (op, rtcp) {
                    ("protect", false) => ctx.protect_rtp(&p),
                    ("protect", true) => ctx.protect_rtcp(&p),
                    (_, false) => ctx.unprotect_rtp(&p),
                    (_, true) => ctx.unprotect_rtcp(&p),
                };
                match r {
                    Ok(v) => {
                        writeln!(out, "{}", hex(&v)).ok();
                        ok += 1;
                    }
                    Err(e) => {
                        eprintln!("{op}: {e}");
                        bad += 1;
                    }
                }
            }
            println!("ok={ok} failed={bad}");
        }
        Some("send") if a.len() == 4 => {
            let sock = UdpSocket::bind("127.0.0.1:0").unwrap_or_else(|_| die("bind"));
            let interval = Duration::from_millis(a[3].parse().unwrap_or_else(|_| die("interval")));
            for p in read_lines(&a[1]) {
                sock.send_to(&p, &a[2]).ok();
                std::thread::sleep(interval);
            }
        }
        _ => die("usage: see source"),
    }
}
