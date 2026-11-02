//! playlist を行に分ける（RFC 8216 4.1）。

use crate::error::HlsError;

/// 解析の上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseLimits {
    pub max_bytes: usize,
    pub max_line: usize,
    pub max_lines: usize,
}

impl ParseLimits {
    pub const DEFAULT: ParseLimits = ParseLimits { max_bytes: 8 * 1024 * 1024, max_line: 16 * 1024, max_lines: 200_000 };
}

impl Default for ParseLimits {
    fn default() -> Self {
        ParseLimits::DEFAULT
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Line<'a> {
    /// `#EXT...` の名前（`#` を除く）と `:` の後ろ。
    Tag(&'a str, Option<&'a str>),
    Uri(&'a str),
}

/// UTF-8 を確かめて行に分ける。先頭は `#EXTM3U`。空行と comment は飛ばす。
pub(crate) fn split_lines<'a>(input: &'a [u8], limits: &ParseLimits) -> Result<Vec<Line<'a>>, HlsError> {
    if input.len() > limits.max_bytes {
        return Err(HlsError::TooLarge);
    }
    let s = std::str::from_utf8(input).map_err(|_| HlsError::Syntax("not UTF-8"))?;
    let mut out = Vec::new();
    let mut first = true;
    for (n, raw) in s.split('\n').enumerate() {
        if n >= limits.max_lines {
            return Err(HlsError::TooLarge);
        }
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.len() > limits.max_line {
            return Err(HlsError::TooLarge);
        }
        if line.chars().any(|c| c.is_control() && c != '\t') {
            return Err(HlsError::Syntax("control character"));
        }
        if first {
            if line != "#EXTM3U" {
                return Err(HlsError::Syntax("missing #EXTM3U"));
            }
            first = false;
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if let Some(tag) = line.strip_prefix('#') {
            if !tag.starts_with("EXT") {
                continue; // comment
            }
            let (name, value) = match tag.split_once(':') {
                Some((n, v)) => (n, Some(v)),
                None => (tag, None),
            };
            if !name.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'-') {
                return Err(HlsError::Syntax("tag name"));
            }
            out.push(Line::Tag(name, value));
        } else {
            if line.chars().any(char::is_whitespace) {
                return Err(HlsError::Syntax("whitespace in URI line"));
            }
            out.push(Line::Uri(line));
        }
    }
    if first {
        return Err(HlsError::Syntax("missing #EXTM3U"));
    }
    Ok(out)
}
