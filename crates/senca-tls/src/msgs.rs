//! ハンドシェイクメッセージの解析と組み立て（RFC 8446 4 章）。

use crate::codec::{Reader, len_prefixed, put_u16, put_u24, put_vec8, put_vec16};
use crate::error::{AlertDescription, Error};

pub(crate) mod hs {
    pub const CLIENT_HELLO: u8 = 1;
    pub const SERVER_HELLO: u8 = 2;
    pub const NEW_SESSION_TICKET: u8 = 4;
    pub const ENCRYPTED_EXTENSIONS: u8 = 8;
    pub const CERTIFICATE: u8 = 11;
    pub const CERTIFICATE_REQUEST: u8 = 13;
    pub const CERTIFICATE_VERIFY: u8 = 15;
    pub const FINISHED: u8 = 20;
    pub const KEY_UPDATE: u8 = 24;
    pub const MESSAGE_HASH: u8 = 254;
}

pub(crate) mod ext {
    pub const SERVER_NAME: u16 = 0;
    pub const SUPPORTED_GROUPS: u16 = 10;
    pub const SIGNATURE_ALGORITHMS: u16 = 13;
    pub const ALPN: u16 = 16;
    pub const RECORD_SIZE_LIMIT: u16 = 28;
    pub const PRE_SHARED_KEY: u16 = 41;
    pub const EARLY_DATA: u16 = 42;
    pub const SUPPORTED_VERSIONS: u16 = 43;
    pub const COOKIE: u16 = 44;
    pub const PSK_KEY_EXCHANGE_MODES: u16 = 45;
    pub const KEY_SHARE: u16 = 51;
    pub const QUIC_TRANSPORT_PARAMETERS: u16 = 0x39;
}

pub(crate) const TLS13: u16 = 0x0304;
pub(crate) const LEGACY_VERSION: u16 = 0x0303;
pub(crate) const PSK_DHE_KE: u8 = 1;

/// HelloRetryRequest を示す ServerHello.random（RFC 8446 4.1.3）。
pub(crate) const HRR_RANDOM: [u8; 32] = [
    0xcf, 0x21, 0xad, 0x74, 0xe5, 0x9a, 0x61, 0x11, 0xbe, 0x1d, 0x8c, 0x02, 0x1e, 0x65, 0xb8, 0x91, 0xc2, 0xa2, 0x11,
    0x16, 0x7a, 0xbb, 0x8c, 0x5e, 0x07, 0x9e, 0x09, 0xe2, 0xc8, 0xa8, 0x33, 0x9c,
];

/// TLS 1.2 以下への格下げを示す ServerHello.random の末尾 8 バイト（RFC 8446 4.1.3）。
pub(crate) const DOWNGRADE_TLS12: [u8; 8] = [0x44, 0x4f, 0x57, 0x4e, 0x47, 0x52, 0x44, 0x01];
pub(crate) const DOWNGRADE_TLS11: [u8; 8] = [0x44, 0x4f, 0x57, 0x4e, 0x47, 0x52, 0x44, 0x00];

/// 一つのメッセージに許す拡張の数。
const MAX_EXTENSIONS: usize = 64;
/// 証明書チェーンの最大本数。
pub(crate) const MAX_CHAIN_LEN: usize = 16;

/// ハンドシェイクヘッダーを付ける。
pub(crate) fn handshake(typ: u8, body: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.push(typ);
    if body.len() > 0xff_ffff {
        return Err(Error::internal("メッセージが大きすぎる"));
    }
    put_u24(&mut out, body.len() as u32);
    out.extend_from_slice(body);
    Ok(out)
}

/// 拡張を一つ書く。
pub(crate) fn put_ext(
    out: &mut Vec<u8>,
    typ: u16,
    f: impl FnOnce(&mut Vec<u8>) -> Result<(), Error>,
) -> Result<(), Error> {
    put_u16(out, typ);
    len_prefixed(out, 2, f)
}

/// 拡張の並びを読む。重複は illegal_parameter。
pub(crate) fn parse_extensions(data: &[u8]) -> Result<Vec<(u16, &[u8])>, Error> {
    let mut r = Reader::new(data);
    let mut v: Vec<(u16, &[u8])> = Vec::new();
    while !r.is_empty() {
        let t = r.u16()?;
        let body = r.vec16()?;
        if v.iter().any(|(x, _)| *x == t) {
            return Err(Error::illegal("拡張が重複している"));
        }
        if v.len() >= MAX_EXTENSIONS {
            return Err(Error::decode("拡張が多すぎる"));
        }
        v.push((t, body));
    }
    Ok(v)
}

