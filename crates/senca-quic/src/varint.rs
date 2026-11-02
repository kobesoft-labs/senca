//! QUIC の可変長整数（RFC 9000 16 章）。2 bit の長さ前置で 1・2・4・8 バイト、62 bit まで。
//!
//! HTTP/3・QPACK・WebTransport もこの符号化を使う（MoQ draft-22 は別の符号化なので使わない）。

/// 表せる最大値（2^62 - 1）。
pub const MAX: u64 = (1 << 62) - 1;

/// 符号化したときのバイト数。`v` は [`MAX`] 以下であること。
pub const fn encoded_len(v: u64) -> usize {
    if v < 1 << 6 {
        1
    } else if v < 1 << 14 {
        2
    } else if v < 1 << 30 {
        4
    } else {
        8
    }
}

/// 最短の形で書く。`v` が [`MAX`] を超えるときは [`MAX`] に丸める（呼び出し側で検査済みの値だけを渡す）。
pub fn write(out: &mut Vec<u8>, v: u64) {
    let v = v.min(MAX);
    match encoded_len(v) {
        1 => out.push(v as u8),
        2 => out.extend_from_slice(&((v as u16) | 0x4000).to_be_bytes()),
        4 => out.extend_from_slice(&((v as u32) | 0x8000_0000).to_be_bytes()),
        _ => out.extend_from_slice(&(v | 0xc000_0000_0000_0000).to_be_bytes()),
    }
}

/// 長さを指定して書く（後から埋める長さ欄など）。`len` で表せない値は `false`。
pub fn write_with_len(out: &mut Vec<u8>, v: u64, len: usize) -> bool {
    match len {
        1 if v < 1 << 6 => out.push(v as u8),
        2 if v < 1 << 14 => out.extend_from_slice(&((v as u16) | 0x4000).to_be_bytes()),
        4 if v < 1 << 30 => out.extend_from_slice(&((v as u32) | 0x8000_0000).to_be_bytes()),
        8 if v <= MAX => out.extend_from_slice(&(v | 0xc000_0000_0000_0000).to_be_bytes()),
        _ => return false,
    }
    true
}

/// 先頭から一つ読む。`(値, 消費したバイト数)`。足りなければ `None`。
pub fn read(buf: &[u8]) -> Option<(u64, usize)> {
    let first = *buf.first()?;
    let len = 1usize << (first >> 6);
    if buf.len() < len {
        return None;
    }
    let mut v = u64::from(first & 0x3f);
    for b in &buf[1..len] {
        v = (v << 8) | u64::from(*b);
    }
    Some((v, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc9000_examples() {
        // RFC 9000 A.1
        let cases: [(&[u8], u64); 4] = [
            (&[0xc2, 0x19, 0x7c, 0x5e, 0xff, 0x14, 0xe8, 0x8c], 151_288_809_941_952_652),
            (&[0x9d, 0x7f, 0x3e, 0x7d], 494_878_333),
            (&[0x7b, 0xbd], 15_293),
            (&[0x25], 37),
        ];
        for (b, v) in cases {
            assert_eq!(read(b), Some((v, b.len())));
            let mut o = Vec::new();
            write(&mut o, v);
            assert_eq!(o, b);
        }
        assert_eq!(read(&[0x40, 0x25]), Some((37, 2)));
        assert_eq!(read(&[0x40]), None);
        assert_eq!(read(&[]), None);
    }
}
