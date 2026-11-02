//! RTX（RFC 4588）の包み直し。SSRC 多重（別 SSRC・別 payload type）の形。
//!
//! RTX パケットの payload は「元の番号（OSN, 16 ビット）+ 元の payload」。

use crate::error::{Error, Result};
use crate::packet::RtpPacket;
use bytes::{BufMut, BytesMut};

/// 元のパケットを RTX パケットにする。ヘッダーの marker・timestamp・CSRC・拡張は元のまま。
pub fn rtx_wrap(
    original: &RtpPacket,
    rtx_payload_type: u8,
    rtx_ssrc: u32,
    rtx_sequence: u16,
) -> RtpPacket {
    let mut payload = BytesMut::with_capacity(2 + original.payload.len());
    payload.put_u16(original.header.sequence);
    payload.extend_from_slice(&original.payload);
    let mut header = original.header.clone();
    header.payload_type = rtx_payload_type & 0x7f;
    header.ssrc = rtx_ssrc;
    header.sequence = rtx_sequence;
    RtpPacket {
        header,
        payload: payload.freeze(),
        padding: 0,
    }
}

/// RTX パケットから元のパケットを取り出す。`original_payload_type`・`original_ssrc` は SDP の apt と ssrc-group:FID から。
///
/// payload が空の RTX（帯域推定のための padding）は `Ok(None)`。
pub fn rtx_unwrap(
    rtx: &RtpPacket,
    original_payload_type: u8,
    original_ssrc: u32,
) -> Result<Option<RtpPacket>> {
    if rtx.payload.is_empty() {
        return Ok(None);
    }
    if rtx.payload.len() < 2 {
        return Err(Error::Truncated);
    }
    let osn = u16::from_be_bytes([rtx.payload[0], rtx.payload[1]]);
    let mut header = rtx.header.clone();
    header.payload_type = original_payload_type & 0x7f;
    header.ssrc = original_ssrc;
    header.sequence = osn;
    Ok(Some(RtpPacket {
        header,
        payload: rtx.payload.slice(2..),
        padding: 0,
    }))
}

/// 再送の判断（仕様 6.2）: 再送が再生期限に間に合うときだけ送る。
///
/// `deadline_remaining` は受信側の再生期限までの残り、`rtt` は往復時間の見積もり。
pub fn retransmission_useful(
    deadline_remaining: std::time::Duration,
    rtt: std::time::Duration,
) -> bool {
    deadline_remaining > rtt
}

/// 送った RTP パケットを再送のために一定数だけ覚えておく（番号で引く）。
#[derive(Debug)]
pub struct RetransmissionHistory {
    slots: Vec<Option<RtpPacket>>,
}

impl RetransmissionHistory {
    /// `capacity` 個まで覚える（2 の累乗に切り上げ、最大 32768）。
    pub fn new(capacity: usize) -> Self {
        let n = capacity.clamp(1, 32768).next_power_of_two();
        RetransmissionHistory {
            slots: vec![None; n],
        }
    }

    pub fn insert(&mut self, pkt: RtpPacket) {
        let i = pkt.header.sequence as usize & (self.slots.len() - 1);
        self.slots[i] = Some(pkt);
    }

    pub fn get(&self, seq: u16) -> Option<&RtpPacket> {
        self.slots[seq as usize & (self.slots.len() - 1)]
            .as_ref()
            .filter(|p| p.header.sequence == seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn wrap_unwrap() {
        let mut p = RtpPacket::new(96, 1234, 5678, 0xAAAA, true, Bytes::from_static(b"media"));
        p.header.csrc = vec![1];
        let r = rtx_wrap(&p, 97, 0xBBBB, 7);
        assert_eq!(r.header.payload_type, 97);
        assert_eq!(r.header.sequence, 7);
        assert_eq!(&r.payload[..], b"\x04\xd2media");
        let back = rtx_unwrap(
            &RtpPacket::parse(r.to_bytes().unwrap()).unwrap(),
            96,
            0xAAAA,
        )
        .unwrap()
        .unwrap();
        assert_eq!(back, p);
        let empty = RtpPacket::new(97, 1, 1, 1, false, Bytes::new());
        assert_eq!(rtx_unwrap(&empty, 96, 1).unwrap(), None);
        let short = RtpPacket::new(97, 1, 1, 1, false, Bytes::from_static(&[1]));
        assert!(rtx_unwrap(&short, 96, 1).is_err());
    }

    #[test]
    fn history() {
        let mut h = RetransmissionHistory::new(100);
        for s in 0u16..300 {
            h.insert(RtpPacket::new(96, s, 0, 1, false, Bytes::new()));
        }
        assert!(h.get(299).is_some());
        assert!(h.get(10).is_none());
        assert!(retransmission_useful(
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(50)
        ));
    }
}
