//! ハンドシェイク（HSv5）の CIF と拡張ブロック（HSREQ/HSRSP、KMREQ/KMRSP、SID など）。

use crate::packet::{ParseError, be32};

/// ハンドシェイクの CIF の固定部の長さ。
pub const HS_CIF_LEN: usize = 48;

pub const HS_VERSION_UDT4: u32 = 4;
pub const HS_VERSION_SRT1: u32 = 5;
/// HSv5 の誘導応答で拡張フィールドに入れる値（"HAIVISIOn"）。
pub const SRT_MAGIC: u16 = 0x4A17;
/// UDT のソケット種別（HSv4 の誘導要求で使う）。
pub const UDT_DGRAM: u16 = 2;

pub const REQ_WAVEAHAND: u32 = 0;
pub const REQ_INDUCTION: u32 = 1;
pub const REQ_CONCLUSION: u32 = 0xFFFF_FFFF;
pub const REQ_AGREEMENT: u32 = 0xFFFF_FFFE;
/// 拒否は 1000 + 理由コード。
pub const REQ_FAILURE_BASE: u32 = 1000;

pub const HS_EXT_HSREQ: u16 = 1;
pub const HS_EXT_KMREQ: u16 = 2;
pub const HS_EXT_CONFIG: u16 = 4;

pub const CMD_HSREQ: u16 = 1;
pub const CMD_HSRSP: u16 = 2;
pub const CMD_KMREQ: u16 = 3;
pub const CMD_KMRSP: u16 = 4;
pub const CMD_SID: u16 = 5;
pub const CMD_CONGESTION: u16 = 6;
pub const CMD_FILTER: u16 = 7;
pub const CMD_GROUP: u16 = 8;

pub const OPT_TSBPDSND: u32 = 0x01;
pub const OPT_TSBPDRCV: u32 = 0x02;
pub const OPT_HAICRYPT: u32 = 0x04;
pub const OPT_TLPKTDROP: u32 = 0x08;
pub const OPT_NAKREPORT: u32 = 0x10;
pub const OPT_REXMITFLG: u32 = 0x20;
pub const OPT_STREAM: u32 = 0x40;
pub const OPT_FILTERCAP: u32 = 0x80;

/// Stream ID の上限（libsrt と同じ 512 バイト）。
pub const MAX_SID_LEN: usize = 512;
/// 拡張ブロックの数の上限。
const MAX_EXTENSIONS: usize = 16;

/// HSREQ/HSRSP の中身。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SrtHs {
    pub version: u32,
    pub flags: u32,
    /// 上位 16 ビット：受信側の TSBPD 遅延（ms）。
    pub rcv_latency_ms: u16,
    /// 下位 16 ビット：送信側の TSBPD 遅延（ms）。
    pub snd_latency_ms: u16,
}

/// 拡張ブロック。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HsExt {
    HsReq(SrtHs),
    HsRsp(SrtHs),
    KmReq(Vec<u8>),
    KmRsp(Vec<u8>),
    Sid(String),
    Congestion(String),
    Filter(String),
    Group(Vec<u8>),
    Other(u16),
}

/// ハンドシェイクの CIF。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handshake {
    pub version: u32,
    pub enc_field: u16,
    pub ext_field: u16,
    pub isn: u32,
    pub mtu: u32,
    pub flow_window: u32,
    pub req_type: u32,
    pub socket_id: u32,
    pub cookie: u32,
    pub peer_ip: [u8; 16],
    pub ext: Vec<HsExt>,
}

/// PBKEYLEN（16/24/32）をハンドシェイクの暗号フィールド（2/3/4）へ。
pub fn keylen_to_field(len: usize) -> u16 {
    match len {
        16 => 2,
        24 => 3,
        32 => 4,
        _ => 0,
    }
}

/// libsrt は 32 ビット語ごとにホスト順（リトルエンディアン）へ並べ替えてから全体を
/// ネットワーク順にするため、文字列は 4 バイトごとに逆順で載る。
fn sid_encode(s: &str) -> Vec<u8> {
    let mut b = s.as_bytes().to_vec();
    while !b.len().is_multiple_of(4) {
        b.push(0);
    }
    for c in b.chunks_mut(4) {
        c.reverse();
    }
    b
}

fn sid_decode(b: &[u8]) -> Result<String, ParseError> {
    if b.len() > MAX_SID_LEN {
        return Err(ParseError::Invalid("stream id too long"));
    }
    let mut v = b.to_vec();
    for c in v.chunks_mut(4) {
        c.reverse();
    }
    while v.last() == Some(&0) {
        v.pop();
    }
    String::from_utf8(v).map_err(|_| ParseError::Invalid("stream id not utf-8"))
}

