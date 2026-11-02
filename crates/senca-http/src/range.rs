//! Range header（単一の byte range のみ。RFC 9110 14）。

/// `bytes=` の 1 つの範囲。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    /// `a-b`（両端を含む）。
    FromTo(u64, u64),
    /// `a-`。
    From(u64),
    /// `-n`（末尾の n バイト）。
    Suffix(u64),
}

/// 解析の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeParse {
    Single(RangeSpec),
    /// 複数範囲や bytes 以外の単位など、対応しない形（Range を無視して 200 で全体を返す）。
    Unsupported,
    /// 文法違反（RFC 9110 14.2 により無視して 200 で返してよい）。
    Invalid,
}

fn num(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 19 || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Range header の値を読む。
pub fn parse_range(value: &str) -> RangeParse {
    let Some((unit, set)) = value.split_once('=') else { return RangeParse::Invalid };
    if !unit.eq_ignore_ascii_case("bytes") {
        return RangeParse::Unsupported;
    }
    let set = set.trim_matches([' ', '\t']);
    if set.contains(',') {
        return RangeParse::Unsupported;
    }
    let Some((a, b)) = set.split_once('-') else { return RangeParse::Invalid };
    match (a.is_empty(), b.is_empty()) {
        (true, true) => RangeParse::Invalid,
        (true, false) => num(b).map_or(RangeParse::Invalid, |n| RangeParse::Single(RangeSpec::Suffix(n))),
        (false, true) => num(a).map_or(RangeParse::Invalid, |n| RangeParse::Single(RangeSpec::From(n))),
        (false, false) => match (num(a), num(b)) {
            (Some(x), Some(y)) if x <= y => RangeParse::Single(RangeSpec::FromTo(x, y)),
            _ => RangeParse::Invalid,
        },
    }
}

impl RangeSpec {
    /// 長さ `len` の表現に当てはめた（先頭, 末尾）。両端を含む。満たせなければ `None`（416）。
    pub fn resolve(self, len: u64) -> Option<(u64, u64)> {
        if len == 0 {
            return None;
        }
        match self {
            RangeSpec::FromTo(a, b) if a < len => Some((a, b.min(len - 1))),
            RangeSpec::From(a) if a < len => Some((a, len - 1)),
            RangeSpec::Suffix(n) if n > 0 => Some((len - n.min(len), len - 1)),
            _ => None,
        }
    }
}

/// `Content-Range: bytes a-b/len`。
pub fn content_range(start: u64, end: u64, len: u64) -> String {
    format!("bytes {start}-{end}/{len}")
}

/// 416 に付ける `Content-Range: bytes */len`。
pub fn unsatisfied_range(len: u64) -> String {
    format!("bytes */{len}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        assert_eq!(parse_range("bytes=0-499"), RangeParse::Single(RangeSpec::FromTo(0, 499)));
        assert_eq!(parse_range("bytes=500-"), RangeParse::Single(RangeSpec::From(500)));
        assert_eq!(parse_range("bytes=-500"), RangeParse::Single(RangeSpec::Suffix(500)));
        assert_eq!(parse_range("bytes=0-1,5-6"), RangeParse::Unsupported);
        assert_eq!(parse_range("items=0-1"), RangeParse::Unsupported);
        assert_eq!(parse_range("bytes=5-1"), RangeParse::Invalid);
        assert_eq!(parse_range("bytes=-"), RangeParse::Invalid);
        assert_eq!(parse_range("bytes=+1-2"), RangeParse::Invalid);
        assert_eq!(parse_range("bytes=99999999999999999999-"), RangeParse::Invalid);
    }

    #[test]
    fn resolve() {
        assert_eq!(RangeSpec::FromTo(0, 9999).resolve(100), Some((0, 99)));
        assert_eq!(RangeSpec::From(100).resolve(100), None);
        assert_eq!(RangeSpec::Suffix(500).resolve(100), Some((0, 99)));
        assert_eq!(RangeSpec::Suffix(10).resolve(100), Some((90, 99)));
        assert_eq!(RangeSpec::Suffix(0).resolve(100), None);
        assert_eq!(content_range(0, 99, 100), "bytes 0-99/100");
    }
}
