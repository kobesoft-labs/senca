//! STUN 属性（RFC 8489 14 節、ICE は RFC 8445 16.1、TURN は RFC 8656 18 節・RFC 6062）。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use bytes::Bytes;

use crate::StunError;
use crate::message::{MAGIC_COOKIE, TransactionId};

/// 属性の型番号。
pub mod attr_type {
    pub const MAPPED_ADDRESS: u16 = 0x0001;
    pub const USERNAME: u16 = 0x0006;
    pub const MESSAGE_INTEGRITY: u16 = 0x0008;
    pub const ERROR_CODE: u16 = 0x0009;
    pub const UNKNOWN_ATTRIBUTES: u16 = 0x000A;
    pub const CHANNEL_NUMBER: u16 = 0x000C;
    pub const LIFETIME: u16 = 0x000D;
    pub const XOR_PEER_ADDRESS: u16 = 0x0012;
    pub const DATA: u16 = 0x0013;
    pub const REALM: u16 = 0x0014;
    pub const NONCE: u16 = 0x0015;
    pub const XOR_RELAYED_ADDRESS: u16 = 0x0016;
    pub const REQUESTED_ADDRESS_FAMILY: u16 = 0x0017;
    pub const EVEN_PORT: u16 = 0x0018;
    pub const REQUESTED_TRANSPORT: u16 = 0x0019;
    pub const DONT_FRAGMENT: u16 = 0x001A;
    pub const MESSAGE_INTEGRITY_SHA256: u16 = 0x001C;
    pub const PASSWORD_ALGORITHM: u16 = 0x001D;
    pub const USERHASH: u16 = 0x001E;
    pub const XOR_MAPPED_ADDRESS: u16 = 0x0020;
    pub const RESERVATION_TOKEN: u16 = 0x0022;
    pub const PRIORITY: u16 = 0x0024;
    pub const USE_CANDIDATE: u16 = 0x0025;
    pub const CONNECTION_ID: u16 = 0x002A;
    pub const ADDITIONAL_ADDRESS_FAMILY: u16 = 0x8000;
    pub const ADDRESS_ERROR_CODE: u16 = 0x8001;
    pub const PASSWORD_ALGORITHMS: u16 = 0x8002;
    pub const ALTERNATE_DOMAIN: u16 = 0x8003;
    pub const ICMP: u16 = 0x8004;
    pub const SOFTWARE: u16 = 0x8022;
    pub const ALTERNATE_SERVER: u16 = 0x8023;
    pub const FINGERPRINT: u16 = 0x8028;
    pub const ICE_CONTROLLED: u16 = 0x8029;
    pub const ICE_CONTROLLING: u16 = 0x802A;
}

use attr_type as t;

/// USERNAME の上限（RFC 8489 14.3: 513 バイト未満）。
pub const MAX_USERNAME: usize = 512;
/// REALM・NONCE・SOFTWARE・理由句の上限（128 文字未満 = 最大 763 バイト）。
pub const MAX_TEXT: usize = 763;

/// アドレス族。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AddressFamily {
    V4,
    V6,
}

impl AddressFamily {
    fn code(self) -> u8 {
        match self {
            AddressFamily::V4 => 1,
            AddressFamily::V6 => 2,
        }
    }

    fn from_code(c: u8, typ: u16) -> Result<Self, StunError> {
        match c {
            1 => Ok(AddressFamily::V4),
            2 => Ok(AddressFamily::V6),
            _ => Err(StunError::BadAttribute(typ)),
        }
    }

    pub fn of(addr: &SocketAddr) -> Self {
        if addr.is_ipv4() {
            AddressFamily::V4
        } else {
            AddressFamily::V6
        }
    }
}

/// パスワードの鍵導出アルゴリズム（RFC 8489 18.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PasswordAlgorithm {
    Md5,
    Sha256,
    /// 未知のアルゴリズム（parameters は捨てる）。
    Other(u16),
}

impl PasswordAlgorithm {
    fn code(self) -> u16 {
        match self {
            PasswordAlgorithm::Md5 => 1,
            PasswordAlgorithm::Sha256 => 2,
            PasswordAlgorithm::Other(c) => c,
        }
    }

