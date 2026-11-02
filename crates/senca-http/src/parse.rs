//! 要求・応答で共通の部品: 行の切り出し、header section、body の枠組み（Content-Length・chunked・切断まで）。

use crate::error::HttpError;
use crate::headers::{HeaderMap, is_field_vchar, is_token, trim_ows};
use bytes::{Bytes, BytesMut};

/// 解析の上限（仕様 9.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// request line / status line の長さ（CRLF を除く）。
    pub max_start_line: usize,
    /// header section の合計（各行の長さ + CRLF）。trailer にも同じ上限を使う。
    pub max_header_bytes: usize,
    /// header の行数。trailer にも同じ上限を使う。
    pub max_headers: usize,
    /// body の上限（chunked は復号後の合計）。
    pub max_body: u64,
    /// chunk-size 行（chunk-ext を含む）の長さ。
    pub max_chunk_line: usize,
    /// 解析器が溜められる未処理バイト数。
    pub max_buffer: usize,
}

impl Limits {
    /// HTTP API 向けの既定値（request line 8 KiB、header 32 KiB・100 行、body 1 MiB）。
    pub const DEFAULT: Limits = Limits {
        max_start_line: 8 * 1024,
        max_header_bytes: 32 * 1024,
        max_headers: 100,
        max_body: 1024 * 1024,
        max_chunk_line: 4 * 1024,
        max_buffer: 128 * 1024,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Limits::DEFAULT
    }
}

/// 解析器が返す出来事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event<H> {
    /// もっとバイトが要る。
    NeedMore,
    /// 開始行と header。
    Head(H),
    /// body の一部（受信バッファの切り出し。コピーしない）。
    Data(Bytes),
    /// message の終わり。chunked の trailer（なければ空）。
    End(HeaderMap),
    /// 相手が閉じ、message の途中ではない（またはプロトコルが切り替わった）。
    Closed,
}

/// HTTP の版。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Version {
    Http10,
    Http11,
}

impl Version {
    pub fn as_str(self) -> &'static str {
        match self {
            Version::Http10 => "HTTP/1.0",
            Version::Http11 => "HTTP/1.1",
        }
    }

    /// `HTTP/x.y` を読む。1.0・1.1 以外の正しい形は 505、形が違えば 400。
    pub(crate) fn parse(s: &[u8]) -> Result<Version, HttpError> {
        match s {
            b"HTTP/1.1" => Ok(Version::Http11),
            b"HTTP/1.0" => Ok(Version::Http10),
            [b'H', b'T', b'T', b'P', b'/', a, b'.', b] if a.is_ascii_digit() && b.is_ascii_digit() => {
                Err(HttpError::UnsupportedVersion)
            }
            _ => Err(HttpError::Malformed("http version")),
        }
    }
}

/// body の枠組み。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFraming {
    /// body なし。
    None,
    /// Content-Length。
    Length(u64),
    /// Transfer-Encoding: chunked。
    Chunked,
    /// 接続が閉じるまで（応答のみ）。
    CloseDelimited,
}

/// CRLF で終わる 1 行を取り出す。途中まで調べた位置を覚えて、少しずつ届いても二乗の手間にしない。
#[derive(Debug, Default, Clone)]
pub(crate) struct LineScanner {
    scanned: usize,
}

impl LineScanner {
    /// `Ok(Some(line))` は CRLF を除いた 1 行。NUL・裸の CR・裸の LF は `Malformed`。
    /// `max` を超えても LF がなければ `too_long`。
    pub(crate) fn take(
        &mut self,
        buf: &mut BytesMut,
        max: usize,
        too_long: HttpError,
    ) -> Result<Option<Bytes>, HttpError> {
        let limit = buf.len().min(max.saturating_add(2));
        let mut i = self.scanned;
        while i < limit {
            match buf[i] {
                b'\n' => {
                    if i == 0 || buf[i - 1] != b'\r' {
                        return Err(HttpError::Malformed("bare LF"));
                    }
                    self.scanned = 0;
                    let line = buf.split_to(i + 1).freeze();
                    return Ok(Some(line.slice(..i - 1)));
                }
                0 => return Err(HttpError::Malformed("NUL")),
                b'\r' => {
                    if let Some(&n) = buf.get(i + 1)
                        && n != b'\n'
                    {
                        return Err(HttpError::Malformed("bare CR"));
                    }
                }
                _ => {}
            }
            i += 1;
        }
        // 最後の CR は LF を待つので次回もう一度見る
        self.scanned = if i > 0 && buf[i - 1] == b'\r' { i - 1 } else { i };
        if buf.len() >= max.saturating_add(2) {
            return Err(too_long);
        }
        Ok(None)
    }

