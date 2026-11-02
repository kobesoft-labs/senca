//! STUN メッセージのヘッダー・解析・組み立て（RFC 8489 5 節・14 節）。

use std::fmt;
use std::net::SocketAddr;

use bytes::Bytes;
use hmac::{Hmac, Mac};
use subtle::ConstantTimeEq;

use crate::StunError;
use crate::attr::{Attribute, attr_type, is_comprehension_required};
use crate::crc32::crc32;

/// magic cookie。
pub const MAGIC_COOKIE: u32 = 0x2112_A442;
/// FINGERPRINT の XOR 値（"STUN"）。
pub const FINGERPRINT_XOR: u32 = 0x5354_554e;
/// 1 メッセージの属性の数の上限（壊れた入力で無制限に確保しない）。
pub const MAX_ATTRIBUTES: usize = 128;
/// ヘッダーの長さ。
pub const HEADER_LEN: usize = 20;

/// メソッド番号。
pub mod method {
    pub const BINDING: u16 = 0x001;
    pub const ALLOCATE: u16 = 0x003;
    pub const REFRESH: u16 = 0x004;
    pub const SEND: u16 = 0x006;
    pub const DATA: u16 = 0x007;
    pub const CREATE_PERMISSION: u16 = 0x008;
    pub const CHANNEL_BIND: u16 = 0x009;
    pub const CONNECT: u16 = 0x00A;
    pub const CONNECTION_BIND: u16 = 0x00B;
    pub const CONNECTION_ATTEMPT: u16 = 0x00C;
}

/// メソッド（12 bit）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Method(pub u16);

impl Method {
    pub const BINDING: Method = Method(method::BINDING);
    pub const ALLOCATE: Method = Method(method::ALLOCATE);
    pub const REFRESH: Method = Method(method::REFRESH);
    pub const SEND: Method = Method(method::SEND);
    pub const DATA: Method = Method(method::DATA);
    pub const CREATE_PERMISSION: Method = Method(method::CREATE_PERMISSION);
    pub const CHANNEL_BIND: Method = Method(method::CHANNEL_BIND);
}

/// メッセージの class。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    Request,
    Indication,
    Success,
    Error,
}

impl Class {
    fn bits(self) -> u16 {
        match self {
            Class::Request => 0,
            Class::Indication => 1,
            Class::Success => 2,
            Class::Error => 3,
        }
    }
}

fn encode_type(class: Class, method: Method) -> u16 {
    let m = method.0 & 0x0FFF;
    let c = class.bits();
    (m & 0x000F) | ((m & 0x0070) << 1) | ((m & 0x0F80) << 2) | ((c & 1) << 4) | ((c & 2) << 7)
}

fn decode_type(t: u16) -> (Class, Method) {
    let m = (t & 0x000F) | ((t >> 1) & 0x0070) | ((t >> 2) & 0x0F80);
    let c = ((t >> 4) & 1) | ((t >> 7) & 2);
    let class = match c {
        0 => Class::Request,
        1 => Class::Indication,
        2 => Class::Success,
        _ => Class::Error,
    };
    (class, Method(m))
}

/// 96 bit の transaction ID。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TransactionId(pub [u8; 12]);

impl TransactionId {
    /// OS の乱数源から作る（RFC 8489 5 節: 暗号論的に乱数であること）。
    pub fn random() -> Result<Self, StunError> {
        let mut b = [0u8; 12];
        getrandom::fill(&mut b).map_err(|_| StunError::Random)?;
        Ok(TransactionId(b))
    }
}

impl fmt::Debug for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TransactionId(")?;
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        write!(f, ")")
    }
}

/// STUN メッセージらしいかを安く判定する（先頭 2 bit・magic cookie・長さ）。
pub fn is_stun(buf: &[u8]) -> bool {
    buf.len() >= HEADER_LEN
        && buf[0] & 0xC0 == 0
        && buf[4..8] == MAGIC_COOKIE.to_be_bytes()
        && u16::from_be_bytes([buf[2], buf[3]]) as usize + HEADER_LEN == buf.len()
        && buf.len().is_multiple_of(4)
}

