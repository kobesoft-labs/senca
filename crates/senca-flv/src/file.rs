//! FLV ファイルの読み書き（I/O は持たない。バイト列を入れる・出す）。
//!
//! 書き出し: [`write_header`] のあと [`write_tag`]（または [`write_out_tag`]）を順に呼ぶ。
//! 読み込み: [`FlvReader::feed`] でバイト列を入れ、[`FlvReader::next_tag`] でタグを取り出す。
//! タグ本体は内部バッファの切り出し（`Bytes`）で、取り出し時にコピーしない。

use crate::Error;
use crate::build::OutTag;
use crate::tag::TagType;
use bytes::{Buf, Bytes, BytesMut};

/// FLV ファイルヘッダーの内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlvHeader {
    pub has_audio: bool,
    pub has_video: bool,
}

/// FLV ヘッダー（9 バイト）と PreviousTagSize0（4 バイト）を書く。
pub fn write_header(h: FlvHeader, out: &mut Vec<u8>) {
    let flags = (h.has_audio as u8) << 2 | h.has_video as u8;
    out.extend_from_slice(&[b'F', b'L', b'V', 1, flags, 0, 0, 0, 9]);
    out.extend_from_slice(&[0, 0, 0, 0]);
}

/// タグ 1 つ（タグヘッダー 11 バイト + 本体 + PreviousTagSize）を書く。本体は部品の連結。
pub fn write_tag(
    tag_type: TagType,
    timestamp: u32,
    parts: &[&[u8]],
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    let size: usize = parts.iter().map(|p| p.len()).sum();
    if size > 0x00FF_FFFF {
        return Err(Error::TooLarge);
    }
    out.reserve(11 + size + 4);
    out.push(tag_type.as_u8());
    out.extend_from_slice(&(size as u32).to_be_bytes()[1..]);
    out.extend_from_slice(&timestamp.to_be_bytes()[1..]);
    out.push((timestamp >> 24) as u8);
    out.extend_from_slice(&[0, 0, 0]);
    for p in parts {
        out.extend_from_slice(p);
    }
    out.extend_from_slice(&((11 + size) as u32).to_be_bytes());
    Ok(())
}

/// [`OutTag`] を書く。
pub fn write_out_tag(tag: &OutTag, out: &mut Vec<u8>) -> Result<(), Error> {
    write_tag(tag.tag_type, tag.timestamp, &tag.body.parts(), out)
}

/// ファイルから読んだタグ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlvTag {
    pub tag_type: TagType,
    /// ミリ秒（下位 24 bit + 拡張 8 bit）。
    pub timestamp: u32,
    pub body: Bytes,
}

/// FLV ファイルの逐次読み込み。
///
/// 1 つのタグは最大 11 + 16 MiB + 4 バイトなので、[`feed`](Self::feed) の都度
/// [`next_tag`](Self::next_tag) で取り出していれば内部バッファはそれ以上に育たない。
#[derive(Debug, Default)]
pub struct FlvReader {
    buf: BytesMut,
    header: Option<FlvHeader>,
    failed: bool,
}

impl FlvReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// ヘッダーを読んだあとなら、その内容。
    pub fn header(&self) -> Option<FlvHeader> {
        self.header
    }

    /// 未処理のバイト数。
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// 次のタグ。データが足りなければ `Ok(None)`。一度 `Err` を返したら以後も `Err`。
    pub fn next_tag(&mut self) -> Result<Option<FlvTag>, Error> {
        if self.failed {
            return Err(Error::Malformed("reader already failed"));
        }
        let r = self.next_inner();
        if r.is_err() {
            self.failed = true;
        }
        r
    }

    fn next_inner(&mut self) -> Result<Option<FlvTag>, Error> {
        if self.header.is_none() {
            if self.buf.len() < 9 {
                return Ok(None);
            }
            if &self.buf[..3] != b"FLV" || self.buf[3] != 1 {
                return Err(Error::Malformed("not an FLV file"));
            }
            let off =
                u32::from_be_bytes([self.buf[5], self.buf[6], self.buf[7], self.buf[8]]) as usize;
            if !(9..=1024).contains(&off) {
                return Err(Error::Malformed("invalid FLV header size"));
            }
            if self.buf.len() < off + 4 {
                return Ok(None);
            }
            let flags = self.buf[4];
            self.header = Some(FlvHeader {
                has_audio: flags & 0x04 != 0,
                has_video: flags & 0x01 != 0,
            });
            self.buf.advance(off + 4); // PreviousTagSize0 は値を問わない
        }
        loop {
            if self.buf.len() < 11 {
                return Ok(None);
            }
            let b = &self.buf[..11];
            let raw_type = b[0];
            if raw_type & 0x20 != 0 {
                return Err(Error::Malformed("encrypted/filtered FLV tag"));
            }
            let size = u32::from_be_bytes([0, b[1], b[2], b[3]]) as usize;
            let timestamp = u32::from_be_bytes([b[7], b[4], b[5], b[6]]);
            let total = 11 + size + 4;
            if self.buf.len() < total {
                return Ok(None);
            }
            let prev = u32::from_be_bytes([
                self.buf[11 + size],
                self.buf[12 + size],
                self.buf[13 + size],
                self.buf[14 + size],
            ]) as usize;
            if prev != 11 + size {
                return Err(Error::Malformed("PreviousTagSize mismatch"));
            }
            let mut tag = self.buf.split_to(total);
            tag.advance(11);
            tag.truncate(size);
            match TagType::from_u8(raw_type & 0x1F) {
                Some(tag_type) => {
                    return Ok(Some(FlvTag {
                        tag_type,
                        timestamp,
                        body: tag.freeze(),
                    }));
                }
                // 未知の種類のタグは読み飛ばす。
                None => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read() {
        let mut out = Vec::new();
        write_header(
            FlvHeader {
                has_audio: true,
                has_video: true,
            },
            &mut out,
        );
        write_tag(
            TagType::Video,
            0x1234_5678,
            &[&[0x17, 0], &[1, 2, 3]],
            &mut out,
        )
        .unwrap();
        write_tag(TagType::Audio, 5, &[&[0xAF, 1, 9]], &mut out).unwrap();
        // 1 バイトずつ入れても同じ結果
        let mut r = FlvReader::new();
        let mut tags = Vec::new();
        for b in &out {
            r.feed(&[*b]);
            while let Some(t) = r.next_tag().unwrap() {
                tags.push(t);
            }
        }
        assert!(r.header().unwrap().has_audio);
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].timestamp, 0x1234_5678);
        assert_eq!(&tags[0].body[..], &[0x17, 0, 1, 2, 3]);
        assert_eq!(tags[1].tag_type, TagType::Audio);
        assert_eq!(r.buffered(), 0);
    }

    #[test]
    fn rejects_garbage() {
        let mut r = FlvReader::new();
        r.feed(b"NOTFLV\0\0\0\x09");
        assert!(r.next_tag().is_err());
        assert!(r.next_tag().is_err());

        let mut out = Vec::new();
        write_header(
            FlvHeader {
                has_audio: false,
                has_video: true,
            },
            &mut out,
        );
        write_tag(TagType::Video, 0, &[&[1, 2]], &mut out).unwrap();
        let n = out.len();
        out[n - 1] ^= 1; // PreviousTagSize を壊す
        let mut r = FlvReader::new();
        r.feed(&out);
        assert!(r.next_tag().is_err());
    }
}
