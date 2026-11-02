//! HelloVerifyRequest の cookie（RFC 6347 4.2.1）。
//!
//! Cookie = HMAC-SHA256(secret, 相手の輸送識別子 || ClientHello の cookie 以外の部分)。
//! server は状態を持たずに検証できる。秘密は入れ替えられ、直前の秘密でも 1 世代だけ検証する。

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::DtlsError;
use crate::handshake::{self, ClientHello, HT_CLIENT_HELLO, HT_HELLO_VERIFY_REQUEST};
use crate::record::{self, CT_HANDSHAKE, DTLS10};

/// cookie を作る秘密。Debug に値を出さない。
#[derive(Clone)]
pub struct CookieKey {
    current: Zeroizing<[u8; 32]>,
    previous: Option<Zeroizing<[u8; 32]>>,
}

impl core::fmt::Debug for CookieKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("CookieKey(..)")
    }
}

impl CookieKey {
    /// OS の乱数で作る。
    pub fn random() -> Result<Self, DtlsError> {
        let mut s = Zeroizing::new([0u8; 32]);
        getrandom::fill(&mut s[..]).map_err(|_| DtlsError::RandomUnavailable)?;
        Ok(Self {
            current: s,
            previous: None,
        })
    }

    /// 与えた秘密で作る（複数の process で共有する場合）。
    pub fn from_secret(secret: [u8; 32]) -> Self {
        Self {
            current: Zeroizing::new(secret),
            previous: None,
        }
    }

    /// 秘密を入れ替える。直前の秘密で作った cookie も次の入れ替えまでは通る。
    pub fn rotate(&mut self) -> Result<(), DtlsError> {
        let next = Self::random()?;
        self.previous = Some(core::mem::replace(&mut self.current, next.current));
        Ok(())
    }

    fn compute(secret: &[u8; 32], peer_id: &[u8], cookie_input: &[u8]) -> [u8; 32] {
        let mut m =
            <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC は任意長の鍵を受ける");
        m.update(&(peer_id.len() as u32).to_be_bytes());
        m.update(peer_id);
        m.update(cookie_input);
        m.finalize().into_bytes().into()
    }

    pub(crate) fn make(&self, peer_id: &[u8], ch: &ClientHello) -> [u8; 32] {
        Self::compute(&self.current, peer_id, &ch.cookie_input)
    }

    pub(crate) fn verify(&self, peer_id: &[u8], ch: &ClientHello) -> bool {
        if ch.cookie.len() != 32 {
            return false;
        }
        let cur = Self::compute(&self.current, peer_id, &ch.cookie_input);
        let mut ok = bool::from(cur[..].ct_eq(&ch.cookie));
        if let Some(prev) = &self.previous {
            let p = Self::compute(prev, peer_id, &ch.cookie_input);
            ok |= bool::from(p[..].ct_eq(&ch.cookie));
        }
        ok
    }
}

/// HelloVerifyRequest の datagram。record の sequence number と message_seq は ClientHello のものを使う。
pub(crate) fn hello_verify_datagram(record_seq: u64, message_seq: u16, cookie: &[u8]) -> Vec<u8> {
    let body = handshake::encode_hello_verify_request(cookie);
    // RFC 6347 4.2.1: HelloVerifyRequest の server_version は DTLS 1.0 を使ってよい（SHOULD）。
    let mut body = body;
    body[0..2].copy_from_slice(&DTLS10.to_be_bytes());
    let frag = handshake::message_bytes(HT_HELLO_VERIFY_REQUEST, message_seq, &body);
    let mut out = Vec::with_capacity(record::RECORD_HEADER_LEN + frag.len());
    record::encode_header(&mut out, CT_HANDSHAKE, DTLS10, 0, record_seq, frag.len());
    out.extend_from_slice(&frag);
    out
}

/// datagram 先頭の ClientHello（分割されていないもの）を取り出す。
pub(crate) fn first_client_hello(datagram: &[u8]) -> Option<(u64, u16, ClientHello)> {
    let recs = record::split_records(datagram);
    let rec = recs.first()?;
    if rec.content_type != CT_HANDSHAKE || rec.epoch != 0 {
        return None;
    }
    let frags = handshake::parse_fragments(rec.body).ok()?;
    let f = frags.first()?;
    if f.msg_type != HT_CLIENT_HELLO || f.offset != 0 || f.data.len() != f.length as usize {
        return None;
    }
    let ch = ClientHello::parse(f.data).ok()?;
    Some((rec.seq, f.seq, ch))
}

/// 状態を持たない cookie 検査の結果。
#[derive(Debug, PartialEq, Eq)]
pub enum CookieDecision {
    /// cookie が無いか不正。この datagram（HelloVerifyRequest）を返し、状態は作らない。
    HelloVerifyRequest(Vec<u8>),
    /// cookie が正しい。endpoint を作ってこの datagram を渡してよい。
    Accept,
    /// ClientHello ではない。捨てる。
    Drop,
}

/// endpoint を作る前に、受け取った datagram を状態を持たずに検査する（増幅・資源消費の防止）。
///
/// `peer_id` は相手の IP とポートなど。同じ値を [`crate::DtlsConfig::peer_transport_id`] に設定する。
pub fn check_client_hello(key: &CookieKey, peer_id: &[u8], datagram: &[u8]) -> CookieDecision {
    let Some((rseq, mseq, ch)) = first_client_hello(datagram) else {
        return CookieDecision::Drop;
    };
    if key.verify(peer_id, &ch) {
        CookieDecision::Accept
    } else {
        // 応答は ClientHello より小さく、増幅にならない。
        CookieDecision::HelloVerifyRequest(hello_verify_datagram(
            rseq,
            mseq,
            &key.make(peer_id, &ch),
        ))
    }
}
