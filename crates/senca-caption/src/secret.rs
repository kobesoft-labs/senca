//! API キーなどの秘密の値。`Debug`・`Display` では中身を出さない（開発の約束「秘密を出さない」）。

use std::fmt;

/// 秘密の文字列。中身は [`Secret::expose`] でだけ取り出す（要求を送る直前に使う）。
#[derive(Clone, Default)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    /// 中身。ログ・誤りの文に入れないこと。
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `text` の中に秘密が含まれていれば伏せ字にする（相手が誤りの文にキーを返してきたとき用）。
    pub fn redact_in(&self, text: &str) -> String {
        if self.0.len() < 4 {
            return text.to_string();
        }
        text.replace(&self.0, "[REDACTED]")
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // できる範囲で消す（最適化で消されないよう black_box を通す）。
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        for b in bytes.iter_mut() {
            *b = 0;
        }
        std::hint::black_box(&bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_printed() {
        let s = Secret::new("sk-very-secret-key");
        assert_eq!(format!("{s:?}"), "Secret([REDACTED])");
        assert_eq!(format!("{s}"), "[REDACTED]");
        assert_eq!(s.expose(), "sk-very-secret-key");
        assert_eq!(s.redact_in("bad key sk-very-secret-key here"), "bad key [REDACTED] here");
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Holder {
            key: Secret,
        }
        assert!(!format!("{:?}", Holder { key: s }).contains("secret-key"));
    }
}