    fn from_code(c: u16) -> Self {
        match c {
            1 => PasswordAlgorithm::Md5,
            2 => PasswordAlgorithm::Sha256,
            c => PasswordAlgorithm::Other(c),
        }
    }
}

/// STUN 属性。
#[derive(Clone, PartialEq, Eq)]
pub enum Attribute {
    MappedAddress(SocketAddr),
    XorMappedAddress(SocketAddr),
    Username(String),
    /// HMAC-SHA1（20 バイト）。組み立てでは [`crate::MessageBuilder::integrity`] を使う。
    MessageIntegrity([u8; 20]),
    /// HMAC-SHA256（16〜32 バイト、4 の倍数）。
    MessageIntegritySha256(Vec<u8>),
    Fingerprint(u32),
    ErrorCode {
        code: u16,
        reason: String,
    },
    UnknownAttributes(Vec<u16>),
    Realm(String),
    Nonce(String),
    PasswordAlgorithms(Vec<PasswordAlgorithm>),
    PasswordAlgorithm(PasswordAlgorithm),
    Userhash([u8; 32]),
    Software(String),
    AlternateServer(SocketAddr),
    AlternateDomain(String),
    // ICE（RFC 8445）
    Priority(u32),
    UseCandidate,
    IceControlled(u64),
    IceControlling(u64),
    // TURN（RFC 8656・RFC 6062）
    ChannelNumber(u16),
    Lifetime(u32),
    XorPeerAddress(SocketAddr),
    Data(Bytes),
    XorRelayedAddress(SocketAddr),
    RequestedAddressFamily(AddressFamily),
    /// R bit（次のポートも予約する）。
    EvenPort(bool),
    /// プロトコル番号（UDP = 17、TCP = 6）。
    RequestedTransport(u8),
    DontFragment,
    ReservationToken([u8; 8]),
    AdditionalAddressFamily(AddressFamily),
    AddressErrorCode {
        family: AddressFamily,
        code: u16,
        reason: String,
    },
    Icmp {
        icmp_type: u8,
        icmp_code: u8,
        data: u32,
    },
    ConnectionId(u32),
    /// 未知の属性（値はそのまま）。
    Unknown {
        typ: u16,
        value: Bytes,
    },
}

impl std::fmt::Debug for Attribute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // 完全性の値・USERHASH は秘密ではないが長いので省く。DATA は中身を出さない。
            Attribute::MessageIntegrity(_) => write!(f, "MessageIntegrity(..)"),
            Attribute::MessageIntegritySha256(v) => {
                write!(f, "MessageIntegritySha256({} bytes)", v.len())
            }
            Attribute::Userhash(_) => write!(f, "Userhash(..)"),
            Attribute::Data(d) => write!(f, "Data({} bytes)", d.len()),
            Attribute::MappedAddress(a) => write!(f, "MappedAddress({a})"),
            Attribute::XorMappedAddress(a) => write!(f, "XorMappedAddress({a})"),
            Attribute::Username(s) => write!(f, "Username({s:?})"),
            Attribute::Fingerprint(v) => write!(f, "Fingerprint({v:08x})"),
            Attribute::ErrorCode { code, reason } => write!(f, "ErrorCode({code} {reason:?})"),
            Attribute::UnknownAttributes(v) => write!(f, "UnknownAttributes({v:x?})"),
            Attribute::Realm(s) => write!(f, "Realm({s:?})"),
            Attribute::Nonce(s) => write!(f, "Nonce({s:?})"),
            Attribute::PasswordAlgorithms(v) => write!(f, "PasswordAlgorithms({v:?})"),
            Attribute::PasswordAlgorithm(v) => write!(f, "PasswordAlgorithm({v:?})"),
            Attribute::Software(s) => write!(f, "Software({s:?})"),
            Attribute::AlternateServer(a) => write!(f, "AlternateServer({a})"),
            Attribute::AlternateDomain(s) => write!(f, "AlternateDomain({s:?})"),
            Attribute::Priority(p) => write!(f, "Priority({p})"),
            Attribute::UseCandidate => write!(f, "UseCandidate"),
            Attribute::IceControlled(v) => write!(f, "IceControlled({v:016x})"),
            Attribute::IceControlling(v) => write!(f, "IceControlling({v:016x})"),
            Attribute::ChannelNumber(c) => write!(f, "ChannelNumber(0x{c:04x})"),
            Attribute::Lifetime(l) => write!(f, "Lifetime({l})"),
            Attribute::XorPeerAddress(a) => write!(f, "XorPeerAddress({a})"),
            Attribute::XorRelayedAddress(a) => write!(f, "XorRelayedAddress({a})"),
            Attribute::RequestedAddressFamily(a) => write!(f, "RequestedAddressFamily({a:?})"),
            Attribute::EvenPort(r) => write!(f, "EvenPort({r})"),
            Attribute::RequestedTransport(p) => write!(f, "RequestedTransport({p})"),
            Attribute::DontFragment => write!(f, "DontFragment"),
            Attribute::ReservationToken(_) => write!(f, "ReservationToken(..)"),
            Attribute::AdditionalAddressFamily(a) => write!(f, "AdditionalAddressFamily({a:?})"),
            Attribute::AddressErrorCode {
                family,
                code,
                reason,
            } => {
                write!(f, "AddressErrorCode({family:?} {code} {reason:?})")
            }
            Attribute::Icmp {
                icmp_type,
                icmp_code,
                data,
            } => {
                write!(f, "Icmp(type={icmp_type} code={icmp_code} data={data:08x})")
            }
            Attribute::ConnectionId(c) => write!(f, "ConnectionId({c})"),
            Attribute::Unknown { typ, value } => {
                write!(f, "Unknown(0x{typ:04x}, {} bytes)", value.len())
            }
        }
    }
}

