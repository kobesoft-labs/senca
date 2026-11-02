//! TURN の一時資格情報（REST API 方式、coturn の `use-auth-secret`）。
//!
//! 利用者名は `<有効期限の UNIX 秒>:<利用者 ID>`、パスワードは
//! `base64(HMAC-SHA1(共有秘密, 利用者名))`。

use hmac::{Hmac, Mac};

/// 共有秘密と利用者名からパスワードを作る。
pub fn rest_api_password(secret: &[u8], username: &str) -> String {
    let mut m =
        <Hmac<sha1::Sha1> as Mac>::new_from_slice(secret).expect("HMAC accepts any key length");
    m.update(username.as_bytes());
    base64(&m.finalize().into_bytes())
}

/// 有効期限 `expires_unix`（UNIX 秒）の一時資格情報（利用者名, パスワード）を作る。
pub fn rest_api_credentials(secret: &[u8], user_id: &str, expires_unix: u64) -> (String, String) {
    let username = format!("{expires_unix}:{user_id}");
    let password = rest_api_password(secret, &username);
    (username, password)
}

/// REST 方式の利用者名から有効期限（UNIX 秒）を取り出す。`<期限>:<ID>` の形でなければ `None`。
pub fn rest_api_expiry(username: &str) -> Option<u64> {
    let (ts, _) = username.split_once(':')?;
    if ts.is_empty() || ts.len() > 20 || !ts.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    ts.parse().ok()
}

/// 標準の base64（padding 付き）。符号化だけなので自前で持つ（暗号の部品ではない）。
pub(crate) fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for c in input.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn rest_password() {
        // `echo -n "1700000000:alice" | openssl dgst -sha1 -hmac secret -binary | base64`
        let (u, p) = rest_api_credentials(b"secret", "alice", 1_700_000_000);
        assert_eq!(u, "1700000000:alice");
        assert_eq!(p, "d8soP47RbdIKLDUOpnJPVQyq5Ts=");
        assert_eq!(rest_api_expiry(&u), Some(1_700_000_000));
        assert_eq!(rest_api_expiry("alice"), None);
        assert_eq!(rest_api_expiry("x1:alice"), None);
    }
}
