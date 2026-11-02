//! handshake message の符号化・解析と、分割された message の再構成（RFC 6347 4.2.2・4.2.3）。

use std::collections::BTreeMap;

use crate::codec::{
    DResult, DecodeError, Reader, put_u16, put_u24, put_vec8, put_vec16, put_vec24, u16_list, u16s,
};

pub(crate) const HT_HELLO_REQUEST: u8 = 0;
pub(crate) const HT_CLIENT_HELLO: u8 = 1;
pub(crate) const HT_SERVER_HELLO: u8 = 2;
pub(crate) const HT_HELLO_VERIFY_REQUEST: u8 = 3;
pub(crate) const HT_CERTIFICATE: u8 = 11;
pub(crate) const HT_SERVER_KEY_EXCHANGE: u8 = 12;
pub(crate) const HT_CERTIFICATE_REQUEST: u8 = 13;
pub(crate) const HT_SERVER_HELLO_DONE: u8 = 14;
pub(crate) const HT_CERTIFICATE_VERIFY: u8 = 15;
pub(crate) const HT_CLIENT_KEY_EXCHANGE: u8 = 16;
pub(crate) const HT_FINISHED: u8 = 20;

pub(crate) const HANDSHAKE_HEADER_LEN: usize = 12;
/// 1 つの handshake message の上限（証明書の連鎖を含む）。
pub(crate) const MAX_MESSAGE_LEN: usize = 32 * 1024;
/// 先読みして保持する message の数。
const MAX_AHEAD: u16 = 16;
/// 再構成中の総量の上限。
const MAX_BUFFERED: usize = 128 * 1024;
/// 1 message の断片の区間数の上限。
const MAX_RANGES: usize = 64;

pub(crate) const EXT_SUPPORTED_GROUPS: u16 = 10;
pub(crate) const EXT_EC_POINT_FORMATS: u16 = 11;
pub(crate) const EXT_SIGNATURE_ALGORITHMS: u16 = 13;
pub(crate) const EXT_USE_SRTP: u16 = 14;
pub(crate) const EXT_EXTENDED_MASTER_SECRET: u16 = 23;
pub(crate) const EXT_RENEGOTIATION_INFO: u16 = 0xff01;

/// TLS_EMPTY_RENEGOTIATION_INFO_SCSV。
pub(crate) const SCSV_RENEGOTIATION: u16 = 0x00ff;
/// ecdsa_secp256r1_sha256。
pub(crate) const SIG_ECDSA_P256_SHA256: u16 = 0x0403;
/// ClientCertificateType ecdsa_sign。
pub(crate) const CERT_TYPE_ECDSA_SIGN: u8 = 64;
/// ECCurveType named_curve。
pub(crate) const CURVE_TYPE_NAMED: u8 = 3;

/// handshake の断片。
pub(crate) struct Fragment<'a> {
    pub msg_type: u8,
    pub length: u32,
    pub seq: u16,
    pub offset: u32,
    pub data: &'a [u8],
}

/// record の本体を断片に分ける。
pub(crate) fn parse_fragments(body: &[u8]) -> DResult<Vec<Fragment<'_>>> {
    let mut r = Reader::new(body);
    let mut out = Vec::new();
    while !r.is_empty() {
        let msg_type = r.u8()?;
        let length = r.u24()?;
        let seq = r.u16()?;
        let offset = r.u24()?;
        let flen = r.u24()?;
        let data = r.bytes(flen as usize)?;
        if length as usize > MAX_MESSAGE_LEN || offset.checked_add(flen).is_none_or(|e| e > length)
        {
            return Err(DecodeError);
        }
        out.push(Fragment {
            msg_type,
            length,
            seq,
            offset,
            data,
        });
    }
    Ok(out)
}

/// 断片の header を付けた byte 列。
pub(crate) fn fragment_bytes(
    msg_type: u8,
    seq: u16,
    total: usize,
    offset: usize,
    data: &[u8],
) -> Vec<u8> {
    let mut v = Vec::with_capacity(HANDSHAKE_HEADER_LEN + data.len());
    v.push(msg_type);
    put_u24(&mut v, total as u32);
    put_u16(&mut v, seq);
    put_u24(&mut v, offset as u32);
    put_u24(&mut v, data.len() as u32);
    v.extend_from_slice(data);
    v
}