/// 理解必須の属性か（0x0000-0x7FFF）。
pub fn is_comprehension_required(typ: u16) -> bool {
    typ < 0x8000
}

/// この実装が理解する属性か。
pub fn is_known(typ: u16) -> bool {
    matches!(
        typ,
        t::MAPPED_ADDRESS
            | t::USERNAME
            | t::MESSAGE_INTEGRITY
            | t::ERROR_CODE
            | t::UNKNOWN_ATTRIBUTES
            | t::CHANNEL_NUMBER
            | t::LIFETIME
            | t::XOR_PEER_ADDRESS
            | t::DATA
            | t::REALM
            | t::NONCE
            | t::XOR_RELAYED_ADDRESS
            | t::REQUESTED_ADDRESS_FAMILY
            | t::EVEN_PORT
            | t::REQUESTED_TRANSPORT
            | t::DONT_FRAGMENT
            | t::MESSAGE_INTEGRITY_SHA256
            | t::PASSWORD_ALGORITHM
            | t::USERHASH
            | t::XOR_MAPPED_ADDRESS
            | t::RESERVATION_TOKEN
            | t::PRIORITY
            | t::USE_CANDIDATE
            | t::CONNECTION_ID
            | t::ADDITIONAL_ADDRESS_FAMILY
            | t::ADDRESS_ERROR_CODE
            | t::PASSWORD_ALGORITHMS
            | t::ALTERNATE_DOMAIN
            | t::ICMP
            | t::SOFTWARE
            | t::ALTERNATE_SERVER
            | t::FINGERPRINT
            | t::ICE_CONTROLLED
            | t::ICE_CONTROLLING
    )
}