/// 解析済みの STUN メッセージ。
#[derive(Clone)]
pub struct Message {
    pub class: Class,
    pub method: Method,
    pub transaction_id: TransactionId,
    /// 受け取った属性（MESSAGE-INTEGRITY 以降の無視すべき属性は含まない）。
    pub attributes: Vec<Attribute>,
    raw: Bytes,
    integrity_at: Option<usize>,
    integrity_sha256_at: Option<usize>,
    unknown_required: Vec<u16>,
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("class", &self.class)
            .field("method", &self.method)
            .field("transaction_id", &self.transaction_id)
            .field("attributes", &self.attributes)
            .finish()
    }
}

impl Message {
    /// 厳格に解析する。FINGERPRINT があれば検証する（一致しなければ `Err`）。
    pub fn parse(buf: Bytes) -> Result<Message, StunError> {
        if buf.len() < HEADER_LEN {
            return Err(StunError::TooShort);
        }
        if buf[0] & 0xC0 != 0 || buf[4..8] != MAGIC_COOKIE.to_be_bytes() {
            return Err(StunError::NotStun);
        }
        let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        if !len.is_multiple_of(4) || HEADER_LEN + len != buf.len() {
            return Err(StunError::BadLength);
        }
        let (class, method) = decode_type(u16::from_be_bytes([buf[0], buf[1]]));
        let mut tid = [0u8; 12];
        tid.copy_from_slice(&buf[8..20]);
        let transaction_id = TransactionId(tid);

        let mut attributes = Vec::new();
        let mut integrity_at = None;
        let mut integrity_sha256_at = None;
        let mut fingerprint_seen = false;
        let mut unknown_required = Vec::new();
        let mut off = HEADER_LEN;
        let mut count = 0usize;
        while off < buf.len() {
            if buf.len() - off < 4 {
                return Err(StunError::BadLength);
            }
            let typ = u16::from_be_bytes([buf[off], buf[off + 1]]);
            let alen = u16::from_be_bytes([buf[off + 2], buf[off + 3]]) as usize;
            let padded = (alen + 3) & !3;
            if off + 4 + padded > buf.len() {
                return Err(StunError::BadAttribute(typ));
            }
            count += 1;
            if count > MAX_ATTRIBUTES {
                return Err(StunError::TooManyAttributes);
            }
            if fingerprint_seen {
                // FINGERPRINT は最後でなければならない。
                return Err(StunError::BadFingerprint);
            }
            let at = off;
            off += 4 + padded;
            // MESSAGE-INTEGRITY 以降は MI-SHA256 と FINGERPRINT だけを見る。MI-SHA256 以降は FINGERPRINT だけ。
            let ignore = (integrity_sha256_at.is_some() && typ != attr_type::FINGERPRINT)
                || (integrity_at.is_some()
                    && typ != attr_type::FINGERPRINT
                    && typ != attr_type::MESSAGE_INTEGRITY_SHA256);
            if ignore {
                continue;
            }
            let value = buf.slice(at + 4..at + 4 + alen);
            let attr = Attribute::decode(typ, value, &transaction_id)?;
            match &attr {
                Attribute::MessageIntegrity(_) => integrity_at = Some(at),
                Attribute::MessageIntegritySha256(_) => integrity_sha256_at = Some(at),
                Attribute::Fingerprint(v) => {
                    let expect = crc32(&[&buf[..at]]) ^ FINGERPRINT_XOR;
                    if *v != expect {
                        return Err(StunError::BadFingerprint);
                    }
                    fingerprint_seen = true;
                }
                Attribute::Unknown { typ, .. }
                    if is_comprehension_required(*typ) && !unknown_required.contains(typ) =>
                {
                    unknown_required.push(*typ);
                }
                _ => {}
            }
            attributes.push(attr);
        }
        Ok(Message {
            class,
            method,
            transaction_id,
            attributes,
            raw: buf,
            integrity_at,
            integrity_sha256_at,
            unknown_required,
        })
    }

    /// 受け取ったままのバイト列。
    pub fn raw(&self) -> &Bytes {
        &self.raw
    }