/// transcript に入れる形（分割しない header 付き）。
pub(crate) fn message_bytes(msg_type: u8, seq: u16, body: &[u8]) -> Vec<u8> {
    fragment_bytes(msg_type, seq, body.len(), 0, body)
}

/// 再構成が済んだ message。
pub(crate) struct Message {
    pub msg_type: u8,
    pub seq: u16,
    pub epoch: u16,
    pub body: Vec<u8>,
}

struct Partial {
    msg_type: u8,
    epoch: u16,
    body: Vec<u8>,
    /// 受け取った区間 [start, end)（整列・併合済み）。
    ranges: Vec<(u32, u32)>,
}

impl Partial {
    fn complete(&self) -> bool {
        let len = self.body.len() as u32;
        len == 0 && self.ranges.is_empty() || self.ranges.len() == 1 && self.ranges[0] == (0, len)
    }

    fn add(&mut self, start: u32, end: u32) -> bool {
        if start == end {
            return true;
        }
        self.ranges.push((start, end));
        self.ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(self.ranges.len());
        for &(s, e) in &self.ranges {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        self.ranges = merged;
        self.ranges.len() <= MAX_RANGES
    }
}

/// 断片を入れた結果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Feed {
    /// 受け取った（または既に受け取り済み）。
    Accepted,
    /// 既に処理した message（相手の再送）。
    Old(u16),
    /// 範囲外・矛盾で捨てた。
    Dropped,
}

/// message_seq の順に message を組み立てる。
pub(crate) struct Reassembler {
    next_seq: u16,
    pending: BTreeMap<u16, Partial>,
    buffered: usize,
}

impl Reassembler {
    pub(crate) fn new() -> Self {
        Self {
            next_seq: 0,
            pending: BTreeMap::new(),
            buffered: 0,
        }
    }

    pub(crate) fn next_seq(&self) -> u16 {
        self.next_seq
    }

    /// 次に待つ message_seq を設定する（cookie 交換後の ClientHello など）。
    pub(crate) fn set_next_seq(&mut self, seq: u16) {
        self.next_seq = seq;
        self.pending.clear();
        self.buffered = 0;
    }

    pub(crate) fn feed(&mut self, f: &Fragment<'_>, epoch: u16) -> Feed {
        if f.seq < self.next_seq {
            return Feed::Old(f.seq);
        }
        if f.seq - self.next_seq >= MAX_AHEAD {
            return Feed::Dropped;
        }
        let len = f.length as usize;
        if !self.pending.contains_key(&f.seq) {
            if self.buffered + len > MAX_BUFFERED {
                return Feed::Dropped;
            }
            self.buffered += len;
            self.pending.insert(
                f.seq,
                Partial {
                    msg_type: f.msg_type,
                    epoch,
                    body: vec![0; len],
                    ranges: Vec::new(),
                },
            );
        }
        let p = self.pending.get_mut(&f.seq).expect("直前に入れた");
        if p.msg_type != f.msg_type || p.body.len() != len || p.epoch != epoch {
            return Feed::Dropped;
        }
        let start = f.offset as usize;
        p.body[start..start + f.data.len()].copy_from_slice(f.data);
        if !p.add(f.offset, f.offset + f.data.len() as u32) {
            return Feed::Dropped;
        }
        Feed::Accepted
    }

    /// 次の message が揃っていれば取り出す。
    pub(crate) fn pop(&mut self) -> Option<Message> {
        let p = self.pending.get(&self.next_seq)?;
        if !p.complete() {
            return None;
        }
        let p = self.pending.remove(&self.next_seq).expect("直前に見た");
        self.buffered -= p.body.len();
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        Some(Message {
            msg_type: p.msg_type,
            seq,
            epoch: p.epoch,
            body: p.body,
        })
    }
}

/// 拡張（関心のあるものだけ解析する）。
#[derive(Default, Debug)]
pub(crate) struct Extensions {
    pub groups: Option<Vec<u16>>,
    pub point_formats: Option<Vec<u8>>,
    pub sig_algs: Option<Vec<u16>>,
    pub use_srtp: Option<(Vec<u16>, Vec<u8>)>,
    pub ems: bool,
    pub reneg: Option<Vec<u8>>,
    /// 上以外の拡張の型。
    pub others: Vec<u16>,
}

