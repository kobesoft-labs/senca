//! SRT パケットの符号化と解析（ヘッダ 16 バイト、データと制御）。
//!
//! 数値はすべてネットワークバイト順。壊れた入力は `Err` を返し、パニックしない。

use bytes::{BufMut, Bytes, BytesMut};

/// SRT ヘッダの長さ。
pub const HEADER_LEN: usize = 16;
/// シーケンス番号は 31 ビット。
pub const SEQ_MASK: u32 = 0x7FFF_FFFF;
/// メッセージ番号は 26 ビット。
pub const MSGNO_MASK: u32 = 0x03FF_FFFF;
/// UDP ペイロードとして受け付ける最大長（MTU 1500 から IP/UDP を引いた値）。
pub const MAX_DATAGRAM: usize = 1472;

/// 解析の失敗。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// 長さが足りない。
    Truncated,
    /// 長さが上限を超える。
    TooLong,
    /// 値が仕様の範囲外。
    Invalid(&'static str),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Truncated => f.write_str("truncated packet"),
            ParseError::TooLong => f.write_str("packet too long"),
            ParseError::Invalid(what) => write!(f, "invalid field: {what}"),
        }
    }
}

impl std::error::Error for ParseError {}

pub(crate) fn be32(b: &[u8], off: usize) -> Result<u32, ParseError> {
    let s = b.get(off..off + 4).ok_or(ParseError::Truncated)?;
    Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// 制御パケットの種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlType {
    Handshake,
    KeepAlive,
    Ack,
    Nak,
    CongestionWarning,
    Shutdown,
    AckAck,
    DropReq,
    PeerError,
    /// 拡張（0x7FFF）。KMREQ/KMRSP はここに subtype として載る。
    UserDefined,
    Unknown(u16),
}

impl ControlType {
    pub fn code(self) -> u16 {
        match self {
            ControlType::Handshake => 0,
            ControlType::KeepAlive => 1,
            ControlType::Ack => 2,
            ControlType::Nak => 3,
            ControlType::CongestionWarning => 4,
            ControlType::Shutdown => 5,
            ControlType::AckAck => 6,
            ControlType::DropReq => 7,
            ControlType::PeerError => 8,
            ControlType::UserDefined => 0x7FFF,
            ControlType::Unknown(c) => c,
        }
    }

    pub fn from_code(c: u16) -> Self {
        match c {
            0 => ControlType::Handshake,
            1 => ControlType::KeepAlive,
            2 => ControlType::Ack,
            3 => ControlType::Nak,
            4 => ControlType::CongestionWarning,
            5 => ControlType::Shutdown,
            6 => ControlType::AckAck,
            7 => ControlType::DropReq,
            8 => ControlType::PeerError,
            0x7FFF => ControlType::UserDefined,
            other => ControlType::Unknown(other),
        }
    }
}

/// 拡張制御メッセージの subtype（KMREQ=3, KMRSP=4）。
pub const EXT_KMREQ: u16 = 3;
pub const EXT_KMRSP: u16 = 4;

/// データパケットのメッセージ内位置（PP フィールド）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    Middle,
    Last,
    First,
    Solo,
}

/// データパケットのヘッダ。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataHeader {
    pub seq: u32,
    pub position: Position,
    pub in_order: bool,
    /// 0=平文、1=偶数鍵、2=奇数鍵（3 は不正）。
    pub key_flags: u8,
    pub retransmitted: bool,
    pub msgno: u32,
    pub timestamp: u32,
    pub dest: u32,
}

impl DataHeader {
    pub fn word1(&self) -> u32 {
        let pp = match self.position {
            Position::Middle => 0,
            Position::Last => 1,
            Position::First => 2,
            Position::Solo => 3,
        };
        (pp << 30)
            | ((self.in_order as u32) << 29)
            | (((self.key_flags & 3) as u32) << 27)
            | ((self.retransmitted as u32) << 26)
            | (self.msgno & MSGNO_MASK)
    }

    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0..4].copy_from_slice(&(self.seq & SEQ_MASK).to_be_bytes());
        h[4..8].copy_from_slice(&self.word1().to_be_bytes());
        h[8..12].copy_from_slice(&self.timestamp.to_be_bytes());
        h[12..16].copy_from_slice(&self.dest.to_be_bytes());
        h
    }
}