    /// 理解できなかった理解必須属性（要求なら 420 で応える）。
    pub fn unknown_comprehension_required(&self) -> &[u16] {
        &self.unknown_required
    }

    /// MESSAGE-INTEGRITY か MESSAGE-INTEGRITY-SHA256 を持つか。
    pub fn has_integrity(&self) -> bool {
        self.integrity_at.is_some() || self.integrity_sha256_at.is_some()
    }

    /// FINGERPRINT を持つか（持っていれば解析時に検証済み）。
    pub fn has_fingerprint(&self) -> bool {
        matches!(self.attributes.last(), Some(Attribute::Fingerprint(_)))
    }

    /// 完全性を定数時間で検査する。MI-SHA256 があればそちらを、無ければ MI を使う。
    pub fn verify_integrity(&self, key: &[u8]) -> Result<(), StunError> {
        if let Some(at) = self.integrity_sha256_at {
            let Some(Attribute::MessageIntegritySha256(got)) = self
                .attributes
                .iter()
                .find(|a| matches!(a, Attribute::MessageIntegritySha256(_)))
            else {
                return Err(StunError::NoIntegrity);
            };
            let mac = hmac_sha256(key, &self.raw, at, 4 + got.len());
            return ct_eq(&mac[..got.len()], got);
        }
        if let Some(at) = self.integrity_at {
            let Some(Attribute::MessageIntegrity(got)) = self
                .attributes
                .iter()
                .find(|a| matches!(a, Attribute::MessageIntegrity(_)))
            else {
                return Err(StunError::NoIntegrity);
            };
            let mac = hmac_sha1(key, &self.raw, at, 24);
            return ct_eq(&mac, got);
        }
        Err(StunError::NoIntegrity)
    }

    /// 最初に現れた属性。
    pub fn get(&self, typ: u16) -> Option<&Attribute> {
        self.attributes.iter().find(|a| a.typ() == typ)
    }

    pub fn username(&self) -> Option<&str> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::Username(s) => Some(s.as_str()),
            _ => None,
        })
    }

    pub fn realm(&self) -> Option<&str> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::Realm(s) => Some(s.as_str()),
            _ => None,
        })
    }

    pub fn nonce(&self) -> Option<&str> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::Nonce(s) => Some(s.as_str()),
            _ => None,
        })
    }

    pub fn software(&self) -> Option<&str> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::Software(s) => Some(s.as_str()),
            _ => None,
        })
    }

    pub fn error_code(&self) -> Option<(u16, &str)> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::ErrorCode { code, reason } => Some((*code, reason.as_str())),
            _ => None,
        })
    }

    /// XOR-MAPPED-ADDRESS、無ければ MAPPED-ADDRESS。
    pub fn mapped_address(&self) -> Option<SocketAddr> {
        self.attributes
            .iter()
            .find_map(|a| match a {
                Attribute::XorMappedAddress(s) => Some(*s),
                _ => None,
            })
            .or_else(|| {
                self.attributes.iter().find_map(|a| match a {
                    Attribute::MappedAddress(s) => Some(*s),
                    _ => None,
                })
            })
    }

    pub fn priority(&self) -> Option<u32> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::Priority(p) => Some(*p),
            _ => None,
        })
    }

    pub fn use_candidate(&self) -> bool {
        self.attributes
            .iter()
            .any(|a| matches!(a, Attribute::UseCandidate))
    }

    pub fn ice_controlling(&self) -> Option<u64> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::IceControlling(v) => Some(*v),
            _ => None,
        })
    }

    pub fn ice_controlled(&self) -> Option<u64> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::IceControlled(v) => Some(*v),
            _ => None,
        })
    }

    pub fn lifetime(&self) -> Option<u32> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::Lifetime(v) => Some(*v),
            _ => None,
        })
    }

    pub fn channel_number(&self) -> Option<u16> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::ChannelNumber(v) => Some(*v),
            _ => None,
        })
    }

    pub fn requested_transport(&self) -> Option<u8> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::RequestedTransport(v) => Some(*v),
            _ => None,
        })
    }

    /// すべての XOR-PEER-ADDRESS（CreatePermission は複数持てる）。
    pub fn xor_peer_addresses(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.attributes.iter().filter_map(|a| match a {
            Attribute::XorPeerAddress(s) => Some(*s),
            _ => None,
        })
    }

    pub fn xor_relayed_address(&self) -> Option<SocketAddr> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::XorRelayedAddress(s) => Some(*s),
            _ => None,
        })
    }

    pub fn data(&self) -> Option<&Bytes> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::Data(d) => Some(d),
            _ => None,
        })
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> Result<(), StunError> {
    if a.len() == b.len() && bool::from(a.ct_eq(b)) {
        Ok(())
    } else {
        Err(StunError::IntegrityMismatch)
    }
}

