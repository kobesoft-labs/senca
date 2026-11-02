//! HPACK/QPACK の Huffman 符号（RFC 7541 5.2・付録 B）。

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::huffman_table::CODES;

/// (ビット長, 符号) → 記号。
fn table() -> &'static HashMap<(u8, u32), u16> {
    static T: OnceLock<HashMap<(u8, u32), u16>> = OnceLock::new();
    T.get_or_init(|| CODES.iter().enumerate().map(|(s, &(c, l))| ((l, c), s as u16)).collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HuffmanError;

/// 復号する。出力は `max` バイトまで（超えたら誤り）。
pub(crate) fn decode(input: &[u8], max: usize) -> Result<Vec<u8>, HuffmanError> {
    let t = table();
    let mut out = Vec::with_capacity(input.len() * 8 / 5);
    let mut code: u32 = 0;
    let mut len: u8 = 0;
    for &byte in input {
        for i in (0..8).rev() {
            code = (code << 1) | u32::from((byte >> i) & 1);
            len += 1;
            if len >= 5
                && let Some(&sym) = t.get(&(len, code))
            {
                if sym == 256 {
                    return Err(HuffmanError); // EOS は文字列に入れてはならない
                }
                if out.len() >= max {
                    return Err(HuffmanError);
                }
                out.push(sym as u8);
                code = 0;
                len = 0;
            } else if len > 30 {
                return Err(HuffmanError);
            }
        }
    }
    // 残りは 7 bit 以下で、EOS の先頭（すべて 1）でなければならない
    if len > 7 || code != (1u32 << len) - 1 {
        return Err(HuffmanError);
    }
    Ok(out)
}

/// 符号化する（試験用。送信では Huffman を使わない）。
#[cfg(test)]
pub(crate) fn encode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut acc: u64 = 0;
    let mut bits = 0u32;
    for &b in input {
        let (c, l) = CODES[b as usize];
        acc = (acc << l) | u64::from(c);
        bits += u32::from(l);
        while bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if bits > 0 {
        out.push(((acc << (8 - bits)) as u8) | (0xff >> bits));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7541_examples() {
        // C.4.1: "www.example.com"
        let enc = [0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0xff];
        assert_eq!(decode(&enc, 100).unwrap(), b"www.example.com");
        assert_eq!(encode(b"www.example.com"), enc);
        // C.4.2: "no-cache"
        assert_eq!(decode(&[0xa8, 0xeb, 0x10, 0x64, 0x9c, 0xbf], 100).unwrap(), b"no-cache");
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&all), 1000).unwrap(), all);
        // 不正な padding（0 で埋めた）
        assert!(decode(&[0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0x00], 100).is_err());
        // 8 bit 以上の padding
        assert!(decode(&[0xff, 0xff], 100).is_err());
        assert!(decode(&encode(b"abc"), 2).is_err());
    }
}
