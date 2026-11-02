//! TLS 1.2 の PRF（RFC 5246 5）。HMAC は RustCrypto の `hmac` を使う。

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256, Sha384};

/// PRF に使うハッシュ。cipher suite で決まる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrfHash {
    /// P_SHA256。
    Sha256,
    /// P_SHA384。
    Sha384,
}

impl PrfHash {
    /// ハッシュの出力長。
    pub fn output_len(self) -> usize {
        match self {
            Self::Sha256 => 32,
            Self::Sha384 => 48,
        }
    }

    /// データ全体のハッシュ（handshake の transcript など）。
    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha256 => Sha256::digest(data).to_vec(),
            Self::Sha384 => Sha384::digest(data).to_vec(),
        }
    }
}

fn p_hash<M: Mac + hmac::digest::KeyInit + Clone>(
    secret: &[u8],
    label: &[u8],
    seed: &[u8],
    out: &mut [u8],
) {
    let base =
        <M as hmac::digest::KeyInit>::new_from_slice(secret).expect("HMAC は任意長の鍵を受ける");
    // A(1) = HMAC(secret, label + seed)
    let mut a = {
        let mut m = base.clone();
        m.update(label);
        m.update(seed);
        m.finalize().into_bytes()
    };
    let mut pos = 0;
    while pos < out.len() {
        let mut m = base.clone();
        m.update(&a);
        m.update(label);
        m.update(seed);
        let block = m.finalize().into_bytes();
        let n = (out.len() - pos).min(block.len());
        out[pos..pos + n].copy_from_slice(&block[..n]);
        pos += n;
        let mut m = base.clone();
        m.update(&a);
        a = m.finalize().into_bytes();
    }
}

/// PRF(secret, label, seed) を `out` の長さだけ出す。
pub fn prf(hash: PrfHash, secret: &[u8], label: &[u8], seed: &[u8], out: &mut [u8]) {
    match hash {
        PrfHash::Sha256 => p_hash::<Hmac<Sha256>>(secret, label, seed, out),
        PrfHash::Sha384 => p_hash::<Hmac<Sha384>>(secret, label, seed, out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // 期待値は OpenSSL 3.5 の `openssl kdf ... TLS1-PRF` で独立に求めた。
    #[test]
    fn prf_sha256_vector() {
        let mut out = [0u8; 100];
        prf(
            PrfHash::Sha256,
            &hex("9bbe436ba940f017b17652849a71db35"),
            b"test label",
            &hex("a0ba9f936cda311827a6f796ffd5198c"),
            &mut out,
        );
        assert_eq!(
            out[..],
            hex(
                "E3F229BA727BE17B8D122620557CD453C2AAB21D07C3D495329B52D4E61EDB5A6B301791E90D35C9C9A46B4E14BAF9AF0FA022F7077DEF17ABFD3797C0564BAB4FBC91666E9DEF9B97FCE34F796789BAA48082D122EE42C5A72E5A5110FFF70187347B66"
            )[..]
        );
    }

    #[test]
    fn prf_sha384_vector() {
        let mut out = [0u8; 100];
        prf(
            PrfHash::Sha384,
            &hex("9bbe436ba940f017b17652849a71db35"),
            b"test label",
            &hex("a0ba9f936cda311827a6f796ffd5198c"),
            &mut out,
        );
        assert_eq!(
            out[..],
            hex(
                "DD88775CD827187B67A3F7652B5C13F715791CC46E0274A6D3FB16651103DEFC544CD8AFB68369A219BB918B8B21DDB1764AF0A70339E6DEC085E574F655851BA692513203536BDFC3675E53768210F0A2389DD324311A440C7C30EF44B391D914C3B0C7"
            )[..]
        );
    }
}