impl Attribute {
    /// 属性の型番号。
    pub fn typ(&self) -> u16 {
        match self {
            Attribute::MappedAddress(_) => t::MAPPED_ADDRESS,
            Attribute::XorMappedAddress(_) => t::XOR_MAPPED_ADDRESS,
            Attribute::Username(_) => t::USERNAME,
            Attribute::MessageIntegrity(_) => t::MESSAGE_INTEGRITY,
            Attribute::MessageIntegritySha256(_) => t::MESSAGE_INTEGRITY_SHA256,
            Attribute::Fingerprint(_) => t::FINGERPRINT,
            Attribute::ErrorCode { .. } => t::ERROR_CODE,
            Attribute::UnknownAttributes(_) => t::UNKNOWN_ATTRIBUTES,
            Attribute::Realm(_) => t::REALM,
            Attribute::Nonce(_) => t::NONCE,
            Attribute::PasswordAlgorithms(_) => t::PASSWORD_ALGORITHMS,
            Attribute::PasswordAlgorithm(_) => t::PASSWORD_ALGORITHM,
            Attribute::Userhash(_) => t::USERHASH,
            Attribute::Software(_) => t::SOFTWARE,
            Attribute::AlternateServer(_) => t::ALTERNATE_SERVER,
            Attribute::AlternateDomain(_) => t::ALTERNATE_DOMAIN,
            Attribute::Priority(_) => t::PRIORITY,
            Attribute::UseCandidate => t::USE_CANDIDATE,
            Attribute::IceControlled(_) => t::ICE_CONTROLLED,
            Attribute::IceControlling(_) => t::ICE_CONTROLLING,
            Attribute::ChannelNumber(_) => t::CHANNEL_NUMBER,
            Attribute::Lifetime(_) => t::LIFETIME,
            Attribute::XorPeerAddress(_) => t::XOR_PEER_ADDRESS,
            Attribute::Data(_) => t::DATA,
            Attribute::XorRelayedAddress(_) => t::XOR_RELAYED_ADDRESS,
            Attribute::RequestedAddressFamily(_) => t::REQUESTED_ADDRESS_FAMILY,
            Attribute::EvenPort(_) => t::EVEN_PORT,
            Attribute::RequestedTransport(_) => t::REQUESTED_TRANSPORT,
            Attribute::DontFragment => t::DONT_FRAGMENT,
            Attribute::ReservationToken(_) => t::RESERVATION_TOKEN,
            Attribute::AdditionalAddressFamily(_) => t::ADDITIONAL_ADDRESS_FAMILY,
            Attribute::AddressErrorCode { .. } => t::ADDRESS_ERROR_CODE,
            Attribute::Icmp { .. } => t::ICMP,
            Attribute::ConnectionId(_) => t::CONNECTION_ID,
            Attribute::Unknown { typ, .. } => *typ,
        }
    }

