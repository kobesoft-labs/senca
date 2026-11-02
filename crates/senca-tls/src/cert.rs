//! 証明書と秘密鍵の読み込み、署名、検証。
//!
//! X.509 の解析とパス検証は rustls-webpki（仕様 2.1 の例外）に任せる。webpki が呼ぶ署名検証の
//! アルゴリズムは RustCrypto（p256・p384・rsa・ed25519-dalek）で実装する。ホスト名・用途（EKU）・
//! 期限・信頼 anchor の方針はこの crate が決める（仕様 9.3）。

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use pki_types::pem::PemObject;
use pki_types::{
    AlgorithmIdentifier, CertificateDer, InvalidSignature, PrivateKeyDer, ServerName, SignatureVerificationAlgorithm,
    TrustAnchor, UnixTime, alg_id,
};
use sha2::{Digest, Sha256, Sha384, Sha512};

use crate::error::{AlertDescription, Error};

/// TLS の SignatureScheme（RFC 8446 4.2.3）。
pub mod scheme {
    pub const RSA_PKCS1_SHA256: u16 = 0x0401;
    pub const RSA_PKCS1_SHA384: u16 = 0x0501;
    pub const RSA_PKCS1_SHA512: u16 = 0x0601;
    pub const ECDSA_SECP256R1_SHA256: u16 = 0x0403;
    pub const ECDSA_SECP384R1_SHA384: u16 = 0x0503;
    pub const RSA_PSS_RSAE_SHA256: u16 = 0x0804;
    pub const RSA_PSS_RSAE_SHA384: u16 = 0x0805;
    pub const RSA_PSS_RSAE_SHA512: u16 = 0x0806;
    pub const ED25519: u16 = 0x0807;
}

/// CertificateVerify に使ってよい署名方式（受信側）。
pub(crate) const VERIFY_SCHEMES: [u16; 6] = [
    scheme::ECDSA_SECP256R1_SHA256,
    scheme::ED25519,
    scheme::RSA_PSS_RSAE_SHA256,
    scheme::RSA_PSS_RSAE_SHA384,
    scheme::RSA_PSS_RSAE_SHA512,
    scheme::ECDSA_SECP384R1_SHA384,
];

/// signature_algorithms で提示する並び。rsa_pkcs1 は証明書の署名のためだけに載せる（RFC 8446 4.2.3）。
pub(crate) const OFFERED_SCHEMES: [u16; 9] = [
    scheme::ECDSA_SECP256R1_SHA256,
    scheme::ED25519,
    scheme::RSA_PSS_RSAE_SHA256,
    scheme::RSA_PSS_RSAE_SHA384,
    scheme::RSA_PSS_RSAE_SHA512,
    scheme::ECDSA_SECP384R1_SHA384,
    scheme::RSA_PKCS1_SHA256,
    scheme::RSA_PKCS1_SHA384,
    scheme::RSA_PKCS1_SHA512,
];

// ---------------------------------------------------------------------------
// webpki 向けの署名検証アルゴリズム（RustCrypto）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum HashKind {
    Sha256,
    Sha384,
    Sha512,
}

