//! record 層（RFC 6347 4.1）と AEAD による保護（RFC 5288・RFC 7905）。

use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes128Gcm, Aes256Gcm, KeyInit};
use chacha20poly1305::ChaCha20Poly1305;
use zeroize::Zeroizing;

use crate::CipherSuite;
use crate::codec::{Reader, put_u16, put_u48};

pub(crate) const CT_CHANGE_CIPHER_SPEC: u8 = 20;
pub(crate) const CT_ALERT: u8 = 21;
pub(crate) const CT_HANDSHAKE: u8 = 22;
pub(crate) const CT_APPLICATION_DATA: u8 = 23;

/// DTLS 1.2 の版。
pub(crate) const DTLS12: u16 = 0xFEFD;
/// DTLS 1.0 の版（最初の ClientHello と HelloVerifyRequest の record で使われることがある）。
pub(crate) const DTLS10: u16 = 0xFEFF;

pub(crate) const RECORD_HEADER_LEN: usize = 13;
/// 平文の上限（2^14）。
pub(crate) const MAX_PLAINTEXT: usize = 1 << 14;
/// 暗号文の上限（2^14 + 2048）。
const MAX_CIPHERTEXT: usize = MAX_PLAINTEXT + 2048;
/// 48 bit の sequence number の上限。
pub(crate) const MAX_SEQ: u64 = (1 << 48) - 1;

/// 受信した record。
pub(crate) struct Record<'a> {
    pub content_type: u8,
    pub version: u16,
    pub epoch: u16,
    pub seq: u64,
    pub body: &'a [u8],
    /// header を含む record 全体（後で処理するために取っておく場合）。
    pub raw: &'a [u8],
}

/// datagram を record に分ける。壊れた残りは捨てる（RFC 6347 4.1.2.7）。
pub(crate) fn split_records(datagram: &[u8]) -> Vec<Record<'_>> {
    let mut out = Vec::new();
    let mut r = Reader::new(datagram);
    while r.remaining() >= RECORD_HEADER_LEN {
        let start = datagram.len() - r.remaining();
        let parsed = (|| {
            let content_type = r.u8()?;
            let version = r.u16()?;
            let epoch = r.u16()?;
            let seq = r.u48()?;
            let body = r.vec16()?;
            Ok::<_, crate::codec::DecodeError>(Record {
                content_type,
                version,
                epoch,
                seq,
                body,
                raw: &[],
            })
        })();
        match parsed {
            Ok(mut rec) => {
                if rec.body.len() > MAX_CIPHERTEXT {
                    break;
                }
                let end = datagram.len() - r.remaining();
                rec.raw = &datagram[start..end];
                out.push(rec);
            }
            Err(_) => break,
        }
    }
    out
}

pub(crate) fn encode_header(
    out: &mut Vec<u8>,
    content_type: u8,
    version: u16,
    epoch: u16,
    seq: u64,
    len: usize,
) {
    out.push(content_type);
    put_u16(out, version);
    put_u16(out, epoch);
    put_u48(out, seq);
    put_u16(out, len as u16);
}

enum Aead {
    Aes128(Box<Aes128Gcm>),
    Aes256(Box<Aes256Gcm>),
    Chacha(Box<ChaCha20Poly1305>),
}

/// 1 方向の epoch の鍵。
pub(crate) struct CipherState {
    aead: Aead,
    iv: Zeroizing<[u8; 12]>,
    suite: CipherSuite,
}

fn aad(epoch: u16, seq: u64, content_type: u8, version: u16, len: usize) -> [u8; 13] {
    let mut a = [0u8; 13];
    a[0..2].copy_from_slice(&epoch.to_be_bytes());
    a[2..8].copy_from_slice(&seq.to_be_bytes()[2..]);
    a[8] = content_type;
    a[9..11].copy_from_slice(&version.to_be_bytes());
    a[11..13].copy_from_slice(&(len as u16).to_be_bytes());
    a
}