    /// 値を解析する。`value` は padding を除いた値。
    pub(crate) fn decode(
        typ: u16,
        value: Bytes,
        tid: &TransactionId,
    ) -> Result<Attribute, StunError> {
        let v = &value[..];
        let bad = || StunError::BadAttribute(typ);
        let fixed = |n: usize| {
            if v.len() == n {
                Ok(())
            } else {
                Err(StunError::BadAttribute(typ))
            }
        };
        let be32 = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        Ok(match typ {
            t::MAPPED_ADDRESS => Attribute::MappedAddress(decode_addr(typ, v, None)?),
            t::XOR_MAPPED_ADDRESS => Attribute::XorMappedAddress(decode_addr(typ, v, Some(tid))?),
            t::ALTERNATE_SERVER => Attribute::AlternateServer(decode_addr(typ, v, None)?),
            t::XOR_PEER_ADDRESS => Attribute::XorPeerAddress(decode_addr(typ, v, Some(tid))?),
            t::XOR_RELAYED_ADDRESS => Attribute::XorRelayedAddress(decode_addr(typ, v, Some(tid))?),
            t::USERNAME => Attribute::Username(decode_text(typ, v, MAX_USERNAME)?),
            t::REALM => Attribute::Realm(decode_text(typ, v, MAX_TEXT)?),
            t::NONCE => Attribute::Nonce(decode_text(typ, v, MAX_TEXT)?),
            t::SOFTWARE => Attribute::Software(decode_text(typ, v, MAX_TEXT)?),
            t::ALTERNATE_DOMAIN => {
                let s = decode_text(typ, v, 255)?;
                if !s.is_ascii() {
                    return Err(bad());
                }
                Attribute::AlternateDomain(s)
            }
            t::MESSAGE_INTEGRITY => {
                fixed(20)?;
                let mut a = [0u8; 20];
                a.copy_from_slice(v);
                Attribute::MessageIntegrity(a)
            }
            t::MESSAGE_INTEGRITY_SHA256 => {
                if !(16..=32).contains(&v.len()) || !v.len().is_multiple_of(4) {
                    return Err(bad());
                }
                Attribute::MessageIntegritySha256(v.to_vec())
            }
            t::FINGERPRINT => {
                fixed(4)?;
                Attribute::Fingerprint(be32(v))
            }
            t::ERROR_CODE => {
                let (code, reason) = decode_error(typ, v)?;
                Attribute::ErrorCode { code, reason }
            }
            t::UNKNOWN_ATTRIBUTES => {
                if !v.len().is_multiple_of(2) {
                    return Err(bad());
                }
                Attribute::UnknownAttributes(
                    v.as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_be_bytes([c[0], c[1]]))
                        .collect(),
                )
            }
            t::PASSWORD_ALGORITHMS => {
                let mut algs = Vec::new();
                let mut rest = v;
                while !rest.is_empty() {
                    let (alg, used) = decode_password_algorithm(typ, rest)?;
                    algs.push(alg);
                    rest = &rest[used..];
                }
                if algs.is_empty() {
                    return Err(bad());
                }
                Attribute::PasswordAlgorithms(algs)
            }
            t::PASSWORD_ALGORITHM => {
                let (alg, used) = decode_password_algorithm(typ, v)?;
                // 値の長さは padding を除くので、params の padding 分だけ短いことがある。
                if used < v.len() {
                    return Err(bad());
                }
                Attribute::PasswordAlgorithm(alg)
            }
            t::USERHASH => {
                fixed(32)?;
                let mut a = [0u8; 32];
                a.copy_from_slice(v);
                Attribute::Userhash(a)
            }
            t::PRIORITY => {
                fixed(4)?;
                Attribute::Priority(be32(v))
            }
            t::USE_CANDIDATE => {
                fixed(0)?;
                Attribute::UseCandidate
            }
            t::ICE_CONTROLLED | t::ICE_CONTROLLING => {
                fixed(8)?;
                let x = u64::from_be_bytes([v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7]]);
                if typ == t::ICE_CONTROLLED {
                    Attribute::IceControlled(x)
                } else {
                    Attribute::IceControlling(x)
                }
            }
            t::CHANNEL_NUMBER => {
                fixed(4)?;
                Attribute::ChannelNumber(u16::from_be_bytes([v[0], v[1]]))
            }
            t::LIFETIME => {
                fixed(4)?;
                Attribute::Lifetime(be32(v))
            }
            t::DATA => Attribute::Data(value),
            t::REQUESTED_ADDRESS_FAMILY | t::ADDITIONAL_ADDRESS_FAMILY => {
                fixed(4)?;
                let fam = AddressFamily::from_code(v[0], typ)?;
                if typ == t::REQUESTED_ADDRESS_FAMILY {
                    Attribute::RequestedAddressFamily(fam)
                } else {
                    Attribute::AdditionalAddressFamily(fam)
                }
            }
            t::EVEN_PORT => {
                fixed(1)?;
                Attribute::EvenPort(v[0] & 0x80 != 0)
            }
            t::REQUESTED_TRANSPORT => {
                fixed(4)?;
                Attribute::RequestedTransport(v[0])
            }
            t::DONT_FRAGMENT => {
                fixed(0)?;
                Attribute::DontFragment
            }
            t::RESERVATION_TOKEN => {
                fixed(8)?;
                let mut a = [0u8; 8];
                a.copy_from_slice(v);
                Attribute::ReservationToken(a)
            }
            t::ADDRESS_ERROR_CODE => {
                if v.len() < 4 {
                    return Err(bad());
                }
                let family = AddressFamily::from_code(v[0], typ)?;
                let mut tmp = [0u8; 4];
                tmp[2..].copy_from_slice(&v[2..4]);
                let mut buf = tmp.to_vec();
                buf.extend_from_slice(&v[4..]);
                let (code, reason) = decode_error(typ, &buf)?;
                Attribute::AddressErrorCode {
                    family,
                    code,
                    reason,
                }
            }
            t::ICMP => {
                fixed(8)?;
                Attribute::Icmp {
                    icmp_type: v[2],
                    icmp_code: v[3],
                    data: be32(&v[4..]),
                }
            }
            t::CONNECTION_ID => {
                fixed(4)?;
                Attribute::ConnectionId(be32(v))
            }
            _ => Attribute::Unknown { typ, value },
        })
    }

    /// TLV（padding 込み）を `out` に書く。
    pub(crate) fn encode(&self, tid: &TransactionId, out: &mut Vec<u8>) -> Result<(), StunError> {
        let start = out.len();
        out.extend_from_slice(&self.typ().to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        match self {
            Attribute::MappedAddress(a) | Attribute::AlternateServer(a) => {
                encode_addr(a, None, out)
            }
            Attribute::XorMappedAddress(a)
            | Attribute::XorPeerAddress(a)
            | Attribute::XorRelayedAddress(a) => encode_addr(a, Some(tid), out),
            Attribute::Username(s)
            | Attribute::Realm(s)
            | Attribute::Nonce(s)
            | Attribute::Software(s)
            | Attribute::AlternateDomain(s) => out.extend_from_slice(s.as_bytes()),
            Attribute::MessageIntegrity(v) => out.extend_from_slice(v),
            Attribute::MessageIntegritySha256(v) => out.extend_from_slice(v),
            Attribute::Fingerprint(v) => out.extend_from_slice(&v.to_be_bytes()),
            Attribute::ErrorCode { code, reason } => encode_error(*code, reason, out)?,
            Attribute::UnknownAttributes(v) => {
                for x in v {
                    out.extend_from_slice(&x.to_be_bytes());
                }
            }
            Attribute::PasswordAlgorithms(v) => {
                for a in v {
                    out.extend_from_slice(&a.code().to_be_bytes());
                    out.extend_from_slice(&[0, 0]);
                }
            }
            Attribute::PasswordAlgorithm(a) => {
                out.extend_from_slice(&a.code().to_be_bytes());
                out.extend_from_slice(&[0, 0]);
            }
            Attribute::Userhash(v) => out.extend_from_slice(v),
            Attribute::Priority(p) => out.extend_from_slice(&p.to_be_bytes()),
            Attribute::UseCandidate | Attribute::DontFragment => {}
            Attribute::IceControlled(v) | Attribute::IceControlling(v) => {
                out.extend_from_slice(&v.to_be_bytes())
            }
            Attribute::ChannelNumber(c) => {
                out.extend_from_slice(&c.to_be_bytes());
                out.extend_from_slice(&[0, 0]);
            }
            Attribute::Lifetime(l) => out.extend_from_slice(&l.to_be_bytes()),
            Attribute::Data(d) => out.extend_from_slice(d),
            Attribute::RequestedAddressFamily(f) | Attribute::AdditionalAddressFamily(f) => {
                out.extend_from_slice(&[f.code(), 0, 0, 0])
            }
            Attribute::EvenPort(r) => out.push(if *r { 0x80 } else { 0 }),
            Attribute::RequestedTransport(p) => out.extend_from_slice(&[*p, 0, 0, 0]),
            Attribute::ReservationToken(v) => out.extend_from_slice(v),
            Attribute::AddressErrorCode {
                family,
                code,
                reason,
            } => {
                let s = out.len();
                encode_error(*code, reason, out)?;
                out[s] = family.code();
            }
            Attribute::Icmp {
                icmp_type,
                icmp_code,
                data,
            } => {
                out.extend_from_slice(&[0, 0, *icmp_type, *icmp_code]);
                out.extend_from_slice(&data.to_be_bytes());
            }
            Attribute::ConnectionId(c) => out.extend_from_slice(&c.to_be_bytes()),
            Attribute::Unknown { value, .. } => out.extend_from_slice(value),
        }
        let len = out.len() - start - 4;
        if len > 0xFFFF {
            out.truncate(start);
            return Err(StunError::TooLarge);
        }
        out[start + 2..start + 4].copy_from_slice(&(len as u16).to_be_bytes());
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        Ok(())
    }
}

