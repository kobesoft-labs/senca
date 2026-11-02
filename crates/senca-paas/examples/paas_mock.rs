//! 模擬 API（`senca_paas::mock`）を HTTP で立てる。`tools/interop/paas.sh` が curl（別の SigV4 実装）で叩く。
//!
//! 使い方: `paas_mock <port>`。資格情報は環境変数（既定値は試験用の固定値）:
//! `MOCK_CF_ACCOUNT`・`MOCK_CF_TOKEN`・`MOCK_AWS_KEY_ID`・`MOCK_AWS_SECRET`・`MOCK_AWS_REGION`。
//! 標準出力に `listening <addr>` を出し、要求ごとに `METHOD path status` を標準エラーへ出す（秘密は出さない）。

use senca_http::{Event, Limits, RequestParser};
use senca_paas::mock::{MockCloud, encode_reply};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

fn serve(mut s: TcpStream, cloud: &mut MockCloud) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut p = RequestParser::new(Limits::DEFAULT);
    let mut buf = [0u8; 16 * 1024];
    let (mut head, mut body) = (None, Vec::new());
    loop {
        match p.poll() {
            Ok(Event::NeedMore) => {
                let n = s.read(&mut buf)?;
                if n == 0 {
                    p.feed_eof();
                } else if p.feed(&buf[..n]).is_err() {
                    return Ok(());
                }
            }
            Ok(Event::Head(h)) => {
                if h.expect_continue {
                    s.write_all(senca_http::CONTINUE_100)?;
                }
                head = Some(h)
            }
            Ok(Event::Data(d)) => body.extend_from_slice(&d),
            Ok(Event::End(_)) => break,
            Ok(Event::Closed) | Err(_) => return Ok(()),
        }
    }
    let Some(h) = head else { return Ok(()) };
    let headers: Vec<(String, String)> = h.headers.iter().map(|(n, v)| (n.to_string(), String::from_utf8_lossy(v).into_owned())).collect();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let reply = cloud.handle(h.method.as_str(), &h.target, &headers, &body, now);
    eprintln!("{} {} {}", h.method, h.path(), reply.status);
    s.write_all(&encode_reply(&reply))
}

fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|p| p.parse().ok()).unwrap_or(0);
    let mut cloud = MockCloud::new(
        &env("MOCK_CF_ACCOUNT", "0123456789abcdef0123456789abcdef"),
        &env("MOCK_CF_TOKEN", "cf-mock-token"),
        &env("MOCK_AWS_KEY_ID", "AKIAMOCK0000000001"),
        &env("MOCK_AWS_SECRET", "mock/secret+0000000000000000000000000000"),
        &env("MOCK_AWS_REGION", "ap-northeast-1"),
    );
    let l = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    println!("listening {}", l.local_addr().expect("addr"));
    for s in l.incoming().flatten() {
        if let Err(e) = serve(s, &mut cloud) {
            eprintln!("connection error: {e}");
        }
    }
}