impl Extensions {
    pub(crate) fn parse(data: &[u8]) -> DResult<Self> {
        let mut e = Extensions::default();
        let mut seen: Vec<u16> = Vec::new();
        let mut r = Reader::new(data);
        while !r.is_empty() {
            let ty = r.u16()?;
            let body = r.vec16()?;
            if seen.contains(&ty) || seen.len() >= 64 {
                return Err(DecodeError);
            }
            seen.push(ty);
            let mut b = Reader::new(body);
            match ty {
                EXT_SUPPORTED_GROUPS => {
                    e.groups = Some(u16_list(b.vec16()?)?);
                    b.finish()?;
                }
                EXT_EC_POINT_FORMATS => {
                    e.point_formats = Some(b.vec8()?.to_vec());
                    b.finish()?;
                }
                EXT_SIGNATURE_ALGORITHMS => {
                    e.sig_algs = Some(u16_list(b.vec16()?)?);
                    b.finish()?;
                }
                EXT_USE_SRTP => {
                    let profiles = u16_list(b.vec16()?)?;
                    let mki = b.vec8()?.to_vec();
                    b.finish()?;
                    e.use_srtp = Some((profiles, mki));
                }
                EXT_EXTENDED_MASTER_SECRET => {
                    b.finish()?;
                    e.ems = true;
                }
                EXT_RENEGOTIATION_INFO => {
                    e.reneg = Some(b.vec8()?.to_vec());
                    b.finish()?;
                }
                other => e.others.push(other),
            }
        }
        Ok(e)
    }
}

fn put_ext(out: &mut Vec<u8>, ty: u16, data: &[u8]) {
    put_u16(out, ty);
    put_vec16(out, data);
}

/// ClientHello。
pub(crate) struct ClientHello {
    pub version: u16,
    pub random: [u8; 32],
    pub cookie: Vec<u8>,
    pub cipher_suites: Vec<u16>,
    pub compression: Vec<u8>,
    pub ext: Extensions,
    /// cookie の計算に使う、cookie 以外の部分の byte 列。
    pub cookie_input: Vec<u8>,
}

impl ClientHello {
    pub(crate) fn parse(body: &[u8]) -> DResult<Self> {
        let mut r = Reader::new(body);
        let version = r.u16()?;
        let random: [u8; 32] = r.bytes(32)?.try_into().map_err(|_| DecodeError)?;
        // session_id は再開に使わない（常に完全な handshake）。長さだけ検査する。
        if r.vec8()?.len() > 32 {
            return Err(DecodeError);
        }
        let after_sid = body.len() - r.remaining();
        let cookie = r.vec8()?.to_vec();
        let after_cookie = body.len() - r.remaining();
        let suites = r.vec16()?;
        if suites.is_empty() {
            return Err(DecodeError);
        }
        let cipher_suites = u16_list(suites)?;
        let compression = r.vec8()?.to_vec();
        if compression.is_empty() {
            return Err(DecodeError);
        }
        let ext = if r.is_empty() {
            Extensions::default()
        } else {
            Extensions::parse(r.vec16()?)?
        };
        r.finish()?;
        let mut cookie_input = body[..after_sid].to_vec();
        cookie_input.extend_from_slice(&body[after_cookie..]);
        Ok(Self {
            version,
            random,
            cookie,
            cipher_suites,
            compression,
            ext,
            cookie_input,
        })
    }
}

/// client が送る ClientHello の内容。
pub(crate) struct ClientHelloParams<'a> {
    pub random: &'a [u8; 32],
    pub cookie: &'a [u8],
    pub suites: &'a [u16],
    pub groups: &'a [u16],
    pub srtp: &'a [u16],
}

