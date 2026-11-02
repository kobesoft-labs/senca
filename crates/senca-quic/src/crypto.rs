//! パケット保護（RFC 9001 5 章）。AEAD と header protection の演算は RustCrypto に任せ、
//! ここでは nonce の作り方・sample の取り方・Retry の integrity tag など QUIC の取り決めだけを書く。

use aes::cipher::BlockEncrypt;
use aes::{Aes128, Aes256};
use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes128Gcm, Aes256Gcm};
use chacha20::ChaCha20;
use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use chacha20poly1305::ChaCha20Poly1305;
use hmac::{Hmac, Mac};
use senca_tls::quic::packet_keys;
use senca_tls::{CipherSuite, Secret};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::cid::{ConnectionId, ResetToken};

/// AEAD の tag 長（3 スイートとも 16）。
pub(crate) const TAG_LEN: usize = 16;
/// header protection の sample 長。
pub(crate) const SAMPLE_LEN: usize = 16;

enum Aead {
    A128(Box<Aes128Gcm>),
    A256(Box<Aes256Gcm>),
    ChaCha(Box<ChaCha20Poly1305>),
}

/// 一方向の AEAD 鍵と IV。
pub(crate) struct PacketKey {
    aead: Aead,
    iv: Zeroizing<[u8; 12]>,
}

impl std::fmt::Debug for PacketKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PacketKey(<秘密>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CryptoFailure;

impl PacketKey {
    fn new(suite: CipherSuite, key: &[u8], iv: &[u8]) -> Result<Self, CryptoFailure> {
        let aead = match suite {
            CipherSuite::Aes128GcmSha256 => Aead::A128(Box::new(Aes128Gcm::new_from_slice(key).map_err(|_| CryptoFailure)?)),
            CipherSuite::Aes256GcmSha384 => Aead::A256(Box::new(Aes256Gcm::new_from_slice(key).map_err(|_| CryptoFailure)?)),
            CipherSuite::Chacha20Poly1305Sha256 => {
                Aead::ChaCha(Box::new(ChaCha20Poly1305::new_from_slice(key).map_err(|_| CryptoFailure)?))
            }
        };
        let mut v = Zeroizing::new([0u8; 12]);
        if iv.len() != 12 {
            return Err(CryptoFailure);
        }
        v.copy_from_slice(iv);
        Ok(Self { aead, iv: v })
    }

    fn nonce(&self, pn: u64) -> [u8; 12] {
        let mut n = *self.iv;
        for (i, b) in pn.to_be_bytes().iter().enumerate() {
            n[4 + i] ^= b;
        }
        n
    }

    /// `payload` をその場で暗号化し、tag を返す。
    pub(crate) fn seal(&self, pn: u64, header: &[u8], payload: &mut [u8]) -> Result<[u8; TAG_LEN], CryptoFailure> {
        let n = self.nonce(pn);
        let nonce = &n.into();
        let tag = match &self.aead {
            Aead::A128(a) => a.encrypt_in_place_detached(nonce, header, payload),
            Aead::A256(a) => a.encrypt_in_place_detached(nonce, header, payload),
            Aead::ChaCha(a) => a.encrypt_in_place_detached(nonce, header, payload),
        }
        .map_err(|_| CryptoFailure)?;
        let mut t = [0u8; TAG_LEN];
        t.copy_from_slice(&tag);
        Ok(t)
    }

    /// `payload`（tag を除く）をその場で復号する。
    pub(crate) fn open(&self, pn: u64, header: &[u8], payload: &mut [u8], tag: &[u8]) -> Result<(), CryptoFailure> {
        if tag.len() != TAG_LEN {
            return Err(CryptoFailure);
        }
        let n = self.nonce(pn);
        let nonce = &n.into();
        let tag: [u8; TAG_LEN] = tag.try_into().map_err(|_| CryptoFailure)?;
        let tag = &tag.into();
        match &self.aead {
            Aead::A128(a) => a.decrypt_in_place_detached(nonce, header, payload, tag),
            Aead::A256(a) => a.decrypt_in_place_detached(nonce, header, payload, tag),
            Aead::ChaCha(a) => a.decrypt_in_place_detached(nonce, header, payload, tag),
        }
        .map_err(|_| CryptoFailure)
    }
}

/// header protection の鍵。
pub(crate) enum HeaderKey {
    A128(Box<Aes128>),
    A256(Box<Aes256>),
    ChaCha(Zeroizing<[u8; 32]>),
}

impl std::fmt::Debug for HeaderKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HeaderKey(<秘密>)")
    }
}

