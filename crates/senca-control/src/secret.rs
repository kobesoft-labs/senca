//! 秘密値（API トークン・publish 鍵）の生成と照合。状態には SHA-256 だけを残す。

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// 表示用に伏せる文字列。Debug・Display で中身を出さない。
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: String) -> Self {
        Secret(s)
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// `prefix` + base64url(32 バイトの乱数)。乱数源が使えなければ `Err`（仕様 9.3）。
pub fn generate(prefix: &str) -> Result<Secret, getrandom::Error> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b)?;
    Ok(Secret(format!("{prefix}{}", URL_SAFE_NO_PAD.encode(b))))
}

/// 保存用のハッシュ（小文字 16 進）。値は 256 bit のエントロピーがあるので遅いハッシュは不要。
pub fn hash(value: &str) -> String {
    hex(&Sha256::digest(value.as_bytes()))
}

/// 定数時間で照合する。
pub fn verify(value: &str, stored_hash: &str) -> bool {
    let h = hash(value);
    h.as_bytes().ct_eq(stored_hash.as_bytes()).into()
}

pub fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_verify() {
        let s = generate("senca_").unwrap();
        assert!(s.expose().starts_with("senca_"));
        assert_eq!(s.expose().len(), 6 + 43);
        let h = hash(s.expose());
        assert!(verify(s.expose(), &h));
        assert!(!verify("senca_x", &h));
        assert_eq!(format!("{s:?}"), "Secret(***)");
    }
}