pub(crate) fn encode_client_hello(p: &ClientHelloParams<'_>) -> Vec<u8> {
    let mut v = Vec::with_capacity(256);
    put_u16(&mut v, crate::record::DTLS12);
    v.extend_from_slice(p.random);
    put_vec8(&mut v, &[]);
    put_vec8(&mut v, p.cookie);
    put_vec16(&mut v, &u16s(p.suites));
    put_vec8(&mut v, &[0]);
    let mut ext = Vec::new();
    put_ext(&mut ext, EXT_EXTENDED_MASTER_SECRET, &[]);
    put_ext(&mut ext, EXT_RENEGOTIATION_INFO, &[0]);
    let mut groups = Vec::new();
    put_vec16(&mut groups, &u16s(p.groups));
    put_ext(&mut ext, EXT_SUPPORTED_GROUPS, &groups);
    put_ext(&mut ext, EXT_EC_POINT_FORMATS, &[1, 0]);
    let mut sig = Vec::new();
    put_vec16(&mut sig, &u16s(&[SIG_ECDSA_P256_SHA256]));
    put_ext(&mut ext, EXT_SIGNATURE_ALGORITHMS, &sig);
    if !p.srtp.is_empty() {
        let mut s = Vec::new();
        put_vec16(&mut s, &u16s(p.srtp));
        put_vec8(&mut s, &[]);
        put_ext(&mut ext, EXT_USE_SRTP, &s);
    }
    put_vec16(&mut v, &ext);
    v
}

/// HelloVerifyRequest。
pub(crate) fn encode_hello_verify_request(cookie: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(3 + cookie.len());
    put_u16(&mut v, crate::record::DTLS12);
    put_vec8(&mut v, cookie);
    v
}

pub(crate) fn parse_hello_verify_request(body: &[u8]) -> DResult<(u16, Vec<u8>)> {
    let mut r = Reader::new(body);
    let version = r.u16()?;
    let cookie = r.vec8()?.to_vec();
    r.finish()?;
    Ok((version, cookie))
}

/// ServerHello。
pub(crate) struct ServerHello {
    pub version: u16,
    pub random: [u8; 32],
    pub cipher_suite: u16,
    pub compression: u8,
    pub ext: Extensions,
}

impl ServerHello {
    pub(crate) fn parse(body: &[u8]) -> DResult<Self> {
        let mut r = Reader::new(body);
        let version = r.u16()?;
        let random: [u8; 32] = r.bytes(32)?.try_into().map_err(|_| DecodeError)?;
        let sid = r.vec8()?;
        if sid.len() > 32 {
            return Err(DecodeError);
        }
        let cipher_suite = r.u16()?;
        let compression = r.u8()?;
        let ext = if r.is_empty() {
            Extensions::default()
        } else {
            Extensions::parse(r.vec16()?)?
        };
        r.finish()?;
        Ok(Self {
            version,
            random,
            cipher_suite,
            compression,
            ext,
        })
    }
}

pub(crate) struct ServerHelloParams<'a> {
    pub random: &'a [u8; 32],
    pub suite: u16,
    pub srtp: Option<u16>,
    pub reneg: bool,
    pub point_formats: bool,
}

pub(crate) fn encode_server_hello(p: &ServerHelloParams<'_>) -> Vec<u8> {
    let mut v = Vec::with_capacity(96);
    put_u16(&mut v, crate::record::DTLS12);
    v.extend_from_slice(p.random);
    put_vec8(&mut v, &[]);
    put_u16(&mut v, p.suite);
    v.push(0);
    let mut ext = Vec::new();
    put_ext(&mut ext, EXT_EXTENDED_MASTER_SECRET, &[]);
    if p.reneg {
        put_ext(&mut ext, EXT_RENEGOTIATION_INFO, &[0]);
    }
    if p.point_formats {
        put_ext(&mut ext, EXT_EC_POINT_FORMATS, &[1, 0]);
    }
    if let Some(profile) = p.srtp {
        let mut s = Vec::new();
        put_vec16(&mut s, &profile.to_be_bytes());
        put_vec8(&mut s, &[]);
        put_ext(&mut ext, EXT_USE_SRTP, &s);
    }
    put_vec16(&mut v, &ext);
    v
}

/// Certificate（証明書の列）。
pub(crate) fn encode_certificate(chain: &[&[u8]]) -> Vec<u8> {
    let mut list = Vec::new();
    for c in chain {
        put_vec24(&mut list, c);
    }
    let mut v = Vec::with_capacity(list.len() + 3);
    put_vec24(&mut v, &list);
    v
}

