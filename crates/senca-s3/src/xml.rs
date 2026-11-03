//! S3 の応答を読むための小さな XML の読み手。上限を置き、壊れた入力は `Err` を返す。
//!
//! 扱うもの: 要素・属性（読み飛ばす）・文字データ・定義済みの実体参照と文字参照・CDATA・コメント・処理命令。
//! 扱わないもの: DOCTYPE（実体の定義による膨張を避けるため拒否する）。
//! 名前空間の接頭辞は外して、局所名だけを持つ。

/// 入力の上限（ListObjectsV2 の 1000 件で数百 KiB）。
pub const MAX_INPUT: usize = 8 << 20;
pub const MAX_DEPTH: usize = 16;
pub const MAX_NODES: usize = 200_000;
const MAX_NAME: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Element {
    /// 局所名（接頭辞を外したもの）。
    pub name: String,
    /// 直下の文字データ（子要素の間のものもつなげる）。
    pub text: String,
    pub children: Vec<Element>,
}

impl Element {
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// 子要素の文字データ。
    pub fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XmlError {
    TooLarge,
    TooDeep,
    TooManyNodes,
    NotUtf8,
    Unsupported(&'static str),
    Malformed(&'static str),
}

impl std::fmt::Display for XmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            XmlError::TooLarge => f.write_str("xml: input too large"),
            XmlError::TooDeep => f.write_str("xml: nesting too deep"),
            XmlError::TooManyNodes => f.write_str("xml: too many nodes"),
            XmlError::NotUtf8 => f.write_str("xml: not utf-8"),
            XmlError::Unsupported(w) => write!(f, "xml: unsupported {w}"),
            XmlError::Malformed(w) => write!(f, "xml: malformed {w}"),
        }
    }
}

impl std::error::Error for XmlError {}

fn find(s: &str, from: usize, pat: &str) -> Result<usize, XmlError> {
    s[from..].find(pat).map(|i| from + i).ok_or(XmlError::Malformed("unterminated markup"))
}

fn local(name: &str) -> &str {
    name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name)
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= MAX_NAME
        && n.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
        && !n.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '.')
}

/// 文字データの実体参照を戻す。
fn decode_text(raw: &str, out: &mut String) -> Result<(), XmlError> {
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let end = after.find(';').filter(|e| *e <= 10).ok_or(XmlError::Malformed("entity"))?;
        let ent = &after[..end];
        let c = match ent {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let n = if let Some(h) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(h, 16).ok()
                } else if let Some(d) = ent.strip_prefix('#') {
                    d.parse::<u32>().ok()
                } else {
                    return Err(XmlError::Unsupported("entity"));
                };
                n.and_then(char::from_u32).filter(|c| *c != '\0').ok_or(XmlError::Malformed("character reference"))?
            }
        };
        out.push(c);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(())
}

/// 文書を読んで根の要素を返す。
pub fn parse(input: &[u8]) -> Result<Element, XmlError> {
    if input.len() > MAX_INPUT {
        return Err(XmlError::TooLarge);
    }
    let s = std::str::from_utf8(input).map_err(|_| XmlError::NotUtf8)?;
    let s = s.strip_prefix('\u{feff}').unwrap_or(s);
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    let mut nodes = 0usize;
    let mut pos = 0usize;
    let close = |stack: &mut Vec<Element>, root: &mut Option<Element>, e: Element| -> Result<(), XmlError> {
        match stack.last_mut() {
            Some(p) => p.children.push(e),
            None => {
                if root.is_some() {
                    return Err(XmlError::Malformed("multiple roots"));
                }
                *root = Some(e);
            }
        }
        Ok(())
    };
    while pos < s.len() {
        let rest = &s[pos..];
        if !rest.starts_with('<') {
            let end = rest.find('<').map(|i| pos + i).unwrap_or(s.len());
            let raw = &s[pos..end];
            match stack.last_mut() {
                Some(top) => decode_text(raw, &mut top.text)?,
                None if raw.trim().is_empty() => {}
                None => return Err(XmlError::Malformed("text outside root")),
            }
            pos = end;
            continue;
        }
        if rest.starts_with("<?") {
            pos = find(s, pos + 2, "?>")? + 2;
        } else if rest.starts_with("<!--") {
            pos = find(s, pos + 4, "-->")? + 3;
        } else if rest.starts_with("<![CDATA[") {
            let end = find(s, pos + 9, "]]>")?;
            let top = stack.last_mut().ok_or(XmlError::Malformed("cdata outside root"))?;
            top.text.push_str(&s[pos + 9..end]);
            pos = end + 3;
        } else if rest.starts_with("<!") {
            return Err(XmlError::Unsupported("doctype"));
        } else if let Some(r) = rest.strip_prefix("</") {
            let end = r.find('>').ok_or(XmlError::Malformed("end tag"))?;
            let name = r[..end].trim_end();
            let e = stack.pop().ok_or(XmlError::Malformed("unbalanced end tag"))?;
            if local(name) != e.name || !valid_name(name) {
                return Err(XmlError::Malformed("mismatched end tag"));
            }
            close(&mut stack, &mut root, e)?;
            pos += 2 + end + 1;
        } else {
            // 開始タグ: 名前、属性（引用符の中の > に注意して読み飛ばす）、`/>` か `>`
            let body = &rest[1..];
            let name_end = body.find(|c: char| c.is_whitespace() || c == '/' || c == '>').ok_or(XmlError::Malformed("start tag"))?;
            let name = &body[..name_end];
            if !valid_name(name) {
                return Err(XmlError::Malformed("element name"));
            }
            let mut i = name_end;
            let bytes = body.as_bytes();
            let mut quote: Option<u8> = None;
            let self_closing;
            loop {
                let Some(&c) = bytes.get(i) else { return Err(XmlError::Malformed("start tag")) };
                match quote {
                    Some(q) if c == q => quote = None,
                    Some(_) => {}
                    None if c == b'"' || c == b'\'' => quote = Some(c),
                    None if c == b'<' => return Err(XmlError::Malformed("start tag")),
                    None if c == b'>' => {
                        self_closing = i > 0 && bytes[i - 1] == b'/';
                        break;
                    }
                    None => {}
                }
                i += 1;
            }
            nodes += 1;
            if nodes > MAX_NODES {
                return Err(XmlError::TooManyNodes);
            }
            if stack.len() >= MAX_DEPTH {
                return Err(XmlError::TooDeep);
            }
            let e = Element { name: local(name).to_string(), ..Default::default() };
            if self_closing {
                close(&mut stack, &mut root, e)?;
            } else {
                stack.push(e);
            }
            pos += 1 + i + 1;
        }
    }
    if !stack.is_empty() {
        return Err(XmlError::Malformed("unclosed element"));
    }
    root.ok_or(XmlError::Malformed("no root"))
}

