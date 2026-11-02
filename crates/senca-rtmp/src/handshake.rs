//! 単純ハンドシェイク（RTMP 仕様 5.2、version 3）。
//!
//! - サーバー: C0+C1 を受けたら S0+S1+S2（S2 は C1 の写し）を返し、C2 を待つ。C2 の中身は検査しない
//!   （digest 付きの C1 を送る FFmpeg・Flash 系のクライアントもこれで通る）。
//!   S1 の 5〜8 バイト目（version 欄）は 0 にする。FFmpeg はここが 3 以上のときだけ digest を検証する。
//! - クライアント: C0+C1 を送り、S0+S1+S2 を受けたら C2（S1 の写し）を返す。
//!
//! 単純ハンドシェイクの乱数欄に安全性の意味はないので、乱数源は持たず種から擬似乱数で埋める。

use crate::Error;
use bytes::BytesMut;

pub const RTMP_VERSION: u8 = 3;
pub const HANDSHAKE_SIZE: usize = 1536;

fn fill_pseudo_random(buf: &mut [u8], mut seed: u64) {
    if seed == 0 {
        seed = 0x9E37_79B9_7F4A_7C15;
    }
    for b in buf {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *b = (seed >> 24) as u8;
    }
}

fn make_packet(time: u32, seed: u64) -> Vec<u8> {
    let mut p = vec![0u8; HANDSHAKE_SIZE];
    p[0..4].copy_from_slice(&time.to_be_bytes());
    // 4..8 は 0（単純ハンドシェイク）
    fill_pseudo_random(&mut p[8..], seed);
    p
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerState {
    WaitC0C1,
    WaitC2,
    Done,
}

/// サーバー側のハンドシェイク。
#[derive(Debug)]
pub struct ServerHandshake {
    state: ServerState,
    buf: Vec<u8>,
    seed: u64,
    time: u32,
}

impl ServerHandshake {
    /// `time` は S1 の時刻欄（ミリ秒、任意の基点）、`seed` は乱数欄の種。
    pub fn new(time: u32, seed: u64) -> Self {
        ServerHandshake {
            state: ServerState::WaitC0C1,
            buf: Vec::new(),
            seed,
            time,
        }
    }

    pub fn is_done(&self) -> bool {
        self.state == ServerState::Done
    }

    /// 入力を消費し、送るべきバイト列を `out` に書く。消費したバイト数を返す（残りは chunk stream）。
    pub fn feed(&mut self, data: &[u8], out: &mut BytesMut) -> Result<usize, Error> {
        let mut used = 0;
        while used < data.len() && self.state != ServerState::Done {
            let need = match self.state {
                ServerState::WaitC0C1 => 1 + HANDSHAKE_SIZE,
                ServerState::WaitC2 => HANDSHAKE_SIZE,
                ServerState::Done => 0,
            };
            let n = (need - self.buf.len()).min(data.len() - used);
            self.buf.extend_from_slice(&data[used..used + n]);
            used += n;
            if self.state == ServerState::WaitC0C1
                && !self.buf.is_empty()
                && self.buf[0] != RTMP_VERSION
            {
                return Err(Error::Handshake("unsupported RTMP version"));
            }
            if self.buf.len() < need {
                break;
            }
            match self.state {
                ServerState::WaitC0C1 => {
                    out.extend_from_slice(&[RTMP_VERSION]);
                    out.extend_from_slice(&make_packet(self.time, self.seed));
                    out.extend_from_slice(&self.buf[1..]);
                    self.state = ServerState::WaitC2;
                }
                ServerState::WaitC2 => self.state = ServerState::Done,
                ServerState::Done => {}
            }
            self.buf.clear();
        }
        if self.is_done() {
            self.buf = Vec::new();
        }
        Ok(used)
    }
}

/// クライアント側のハンドシェイク。
#[derive(Debug)]
pub struct ClientHandshake {
    buf: Vec<u8>,
    done: bool,
}

impl ClientHandshake {
    /// C0+C1 を `out` に書いて始める。
    pub fn start(time: u32, seed: u64, out: &mut BytesMut) -> Self {
        out.extend_from_slice(&[RTMP_VERSION]);
        out.extend_from_slice(&make_packet(time, seed));
        ClientHandshake {
            buf: Vec::new(),
            done: false,
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// S0+S1+S2 を受け、C2 を `out` に書く。消費したバイト数を返す。
    pub fn feed(&mut self, data: &[u8], out: &mut BytesMut) -> Result<usize, Error> {
        if self.done {
            return Ok(0);
        }
        let need = 1 + 2 * HANDSHAKE_SIZE;
        let n = (need - self.buf.len()).min(data.len());
        self.buf.extend_from_slice(&data[..n]);
        if !self.buf.is_empty() && self.buf[0] != RTMP_VERSION {
            return Err(Error::Handshake("unsupported RTMP version"));
        }
        if self.buf.len() == need {
            out.extend_from_slice(&self.buf[1..1 + HANDSHAKE_SIZE]);
            self.done = true;
            self.buf = Vec::new();
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_server() {
        let mut c_out = BytesMut::new();
        let mut c = ClientHandshake::start(0, 1, &mut c_out);
        let mut s = ServerHandshake::new(0, 2);
        let mut s_out = BytesMut::new();
        // 1 バイトずつ
        for b in c_out.split().iter() {
            assert_eq!(s.feed(&[*b], &mut s_out).unwrap(), 1);
        }
        assert_eq!(s_out.len(), 1 + 2 * HANDSHAKE_SIZE);
        let mut extra = s_out.split().to_vec();
        extra.extend_from_slice(b"rest");
        let used = c.feed(&extra, &mut c_out).unwrap();
        assert_eq!(used, extra.len() - 4);
        assert!(c.is_done());
        let mut c2 = c_out.split().to_vec();
        c2.extend_from_slice(b"chunk");
        assert_eq!(s.feed(&c2, &mut s_out).unwrap(), HANDSHAKE_SIZE);
        assert!(s.is_done());
    }

    #[test]
    fn rejects_bad_version() {
        let mut s = ServerHandshake::new(0, 0);
        assert!(s.feed(&[6], &mut BytesMut::new()).is_err());
        let mut out = BytesMut::new();
        let mut c = ClientHandshake::start(0, 0, &mut out);
        assert!(c.feed(&[0x16, 0x03], &mut out).is_err()); // TLS を平文に向けた
    }
}