pub(crate) fn parse_certificate(body: &[u8]) -> DResult<Vec<Vec<u8>>> {
    let mut r = Reader::new(body);
    let list = r.vec24()?;
    r.finish()?;
    let mut l = Reader::new(list);
    let mut out = Vec::new();
    while !l.is_empty() {
        let c = l.vec24()?;
        if c.is_empty() || out.len() >= 8 {
            return Err(DecodeError);
        }
        out.push(c.to_vec());
    }
    Ok(out)
}

/// ServerKeyExchange の ECDHE パラメーター部分（署名の対象）。
pub(crate) fn encode_ecdh_params(group: u16, public: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + public.len());
    v.push(CURVE_TYPE_NAMED);
    put_u16(&mut v, group);
    put_vec8(&mut v, public);
    v
}

pub(crate) fn encode_server_key_exchange(params: &[u8], signature: &[u8]) -> Vec<u8> {
    let mut v = params.to_vec();
    put_u16(&mut v, SIG_ECDSA_P256_SHA256);
    put_vec16(&mut v, signature);
    v
}

pub(crate) struct ServerKeyExchange {
    pub curve_type: u8,
    pub group: u16,
    pub public: Vec<u8>,
    /// 署名の対象（ServerECDHParams）。
    pub params: Vec<u8>,
    pub sig_alg: u16,
    pub signature: Vec<u8>,
}

impl ServerKeyExchange {
    pub(crate) fn parse(body: &[u8]) -> DResult<Self> {
        let mut r = Reader::new(body);
        let curve_type = r.u8()?;
        let group = r.u16()?;
        let public = r.vec8()?.to_vec();
        let params = body[..body.len() - r.remaining()].to_vec();
        let sig_alg = r.u16()?;
        let signature = r.vec16()?.to_vec();
        r.finish()?;
        Ok(Self {
            curve_type,
            group,
            public,
            params,
            sig_alg,
            signature,
        })
    }
}

pub(crate) fn encode_certificate_request() -> Vec<u8> {
    let mut v = Vec::new();
    put_vec8(&mut v, &[CERT_TYPE_ECDSA_SIGN]);
    put_vec16(&mut v, &u16s(&[SIG_ECDSA_P256_SHA256]));
    put_vec16(&mut v, &[]);
    v
}

pub(crate) struct CertificateRequest {
    pub cert_types: Vec<u8>,
    pub sig_algs: Vec<u16>,
}

impl CertificateRequest {
    pub(crate) fn parse(body: &[u8]) -> DResult<Self> {
        let mut r = Reader::new(body);
        let cert_types = r.vec8()?.to_vec();
        let sig_algs = u16_list(r.vec16()?)?;
        let _authorities = r.vec16()?;
        r.finish()?;
        Ok(Self {
            cert_types,
            sig_algs,
        })
    }
}

/// 署名付きの message（CertificateVerify）。
pub(crate) fn encode_signed(signature: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + signature.len());
    put_u16(&mut v, SIG_ECDSA_P256_SHA256);
    put_vec16(&mut v, signature);
    v
}

pub(crate) fn parse_signed(body: &[u8]) -> DResult<(u16, Vec<u8>)> {
    let mut r = Reader::new(body);
    let alg = r.u16()?;
    let sig = r.vec16()?.to_vec();
    r.finish()?;
    Ok((alg, sig))
}

/// ClientKeyExchange（ECDHE の公開値）。
pub(crate) fn encode_client_key_exchange(public: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + public.len());
    put_vec8(&mut v, public);
    v
}

