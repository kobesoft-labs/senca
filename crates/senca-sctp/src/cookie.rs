//! State Cookie（RFC 9260 5.1.3）と乱数。
//!
//! cookie は「中身 + HMAC-SHA256」。鍵は association ごとに乱数で作る。
//! 比べるときは定数時間（`subtle`）で比べる。

use bytes::{BufMut, Bytes};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::SctpError;

type HmacSha256 = Hmac<Sha256>;

const COOKIE_VERSION: u8 = 1;
const MAC_LEN: usize = 32;
const BODY_LEN: usize = 1 + 4 * 5 + 2 * 4 + 1 + 8;
/// cookie 全体の長さ。
pub(crate) const COOKIE_LEN: usize = BODY_LEN + MAC_LEN;

/// cookie に入れる値。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CookieData {
    /// こちらの Verification Tag（INIT-ACK の Initiate Tag）。
    pub my_tag: u32,
    /// 相手の Verification Tag（INIT の Initiate Tag）。
    pub peer_tag: u32,
    pub my_initial_tsn: u32,
    pub peer_initial_tsn: u32,
    pub peer_a_rwnd: u32,
    pub peer_os: u16,
    pub peer_mis: u16,
    pub my_os: u16,
    pub my_mis: u16,
    pub peer_forward_tsn: bool,
    pub peer_reconfig: bool,
    /// 作った時刻（association を作った時刻からのミリ秒）。
    pub timestamp_ms: u64,
}

fn mac(key: &[u8; 32], body: &[u8]) -> [u8; MAC_LEN] {
    // 鍵の長さは固定なので失敗しない。
    let mut m = match HmacSha256::new_from_slice(key) {
        Ok(m) => m,
        Err(_) => return [0; MAC_LEN],
    };
    m.update(body);
    m.finalize().into_bytes().into()
}

impl CookieData {
    pub fn encode(&self, key: &[u8; 32]) -> Bytes {
        let mut v = Vec::with_capacity(COOKIE_LEN);
        v.put_u8(COOKIE_VERSION);
        v.put_u32(self.my_tag);
        v.put_u32(self.peer_tag);
        v.put_u32(self.my_initial_tsn);
        v.put_u32(self.peer_initial_tsn);
        v.put_u32(self.peer_a_rwnd);
        v.put_u16(self.peer_os);
        v.put_u16(self.peer_mis);
        v.put_u16(self.my_os);
        v.put_u16(self.my_mis);
        v.put_u8(self.peer_forward_tsn as u8 | (self.peer_reconfig as u8) << 1);
        v.put_u64(self.timestamp_ms);
        let m = mac(key, &v);
        v.put_slice(&m);
        Bytes::from(v)
    }

    /// 署名を確かめて読む。形が違う・署名が合わなければ `None`。
    pub fn decode(b: &[u8], key: &[u8; 32]) -> Option<CookieData> {
        if b.len() != COOKIE_LEN {
            return None;
        }
        let (body, tag) = b.split_at(BODY_LEN);
        let want = mac(key, body);
        if !bool::from(want.ct_eq(tag)) {
            return None;
        }
        if body[0] != COOKIE_VERSION {
            return None;
        }
        let u32_at =
            |i: usize| u32::from_be_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
        let u16_at = |i: usize| u16::from_be_bytes([body[i], body[i + 1]]);
        let flags = body[29];
        let mut ts = [0u8; 8];
        ts.copy_from_slice(&body[30..38]);
        Some(CookieData {
            my_tag: u32_at(1),
            peer_tag: u32_at(5),
            my_initial_tsn: u32_at(9),
            peer_initial_tsn: u32_at(13),
            peer_a_rwnd: u32_at(17),
            peer_os: u16_at(21),
            peer_mis: u16_at(23),
            my_os: u16_at(25),
            my_mis: u16_at(27),
            peer_forward_tsn: flags & 1 != 0,
            peer_reconfig: flags & 2 != 0,
            timestamp_ms: u64::from_be_bytes(ts),
        })
    }
}

/// 乱数（OS の乱数で種をとり、HMAC-SHA256 のカウンタで引き出す）。
pub(crate) struct Rng {
    key: [u8; 32],
    counter: u64,
    buf: [u8; 32],
    pos: usize,
}

impl Rng {
    pub fn new() -> Result<Rng, SctpError> {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).map_err(|_| SctpError::Random)?;
        Ok(Rng {
            key,
            counter: 0,
            buf: [0; 32],
            pos: 32,
        })
    }

    pub fn fill(&mut self, out: &mut [u8]) {
        for b in out {
            if self.pos >= self.buf.len() {
                self.counter = self.counter.wrapping_add(1);
                self.buf = mac(&self.key, &self.counter.to_be_bytes());
                self.pos = 0;
            }
            *b = self.buf[self.pos];
            self.pos += 1;
        }
    }

    pub fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.fill(&mut b);
        u32::from_be_bytes(b)
    }

    /// 0 でない値（Verification Tag 用）。
    pub fn nonzero_u32(&mut self) -> u32 {
        loop {
            let v = self.next_u32();
            if v != 0 {
                return v;
            }
        }
    }

    pub fn key32(&mut self) -> [u8; 32] {
        let mut k = [0u8; 32];
        self.fill(&mut k);
        k
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CookieData {
        CookieData {
            my_tag: 1,
            peer_tag: 2,
            my_initial_tsn: 3,
            peer_initial_tsn: 4,
            peer_a_rwnd: 5,
            peer_os: 6,
            peer_mis: 7,
            my_os: 8,
            my_mis: 9,
            peer_forward_tsn: true,
            peer_reconfig: false,
            timestamp_ms: 12345,
        }
    }

    #[test]
    fn round_trip_and_tamper() {
        let key = [7u8; 32];
        let c = sample();
        let b = c.encode(&key);
        assert_eq!(b.len(), COOKIE_LEN);
        assert_eq!(CookieData::decode(&b, &key), Some(c));
        for i in 0..b.len() {
            let mut t = b.to_vec();
            t[i] ^= 0x01;
            assert_eq!(CookieData::decode(&t, &key), None, "byte {i}");
        }
        assert_eq!(CookieData::decode(&b, &[8u8; 32]), None);
        assert_eq!(CookieData::decode(&b[..b.len() - 1], &key), None);
    }

    #[test]
    fn rng_differs() {
        let mut r = Rng::new().unwrap();
        let a = r.next_u32();
        let b = r.next_u32();
        let c = r.next_u32();
        assert!(a != b || b != c);
        assert_ne!(r.nonzero_u32(), 0);
    }
}
