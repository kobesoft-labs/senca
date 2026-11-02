//! Capsule（RFC 9297 3 章）と WebTransport の capsule（draft-ietf-webtrans-http3-07 5 章）。

use bytes::{Buf, Bytes, BytesMut};
use senca_quic::varint;

pub(crate) const CLOSE_WEBTRANSPORT_SESSION: u64 = 0x2843;
pub(crate) const DRAIN_WEBTRANSPORT_SESSION: u64 = 0x78ae;
/// 一つの capsule の上限（CLOSE は 4 + 1024 バイト以下。他は読み捨てる）。
const MAX_CAPSULE: u64 = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capsule {
    /// CLOSE_WEBTRANSPORT_SESSION（コード、理由）。
    CloseWebTransportSession { code: u32, reason: String },
    DrainWebTransportSession,
    /// 未知の capsule（読み捨てる）。
    Unknown { ty: u64 },
}

impl Capsule {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        match self {
            Self::CloseWebTransportSession { code, reason } => {
                let r = &reason.as_bytes()[..reason.len().min(1024)];
                varint::write(&mut o, CLOSE_WEBTRANSPORT_SESSION);
                varint::write(&mut o, 4 + r.len() as u64);
                o.extend_from_slice(&code.to_be_bytes());
                o.extend_from_slice(r);
            }
            Self::DrainWebTransportSession => {
                varint::write(&mut o, DRAIN_WEBTRANSPORT_SESSION);
                varint::write(&mut o, 0);
            }
            Self::Unknown { ty } => {
                varint::write(&mut o, *ty);
                varint::write(&mut o, 0);
            }
        }
        o
    }
}

/// capsule の並びを読む（DATA フレームの payload を連結したもの）。
#[derive(Debug, Default)]
pub(crate) struct CapsuleReader {
    buf: BytesMut,
    /// 読み捨て中の未知 capsule の残り。
    skip: u64,
}

impl CapsuleReader {
    pub(crate) fn push(&mut self, data: &[u8]) -> Result<Vec<Capsule>, &'static str> {
        let mut data = data;
        if self.skip > 0 {
            let n = (self.skip as usize).min(data.len());
            self.skip -= n as u64;
            data = &data[n..];
        }
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        while let Some((ty, a)) = varint::read(&self.buf) {
            let Some((len, b)) = varint::read(&self.buf[a..]) else { break };
            let hdr = a + b;
            match ty {
                CLOSE_WEBTRANSPORT_SESSION | DRAIN_WEBTRANSPORT_SESSION => {
                    if len > MAX_CAPSULE || (ty == CLOSE_WEBTRANSPORT_SESSION && !(4..=4 + 1024).contains(&len)) {
                        return Err("WebTransport の capsule の長さが不正");
                    }
                    if ty == DRAIN_WEBTRANSPORT_SESSION && len != 0 {
                        return Err("DRAIN_WEBTRANSPORT_SESSION の長さが 0 でない");
                    }
                    if self.buf.len() < hdr + len as usize {
                        break;
                    }
                    self.buf.advance(hdr);
                    let body: Bytes = self.buf.split_to(len as usize).freeze();
                    if ty == DRAIN_WEBTRANSPORT_SESSION {
                        out.push(Capsule::DrainWebTransportSession);
                    } else {
                        let code = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                        let reason = String::from_utf8(body[4..].to_vec()).map_err(|_| "理由が UTF-8 でない")?;
                        out.push(Capsule::CloseWebTransportSession { code, reason });
                    }
                }
                _ => {
                    // 未知の capsule は読み捨てる（RFC 9297 3.2）
                    self.buf.advance(hdr);
                    let have = (self.buf.len() as u64).min(len);
                    self.buf.advance(have as usize);
                    self.skip = len - have;
                    out.push(Capsule::Unknown { ty });
                    if self.skip > 0 {
                        break;
                    }
                }
            }
        }
        if self.buf.len() > 2 * MAX_CAPSULE as usize {
            return Err("capsule が大きすぎる");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capsule_roundtrip_and_split() {
        let c = Capsule::CloseWebTransportSession {
            code: 7,
            reason: "bye".into(),
        };
        let mut e = Capsule::Unknown { ty: 0x1234 }.encode();
        e.extend_from_slice(&c.encode());
        e.extend_from_slice(&Capsule::DrainWebTransportSession.encode());
        let mut r = CapsuleReader::default();
        let mut got = Vec::new();
        for b in e.chunks(1) {
            got.extend(r.push(b).unwrap());
        }
        assert_eq!(got, vec![Capsule::Unknown { ty: 0x1234 }, c, Capsule::DrainWebTransportSession]);
        let mut r = CapsuleReader::default();
        assert!(r.push(&[0x80, 0x00, 0x28, 0x43, 0x02, 0, 0]).is_err());
    }
}
