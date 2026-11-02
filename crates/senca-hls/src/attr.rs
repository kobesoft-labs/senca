//! attribute-list（RFC 8216 4.2）の解析と書き出し、10 進小数（マイクロ秒）の換算。

use crate::error::HlsError;

/// 属性の値（引用符付きか否かだけを区別し、型は取り出すときに検査する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttrValue<'a> {
    Quoted(&'a str),
    Bare(&'a str),
}

/// 解析済みの attribute-list。名前の重複は拒否済み。
#[derive(Debug, Default)]
pub(crate) struct AttrList<'a> {
    items: Vec<(&'a str, AttrValue<'a>)>,
}

const MAX_ATTRS: usize = 64;

fn is_attr_name_char(c: u8) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'-'
}

impl<'a> AttrList<'a> {
    pub(crate) fn parse(s: &'a str) -> Result<Self, HlsError> {
        let b = s.as_bytes();
        let mut items: Vec<(&'a str, AttrValue<'a>)> = Vec::new();
        let mut i = 0;
        if b.is_empty() {
            return Ok(AttrList { items });
        }
        loop {
            let start = i;
            while i < b.len() && is_attr_name_char(b[i]) {
                i += 1;
            }
            if i == start || b.get(i) != Some(&b'=') {
                return Err(HlsError::Syntax("attribute name"));
            }
            let name = &s[start..i];
            i += 1;
            let value = if b.get(i) == Some(&b'"') {
                let vs = i + 1;
                let end = b[vs..].iter().position(|&c| c == b'"').ok_or(HlsError::Syntax("unterminated quoted string"))?;
                i = vs + end + 1;
                AttrValue::Quoted(&s[vs..vs + end])
            } else {
                let vs = i;
                while i < b.len() && b[i] != b',' {
                    let c = b[i];
                    if c == b'"' || c == b' ' || c == b'\t' || c < 0x20 {
                        return Err(HlsError::Syntax("enumerated value"));
                    }
                    i += 1;
                }
                if i == vs {
                    return Err(HlsError::Syntax("empty attribute value"));
                }
                AttrValue::Bare(&s[vs..i])
            };
            if items.iter().any(|(n, _)| *n == name) {
                return Err(HlsError::Syntax("duplicate attribute"));
            }
            if items.len() >= MAX_ATTRS {
                return Err(HlsError::Syntax("too many attributes"));
            }
            items.push((name, value));
            match b.get(i) {
                None => break,
                Some(b',') if i + 1 < b.len() => i += 1,
                _ => return Err(HlsError::Syntax("attribute separator")),
            }
        }
        Ok(AttrList { items })
    }

    fn get(&self, name: &str) -> Option<AttrValue<'a>> {
        self.items.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
    }

    pub(crate) fn quoted(&self, name: &str) -> Result<Option<&'a str>, HlsError> {
        match self.get(name) {
            None => Ok(None),
            Some(AttrValue::Quoted(s)) => Ok(Some(s)),
            Some(AttrValue::Bare(_)) => Err(HlsError::Attribute(name.to_owned())),
        }
    }

    pub(crate) fn enumerated(&self, name: &str) -> Result<Option<&'a str>, HlsError> {
        match self.get(name) {
            None => Ok(None),
            Some(AttrValue::Bare(s)) => Ok(Some(s)),
            Some(AttrValue::Quoted(_)) => Err(HlsError::Attribute(name.to_owned())),
        }
    }

    pub(crate) fn integer(&self, name: &str) -> Result<Option<u64>, HlsError> {
        self.enumerated(name)?
            .map(|s| parse_integer(s).ok_or_else(|| HlsError::Attribute(name.to_owned())))
            .transpose()
    }

    pub(crate) fn micros(&self, name: &str) -> Result<Option<u64>, HlsError> {
        self.enumerated(name)?
            .map(|s| parse_micros(s).ok_or_else(|| HlsError::Attribute(name.to_owned())))
            .transpose()
    }

    /// YES / NO（省略時は `None`）。
    pub(crate) fn yes_no(&self, name: &str) -> Result<Option<bool>, HlsError> {
        match self.enumerated(name)? {
            None => Ok(None),
            Some("YES") => Ok(Some(true)),
            Some("NO") => Ok(Some(false)),
            Some(_) => Err(HlsError::Attribute(name.to_owned())),
        }
    }

    pub(crate) fn resolution(&self, name: &str) -> Result<Option<(u32, u32)>, HlsError> {
        let Some(s) = self.enumerated(name)? else { return Ok(None) };
        let (w, h) = s.split_once('x').ok_or_else(|| HlsError::Attribute(name.to_owned()))?;
        let w = parse_integer(w).and_then(|v| u32::try_from(v).ok());
        let h = parse_integer(h).and_then(|v| u32::try_from(v).ok());
        match (w, h) {
            (Some(w), Some(h)) => Ok(Some((w, h))),
            _ => Err(HlsError::Attribute(name.to_owned())),
        }
    }

    pub(crate) fn require<T>(v: Option<T>, name: &str) -> Result<T, HlsError> {
        v.ok_or_else(|| HlsError::MissingAttribute(name.to_owned()))
    }
}