fn u16_list(data: &[u8], min_bytes: usize) -> Result<Vec<u16>, Error> {
    let mut r = Reader::new(data);
    let l = r.vec16()?;
    r.finish()?;
    if l.len() < min_bytes || l.len() % 2 != 0 {
        return Err(Error::decode("u16 の並びの長さが不正"));
    }
    Ok(l.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect())
}

/// ALPN のプロトコル名の並び（RFC 7301）。空の名前・空の並びは decode_error。
pub(crate) fn parse_alpn(data: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    let mut r = Reader::new(data);
    let l = r.vec16()?;
    r.finish()?;
    let mut lr = Reader::new(l);
    let mut v = Vec::new();
    while !lr.is_empty() {
        let p = lr.vec8()?;
        if p.is_empty() {
            return Err(Error::decode("ALPN の名前が空"));
        }
        v.push(p.to_vec());
    }
    if v.is_empty() {
        return Err(Error::decode("ALPN の並びが空"));
    }
    Ok(v)
}

pub(crate) fn put_alpn(out: &mut Vec<u8>, protos: &[Vec<u8>]) -> Result<(), Error> {
    len_prefixed(out, 2, |o| {
        for p in protos {
            put_vec8(o, p)?;
        }
        Ok(())
    })
}

fn parse_record_size_limit(data: &[u8]) -> Result<u16, Error> {
    let mut r = Reader::new(data);
    let v = r.u16()?;
    r.finish()?;
    // RFC 8449 4: 64 未満は illegal_parameter。
    if v < 64 {
        return Err(Error::illegal("record_size_limit が 64 未満"));
    }
    Ok(v)
}

/// 提示された PSK（RFC 8446 4.2.11）。
pub(crate) struct OfferedPsk {
    pub identities: Vec<(Vec<u8>, u32)>,
    pub binders: Vec<Vec<u8>>,
    /// binder の並びの直前までの長さ（ハンドシェイクヘッダーを含む）。
    pub truncated_len: usize,
}

pub(crate) struct ClientHello {
    pub random: [u8; 32],
    pub session_id: Vec<u8>,
    pub cipher_suites: Vec<u16>,
    pub server_name: Option<String>,
    pub supported_versions: Option<Vec<u16>>,
    pub supported_groups: Option<Vec<u16>>,
    pub signature_algorithms: Option<Vec<u16>>,
    pub key_shares: Option<Vec<(u16, Vec<u8>)>>,
    pub alpn: Option<Vec<Vec<u8>>>,
    pub psk_modes: Option<Vec<u8>>,
    pub psk: Option<OfferedPsk>,
    pub early_data: bool,
    pub cookie: Option<Vec<u8>>,
    pub record_size_limit: Option<u16>,
    pub quic_transport_parameters: Option<Vec<u8>>,
}