    pub(crate) fn reset(&mut self) {
        self.scanned = 0;
    }
}

/// `name: value` の 1 行を読む。obs-fold・名前の後の空白・不正な文字は拒否。
pub(crate) fn parse_field_line(line: &Bytes) -> Result<(String, Bytes), HttpError> {
    if matches!(line.first(), Some(b' ' | b'\t')) {
        return Err(HttpError::Malformed("obs-fold"));
    }
    let colon = line.iter().position(|&c| c == b':').ok_or(HttpError::Malformed("header without colon"))?;
    let name = &line[..colon];
    if !is_token(name) {
        return Err(HttpError::Malformed("header name"));
    }
    let raw = &line[colon + 1..];
    if !raw.iter().all(|&c| is_field_vchar(c)) {
        return Err(HttpError::Malformed("header value"));
    }
    let v = trim_ows(raw);
    let start = v.as_ptr() as usize - line.as_ptr() as usize;
    let value = line.slice(start..start + v.len());
    // name は token なので ASCII
    let name = String::from_utf8_lossy(name).into_owned();
    Ok((name, value))
}

/// header section（と trailer section）を 1 行ずつ読む。
#[derive(Debug, Default)]
pub(crate) struct FieldReader {
    pub(crate) fields: HeaderMap,
    bytes: usize,
    scanner: LineScanner,
}

impl FieldReader {
    /// 空行まで読めたら `Ok(true)`。
    pub(crate) fn poll(&mut self, buf: &mut BytesMut, limits: &Limits) -> Result<bool, HttpError> {
        loop {
            let budget = limits.max_header_bytes.saturating_sub(self.bytes);
            let Some(line) = self.scanner.take(buf, budget, HttpError::HeadersTooLarge)? else {
                return Ok(false);
            };
            if line.is_empty() {
                return Ok(true);
            }
            self.bytes += line.len() + 2;
            if self.bytes > limits.max_header_bytes {
                return Err(HttpError::HeadersTooLarge);
            }
            if self.fields.len() >= limits.max_headers {
                return Err(HttpError::HeadersTooLarge);
            }
            let (n, v) = parse_field_line(&line)?;
            self.fields.push_unchecked(n, v);
        }
    }
}

/// 開始行と header section。
#[derive(Debug, Default)]
pub(crate) struct HeadReader {
    start: Option<Bytes>,
    empty_lines: usize,
    scanner: LineScanner,
    fields: FieldReader,
}

impl HeadReader {
    /// 完成したら（開始行, header）。
    pub(crate) fn poll(
        &mut self,
        buf: &mut BytesMut,
        limits: &Limits,
        start_too_long: HttpError,
    ) -> Result<Option<(Bytes, HeaderMap)>, HttpError> {
        while self.start.is_none() {
            let Some(line) = self.scanner.take(buf, limits.max_start_line, start_too_long.clone())? else {
                return Ok(None);
            };
            if line.is_empty() {
                // RFC 9112 2.2: 開始行の前の空行は無視してよい（数は抑える）
                self.empty_lines += 1;
                if self.empty_lines > 8 {
                    return Err(HttpError::Malformed("leading empty lines"));
                }
                continue;
            }
            if matches!(line.first(), Some(b' ' | b'\t')) {
                return Err(HttpError::Malformed("start line begins with whitespace"));
            }
            self.start = Some(line);
        }
        if !self.fields.poll(buf, limits)? {
            return Ok(None);
        }
        let start = self.start.take().unwrap_or_default();
        let fields = std::mem::take(&mut self.fields.fields);
        *self = HeadReader::default();
        Ok(Some((start, fields)))
    }

    /// まだ何も読んでいない（空行も含めて）。
    pub(crate) fn is_idle(&self, buf: &BytesMut) -> bool {
        self.start.is_none() && buf.is_empty()
    }
}

/// Content-Length を読む（1 行のみ・数字のみ・19 桁まで）。
pub(crate) fn content_length(headers: &HeaderMap) -> Result<Option<u64>, HttpError> {
    let mut it = headers.get_all("content-length");
    let Some(v) = it.next() else { return Ok(None) };
    if it.next().is_some() {
        return Err(HttpError::Malformed("multiple content-length"));
    }
    if v.is_empty() || v.len() > 19 || !v.iter().all(u8::is_ascii_digit) {
        return Err(HttpError::Malformed("content-length"));
    }
    let mut n = 0u64;
    for &c in v.iter() {
        n = n.checked_mul(10).and_then(|n| n.checked_add((c - b'0') as u64)).ok_or(HttpError::Malformed("content-length"))?;
    }
    Ok(Some(n))
}

