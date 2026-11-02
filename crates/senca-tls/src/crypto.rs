//! 暗号の部品の薄い包み。演算はすべて検証済みライブラリ（ADR-0002）に任せる。
//!
//! ここで書くのは TLS 1.3 の取り決め（HKDF-Expand-Label、nonce の作り方など）だけ。

use std::fmt;

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes128Gcm, Aes256Gcm};
use chacha20poly1305::ChaCha20Poly1305;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256, Sha384};
use zeroize::Zeroizing;

use crate::error::{AlertDescription, Error};

/// OS の乱数源で埋める。失敗したら接続を確立しない（仕様 9.3）。
pub(crate) fn random(buf: &mut [u8]) -> Result<(), Error> {
    getrandom::fill(buf).map_err(|_| Error::Rng)
}

pub(crate) fn random_array<const N: usize>() -> Result<[u8; N], Error> {
    let mut a = [0u8; N];
    random(&mut a)?;
    Ok(a)
}

/// 秘密のバイト列。破棄時にゼロで埋め、Debug には中身を出さない。
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Zeroizing<Vec<u8>>);

impl Secret {
    pub(crate) fn new(v: Vec<u8>) -> Self {
        Self(Zeroizing::new(v))
    }

    /// 中身。扱いに注意する（ログに出さない）。
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret({} bytes)", self.0.len())
    }
}

/// 鍵スケジュールの hash。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HashAlg {
    Sha256,
    Sha384,
}

impl HashAlg {
    pub fn output_len(self) -> usize {
        match self {
            Self::Sha256 => 32,
            Self::Sha384 => 48,
        }
    }

    pub(crate) fn hash(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha256 => Sha256::digest(data).to_vec(),
            Self::Sha384 => Sha384::digest(data).to_vec(),
        }
    }

    pub(crate) fn hmac(self, key: &[u8], data: &[u8]) -> Result<Vec<u8>, Error> {
        // HMAC は任意長の鍵を受け付けるので通常は失敗しない。
        let bad = |_| Error::internal("HMAC の鍵が不正");
        Ok(match self {
            Self::Sha256 => {
                let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).map_err(bad)?;
                m.update(data);
                m.finalize().into_bytes().to_vec()
            }
            Self::Sha384 => {
                let mut m = <Hmac<Sha384> as Mac>::new_from_slice(key).map_err(bad)?;
                m.update(data);
                m.finalize().into_bytes().to_vec()
            }
        })
    }

    /// HKDF-Extract（RFC 5869）。salt が空なら hash 長のゼロ列。
    pub(crate) fn extract(self, salt: &[u8], ikm: &[u8]) -> Secret {
        match self {
            Self::Sha256 => Secret::new(Hkdf::<Sha256>::extract(Some(salt), ikm).0.to_vec()),
            Self::Sha384 => Secret::new(Hkdf::<Sha384>::extract(Some(salt), ikm).0.to_vec()),
        }
    }

    /// HKDF-Expand。prk は hash 長以上、出力は 255*hash 長以下（呼び出し側が守る）。
    pub(crate) fn expand(self, prk: &[u8], info: &[u8], len: usize) -> Result<Secret, Error> {
        let mut out = vec![0u8; len];
        let ok = match self {
            Self::Sha256 => Hkdf::<Sha256>::from_prk(prk)
                .ok()
                .and_then(|h| h.expand(info, &mut out).ok()),
            Self::Sha384 => Hkdf::<Sha384>::from_prk(prk)
                .ok()
                .and_then(|h| h.expand(info, &mut out).ok()),
        };
        ok.ok_or(Error::internal("HKDF-Expand に失敗"))?;
        Ok(Secret::new(out))
    }

    pub(crate) fn hasher(self) -> Hasher {
        match self {
            Self::Sha256 => Hasher::Sha256(Sha256::new()),
            Self::Sha384 => Hasher::Sha384(Sha384::new()),
        }
    }
}

/// 逐次 hash（transcript 用）。途中の値を何度でも取り出せる。
#[derive(Clone)]
pub(crate) enum Hasher {
    Sha256(Sha256),
    Sha384(Sha384),
}

impl Hasher {
    pub(crate) fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha256(h) => h.update(data),
            Self::Sha384(h) => h.update(data),
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<u8> {
        match self {
            Self::Sha256(h) => h.clone().finalize().to_vec(),
            Self::Sha384(h) => h.clone().finalize().to_vec(),
        }
    }
}

/// HKDF-Expand-Label（RFC 8446 7.1）。`label` には "tls13 " を付けずに渡す。
pub fn hkdf_expand_label(
    hash: HashAlg,
    secret: &[u8],
    label: &[u8],
    context: &[u8],
    len: usize,
) -> Result<Secret, Error> {
    if len > 255 * hash.output_len() || label.len() > 249 || context.len() > 255 {
        return Err(Error::internal("HKDF-Expand-Label の引数が範囲外"));
    }
    let mut info = Vec::with_capacity(4 + 6 + label.len() + context.len());
    info.extend_from_slice(&(len as u16).to_be_bytes());
    info.push((6 + label.len()) as u8);
    info.extend_from_slice(b"tls13 ");
    info.extend_from_slice(label);
    info.push(context.len() as u8);
    info.extend_from_slice(context);
    hash.expand(secret, &info, len)
}

