//! 秘密値の保管（仕様 9.3、ADR-0009）。値は AES-256-GCM で封じ、制御状態・ログ・バックアップには封じた形だけが残る。
//!
//! - **単一ノード**: 鍵はノードの `control/secret-key`（0600、32 バイト、無ければ作る）。
//! - **分散**: 鍵はクラスタ共通の**データ鍵**。データ鍵は、クラスタの合言葉から HKDF-SHA256 で導いた鍵（KEK）で
//!   AES-256-GCM により包み、制御状態（`secret_data_key`）で全ノードへ複製する。最初に適用された包みが使われる
//!   （どのノードも、まだ無ければ自分の `secret-key` を包んで提案する）。各ノードは状態の包みを開いてデータ鍵を得る。
//! - **移行**: 以前の版の分散では、秘密値は受けたノードの `secret-key` で封じていた。開くときはデータ鍵の次に手元の
//!   `secret-key` も試し、手元の鍵でしか開けない値は、データ鍵で封じ直して置き換える（[`SecretBox::reseal`]）。
//!   単一ノードは今までどおり（`secret-key` がそのまま鍵）。
//!
//! 合言葉を知らない者は、制御状態の写し（バックアップ・Raft のログ）だけからは秘密値を開けない。KMS は未了。

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::path::Path;
use std::sync::Mutex;

/// 包みの AAD（別の用途の暗号文と取り違えない）。
const WRAP_AAD: &[u8] = b"senca/secret-data-key/v1";

pub struct SecretBox {
    /// ノードの鍵（`control/secret-key`）。単一ノードでは封じる鍵、分散では移行のための古い鍵。
    local: [u8; 32],
    /// 分散のとき: クラスタの合言葉から導いた包みの鍵。
    kek: Option<[u8; 32]>,
    /// 開いたデータ鍵（包みの文字列と組で覚える）。
    data: Mutex<Option<(String, [u8; 32])>>,
}

/// 秘密値を封じられない理由。
#[derive(Debug, PartialEq, Eq)]
pub enum SealError {
    /// 分散で、データ鍵がまだ届いていない（少し待ってやり直す）。
    NotReady,
    /// 包みを開けない（合言葉が違う）。
    BadKey,
    Internal,
}

fn seal_with(key: &[u8; 32], plain: &[u8], aad: &[u8]) -> Result<String, SealError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| SealError::Internal)?;
    let mut n = [0u8; 12];
    getrandom::fill(&mut n).map_err(|_| SealError::Internal)?;
    let ct = cipher.encrypt(&Nonce::from(n), aes_gcm::aead::Payload { msg: plain, aad }).map_err(|_| SealError::Internal)?;
    let mut out = n.to_vec();
    out.extend_from_slice(&ct);
    Ok(STANDARD.encode(out))
}

fn open_with(key: &[u8; 32], sealed: &str, aad: &[u8]) -> Option<Vec<u8>> {
    // 封じた値は秘密値（8 KiB まで）か鍵だけ。大きすぎるものは開かない
    if sealed.len() > 16 * 1024 {
        return None;
    }
    let b = STANDARD.decode(sealed).ok()?;
    if b.len() < 12 + 16 {
        return None;
    }
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    cipher.decrypt(&Nonce::from(<[u8; 12]>::try_from(&b[..12]).ok()?), aes_gcm::aead::Payload { msg: &b[12..], aad }).ok()
}

/// クラスタの合言葉から包みの鍵を導く（用途の文字列でノード間の認証の鍵と分ける）。
fn derive_kek(cluster_secret: &str) -> [u8; 32] {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(b"senca-cluster/v1"), cluster_secret.as_bytes());
    let mut k = [0u8; 32];
    hk.expand(b"secret-data-key-wrap", &mut k).expect("32 bytes is a valid HKDF-SHA256 length");
    k
}