impl HashKind {
    fn digest(self, m: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha256 => Sha256::digest(m).to_vec(),
            Self::Sha384 => Sha384::digest(m).to_vec(),
            Self::Sha512 => Sha512::digest(m).to_vec(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    EcdsaP256(HashKind),
    EcdsaP384(HashKind),
    RsaPkcs1(HashKind),
    RsaPss(HashKind),
    Ed25519,
}

#[derive(Debug)]
struct Alg {
    pk: AlgorithmIdentifier,
    sig: AlgorithmIdentifier,
    kind: Kind,
}

/// RSA の鍵長の範囲（ビット）。
const RSA_MIN_BITS: usize = 2048;
const RSA_MAX_BITS: usize = 8192;

fn rsa_public(pk: &[u8]) -> Result<rsa::RsaPublicKey, InvalidSignature> {
    use rsa::pkcs1::DecodeRsaPublicKey;
    use rsa::traits::PublicKeyParts;
    let k = rsa::RsaPublicKey::from_pkcs1_der(pk).map_err(|_| InvalidSignature)?;
    let bits = k.n().bits();
    if !(RSA_MIN_BITS..=RSA_MAX_BITS).contains(&bits) {
        return Err(InvalidSignature);
    }
    Ok(k)
}

impl SignatureVerificationAlgorithm for Alg {
    fn verify_signature(&self, public_key: &[u8], message: &[u8], signature: &[u8]) -> Result<(), InvalidSignature> {
        use rsa::signature::Verifier;
        use rsa::signature::hazmat::PrehashVerifier;
        match self.kind {
            Kind::EcdsaP256(h) => {
                let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(public_key).map_err(|_| InvalidSignature)?;
                let sig = p256::ecdsa::Signature::from_der(signature).map_err(|_| InvalidSignature)?;
                vk.verify_prehash(&h.digest(message), &sig).map_err(|_| InvalidSignature)
            }
            Kind::EcdsaP384(h) => {
                let vk = p384::ecdsa::VerifyingKey::from_sec1_bytes(public_key).map_err(|_| InvalidSignature)?;
                let sig = p384::ecdsa::Signature::from_der(signature).map_err(|_| InvalidSignature)?;
                vk.verify_prehash(&h.digest(message), &sig).map_err(|_| InvalidSignature)
            }
            Kind::RsaPkcs1(h) => {
                let k = rsa_public(public_key)?;
                let sig = rsa::pkcs1v15::Signature::try_from(signature).map_err(|_| InvalidSignature)?;
                let r = match h {
                    HashKind::Sha256 => rsa::pkcs1v15::VerifyingKey::<Sha256>::new(k).verify(message, &sig),
                    HashKind::Sha384 => rsa::pkcs1v15::VerifyingKey::<Sha384>::new(k).verify(message, &sig),
                    HashKind::Sha512 => rsa::pkcs1v15::VerifyingKey::<Sha512>::new(k).verify(message, &sig),
                };
                r.map_err(|_| InvalidSignature)
            }
            Kind::RsaPss(h) => {
                let k = rsa_public(public_key)?;
                let sig = rsa::pss::Signature::try_from(signature).map_err(|_| InvalidSignature)?;
                // salt 長は hash 長に固定（RFC 8446 4.2.3）。
                let r = match h {
                    HashKind::Sha256 => rsa::pss::VerifyingKey::<Sha256>::new(k).verify(message, &sig),
                    HashKind::Sha384 => rsa::pss::VerifyingKey::<Sha384>::new(k).verify(message, &sig),
                    HashKind::Sha512 => rsa::pss::VerifyingKey::<Sha512>::new(k).verify(message, &sig),
                };
                r.map_err(|_| InvalidSignature)
            }
            Kind::Ed25519 => {
                let pk: [u8; 32] = public_key.try_into().map_err(|_| InvalidSignature)?;
                let vk = ed25519_dalek::VerifyingKey::from_bytes(&pk).map_err(|_| InvalidSignature)?;
                let sig = ed25519_dalek::Signature::from_slice(signature).map_err(|_| InvalidSignature)?;
                vk.verify_strict(message, &sig).map_err(|_| InvalidSignature)
            }
        }
    }

    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        self.pk
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        self.sig
    }
}

static ECDSA_P256_SHA256: Alg = Alg {
    pk: alg_id::ECDSA_P256,
    sig: alg_id::ECDSA_SHA256,
    kind: Kind::EcdsaP256(HashKind::Sha256),
};
static ECDSA_P256_SHA384: Alg = Alg {
    pk: alg_id::ECDSA_P256,
    sig: alg_id::ECDSA_SHA384,
    kind: Kind::EcdsaP256(HashKind::Sha384),
};
static ECDSA_P384_SHA384: Alg = Alg {
    pk: alg_id::ECDSA_P384,
    sig: alg_id::ECDSA_SHA384,
    kind: Kind::EcdsaP384(HashKind::Sha384),
};
static ECDSA_P384_SHA256: Alg = Alg {
    pk: alg_id::ECDSA_P384,
    sig: alg_id::ECDSA_SHA256,
    kind: Kind::EcdsaP384(HashKind::Sha256),
};
static RSA_PKCS1_SHA256: Alg = Alg {
    pk: alg_id::RSA_ENCRYPTION,
    sig: alg_id::RSA_PKCS1_SHA256,
    kind: Kind::RsaPkcs1(HashKind::Sha256),
};
static RSA_PKCS1_SHA384: Alg = Alg {
    pk: alg_id::RSA_ENCRYPTION,
    sig: alg_id::RSA_PKCS1_SHA384,
    kind: Kind::RsaPkcs1(HashKind::Sha384),
};
static RSA_PKCS1_SHA512: Alg = Alg {
    pk: alg_id::RSA_ENCRYPTION,
    sig: alg_id::RSA_PKCS1_SHA512,
    kind: Kind::RsaPkcs1(HashKind::Sha512),
};
static RSA_PSS_SHA256: Alg = Alg {
    pk: alg_id::RSA_ENCRYPTION,
    sig: alg_id::RSA_PSS_SHA256,
    kind: Kind::RsaPss(HashKind::Sha256),
};
static RSA_PSS_SHA384: Alg = Alg {
    pk: alg_id::RSA_ENCRYPTION,
    sig: alg_id::RSA_PSS_SHA384,
    kind: Kind::RsaPss(HashKind::Sha384),
};
static RSA_PSS_SHA512: Alg = Alg {
    pk: alg_id::RSA_ENCRYPTION,
    sig: alg_id::RSA_PSS_SHA512,
    kind: Kind::RsaPss(HashKind::Sha512),
};
static ED25519: Alg = Alg {
    pk: alg_id::ED25519,
    sig: alg_id::ED25519,
    kind: Kind::Ed25519,
};

/// 証明書チェーンの署名に受け付けるアルゴリズム。
static CHAIN_ALGS: &[&dyn SignatureVerificationAlgorithm] = &[
    &ECDSA_P256_SHA256,
    &ECDSA_P256_SHA384,
    &ECDSA_P384_SHA384,
    &ECDSA_P384_SHA256,
    &ED25519,
    &RSA_PKCS1_SHA256,
    &RSA_PKCS1_SHA384,
    &RSA_PKCS1_SHA512,
    &RSA_PSS_SHA256,
    &RSA_PSS_SHA384,
    &RSA_PSS_SHA512,
];

/// CertificateVerify の SignatureScheme に対応する検証アルゴリズム。rsa_pkcs1 は許さない。
fn cv_alg(scheme_id: u16) -> Option<&'static dyn SignatureVerificationAlgorithm> {
    Some(match scheme_id {
        scheme::ECDSA_SECP256R1_SHA256 => &ECDSA_P256_SHA256,
        scheme::ECDSA_SECP384R1_SHA384 => &ECDSA_P384_SHA384,
        scheme::RSA_PSS_RSAE_SHA256 => &RSA_PSS_SHA256,
        scheme::RSA_PSS_RSAE_SHA384 => &RSA_PSS_SHA384,
        scheme::RSA_PSS_RSAE_SHA512 => &RSA_PSS_SHA512,
        scheme::ED25519 => &ED25519,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// 署名鍵
// ---------------------------------------------------------------------------

/// rsa crate に渡す乱数源。OS の乱数源を使い、失敗したら印を付ける（その署名は捨てる）。
struct OsRng {
    failed: bool,
}

impl rsa::rand_core::RngCore for OsRng {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.fill_bytes(&mut b);
        u32::from_le_bytes(b)
    }
    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill_bytes(&mut b);
        u64::from_le_bytes(b)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        if getrandom::fill(dest).is_err() {
            self.failed = true;
            dest.fill(0);
        }
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rsa::rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl rsa::rand_core::CryptoRng for OsRng {}

/// 自分の署名鍵。
pub(crate) enum SigningKey {
    EcdsaP256(p256::ecdsa::SigningKey),
    Rsa(Box<rsa::RsaPrivateKey>),
    Ed25519(Box<ed25519_dalek::SigningKey>),
}

impl SigningKey {
    /// 優先順の署名方式。
    pub(crate) fn schemes(&self) -> &'static [u16] {
        match self {
            Self::EcdsaP256(_) => &[scheme::ECDSA_SECP256R1_SHA256],
            Self::Rsa(_) => &[
                scheme::RSA_PSS_RSAE_SHA256,
                scheme::RSA_PSS_RSAE_SHA384,
                scheme::RSA_PSS_RSAE_SHA512,
            ],
            Self::Ed25519(_) => &[scheme::ED25519],
        }
    }

    /// 相手の signature_algorithms から使える方式を選ぶ。
    pub(crate) fn choose_scheme(&self, offered: &[u16]) -> Option<u16> {
        self.schemes().iter().copied().find(|s| offered.contains(s))
    }

    pub(crate) fn sign(&self, scheme_id: u16, msg: &[u8]) -> Result<Vec<u8>, Error> {
        use rsa::signature::{RandomizedSigner, SignatureEncoding, Signer};
        let bad = |_| Error::internal("署名に失敗");
        match (self, scheme_id) {
            (Self::EcdsaP256(k), scheme::ECDSA_SECP256R1_SHA256) => {
                let sig: p256::ecdsa::Signature = k.try_sign(msg).map_err(bad)?;
                Ok(sig.to_der().as_bytes().to_vec())
            }
            (Self::Ed25519(k), scheme::ED25519) => Ok(k.sign(msg).to_bytes().to_vec()),
            (Self::Rsa(k), s) => {
                let mut rng = OsRng { failed: false };
                let sig = match s {
                    scheme::RSA_PSS_RSAE_SHA256 => rsa::pss::BlindedSigningKey::<Sha256>::new((**k).clone())
                        .try_sign_with_rng(&mut rng, msg)
                        .map_err(bad)?
                        .to_vec(),
                    scheme::RSA_PSS_RSAE_SHA384 => rsa::pss::BlindedSigningKey::<Sha384>::new((**k).clone())
                        .try_sign_with_rng(&mut rng, msg)
                        .map_err(bad)?
                        .to_vec(),
                    scheme::RSA_PSS_RSAE_SHA512 => rsa::pss::BlindedSigningKey::<Sha512>::new((**k).clone())
                        .try_sign_with_rng(&mut rng, msg)
                        .map_err(bad)?
                        .to_vec(),
                    _ => return Err(Error::internal("鍵に合わない署名方式")),
                };
                if rng.failed {
                    return Err(Error::Rng);
                }
                Ok(sig)
            }
            _ => Err(Error::internal("鍵に合わない署名方式")),
        }
    }

    /// 証明書の SubjectPublicKeyInfo（DER）とこの鍵の公開鍵が一致するか。
    fn matches_spki(&self, spki: &[u8]) -> bool {
        match self {
            Self::EcdsaP256(k) => {
                use p256::pkcs8::DecodePublicKey;
                p256::PublicKey::from_public_key_der(spki)
                    .map(|p| p == p256::PublicKey::from(k.verifying_key()))
                    .unwrap_or(false)
            }
            Self::Rsa(k) => {
                use rsa::pkcs8::DecodePublicKey;
                rsa::RsaPublicKey::from_public_key_der(spki)
                    .map(|p| p == k.to_public_key())
                    .unwrap_or(false)
            }
            Self::Ed25519(k) => {
                use ed25519_dalek::pkcs8::DecodePublicKey;
                ed25519_dalek::VerifyingKey::from_public_key_der(spki)
                    .map(|p| p == k.verifying_key())
                    .unwrap_or(false)
            }
        }
    }

    fn from_der(key: &PrivateKeyDer<'_>) -> Result<Self, Error> {
        let bad = |what: &str| Error::Config(format!("秘密鍵を読めない: {what}"));
        match key {
            PrivateKeyDer::Pkcs8(k) => {
                let der = k.secret_pkcs8_der();
                {
                    use p256::pkcs8::DecodePrivateKey;
                    if let Ok(s) = p256::ecdsa::SigningKey::from_pkcs8_der(der) {
                        return Ok(Self::EcdsaP256(s));
                    }
                }
                {
                    use ed25519_dalek::pkcs8::DecodePrivateKey;
                    if let Ok(s) = ed25519_dalek::SigningKey::from_pkcs8_der(der) {
                        return Ok(Self::Ed25519(Box::new(s)));
                    }
                }
                {
                    use rsa::pkcs8::DecodePrivateKey;
                    if let Ok(s) = rsa::RsaPrivateKey::from_pkcs8_der(der) {
                        return Self::rsa(s);
                    }
                }
                Err(bad("PKCS#8 の鍵の種類に対応していない（ECDSA P-256・RSA・Ed25519 だけ）"))
            }
            PrivateKeyDer::Sec1(k) => {
                let s = p256::SecretKey::from_sec1_der(k.secret_sec1_der())
                    .map_err(|_| bad("SEC1 の鍵が P-256 でない"))?;
                Ok(Self::EcdsaP256(p256::ecdsa::SigningKey::from(s)))
            }
            PrivateKeyDer::Pkcs1(k) => {
                use rsa::pkcs1::DecodeRsaPrivateKey;
                let s = rsa::RsaPrivateKey::from_pkcs1_der(k.secret_pkcs1_der()).map_err(|_| bad("PKCS#1"))?;
                Self::rsa(s)
            }
            _ => Err(bad("未対応の形式")),
        }
    }

    fn rsa(k: rsa::RsaPrivateKey) -> Result<Self, Error> {
        use rsa::traits::PublicKeyParts;
        let bits = k.n().bits();
        if !(RSA_MIN_BITS..=RSA_MAX_BITS).contains(&bits) {
            return Err(Error::Config(format!("RSA の鍵長 {bits} ビットは使えない（2048〜8192）")));
        }
        k.validate().map_err(|_| Error::Config("RSA 鍵が不正".into()))?;
        Ok(Self::Rsa(Box::new(k)))
    }
}

// ---------------------------------------------------------------------------
// 証明書チェーン + 秘密鍵
// ---------------------------------------------------------------------------

/// 証明書チェーン（leaf が先頭）と、その leaf の秘密鍵。
pub struct CertifiedKey {
    pub(crate) chain: Vec<CertificateDer<'static>>,
    pub(crate) key: SigningKey,
}

impl fmt::Debug for CertifiedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertifiedKey")
            .field("chain_len", &self.chain.len())
            .field("key", &"<秘密>")
            .finish()
    }
}