/// TLS 1.3 の暗号スイート。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CipherSuite {
    /// TLS_AES_128_GCM_SHA256 (0x1301)
    Aes128GcmSha256,
    /// TLS_AES_256_GCM_SHA384 (0x1302)
    Aes256GcmSha384,
    /// TLS_CHACHA20_POLY1305_SHA256 (0x1303)
    Chacha20Poly1305Sha256,
}

impl CipherSuite {
    /// 既定の優先順。
    pub const ALL: [CipherSuite; 3] = [
        Self::Aes128GcmSha256,
        Self::Aes256GcmSha384,
        Self::Chacha20Poly1305Sha256,
    ];

    pub fn from_u16(v: u16) -> Option<Self> {
        match v {
            0x1301 => Some(Self::Aes128GcmSha256),
            0x1302 => Some(Self::Aes256GcmSha384),
            0x1303 => Some(Self::Chacha20Poly1305Sha256),
            _ => None,
        }
    }

    pub fn to_u16(self) -> u16 {
        match self {
            Self::Aes128GcmSha256 => 0x1301,
            Self::Aes256GcmSha384 => 0x1302,
            Self::Chacha20Poly1305Sha256 => 0x1303,
        }
    }

    pub fn hash(self) -> HashAlg {
        match self {
            Self::Aes256GcmSha384 => HashAlg::Sha384,
            _ => HashAlg::Sha256,
        }
    }

    pub fn key_len(self) -> usize {
        match self {
            Self::Aes128GcmSha256 => 16,
            _ => 32,
        }
    }

    /// IANA 名。
    pub fn name(self) -> &'static str {
        match self {
            Self::Aes128GcmSha256 => "TLS_AES_128_GCM_SHA256",
            Self::Aes256GcmSha384 => "TLS_AES_256_GCM_SHA384",
            Self::Chacha20Poly1305Sha256 => "TLS_CHACHA20_POLY1305_SHA256",
        }
    }
}

/// AEAD の鍵。
pub(crate) enum Aead {
    Aes128(Box<Aes128Gcm>),
    Aes256(Box<Aes256Gcm>),
    ChaCha(Box<ChaCha20Poly1305>),
}

impl Aead {
    pub(crate) fn new(suite: CipherSuite, key: &[u8]) -> Result<Self, Error> {
        let bad = |_| Error::internal("AEAD 鍵長が不正");
        Ok(match suite {
            CipherSuite::Aes128GcmSha256 => Self::Aes128(Box::new(Aes128Gcm::new_from_slice(key).map_err(bad)?)),
            CipherSuite::Aes256GcmSha384 => Self::Aes256(Box::new(Aes256Gcm::new_from_slice(key).map_err(bad)?)),
            CipherSuite::Chacha20Poly1305Sha256 => {
                Self::ChaCha(Box::new(ChaCha20Poly1305::new_from_slice(key).map_err(bad)?))
            }
        })
    }

    /// 暗号化して tag（16 バイト）を末尾に足す。
    pub(crate) fn seal(&self, nonce: &[u8; 12], aad: &[u8], buf: &mut Vec<u8>) -> Result<(), Error> {
        let n = &(*nonce).into();
        let r = match self {
            Self::Aes128(k) => k.encrypt_in_place(n, aad, buf),
            Self::Aes256(k) => k.encrypt_in_place(n, aad, buf),
            Self::ChaCha(k) => k.encrypt_in_place(n, aad, buf),
        };
        r.map_err(|_| Error::internal("暗号化に失敗"))
    }

    /// 復号して tag を取り除く。認証失敗は bad_record_mac。
    pub(crate) fn open(&self, nonce: &[u8; 12], aad: &[u8], buf: &mut Vec<u8>) -> Result<(), Error> {
        let n = &(*nonce).into();
        let r = match self {
            Self::Aes128(k) => k.decrypt_in_place(n, aad, buf),
            Self::Aes256(k) => k.decrypt_in_place(n, aad, buf),
            Self::ChaCha(k) => k.decrypt_in_place(n, aad, buf),
        };
        r.map_err(|_| Error::fatal(AlertDescription::BAD_RECORD_MAC, "レコードの認証に失敗"))
    }
}

/// レコード保護の鍵・IV・連番（RFC 8446 5.3）。
pub(crate) struct RecordKey {
    aead: Aead,
    iv: Zeroizing<[u8; 12]>,
    seq: u64,
}