/// 文字データ・属性値に入れるための符号化。
pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '&' => o.push_str("&amp;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            _ => o.push(c),
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let d = br#"<?xml version="1.0" encoding="UTF-8"?>
<!-- c --><a:Root xmlns:a="urn:x" k='v>w'><B>1 &amp; 2 &#x41;&#66;</B><C/><B><![CDATA[<x>]]></B></a:Root>"#;
        let r = parse(d).unwrap();
        assert_eq!(r.name, "Root");
        let bs: Vec<_> = r.children_named("B").map(|b| b.text.as_str()).collect();
        assert_eq!(bs, vec!["1 & 2 AB", "<x>"]);
        assert!(r.child("C").is_some());
        assert_eq!(escape("a<&\"'>"), "a&lt;&amp;&quot;&apos;&gt;");
    }

    #[test]
    fn rejects() {
        assert!(matches!(parse(b"<a><b></a></b>"), Err(XmlError::Malformed(_))));
        assert!(matches!(parse(b"<a>"), Err(XmlError::Malformed(_))));
        assert!(matches!(parse(b"<a/><b/>"), Err(XmlError::Malformed(_))));
        assert!(matches!(parse(b"<!DOCTYPE a [<!ENTITY x \"y\">]><a>&x;</a>"), Err(XmlError::Unsupported(_))));
        assert!(matches!(parse(b"<a>&bogus;</a>"), Err(XmlError::Unsupported(_))));
        assert!(matches!(parse(b"<a>&#0;</a>"), Err(XmlError::Malformed(_))));
        assert!(matches!(parse(b"<a>&#xD800;</a>"), Err(XmlError::Malformed(_))));
        assert!(matches!(parse(b"x<a/>"), Err(XmlError::Malformed(_))));
        assert!(matches!(parse(b"<1a/>"), Err(XmlError::Malformed(_))));
        assert!(matches!(parse(b"<a b=\"<\"/"), Err(XmlError::Malformed(_))));
        assert!(matches!(parse(&[0xff, b'<']), Err(XmlError::NotUtf8)));
        let deep = "<a>".repeat(MAX_DEPTH + 1) + &"</a>".repeat(MAX_DEPTH + 1);
        assert_eq!(parse(deep.as_bytes()), Err(XmlError::TooDeep));
        assert_eq!(parse(&vec![b' '; MAX_INPUT + 1]), Err(XmlError::TooLarge));
        let many = format!("<a>{}</a>", "<b/>".repeat(MAX_NODES));
        assert_eq!(parse(many.as_bytes()), Err(XmlError::TooManyNodes));
    }

    #[test]
    fn never_panics_on_prefixes() {
        // どこで切れても panic しない
        let d = br#"<?xml version="1.0"?><R><K a="1">t&lt;</K><![CDATA[z]]><!-- c --></R>"#;
        for n in 0..=d.len() {
            let _ = parse(&d[..n]);
        }
    }
}
