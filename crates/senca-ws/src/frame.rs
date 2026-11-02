//! frame の見出し（RFC 6455 5.2）と符号化。

use bytes::{BufMut, Bytes, BytesMut};

/// opcode。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpCode {
    Continuation,
    Text,
    Binary,
    Close,
    Ping,
    Pong,
}

impl OpCode {
    pub fn from_u8(v: u8) -> Option<OpCode> {
        Some(match v {
            0 => OpCode::Continuation,
            1 => OpCode::Text,
            2 => OpCode::Binary,
            8 => OpCode::Close,
            9 => OpCode::Ping,
            10 => OpCode::Pong,
            _ => return None,
        })
    }

    pub fn as_u8(self) -> u8 {
        match self {
            OpCode::Continuation => 0,
            OpCode::Text => 1,
            OpCode::Binary => 2,
            OpCode::Close => 8,
            OpCode::Ping => 9,
            OpCode::Pong => 10,
        }
    }

    pub fn is_control(self) -> bool {
        matches!(self, OpCode::Close | OpCode::Ping | OpCode::Pong)
    }
}

/// 解析した見出し。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub fin: bool,
    /// RSV1〜3（拡張を交渉しないので 0 でなければならない）。
    pub rsv: u8,
    /// 未知の opcode は `Err`（呼び出し側が 1002 で閉じる）。
    pub opcode: Result<OpCode, u8>,
    pub mask: Option<[u8; 4]>,
    pub payload_len: u64,
    /// 見出しの長さ（byte）。
    pub header_len: usize,
}

/// 見出しの誤り。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// 長さが最小の形で符号化されていない、または 64 bit の最上位ビットが立っている。
    BadLength,
}

impl FrameHeader {
    /// `buf` の先頭の見出しを読む。足りなければ `Ok(None)`。
    pub fn parse(buf: &[u8]) -> Result<Option<FrameHeader>, HeaderError> {
        if buf.len() < 2 {
            return Ok(None);
        }
        let b0 = buf[0];
        let b1 = buf[1];
        let masked = b1 & 0x80 != 0;
        let len7 = b1 & 0x7f;
        let mut pos = 2usize;
        let payload_len = match len7 {
            126 => {
                if buf.len() < pos + 2 {
                    return Ok(None);
                }
                let v = u16::from_be_bytes([buf[2], buf[3]]) as u64;
                pos += 2;
                if v < 126 {
                    return Err(HeaderError::BadLength);
                }
                v
            }
            127 => {
                if buf.len() < pos + 8 {
                    return Ok(None);
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&buf[2..10]);
                let v = u64::from_be_bytes(b);
                pos += 8;
                if v >> 63 != 0 || v <= 0xffff {
                    return Err(HeaderError::BadLength);
                }
                v
            }
            n => n as u64,
        };
        let mask = if masked {
            if buf.len() < pos + 4 {
                return Ok(None);
            }
            let m = [buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]];
            pos += 4;
            Some(m)
        } else {
            None
        };
        Ok(Some(FrameHeader {
            fin: b0 & 0x80 != 0,
            rsv: (b0 >> 4) & 0x07,
            opcode: OpCode::from_u8(b0 & 0x0f).ok_or(b0 & 0x0f),
            mask,
            payload_len,
            header_len: pos,
        }))
    }
}

/// payload に mask を掛ける（掛け直すと元に戻る）。`offset` は payload の中の位置（分けて掛けるとき）。
pub fn apply_mask(data: &mut [u8], mask: [u8; 4], offset: usize) {
    for (i, b) in data.iter_mut().enumerate() {
        *b ^= mask[(offset + i) & 3];
    }
}

