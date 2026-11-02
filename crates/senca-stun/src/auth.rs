//! 資格情報と鍵（RFC 8489 9 節）。
//!
//! 文字列の正規化（OpaqueString・SASLprep）はしない。ASCII の利用者名・パスワードでは結果が同じになる。

use md5::{Digest as _, Md5};
use sha2::Sha256;

use crate::attr::PasswordAlgorithm;

/// 短期資格情報の鍵（RFC 8489 9.1.1: パスワードそのもの）。ICE の connectivity check で使う。
pub fn short_term_key(password: &str) -> Vec<u8> {
    password.as_bytes().to_vec()
}

/// 長期資格情報の鍵（RFC 8489 9.2.2）。MD5 なら 16 バイト、SHA-256 なら 32 バイト。
/// 未知のアルゴリズムは MD5 として扱う（呼び出し側が PASSWORD-ALGORITHMS で選んだものだけを渡す）。
pub fn long_term_key(
    username: &str,
    realm: &str,
    password: &str,
    alg: PasswordAlgorithm,
) -> Vec<u8> {
    match alg {
        PasswordAlgorithm::Sha256 => {
            let mut h = Sha256::new();
            h.update(username.as_bytes());
            h.update(b":");
            h.update(realm.as_bytes());
            h.update(b":");
            h.update(password.as_bytes());
            h.finalize().to_vec()
        }
        _ => {
            let mut h = Md5::new();
            h.update(username.as_bytes());
            h.update(b":");
            h.update(realm.as_bytes());
            h.update(b":");
            h.update(password.as_bytes());
            h.finalize().to_vec()
        }
    }
}

/// USERHASH（RFC 8489 14.4: SHA-256(username ":" realm)）。
pub fn userhash(username: &str, realm: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(username.as_bytes());
    h.update(b":");
    h.update(realm.as_bytes());
    h.finalize().into()
}
