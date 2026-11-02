//! sans-io の厳格な HTTP/1.1（RFC 9110・9112）。I/O・時計・TLS を持たない。
//!
//! - [`request`] — 要求の逐次解析（サーバー）と生成（クライアント）
//! - [`response`] — 応答の生成（head と body を分けて返す）と逐次解析（クライアント）、chunked の符号化
//! - [`headers`] — 大文字小文字を区別しない順序付き header map
//! - [`pct`]・[`router`] — path の厳格な復号、query、path pattern
//! - [`range`]・[`etag`]・[`auth`] — Range・ETag 条件・Bearer
//! - [`sse`] — Server-Sent Events の frame
//!
//! 拒否するもの: obs-fold、裸の CR/LF、NUL、Content-Length と Transfer-Encoding の併用、
//! Content-Length の重複、chunked 以外の転送符号化（要求）、上限超過（request line 8 KiB → 414、
//! header 32 KiB・100 行 → 431、body 既定 1 MiB → 413）。

pub mod auth;
pub mod error;
pub mod etag;
pub mod headers;
mod parse;
pub mod pct;
pub mod range;
pub mod request;
pub mod response;
pub mod router;
pub mod sse;

pub use error::HttpError;
pub use headers::HeaderMap;
pub use parse::{BodyFraming, Event, Limits, Version};
pub use request::{Method, Request, RequestEvent, RequestHead, RequestParser};
pub use response::{CONTINUE_100, Encoded, Framing, Response, ResponseEvent, ResponseHead, ResponseParser, chunked};

#[cfg(test)]
mod fuzz {
    //! 乱れた入力でパニック・無限ループしないこと（決定的な疑似乱数）。
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    const SEEDS: &[&[u8]] = &[
        b"GET /a?b=c HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\n\r\n",
        b"POST /p HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n5;e=1\r\nhello\r\n0\r\nT: v\r\n\r\n",
        b"PUT /p HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\nabcd",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
        b"HTTP/1.0 404 Not Found\r\nContent-Length: 2\r\n\r\nno",
    ];

    fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
        let mut v = seed.to_vec();
        for _ in 0..1 + rng.below(6) {
            if v.is_empty() {
                break;
            }
            let i = rng.below(v.len());
            match rng.below(6) {
                0 => v[i] = rng.next() as u8,
                1 => {
                    v.remove(i);
                }
                2 => {
                    let set = b"\r\n:; \t\0\x7f0F-";
                    v.insert(i, set[rng.below(set.len())])
                }
                3 => v.truncate(i),
                4 => {
                    let tail = v[i..].to_vec();
                    v.extend_from_slice(&tail[..tail.len().min(64)]);
                }
                _ => v[i] = v[i].to_ascii_uppercase(),
            }
        }
        v
    }

    fn drive_request(input: &[u8], step: usize) {
        let mut p = RequestParser::new(Limits::DEFAULT);
        let mut polls = 0;
        for chunk in input.chunks(step.max(1)) {
            if p.feed(chunk).is_err() {
                return;
            }
            loop {
                polls += 1;
                assert!(polls < 100_000, "parser did not make progress");
                match p.poll() {
                    Ok(Event::NeedMore) => break,
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        }
        p.feed_eof();
        for _ in 0..10_000 {
            match p.poll() {
                Ok(Event::Closed) | Err(_) => return,
                Ok(Event::NeedMore) => panic!("NeedMore after eof"),
                Ok(_) => {}
            }
        }
        panic!("no termination after eof");
    }

    fn drive_response(input: &[u8], step: usize) {
        let mut p = ResponseParser::new(Limits::DEFAULT);
        let mut polls = 0;
        for chunk in input.chunks(step.max(1)) {
            if p.feed(chunk).is_err() {
                return;
            }
            loop {
                polls += 1;
                assert!(polls < 100_000);
                match p.poll() {
                    Ok(Event::NeedMore) => break,
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        }
        p.feed_eof();
        for _ in 0..10_000 {
            match p.poll() {
                Ok(Event::Closed) | Err(_) => return,
                Ok(Event::NeedMore) => panic!("NeedMore after eof"),
                Ok(_) => {}
            }
        }
        panic!("no termination after eof");
    }

    #[test]
    fn mutated_and_random() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..20_000 {
            let seed = SEEDS[rng.below(SEEDS.len())];
            let input = mutate(&mut rng, seed);
            let step = 1 + rng.below(input.len().max(1));
            drive_request(&input, step);
            drive_response(&input, step);
        }
        for _ in 0..2_000 {
            let len = rng.below(512);
            let input: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            drive_request(&input, 1 + rng.below(64));
            drive_response(&input, 1 + rng.below(64));
        }
    }

    #[test]
    fn truncations() {
        for seed in SEEDS {
            for n in 0..seed.len() {
                drive_request(&seed[..n], 1);
                drive_request(&seed[..n], 7);
                drive_response(&seed[..n], 3);
            }
        }
    }

    #[test]
    fn huge_inputs_are_bounded() {
        // 終わらない header 行・終わらない chunk-ext
        let mut p = RequestParser::new(Limits::DEFAULT);
        p.feed(b"GET / HTTP/1.1\r\nHost: a\r\nX: ").unwrap();
        let junk = vec![b'a'; 4096];
        let mut failed = false;
        for _ in 0..100 {
            if p.feed(&junk).is_err() {
                failed = true;
                break;
            }
            if let Err(e) = p.poll() {
                assert_eq!(e.status(), 431);
                failed = true;
                break;
            }
        }
        assert!(failed);

        let mut p = RequestParser::new(Limits::DEFAULT);
        p.feed(b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n1;").unwrap();
        assert!(matches!(p.poll().unwrap(), Event::Head(_)));
        let mut failed = false;
        for _ in 0..100 {
            p.feed(&junk).unwrap();
            if p.poll().is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed);
    }
}
