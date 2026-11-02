//! percent-encoding の厳格な復号（path segment）と query string の解析。

use crate::error::HttpError;

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn decode_bytes(s: &[u8], plus_as_space: bool) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'%' => {
                let (Some(h), Some(l)) = (s.get(i + 1).and_then(|&c| hex_val(c)), s.get(i + 2).and_then(|&c| hex_val(c)))
                else {
                    return Err(HttpError::Malformed("percent-encoding"));
                };
                out.push((h << 4) | l);
                i += 3;
            }
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// path segment 1 つを復号する。`/`（%2F）・NUL・制御文字に復号されるもの、UTF-8 でないもの、
/// `.`・`..` の segment は拒否する（path の取り違えを防ぐ）。
pub fn decode_path_segment(seg: &str) -> Result<String, HttpError> {
    let raw = decode_bytes(seg.as_bytes(), false)?;
    if raw.iter().any(|&c| c == b'/' || c < 0x20 || c == 0x7f) {
        return Err(HttpError::Malformed("path segment"));
    }
    let s = String::from_utf8(raw).map_err(|_| HttpError::Malformed("path segment utf-8"))?;
    if s == "." || s == ".." {
        return Err(HttpError::Malformed("dot segment"));
    }
    Ok(s)
}

/// path（`?` の前）を segment に分けて復号する。先頭は `/` であること。空の segment（`//`）は拒否、
/// 末尾の `/` は空でない最後の segment の後なら許す。
pub fn decode_path(path: &str) -> Result<Vec<String>, HttpError> {
    let rest = path.strip_prefix('/').ok_or(HttpError::Malformed("path must start with /"))?;
    if rest.is_empty() {
        return Ok(Vec::new());
    }
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let mut out = Vec::new();
    for seg in rest.split('/') {
        if seg.is_empty() {
            return Err(HttpError::Malformed("empty path segment"));
        }
        if out.len() >= 64 {
            return Err(HttpError::Malformed("too many path segments"));
        }
        out.push(decode_path_segment(seg)?);
    }
    Ok(out)
}

/// 1 要素あたり・全体の上限。
const MAX_QUERY_PAIRS: usize = 64;

/// `application/x-www-form-urlencoded` 形式の query を (名前, 値) の列にする。`+` は空白。
/// 名前の重複は保持する（使う側で拒否するか決める）。UTF-8 でない値や不正な % は `Err`。
pub fn parse_query(q: &str) -> Result<Vec<(String, String)>, HttpError> {
    let mut out = Vec::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        if out.len() >= MAX_QUERY_PAIRS {
            return Err(HttpError::Malformed("too many query parameters"));
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = String::from_utf8(decode_bytes(k.as_bytes(), true)?).map_err(|_| HttpError::Malformed("query utf-8"))?;
        let v = String::from_utf8(decode_bytes(v.as_bytes(), true)?).map_err(|_| HttpError::Malformed("query utf-8"))?;
        if k.is_empty() || k.chars().chain(v.chars()).any(char::is_control) {
            return Err(HttpError::Malformed("query parameter"));
        }
        out.push((k, v));
    }
    Ok(out)
}

/// 名前が 1 回だけ現れる値。2 回以上なら `Err`。
pub fn query_unique<'a>(pairs: &'a [(String, String)], name: &str) -> Result<Option<&'a str>, HttpError> {
    let mut it = pairs.iter().filter(|(k, _)| k == name);
    let first = it.next().map(|(_, v)| v.as_str());
    if it.next().is_some() {
        return Err(HttpError::Malformed("duplicate query parameter"));
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments() {
        assert_eq!(decode_path_segment("a%20b").unwrap(), "a b");
        assert_eq!(decode_path_segment("%E6%97%A5").unwrap(), "日");
        assert!(decode_path_segment("a%2Fb").is_err());
        assert!(decode_path_segment("%00").is_err());
        assert!(decode_path_segment("%zz").is_err());
        assert!(decode_path_segment("%4").is_err());
        assert!(decode_path_segment("%ff").is_err());
        assert!(decode_path_segment("..").is_err());
        assert!(decode_path_segment("%2e%2e").is_err());
        assert_eq!(decode_path("/api/v1/streams/").unwrap(), vec!["api", "v1", "streams"]);
        assert!(decode_path("/api//v1").is_err());
        assert!(decode_path("api").is_err());
        assert!(decode_path("/").unwrap().is_empty());
    }

    #[test]
    fn query() {
        let q = parse_query("a=1&b=x+y&c=%26&&d").unwrap();
        assert_eq!(q, vec![("a".into(), "1".into()), ("b".into(), "x y".into()), ("c".into(), "&".into()), ("d".into(), "".into())]);
        assert!(parse_query("a=%zz").is_err());
        assert!(parse_query("a=%0a").is_err());
        let q = parse_query("a=1&a=2").unwrap();
        assert!(query_unique(&q, "a").is_err());
        assert_eq!(query_unique(&q, "b").unwrap(), None);
    }
}
