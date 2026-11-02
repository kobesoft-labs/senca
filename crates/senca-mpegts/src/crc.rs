//! MPEG-2 の CRC32（多項式 0x04C11DB7、初期値 0xFFFFFFFF、反転なし、最終 XOR なし）。

const TABLE: [u32; 256] = make_table();

const fn make_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u32) << 24;
        let mut j = 0;
        while j < 8 {
            c = if c & 0x8000_0000 != 0 { (c << 1) ^ 0x04C1_1DB7 } else { c << 1 };
            j += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

/// PSI セクションの CRC32。CRC を含めたセクション全体にかけると 0 になる。
pub fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = (crc << 8) ^ TABLE[((crc >> 24) as u8 ^ b) as usize];
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_value() {
        assert_eq!(crc32_mpeg2(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn residue_is_zero() {
        let mut v = b"hello section".to_vec();
        let c = crc32_mpeg2(&v);
        v.extend_from_slice(&c.to_be_bytes());
        assert_eq!(crc32_mpeg2(&v), 0);
    }
}