/// ClientHello を読む。`msg` はハンドシェイクヘッダーを含む全体。
pub(crate) fn parse_client_hello(msg: &[u8]) -> Result<ClientHello, Error> {
    let mut r = Reader::new(msg.get(4..).ok_or(Error::decode("短い"))?);
    let _legacy_version = r.u16()?;
    let mut random = [0u8; 32];
    random.copy_from_slice(r.take(32)?);
    let session_id = r.vec8()?;
    if session_id.len() > 32 {
        return Err(Error::illegal("legacy_session_id が長すぎる"));
    }
    let suites = r.vec16()?;
    if suites.is_empty() || suites.len() % 2 != 0 {
        return Err(Error::decode("cipher_suites の長さが不正"));
    }
    let compression = r.vec8()?;
    let ext_start = 4 + r.pos();
    let ext_data = if r.is_empty() { &[][..] } else { r.vec16()? };
    r.finish()?;
    let exts = parse_extensions(ext_data)?;

    let mut ch = ClientHello {
        random,
        session_id: session_id.to_vec(),
        cipher_suites: suites.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect(),
        server_name: None,
        supported_versions: None,
        supported_groups: None,
        signature_algorithms: None,
        key_shares: None,
        alpn: None,
        psk_modes: None,
        psk: None,
        early_data: false,
        cookie: None,
        record_size_limit: None,
        quic_transport_parameters: None,
    };

    // 拡張の中身の位置（binder の切り詰め位置を求めるため）。
    let mut off = ext_start + 2;
    let n = exts.len();
    for (i, (t, body)) in exts.into_iter().enumerate() {
        let body_off = off + 4;
        off = body_off + body.len();
        match t {
            ext::SERVER_NAME => ch.server_name = Some(parse_server_name(body)?),
            ext::SUPPORTED_VERSIONS => {
                let mut br = Reader::new(body);
                let l = br.vec8()?;
                br.finish()?;
                if l.len() < 2 || l.len() % 2 != 0 {
                    return Err(Error::decode("supported_versions の長さが不正"));
                }
                ch.supported_versions = Some(l.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect());
            }
            ext::SUPPORTED_GROUPS => ch.supported_groups = Some(u16_list(body, 2)?),
            ext::SIGNATURE_ALGORITHMS => ch.signature_algorithms = Some(u16_list(body, 2)?),
            ext::KEY_SHARE => {
                let mut br = Reader::new(body);
                let l = br.vec16()?;
                br.finish()?;
                let mut lr = Reader::new(l);
                let mut v: Vec<(u16, Vec<u8>)> = Vec::new();
                while !lr.is_empty() {
                    let g = lr.u16()?;
                    let k = lr.vec16()?;
                    if k.is_empty() {
                        return Err(Error::decode("key_exchange が空"));
                    }
                    if v.iter().any(|(x, _)| *x == g) {
                        return Err(Error::illegal("key_share の group が重複"));
                    }
                    if v.len() >= 16 {
                        return Err(Error::illegal("key_share が多すぎる"));
                    }
                    v.push((g, k.to_vec()));
                }
                ch.key_shares = Some(v);
            }
            ext::ALPN => ch.alpn = Some(parse_alpn(body)?),
            ext::PSK_KEY_EXCHANGE_MODES => {
                let mut br = Reader::new(body);
                let l = br.vec8()?;
                br.finish()?;
                if l.is_empty() {
                    return Err(Error::decode("psk_key_exchange_modes が空"));
                }
                ch.psk_modes = Some(l.to_vec());
            }
            ext::PRE_SHARED_KEY => {
                // 最後の拡張でなければならない（RFC 8446 4.2.11）。
                if i + 1 != n {
                    return Err(Error::illegal("pre_shared_key が最後の拡張でない"));
                }
                let mut br = Reader::new(body);
                let ids = br.vec16()?;
                let binders_pos = body_off + br.pos();
                let binders = br.vec16()?;
                br.finish()?;
                let mut ir = Reader::new(ids);
                let mut identities = Vec::new();
                while !ir.is_empty() {
                    let id = ir.vec16()?;
                    let age = ir.u32()?;
                    if id.is_empty() {
                        return Err(Error::decode("PSK identity が空"));
                    }
                    identities.push((id.to_vec(), age));
                }
                let mut brr = Reader::new(binders);
                let mut bv = Vec::new();
                while !brr.is_empty() {
                    let b = brr.vec8()?;
                    if b.len() < 32 {
                        return Err(Error::decode("PSK binder が短い"));
                    }
                    bv.push(b.to_vec());
                }
                if identities.is_empty() || identities.len() != bv.len() {
                    return Err(Error::illegal("PSK の identity と binder の数が合わない"));
                }
                ch.psk = Some(OfferedPsk {
                    identities,
                    binders: bv,
                    truncated_len: binders_pos,
                });
            }
            ext::EARLY_DATA => {
                if !body.is_empty() {
                    return Err(Error::decode("early_data の中身が空でない"));
                }
                ch.early_data = true;
            }
            ext::COOKIE => {
                let mut br = Reader::new(body);
                let c = br.vec16()?;
                br.finish()?;
                ch.cookie = Some(c.to_vec());
            }
            ext::RECORD_SIZE_LIMIT => ch.record_size_limit = Some(parse_record_size_limit(body)?),
            ext::QUIC_TRANSPORT_PARAMETERS => ch.quic_transport_parameters = Some(body.to_vec()),
            _ => {} // 知らない拡張は無視する（RFC 8446 4.2）。
        }
    }
    // 圧縮は null だけ（RFC 8446 4.1.2）。
    if ch
        .supported_versions
        .as_ref()
        .is_some_and(|v| v.contains(&TLS13))
        && compression != [0]
    {
        return Err(Error::illegal("legacy_compression_methods が null だけでない"));
    }
    Ok(ch)
}