/// ヘッダーの長さを「完全性属性の末尾まで」に置き換えた先頭部分。
fn adjusted_header(raw: &[u8], at: usize, attr_total: usize) -> [u8; 4] {
    let len = (at - HEADER_LEN + attr_total) as u16;
    let lb = len.to_be_bytes();
    [raw[0], raw[1], lb[0], lb[1]]
}

fn hmac_sha1(key: &[u8], raw: &[u8], at: usize, attr_total: usize) -> [u8; 20] {
    // HMAC は任意長の鍵を受け付けるので new_from_slice は失敗しない。
    let mut m =
        <Hmac<sha1::Sha1> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(&adjusted_header(raw, at, attr_total));
    m.update(&raw[4..at]);
    m.finalize().into_bytes().into()
}

fn hmac_sha256(key: &[u8], raw: &[u8], at: usize, attr_total: usize) -> [u8; 32] {
    let mut m =
        <Hmac<sha2::Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(&adjusted_header(raw, at, attr_total));
    m.update(&raw[4..at]);
    m.finalize().into_bytes().into()
}

/// STUN メッセージを組み立てる。
///
/// ```
/// use senca_stun::{Attribute, Class, MessageBuilder, Method, TransactionId};
/// let b = MessageBuilder::new(Class::Request, Method::BINDING, TransactionId([1; 12]))
///     .attr(Attribute::Username("a:b".into()))
///     .integrity(b"pass")
///     .fingerprint()
///     .build()
///     .unwrap();
/// let m = senca_stun::Message::parse(b).unwrap();
/// m.verify_integrity(b"pass").unwrap();
/// ```
pub struct MessageBuilder {
    buf: Vec<u8>,
    tid: TransactionId,
    err: Option<StunError>,
}

impl MessageBuilder {
    pub fn new(class: Class, method: Method, tid: TransactionId) -> Self {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(&encode_type(class, method).to_be_bytes());
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        buf.extend_from_slice(&tid.0);
        MessageBuilder {
            buf,
            tid,
            err: None,
        }
    }

    pub fn transaction_id(&self) -> TransactionId {
        self.tid
    }

    fn set_len(&mut self, extra: usize) -> bool {
        let len = self.buf.len() - HEADER_LEN + extra;
        if len > 0xFFFF {
            self.err.get_or_insert(StunError::TooLarge);
            return false;
        }
        self.buf[2..4].copy_from_slice(&(len as u16).to_be_bytes());
        true
    }

    /// 属性を足す。
    pub fn attr(mut self, a: Attribute) -> Self {
        if self.err.is_none()
            && let Err(e) = a.encode(&self.tid, &mut self.buf)
        {
            self.err = Some(e);
        }
        self
    }

    /// 複数の属性を足す。
    pub fn attrs(mut self, list: impl IntoIterator<Item = Attribute>) -> Self {
        for a in list {
            self = self.attr(a);
        }
        self
    }

    /// MESSAGE-INTEGRITY（HMAC-SHA1）を足す。
    pub fn integrity(mut self, key: &[u8]) -> Self {
        if self.err.is_some() || !self.set_len(24) {
            return self;
        }
        let at = self.buf.len();
        let mac = hmac_sha1(key, &self.buf, at, 24);
        self.buf
            .extend_from_slice(&attr_type::MESSAGE_INTEGRITY.to_be_bytes());
        self.buf.extend_from_slice(&20u16.to_be_bytes());
        self.buf.extend_from_slice(&mac);
        self
    }

