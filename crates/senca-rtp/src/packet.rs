//! RTP パケット（RFC 3550 5.1）とヘッダー拡張（RFC 3550 5.3.1・RFC 8285）。

use crate::error::{Error, Result};
use bytes::Bytes;

/// CSRC の最大数（CC は 4 ビット）。
pub const MAX_CSRC: usize = 15;
/// RFC 8285 の one-byte 形式の profile（0xBEDE）。
pub const ONE_BYTE_PROFILE: u16 = 0xBEDE;
/// RFC 8285 の two-byte 形式の profile（上位 12 ビットが 0x100、下位 4 ビットは appbits）。
pub const TWO_BYTE_PROFILE: u16 = 0x1000;

/// RTP の固定ヘッダーと CSRC・拡張。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RtpHeader {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub csrc: Vec<u32>,
    pub extension: Option<HeaderExtension>,
}

/// ヘッダー拡張（profile 固有の 16 ビットと、4 バイト単位のデータ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderExtension {
    pub profile: u16,
    /// 拡張の本体（長さは 4 の倍数）。
    pub data: Bytes,
}

/// RFC 8285 の拡張要素 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionElement {
    pub id: u8,
    pub data: Bytes,
}

impl HeaderExtension {
    /// one-byte 形式か。
    pub fn is_one_byte(&self) -> bool {
        self.profile == ONE_BYTE_PROFILE
    }

    /// two-byte 形式か（appbits は無視する）。
    pub fn is_two_byte(&self) -> bool {
        self.profile & 0xFFF0 == TWO_BYTE_PROFILE
    }

    /// RFC 8285 の要素に分ける。one-byte・two-byte 以外の profile は `Unsupported`。
    pub fn elements(&self) -> Result<Vec<ExtensionElement>> {
        let d = &self.data;
        let mut out = Vec::new();
        let mut i = 0usize;
        if self.is_one_byte() {
            while i < d.len() {
                let b = d[i];
                if b == 0 {
                    i += 1; // padding
                    continue;
                }
                let id = b >> 4;
                if id == 15 {
                    break; // 以降は読まない（RFC 8285 4.2）
                }
                let len = (b & 0x0f) as usize + 1;
                let start = i + 1;
                let end = start + len;
                if end > d.len() {
                    return Err(Error::Truncated);
                }
                out.push(ExtensionElement {
                    id,
                    data: d.slice(start..end),
                });
                i = end;
            }
        } else if self.is_two_byte() {
            while i < d.len() {
                let id = d[i];
                if id == 0 {
                    i += 1;
                    continue;
                }
                let len = *d.get(i + 1).ok_or(Error::Truncated)? as usize;
                let start = i + 2;
                let end = start + len;
                if end > d.len() {
                    return Err(Error::Truncated);
                }
                out.push(ExtensionElement {
                    id,
                    data: d.slice(start..end),
                });
                i = end;
            }
        } else {
            return Err(Error::Unsupported("RFC 8285 以外のヘッダー拡張"));
        }
        Ok(out)
    }

    /// 指定した ID の要素（最初のもの）。
    pub fn get(&self, id: u8) -> Option<Bytes> {
        self.elements()
            .ok()?
            .into_iter()
            .find(|e| e.id == id)
            .map(|e| e.data)
    }

    /// 要素から組み立てる。すべて ID 1〜14・長さ 1〜16 なら one-byte、そうでなければ two-byte（ID 1〜255・長さ 0〜255）。
    pub fn build(elements: &[(u8, &[u8])]) -> Result<HeaderExtension> {
        let one = elements
            .iter()
            .all(|(id, d)| (1..=14).contains(id) && (1..=16).contains(&d.len()));
        let mut data = Vec::new();
        let profile = if one {
            for (id, d) in elements {
                data.push((id << 4) | (d.len() as u8 - 1));
                data.extend_from_slice(d);
            }
            ONE_BYTE_PROFILE
        } else {
            for (id, d) in elements {
                if *id == 0 {
                    return Err(Error::Invalid("拡張要素の ID 0"));
                }
                let len = u8::try_from(d.len())
                    .map_err(|_| Error::Limit("拡張要素が 255 バイトを超える"))?;
                data.push(*id);
                data.push(len);
                data.extend_from_slice(d);
            }
            TWO_BYTE_PROFILE
        };
        while data.len() % 4 != 0 {
            data.push(0);
        }
        if data.len() / 4 > u16::MAX as usize {
            return Err(Error::Limit("ヘッダー拡張が長すぎる"));
        }
        Ok(HeaderExtension {
            profile,
            data: Bytes::from(data),
        })
    }
}

/// RTP パケット。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RtpPacket {
    pub header: RtpHeader,
    pub payload: Bytes,
    /// 末尾の padding の長さ（0 は padding なし。最後のバイトも数える）。
    pub padding: u8,
}

