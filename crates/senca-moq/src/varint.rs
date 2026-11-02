//! draft-22 の可変長整数（8.1）。最初のバイトの先頭の 1 の数で長さ（1〜9 バイト）を表す。
//! 64 bit 全域を表せる。受信では最短でない形も受け付ける（draft-22 で許される）。QUIC の varint とは別物。

/// 符号化したときの長さ（最短）。
pub const fn encoded_len(v: u64) -> usize {
    if v < 1 << 7 {
        1
    } else if v < 1 << 14 {
        2
    } else if v < 1 << 21 {
        3
    } else if v < 1 << 28 {
        4
    } else if v < 1 << 35 {
        5
    } else if v < 1 << 42 {
        6
    } else if v < 1 << 49 {
        7
    } else if v < 1 << 56 {
        8
    } else {
        9
    }
}

/// 最短の形で書く。
pub fn write(out: &mut Vec<u8>, v: u64) {
    let n = encoded_len(v);
    if n == 9 {
        out.push(0xff);
        out.extend_from_slice(&v.to_be_bytes());
        return;
    }
    // 先頭 n-1 ビットを 1、続く 1 ビットを 0、残りに値
    let prefix: u8 = !(0xffu8 >> (n - 1));
    let bytes = v.to_be_bytes();
    let mut b = bytes[8 - n..].to_vec();
    b[0] = (b[0] & (0xffu16 >> n) as u8) | prefix;
    out.extend_from_slice(&b);
}

/// 先頭から一つ読む。`(値, 消費バイト数)`。
pub fn read(buf: &[u8]) -> Option<(u64, usize)> {
    let first = *buf.first()?;
    let n = (first.leading_ones() as usize) + 1;
    if n >= 9 {
        if buf.len() < 9 {
            return None;
        }
        let mut a = [0u8; 8];
        a.copy_from_slice(&buf[1..9]);
        return Some((u64::from_be_bytes(a), 9));
    }
    if buf.len() < n {
        return None;
    }
    let mut v = u64::from(first & (0xffu16 >> n) as u8);
    for &b in &buf[1..n] {
        v = (v << 8) | u64::from(b);
    }
    Some((v, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft22_examples() {
        // draft-22 8.1 表 4
        let cases: [(&[u8], u64); 8] = [
            (&[0x25], 37),
            (&[0x80, 0x25], 37),
            (&[0xbb, 0xbd], 15_293),
            (&[0xed, 0x7f, 0x3e, 0x7d], 226_442_877),
            (&[0xfa, 0xa1, 0xa0, 0xe4, 0x03, 0xd8], 2_893_212_287_960),
            (&[0xfc, 0x89, 0x98, 0xab, 0xc6, 0x6b, 0xc0], 151_288_809_941_952),
            (&[0xfe, 0xfa, 0x31, 0x8f, 0xa8, 0xe3, 0xca, 0x11], 70_423_237_261_249_041),
            (&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], u64::MAX),
        ];
        for (b, v) in cases {
            assert_eq!(read(b), Some((v, b.len())), "{v}");
        }
        for v in [0u64, 37, 127, 128, 15_293, 16_383, 16_384, 226_442_877, 2_893_212_287_960, 1 << 56, u64::MAX] {
            let mut o = Vec::new();
            write(&mut o, v);
            assert_eq!(o.len(), encoded_len(v));
            assert_eq!(read(&o), Some((v, o.len())), "{v}");
        }
        let mut o = Vec::new();
        write(&mut o, 15_293);
        assert_eq!(o, [0xbb, 0xbd]);
        assert_eq!(read(&[0xc0, 0x00]), None);
        assert_eq!(read(&[]), None);
        // どの先頭バイトでも panic しない
        for b in 0..=255u8 {
            let mut v = vec![b];
            v.extend_from_slice(&[0xff; 8]);
            assert!(read(&v).is_some());
        }
    }
}