/// 制御パケットのヘッダ。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlHeader {
    pub ctype: ControlType,
    pub subtype: u16,
    pub info: u32,
    pub timestamp: u32,
    pub dest: u32,
}

/// 解析したパケット。本体は入力の部分スライス。
#[derive(Debug)]
pub enum Packet<'a> {
    Data(DataHeader, &'a [u8]),
    Control(ControlHeader, &'a [u8]),
}

/// 宛先 socket ID だけを読む（Listener の振り分け用）。
pub fn peek_dest(buf: &[u8]) -> Option<u32> {
    be32(buf, 12).ok()
}

/// パケットを解析する。
pub fn parse(buf: &[u8]) -> Result<Packet<'_>, ParseError> {
    if buf.len() < HEADER_LEN {
        return Err(ParseError::Truncated);
    }
    if buf.len() > MAX_DATAGRAM {
        return Err(ParseError::TooLong);
    }
    let w0 = be32(buf, 0)?;
    let w1 = be32(buf, 4)?;
    let timestamp = be32(buf, 8)?;
    let dest = be32(buf, 12)?;
    let body = &buf[HEADER_LEN..];
    if w0 & 0x8000_0000 == 0 {
        let position = match w1 >> 30 {
            0 => Position::Middle,
            1 => Position::Last,
            2 => Position::First,
            _ => Position::Solo,
        };
        let key_flags = ((w1 >> 27) & 3) as u8;
        if key_flags == 3 {
            return Err(ParseError::Invalid("key flags"));
        }
        Ok(Packet::Data(
            DataHeader {
                seq: w0,
                position,
                in_order: (w1 >> 29) & 1 == 1,
                key_flags,
                retransmitted: (w1 >> 26) & 1 == 1,
                msgno: w1 & MSGNO_MASK,
                timestamp,
                dest,
            },
            body,
        ))
    } else {
        Ok(Packet::Control(
            ControlHeader {
                ctype: ControlType::from_code(((w0 >> 16) & 0x7FFF) as u16),
                subtype: (w0 & 0xFFFF) as u16,
                info: w1,
                timestamp,
                dest,
            },
            body,
        ))
    }
}

/// 制御パケットを組み立てる。
pub fn build_control(ctype: ControlType, subtype: u16, info: u32, timestamp: u32, dest: u32, cif: &[u8]) -> Bytes {
    let mut b = BytesMut::with_capacity(HEADER_LEN + cif.len());
    b.put_u32(0x8000_0000 | ((ctype.code() as u32 & 0x7FFF) << 16) | subtype as u32);
    b.put_u32(info);
    b.put_u32(timestamp);
    b.put_u32(dest);
    b.put_slice(cif);
    b.freeze()
}

/// 32 ビット語の列を CIF にする。
pub fn words(ws: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(ws.len() * 4);
    for w in ws {
        v.extend_from_slice(&w.to_be_bytes());
    }
    v
}

/// NAK の損失リスト（範囲は先頭語の最上位ビットで示す）を符号化する。上限語数を超えた分は捨てる。
pub fn encode_loss_list(ranges: &[(u32, u32)], max_words: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut n = 0;
    for &(a, b) in ranges {
        let need = if a == b { 1 } else { 2 };
        if n + need > max_words {
            break;
        }
        if a == b {
            out.extend_from_slice(&(a & SEQ_MASK).to_be_bytes());
        } else {
            out.extend_from_slice(&((a & SEQ_MASK) | 0x8000_0000).to_be_bytes());
            out.extend_from_slice(&(b & SEQ_MASK).to_be_bytes());
        }
        n += need;
    }
    out
}