impl HeaderKey {
    fn new(suite: CipherSuite, hp: &[u8]) -> Result<Self, CryptoFailure> {
        Ok(match suite {
            CipherSuite::Aes128GcmSha256 => Self::A128(Box::new(Aes128::new_from_slice(hp).map_err(|_| CryptoFailure)?)),
            CipherSuite::Aes256GcmSha384 => Self::A256(Box::new(Aes256::new_from_slice(hp).map_err(|_| CryptoFailure)?)),
            CipherSuite::Chacha20Poly1305Sha256 => {
                let mut k = Zeroizing::new([0u8; 32]);
                if hp.len() != 32 {
                    return Err(CryptoFailure);
                }
                k.copy_from_slice(hp);
                Self::ChaCha(k)
            }
        })
    }

    /// 5 バイトの mask（RFC 9001 5.4.3・5.4.4）。
    pub(crate) fn mask(&self, sample: &[u8; SAMPLE_LEN]) -> [u8; 5] {
        let mut out = [0u8; 5];
        match self {
            Self::A128(c) => {
                let mut b = (*sample).into();
                c.encrypt_block(&mut b);
                out.copy_from_slice(&b[..5]);
            }
            Self::A256(c) => {
                let mut b = (*sample).into();
                c.encrypt_block(&mut b);
                out.copy_from_slice(&b[..5]);
            }
            Self::ChaCha(k) => {
                let counter = u32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]);
                let mut nonce = [0u8; 12];
                nonce.copy_from_slice(&sample[4..]);
                let mut c = ChaCha20::new(&(**k).into(), &nonce.into());
                c.seek(u64::from(counter) * 64);
                c.apply_keystream(&mut out);
            }
        }
        out
    }
}

impl PacketKey {
    /// secret から AEAD 鍵だけを作る（鍵更新では header protection の鍵は変わらない）。
    pub(crate) fn from_secret(suite: CipherSuite, secret: &Secret) -> Result<Self, CryptoFailure> {
        let m = packet_keys(suite, secret).map_err(|_| CryptoFailure)?;
        Self::new(suite, m.key.as_bytes(), m.iv.as_bytes())
    }
}

/// 一方向の鍵一式。
#[derive(Debug)]
pub(crate) struct DirKeys {
    pub packet: PacketKey,
    pub header: HeaderKey,
}

impl DirKeys {
    pub(crate) fn from_secret(suite: CipherSuite, secret: &Secret) -> Result<Self, CryptoFailure> {
        let m = packet_keys(suite, secret).map_err(|_| CryptoFailure)?;
        Ok(Self {
            packet: PacketKey::new(suite, m.key.as_bytes(), m.iv.as_bytes())?,
            header: HeaderKey::new(suite, m.hp.as_bytes())?,
        })
    }
}

/// 送受信の鍵一式（Initial・Handshake）。
#[derive(Debug)]
pub(crate) struct Keys {
    pub local: DirKeys,
    pub remote: DirKeys,
}

impl Keys {
    /// Initial の鍵（RFC 9001 5.2）。`dcid` は client が最初の Initial に入れた DCID。
    pub(crate) fn initial(dcid: &ConnectionId, is_client: bool) -> Result<Self, CryptoFailure> {
        let (c, s) = senca_tls::quic::initial_secrets(dcid.as_slice()).map_err(|_| CryptoFailure)?;
        let suite = CipherSuite::Aes128GcmSha256;
        let (cl, sv) = (DirKeys::from_secret(suite, &c)?, DirKeys::from_secret(suite, &s)?);
        Ok(if is_client {
            Self { local: cl, remote: sv }
        } else {
            Self { local: sv, remote: cl }
        })
    }
}

