//! Annex B（開始符号区切り）と長さ前置（AVCC/HVCC）の NAL 列の分割・変換。
//!
//! NAL ヘッダーの中身は見ないので H.264 と H.265 で同じ処理になる。

use crate::error::{Error, Result};
use bytes::Bytes;

/// `00 00 01` の位置を `from` から探す。
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 2 < data.len() {
        let b = data[i + 2];
        if b > 1 {
            i += 3;
        } else if b == 1 && data[i] == 0 && data[i + 1] == 0 {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

/// Annex B のバイト列から NAL ユニット（開始符号を除く）を順に返す。
///
/// 3 バイト・4 バイトの開始符号のどちらも受け付ける。最初の開始符号より前のバイト列と、
/// NAL の後ろに付いた 0（trailing_zero_8bits・次の開始符号の zero_byte）は捨てる。空の NAL は返さない。
#[derive(Debug, Clone)]
pub struct AnnexBNals<'a> {
    data: &'a [u8],
    next_start: Option<usize>,
}

/// [`AnnexBNals`] を作る。
pub fn annexb_nals(data: &[u8]) -> AnnexBNals<'_> {
    AnnexBNals {
        data,
        next_start: find_start_code(data, 0).map(|i| i + 3),
    }
}

impl<'a> Iterator for AnnexBNals<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            let start = self.next_start?;
            let (end, next) = match find_start_code(self.data, start) {
                Some(i) => (i, Some(i + 3)),
                None => (self.data.len(), None),
            };
            self.next_start = next;
            let mut nal = &self.data[start..end];
            while let [rest @ .., 0] = nal {
                nal = rest;
            }
            if !nal.is_empty() {
                return Some(nal);
            }
        }
    }
}

/// Annex B のバイト列を NAL ごとの [`Bytes`] に分ける（コピーしない）。
pub fn annexb_nals_bytes(data: &Bytes) -> Vec<Bytes> {
    annexb_nals(data).map(|n| data.slice_ref(n)).collect()
}

/// 長さ前置の NAL 列を順に返す。長さが足りないときは `Err` を一度返して終わる。長さ 0 の NAL は飛ばす。
#[derive(Debug, Clone)]
pub struct LengthPrefixedNals<'a> {
    data: &'a [u8],
    pos: usize,
    length_size: usize,
}

/// [`LengthPrefixedNals`] を作る。`length_size` は 1・2・4 のどれか。
pub fn length_prefixed_nals(data: &[u8], length_size: u8) -> Result<LengthPrefixedNals<'_>> {
    if !matches!(length_size, 1 | 2 | 4) {
        return Err(Error::Invalid("NAL 長さのバイト数は 1・2・4 のどれか"));
    }
    Ok(LengthPrefixedNals {
        data,
        pos: 0,
        length_size: length_size as usize,
    })
}

impl<'a> Iterator for LengthPrefixedNals<'a> {
    type Item = Result<&'a [u8]>;

    fn next(&mut self) -> Option<Result<&'a [u8]>> {
        loop {
            if self.pos >= self.data.len() {
                return None;
            }
            let rest = &self.data[self.pos..];
            if rest.len() < self.length_size {
                self.pos = self.data.len();
                return Some(Err(Error::Truncated));
            }
            let len = rest[..self.length_size]
                .iter()
                .fold(0usize, |acc, &b| (acc << 8) | b as usize);
            let body = &rest[self.length_size..];
            if len > body.len() {
                self.pos = self.data.len();
                return Some(Err(Error::Truncated));
            }
            self.pos += self.length_size + len;
            if len > 0 {
                return Some(Ok(&body[..len]));
            }
        }
    }
}

/// 長さ前置の NAL 列を NAL ごとの [`Bytes`] に分ける（コピーしない）。
pub fn length_prefixed_nals_bytes(data: &Bytes, length_size: u8) -> Result<Vec<Bytes>> {
    length_prefixed_nals(data, length_size)?
        .map(|n| n.map(|n| data.slice_ref(n)))
        .collect()
}

