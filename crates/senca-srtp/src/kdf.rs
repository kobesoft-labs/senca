//! 鍵導出（RFC 3711 4.3.1・4.3.3、RFC 6188 の AES_256_CM_PRF）。

use aes::cipher::{KeyIvInit, StreamCipher};
use aes::{Aes128, Aes256};

type Aes128Ctr = ctr::Ctr128BE<Aes128>;
type Aes256Ctr = ctr::Ctr128BE<Aes256>;

/// SRTP の label（RFC 3711 4.3.2）。
pub(crate) const LABEL_RTP_ENCRYPTION: u8 = 0x00;
pub(crate) const LABEL_RTP_AUTH: u8 = 0x01;
pub(crate) const LABEL_RTP_SALT: u8 = 0x02;
pub(crate) const LABEL_RTCP_ENCRYPTION: u8 = 0x03;
pub(crate) const LABEL_RTCP_AUTH: u8 = 0x04;
pub(crate) const LABEL_RTCP_SALT: u8 = 0x05;

/// AES-CM PRF で session key を導出する（key_derivation_rate = 0）。
///
/// x = (label || index DIV kdr) XOR master_salt（右詰め）、入力ブロック = x * 2^16。
/// master salt が 96 bit（GCM）の場合は右に 0 を 2 byte 補って 112 bit として扱う（libsrtp と同じ）。
/// master key は 16 byte（AES-128）か 32 byte（AES-256）。それ以外は `false`。
pub fn aes_cm_prf(master_key: &[u8], master_salt: &[u8], label: u8, out: &mut [u8]) -> bool {
    if master_salt.len() > 14 {
        return false;
    }
    let mut iv = [0u8; 16];
    iv[..master_salt.len()].copy_from_slice(master_salt);
    // label は 7 byte の key_id の先頭。112 bit の右詰めで byte 7 に来る。index DIV kdr は 0。
    iv[7] ^= label;
    out.fill(0);
    match master_key.len() {
        16 => match Aes128Ctr::new_from_slices(master_key, &iv) {
            Ok(mut c) => c.apply_keystream(out),
            Err(_) => return false,
        },
        32 => match Aes256Ctr::new_from_slices(master_key, &iv) {
            Ok(mut c) => c.apply_keystream(out),
            Err(_) => return false,
        },
        _ => return false,
    }
    true
}

/// AES-CM の keystream を `data` に XOR する（RFC 3711 4.1.1）。`iv` は 128 bit の初期カウンター。
pub(crate) fn aes_cm_xor(key: &[u8], iv: &[u8; 16], data: &mut [u8]) {
    match key.len() {
        16 => {
            if let Ok(mut c) = Aes128Ctr::new_from_slices(key, iv) {
                c.apply_keystream(data);
            }
        }
        32 => {
            if let Ok(mut c) = Aes256Ctr::new_from_slices(key, iv) {
                c.apply_keystream(data);
            }
        }
        // 鍵長は context の生成時に検査済みで、ここには来ない。
        _ => unreachable!("鍵長は生成時に検査済み"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// RFC 3711 B.2 AES-CM の keystream。
    #[test]
    fn rfc3711_b2_aes_cm_keystream() {
        let key = hex("2B7E151628AED2A6ABF7158809CF4F3C");
        let mut iv = [0u8; 16];
        iv.copy_from_slice(&hex("F0F1F2F3F4F5F6F7F8F9FAFBFCFD0000"));
        let mut ks = vec![0u8; 65282 * 16];
        aes_cm_xor(&key, &iv, &mut ks);
        assert_eq!(ks[0..16], hex("E03EAD0935C95E80E166B16DD92B4EB4")[..]);
        assert_eq!(ks[16..32], hex("D23513162B02D0F72A43A2FE4A5F97AB")[..]);
        assert_eq!(ks[32..48], hex("41E95B3BB0A2E8DD477901E4FCA894C0")[..]);
        let n = ks.len();
        assert_eq!(
            ks[n - 48..n - 32],
            hex("EC8CDF7398607CB0F2D21675EA9EA1E4")[..]
        );
        assert_eq!(
            ks[n - 32..n - 16],
            hex("362B7C3C6773516318A077D7FC5073AE")[..]
        );
        assert_eq!(ks[n - 16..], hex("6A2CC3787889374FBEB4C81B17BA6C44")[..]);
    }

    /// RFC 3711 B.3 鍵導出。
    #[test]
    fn rfc3711_b3_key_derivation() {
        let mk = hex("E1F97A0D3E018BE0D64FA32C06DE4139");
        let ms = hex("0EC675AD498AFEEBB6960B3AABE6");
        let mut ck = [0u8; 16];
        assert!(aes_cm_prf(&mk, &ms, LABEL_RTP_ENCRYPTION, &mut ck));
        assert_eq!(ck[..], hex("C61E7A93744F39EE10734AFE3FF7A087")[..]);
        let mut cs = [0u8; 14];
        assert!(aes_cm_prf(&mk, &ms, LABEL_RTP_SALT, &mut cs));
        assert_eq!(cs[..], hex("30CBBC08863D8C85D49DB34A9AE1")[..]);
        let mut ak = [0u8; 94];
        assert!(aes_cm_prf(&mk, &ms, LABEL_RTP_AUTH, &mut ak));
        let expect = hex(
            "CEBE321F6FF7716B6FD4AB49AF256A15 6D38BAA48F0A0ACF3C34E2359E6CDBCE
             E049646C43D9327AD175578EF7227098 6371C10C9A369AC2F94A8C5FBCDDDC25
             6D6E919A48B610EF17C2041E47403576 6B68642C59BBFC2F34DB60DBDFB2",
        );
        assert_eq!(ak[..], expect[..]);
    }

    #[test]
    fn rejects_bad_key_len() {
        let mut out = [0u8; 16];
        assert!(!aes_cm_prf(&[0u8; 15], &[0u8; 14], 0, &mut out));
        assert!(!aes_cm_prf(&[0u8; 16], &[0u8; 15], 0, &mut out));
    }
}
