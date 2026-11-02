//! 検査値: CRC-32（PNG・ISO 3309）と Adler-32（zlib）。

const fn make_crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[n] = c;
        n += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = make_crc_table();

/// CRC-32 を続きから更新する（初期値 0、最終の反転込み）。
pub fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

/// CRC-32。
pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0, data)
}

/// Adler-32 を続きから更新する（初期値 1）。
pub fn adler32_update(adler: u32, data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let (mut a, mut b) = (adler & 0xFFFF, adler >> 16);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

/// Adler-32。
pub fn adler32(data: &[u8]) -> u32 {
    adler32_update(1, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32_update(crc32(b"1234"), b"56789"), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        assert_eq!(adler32(b""), 1);
        let big = vec![0xFFu8; 100_000];
        let mut a = 1;
        for c in big.chunks(777) {
            a = adler32_update(a, c);
        }
        assert_eq!(a, adler32(&big));
    }
}