/// Retry の integrity tag（RFC 9001 5.8）。`retry_without_tag` は tag の前までの Retry パケット。
pub(crate) fn retry_tag(odcid: &ConnectionId, retry_without_tag: &[u8]) -> [u8; TAG_LEN] {
    const KEY: [u8; 16] = [
        0xbe, 0x0c, 0x69, 0x0b, 0x9f, 0x66, 0x57, 0x5a, 0x1d, 0x76, 0x6b, 0x54, 0xe3, 0x68, 0xc8, 0x4e,
    ];
    const NONCE: [u8; 12] = [0x46, 0x15, 0x99, 0xd3, 0x5d, 0x63, 0x2b, 0xf2, 0x23, 0x98, 0x25, 0xbb];
    let mut pseudo = Vec::with_capacity(1 + odcid.len() + retry_without_tag.len());
    pseudo.push(odcid.len() as u8);
    pseudo.extend_from_slice(odcid.as_slice());
    pseudo.extend_from_slice(retry_without_tag);
    let aead = Aes128Gcm::new(&KEY.into());
    let mut empty: [u8; 0] = [];
    let tag = aead
        .encrypt_in_place_detached(&NONCE.into(), &pseudo, &mut empty)
        .unwrap_or_default();
    let mut t = [0u8; TAG_LEN];
    t.copy_from_slice(&tag);
    t
}

/// endpoint の秘密の鍵（stateless reset token と Retry トークンの HMAC）。
#[derive(Clone)]
pub(crate) struct HmacKey(Zeroizing<[u8; 32]>);

impl std::fmt::Debug for HmacKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HmacKey(<秘密>)")
    }
}

impl HmacKey {
    pub(crate) fn new(k: [u8; 32]) -> Self {
        Self(Zeroizing::new(k))
    }

    pub(crate) fn sign(&self, label: &[u8], parts: &[&[u8]]) -> [u8; 32] {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&self.0[..]).unwrap_or_else(|_| unreachable!());
        m.update(label);
        for p in parts {
            m.update(&(p.len() as u32).to_be_bytes());
            m.update(p);
        }
        m.finalize().into_bytes().into()
    }

    /// connection ID に対する stateless reset token（RFC 9000 10.3.2）。
    pub(crate) fn reset_token(&self, cid: &ConnectionId) -> ResetToken {
        let h = self.sign(b"senca quic reset", &[cid.as_slice()]);
        let mut t = [0u8; 16];
        t.copy_from_slice(&h[..16]);
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn rfc9001_a5_chacha_short_header() {
        // RFC 9001 A.5
        let hp = hex("25a282b9e82f06f21f488917a4fc8f1b73573685608597d0efcb076b0ab7a7a4");
        let k = HeaderKey::new(CipherSuite::Chacha20Poly1305Sha256, &hp).unwrap();
        let sample: [u8; 16] = hex("5e5cd55c41f69080575d7999c25a5bfb").try_into().unwrap();
        assert_eq!(k.mask(&sample).to_vec(), hex("aefefe7d03"));
    }

    #[test]
    fn rfc9001_a1_initial_hp() {
        // client の hp 鍵と A.2 の sample
        let hp = hex("9f50449e04a0e810283a1e9933adedd2");
        let k = HeaderKey::new(CipherSuite::Aes128GcmSha256, &hp).unwrap();
        let sample: [u8; 16] = hex("d1b1c98dd7689fb8ec11d242b123dc9b").try_into().unwrap();
        assert_eq!(k.mask(&sample).to_vec(), hex("437b9aec36"));
    }

    #[test]
    fn rfc9001_a4_retry_tag() {
        let odcid = ConnectionId::new(&hex("8394c8f03e515708")).unwrap();
        let pkt = hex("ff000000010008f067a5502a4262b5746f6b656e04a265ba2eff4d829058fb3f0f2496ba");
        let (body, tag) = pkt.split_at(pkt.len() - 16);
        assert_eq!(retry_tag(&odcid, body).to_vec(), tag.to_vec());
    }
}