fn decode_text(typ: u16, v: &[u8], max: usize) -> Result<String, StunError> {
    if v.len() > max {
        return Err(StunError::BadAttribute(typ));
    }
    String::from_utf8(v.to_vec()).map_err(|_| StunError::BadAttribute(typ))
}

fn decode_error(typ: u16, v: &[u8]) -> Result<(u16, String), StunError> {
    if v.len() < 4 {
        return Err(StunError::BadAttribute(typ));
    }
    let class = (v[2] & 0x07) as u16;
    let number = v[3] as u16;
    if !(3..=6).contains(&class) || number > 99 {
        return Err(StunError::BadAttribute(typ));
    }
    let reason = decode_text(typ, &v[4..], MAX_TEXT)?;
    Ok((class * 100 + number, reason))
}

fn encode_error(code: u16, reason: &str, out: &mut Vec<u8>) -> Result<(), StunError> {
    if !(300..=699).contains(&code) || reason.len() > MAX_TEXT {
        return Err(StunError::BadAttribute(attr_type::ERROR_CODE));
    }
    out.extend_from_slice(&[0, 0, (code / 100) as u8, (code % 100) as u8]);
    out.extend_from_slice(reason.as_bytes());
    Ok(())
}

/// 1 つ分の PASSWORD-ALGORITHM を読む。使ったバイト数（params の padding 込み、ただし値の末尾を越えない）を返す。
fn decode_password_algorithm(typ: u16, v: &[u8]) -> Result<(PasswordAlgorithm, usize), StunError> {
    if v.len() < 4 {
        return Err(StunError::BadAttribute(typ));
    }
    let alg = u16::from_be_bytes([v[0], v[1]]);
    let plen = u16::from_be_bytes([v[2], v[3]]) as usize;
    let end = 4 + plen;
    if end > v.len() {
        return Err(StunError::BadAttribute(typ));
    }
    let padded = (end + 3) & !3;
    Ok((PasswordAlgorithm::from_code(alg), padded.min(v.len())))
}

