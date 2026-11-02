//! OBU（AV1 仕様 5.3）のヘッダー、leb128、Low Overhead Bitstream Format の分割と Temporal Delimiter の除去。

use crate::error::{Error, Result};
use bytes::Bytes;

/// obu_type。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObuType {
    SequenceHeader,
    TemporalDelimiter,
    FrameHeader,
    TileGroup,
    Metadata,
    Frame,
    RedundantFrameHeader,
    TileList,
    Padding,
    /// 予約値（0・9〜14）。
    Reserved(u8),
}

impl ObuType {
    pub fn from_u8(v: u8) -> Self {
        match v & 0x0f {
            1 => Self::SequenceHeader,
            2 => Self::TemporalDelimiter,
            3 => Self::FrameHeader,
            4 => Self::TileGroup,
            5 => Self::Metadata,
            6 => Self::Frame,
            7 => Self::RedundantFrameHeader,
            8 => Self::TileList,
            15 => Self::Padding,
            v => Self::Reserved(v),
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::SequenceHeader => 1,
            Self::TemporalDelimiter => 2,
            Self::FrameHeader => 3,
            Self::TileGroup => 4,
            Self::Metadata => 5,
            Self::Frame => 6,
            Self::RedundantFrameHeader => 7,
            Self::TileList => 8,
            Self::Padding => 15,
            Self::Reserved(v) => v & 0x0f,
        }
    }
}

/// OBU ヘッダー。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObuHeader {
    pub obu_type: ObuType,
    pub has_size_field: bool,
    /// obu_extension_flag が 1 のとき (temporal_id, spatial_id)。
    pub extension: Option<(u8, u8)>,
}

impl ObuHeader {
    /// 先頭の 1〜2 バイトを読む。戻り値の 2 番目はヘッダーの長さ。
    pub fn parse(data: &[u8]) -> Result<(ObuHeader, usize)> {
        let b = *data.first().ok_or(Error::Truncated)?;
        if b & 0x80 != 0 {
            return Err(Error::Invalid("obu_forbidden_bit"));
        }
        let ext_flag = b & 0x04 != 0;
        let extension = if ext_flag {
            let e = *data.get(1).ok_or(Error::Truncated)?;
            Some((e >> 5, (e >> 3) & 3))
        } else {
            None
        };
        Ok((
            ObuHeader {
                obu_type: ObuType::from_u8(b >> 3),
                has_size_field: b & 0x02 != 0,
                extension,
            },
            if ext_flag { 2 } else { 1 },
        ))
    }

    /// ヘッダーのバイト列を `out` に書く。
    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(
            (self.obu_type.as_u8() << 3)
                | ((self.extension.is_some() as u8) << 2)
                | ((self.has_size_field as u8) << 1),
        );
        if let Some((t, s)) = self.extension {
            out.push(((t & 7) << 5) | ((s & 3) << 3));
        }
    }
}

/// leb128() を読む（4.10.5）。戻り値は（値, バイト数）。8 バイトを超える・値が 2^32 - 1 を超えるものは `Err`。
pub fn read_leb128(data: &[u8]) -> Result<(u32, usize)> {
    let mut value = 0u64;
    for i in 0..8 {
        let b = *data.get(i).ok_or(Error::Truncated)?;
        value |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            let v =
                u32::try_from(value).map_err(|_| Error::Invalid("leb128 が 2^32 - 1 を超える"))?;
            return Ok((v, i + 1));
        }
    }
    Err(Error::Invalid("leb128 が 8 バイトを超える"))
}

/// leb128 を書く（最短の形）。
pub fn write_leb128(mut v: u32, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// 1 つの OBU。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obu<'a> {
    pub header: ObuHeader,
    /// obu_size の後ろのペイロード。
    pub payload: &'a [u8],
    /// ヘッダーからペイロードの終わりまで。
    pub raw: &'a [u8],
}

/// Low Overhead Bitstream Format の OBU 列を順に返す。obu_has_size_field が 0 の OBU は残り全部をペイロードとする。
#[derive(Debug, Clone)]
pub struct Obus<'a> {
    data: &'a [u8],
    pos: usize,
}

/// [`Obus`] を作る。
pub fn obus(data: &[u8]) -> Obus<'_> {
    Obus { data, pos: 0 }
}

impl<'a> Obus<'a> {
    fn next_obu(&mut self) -> Result<Obu<'a>> {
        let rest = &self.data[self.pos..];
        let (header, hlen) = ObuHeader::parse(rest)?;
        let (size, slen) = if header.has_size_field {
            let (s, n) = read_leb128(&rest[hlen..])?;
            (s as usize, n)
        } else {
            (rest.len() - hlen, 0)
        };
        let start = hlen + slen;
        let end = start.checked_add(size).ok_or(Error::Truncated)?;
        if end > rest.len() {
            return Err(Error::Truncated);
        }
        self.pos += end;
        Ok(Obu {
            header,
            payload: &rest[start..end],
            raw: &rest[..end],
        })
    }
}

impl<'a> Iterator for Obus<'a> {
    type Item = Result<Obu<'a>>;

    fn next(&mut self) -> Option<Result<Obu<'a>>> {
        if self.pos >= self.data.len() {
            return None;
        }
        let r = self.next_obu();
        if r.is_err() {
            self.pos = self.data.len();
        }
        Some(r)
    }
}

