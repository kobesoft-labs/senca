//! DCEP（Data Channel Establishment Protocol、RFC 8832）のメッセージ。

use bytes::{BufMut, Bytes};

use crate::{Reliability, SctpError};

/// DATA_CHANNEL_OPEN のメッセージ型。
pub const MSG_OPEN: u8 = 0x03;
/// DATA_CHANNEL_ACK のメッセージ型。
pub const MSG_ACK: u8 = 0x02;

/// チャネルの型（RFC 8832 5.1）。
pub mod channel_type {
    pub const RELIABLE: u8 = 0x00;
    pub const RELIABLE_UNORDERED: u8 = 0x80;
    pub const PARTIAL_RELIABLE_REXMIT: u8 = 0x01;
    pub const PARTIAL_RELIABLE_REXMIT_UNORDERED: u8 = 0x81;
    pub const PARTIAL_RELIABLE_TIMED: u8 = 0x02;
    pub const PARTIAL_RELIABLE_TIMED_UNORDERED: u8 = 0x82;
}

/// DATA_CHANNEL_OPEN。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Open {
    pub ordered: bool,
    pub reliability: Reliability,
    pub priority: u16,
    pub label: String,
    pub protocol: String,
}

/// DCEP のメッセージ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Open(Open),
    Ack,
}

fn malformed(s: &'static str) -> SctpError {
    SctpError::Malformed(s)
}

impl Open {
    /// 書く。label・protocol は 65535 byte まで。
    pub fn encode(&self) -> Result<Bytes, SctpError> {
        if self.label.len() > u16::MAX as usize || self.protocol.len() > u16::MAX as usize {
            return Err(SctpError::TooLarge);
        }
        let (ty, param) = match self.reliability {
            Reliability::Reliable => (channel_type::RELIABLE, 0),
            Reliability::MaxRetransmits(n) => (channel_type::PARTIAL_RELIABLE_REXMIT, n),
            Reliability::MaxLifetimeMs(ms) => (channel_type::PARTIAL_RELIABLE_TIMED, ms),
        };
        let ty = if self.ordered { ty } else { ty | 0x80 };
        let mut v = Vec::with_capacity(12 + self.label.len() + self.protocol.len());
        v.put_u8(MSG_OPEN);
        v.put_u8(ty);
        v.put_u16(self.priority);
        v.put_u32(param);
        v.put_u16(self.label.len() as u16);
        v.put_u16(self.protocol.len() as u16);
        v.put_slice(self.label.as_bytes());
        v.put_slice(self.protocol.as_bytes());
        Ok(Bytes::from(v))
    }
}

/// DATA_CHANNEL_ACK を書く。
pub fn encode_ack() -> Bytes {
    Bytes::from_static(&[MSG_ACK])
}

/// DCEP のメッセージを読む。
pub fn parse(b: &[u8]) -> Result<Message, SctpError> {
    match b.first() {
        Some(&MSG_ACK) => {
            // RFC 8832 5.2: ACK は 1 byte。余計な byte は誤り。
            if b.len() != 1 {
                return Err(malformed("DCEP ACK length"));
            }
            Ok(Message::Ack)
        }
        Some(&MSG_OPEN) => {
            if b.len() < 12 {
                return Err(malformed("DCEP OPEN length"));
            }
            let ty = b[1];
            let priority = u16::from_be_bytes([b[2], b[3]]);
            let param = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
            let ll = u16::from_be_bytes([b[8], b[9]]) as usize;
            let pl = u16::from_be_bytes([b[10], b[11]]) as usize;
            if b.len() != 12 + ll + pl {
                return Err(malformed("DCEP OPEN label/protocol length"));
            }
            let reliability = match ty & 0x7f {
                0x00 => Reliability::Reliable,
                0x01 => Reliability::MaxRetransmits(param),
                0x02 => Reliability::MaxLifetimeMs(param),
                _ => return Err(malformed("DCEP channel type")),
            };
            let label = std::str::from_utf8(&b[12..12 + ll])
                .map_err(|_| malformed("DCEP label not UTF-8"))?
                .to_owned();
            let protocol = std::str::from_utf8(&b[12 + ll..])
                .map_err(|_| malformed("DCEP protocol not UTF-8"))?
                .to_owned();
            Ok(Message::Open(Open {
                ordered: ty & 0x80 == 0,
                reliability,
                priority,
                label,
                protocol,
            }))
        }
        _ => Err(malformed("DCEP message type")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_round_trip() {
        for (ordered, rel) in [
            (true, Reliability::Reliable),
            (false, Reliability::Reliable),
            (true, Reliability::MaxRetransmits(3)),
            (false, Reliability::MaxRetransmits(0)),
            (true, Reliability::MaxLifetimeMs(500)),
            (false, Reliability::MaxLifetimeMs(1)),
        ] {
            let o = Open {
                ordered,
                reliability: rel,
                priority: 256,
                label: "チャット".into(),
                protocol: "proto".into(),
            };
            let b = o.encode().unwrap();
            assert_eq!(parse(&b).unwrap(), Message::Open(o));
        }
        assert_eq!(parse(&encode_ack()).unwrap(), Message::Ack);
    }

    #[test]
    fn chrome_open_bytes() {
        // Chrome の createDataChannel("chat") の OPEN（reliable・ordered・priority 256）。
        let b = [
            0x03, 0x00, 0x01, 0x00, 0, 0, 0, 0, 0x00, 0x04, 0x00, 0x00, b'c', b'h', b'a', b't',
        ];
        let Message::Open(o) = parse(&b).unwrap() else {
            panic!()
        };
        assert_eq!(o.label, "chat");
        assert!(o.ordered);
        assert_eq!(o.priority, 256);
    }

    #[test]
    fn bad_messages() {
        assert!(parse(&[]).is_err());
        assert!(parse(&[0x02, 0]).is_err());
        assert!(parse(&[0x03, 0, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, b'a']).is_err());
        assert!(parse(&[0x03, 0x05, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
        assert!(parse(&[0x03, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0xff]).is_err());
        assert!(parse(&[0x07]).is_err());
    }
}