impl CertifiedKey {
    /// PEM の証明書チェーン（leaf から順）と PEM の秘密鍵（PKCS#8・SEC1・PKCS#1）から作る。
    ///
    /// 鍵と leaf の公開鍵が一致することを確かめる。
    pub fn from_pem(chain_pem: &[u8], key_pem: &[u8]) -> Result<Self, Error> {
        let chain = CertificateDer::pem_slice_iter(chain_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::Config(format!("証明書の PEM を読めない: {e:?}")))?;
        let key =
            PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| Error::Config(format!("鍵の PEM を読めない: {e:?}")))?;
        Self::from_der(chain, &key)
    }

    /// DER の証明書チェーンと秘密鍵から作る。
    pub fn from_der(chain: Vec<CertificateDer<'static>>, key: &PrivateKeyDer<'_>) -> Result<Self, Error> {
        if chain.is_empty() {
            return Err(Error::Config("証明書がない".into()));
        }
        if chain.len() > crate::msgs::MAX_CHAIN_LEN {
            return Err(Error::Config("証明書チェーンが長すぎる".into()));
        }
        let total: usize = chain.iter().map(|c| c.len() + 5).sum();
        if total + 8 > crate::msgs::max_message_len(crate::msgs::hs::CERTIFICATE) {
            return Err(Error::Config("証明書チェーンが大きすぎる（128 KiB まで）".into()));
        }
        let key = SigningKey::from_der(key)?;
        let leaf = webpki::EndEntityCert::try_from(&chain[0])
            .map_err(|e| Error::Config(format!("leaf 証明書を読めない: {e:?}")))?;
        if !key.matches_spki(leaf.subject_public_key_info().as_ref()) {
            return Err(Error::Config("秘密鍵が leaf 証明書と一致しない".into()));
        }
        Ok(Self { chain, key })
    }

    /// ファイルから読む（設定の読み込み時だけ使う）。
    pub fn from_pem_files(chain: impl AsRef<Path>, key: impl AsRef<Path>) -> Result<Self, Error> {
        let c = std::fs::read(chain.as_ref()).map_err(|e| Error::Config(format!("証明書ファイル: {e}")))?;
        let k = std::fs::read(key.as_ref()).map_err(|e| Error::Config(format!("鍵ファイル: {e}")))?;
        Self::from_pem(&c, &k)
    }

    pub fn chain(&self) -> &[CertificateDer<'static>] {
        &self.chain
    }

    /// leaf の SubjectPublicKeyInfo の SHA-256（pin に使う値）。
    pub fn spki_sha256(&self) -> Result<[u8; 32], Error> {
        spki_sha256(&self.chain[0])
    }
}

