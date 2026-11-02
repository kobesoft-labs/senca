//! ECDSA P-256 の自己署名証明書と fingerprint（RFC 8122・RFC 8827 6.5）。
//!
//! 証明書の生成・解析は `x509-cert`（RustCrypto）、署名は `p256` を使う。WebRTC では証明書は
//! 自己署名で、信頼の根拠は SDP の `a=fingerprint`（SHA-256）との一致である。

use std::time::Duration;

use p256::ecdsa::{SigningKey, VerifyingKey};
use p256::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePublicKey};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use x509_cert::der::{Decode, Encode};

use crate::DtlsError;

/// 自分の証明書と秘密鍵。Debug に秘密鍵を出さない。
#[derive(Clone)]
pub struct DtlsCertificate {
    der: Vec<u8>,
    key: SigningKey,
    fingerprint: [u8; 32],
}

impl core::fmt::Debug for DtlsCertificate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DtlsCertificate")
            .field("fingerprint", &format_fingerprint(&self.fingerprint))
            .finish_non_exhaustive()
    }
}

fn random_bytes<const N: usize>() -> Result<[u8; N], DtlsError> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|_| DtlsError::RandomUnavailable)?;
    Ok(b)
}

/// OS の乱数から P-256 の秘密鍵を作る。
pub(crate) fn random_p256_secret() -> Result<p256::SecretKey, DtlsError> {
    // 範囲外（0 や群の位数以上）の値は確率 2^-32 程度。数回やり直す。
    for _ in 0..8 {
        let mut b = zeroize::Zeroizing::new(random_bytes::<32>()?);
        if let Ok(sk) = p256::SecretKey::from_slice(&b[..]) {
            zeroize::Zeroize::zeroize(&mut *b);
            return Ok(sk);
        }
    }
    Err(DtlsError::RandomUnavailable)
}

impl DtlsCertificate {
    /// ECDSA P-256 の鍵を作り、自己署名証明書を作る。有効期間は 30 日。
    pub fn generate_self_signed(common_name: &str) -> Result<Self, DtlsError> {
        Self::generate_self_signed_with_validity(common_name, Duration::from_secs(30 * 86400))
    }

    /// 有効期間を指定して自己署名証明書を作る。
    pub fn generate_self_signed_with_validity(
        common_name: &str,
        validity: Duration,
    ) -> Result<Self, DtlsError> {
        use std::str::FromStr;
        use x509_cert::builder::{Builder, CertificateBuilder, Profile};
        use x509_cert::name::Name;
        use x509_cert::serial_number::SerialNumber;
        use x509_cert::spki::SubjectPublicKeyInfoOwned;
        use x509_cert::time::Validity;

        let key = SigningKey::from(random_p256_secret()?);
        let mut serial = random_bytes::<16>()?;
        serial[0] = (serial[0] & 0x7f) | 0x01;
        let serial = SerialNumber::new(&serial).map_err(|_| DtlsError::Certificate("serial"))?;
        let validity =
            Validity::from_now(validity).map_err(|_| DtlsError::Certificate("validity"))?;
        let cn: String = common_name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
            .collect();
        let cn = if cn.is_empty() {
            "senca".to_string()
        } else {
            cn
        };
        let subject =
            Name::from_str(&format!("CN={cn}")).map_err(|_| DtlsError::Certificate("subject"))?;
        let spki_der = key
            .verifying_key()
            .to_public_key_der()
            .map_err(|_| DtlsError::Certificate("spki"))?;
        let spki = SubjectPublicKeyInfoOwned::from_der(spki_der.as_bytes())
            .map_err(|_| DtlsError::Certificate("spki"))?;
        // 自己署名の末端証明書（CA ではない、digitalSignature）。
        let profile = Profile::Leaf {
            issuer: subject.clone(),
            enable_key_agreement: false,
            enable_key_encipherment: false,
        };
        let builder = CertificateBuilder::new(profile, serial, validity, subject, spki, &key)
            .map_err(|_| DtlsError::Certificate("builder"))?;
        let cert = builder
            .build::<p256::ecdsa::DerSignature>()
            .map_err(|_| DtlsError::Certificate("sign"))?;
        let der = cert
            .to_der()
            .map_err(|_| DtlsError::Certificate("encode"))?;
        Ok(Self::assemble(der, key))
    }