/// Transfer-Encoding の要素を読む。なければ `None`、最後が chunked なら `Some(true)`。
/// chunked 以外の符号化があれば `NotImplemented`（要求）か `Some(false)`（応答: 切断まで）。
pub(crate) fn transfer_encoding(headers: &HeaderMap, is_request: bool) -> Result<Option<bool>, HttpError> {
    if !headers.contains("transfer-encoding") {
        return Ok(None);
    }
    let items: Vec<&[u8]> = headers.list_items("transfer-encoding").collect();
    if items.is_empty() {
        return Err(HttpError::Malformed("empty transfer-encoding"));
    }
    let chunked_count = items.iter().filter(|t| t.eq_ignore_ascii_case(b"chunked")).count();
    let last_chunked = items.last().is_some_and(|t| t.eq_ignore_ascii_case(b"chunked"));
    if chunked_count > 1 || (chunked_count == 1 && !last_chunked) {
        return Err(HttpError::Malformed("chunked must be applied once and last"));
    }
    if items.iter().any(|t| !is_token(t)) {
        return Err(HttpError::Malformed("transfer-encoding"));
    }
    if is_request {
        if items.len() != 1 || !last_chunked {
            return Err(HttpError::NotImplemented("transfer-coding"));
        }
        return Ok(Some(true));
    }
    Ok(Some(last_chunked))
}

#[derive(Debug)]
enum ChunkState {
    SizeLine,
    Data(u64),
    DataEnd,
    Trailers,
}

/// body の復号。
#[derive(Debug)]
pub(crate) struct BodyDecoder {
    kind: BodyKind,
    received: u64,
    limit: u64,
}

#[derive(Debug)]
enum BodyKind {
    Empty,
    Length(u64),
    Chunked { state: ChunkState, scanner: LineScanner, trailers: FieldReader },
    Close,
}

pub(crate) enum BodyPoll {
    NeedMore,
    Data(Bytes),
    End(HeaderMap),
}

impl BodyDecoder {
    /// Content-Length が上限を超えていれば `BodyTooLarge`。
    pub(crate) fn new(framing: BodyFraming, limit: u64) -> Result<Self, HttpError> {
        let kind = match framing {
            BodyFraming::None | BodyFraming::Length(0) => BodyKind::Empty,
            BodyFraming::Length(n) => {
                if n > limit {
                    return Err(HttpError::BodyTooLarge);
                }
                BodyKind::Length(n)
            }
            BodyFraming::Chunked => BodyKind::Chunked {
                state: ChunkState::SizeLine,
                scanner: LineScanner::default(),
                trailers: FieldReader::default(),
            },
            BodyFraming::CloseDelimited => BodyKind::Close,
        };
        Ok(BodyDecoder { kind, received: 0, limit })
    }

    fn count(&mut self, n: u64) -> Result<(), HttpError> {
        self.received = self.received.checked_add(n).ok_or(HttpError::BodyTooLarge)?;
        if self.received > self.limit {
            return Err(HttpError::BodyTooLarge);
        }
        Ok(())
    }

    pub(crate) fn poll(&mut self, buf: &mut BytesMut, eof: bool, limits: &Limits) -> Result<BodyPoll, HttpError> {
        match &mut self.kind {
            BodyKind::Empty => Ok(BodyPoll::End(HeaderMap::new())),
            BodyKind::Length(remaining) => {
                if *remaining == 0 {
                    return Ok(BodyPoll::End(HeaderMap::new()));
                }
                if buf.is_empty() {
                    return if eof { Err(HttpError::Incomplete) } else { Ok(BodyPoll::NeedMore) };
                }
                let n = (buf.len() as u64).min(*remaining);
                *remaining -= n;
                Ok(BodyPoll::Data(buf.split_to(n as usize).freeze()))
            }
            BodyKind::Close => {
                if buf.is_empty() {
                    return if eof { Ok(BodyPoll::End(HeaderMap::new())) } else { Ok(BodyPoll::NeedMore) };
                }
                let n = buf.len() as u64;
                self.count(n)?;
                Ok(BodyPoll::Data(buf.split().freeze()))
            }
            BodyKind::Chunked { .. } => {
                let r = self.poll_chunked(buf, limits)?;
                if matches!(r, BodyPoll::NeedMore) && eof {
                    return Err(HttpError::Incomplete);
                }
                Ok(r)
            }
        }
    }

