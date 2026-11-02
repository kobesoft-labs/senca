//! CRC32c（Castagnoli、RFC 9260 付録 B）。
//!
//! 暗号の部品ではないので自前で持つ（表引き、反転多項式 0x82F63B78）。
//! SCTP のパケットでは、checksum 欄を 0 にして計算し、結果を little-endian で書く。

const POLY: u32 = 0x82F6_3B78;

const fn make_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static TABLE: [u32; 256] = make_table();

/// 途中の値（反転したまま）を更新する。
fn update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc = TABLE[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

/// バイト列の CRC32c。
pub fn crc32c(data: &[u8]) -> u32 {
    !update(!0, data)
}

/// SCTP パケットの checksum（offset 8..12 を 0 とみなして計算する）。`packet` は 12 byte 以上。
pub(crate) fn packet_checksum(packet: &[u8]) -> u32 {
    let mut c = update(!0, &packet[..8]);
    c = update(c, &[0, 0, 0, 0]);
    c = update(c, &packet[12..]);
    !c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn rfc3720_b4_vectors() {
        // RFC 3720 B.4 は CRC を little-endian のバイト列で示している（aa 36 91 8a など）。
        assert_eq!(crc32c(&[0u8; 32]).to_le_bytes(), [0xaa, 0x36, 0x91, 0x8a]);
        assert_eq!(
            crc32c(&[0xffu8; 32]).to_le_bytes(),
            [0x43, 0xab, 0xa8, 0x62]
        );
        let inc: Vec<u8> = (0u8..32).collect();
        assert_eq!(crc32c(&inc).to_le_bytes(), [0x4e, 0x79, 0xdd, 0x46]);
        let dec: Vec<u8> = (0u8..32).rev().collect();
        assert_eq!(crc32c(&dec).to_le_bytes(), [0x5c, 0xdb, 0x3f, 0x11]);
    }

    #[test]
    fn packet_checksum_ignores_field() {
        let mut p = vec![0u8; 32];
        for (i, b) in p.iter_mut().enumerate() {
            *b = i as u8;
        }
        let c = packet_checksum(&p);
        p[8..12].copy_from_slice(&[0, 0, 0, 0]);
        assert_eq!(c, crc32c(&p));
    }
}