/// decimal-integer（0〜2^64-1、20 桁まで）。
pub(crate) fn parse_integer(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 20 || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// decimal-floating-point（非負）をマイクロ秒にする。小数は 9 桁まで受け付け、マイクロ秒に丸める。
pub(crate) fn parse_micros(s: &str) -> Option<u64> {
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    if int.is_empty() || int.len() > 12 || !int.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if s.contains('.') && (frac.is_empty() || frac.len() > 9 || !frac.bytes().all(|c| c.is_ascii_digit())) {
        return None;
    }
    let int: u64 = int.parse().ok()?;
    let mut nanos: u64 = 0;
    for (k, c) in frac.bytes().enumerate() {
        nanos += (c - b'0') as u64 * 10u64.pow(8 - k as u32);
    }
    let micros = (nanos + 500) / 1000;
    int.checked_mul(1_000_000)?.checked_add(micros)
}

/// マイクロ秒を 10 進小数にする（小数は 3 桁以上、末尾の 0 は 3 桁まで削る）。
pub(crate) fn format_micros(us: u64) -> String {
    let int = us / 1_000_000;
    let frac = us % 1_000_000;
    let mut f = format!("{frac:06}");
    while f.len() > 3 && f.ends_with('0') {
        f.pop();
    }
    format!("{int}.{f}")
}

/// 引用符付き文字列に入れられるか（`"`・CR・LF を含まない）。
pub(crate) fn check_quoted(s: &str) -> Result<(), HlsError> {
    if s.contains(['"', '\r', '\n']) {
        return Err(HlsError::InvalidValue("quoted string contains \" or line break"));
    }
    Ok(())
}

/// enumerated-string として書けるか。
pub(crate) fn check_enumerated(s: &str) -> Result<(), HlsError> {
    if s.is_empty() || s.bytes().any(|c| c == b'"' || c == b',' || c <= b' ' || c == 0x7f) {
        return Err(HlsError::InvalidValue("enumerated string"));
    }
    Ok(())
}

/// attribute-list の書き出し。
#[derive(Debug, Default)]
pub(crate) struct AttrWriter {
    out: String,
}

impl AttrWriter {
    fn sep(&mut self, name: &str) {
        if !self.out.is_empty() {
            self.out.push(',');
        }
        self.out.push_str(name);
        self.out.push('=');
    }

    pub(crate) fn quoted(&mut self, name: &str, v: &str) -> Result<&mut Self, HlsError> {
        check_quoted(v)?;
        self.sep(name);
        self.out.push('"');
        self.out.push_str(v);
        self.out.push('"');
        Ok(self)
    }

    pub(crate) fn enumerated(&mut self, name: &str, v: &str) -> Result<&mut Self, HlsError> {
        check_enumerated(v)?;
        self.sep(name);
        self.out.push_str(v);
        Ok(self)
    }

    pub(crate) fn integer(&mut self, name: &str, v: u64) -> &mut Self {
        self.sep(name);
        self.out.push_str(&v.to_string());
        self
    }

    pub(crate) fn micros(&mut self, name: &str, v: u64) -> &mut Self {
        self.sep(name);
        self.out.push_str(&format_micros(v));
        self
    }

    pub(crate) fn yes(&mut self, name: &str) -> &mut Self {
        self.sep(name);
        self.out.push_str("YES");
        self
    }

    pub(crate) fn finish(&mut self) -> String {
        std::mem::take(&mut self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list() {
        let a = AttrList::parse("BANDWIDTH=1280000,CODECS=\"avc1.4d401f,mp4a.40.2\",RESOLUTION=1280x720,X-Y=abc").unwrap();
        assert_eq!(a.integer("BANDWIDTH").unwrap(), Some(1_280_000));
        assert_eq!(a.quoted("CODECS").unwrap(), Some("avc1.4d401f,mp4a.40.2"));
        assert_eq!(a.resolution("RESOLUTION").unwrap(), Some((1280, 720)));
        assert!(a.quoted("BANDWIDTH").is_err());
        assert!(a.integer("CODECS").is_err());
        for bad in ["A=1,A=2", "a=1", "A=", "A=\"x", "A=1,", "=1", "A=1 ,B=2", "A=\"x\"y", "A=x\"y"] {
            assert!(AttrList::parse(bad).is_err(), "{bad}");
        }
        assert!(AttrList::parse("").unwrap().items.is_empty());
    }

    #[test]
    fn decimals() {
        assert_eq!(parse_micros("2.000000"), Some(2_000_000));
        assert_eq!(parse_micros("10"), Some(10_000_000));
        assert_eq!(parse_micros("0.0333333"), Some(33_333));
        assert_eq!(parse_micros("1.0000005"), Some(1_000_001));
        assert_eq!(parse_micros("1."), None);
        assert_eq!(parse_micros(".5"), None);
        assert_eq!(parse_micros("-1"), None);
        assert_eq!(parse_micros("1e3"), None);
        assert_eq!(format_micros(2_000_000), "2.000");
        assert_eq!(format_micros(1_001_000), "1.001");
        assert_eq!(format_micros(33_333), "0.033333");
        for v in [0u64, 1, 999_999, 4_004_000, 123_456_789] {
            assert_eq!(parse_micros(&format_micros(v)), Some(v));
        }
    }
}