    /// MESSAGE-INTEGRITY-SHA256（切り詰めない 32 バイト）を足す。
    pub fn integrity_sha256(mut self, key: &[u8]) -> Self {
        if self.err.is_some() || !self.set_len(36) {
            return self;
        }
        let at = self.buf.len();
        let mac = hmac_sha256(key, &self.buf, at, 36);
        self.buf
            .extend_from_slice(&attr_type::MESSAGE_INTEGRITY_SHA256.to_be_bytes());
        self.buf.extend_from_slice(&32u16.to_be_bytes());
        self.buf.extend_from_slice(&mac);
        self
    }

    /// FINGERPRINT を足す（最後に呼ぶ）。
    pub fn fingerprint(mut self) -> Self {
        if self.err.is_some() || !self.set_len(8) {
            return self;
        }
        let crc = crc32(&[&self.buf]) ^ FINGERPRINT_XOR;
        self.buf
            .extend_from_slice(&attr_type::FINGERPRINT.to_be_bytes());
        self.buf.extend_from_slice(&4u16.to_be_bytes());
        self.buf.extend_from_slice(&crc.to_be_bytes());
        self
    }

    pub fn build(mut self) -> Result<Bytes, StunError> {
        if let Some(e) = self.err {
            return Err(e);
        }
        if !self.set_len(0) {
            return Err(StunError::TooLarge);
        }
        Ok(Bytes::from(self.buf))
    }
}

/// Binding 要求への成功応答（XOR-MAPPED-ADDRESS = `source`）。完全性・FINGERPRINT は呼び出し側が足す。
pub fn binding_success(req: &Message, source: SocketAddr) -> MessageBuilder {
    MessageBuilder::new(Class::Success, req.method, req.transaction_id)
        .attr(Attribute::XorMappedAddress(source))
}

