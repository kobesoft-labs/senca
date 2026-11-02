//! FINGERPRINT 用の CRC-32（ISO/IEC 13239・ITU-T V.42、多項式 0x04C11DB7 の反転形）。
//! 暗号の部品ではない（誤り検出だけ）ので自前で持つ。

const fn make_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static TABLE: [u32; 256] = make_table();

/// 複数の断片をつないだ列の CRC-32。
pub(crate) fn crc32(parts: &[&[u8]]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for p in parts {
        for &b in *p {
            c = TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
    }
    c ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    #[test]
    fn check_value() {
        assert_eq!(super::crc32(&[b"123456789"]), 0xCBF4_3926);
        assert_eq!(super::crc32(&[b"1234", b"56789"]), 0xCBF4_3926);
    }
}