    /// DER の証明書と PKCS#8 DER の秘密鍵から作る。公開鍵が一致しなければ誤り。
    pub fn from_der(cert_der: &[u8], pkcs8_key_der: &[u8]) -> Result<Self, DtlsError> {
        let key = SigningKey::from_pkcs8_der(pkcs8_key_der)
            .map_err(|_| DtlsError::Certificate("private key"))?;
        let public = certificate_public_key(cert_der)?;
        if public != *key.verifying_key() {
            return Err(DtlsError::Certificate("key mismatch"));
        }
        Ok(Self::assemble(cert_der.to_vec(), key))
    }

    /// PEM の証明書と PKCS#8 PEM の秘密鍵から作る。
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self, DtlsError> {
        use x509_cert::der::DecodePem;
        let cert = x509_cert::Certificate::from_pem(cert_pem)
            .map_err(|_| DtlsError::Certificate("pem"))?;
        let der = cert.to_der().map_err(|_| DtlsError::Certificate("pem"))?;
        let key = SigningKey::from_pkcs8_pem(key_pem)
            .map_err(|_| DtlsError::Certificate("private key"))?;
        let public = certificate_public_key(&der)?;
        if public != *key.verifying_key() {
            return Err(DtlsError::Certificate("key mismatch"));
        }
        Ok(Self::assemble(der, key))
    }

    fn assemble(der: Vec<u8>, key: SigningKey) -> Self {
        let fingerprint = Sha256::digest(&der).into();
        Self {
            der,
            key,
            fingerprint,
        }
    }

    /// 証明書の DER。
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// 証明書の SHA-256 fingerprint。
    pub fn fingerprint_sha256(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// SDP の `a=fingerprint:` の値（`sha-256 AB:CD:...`）。
    pub fn sdp_fingerprint(&self) -> String {
        format!("sha-256 {}", format_fingerprint(&self.fingerprint))
    }

    pub(crate) fn signing_key(&self) -> &SigningKey {
        &self.key
    }
}

/// 証明書（DER）から ECDSA P-256 の公開鍵を取り出す。P-256 以外は誤り。
pub(crate) fn certificate_public_key(der: &[u8]) -> Result<VerifyingKey, DtlsError> {
    let cert =
        x509_cert::Certificate::from_der(der).map_err(|_| DtlsError::Certificate("parse"))?;
    let spki = cert
        .tbs_certificate
        .subject_public_key_info
        .to_der()
        .map_err(|_| DtlsError::Certificate("spki"))?;
    // id-ecPublicKey + prime256v1 でなければ失敗する。
    VerifyingKey::from_public_key_der(&spki).map_err(|_| DtlsError::Certificate("unsupported key"))
}

/// fingerprint を `AB:CD:..` の形にする。
pub fn format_fingerprint(fp: &[u8]) -> String {
    fp.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// `AB:CD:..`（大文字小文字は問わない）を 32 byte に戻す。先頭の `sha-256 ` は許す。
pub fn parse_fingerprint(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    let s = s
        .strip_prefix("sha-256")
        .or_else(|| s.strip_prefix("SHA-256"))
        .map(str::trim_start)
        .unwrap_or(s);
    let mut out = [0u8; 32];
    let mut n = 0;
    for part in s.split(':') {
        if n >= 32 || part.len() != 2 {
            return None;
        }
        out[n] = u8::from_str_radix(part, 16).ok()?;
        n += 1;
    }
    (n == 32).then_some(out)
}

/// 定数時間で fingerprint を比べる。
pub fn fingerprint_matches(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_parse() {
        let c = DtlsCertificate::generate_self_signed("senca-test").unwrap();
        let pk = certificate_public_key(c.der()).unwrap();
        assert_eq!(pk, *c.signing_key().verifying_key());
        let s = c.sdp_fingerprint();
        assert_eq!(parse_fingerprint(&s), Some(c.fingerprint_sha256()));
        assert!(fingerprint_matches(
            &parse_fingerprint(&s).unwrap(),
            &c.fingerprint_sha256()
        ));
        assert!(parse_fingerprint("AB:CD").is_none());
        assert!(certificate_public_key(&c.der()[..c.der().len() - 1]).is_err());
    }
}
