//! 最小の PNG 書き出し（白黒 1 bit のグレースケール、圧縮なしの zlib）。QR コードの画像用。
//!
//! 圧縮しない（stored ブロック）ので小さな画像向け。CRC-32 と Adler-32 は PNG・zlib の形式の一部で、
//! 暗号の部品ではない。

/// CRC-32（ISO 3309、PNG のチャンク用）。
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    b << 16 | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// 白黒の画像を PNG にする。`pixel(x, y)` が true の画素を黒にする。
pub fn encode_bilevel(width: u32, height: u32, pixel: impl Fn(u32, u32) -> bool) -> Vec<u8> {
    let row_bytes = width.div_ceil(8) as usize;
    let mut raw = Vec::with_capacity((row_bytes + 1) * height as usize);
    for y in 0..height {
        raw.push(0); // filter: None
        let start = raw.len();
        raw.resize(start + row_bytes, 0);
        for x in 0..width {
            // グレースケール 1 bit: 1 = 白
            if !pixel(x, y) {
                raw[start + (x / 8) as usize] |= 0x80 >> (x % 8);
            }
        }
    }
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65535).peekable();
    if blocks.peek().is_none() {
        z.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(b) = blocks.next() {
        z.push(u8::from(blocks.peek().is_none()));
        let len = b.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(b);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[1, 0, 0, 0, 0]); // bit depth 1, grayscale, deflate, filter 0, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_checksums() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn png_structure() {
        let p = encode_bilevel(3, 2, |x, y| (x + y) % 2 == 0);
        assert_eq!(&p[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&p[12..16], b"IHDR");
        assert_eq!(&p[p.len() - 8..p.len() - 4], b"IEND");
        // 生データ: 行ごとに filter 0 + 1 バイト（1 = 白）
        let idat_len = u32::from_be_bytes(p[33..37].try_into().unwrap()) as usize;
        let z = &p[41..41 + idat_len];
        assert_eq!(&z[..2], &[0x78, 0x01]);
        assert_eq!(&z[7..11], &[0, 0b0100_0000, 0, 0b1010_0000]);
    }
}