/// 証明書（DER）の SubjectPublicKeyInfo の SHA-256。
pub fn spki_sha256(cert_der: &[u8]) -> Result<[u8; 32], Error> {
    let c = CertificateDer::from(cert_der);
    let ee = webpki::EndEntityCert::try_from(&c).map_err(|e| Error::Certificate {
        alert: AlertDescription::BAD_CERTIFICATE,
        detail: format!("{e:?}"),
    })?;
    Ok(Sha256::digest(ee.subject_public_key_info().as_ref()).into())
}

// ---------------------------------------------------------------------------
// 信頼 anchor
// ---------------------------------------------------------------------------

/// 信頼する root 証明書の集まり。
#[derive(Clone, Default)]
pub struct RootStore {
    anchors: Vec<TrustAnchor<'static>>,
}

impl fmt::Debug for RootStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RootStore({} anchors)", self.anchors.len())
    }
}

impl RootStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.anchors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.anchors.is_empty()
    }

    /// DER の root 証明書を一つ足す。
    pub fn add_der(&mut self, der: &[u8]) -> Result<(), Error> {
        let c = CertificateDer::from(der);
        let a = webpki::anchor_from_trusted_cert(&c)
            .map_err(|e| Error::Config(format!("root 証明書を読めない: {e:?}")))?;
        self.anchors.push(a.to_owned());
        Ok(())
    }

    /// PEM の root 証明書を全部足す。一つでも読めなければエラー。足した数を返す。
    pub fn add_pem(&mut self, pem: &[u8]) -> Result<usize, Error> {
        let mut n = 0;
        for c in CertificateDer::pem_slice_iter(pem) {
            let c = c.map_err(|e| Error::Config(format!("PEM を読めない: {e:?}")))?;
            self.add_der(&c)?;
            n += 1;
        }
        Ok(n)
    }

    /// PEM ファイルから足す。
    pub fn add_pem_file(&mut self, path: impl AsRef<Path>) -> Result<usize, Error> {
        let b = std::fs::read(path.as_ref()).map_err(|e| Error::Config(format!("root ファイル: {e}")))?;
        self.add_pem(&b)
    }

    /// OS の root（`SSL_CERT_FILE`、なければ /etc/ssl/certs など）を読む。
    /// webpki が解釈できない証明書は飛ばす。
    pub fn system() -> Result<Self, Error> {
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        if let Some(p) = std::env::var_os("SSL_CERT_FILE") {
            candidates.push(p.into());
        }
        for p in [
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/pki/tls/certs/ca-bundle.crt",
            "/etc/ssl/cert.pem",
        ] {
            candidates.push(p.into());
        }
        let mut s = Self::new();
        for p in candidates {
            if let Ok(b) = std::fs::read(&p) {
                s.add_pem_lenient(&b);
                if !s.is_empty() {
                    return Ok(s);
                }
            }
        }
        // 束ねたファイルがなければ /etc/ssl/certs/*.pem を読む。
        if let Ok(rd) = std::fs::read_dir("/etc/ssl/certs") {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "pem")
                    && let Ok(b) = std::fs::read(&p)
                {
                    s.add_pem_lenient(&b);
                }
            }
        }
        if s.is_empty() {
            return Err(Error::Config("OS の root 証明書が見つからない".into()));
        }
        Ok(s)
    }

    fn add_pem_lenient(&mut self, pem: &[u8]) {
        for c in CertificateDer::pem_slice_iter(pem).flatten() {
            let _ = self.add_der(&c);
        }
    }

    pub(crate) fn anchors(&self) -> &[TrustAnchor<'static>] {
        &self.anchors
    }
}