/// 要求への誤り応答。420 なら UNKNOWN-ATTRIBUTES に理解できなかった属性を入れる。
pub fn error_response(req: &Message, code: u16, reason: &str) -> MessageBuilder {
    let mut b = MessageBuilder::new(Class::Error, req.method, req.transaction_id).attr(
        Attribute::ErrorCode {
            code,
            reason: reason.to_string(),
        },
    );
    if code == 420 && !req.unknown_required.is_empty() {
        b = b.attr(Attribute::UnknownAttributes(req.unknown_required.clone()));
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attr::PasswordAlgorithm;
    use crate::auth::{long_term_key, userhash};

    fn hex(s: &str) -> Bytes {
        let s: String = s.split_whitespace().collect();
        Bytes::from(
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect::<Vec<_>>(),
        )
    }

    const PASS: &[u8] = b"VOkJxbRl1RmTxUk/WvJxBt";

    fn rfc5769_request() -> Bytes {
        hex("00 01 00 58 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae
             80 22 00 10 53 54 55 4e 20 74 65 73 74 20 63 6c 69 65 6e 74
             00 24 00 04 6e 00 01 ff 80 29 00 08 93 2f f9 b1 51 26 3b 36
             00 06 00 09 65 76 74 6a 3a 68 36 76 59 20 20 20
             00 08 00 14 9a ea a7 0c bf d8 cb 56 78 1e f2 b5 b2 d3 f2 49 c1 b5 71 a2
             80 28 00 04 e5 7a 3b cf")
    }

    #[test]
    fn rfc5769_sample_request() {
        let m = Message::parse(rfc5769_request()).unwrap();
        assert_eq!(m.class, Class::Request);
        assert_eq!(m.method, Method::BINDING);
        assert_eq!(m.software(), Some("STUN test client"));
        assert_eq!(m.priority(), Some(0x6e0001ff));
        assert_eq!(m.ice_controlled(), Some(0x932ff9b151263b36));
        assert_eq!(m.username(), Some("evtj:h6vY"));
        assert!(m.has_fingerprint());
        m.verify_integrity(PASS).unwrap();
        assert_eq!(
            m.verify_integrity(b"wrong"),
            Err(StunError::IntegrityMismatch)
        );

        // 同じ属性で組み立てると完全性・FINGERPRINT が一致する（USERNAME の padding だけが違う）
        let b = MessageBuilder::new(Class::Request, Method::BINDING, m.transaction_id)
            .attr(Attribute::Software("STUN test client".into()))
            .attr(Attribute::Priority(0x6e0001ff))
            .attr(Attribute::IceControlled(0x932ff9b151263b36))
            .attr(Attribute::Username("evtj:h6vY".into()))
            .integrity(PASS)
            .fingerprint()
            .build()
            .unwrap();
        let m2 = Message::parse(b.clone()).unwrap();
        m2.verify_integrity(PASS).unwrap();
        assert_eq!(b.len(), rfc5769_request().len());
        // HMAC は padding の値を含むので RFC の例（空白で padding）とは違う。それ以外の範囲は一致する。
        assert_eq!(b[..73], rfc5769_request()[..73]);
    }

    #[test]
    fn rfc5769_corruption_detected() {
        let mut v = rfc5769_request().to_vec();
        v[30] ^= 1; // SOFTWARE の 1 バイト
        assert_eq!(
            Message::parse(Bytes::from(v)).unwrap_err(),
            StunError::BadFingerprint
        );
    }

    #[test]
    fn rfc5769_ipv4_response() {
        let raw = hex("01 01 00 3c 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae
                       80 22 00 0b 74 65 73 74 20 76 65 63 74 6f 72 20
                       00 20 00 08 00 01 a1 47 e1 12 a6 43
                       00 08 00 14 2b 91 f5 99 fd 9e 90 c3 8c 74 89 f9 2a f9 ba 53 f0 6b e7 d7
                       80 28 00 04 c0 7d 4c 96");
        let m = Message::parse(raw.clone()).unwrap();
        assert_eq!(m.class, Class::Success);
        assert_eq!(m.mapped_address(), Some("192.0.2.1:32853".parse().unwrap()));
        m.verify_integrity(PASS).unwrap();
        // 組み立て直しても同じバイト列（SOFTWARE の padding は RFC 例では空白 0x20）
        let b = MessageBuilder::new(Class::Success, Method::BINDING, m.transaction_id)
            .attr(Attribute::Software("test vector".into()))
            .attr(Attribute::XorMappedAddress(
                "192.0.2.1:32853".parse().unwrap(),
            ))
            .build()
            .unwrap();
        assert_eq!(b[24..35], raw[24..35]);
        assert_eq!(b[36..48], raw[36..48]);
    }

    #[test]
    fn rfc5769_ipv6_response() {
        let raw = hex("01 01 00 48 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae
                       80 22 00 0b 74 65 73 74 20 76 65 63 74 6f 72 20
                       00 20 00 14 00 02 a1 47 01 13 a9 fa a5 d3 f1 79 bc 25 f4 b5 be d2 b9 d9
                       00 08 00 14 a3 82 95 4e 4b e6 7b f1 17 84 c9 7c 82 92 c2 75 bf e3 ed 41
                       80 28 00 04 c8 fb 0b 4c");
        let m = Message::parse(raw).unwrap();
        assert_eq!(
            m.mapped_address(),
            Some(
                "[2001:db8:1234:5678:11:2233:4455:6677]:32853"
                    .parse()
                    .unwrap()
            )
        );
        m.verify_integrity(PASS).unwrap();
    }

    const MATRIX_USER: &str = "\u{30DE}\u{30C8}\u{30EA}\u{30C3}\u{30AF}\u{30B9}";

    #[test]
    fn rfc5769_long_term() {
        let raw = hex("00 01 00 60 21 12 a4 42 78 ad 34 33 c6 ad 72 c0 29 da 41 2e
                       00 06 00 12 e3 83 9e e3 83 88 e3 83 aa e3 83 83 e3 82 af e3 82 b9 00 00
                       00 15 00 1c 66 2f 2f 34 39 39 6b 39 35 34 64 36 4f 4c 33 34 6f 4c 39 46 53 54 76 79 36 34 73 41
                       00 14 00 0b 65 78 61 6d 70 6c 65 2e 6f 72 67 00
                       00 08 00 14 f6 70 24 65 6d d6 4a 3e 02 b8 e0 71 2e 85 c9 a2 8c a8 96 66");
        let m = Message::parse(raw).unwrap();
        assert_eq!(m.username(), Some(MATRIX_USER));
        assert_eq!(m.nonce(), Some("f//499k954d6OL34oL9FSTvy64sA"));
        assert_eq!(m.realm(), Some("example.org"));
        let key = long_term_key(
            MATRIX_USER,
            "example.org",
            "TheMatrIX",
            PasswordAlgorithm::Md5,
        );
        m.verify_integrity(&key).unwrap();
    }

    #[test]
    fn rfc8489_sha256_userhash() {
        // RFC の例はヘッダーの長さ 0x9c が載っている属性の合計（0x88）と合わない（20 バイト分の属性が
        // 例から抜けている）。長さを合計に直して解析し、HMAC は元の長さ 0x9c のヘッダーで計算されたものとして照合する。
        let raw = hex("00 01 00 88 21 12 a4 42 78 ad 34 33 c6 ad 72 c0 29 da 41 2e
                       00 1e 00 20 4a 3c f3 8f ef 69 92 bd a9 52 c6 78 04 17 da 0f 24 81 94 15 56 9e 60 b2 05 c4 6e 41 40 7f 17 04
                       00 15 00 29 6f 62 4d 61 74 4a 6f 73 32 41 41 41 43 66 2f 2f 34 39 39 6b 39 35 34 64 36 4f 4c 33 34 6f 4c 39 46 53 54 76 79 36 34 73 41 00 00 00
                       00 14 00 0b 65 78 61 6d 70 6c 65 2e 6f 72 67 00
                       00 1c 00 20 e4 68 6c 8f 0e de b5 90 13 e0 70 90 01 0a 93 ef cc bc cc 54 4c 0a 45 d9 f8 30 aa 6d 6f 73 5a 01");
        let m = Message::parse(raw).unwrap();
        let Some(Attribute::Userhash(h)) = m.get(attr_type::USERHASH) else {
            panic!()
        };
        assert_eq!(*h, userhash(MATRIX_USER, "example.org"));
        // 鍵は SHA-256（抜けている属性は PASSWORD-ALGORITHM(S) で SHA-256 を選んでいたと読める）。
        let key = long_term_key(
            MATRIX_USER,
            "example.org",
            "TheMatrIX",
            PasswordAlgorithm::Sha256,
        );
        let at = m.integrity_sha256_at.unwrap();
        let mac = hmac_sha256(&key, m.raw(), at, 0x9c + HEADER_LEN - at);
        let Some(Attribute::MessageIntegritySha256(got)) =
            m.get(attr_type::MESSAGE_INTEGRITY_SHA256)
        else {
            panic!()
        };
        assert_eq!(&mac[..], &got[..]);
        // 正しい長さで作り直した MI-SHA256 は自分の検査を通る
        let b = MessageBuilder::new(Class::Request, Method::BINDING, m.transaction_id)
            .attr(Attribute::Userhash(*h))
            .attr(Attribute::Nonce(m.nonce().unwrap().into()))
            .attr(Attribute::Realm("example.org".into()))
            .integrity_sha256(&key)
            .build()
            .unwrap();
        Message::parse(b).unwrap().verify_integrity(&key).unwrap();
    }

    #[test]
    fn strict_parsing() {
        let ok = MessageBuilder::new(Class::Request, Method::BINDING, TransactionId([3; 12]))
            .build()
            .unwrap();
        assert!(Message::parse(ok.clone()).is_ok());
        assert_eq!(
            Message::parse(ok.slice(..19)).unwrap_err(),
            StunError::TooShort
        );
        let mut v = ok.to_vec();
        v[4] = 0;
        assert_eq!(
            Message::parse(Bytes::from(v)).unwrap_err(),
            StunError::NotStun
        );
        let mut v = ok.to_vec();
        v[0] |= 0x80;
        assert_eq!(
            Message::parse(Bytes::from(v)).unwrap_err(),
            StunError::NotStun
        );
        // 長さが合わない
        let mut v = ok.to_vec();
        v[3] = 4;
        assert_eq!(
            Message::parse(Bytes::from(v)).unwrap_err(),
            StunError::BadLength
        );
        // 属性長が本体を越える
        let mut v = ok.to_vec();
        v.extend_from_slice(&[0x80, 0x22, 0x00, 0x09, b'a', b'b', b'c', b'd']);
        v[3] = 8;
        assert!(matches!(
            Message::parse(Bytes::from(v)).unwrap_err(),
            StunError::BadAttribute(0x8022)
        ));
        // FINGERPRINT の後ろに属性
        let fp = MessageBuilder::new(Class::Request, Method::BINDING, TransactionId([3; 12]))
            .fingerprint();
        let mut v = fp.build().unwrap().to_vec();
        v.extend_from_slice(&[0x80, 0x22, 0, 0]);
        let l = (v.len() - 20) as u16;
        v[2..4].copy_from_slice(&l.to_be_bytes());
        assert_eq!(
            Message::parse(Bytes::from(v)).unwrap_err(),
            StunError::BadFingerprint
        );
        // 属性が多すぎる
        let mut b = MessageBuilder::new(Class::Request, Method::BINDING, TransactionId([3; 12]));
        for _ in 0..200 {
            b = b.attr(Attribute::UseCandidate);
        }
        assert_eq!(
            Message::parse(b.build().unwrap()).unwrap_err(),
            StunError::TooManyAttributes
        );
    }

    #[test]
    fn unknown_comprehension_required_gives_420() {
        let req = MessageBuilder::new(Class::Request, Method::BINDING, TransactionId([9; 12]))
            .attr(Attribute::Unknown {
                typ: 0x0042,
                value: Bytes::from_static(b"x"),
            })
            .attr(Attribute::Unknown {
                typ: 0xC001,
                value: Bytes::new(),
            })
            .build()
            .unwrap();
        let m = Message::parse(req).unwrap();
        assert_eq!(m.unknown_comprehension_required(), &[0x0042]);
        let resp = Message::parse(
            error_response(&m, 420, "Unknown Attribute")
                .build()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(resp.class, Class::Error);
        assert_eq!(resp.error_code(), Some((420, "Unknown Attribute")));
        assert_eq!(
            resp.get(attr_type::UNKNOWN_ATTRIBUTES),
            Some(&Attribute::UnknownAttributes(vec![0x0042]))
        );
    }

    #[test]
    fn attributes_after_integrity_are_ignored() {
        let b = MessageBuilder::new(Class::Request, Method::BINDING, TransactionId([1; 12]))
            .attr(Attribute::Username("u".into()))
            .integrity(b"k")
            .attr(Attribute::Software("evil".into()))
            .integrity_sha256(b"k")
            .fingerprint()
            .build()
            .unwrap();
        let m = Message::parse(b).unwrap();
        assert!(m.software().is_none());
        m.verify_integrity(b"k").unwrap();
        assert!(m.verify_integrity(b"x").is_err());
    }

    #[test]
    fn message_type_encoding() {
        for (c, code) in [
            (Class::Request, 0x0001),
            (Class::Indication, 0x0011),
            (Class::Success, 0x0101),
            (Class::Error, 0x0111),
        ] {
            assert_eq!(encode_type(c, Method::BINDING), code);
            assert_eq!(decode_type(code), (c, Method::BINDING));
        }
        assert_eq!(encode_type(Class::Request, Method::ALLOCATE), 0x0003);
        assert_eq!(encode_type(Class::Indication, Method::DATA), 0x0017);
        assert_eq!(decode_type(0x0113), (Class::Error, Method::ALLOCATE));
        assert_eq!(
            decode_type(encode_type(Class::Error, Method(0xFFF))),
            (Class::Error, Method(0xFFF))
        );
    }

    #[test]
    fn fuzz_like_inputs_do_not_panic() {
        let base = rfc5769_request().to_vec();
        let mut seed = 1u32;
        for _ in 0..20000 {
            let mut v = base.clone();
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let i = (seed as usize >> 4) % v.len();
            v[i] = (seed >> 16) as u8;
            if seed & 1 == 0 {
                v.truncate((seed as usize >> 8) % (v.len() + 1));
            }
            let _ = Message::parse(Bytes::from(v));
        }
    }
}