impl Handshake {
    pub fn parse(cif: &[u8]) -> Result<Self, ParseError> {
        if cif.len() < HS_CIF_LEN {
            return Err(ParseError::Truncated);
        }
        let type_field = be32(cif, 4)?;
        let mut peer_ip = [0u8; 16];
        peer_ip.copy_from_slice(&cif[32..48]);
        let mut hs = Handshake {
            version: be32(cif, 0)?,
            enc_field: (type_field >> 16) as u16,
            ext_field: (type_field & 0xFFFF) as u16,
            isn: be32(cif, 8)?,
            mtu: be32(cif, 12)?,
            flow_window: be32(cif, 16)?,
            req_type: be32(cif, 20)?,
            socket_id: be32(cif, 24)?,
            cookie: be32(cif, 28)?,
            peer_ip,
            ext: Vec::new(),
        };
        // 拡張ブロックは HSv5 の結論（と拒否）だけで読む。
        if hs.version == HS_VERSION_SRT1 && hs.req_type == REQ_CONCLUSION {
            let mut off = HS_CIF_LEN;
            while off + 4 <= cif.len() {
                if hs.ext.len() >= MAX_EXTENSIONS {
                    return Err(ParseError::Invalid("too many extensions"));
                }
                let spec = be32(cif, off)?;
                let cmd = (spec >> 16) as u16;
                let len = (spec & 0xFFFF) as usize * 4;
                off += 4;
                let body = cif.get(off..off + len).ok_or(ParseError::Truncated)?;
                off += len;
                let e = match cmd {
                    CMD_HSREQ | CMD_HSRSP => {
                        if body.len() < 12 {
                            return Err(ParseError::Invalid("HSREQ length"));
                        }
                        let lat = be32(body, 8)?;
                        let s = SrtHs {
                            version: be32(body, 0)?,
                            flags: be32(body, 4)?,
                            rcv_latency_ms: (lat >> 16) as u16,
                            snd_latency_ms: (lat & 0xFFFF) as u16,
                        };
                        if cmd == CMD_HSREQ { HsExt::HsReq(s) } else { HsExt::HsRsp(s) }
                    }
                    CMD_KMREQ => HsExt::KmReq(body.to_vec()),
                    CMD_KMRSP => HsExt::KmRsp(body.to_vec()),
                    CMD_SID => HsExt::Sid(sid_decode(body)?),
                    CMD_CONGESTION => HsExt::Congestion(sid_decode(body)?),
                    CMD_FILTER => HsExt::Filter(sid_decode(body)?),
                    CMD_GROUP => HsExt::Group(body.to_vec()),
                    other => HsExt::Other(other),
                };
                hs.ext.push(e);
            }
            if off != cif.len() {
                return Err(ParseError::Invalid("trailing bytes in handshake"));
            }
        }
        Ok(hs)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(128);
        v.extend_from_slice(&self.version.to_be_bytes());
        v.extend_from_slice(&(((self.enc_field as u32) << 16) | self.ext_field as u32).to_be_bytes());
        v.extend_from_slice(&self.isn.to_be_bytes());
        v.extend_from_slice(&self.mtu.to_be_bytes());
        v.extend_from_slice(&self.flow_window.to_be_bytes());
        v.extend_from_slice(&self.req_type.to_be_bytes());
        v.extend_from_slice(&self.socket_id.to_be_bytes());
        v.extend_from_slice(&self.cookie.to_be_bytes());
        v.extend_from_slice(&self.peer_ip);
        for e in &self.ext {
            let (cmd, body): (u16, Vec<u8>) = match e {
                HsExt::HsReq(s) | HsExt::HsRsp(s) => {
                    let mut b = Vec::with_capacity(12);
                    b.extend_from_slice(&s.version.to_be_bytes());
                    b.extend_from_slice(&s.flags.to_be_bytes());
                    b.extend_from_slice(
                        &(((s.rcv_latency_ms as u32) << 16) | s.snd_latency_ms as u32).to_be_bytes(),
                    );
                    (if matches!(e, HsExt::HsReq(_)) { CMD_HSREQ } else { CMD_HSRSP }, b)
                }
                HsExt::KmReq(b) => (CMD_KMREQ, b.clone()),
                HsExt::KmRsp(b) => (CMD_KMRSP, b.clone()),
                HsExt::Sid(s) => (CMD_SID, sid_encode(s)),
                HsExt::Congestion(s) => (CMD_CONGESTION, sid_encode(s)),
                HsExt::Filter(s) => (CMD_FILTER, sid_encode(s)),
                HsExt::Group(b) => (CMD_GROUP, b.clone()),
                HsExt::Other(_) => continue,
            };
            let mut body = body;
            while !body.len().is_multiple_of(4) {
                body.push(0);
            }
            v.extend_from_slice(&(((cmd as u32) << 16) | (body.len() / 4) as u32).to_be_bytes());
            v.extend_from_slice(&body);
        }
        v
    }
}

/// 相手の IP をハンドシェイクの peer IP フィールドへ（libsrt と同じ語順）。
pub fn encode_peer_ip(addr: &std::net::SocketAddr) -> [u8; 16] {
    let mut out = [0u8; 16];
    match addr.ip() {
        std::net::IpAddr::V4(v4) => {
            let mut o = v4.octets();
            o.reverse();
            out[0..4].copy_from_slice(&o);
        }
        std::net::IpAddr::V6(v6) => {
            let o = v6.octets();
            for (i, c) in o.chunks(4).enumerate() {
                for j in 0..4 {
                    out[i * 4 + j] = c[3 - j];
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sid_word_order() {
        // "#!::r=live" → 語ごとに逆順
        let enc = sid_encode("#!::r=abc");
        assert_eq!(&enc[0..4], b"::!#");
        assert_eq!(sid_decode(&enc).unwrap(), "#!::r=abc");
    }

    #[test]
    fn handshake_roundtrip() {
        let hs = Handshake {
            version: 5,
            enc_field: 2,
            ext_field: HS_EXT_HSREQ | HS_EXT_KMREQ | HS_EXT_CONFIG,
            isn: 1234,
            mtu: 1500,
            flow_window: 8192,
            req_type: REQ_CONCLUSION,
            socket_id: 99,
            cookie: 0xABCD,
            peer_ip: [0; 16],
            ext: vec![
                HsExt::HsReq(SrtHs { version: 0x10504, flags: 0x3F, rcv_latency_ms: 120, snd_latency_ms: 200 }),
                HsExt::KmReq(vec![1; 48]),
                HsExt::Sid("#!::r=live/a,m=publish".into()),
            ],
        };
        let b = hs.to_bytes();
        assert_eq!(Handshake::parse(&b).unwrap(), hs);
        for n in 0..b.len() {
            let _ = Handshake::parse(&b[..n]);
        }
    }
}