/// サーバー証明書の検証方針（client 側）。
#[derive(Clone, Debug)]
pub enum ServerVerifier {
    /// root へのパス・期限・EKU(serverAuth)・ホスト名／IP を webpki で検証する。
    /// `spki_pins` が空でなければ、leaf の SPKI の SHA-256 がどれかと一致することも求める。
    WebPki {
        roots: Arc<RootStore>,
        spki_pins: Vec<[u8; 32]>,
    },
    /// leaf の SPKI の SHA-256 だけで照合する（自己署名の node 間通信など）。
    /// チェーン・期限・ホスト名は見ない。CertificateVerify の署名は検証する。
    PinnedSpkiOnly(Vec<[u8; 32]>),
}

/// クライアント証明書の要求（server 側、mTLS）。
#[derive(Clone, Debug, Default)]
pub enum ClientAuth {
    /// 要求しない。
    #[default]
    None,
    /// 要求するが、出さなくても受け付ける。出されたら検証する。
    Optional(Arc<RootStore>),
    /// 必須。出さなければ certificate_required で終える。
    Required(Arc<RootStore>),
}

fn webpki_alert(e: &webpki::Error) -> AlertDescription {
    use webpki::Error as E;
    match e {
        E::UnknownIssuer => AlertDescription::UNKNOWN_CA,
        E::CertExpired { .. } | E::CertNotValidYet { .. } => AlertDescription::CERTIFICATE_EXPIRED,
        E::CertRevoked => AlertDescription::CERTIFICATE_REVOKED,
        E::BadDer | E::BadDerTime => AlertDescription::DECODE_ERROR,
        _ => AlertDescription::BAD_CERTIFICATE,
    }
}