/// server_name（RFC 6066 3）。host_name を一つだけ受け付ける。
fn parse_server_name(body: &[u8]) -> Result<String, Error> {
    let mut r = Reader::new(body);
    let l = r.vec16()?;
    r.finish()?;
    let mut lr = Reader::new(l);
    let mut name = None;
    while !lr.is_empty() {
        let t = lr.u8()?;
        let n = lr.vec16()?;
        if t == 0 {
            if name.is_some() {
                return Err(Error::illegal("host_name が複数ある"));
            }
            name = Some(n);
        }
    }
    let n = name.ok_or(Error::decode("host_name がない"))?;
    let s = std::str::from_utf8(n).map_err(|_| Error::illegal("host_name が ASCII でない"))?;
    if s.is_empty() || !s.is_ascii() {
        return Err(Error::illegal("host_name が不正"));
    }
    // 末尾の点は取り除いて比較する。
    let s = s.strip_suffix('.').unwrap_or(s);
    if pki_types::DnsName::try_from(s).is_err() || s.parse::<std::net::IpAddr>().is_ok() {
        return Err(Error::illegal("host_name が DNS 名でない"));
    }
    Ok(s.to_ascii_lowercase())
}

pub(crate) fn put_server_name(out: &mut Vec<u8>, name: &str) -> Result<(), Error> {
    len_prefixed(out, 2, |o| {
        o.push(0);
        put_vec16(o, name.as_bytes())
    })
}

pub(crate) struct ServerHello {
    pub session_id: Vec<u8>,
    pub cipher_suite: u16,
    pub is_hrr: bool,
    pub key_share: Option<(u16, Vec<u8>)>,
    pub psk_identity: Option<u16>,
    pub cookie: Option<Vec<u8>>,
}

/// ServerHello / HelloRetryRequest を読む。TLS 1.3 以外は拒否する。
pub(crate) fn parse_server_hello(msg: &[u8]) -> Result<ServerHello, Error> {
    let mut r = Reader::new(msg.get(4..).ok_or(Error::decode("短い"))?);
    let legacy_version = r.u16()?;
    let mut random = [0u8; 32];
    random.copy_from_slice(r.take(32)?);
    let session_id = r.vec8()?.to_vec();
    if session_id.len() > 32 {
        return Err(Error::illegal("legacy_session_id_echo が長すぎる"));
    }
    let cipher_suite = r.u16()?;
    let compression = r.u8()?;
    let ext_data = if r.is_empty() { &[][..] } else { r.vec16()? };
    r.finish()?;
    let exts = parse_extensions(ext_data)?;
    let is_hrr = random == HRR_RANDOM;

    // supported_versions を先に見て、TLS 1.2 以下への格下げを拒否する。
    let version = match exts.iter().find(|(t, _)| *t == ext::SUPPORTED_VERSIONS) {
        Some((_, b)) => {
            let mut br = Reader::new(b);
            let v = br.u16()?;
            br.finish()?;
            Some(v)
        }
        None => None,
    };
    if version != Some(TLS13) {
        // TLS 1.3 を提示した client が格下げ番兵を見たら illegal_parameter（RFC 8446 4.1.3）。
        if random[24..] == DOWNGRADE_TLS12 || random[24..] == DOWNGRADE_TLS11 {
            return Err(Error::illegal("格下げ番兵を検出"));
        }
        return Err(Error::fatal(AlertDescription::PROTOCOL_VERSION, "TLS 1.3 以外の ServerHello"));
    }
    if legacy_version != LEGACY_VERSION {
        return Err(Error::illegal("ServerHello.legacy_version が 0x0303 でない"));
    }
    if compression != 0 {
        return Err(Error::illegal("legacy_compression_method が 0 でない"));
    }
    // TLS 1.3 なのに番兵が付いているのは不正（偶然一致の確率は 2^-64）。
    if !is_hrr && (random[24..] == DOWNGRADE_TLS12 || random[24..] == DOWNGRADE_TLS11) {
        return Err(Error::illegal("TLS 1.3 の ServerHello に格下げ番兵"));
    }

    let mut sh = ServerHello {
        session_id,
        cipher_suite,
        is_hrr,
        key_share: None,
        psk_identity: None,
        cookie: None,
    };
    for (t, body) in exts {
        let mut br = Reader::new(body);
        match t {
            ext::SUPPORTED_VERSIONS => {}
            ext::KEY_SHARE => {
                let g = br.u16()?;
                if is_hrr {
                    sh.key_share = Some((g, Vec::new()));
                } else {
                    let k = br.vec16()?;
                    if k.is_empty() {
                        return Err(Error::decode("key_exchange が空"));
                    }
                    sh.key_share = Some((g, k.to_vec()));
                }
                br.finish()?;
            }
            ext::PRE_SHARED_KEY if !is_hrr => {
                sh.psk_identity = Some(br.u16()?);
                br.finish()?;
            }
            ext::COOKIE if is_hrr => {
                let c = br.vec16()?;
                br.finish()?;
                if c.is_empty() {
                    return Err(Error::decode("cookie が空"));
                }
                sh.cookie = Some(c.to_vec());
            }
            _ => {
                return Err(Error::fatal(
                    AlertDescription::UNSUPPORTED_EXTENSION,
                    "ServerHello に要求していない拡張",
                ));
            }
        }
    }
    Ok(sh)
}