/// frame を 1 つ符号化する。`mask` はクライアントが送るときだけ（サーバーは `None`）。
pub fn encode_frame(fin: bool, opcode: OpCode, payload: &[u8], mask: Option<[u8; 4]>) -> Bytes {
    let len = payload.len();
    let mut out = BytesMut::with_capacity(len + 14);
    out.put_u8(if fin { 0x80 } else { 0 } | opcode.as_u8());
    let mbit = if mask.is_some() { 0x80 } else { 0 };
    if len < 126 {
        out.put_u8(mbit | len as u8);
    } else if len <= 0xffff {
        out.put_u8(mbit | 126);
        out.put_u16(len as u16);
    } else {
        out.put_u8(mbit | 127);
        out.put_u64(len as u64);
    }
    match mask {
        Some(m) => {
            out.put_slice(&m);
            let start = out.len();
            out.put_slice(payload);
            apply_mask(&mut out[start..], m, 0);
        }
        None => out.put_slice(payload),
    }
    out.freeze()
}

/// 受けた close の code が送ってよいものか（RFC 6455 7.4、IANA の登録）。
/// 1004・1005・1006・1015 は frame に載せてはいけない。3000〜4999 は利用者の領域。
pub fn close_code_is_valid(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_examples() {
        // RFC 6455 5.7: 1 つの mask していない text "Hello"
        let f = encode_frame(true, OpCode::Text, b"Hello", None);
        assert_eq!(&f[..], &[0x81, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f]);
        // mask した "Hello"
        let f = encode_frame(true, OpCode::Text, b"Hello", Some([0x37, 0xfa, 0x21, 0x3d]));
        assert_eq!(
            &f[..],
            &[0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58]
        );
        let h = FrameHeader::parse(&f).unwrap().unwrap();
        assert!(h.fin);
        assert_eq!(h.opcode, Ok(OpCode::Text));
        assert_eq!(h.payload_len, 5);
        assert_eq!(h.header_len, 6);
        // 256 byte の binary（16 bit の長さ）
        let f = encode_frame(true, OpCode::Binary, &[0u8; 256], None);
        assert_eq!(&f[..4], &[0x82, 0x7e, 0x01, 0x00]);
        // 64 KiB（64 bit の長さ）
        let f = encode_frame(true, OpCode::Binary, &vec![0u8; 65536], None);
        assert_eq!(&f[..10], &[0x82, 0x7f, 0, 0, 0, 0, 0, 1, 0, 0]);
        let h = FrameHeader::parse(&f).unwrap().unwrap();
        assert_eq!(h.payload_len, 65536);
        assert_eq!(h.header_len, 10);
    }

    #[test]
    fn partial_and_bad_lengths() {
        let f = encode_frame(true, OpCode::Binary, &vec![1u8; 70000], Some([1, 2, 3, 4]));
        for n in 0..14 {
            assert_eq!(FrameHeader::parse(&f[..n]).unwrap(), None, "{n}");
        }
        assert!(FrameHeader::parse(&f[..14]).unwrap().is_some());
        // 16 bit で 125 以下は最小でない
        assert_eq!(FrameHeader::parse(&[0x82, 0x7e, 0x00, 0x7d]), Err(HeaderError::BadLength));
        // 64 bit で 65535 以下
        assert_eq!(
            FrameHeader::parse(&[0x82, 0x7f, 0, 0, 0, 0, 0, 0, 0xff, 0xff]),
            Err(HeaderError::BadLength)
        );
        // 最上位ビット
        assert_eq!(
            FrameHeader::parse(&[0x82, 0x7f, 0x80, 0, 0, 0, 0, 0, 0, 0]),
            Err(HeaderError::BadLength)
        );
        // 予約の opcode と RSV
        let h = FrameHeader::parse(&[0xf3, 0x00]).unwrap().unwrap();
        assert_eq!(h.opcode, Err(3));
        assert_eq!(h.rsv, 7);
    }

    #[test]
    fn close_codes() {
        for ok in [1000, 1001, 1002, 1003, 1007, 1008, 1009, 1010, 1011, 3000, 3999, 4000, 4999] {
            assert!(close_code_is_valid(ok), "{ok}");
        }
        for bad in [0, 999, 1004, 1005, 1006, 1015, 1016, 1100, 2000, 2999, 5000, 65535] {
            assert!(!close_code_is_valid(bad), "{bad}");
        }
    }
}