/// NAK の損失リストを解析する。範囲は (先頭, 末尾) の 31 ビット番号。
pub fn decode_loss_list(cif: &[u8], max_ranges: usize) -> Result<Vec<(u32, u32)>, ParseError> {
    if !cif.len().is_multiple_of(4) {
        return Err(ParseError::Invalid("loss list length"));
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= cif.len() {
        if out.len() >= max_ranges {
            break;
        }
        let w = be32(cif, i)?;
        i += 4;
        if w & 0x8000_0000 != 0 {
            let end = be32(cif, i)?;
            i += 4;
            if end & 0x8000_0000 != 0 {
                return Err(ParseError::Invalid("loss range end"));
            }
            out.push((w & SEQ_MASK, end));
        } else {
            out.push((w, w));
        }
    }
    Ok(out)
}

/// 31 ビットのシーケンス番号を、参照点（64 ビットの通し番号）に最も近い通し番号へ戻す。
pub fn seq_to_abs(reference: u64, seq: u32) -> u64 {
    let r = (reference as u32) & SEQ_MASK;
    let d = seq.wrapping_sub(r) & SEQ_MASK;
    let d = if d >= 0x4000_0000 { d as i64 - 0x8000_0000 } else { d as i64 };
    (reference as i64).saturating_add(d).max(0) as u64
}

/// 通し番号から 31 ビットのシーケンス番号へ。
pub fn abs_to_seq(abs: u64) -> u32 {
    (abs as u32) & SEQ_MASK
}

/// 32 ビットのタイムスタンプ（µs）を参照点に最も近い 64 ビット値へ戻す。
pub fn ts_to_abs(reference: u64, ts: u32) -> u64 {
    let d = ts.wrapping_sub(reference as u32) as i32 as i64;
    (reference as i64).saturating_add(d).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_header_roundtrip() {
        let h = DataHeader {
            seq: 0x7FFF_FFFE,
            position: Position::Solo,
            in_order: false,
            key_flags: 2,
            retransmitted: true,
            msgno: 12345,
            timestamp: 0xDEAD_BEEF,
            dest: 77,
        };
        let mut buf = h.to_bytes().to_vec();
        buf.extend_from_slice(b"hello");
        match parse(&buf).unwrap() {
            Packet::Data(p, body) => {
                assert_eq!(p, h);
                assert_eq!(body, b"hello");
            }
            _ => panic!("not data"),
        }
    }

    #[test]
    fn control_roundtrip() {
        let b = build_control(ControlType::UserDefined, EXT_KMREQ, 9, 10, 11, &[1, 2, 3, 4]);
        match parse(&b).unwrap() {
            Packet::Control(h, body) => {
                assert_eq!(h.ctype, ControlType::UserDefined);
                assert_eq!(h.subtype, EXT_KMREQ);
                assert_eq!((h.info, h.timestamp, h.dest), (9, 10, 11));
                assert_eq!(body, &[1, 2, 3, 4]);
            }
            _ => panic!("not control"),
        }
    }

    #[test]
    fn loss_list_roundtrip() {
        let r = vec![(5, 5), (10, 20), (SEQ_MASK, SEQ_MASK)];
        let enc = encode_loss_list(&r, 100);
        assert_eq!(decode_loss_list(&enc, 100).unwrap(), r);
        assert!(decode_loss_list(&enc[..12], 100).is_ok());
        assert!(decode_loss_list(&enc[..8], 100).is_err());
        assert!(decode_loss_list(&enc[..6], 100).is_err());
        // 範囲の末尾が欠けている
        assert!(decode_loss_list(&enc[4..8], 100).is_err());
    }

    #[test]
    fn seq_wrap() {
        let base = (1u64 << 40) + SEQ_MASK as u64; // seq = 0x7FFFFFFF
        assert_eq!(abs_to_seq(base), SEQ_MASK);
        assert_eq!(seq_to_abs(base, 0), base + 1);
        assert_eq!(seq_to_abs(base, SEQ_MASK - 5), base - 5);
        assert_eq!(ts_to_abs(0xFFFF_FFF0, 0x10), 0x1_0000_0010);
        assert_eq!(ts_to_abs(0x1_0000_0010, 0xFFFF_FFF0), 0xFFFF_FFF0);
    }
}