impl CipherState {
    pub(crate) fn new(suite: CipherSuite, key: &[u8], fixed_iv: &[u8]) -> Self {
        let aead = match suite {
            CipherSuite::EcdheEcdsaAes128GcmSha256 => Aead::Aes128(Box::new(
                Aes128Gcm::new_from_slice(key).expect("鍵長は suite で決まる"),
            )),
            CipherSuite::EcdheEcdsaAes256GcmSha384 => Aead::Aes256(Box::new(
                Aes256Gcm::new_from_slice(key).expect("鍵長は suite で決まる"),
            )),
            CipherSuite::EcdheEcdsaChacha20Poly1305Sha256 => Aead::Chacha(Box::new(
                ChaCha20Poly1305::new_from_slice(key).expect("鍵長は suite で決まる"),
            )),
        };
        let mut iv = Zeroizing::new([0u8; 12]);
        iv[..fixed_iv.len()].copy_from_slice(fixed_iv);
        Self { aead, iv, suite }
    }

    pub(crate) fn overhead(&self) -> usize {
        self.suite.overhead()
    }

    /// nonce: GCM は fixed_iv(4) || explicit(8)、ChaCha20 は iv XOR (0^32 || epoch || seq)。
    fn nonce(&self, explicit: &[u8; 8]) -> [u8; 12] {
        let mut n = *self.iv;
        match self.suite {
            CipherSuite::EcdheEcdsaChacha20Poly1305Sha256 => {
                for (d, s) in n[4..].iter_mut().zip(explicit) {
                    *d ^= s;
                }
            }
            _ => n[4..].copy_from_slice(explicit),
        }
        n
    }

    fn seal_in_place(&self, nonce: &[u8; 12], aad: &[u8], buf: &mut [u8]) -> [u8; 16] {
        let nonce = aes_gcm::Nonce::from(*nonce);
        let tag = match &self.aead {
            Aead::Aes128(c) => c.encrypt_in_place_detached(&nonce, aad, buf),
            Aead::Aes256(c) => c.encrypt_in_place_detached(&nonce, aad, buf),
            Aead::Chacha(c) => c.encrypt_in_place_detached(&nonce, aad, buf),
        };
        // 長さは 2^14 以下に制限しており、AEAD の上限には届かない。
        tag.expect("AEAD の長さ上限に届かない").into()
    }

    fn open_in_place(&self, nonce: &[u8; 12], aad: &[u8], buf: &mut [u8], tag: &[u8; 16]) -> bool {
        let nonce = aes_gcm::Nonce::from(*nonce);
        let tag = aes_gcm::Tag::from(*tag);
        match &self.aead {
            Aead::Aes128(c) => c.decrypt_in_place_detached(&nonce, aad, buf, &tag).is_ok(),
            Aead::Aes256(c) => c.decrypt_in_place_detached(&nonce, aad, buf, &tag).is_ok(),
            Aead::Chacha(c) => c.decrypt_in_place_detached(&nonce, aad, buf, &tag).is_ok(),
        }
    }

    /// 平文を暗号化して record の本体（explicit nonce・暗号文・tag）を返す。
    pub(crate) fn seal(&self, epoch: u16, seq: u64, content_type: u8, plaintext: &[u8]) -> Vec<u8> {
        let mut explicit = [0u8; 8];
        explicit[..2].copy_from_slice(&epoch.to_be_bytes());
        explicit[2..].copy_from_slice(&seq.to_be_bytes()[2..]);
        let nonce = self.nonce(&explicit);
        let a = aad(
            epoch,
            seq,
            content_type,
            crate::record::DTLS12,
            plaintext.len(),
        );
        let mut out = Vec::with_capacity(plaintext.len() + self.overhead());
        let explicit_len = if self.suite == CipherSuite::EcdheEcdsaChacha20Poly1305Sha256 {
            0
        } else {
            8
        };
        out.extend_from_slice(&explicit[..explicit_len]);
        out.extend_from_slice(plaintext);
        let tag = self.seal_in_place(&nonce, &a, &mut out[explicit_len..]);
        out.extend_from_slice(&tag);
        out
    }

