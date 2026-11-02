//! 秘密の文字列（API トークン・secret access key・stream key・passphrase）。

use std::fmt;

/// 秘密の文字列。`Debug` は値を伏せ、`Display` は実装しない。値は [`Secret::expose`] でだけ取り出す。
/// 破棄するときに中身を 0 で上書きする（最善の努力。コピーされた先までは追わない）。
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }

    /// 値を取り出す。ログ・エラー・監視ラベルに渡さないこと（仕様 6.7・9.3）。
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 定数時間で比べる（長さは漏れる）。
    pub fn ct_eq(&self, other: &str) -> bool {
        use subtle::ConstantTimeEq;
        self.0.len() == other.len() && bool::from(self.0.as_bytes().ct_eq(other.as_bytes()))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        let mut v = std::mem::take(&mut self.0).into_bytes();
        v.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&v);
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Secret(s)
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Secret(s.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_value() {
        let s = Secret::new("sk_live_abc");
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        assert!(!format!("{:?}", Some(&s)).contains("sk_live"));
        assert!(s.ct_eq("sk_live_abc"));
        assert!(!s.ct_eq("sk_live_abd"));
        assert!(!s.ct_eq("sk"));
    }
}