impl SecretBox {
    /// ノードの鍵を読む。無ければ作る（0600、create_new）。
    pub fn open(path: &Path) -> Result<SecretBox, String> {
        let key = match std::fs::read(path) {
            Ok(k) if k.len() == 32 => k,
            Ok(_) => return Err(format!("{}: 鍵の長さが違います", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut k = vec![0u8; 32];
                getrandom::fill(&mut k).map_err(|e| e.to_string())?;
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                f.write_all(&k).and_then(|_| f.sync_all()).map_err(|e| e.to_string())?;
                k
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let local: [u8; 32] = key.try_into().map_err(|_| "bad key".to_string())?;
        Ok(SecretBox { local, kek: None, data: Mutex::new(None) })
    }

    /// 分散で使う（クラスタの合言葉から包みの鍵を導く）。
    pub fn with_cluster_secret(mut self, cluster_secret: &str) -> SecretBox {
        self.kek = Some(derive_kek(cluster_secret));
        self
    }

    pub fn clustered(&self) -> bool {
        self.kek.is_some()
    }

    /// 分散: このノードの鍵をデータ鍵として包む（状態にまだデータ鍵が無いときに提案する）。
    pub fn wrap_local(&self) -> Result<String, SealError> {
        let kek = self.kek.as_ref().ok_or(SealError::Internal)?;
        seal_with(kek, &self.local, WRAP_AAD)
    }

    /// 状態の包み（`secret_data_key`）からデータ鍵を得る（開いた鍵は覚えておく）。
    fn data_key(&self, wrapped: Option<&str>) -> Result<[u8; 32], SealError> {
        let (Some(kek), Some(w)) = (self.kek.as_ref(), wrapped) else { return Err(SealError::NotReady) };
        let mut cur = self.data.lock().unwrap();
        if let Some((cw, k)) = cur.as_ref()
            && cw == w
        {
            return Ok(*k);
        }
        let k: [u8; 32] = open_with(kek, w, WRAP_AAD).and_then(|v| v.try_into().ok()).ok_or(SealError::BadKey)?;
        *cur = Some((w.to_string(), k));
        Ok(k)
    }

    /// 分散で、状態の包みを開けるか（合言葉が合っているか）。
    pub fn check(&self, wrapped: Option<&str>) -> Result<(), SealError> {
        if !self.clustered() {
            return Ok(());
        }
        self.data_key(wrapped).map(|_| ())
    }

    /// 封じる。`aad` には tenant と ID を入れる（別の資源へ写しても開けないように）。
    /// `wrapped` は制御状態の `secret_data_key`（単一ノードでは使わない）。
    pub fn seal(&self, wrapped: Option<&str>, plain: &[u8], aad: &str) -> Result<String, SealError> {
        if self.clustered() {
            let k = self.data_key(wrapped)?;
            seal_with(&k, plain, aad.as_bytes())
        } else {
            seal_with(&self.local, plain, aad.as_bytes())
        }
    }

    /// 開く。分散ではデータ鍵、次に手元の鍵（以前の版で封じた値）を試す。
    pub fn open_sealed(&self, wrapped: Option<&str>, sealed: &str, aad: &str) -> Option<Vec<u8>> {
        if self.clustered()
            && let Ok(k) = self.data_key(wrapped)
            && let Some(v) = open_with(&k, sealed, aad.as_bytes())
        {
            return Some(v);
        }
        open_with(&self.local, sealed, aad.as_bytes())
    }

    /// 分散の移行: 手元の鍵でだけ開ける値を、データ鍵で封じ直した値（データ鍵で開けるか、手元の鍵でも開けなければ None）。
    pub fn reseal(&self, wrapped: Option<&str>, sealed: &str, aad: &str) -> Option<String> {
        let k = self.data_key(wrapped).ok()?;
        if open_with(&k, sealed, aad.as_bytes()).is_some() {
            return None;
        }
        let v = open_with(&self.local, sealed, aad.as_bytes())?;
        seal_with(&k, &v, aad.as_bytes()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("senca-sk-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn roundtrip_and_aad() {
        let p = tmp("single");
        let b = SecretBox::open(&p).unwrap();
        let s = b.seal(None, b"pw", "t/1").unwrap();
        assert_eq!(b.open_sealed(None, &s, "t/1").unwrap(), b"pw");
        assert!(b.open_sealed(None, &s, "t/2").is_none());
        let b2 = SecretBox::open(&p).unwrap();
        assert_eq!(b2.open_sealed(None, &s, "t/1").unwrap(), b"pw");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn cluster_wide_data_key() {
        let (p1, p2) = (tmp("n1"), tmp("n2"));
        let n1 = SecretBox::open(&p1).unwrap().with_cluster_secret("0123456789abcdef-secret");
        let n2 = SecretBox::open(&p2).unwrap().with_cluster_secret("0123456789abcdef-secret");
        // まだデータ鍵が無い: 封じられない
        assert_eq!(n2.seal(None, b"x", "t/1"), Err(SealError::NotReady));
        // 以前の版で n2 が手元の鍵で封じた値（移行の元）
        let legacy = SecretBox::open(&p2).unwrap().seal(None, b"old", "t/9").unwrap();
        // n1 の包みが状態に入った
        let w = n1.wrap_local().unwrap();
        let s = n2.seal(Some(&w), b"pw", "t/1").unwrap();
        assert_eq!(n1.open_sealed(Some(&w), &s, "t/1").unwrap(), b"pw", "別のノードで封じた値を開ける");
        // 古い値: n2 は手元の鍵で開ける。封じ直すと n1 でも開ける
        assert_eq!(n2.open_sealed(Some(&w), &legacy, "t/9").unwrap(), b"old");
        assert!(n1.open_sealed(Some(&w), &legacy, "t/9").is_none());
        let re = n2.reseal(Some(&w), &legacy, "t/9").unwrap();
        assert_eq!(n1.open_sealed(Some(&w), &re, "t/9").unwrap(), b"old");
        assert!(n2.reseal(Some(&w), &re, "t/9").is_none(), "封じ直した値はそのまま");
        // 合言葉が違うノードは包みを開けない
        let bad = SecretBox::open(&p2).unwrap().with_cluster_secret("another-secret-0000");
        assert_eq!(bad.check(Some(&w)), Err(SealError::BadKey));
        assert!(bad.open_sealed(Some(&w), &s, "t/1").is_none());
        std::fs::remove_file(&p1).unwrap();
        std::fs::remove_file(&p2).unwrap();
    }
}