/// Temporal Delimiter を取り除き、すべての OBU に obu_size を付けた OBU 列にする（ADR-0003 の内部表現）。
///
/// Temporal Delimiter が先頭にだけあり、他の OBU がすべて obu_size を持つときは切り出すだけ（コピーしない）。
pub fn strip_temporal_delimiters(tu: &Bytes) -> Result<Bytes> {
    let mut start = 0usize;
    let mut leading = true;
    let mut simple = true;
    for obu in obus(tu) {
        let obu = obu?;
        let is_td = obu.header.obu_type == ObuType::TemporalDelimiter;
        if is_td && leading {
            start += obu.raw.len();
            continue;
        }
        leading = false;
        if is_td || !obu.header.has_size_field {
            simple = false;
        }
    }
    if simple {
        return Ok(tu.slice(start..));
    }
    let mut out = Vec::with_capacity(tu.len() + 8);
    for obu in obus(tu) {
        let obu = obu?;
        if obu.header.obu_type == ObuType::TemporalDelimiter {
            continue;
        }
        write_obu(
            obu.header.obu_type,
            obu.header.extension,
            obu.payload,
            &mut out,
        )?;
    }
    Ok(Bytes::from(out))
}

/// obu_size 付きの OBU を書く。
pub fn write_obu(
    obu_type: ObuType,
    extension: Option<(u8, u8)>,
    payload: &[u8],
    out: &mut Vec<u8>,
) -> Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| Error::Invalid("OBU が 4 GiB を超える"))?;
    ObuHeader {
        obu_type,
        has_size_field: true,
        extension,
    }
    .write(out);
    write_leb128(len, out);
    out.extend_from_slice(payload);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb128_roundtrip() {
        for v in [0u32, 1, 127, 128, 300, 16383, 16384, u32::MAX] {
            let mut b = Vec::new();
            write_leb128(v, &mut b);
            assert_eq!(read_leb128(&b).unwrap(), (v, b.len()));
        }
        // 冗長な形（0x80 0x00）も読める
        assert_eq!(read_leb128(&[0x80, 0x00]).unwrap(), (0, 2));
        assert_eq!(read_leb128(&[0x80]), Err(Error::Truncated));
        assert!(read_leb128(&[0xFF; 9]).is_err());
        assert!(read_leb128(&[0xFF, 0xFF, 0xFF, 0xFF, 0x7F]).is_err()); // 2^35 - 1
    }

    #[test]
    fn header() {
        let (h, n) = ObuHeader::parse(&[0x12, 0x00]).unwrap();
        assert_eq!(h.obu_type, ObuType::TemporalDelimiter);
        assert!(h.has_size_field);
        assert_eq!(n, 1);
        let (h, n) = ObuHeader::parse(&[0x36, 0b0110_1000]).unwrap();
        assert_eq!(h.obu_type, ObuType::Frame);
        assert_eq!(h.extension, Some((3, 1)));
        assert_eq!(n, 2);
        let mut b = Vec::new();
        h.write(&mut b);
        assert_eq!(b, vec![0x36, 0b0110_1000]);
        assert!(ObuHeader::parse(&[0x92]).is_err());
        assert_eq!(ObuHeader::parse(&[0x36]), Err(Error::Truncated));
        for v in 0..16 {
            assert_eq!(ObuType::from_u8(v).as_u8(), v);
        }
    }

    #[test]
    fn iterate_and_strip() {
        // TD, SH(2 バイト), Frame(3 バイト)
        let tu = Bytes::from_static(&[0x12, 0x00, 0x0A, 0x02, 0xAA, 0xBB, 0x32, 0x03, 1, 2, 3]);
        let v: Vec<_> = obus(&tu).collect::<Result<_>>().unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[1].payload, &[0xAA, 0xBB]);
        assert_eq!(v[2].header.obu_type, ObuType::Frame);
        let s = strip_temporal_delimiters(&tu).unwrap();
        assert_eq!(&s[..], &tu[2..]);
        assert_eq!(s.as_ptr(), tu[2..].as_ptr());

        // 最後の OBU に obu_size がない → 付け直す
        let tu = Bytes::from_static(&[0x12, 0x00, 0x30, 1, 2, 3]);
        let s = strip_temporal_delimiters(&tu).unwrap();
        assert_eq!(&s[..], &[0x32, 0x03, 1, 2, 3]);

        // 途中にある TD も取り除く
        let tu = Bytes::from_static(&[0x32, 0x01, 9, 0x12, 0x00, 0x32, 0x01, 8]);
        let s = strip_temporal_delimiters(&tu).unwrap();
        assert_eq!(&s[..], &[0x32, 0x01, 9, 0x32, 0x01, 8]);

        // 壊れたもの
        let bad = Bytes::from_static(&[0x12, 0x00, 0x32, 0x05, 1]);
        assert_eq!(strip_temporal_delimiters(&bad), Err(Error::Truncated));
        let r: Vec<_> = obus(&bad).collect();
        assert_eq!(r.len(), 2);
        assert!(r[1].is_err());
        assert!(strip_temporal_delimiters(&Bytes::from_static(&[0x92, 0x00])).is_err());
        assert_eq!(strip_temporal_delimiters(&Bytes::new()).unwrap().len(), 0);
    }
}