impl RtpPacket {
    /// 拡張・CSRC・padding なしのパケットを作る。
    pub fn new(
        payload_type: u8,
        sequence: u16,
        timestamp: u32,
        ssrc: u32,
        marker: bool,
        payload: Bytes,
    ) -> Self {
        RtpPacket {
            header: RtpHeader {
                marker,
                payload_type: payload_type & 0x7f,
                sequence,
                timestamp,
                ssrc,
                csrc: Vec::new(),
                extension: None,
            },
            payload,
            padding: 0,
        }
    }

    /// 解析する。payload・拡張は `data` の切り出し（コピーしない）。
    pub fn parse(data: Bytes) -> Result<RtpPacket> {
        if data.len() < 12 {
            return Err(Error::Truncated);
        }
        let b0 = data[0];
        if b0 >> 6 != 2 {
            return Err(Error::Invalid("RTP の version が 2 ではない"));
        }
        let has_padding = b0 & 0x20 != 0;
        let has_ext = b0 & 0x10 != 0;
        let cc = (b0 & 0x0f) as usize;
        let b1 = data[1];
        let mut pos = 12usize;
        let mut csrc = Vec::with_capacity(cc);
        for _ in 0..cc {
            let s = data.get(pos..pos + 4).ok_or(Error::Truncated)?;
            csrc.push(u32::from_be_bytes([s[0], s[1], s[2], s[3]]));
            pos += 4;
        }
        let extension = if has_ext {
            let h = data.get(pos..pos + 4).ok_or(Error::Truncated)?;
            let profile = u16::from_be_bytes([h[0], h[1]]);
            let len = u16::from_be_bytes([h[2], h[3]]) as usize * 4;
            pos += 4;
            if pos + len > data.len() {
                return Err(Error::Truncated);
            }
            let ext = HeaderExtension {
                profile,
                data: data.slice(pos..pos + len),
            };
            pos += len;
            Some(ext)
        } else {
            None
        };
        let mut end = data.len();
        let mut padding = 0u8;
        if has_padding {
            let p = data[end - 1];
            if p == 0 || pos + p as usize > end {
                return Err(Error::Invalid("padding の長さ"));
            }
            padding = p;
            end -= p as usize;
        }
        Ok(RtpPacket {
            header: RtpHeader {
                marker: b1 & 0x80 != 0,
                payload_type: b1 & 0x7f,
                sequence: u16::from_be_bytes([data[2], data[3]]),
                timestamp: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
                ssrc: u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
                csrc,
                extension,
            },
            payload: data.slice(pos..end),
            padding,
        })
    }

    /// ヘッダーの長さ（CSRC・拡張を含む）。
    pub fn header_len(&self) -> usize {
        12 + 4 * self.header.csrc.len()
            + self
                .header
                .extension
                .as_ref()
                .map_or(0, |e| 4 + e.data.len())
    }

    /// 書き出したときの長さ。
    pub fn encoded_len(&self) -> usize {
        self.header_len() + self.payload.len() + self.padding as usize
    }

    /// `out` に書く。
    pub fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        let h = &self.header;
        if h.csrc.len() > MAX_CSRC {
            return Err(Error::Limit("CSRC が 15 個を超える"));
        }
        if h.payload_type > 127 {
            return Err(Error::Invalid("payload type"));
        }
        out.reserve(self.encoded_len());
        out.push(
            0x80 | ((self.padding > 0) as u8) << 5
                | (h.extension.is_some() as u8) << 4
                | h.csrc.len() as u8,
        );
        out.push((h.marker as u8) << 7 | h.payload_type);
        out.extend_from_slice(&h.sequence.to_be_bytes());
        out.extend_from_slice(&h.timestamp.to_be_bytes());
        out.extend_from_slice(&h.ssrc.to_be_bytes());
        for c in &h.csrc {
            out.extend_from_slice(&c.to_be_bytes());
        }
        if let Some(e) = &h.extension {
            if e.data.len() % 4 != 0 {
                return Err(Error::Invalid("ヘッダー拡張の長さが 4 の倍数ではない"));
            }
            let words = u16::try_from(e.data.len() / 4)
                .map_err(|_| Error::Limit("ヘッダー拡張が長すぎる"))?;
            out.extend_from_slice(&e.profile.to_be_bytes());
            out.extend_from_slice(&words.to_be_bytes());
            out.extend_from_slice(&e.data);
        }
        out.extend_from_slice(&self.payload);
        if self.padding > 0 {
            out.resize(out.len() + self.padding as usize - 1, 0);
            out.push(self.padding);
        }
        Ok(())
    }

    /// バイト列にする。
    pub fn to_bytes(&self) -> Result<Bytes> {
        let mut v = Vec::with_capacity(self.encoded_len());
        self.write(&mut v)?;
        Ok(Bytes::from(v))
    }
}