/// 証明書の項目（RFC 8446 4.4.2）。
pub(crate) struct CertificateMsg {
    pub context: Vec<u8>,
    pub certs: Vec<Vec<u8>>,
}

pub(crate) fn parse_certificate(msg: &[u8]) -> Result<CertificateMsg, Error> {
    let mut r = Reader::new(msg.get(4..).ok_or(Error::decode("短い"))?);
    let context = r.vec8()?.to_vec();
    let list = r.vec24()?;
    r.finish()?;
    let mut lr = Reader::new(list);
    let mut certs = Vec::new();
    while !lr.is_empty() {
        let c = lr.vec24()?;
        let e = lr.vec16()?;
        if c.is_empty() {
            return Err(Error::decode("cert_data が空"));
        }
        // 要求していない拡張（status_request・SCT など）は送られてこないはず。
        if !parse_extensions(e)?.is_empty() {
            return Err(Error::fatal(
                AlertDescription::UNSUPPORTED_EXTENSION,
                "CertificateEntry に要求していない拡張",
            ));
        }
        if certs.len() >= MAX_CHAIN_LEN {
            return Err(Error::fatal(AlertDescription::BAD_CERTIFICATE, "証明書チェーンが長すぎる"));
        }
        certs.push(c.to_vec());
    }
    Ok(CertificateMsg { context, certs })
}

pub(crate) fn encode_certificate(context: &[u8], certs: &[&[u8]]) -> Result<Vec<u8>, Error> {
    let mut b = Vec::new();
    put_vec8(&mut b, context)?;
    len_prefixed(&mut b, 3, |o| {
        for c in certs {
            crate::codec::put_vec24(o, c)?;
            put_u16(o, 0);
        }
        Ok(())
    })?;
    handshake(hs::CERTIFICATE, &b)
}

pub(crate) fn parse_certificate_verify(msg: &[u8]) -> Result<(u16, Vec<u8>), Error> {
    let mut r = Reader::new(msg.get(4..).ok_or(Error::decode("短い"))?);
    let scheme = r.u16()?;
    let sig = r.vec16()?.to_vec();
    r.finish()?;
    if sig.is_empty() {
        return Err(Error::decode("署名が空"));
    }
    Ok((scheme, sig))
}

pub(crate) fn encode_certificate_verify(scheme: u16, sig: &[u8]) -> Result<Vec<u8>, Error> {
    let mut b = Vec::new();
    put_u16(&mut b, scheme);
    put_vec16(&mut b, sig)?;
    handshake(hs::CERTIFICATE_VERIFY, &b)
}

/// CertificateVerify の署名対象（RFC 8446 4.4.3）。
pub(crate) fn verify_message(server: bool, transcript_hash: &[u8]) -> Vec<u8> {
    let mut m = vec![0x20u8; 64];
    m.extend_from_slice(if server {
        b"TLS 1.3, server CertificateVerify"
    } else {
        b"TLS 1.3, client CertificateVerify"
    });
    m.push(0);
    m.extend_from_slice(transcript_hash);
    m
}