    fn poll_chunked(&mut self, buf: &mut BytesMut, limits: &Limits) -> Result<BodyPoll, HttpError> {
        loop {
            let BodyKind::Chunked { state, scanner, trailers } = &mut self.kind else {
                unreachable!("poll_chunked on non-chunked body");
            };
            match state {
                ChunkState::SizeLine => {
                    let Some(line) =
                        scanner.take(buf, limits.max_chunk_line, HttpError::Malformed("chunk line too long"))?
                    else {
                        return Ok(BodyPoll::NeedMore);
                    };
                    let size = parse_chunk_size(&line)?;
                    if size == 0 {
                        *state = ChunkState::Trailers;
                    } else {
                        *state = ChunkState::Data(size);
                        self.count(size)?;
                    }
                }
                ChunkState::Data(remaining) => {
                    if buf.is_empty() {
                        return Ok(BodyPoll::NeedMore);
                    }
                    let n = (buf.len() as u64).min(*remaining);
                    *remaining -= n;
                    if *remaining == 0 {
                        *state = ChunkState::DataEnd;
                    }
                    return Ok(BodyPoll::Data(buf.split_to(n as usize).freeze()));
                }
                ChunkState::DataEnd => {
                    if let Some(&c) = buf.first()
                        && c != b'\r'
                    {
                        return Err(HttpError::Malformed("chunk data not followed by CRLF"));
                    }
                    if buf.len() < 2 {
                        return Ok(BodyPoll::NeedMore);
                    }
                    if &buf[..2] != b"\r\n" {
                        return Err(HttpError::Malformed("chunk data not followed by CRLF"));
                    }
                    let _ = buf.split_to(2);
                    *state = ChunkState::SizeLine;
                    scanner.reset();
                }
                ChunkState::Trailers => {
                    if !trailers.poll(buf, limits)? {
                        return Ok(BodyPoll::NeedMore);
                    }
                    let t = std::mem::take(&mut trailers.fields);
                    self.kind = BodyKind::Empty;
                    return Ok(BodyPoll::End(t));
                }
            }
        }
    }
}

/// `chunk-size [ chunk-ext ]`。16 進は 1〜16 桁。chunk-ext は読み飛ばす（文字だけ検査）。
fn parse_chunk_size(line: &[u8]) -> Result<u64, HttpError> {
    let digits = line.iter().take_while(|c| c.is_ascii_hexdigit()).count();
    if digits == 0 || digits > 16 {
        return Err(HttpError::Malformed("chunk size"));
    }
    let mut n = 0u64;
    for &c in &line[..digits] {
        let d = (c as char).to_digit(16).unwrap_or(0) as u64;
        n = (n << 4) | d;
    }
    let rest = &line[digits..];
    if !rest.is_empty() {
        let ws = rest.iter().take_while(|&&c| c == b' ' || c == b'\t').count();
        let ext = &rest[ws..];
        if ext.first() != Some(&b';') {
            return Err(HttpError::Malformed("chunk size"));
        }
        if !ext.iter().all(|&c| is_field_vchar(c)) {
            return Err(HttpError::Malformed("chunk extension"));
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_size_line() {
        assert_eq!(parse_chunk_size(b"1a").unwrap(), 26);
        assert_eq!(parse_chunk_size(b"FFFFFFFFFFFFFFFF").unwrap(), u64::MAX);
        assert_eq!(parse_chunk_size(b"10;name=value").unwrap(), 16);
        assert_eq!(parse_chunk_size(b"10 ; name=\"v\"").unwrap(), 16);
        assert!(parse_chunk_size(b"").is_err());
        assert!(parse_chunk_size(b"-1").is_err());
        assert!(parse_chunk_size(b"0x10").is_err());
        assert!(parse_chunk_size(b"10000000000000000").is_err());
        assert!(parse_chunk_size(b"10 junk").is_err());
    }

    #[test]
    fn line_scanner_incremental() {
        let mut s = LineScanner::default();
        let mut b = BytesMut::new();
        for c in b"abc\r" {
            b.extend_from_slice(&[*c]);
            assert!(s.take(&mut b, 10, HttpError::UriTooLong).unwrap().is_none());
        }
        b.extend_from_slice(b"\nrest");
        assert_eq!(s.take(&mut b, 10, HttpError::UriTooLong).unwrap().unwrap(), "abc");
        assert_eq!(&b[..], b"rest");
    }

    #[test]
    fn line_scanner_rejects() {
        let mut b = BytesMut::from(&b"a\rb\r\n"[..]);
        assert!(LineScanner::default().take(&mut b, 10, HttpError::UriTooLong).is_err());
        let mut b = BytesMut::from(&b"a\nb"[..]);
        assert!(LineScanner::default().take(&mut b, 10, HttpError::UriTooLong).is_err());
        let mut b = BytesMut::from(&b"aaaaaaaaaaaaaaaa"[..]);
        assert_eq!(LineScanner::default().take(&mut b, 10, HttpError::UriTooLong), Err(HttpError::UriTooLong));
    }
}