/// RTP と RTCP を同じポートで受けるとき（RFC 5761 4）、RTCP かどうか（2 バイト目が 192〜223）。
pub fn is_rtcp(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] >> 6 == 2 && (192..=223).contains(&data[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_basic() {
        let p = RtpPacket::new(
            96,
            0xFFFF,
            0xDEADBEEF,
            0x1234_5678,
            true,
            Bytes::from_static(b"abc"),
        );
        let b = p.to_bytes().unwrap();
        assert_eq!(
            &b[..12],
            &[
                0x80, 0xE0, 0xFF, 0xFF, 0xDE, 0xAD, 0xBE, 0xEF, 0x12, 0x34, 0x56, 0x78
            ]
        );
        assert_eq!(RtpPacket::parse(b).unwrap(), p);
    }

    #[test]
    fn csrc_extension_padding() {
        let ext = HeaderExtension::build(&[(1, &[0xAA]), (3, &[1, 2, 3])]).unwrap();
        assert_eq!(ext.profile, ONE_BYTE_PROFILE);
        assert_eq!(&ext.data[..], &[0x10, 0xAA, 0x32, 1, 2, 3, 0, 0]);
        let mut p = RtpPacket::new(0, 1, 2, 3, false, Bytes::from_static(b"hello"));
        p.header.csrc = vec![7, 8];
        p.header.extension = Some(ext);
        p.padding = 3;
        let b = p.to_bytes().unwrap();
        assert_eq!(b.len(), 12 + 8 + 4 + 8 + 5 + 3);
        assert_eq!(b[b.len() - 1], 3);
        let q = RtpPacket::parse(b).unwrap();
        assert_eq!(q, p);
        let els = q.header.extension.as_ref().unwrap().elements().unwrap();
        assert_eq!(els.len(), 2);
        assert_eq!(els[1].id, 3);
        assert_eq!(&els[1].data[..], &[1, 2, 3]);
        assert_eq!(
            q.header.extension.as_ref().unwrap().get(1).unwrap(),
            Bytes::from_static(&[0xAA])
        );
    }

    #[test]
    fn two_byte_extension() {
        let big = [9u8; 20];
        let ext = HeaderExtension::build(&[(20, &[1]), (2, &big), (5, &[])]).unwrap();
        assert!(ext.is_two_byte());
        let els = ext.elements().unwrap();
        assert_eq!(els.len(), 3);
        assert_eq!(els[0].id, 20);
        assert_eq!(els[1].data.len(), 20);
        assert!(els[2].data.is_empty());
    }

    #[test]
    fn one_byte_stop_and_padding() {
        // RFC 8285: 0 はパディング、ID 15 で打ち切り
        let ext = HeaderExtension {
            profile: ONE_BYTE_PROFILE,
            data: Bytes::from_static(&[0, 0x21, 1, 2, 0xF0, 0x55, 0, 0]),
        };
        let els = ext.elements().unwrap();
        assert_eq!(els.len(), 1);
        assert_eq!(&els[0].data[..], &[1, 2]);
        let bad = HeaderExtension {
            profile: ONE_BYTE_PROFILE,
            data: Bytes::from_static(&[0x2F, 1, 2, 3]),
        };
        assert_eq!(bad.elements(), Err(Error::Truncated));
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(
            RtpPacket::parse(Bytes::from_static(&[0x80; 11])),
            Err(Error::Truncated)
        );
        assert!(RtpPacket::parse(Bytes::from_static(&[0x40; 12])).is_err());
        // CC=2 だが CSRC が足りない
        let mut v = vec![0x82, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3];
        assert!(RtpPacket::parse(Bytes::from(v.clone())).is_err());
        // padding 長がヘッダーまで食い込む
        v = vec![0xA0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 50];
        assert!(RtpPacket::parse(Bytes::from(v)).is_err());
        // padding 0
        let v = vec![0xA0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(RtpPacket::parse(Bytes::from(v)).is_err());
        // 拡張長が足りない
        let v = vec![0x90, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xBE, 0xDE, 0, 9];
        assert!(RtpPacket::parse(Bytes::from(v)).is_err());
    }

    #[test]
    fn fuzz_never_panics() {
        let mut seed = 0x1234_5678u32;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..20000 {
            let len = (rnd() % 64) as usize;
            let mut v: Vec<u8> = (0..len).map(|_| rnd() as u8).collect();
            if !v.is_empty() {
                v[0] = (v[0] & 0x3f) | 0x80;
            }
            if let Ok(p) = RtpPacket::parse(Bytes::from(v)) {
                if let Some(e) = &p.header.extension {
                    let _ = e.elements();
                }
                let b = p.to_bytes().unwrap();
                assert_eq!(RtpPacket::parse(b).unwrap(), p);
            }
        }
    }

    #[test]
    fn rtcp_demux() {
        assert!(is_rtcp(&[0x80, 200]));
        assert!(is_rtcp(&[0x81, 201]));
        assert!(!is_rtcp(&[0x80, 96]));
        assert!(!is_rtcp(&[0x80, 0xE0]));
    }
}