    /// record の本体を認証して復号する。失敗は `None`（呼び出し側は黙って捨てる）。
    pub(crate) fn open(
        &self,
        epoch: u16,
        seq: u64,
        content_type: u8,
        version: u16,
        body: &[u8],
    ) -> Option<Vec<u8>> {
        let chacha = self.suite == CipherSuite::EcdheEcdsaChacha20Poly1305Sha256;
        let explicit_len = if chacha { 0 } else { 8 };
        if body.len() < explicit_len + 16 {
            return None;
        }
        let mut explicit = [0u8; 8];
        if chacha {
            explicit[..2].copy_from_slice(&epoch.to_be_bytes());
            explicit[2..].copy_from_slice(&seq.to_be_bytes()[2..]);
        } else {
            explicit.copy_from_slice(&body[..8]);
        }
        let nonce = self.nonce(&explicit);
        let ct = &body[explicit_len..body.len() - 16];
        if ct.len() > MAX_PLAINTEXT {
            return None;
        }
        let mut tag = [0u8; 16];
        tag.copy_from_slice(&body[body.len() - 16..]);
        let a = aad(epoch, seq, content_type, version, ct.len());
        let mut buf = ct.to_vec();
        self.open_in_place(&nonce, &a, &mut buf, &tag)
            .then_some(buf)
    }
}

/// DTLS の replay window（RFC 6347 4.1.2.6、幅 64）。
#[derive(Default)]
pub(crate) struct ReplayWindow {
    top: u64,
    bits: u64,
    any: bool,
}

impl ReplayWindow {
    pub(crate) fn is_fresh(&self, seq: u64) -> bool {
        if !self.any || seq > self.top {
            return true;
        }
        let d = self.top - seq;
        d < 64 && self.bits & (1u64 << d) == 0
    }

    pub(crate) fn accept(&mut self, seq: u64) {
        if !self.any {
            self.any = true;
            self.top = seq;
            self.bits = 1;
        } else if seq > self.top {
            let d = seq - self.top;
            self.bits = if d >= 64 { 0 } else { self.bits << d };
            self.bits |= 1;
            self.top = seq;
        } else {
            let d = self.top - seq;
            if d < 64 {
                self.bits |= 1u64 << d;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_all_suites() {
        for suite in [
            CipherSuite::EcdheEcdsaAes128GcmSha256,
            CipherSuite::EcdheEcdsaAes256GcmSha384,
            CipherSuite::EcdheEcdsaChacha20Poly1305Sha256,
        ] {
            let key = vec![7u8; suite.key_len()];
            let iv = vec![3u8; suite.fixed_iv_len()];
            let c = CipherState::new(suite, &key, &iv);
            let body = c.seal(1, 5, CT_APPLICATION_DATA, b"hello");
            assert_eq!(body.len(), 5 + suite.overhead());
            assert_eq!(
                c.open(1, 5, CT_APPLICATION_DATA, DTLS12, &body).unwrap(),
                b"hello"
            );
            // 改変・別 seq・別 type は通らない
            for i in 0..body.len() {
                let mut b = body.clone();
                b[i] ^= 0x40;
                assert!(
                    c.open(1, 5, CT_APPLICATION_DATA, DTLS12, &b).is_none(),
                    "{suite:?} {i}"
                );
            }
            assert!(c.open(1, 6, CT_APPLICATION_DATA, DTLS12, &body).is_none());
            assert!(c.open(1, 5, CT_HANDSHAKE, DTLS12, &body).is_none());
        }
    }

    #[test]
    fn replay_window() {
        let mut w = ReplayWindow::default();
        assert!(w.is_fresh(0));
        w.accept(0);
        assert!(!w.is_fresh(0));
        w.accept(100);
        assert!(!w.is_fresh(36));
        assert!(w.is_fresh(37));
        w.accept(37);
        assert!(!w.is_fresh(37));
        assert!(w.is_fresh(99));
    }
}
