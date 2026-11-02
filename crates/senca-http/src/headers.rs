//! header の名前と値の検査、大文字小文字を区別しない順序付き map（重複を保持する）。

use crate::error::HttpError;
use bytes::Bytes;
use std::fmt;

/// RFC 9110 5.6.2 の tchar。
pub fn is_tchar(c: u8) -> bool {
    matches!(c,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
    ) || c.is_ascii_alphanumeric()
}

/// token（1 文字以上の tchar）か。
pub fn is_token(s: &[u8]) -> bool {
    !s.is_empty() && s.iter().all(|&c| is_tchar(c))
}

/// field-value に使える文字か（VCHAR・obs-text・SP・HTAB）。
pub fn is_field_vchar(c: u8) -> bool {
    c == b'\t' || c == b' ' || (0x21..=0x7e).contains(&c) || c >= 0x80
}

/// field-value として正しいか（前後の空白は含まない形）。
pub fn is_field_value(v: &[u8]) -> bool {
    v.iter().all(|&c| is_field_vchar(c))
        && !matches!(v.first(), Some(b' ' | b'\t'))
        && !matches!(v.last(), Some(b' ' | b'\t'))
}

/// 値を Debug に出さない header（秘密を含みうる）。
fn is_sensitive(name: &str) -> bool {
    ["authorization", "proxy-authorization", "cookie", "set-cookie", "x-api-key"]
        .iter()
        .any(|s| s.eq_ignore_ascii_case(name))
}

/// header の並び。名前の比較は大文字小文字を区別しない。同じ名前の行を順番どおりに保持する。
#[derive(Clone, Default, PartialEq, Eq)]
pub struct HeaderMap {
    entries: Vec<(String, Bytes)>,
}

impl HeaderMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 名前と値を検査して末尾に足す。
    pub fn append(&mut self, name: &str, value: impl Into<Bytes>) -> Result<(), HttpError> {
        let value = value.into();
        if !is_token(name.as_bytes()) {
            return Err(HttpError::InvalidInput("header name"));
        }
        if !is_field_value(&value) {
            return Err(HttpError::InvalidInput("header value"));
        }
        self.entries.push((name.to_owned(), value));
        Ok(())
    }

    /// 同じ名前の行をすべて消してから足す。
    pub fn insert(&mut self, name: &str, value: impl Into<Bytes>) -> Result<(), HttpError> {
        let value = value.into();
        if !is_token(name.as_bytes()) {
            return Err(HttpError::InvalidInput("header name"));
        }
        if !is_field_value(&value) {
            return Err(HttpError::InvalidInput("header value"));
        }
        self.remove(name);
        self.entries.push((name.to_owned(), value));
        Ok(())
    }

    /// 検査済みの値を足す（解析器用）。
    pub(crate) fn push_unchecked(&mut self, name: String, value: Bytes) {
        self.entries.push((name, value));
    }

    /// 同じ名前の行をすべて消す。消した数を返す。
    pub fn remove(&mut self, name: &str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        before - self.entries.len()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entries.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
    }

    /// 最初の値。
    pub fn get(&self, name: &str) -> Option<&Bytes> {
        self.entries.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v)
    }

    /// 最初の値を文字列で（UTF-8 でなければ `None`）。
    pub fn get_str(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(|v| std::str::from_utf8(v).ok())
    }

    /// 同じ名前の値を順番に。
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Bytes> + 'a {
        self.entries.iter().filter(move |(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v)
    }

    /// 高々 1 行であるべき header を取り出す。2 行以上なら `Err`。
    pub fn get_unique(&self, name: &str) -> Result<Option<&Bytes>, HttpError> {
        let mut it = self.entries.iter().filter(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v);
        let first = it.next();
        if it.next().is_some() {
            return Err(HttpError::Malformed("duplicate header"));
        }
        Ok(first)
    }

    /// 同じ名前のすべての行をカンマで分けた要素（前後の空白を除き、空の要素は飛ばす）。
    pub fn list_items<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a [u8]> + 'a {
        self.get_all(name).flat_map(|v| v.split(|&c| c == b',')).map(|s: &'a [u8]| trim_ows(s)).filter(|s| !s.is_empty())
    }

    /// カンマ区切りの一覧に、大文字小文字を区別せず `token` が含まれるか。
    pub fn has_token(&self, name: &str, token: &str) -> bool {
        self.list_items(name).any(|t| t.eq_ignore_ascii_case(token.as_bytes()))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Bytes)> {
        self.entries.iter().map(|(n, v)| (n.as_str(), v))
    }

    /// 書き出し時の長さ（`name: value\r\n` の合計）。
    pub(crate) fn encoded_len(&self) -> usize {
        self.entries.iter().map(|(n, v)| n.len() + v.len() + 4).sum()
    }

    pub(crate) fn write_to(&self, out: &mut Vec<u8>) {
        for (n, v) in &self.entries {
            out.extend_from_slice(n.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(v);
            out.extend_from_slice(b"\r\n");
        }
    }
}

impl fmt::Debug for HeaderMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut m = f.debug_list();
        for (n, v) in &self.entries {
            if is_sensitive(n) {
                m.entry(&format_args!("{n}: <redacted>"));
            } else {
                m.entry(&format_args!("{n}: {}", String::from_utf8_lossy(v)));
            }
        }
        m.finish()
    }
}

/// 前後の OWS（SP・HTAB）を除く。
pub fn trim_ows(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&c| c != b' ' && c != b'\t').unwrap_or(s.len());
    let end = s.iter().rposition(|&c| c != b' ' && c != b'\t').map_or(start, |p| p + 1);
    &s[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_insensitive_and_ordered() {
        let mut h = HeaderMap::new();
        h.append("Accept", "a").unwrap();
        h.append("X-Foo", "1").unwrap();
        h.append("accept", "b").unwrap();
        assert_eq!(h.get("ACCEPT").unwrap(), "a");
        let all: Vec<_> = h.get_all("accept").collect();
        assert_eq!(all, vec!["a", "b"]);
        assert!(h.get_unique("accept").is_err());
        assert_eq!(h.get_unique("x-foo").unwrap().unwrap(), "1");
        assert_eq!(h.remove("Accept"), 2);
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn rejects_injection() {
        let mut h = HeaderMap::new();
        assert!(h.append("X", "a\r\nInjected: 1").is_err());
        assert!(h.append("Bad Name", "a").is_err());
        assert!(h.append("X", " lead").is_err());
        assert!(h.append("X", "a\0b").is_err());
    }

    #[test]
    fn list_tokens() {
        let mut h = HeaderMap::new();
        h.append("Connection", "keep-alive, Upgrade").unwrap();
        h.append("Connection", ",close").unwrap();
        assert!(h.has_token("connection", "upgrade"));
        assert!(h.has_token("connection", "CLOSE"));
        assert!(!h.has_token("connection", "te"));
    }

    #[test]
    fn debug_redacts() {
        let mut h = HeaderMap::new();
        h.append("Authorization", "Bearer secret-token").unwrap();
        let s = format!("{h:?}");
        assert!(!s.contains("secret"));
    }
}