/// NAL の列を 4 バイト長さ前置で `out` の後ろに書く。
pub fn write_length_prefixed<'a, I>(nals: I, out: &mut Vec<u8>) -> Result<()>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    for nal in nals {
        let len = u32::try_from(nal.len()).map_err(|_| Error::Invalid("NAL が 4 GiB を超える"))?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(nal);
    }
    Ok(())
}

/// Annex B を 4 バイト長さ前置（ADR-0003 の内部表現）に変換する。
pub fn annexb_to_length_prefixed(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() + 16);
    write_length_prefixed(annexb_nals(data), &mut out)?;
    Ok(out)
}

/// 長さ前置を Annex B（4 バイト開始符号）に変換する。
pub fn length_prefixed_to_annexb(data: &[u8], length_size: u8) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() + 16);
    for nal in length_prefixed_nals(data, length_size)? {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_mixed_start_codes() {
        let d = [
            0xAA, 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 0, 0, 3, 1, 0, 0,
        ];
        let nals: Vec<_> = annexb_nals(&d).collect();
        assert_eq!(
            nals,
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 0, 0, 3, 1][..]]
        );
    }

    #[test]
    fn split_degenerate() {
        assert_eq!(annexb_nals(&[]).count(), 0);
        assert_eq!(annexb_nals(&[0, 0]).count(), 0);
        assert_eq!(annexb_nals(&[1, 2, 3, 4]).count(), 0);
        assert_eq!(annexb_nals(&[0, 0, 1]).count(), 0);
        assert_eq!(annexb_nals(&[0, 0, 1, 0, 0, 1, 0, 0, 0, 1]).count(), 0);
        assert_eq!(
            annexb_nals(&[0, 0, 1, 9]).collect::<Vec<_>>(),
            vec![&[9u8][..]]
        );
    }

    #[test]
    fn bytes_variant_shares_buffer() {
        let b = Bytes::from_static(&[0, 0, 1, 0x67, 5, 0, 0, 1, 0x68, 6]);
        let v = annexb_nals_bytes(&b);
        assert_eq!(v.len(), 2);
        assert_eq!(&v[1][..], &[0x68, 6]);
        assert_eq!(v[0].as_ptr(), b[3..].as_ptr());
    }

    #[test]
    fn roundtrip_length_prefixed() {
        let d = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3];
        let lp = annexb_to_length_prefixed(&d).unwrap();
        assert_eq!(lp, vec![0, 0, 0, 3, 0x67, 1, 2, 0, 0, 0, 2, 0x68, 3]);
        let back = length_prefixed_to_annexb(&lp, 4).unwrap();
        assert_eq!(back, vec![0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3]);
    }

    #[test]
    fn length_prefixed_sizes_and_errors() {
        let one = [2, 0xAA, 0xBB, 0, 1, 0xCC];
        let v: Vec<_> = length_prefixed_nals(&one, 1)
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(v, vec![&[0xAA, 0xBB][..], &[0xCC][..]]);
        let two = [0, 1, 0xAA];
        assert_eq!(length_prefixed_nals(&two, 2).unwrap().count(), 1);
        assert!(length_prefixed_nals(&two, 3).is_err());
        assert!(length_prefixed_nals(&two, 0).is_err());
        // 長さが足りない
        let bad = [0, 0, 0, 9, 1, 2];
        let r: Vec<_> = length_prefixed_nals(&bad, 4).unwrap().collect();
        assert_eq!(r, vec![Err(Error::Truncated)]);
        // 長さフィールドの途中で終わる
        let bad = [0, 0, 0, 1, 7, 0, 0];
        let r: Vec<_> = length_prefixed_nals(&bad, 4).unwrap().collect();
        assert_eq!(r, vec![Ok(&[7u8][..]), Err(Error::Truncated)]);
        assert!(length_prefixed_to_annexb(&bad, 4).is_err());
        let b = Bytes::from_static(&[0, 0, 0, 1, 7]);
        assert_eq!(length_prefixed_nals_bytes(&b, 4).unwrap().len(), 1);
    }
}
