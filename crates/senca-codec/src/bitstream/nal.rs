//! NAL ユニットの区切り（Annex B の開始符号 ⇔ 長さ前置）と、エミュレーション防止バイトの除去。
//!
//! ハードウェアのエンコーダーは Annex B を出し、デコーダーは Annex B を受けるものが多い。
//! 内部形式（ADR-0003）は 4 バイト長さ前置なので、ここで相互に変換する。

use crate::error::{CodecError, Result};

/// 1 つの AU に入れてよい NAL の数の上限。
pub const MAX_NALS_PER_AU: usize = 4096;

/// Annex B のバイト列を NAL ごとに分ける（開始符号は 3・4 バイトのどちらも可。末尾の 0 は捨てる）。
/// 開始符号で始まらない先頭部分は捨てずに 1 つの NAL として扱う（エンコーダーの出力は必ず開始符号で始まる）。
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    if starts.is_empty() {
        let t = trim_trailing_zeros(data);
        if !t.is_empty() {
            out.push(t);
        }
        return out;
    }
    if starts[0].0 > 0 {
        let head = trim_trailing_zeros(&data[..starts[0].0]);
        if !head.is_empty() {
            out.push(head);
        }
    }
    for (k, &(_, body)) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(data.len(), |n| n.0);
        let nal = trim_trailing_zeros(&data[body..end]);
        if !nal.is_empty() {
            out.push(nal);
        }
    }
    out
}

fn trim_trailing_zeros(mut d: &[u8]) -> &[u8] {
    while let [rest @ .., 0] = d {
        d = rest;
    }
    d
}

/// NAL の列を 4 バイト長さ前置にして連結する。
pub fn to_length_prefixed<'a>(nals: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    for n in nals {
        out.extend_from_slice(&(n.len() as u32).to_be_bytes());
        out.extend_from_slice(n);
    }
    out
}

/// 長さ前置（`len_size` = 1, 2, 4 バイト）の列を NAL ごとに分ける。壊れていれば `Err`。
pub fn split_length_prefixed(data: &[u8], len_size: usize) -> Result<Vec<&[u8]>> {
    if !matches!(len_size, 1 | 2 | 4) {
        return Err(CodecError::invalid_input("bad NAL length size"));
    }
    let mut out = Vec::new();
    let mut p = 0;
    while p < data.len() {
        if data.len() - p < len_size {
            return Err(CodecError::invalid_input("truncated NAL length"));
        }
        let mut n = 0usize;
        for b in &data[p..p + len_size] {
            n = (n << 8) | *b as usize;
        }
        p += len_size;
        if n == 0 || n > data.len() - p {
            return Err(CodecError::invalid_input("bad NAL length"));
        }
        out.push(&data[p..p + n]);
        p += n;
        if out.len() > MAX_NALS_PER_AU {
            return Err(CodecError::invalid_input("too many NAL units"));
        }
    }
    Ok(out)
}

/// 長さ前置を Annex B（4 バイトの開始符号）にする。
pub fn length_prefixed_to_annexb(data: &[u8], len_size: usize) -> Result<Vec<u8>> {
    let nals = split_length_prefixed(data, len_size)?;
    let mut out = Vec::with_capacity(data.len() + nals.len() * 4);
    for n in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(n);
    }
    Ok(out)
}

/// NAL の列を Annex B（4 バイトの開始符号）にする。
pub fn nals_to_annexb<'a>(nals: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    for n in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(n);
    }
    out
}

/// エミュレーション防止バイト（00 00 03 の 03）を取り除いた RBSP を返す。
pub fn unescape_rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annexb_split() {
        let d = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 0,
        ];
        let n = split_annexb(&d);
        assert_eq!(n, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9][..]]);
        let lp = to_length_prefixed(n.iter().copied());
        assert_eq!(&lp[..7], &[0, 0, 0, 3, 0x67, 1, 2]);
        let back = split_length_prefixed(&lp, 4).unwrap();
        assert_eq!(back.len(), 3);
        let ab = length_prefixed_to_annexb(&lp, 4).unwrap();
        assert_eq!(split_annexb(&ab), back);
        assert!(split_annexb(&[]).is_empty());
        assert_eq!(split_annexb(&[0x65, 1]), vec![&[0x65, 1][..]]);
    }

    #[test]
    fn length_prefixed_rejects() {
        assert!(split_length_prefixed(&[0, 0, 0, 5, 1], 4).is_err());
        assert!(split_length_prefixed(&[0, 0, 0], 4).is_err());
        assert!(split_length_prefixed(&[0, 0, 0, 0], 4).is_err());
        assert!(split_length_prefixed(&[1, 1], 3).is_err());
        assert_eq!(
            split_length_prefixed(&[1, 7, 1, 8], 1).unwrap(),
            vec![&[7][..], &[8][..]]
        );
    }

    #[test]
    fn unescape() {
        assert_eq!(unescape_rbsp(&[0, 0, 3, 1, 0, 0, 3]), vec![0, 0, 1, 0, 0]);
        assert_eq!(unescape_rbsp(&[0, 3, 0]), vec![0, 3, 0]);
    }
}