fn decode_addr(typ: u16, v: &[u8], xor: Option<&TransactionId>) -> Result<SocketAddr, StunError> {
    if v.len() < 4 {
        return Err(StunError::BadAttribute(typ));
    }
    let family = AddressFamily::from_code(v[1], typ)?;
    let mut port = u16::from_be_bytes([v[2], v[3]]);
    if xor.is_some() {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    let ip = match family {
        AddressFamily::V4 => {
            if v.len() != 8 {
                return Err(StunError::BadAttribute(typ));
            }
            let mut a = u32::from_be_bytes([v[4], v[5], v[6], v[7]]);
            if xor.is_some() {
                a ^= MAGIC_COOKIE;
            }
            IpAddr::V4(Ipv4Addr::from(a))
        }
        AddressFamily::V6 => {
            if v.len() != 20 {
                return Err(StunError::BadAttribute(typ));
            }
            let mut a = [0u8; 16];
            a.copy_from_slice(&v[4..20]);
            if let Some(tid) = xor {
                let key = xor_key(tid);
                for (x, k) in a.iter_mut().zip(key.iter()) {
                    *x ^= k;
                }
            }
            IpAddr::V6(Ipv6Addr::from(a))
        }
    };
    Ok(SocketAddr::new(ip, port))
}

fn encode_addr(a: &SocketAddr, xor: Option<&TransactionId>, out: &mut Vec<u8>) {
    let mut port = a.port();
    if xor.is_some() {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    out.push(0);
    out.push(AddressFamily::of(a).code());
    out.extend_from_slice(&port.to_be_bytes());
    match a.ip() {
        IpAddr::V4(ip) => {
            let mut x = u32::from(ip);
            if xor.is_some() {
                x ^= MAGIC_COOKIE;
            }
            out.extend_from_slice(&x.to_be_bytes());
        }
        IpAddr::V6(ip) => {
            let mut x = ip.octets();
            if let Some(tid) = xor {
                let key = xor_key(tid);
                for (b, k) in x.iter_mut().zip(key.iter()) {
                    *b ^= k;
                }
            }
            out.extend_from_slice(&x);
        }
    }
}

fn xor_key(tid: &TransactionId) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    k[4..].copy_from_slice(&tid.0);
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(a: Attribute) {
        let tid = TransactionId([7; 12]);
        let mut out = Vec::new();
        a.encode(&tid, &mut out).unwrap();
        assert_eq!(out.len() % 4, 0);
        let typ = u16::from_be_bytes([out[0], out[1]]);
        let len = u16::from_be_bytes([out[2], out[3]]) as usize;
        let back = Attribute::decode(typ, Bytes::copy_from_slice(&out[4..4 + len]), &tid).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn attribute_roundtrips() {
        roundtrip(Attribute::XorMappedAddress(
            "192.0.2.1:32853".parse().unwrap(),
        ));
        roundtrip(Attribute::XorPeerAddress(
            "[2001:db8::1]:9".parse().unwrap(),
        ));
        roundtrip(Attribute::MappedAddress("[::1]:1".parse().unwrap()));
        roundtrip(Attribute::ErrorCode {
            code: 487,
            reason: "Role Conflict".into(),
        });
        roundtrip(Attribute::UnknownAttributes(vec![0x1234, 0x0042]));
        roundtrip(Attribute::PasswordAlgorithms(vec![
            PasswordAlgorithm::Sha256,
            PasswordAlgorithm::Md5,
        ]));
        roundtrip(Attribute::PasswordAlgorithm(PasswordAlgorithm::Sha256));
        roundtrip(Attribute::IceControlling(0x0102030405060708));
        roundtrip(Attribute::UseCandidate);
        roundtrip(Attribute::ChannelNumber(0x4001));
        roundtrip(Attribute::EvenPort(true));
        roundtrip(Attribute::RequestedTransport(17));
        roundtrip(Attribute::RequestedAddressFamily(AddressFamily::V6));
        roundtrip(Attribute::AddressErrorCode {
            family: AddressFamily::V6,
            code: 440,
            reason: "x".into(),
        });
        roundtrip(Attribute::Icmp {
            icmp_type: 3,
            icmp_code: 1,
            data: 5,
        });
        roundtrip(Attribute::Data(Bytes::from_static(b"hello")));
    }

    #[test]
    fn rejects_bad_values() {
        let tid = TransactionId([0; 12]);
        // IPv4 で 20 バイト
        let mut v = vec![0, 1, 0, 0];
        v.extend_from_slice(&[0; 16]);
        assert!(Attribute::decode(t::XOR_MAPPED_ADDRESS, Bytes::from(v), &tid).is_err());
        // 未知のアドレス族
        assert!(
            Attribute::decode(
                t::MAPPED_ADDRESS,
                Bytes::from_static(&[0, 3, 0, 0, 1, 2, 3, 4]),
                &tid
            )
            .is_err()
        );
        // ERROR-CODE の class 外
        assert!(Attribute::decode(t::ERROR_CODE, Bytes::from_static(&[0, 0, 7, 0]), &tid).is_err());
        assert!(
            Attribute::decode(t::ERROR_CODE, Bytes::from_static(&[0, 0, 4, 100]), &tid).is_err()
        );
        // MI-SHA256 の長さ
        assert!(
            Attribute::decode(t::MESSAGE_INTEGRITY_SHA256, Bytes::from(vec![0; 12]), &tid).is_err()
        );
        assert!(
            Attribute::decode(t::MESSAGE_INTEGRITY_SHA256, Bytes::from(vec![0; 18]), &tid).is_err()
        );
        // USERNAME の長さ・UTF-8
        assert!(Attribute::decode(t::USERNAME, Bytes::from(vec![b'a'; 513]), &tid).is_err());
        assert!(Attribute::decode(t::USERNAME, Bytes::from_static(&[0xff, 0xfe]), &tid).is_err());
        // PASSWORD-ALGORITHMS の params が溢れる
        assert!(
            Attribute::decode(
                t::PASSWORD_ALGORITHMS,
                Bytes::from_static(&[0, 1, 0, 8, 0, 0]),
                &tid
            )
            .is_err()
        );
        assert!(Attribute::decode(t::PRIORITY, Bytes::from_static(&[0, 1, 0]), &tid).is_err());
        assert!(Attribute::decode(t::USE_CANDIDATE, Bytes::from_static(&[0]), &tid).is_err());
    }
}