impl RecordKey {
    pub(crate) fn from_secret(suite: CipherSuite, secret: &Secret) -> Result<Self, Error> {
        let h = suite.hash();
        let key = hkdf_expand_label(h, secret.as_bytes(), b"key", &[], suite.key_len())?;
        let ivs = hkdf_expand_label(h, secret.as_bytes(), b"iv", &[], 12)?;
        let mut iv = Zeroizing::new([0u8; 12]);
        iv.copy_from_slice(ivs.as_bytes());
        Ok(Self {
            aead: Aead::new(suite, key.as_bytes())?,
            iv,
            seq: 0,
        })
    }

    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }

    /// 次の nonce を作り、連番を進める。連番が尽きたらエラー（nonce を再利用しない）。
    fn next_nonce(&mut self) -> Result<[u8; 12], Error> {
        if self.seq == u64::MAX {
            return Err(Error::internal("レコード連番が尽きた"));
        }
        let mut n = *self.iv;
        for (a, b) in n[4..].iter_mut().zip(self.seq.to_be_bytes()) {
            *a ^= b;
        }
        self.seq += 1;
        Ok(n)
    }

    pub(crate) fn seal(&mut self, aad: &[u8], buf: &mut Vec<u8>) -> Result<(), Error> {
        let n = self.next_nonce()?;
        self.aead.seal(&n, aad, buf)
    }

    /// 失敗しても連番は進めない（読み飛ばし用）。
    pub(crate) fn open(&mut self, aad: &[u8], buf: &mut Vec<u8>) -> Result<(), Error> {
        if self.seq == u64::MAX {
            return Err(Error::internal("レコード連番が尽きた"));
        }
        let mut n = *self.iv;
        for (a, b) in n[4..].iter_mut().zip(self.seq.to_be_bytes()) {
            *a ^= b;
        }
        self.aead.open(&n, aad, buf)?;
        self.seq += 1;
        Ok(())
    }
}

/// 鍵交換の group。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NamedGroup {
    /// x25519 (0x001d)
    X25519,
    /// secp256r1 (0x0017)
    Secp256r1,
}

impl NamedGroup {
    pub const ALL: [NamedGroup; 2] = [Self::X25519, Self::Secp256r1];

    pub fn from_u16(v: u16) -> Option<Self> {
        match v {
            0x001d => Some(Self::X25519),
            0x0017 => Some(Self::Secp256r1),
            _ => None,
        }
    }

    pub fn to_u16(self) -> u16 {
        match self {
            Self::X25519 => 0x001d,
            Self::Secp256r1 => 0x0017,
        }
    }
}

/// 一時鍵（ECDHE）。
pub(crate) enum KeyExchange {
    X25519(x25519_dalek::StaticSecret),
    P256(p256::SecretKey),
}

impl KeyExchange {
    /// 鍵対を作り、公開値（KeyShareEntry.key_exchange）を返す。
    pub(crate) fn generate(group: NamedGroup) -> Result<(Self, Vec<u8>), Error> {
        match group {
            NamedGroup::X25519 => {
                let mut b = Zeroizing::new([0u8; 32]);
                random(&mut b[..])?;
                let s = x25519_dalek::StaticSecret::from(*b);
                let p = x25519_dalek::PublicKey::from(&s);
                Ok((Self::X25519(s), p.as_bytes().to_vec()))
            }
            NamedGroup::Secp256r1 => {
                // 範囲外（0 や位数以上）の値は引き直す。確率は無視できるほど小さい。
                for _ in 0..16 {
                    let mut b = Zeroizing::new([0u8; 32]);
                    random(&mut b[..])?;
                    if let Ok(s) = p256::SecretKey::from_slice(&b[..]) {
                        use p256::elliptic_curve::sec1::ToEncodedPoint;
                        let p = s.public_key().to_encoded_point(false).as_bytes().to_vec();
                        return Ok((Self::P256(s), p));
                    }
                }
                Err(Error::Rng)
            }
        }
    }

    /// 共有秘密を計算する。相手の公開値を厳格に検査する。
    pub(crate) fn agree(&self, peer: &[u8]) -> Result<Secret, Error> {
        match self {
            Self::X25519(s) => {
                let pk: [u8; 32] = peer
                    .try_into()
                    .map_err(|_| Error::illegal("x25519 の公開値の長さが不正"))?;
                let shared = s.diffie_hellman(&x25519_dalek::PublicKey::from(pk));
                // 小位数点などによる全ゼロの共有秘密を拒否する（RFC 8446 7.4.2）。
                if !shared.was_contributory() {
                    return Err(Error::illegal("x25519 の共有秘密が全ゼロ"));
                }
                Ok(Secret::new(shared.as_bytes().to_vec()))
            }
            Self::P256(s) => {
                // 非圧縮形式（0x04 || X || Y）だけを受け付ける（RFC 8446 4.2.8.2）。
                if peer.len() != 65 || peer[0] != 0x04 {
                    return Err(Error::illegal("secp256r1 の公開値は非圧縮形式でなければならない"));
                }
                let pk = p256::PublicKey::from_sec1_bytes(peer)
                    .map_err(|_| Error::illegal("secp256r1 の公開値が曲線上にない"))?;
                let shared = p256::ecdh::diffie_hellman(s.to_nonzero_scalar(), pk.as_affine());
                Ok(Secret::new(shared.raw_secret_bytes().to_vec()))
            }
        }
    }
}
