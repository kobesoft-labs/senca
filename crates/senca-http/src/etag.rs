//! ETag と If-Match / If-None-Match（RFC 9110 8.8.3・13.1）。

use crate::error::HttpError;
use std::fmt;

/// entity-tag。`tag` は引用符を除いた中身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityTag {
    pub weak: bool,
    pub tag: String,
}

/// etagc = "!" / %x23-7E / obs-text
fn is_etagc(c: u8) -> bool {
    c == 0x21 || (0x23..=0x7e).contains(&c) || c >= 0x80
}

impl EntityTag {
    pub fn strong(tag: impl Into<String>) -> Result<Self, HttpError> {
        let tag = tag.into();
        if !tag.bytes().all(is_etagc) {
            return Err(HttpError::InvalidInput("etag"));
        }
        Ok(EntityTag { weak: false, tag })
    }

    pub fn parse(s: &str) -> Result<Self, HttpError> {
        let (weak, rest) = match s.strip_prefix("W/") {
            Some(r) => (true, r),
            None => (false, s),
        };
        let inner = rest
            .strip_prefix('"')
            .and_then(|r| r.strip_suffix('"'))
            .ok_or(HttpError::Malformed("entity-tag"))?;
        if !inner.bytes().all(is_etagc) {
            return Err(HttpError::Malformed("entity-tag"));
        }
        Ok(EntityTag { weak, tag: inner.to_owned() })
    }

    /// 強い比較（If-Match・Range 用）。
    pub fn strong_eq(&self, other: &EntityTag) -> bool {
        !self.weak && !other.weak && self.tag == other.tag
    }

    /// 弱い比較（If-None-Match 用）。
    pub fn weak_eq(&self, other: &EntityTag) -> bool {
        self.tag == other.tag
    }
}

impl fmt::Display for EntityTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.weak {
            f.write_str("W/")?;
        }
        write!(f, "\"{}\"", self.tag)
    }
}

/// If-Match / If-None-Match の値。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EtagCondition {
    Any,
    List(Vec<EntityTag>),
}

const MAX_TAGS: usize = 32;

impl EtagCondition {
    /// `*` または entity-tag のカンマ区切り。引用符の中のカンマを区切りとみなさない。
    pub fn parse(value: &str) -> Result<Self, HttpError> {
        let v = value.trim_matches([' ', '\t']);
        if v == "*" {
            return Ok(EtagCondition::Any);
        }
        let b = v.as_bytes();
        let mut tags = Vec::new();
        let mut i = 0;
        while i < b.len() {
            while i < b.len() && matches!(b[i], b' ' | b'\t' | b',') {
                i += 1;
            }
            if i >= b.len() {
                break;
            }
            let start = i;
            if b[i..].starts_with(b"W/") {
                i += 2;
            }
            if b.get(i) != Some(&b'"') {
                return Err(HttpError::Malformed("entity-tag"));
            }
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            if i >= b.len() {
                return Err(HttpError::Malformed("entity-tag"));
            }
            i += 1;
            if tags.len() >= MAX_TAGS {
                return Err(HttpError::Malformed("too many entity-tags"));
            }
            tags.push(EntityTag::parse(&v[start..i])?);
            while i < b.len() && matches!(b[i], b' ' | b'\t') {
                i += 1;
            }
            if i < b.len() && b[i] != b',' {
                return Err(HttpError::Malformed("entity-tag list"));
            }
        }
        if tags.is_empty() {
            return Err(HttpError::Malformed("empty entity-tag list"));
        }
        Ok(EtagCondition::List(tags))
    }

    /// If-Match の判定（強い比較）。`current` は現在の表現の ETag（資源がなければ `None`）。
    pub fn if_match(&self, current: Option<&EntityTag>) -> bool {
        match (self, current) {
            (_, None) => false,
            (EtagCondition::Any, Some(_)) => true,
            (EtagCondition::List(l), Some(c)) => l.iter().any(|t| t.strong_eq(c)),
        }
    }

    /// If-None-Match の判定（弱い比較）。`true` なら条件成立（処理してよい）。
    pub fn if_none_match(&self, current: Option<&EntityTag>) -> bool {
        match (self, current) {
            (_, None) => true,
            (EtagCondition::Any, Some(_)) => false,
            (EtagCondition::List(l), Some(c)) => !l.iter().any(|t| t.weak_eq(c)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_compare() {
        let t = EntityTag::parse("\"v1\"").unwrap();
        assert_eq!(t.to_string(), "\"v1\"");
        let w = EntityTag::parse("W/\"v1\"").unwrap();
        assert!(!t.strong_eq(&w));
        assert!(t.weak_eq(&w));
        assert!(EntityTag::parse("v1").is_err());
        assert!(EntityTag::parse("\"a\"b\"").is_err());

        let c = EtagCondition::parse(" \"a,b\", W/\"v1\" ,\"x\"").unwrap();
        let EtagCondition::List(l) = &c else { panic!() };
        assert_eq!(l.len(), 3);
        assert_eq!(l[0].tag, "a,b");
        assert!(!c.if_match(Some(&t))); // W/"v1" は強い比較で一致しない
        assert!(!c.if_none_match(Some(&t)));
        assert_eq!(EtagCondition::parse("*").unwrap(), EtagCondition::Any);
        assert!(EtagCondition::Any.if_match(Some(&t)));
        assert!(!EtagCondition::Any.if_match(None));
        assert!(EtagCondition::parse("\"a\" \"b\"").is_err());
        assert!(EtagCondition::parse("\"a").is_err());
        assert!(EtagCondition::parse("").is_err());
    }
}
