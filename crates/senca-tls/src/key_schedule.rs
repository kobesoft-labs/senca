//! 鍵スケジュール（RFC 8446 7.1）。
//!
//! ```text
//! early  = HKDF-Extract(0, PSK or 0)
//! hs     = HKDF-Extract(Derive-Secret(early, "derived", ""), (EC)DHE)
//! master = HKDF-Extract(Derive-Secret(hs, "derived", ""), 0)
//! ```

use subtle::ConstantTimeEq;

use crate::crypto::{HashAlg, Secret, hkdf_expand_label};
use crate::error::{AlertDescription, Error};

/// Derive-Secret(secret, label, transcript_hash)。
pub(crate) fn derive_secret(
    hash: HashAlg,
    secret: &Secret,
    label: &[u8],
    transcript_hash: &[u8],
) -> Result<Secret, Error> {
    hkdf_expand_label(hash, secret.as_bytes(), label, transcript_hash, hash.output_len())
}

/// Finished の verify_data（RFC 8446 4.4.4）。PSK binder にも使う。
pub(crate) fn finished_mac(hash: HashAlg, base_key: &Secret, transcript_hash: &[u8]) -> Result<Vec<u8>, Error> {
    let fk = hkdf_expand_label(hash, base_key.as_bytes(), b"finished", &[], hash.output_len())?;
    hash.hmac(fk.as_bytes(), transcript_hash)
}

/// verify_data を定数時間で比べる。
pub(crate) fn check_finished(
    hash: HashAlg,
    base_key: &Secret,
    transcript_hash: &[u8],
    received: &[u8],
) -> Result<(), Error> {
    let expected = finished_mac(hash, base_key, transcript_hash)?;
    if expected.len() == received.len() && bool::from(expected.ct_eq(received)) {
        Ok(())
    } else {
        Err(Error::fatal(AlertDescription::DECRYPT_ERROR, "Finished の検証に失敗"))
    }
}

/// application traffic secret の更新（RFC 8446 7.2）。
pub(crate) fn next_traffic_secret(hash: HashAlg, secret: &Secret) -> Result<Secret, Error> {
    hkdf_expand_label(hash, secret.as_bytes(), b"traffic upd", &[], hash.output_len())
}

/// 再開用 PSK（RFC 8446 4.6.1）。
pub(crate) fn resumption_psk(hash: HashAlg, resumption_master: &Secret, nonce: &[u8]) -> Result<Secret, Error> {
    hkdf_expand_label(hash, resumption_master.as_bytes(), b"resumption", nonce, hash.output_len())
}

/// 段階を進める鍵スケジュール。
pub(crate) struct KeySchedule {
    hash: HashAlg,
    current: Secret,
}

impl KeySchedule {
    /// early secret を作る。PSK がなければ hash 長のゼロ列。
    pub(crate) fn early(hash: HashAlg, psk: Option<&Secret>) -> Self {
        let zeros = vec![0u8; hash.output_len()];
        let ikm = psk.map(|p| p.as_bytes()).unwrap_or(&zeros);
        Self {
            hash,
            current: hash.extract(&zeros, ikm),
        }
    }

    #[cfg(test)]
    pub(crate) fn current(&self) -> &Secret {
        &self.current
    }

    /// resumption binder key（early 段階で使う）。
    pub(crate) fn resumption_binder_key(&self) -> Result<Secret, Error> {
        derive_secret(self.hash, &self.current, b"res binder", &self.hash.hash(b""))
    }

    /// 次の段階へ。`ikm` が `None` なら hash 長のゼロ列（master 段階）。
    pub(crate) fn advance(&mut self, ikm: Option<&Secret>) -> Result<(), Error> {
        let derived = derive_secret(self.hash, &self.current, b"derived", &self.hash.hash(b""))?;
        let zeros = vec![0u8; self.hash.output_len()];
        let ikm = ikm.map(|s| s.as_bytes()).unwrap_or(&zeros);
        self.current = self.hash.extract(derived.as_bytes(), ikm);
        Ok(())
    }

    pub(crate) fn derive(&self, label: &[u8], transcript_hash: &[u8]) -> Result<Secret, Error> {
        derive_secret(self.hash, &self.current, label, transcript_hash)
    }
}

/// TLS-Exporter（RFC 8446 7.5）。
pub(crate) fn export(
    hash: HashAlg,
    exporter_master: &Secret,
    label: &[u8],
    context: &[u8],
    len: usize,
) -> Result<Vec<u8>, Error> {
    let s = derive_secret(hash, exporter_master, label, &hash.hash(b""))?;
    Ok(hkdf_expand_label(hash, s.as_bytes(), b"exporter", &hash.hash(context), len)?
        .as_bytes()
        .to_vec())
}