pub(crate) struct CertificateRequest {
    pub context: Vec<u8>,
    pub signature_algorithms: Vec<u16>,
}

pub(crate) fn parse_certificate_request(msg: &[u8]) -> Result<CertificateRequest, Error> {
    let mut r = Reader::new(msg.get(4..).ok_or(Error::decode("短い"))?);
    let context = r.vec8()?.to_vec();
    let exts = parse_extensions(r.vec16()?)?;
    r.finish()?;
    let mut sa = None;
    for (t, b) in exts {
        if t == ext::SIGNATURE_ALGORITHMS {
            sa = Some(u16_list(b, 2)?);
        }
        // 他の拡張（certificate_authorities など）は無視してよい（RFC 8446 4.3.2）。
    }
    Ok(CertificateRequest {
        context,
        signature_algorithms: sa.ok_or(Error::fatal(
            AlertDescription::MISSING_EXTENSION,
            "CertificateRequest に signature_algorithms がない",
        ))?,
    })
}

pub(crate) fn encode_certificate_request(sig_algs: &[u16]) -> Result<Vec<u8>, Error> {
    let mut b = Vec::new();
    put_vec8(&mut b, &[])?;
    len_prefixed(&mut b, 2, |o| put_ext(o, ext::SIGNATURE_ALGORITHMS, |o| put_u16_list(o, sig_algs)))?;
    handshake(hs::CERTIFICATE_REQUEST, &b)
}

pub(crate) fn put_u16_list(out: &mut Vec<u8>, v: &[u16]) -> Result<(), Error> {
    len_prefixed(out, 2, |o| {
        for x in v {
            put_u16(o, *x);
        }
        Ok(())
    })
}

pub(crate) struct NewSessionTicket {
    pub lifetime: u32,
    pub age_add: u32,
    pub nonce: Vec<u8>,
    pub ticket: Vec<u8>,
}

pub(crate) fn parse_new_session_ticket(msg: &[u8]) -> Result<NewSessionTicket, Error> {
    let mut r = Reader::new(msg.get(4..).ok_or(Error::decode("短い"))?);
    let lifetime = r.u32()?;
    let age_add = r.u32()?;
    let nonce = r.vec8()?.to_vec();
    let ticket = r.vec16()?.to_vec();
    let _exts = parse_extensions(r.vec16()?)?; // early_data などは使わないので無視する。
    r.finish()?;
    if ticket.is_empty() {
        return Err(Error::decode("ticket が空"));
    }
    // 7 日を超える寿命は不正（RFC 8446 4.6.1）。
    if lifetime > 604_800 {
        return Err(Error::illegal("ticket_lifetime が 7 日を超える"));
    }
    Ok(NewSessionTicket {
        lifetime,
        age_add,
        nonce,
        ticket,
    })
}

pub(crate) fn encode_new_session_ticket(t: &NewSessionTicket) -> Result<Vec<u8>, Error> {
    let mut b = Vec::new();
    crate::codec::put_u32(&mut b, t.lifetime);
    crate::codec::put_u32(&mut b, t.age_add);
    put_vec8(&mut b, &t.nonce)?;
    put_vec16(&mut b, &t.ticket)?;
    put_u16(&mut b, 0);
    handshake(hs::NEW_SESSION_TICKET, &b)
}

pub(crate) fn parse_key_update(msg: &[u8]) -> Result<bool, Error> {
    let mut r = Reader::new(msg.get(4..).ok_or(Error::decode("短い"))?);
    let v = r.u8()?;
    r.finish()?;
    match v {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Error::illegal("KeyUpdate の値が不正")),
    }
}

pub(crate) fn encode_key_update(request: bool) -> Result<Vec<u8>, Error> {
    handshake(hs::KEY_UPDATE, &[u8::from(request)])
}

/// HRR 後の transcript の先頭に置く message_hash（RFC 8446 4.4.1）。
pub(crate) fn message_hash(ch1_hash: &[u8]) -> Vec<u8> {
    let mut v = vec![hs::MESSAGE_HASH, 0, 0, ch1_hash.len() as u8];
    v.extend_from_slice(ch1_hash);
    v
}

/// 一つのメッセージの上限。Certificate だけ大きく取る。
pub(crate) fn max_message_len(typ: u8) -> usize {
    match typ {
        hs::CERTIFICATE => 128 * 1024,
        _ => 64 * 1024,
    }
}
