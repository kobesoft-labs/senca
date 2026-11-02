//! 秘密値の保管（仕様 9.3）。値はノードの鍵（`control/secret-key`、0600、32 バイト）で AES-256-GCM により封じる。
//! 制御状態・ログ・バックアップには封じた形だけが残る。分散（M9）では鍵を各ノードへ安全に配る必要がある（KMS へ置き換え可能）。

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::path::Path;

pub struct SecretBox {
    cipher: Aes256Gcm,
}

impl SecretBox {
    /// 鍵を読む。無ければ作る（0600、create_new）。
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
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "bad key".to_string())?;
        Ok(SecretBox { cipher })
    }

    /// 封じる。`aad` には tenant と ID を入れる（別の資源へ写しても開けないように）。
    pub fn seal(&self, plain: &[u8], aad: &str) -> Result<String, String> {
        let mut n = [0u8; 12];
        getrandom::fill(&mut n).map_err(|e| e.to_string())?;
        let ct = self
            .cipher
            .encrypt(&Nonce::from(n), aes_gcm::aead::Payload { msg: plain, aad: aad.as_bytes() })
            .map_err(|_| "seal failed".to_string())?;
        let mut out = n.to_vec();
        out.extend_from_slice(&ct);
        Ok(STANDARD.encode(out))
    }

    pub fn open_sealed(&self, sealed: &str, aad: &str) -> Option<Vec<u8>> {
        let b = STANDARD.decode(sealed).ok()?;
        if b.len() < 12 + 16 {
            return None;
        }
        self.cipher
            .decrypt(&Nonce::from(<[u8; 12]>::try_from(&b[..12]).ok()?), aes_gcm::aead::Payload { msg: &b[12..], aad: aad.as_bytes() })
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_aad() {
        let p = std::env::temp_dir().join(format!("senca-sk-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let b = SecretBox::open(&p).unwrap();
        let s = b.seal(b"pw", "t/1").unwrap();
        assert_eq!(b.open_sealed(&s, "t/1").unwrap(), b"pw");
        assert!(b.open_sealed(&s, "t/2").is_none());
        let b2 = SecretBox::open(&p).unwrap();
        assert_eq!(b2.open_sealed(&s, "t/1").unwrap(), b"pw");
        std::fs::remove_file(&p).unwrap();
    }
}