fn cert_err(e: webpki::Error) -> Error {
    Error::Certificate {
        alert: webpki_alert(&e),
        detail: format!("{e:?}"),
    }
}

/// 証明書チェーンを root まで検証する。
fn verify_chain(
    chain: &[CertificateDer<'_>],
    roots: &RootStore,
    now: UnixTime,
    usage: webpki::KeyUsage,
) -> Result<(), Error> {
    let ee = webpki::EndEntityCert::try_from(&chain[0]).map_err(cert_err)?;
    ee.verify_for_usage(CHAIN_ALGS, roots.anchors(), &chain[1..], now, usage, None, None)
        .map_err(cert_err)?;
    Ok(())
}

/// サーバーの証明書チェーンを検証する（client 側）。
pub(crate) fn verify_server(
    verifier: &ServerVerifier,
    chain: &[CertificateDer<'_>],
    server_name: &ServerName<'_>,
    now: UnixTime,
) -> Result<(), Error> {
    let pins = match verifier {
        ServerVerifier::WebPki { roots, spki_pins } => {
            verify_chain(chain, roots, now, webpki::KeyUsage::server_auth())?;
            let ee = webpki::EndEntityCert::try_from(&chain[0]).map_err(cert_err)?;
            ee.verify_is_valid_for_subject_name(server_name).map_err(cert_err)?;
            spki_pins
        }
        ServerVerifier::PinnedSpkiOnly(p) => p,
    };
    if !pins.is_empty() || matches!(verifier, ServerVerifier::PinnedSpkiOnly(_)) {
        let h = spki_sha256(&chain[0])?;
        if !pins.contains(&h) {
            return Err(Error::Certificate {
                alert: AlertDescription::BAD_CERTIFICATE,
                detail: "SPKI の pin と一致しない".into(),
            });
        }
    }
    Ok(())
}

/// クライアント証明書のチェーンを検証する（server 側）。
pub(crate) fn verify_client(roots: &RootStore, chain: &[CertificateDer<'_>], now: UnixTime) -> Result<(), Error> {
    verify_chain(chain, roots, now, webpki::KeyUsage::client_auth())
}

/// CertificateVerify の署名を leaf の公開鍵で検証する。
pub(crate) fn verify_signature(leaf: &CertificateDer<'_>, scheme_id: u16, msg: &[u8], sig: &[u8]) -> Result<(), Error> {
    let alg = cv_alg(scheme_id).ok_or(Error::illegal("CertificateVerify の署名方式が提示外"))?;
    let ee = webpki::EndEntityCert::try_from(leaf).map_err(cert_err)?;
    ee.verify_signature(alg, msg, sig)
        .map_err(|_| Error::fatal(AlertDescription::DECRYPT_ERROR, "CertificateVerify の署名が不正"))
}