pub(crate) fn parse_client_key_exchange(body: &[u8]) -> DResult<Vec<u8>> {
    let mut r = Reader::new(body);
    let p = r.vec8()?.to_vec();
    r.finish()?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frags(msg_type: u8, seq: u16, body: &[u8], size: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut off = 0;
        loop {
            let n = size.min(body.len() - off);
            out.push(fragment_bytes(
                msg_type,
                seq,
                body.len(),
                off,
                &body[off..off + n],
            ));
            off += n;
            if off >= body.len() {
                break;
            }
        }
        out
    }

    #[test]
    fn reassembly_out_of_order_and_duplicates() {
        let body: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        let mut f = frags(HT_CERTIFICATE, 1, &body, 97);
        f.reverse();
        let dup = f[3].clone();
        f.push(dup);
        let mut r = Reassembler::new();
        r.set_next_seq(1);
        for b in &f {
            for fr in parse_fragments(b).unwrap() {
                assert_eq!(r.feed(&fr, 0), Feed::Accepted);
            }
        }
        let m = r.pop().unwrap();
        assert_eq!(m.body, body);
        assert_eq!(m.seq, 1);
        assert!(r.pop().is_none());
        let fr = parse_fragments(&f[0]).unwrap();
        assert_eq!(r.feed(&fr[0], 0), Feed::Old(1));
    }

    #[test]
    fn reassembly_rejects_inconsistent() {
        let mut r = Reassembler::new();
        let a = fragment_bytes(HT_CERTIFICATE, 0, 100, 0, &[1; 10]);
        let b = fragment_bytes(HT_SERVER_HELLO, 0, 100, 10, &[1; 10]);
        let c = fragment_bytes(HT_CERTIFICATE, 0, 120, 10, &[1; 10]);
        assert_eq!(r.feed(&parse_fragments(&a).unwrap()[0], 0), Feed::Accepted);
        assert_eq!(r.feed(&parse_fragments(&b).unwrap()[0], 0), Feed::Dropped);
        assert_eq!(r.feed(&parse_fragments(&c).unwrap()[0], 0), Feed::Dropped);
        assert_eq!(r.feed(&parse_fragments(&a).unwrap()[0], 1), Feed::Dropped);
        // 範囲外の offset は解析で拒否
        let mut d = fragment_bytes(HT_CERTIFICATE, 0, 100, 95, &[1; 5]);
        d[8] = 10; // fragment_length を 10 にして本体を足す
        d.extend_from_slice(&[0; 5]);
        assert!(parse_fragments(&d).is_err());
        // 先読みの上限
        let far = fragment_bytes(HT_CERTIFICATE, 40, 10, 0, &[0; 10]);
        assert_eq!(r.feed(&parse_fragments(&far).unwrap()[0], 0), Feed::Dropped);
        let big = fragment_bytes(HT_CERTIFICATE, 0, MAX_MESSAGE_LEN + 1, 0, &[]);
        assert!(parse_fragments(&big).is_err());
    }

    #[test]
    fn empty_message() {
        let mut r = Reassembler::new();
        let b = fragment_bytes(HT_SERVER_HELLO_DONE, 0, 0, 0, &[]);
        assert_eq!(r.feed(&parse_fragments(&b).unwrap()[0], 0), Feed::Accepted);
        let m = r.pop().unwrap();
        assert_eq!(m.msg_type, HT_SERVER_HELLO_DONE);
        assert!(m.body.is_empty());
    }

    #[test]
    fn client_hello_roundtrip() {
        let random = [9u8; 32];
        let body = encode_client_hello(&ClientHelloParams {
            random: &random,
            cookie: b"cookie",
            suites: &[0xC02B, 0xCCA9],
            groups: &[29, 23],
            srtp: &[7, 1],
        });
        let ch = ClientHello::parse(&body).unwrap();
        assert_eq!(ch.cookie, b"cookie");
        assert_eq!(ch.cipher_suites, vec![0xC02B, 0xCCA9]);
        assert!(ch.ext.ems);
        assert_eq!(ch.ext.reneg.as_deref(), Some(&[][..]));
        assert_eq!(ch.ext.use_srtp.as_ref().unwrap().0, vec![7, 1]);
        assert_eq!(ch.ext.groups.as_deref(), Some(&[29u16, 23][..]));
        // cookie を変えても cookie_input は同じ
        let body2 = encode_client_hello(&ClientHelloParams {
            random: &random,
            cookie: b"",
            suites: &[0xC02B, 0xCCA9],
            groups: &[29, 23],
            srtp: &[7, 1],
        });
        assert_eq!(
            ClientHello::parse(&body2).unwrap().cookie_input,
            ch.cookie_input
        );
        // 途中で切れたものはパニックせず、拡張の途中で切れたものは拒否
        for n in 0..body.len() {
            let r = ClientHello::parse(&body[..n]);
            if n != 2 + 32 + 1 + 7 + 2 + 4 + 2 {
                assert!(r.is_err(), "{n}");
            }
        }
    }
}
