//! 外部サービスとのやり取りの形（sans-io）。送受信はノードの HTTP クライアントが行う。
//!
//! - [`Endpoint`] — 送り先（`http(s)://host[:port][/base]`）。既定値はない。
//! - [`HttpCall`] — 送る要求（method・target・header・body）。秘密の header は表示に出さない。
//! - [`HttpReply`] — 受けた応答（状態コード・Retry-After・body）。
//! - [`ReplyReader`] — `senca-http` の [`ResponseParser`] で応答を集める（上限付き）。
//! - [`Multipart`] — multipart/form-data（RFC 7578）の組み立て。

use crate::error::{Error, Result};
use crate::secret::Secret;
use bytes::{Bytes, BytesMut};
use senca_http::{BodyFraming, Event, Limits, Method, Request, ResponseHead, ResponseParser};
use std::fmt;

/// 送り先の scheme。`Https` は TLS（`senca-tls`）で包んで送る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

/// 送り先。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub scheme: Scheme,
    /// ホスト名・IPv4・`[IPv6]`。
    pub host: String,
    pub port: u16,
    /// 先頭が `/`、末尾の `/` なし（空でもよい）。
    pub base_path: String,
}

impl Endpoint {
    /// `http://host[:port][/path]` または `https://…` を読む。userinfo・query・fragment は拒否する。
    pub fn parse(url: &str) -> Result<Endpoint> {
        if url.len() > 2048 || !url.bytes().all(|c| c.is_ascii_graphic()) {
            return Err(Error::InvalidConfig("endpoint URL"));
        }
        let (scheme, rest) = if let Some(r) = url.strip_prefix("https://") {
            (Scheme::Https, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (Scheme::Http, r)
        } else {
            return Err(Error::InvalidConfig("endpoint URL must start with http:// or https://"));
        };
        if rest.contains(['@', '?', '#', '\\']) {
            return Err(Error::InvalidConfig("endpoint URL must not have userinfo, query or fragment"));
        }
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let default_port = if scheme == Scheme::Https { 443 } else { 80 };
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let end = v6.find(']').ok_or(Error::InvalidConfig("endpoint host"))?;
            let host = &authority[..end + 2];
            let tail = &v6[end + 1..];
            let port = match tail.strip_prefix(':') {
                Some(p) => parse_port(p)?,
                None if tail.is_empty() => default_port,
                None => return Err(Error::InvalidConfig("endpoint host")),
            };
            if !v6[..end].bytes().all(|c| c.is_ascii_hexdigit() || c == b':' || c == b'.') {
                return Err(Error::InvalidConfig("endpoint host"));
            }
            (host.to_string(), port)
        } else {
            let (h, p) = match authority.rfind(':') {
                Some(i) => (&authority[..i], parse_port(&authority[i + 1..])?),
                None => (authority, default_port),
            };
            if h.is_empty() || h.len() > 253 || !h.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-') {
                return Err(Error::InvalidConfig("endpoint host"));
            }
            (h.to_ascii_lowercase(), p)
        };
        let base_path = path.trim_end_matches('/').to_string();
        if base_path.contains("//") || base_path.split('/').any(|s| s == ".." || s == ".") {
            return Err(Error::InvalidConfig("endpoint path"));
        }
        Ok(Endpoint { scheme, host, port, base_path })
    }

    /// Host header の値（既定のポートは省く）。
    pub fn authority(&self) -> String {
        let default = if self.scheme == Scheme::Https { 443 } else { 80 };
        if self.port == default { self.host.clone() } else { format!("{}:{}", self.host, self.port) }
    }

    /// base_path の後ろに `path`（`/` で始まる）をつなげた request target。
    pub fn target(&self, path: &str) -> String {
        format!("{}{}", self.base_path, path)
    }

    /// 表示用の URL（秘密は入っていない）。
    pub fn url(&self) -> String {
        let s = if self.scheme == Scheme::Https { "https" } else { "http" };
        format!("{s}://{}{}", self.authority(), self.base_path)
    }
}

fn parse_port(p: &str) -> Result<u16> {
    match p.parse::<u16>() {
        Ok(v) if v != 0 && !p.starts_with('+') => Ok(v),
        _ => Err(Error::InvalidConfig("endpoint port")),
    }
}

/// header の値。秘密の値は表示に出さない。
#[derive(Clone)]
pub enum HeaderValue {
    Plain(String),
    Secret(Secret),
}

impl fmt::Debug for HeaderValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeaderValue::Plain(s) => write!(f, "{s:?}"),
            HeaderValue::Secret(_) => f.write_str("[REDACTED]"),
        }
    }
}

/// 送る要求。ノードは [`HttpCall::endpoint`] に接続し、[`HttpCall::encode_head`] と [`HttpCall::body`] を書き、
/// [`HttpCall::reader`] で応答を読む。
#[derive(Debug, Clone)]
pub struct HttpCall {
    pub endpoint: Endpoint,
    pub method: Method,
    pub target: String,
    /// Host・Content-Length 以外の header。
    pub headers: Vec<(String, HeaderValue)>,
    pub body: Bytes,
    /// 応答の body の上限（これを超える応答は拒否する）。
    pub max_response_bytes: usize,
    /// ノードが使うべき時間切れ（ms）。
    pub timeout_ms: u64,
}

impl HttpCall {
    pub fn post(endpoint: &Endpoint, path: &str, content_type: &str, body: Bytes) -> HttpCall {
        HttpCall {
            endpoint: endpoint.clone(),
            method: Method::Post,
            target: endpoint.target(path),
            headers: vec![("Content-Type".into(), HeaderValue::Plain(content_type.into()))],
            body,
            max_response_bytes: 1 << 20,
            timeout_ms: 30_000,
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), HeaderValue::Plain(value.into())));
        self
    }

    pub fn secret_header(mut self, name: &str, value: Secret) -> Self {
        self.headers.push((name.into(), HeaderValue::Secret(value)));
        self
    }

    /// request line と header（Host・Content-Length を付ける）。ここだけが秘密を平文にする。
    pub fn encode_head(&self) -> Result<Bytes> {
        let mut r = Request::new(self.method.clone(), self.target.clone()).header("Host", self.endpoint.authority())?;
        for (n, v) in &self.headers {
            let v = match v {
                HeaderValue::Plain(s) => s.clone(),
                HeaderValue::Secret(s) => s.expose().to_string(),
            };
            r = r.header(n, v).map_err(|_| Error::InvalidInput("header value"))?;
        }
        Ok(r.encode_head(BodyFraming::Length(self.body.len() as u64))?)
    }

    /// 要求の全体（head + body）。
    pub fn encode(&self) -> Result<Vec<u8>> {
        let head = self.encode_head()?;
        let mut out = Vec::with_capacity(head.len() + self.body.len());
        out.extend_from_slice(&head);
        out.extend_from_slice(&self.body);
        Ok(out)
    }

    /// 応答を読む状態機械（上限付き）。
    pub fn reader(&self) -> ReplyReader {
        ReplyReader::new(self.max_response_bytes)
    }

    /// header の値（試験・ログ用。秘密の header は `None`）。
    pub fn plain_header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).and_then(|(_, v)| match v {
            HeaderValue::Plain(s) => Some(s.as_str()),
            HeaderValue::Secret(_) => None,
        })
    }
}

/// 受けた応答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReply {
    pub status: u16,
    /// Retry-After（秒。delta-seconds の形だけ読む）。
    pub retry_after_s: Option<u64>,
    pub content_type: Option<String>,
    pub body: Bytes,
}

impl HttpReply {
    pub fn new(status: u16, body: impl Into<Bytes>) -> Self {
        HttpReply { status, retry_after_s: None, content_type: None, body: body.into() }
    }

    pub fn from_head(head: &ResponseHead, body: Bytes) -> Self {
        let retry_after_s = head.headers.get_str("retry-after").and_then(|v| v.trim().parse::<u64>().ok());
        let content_type = head.headers.get_str("content-type").map(|s| s.to_string());
        HttpReply { status: head.status, retry_after_s, content_type, body }
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// 応答を集める（sans-io）。受けたバイト列を [`ReplyReader::feed`] に入れ、`Some` が返れば完了。
#[derive(Debug)]
pub struct ReplyReader {
    parser: ResponseParser,
    head: Option<ResponseHead>,
    body: BytesMut,
    max: usize,
    done: Option<HttpReply>,
}

impl ReplyReader {
    pub fn new(max_body: usize) -> Self {
        let limits = Limits { max_body: max_body as u64, max_buffer: max_body.saturating_add(64 * 1024), ..Limits::DEFAULT };
        ReplyReader { parser: ResponseParser::new(limits), head: None, body: BytesMut::new(), max: max_body, done: None }
    }

    /// 受けたバイト列を入れる。応答がそろえば返す。
    pub fn feed(&mut self, data: &[u8]) -> Result<Option<HttpReply>> {
        if let Some(r) = &self.done {
            return Ok(Some(r.clone()));
        }
        self.parser.feed(data).map_err(|_| Error::TooLarge("response"))?;
        self.drive()
    }

    /// 相手が接続を閉じた。
    pub fn feed_eof(&mut self) -> Result<HttpReply> {
        if let Some(r) = &self.done {
            return Ok(r.clone());
        }
        self.parser.feed_eof();
        self.drive()?.ok_or(Error::Http("connection closed before the response completed".into()))
    }

    fn drive(&mut self) -> Result<Option<HttpReply>> {
        loop {
            match self.parser.poll().map_err(|e| match e {
                senca_http::HttpError::BodyTooLarge => Error::TooLarge("response body"),
                e => Error::from(e),
            })? {
                Event::NeedMore => return Ok(None),
                Event::Head(h) => {
                    if (100..200).contains(&h.status) {
                        continue;
                    }
                    if let BodyFraming::Length(n) = h.body
                        && n > self.max as u64
                    {
                        return Err(Error::TooLarge("response body"));
                    }
                    self.head = Some(h);
                }
                Event::Data(d) => {
                    if self.body.len() + d.len() > self.max {
                        return Err(Error::TooLarge("response body"));
                    }
                    self.body.extend_from_slice(&d);
                }
                Event::End(_) | Event::Closed => {
                    let Some(h) = self.head.take() else {
                        return Err(Error::Http("no response".into()));
                    };
                    let r = HttpReply::from_head(&h, std::mem::take(&mut self.body).freeze());
                    self.done = Some(r.clone());
                    return Ok(Some(r));
                }
            }
        }
    }
}

/// multipart/form-data の組み立て（RFC 7578）。
#[derive(Debug, Clone)]
pub struct Multipart {
    seed: u64,
    parts: Vec<Part>,
}

#[derive(Debug, Clone)]
struct Part {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    data: Bytes,
}

fn check_param(s: &str) -> Result<()> {
    if s.is_empty() || s.len() > 200 || !s.bytes().all(|c| (0x20..0x7f).contains(&c) && c != b'"' && c != b'\\') {
        return Err(Error::InvalidInput("multipart name"));
    }
    Ok(())
}

impl Multipart {
    /// `seed` は境界の文字列を決める値（同じ入力なら同じ出力になる）。
    pub fn new(seed: u64) -> Self {
        Multipart { seed, parts: Vec::new() }
    }

    pub fn text(&mut self, name: &str, value: &str) -> Result<&mut Self> {
        check_param(name)?;
        self.parts.push(Part { name: name.into(), filename: None, content_type: None, data: Bytes::copy_from_slice(value.as_bytes()) });
        Ok(self)
    }

    pub fn file(&mut self, name: &str, filename: &str, content_type: &str, data: Bytes) -> Result<&mut Self> {
        check_param(name)?;
        check_param(filename)?;
        check_param(content_type)?;
        self.parts.push(Part { name: name.into(), filename: Some(filename.into()), content_type: Some(content_type.into()), data });
        Ok(self)
    }

    /// (Content-Type の値, body)。境界の文字列はどの部分の中身にも現れないものを選ぶ。
    pub fn finish(&self) -> Result<(String, Bytes)> {
        let mut boundary = None;
        for i in 0..64u64 {
            let b = format!("senca-{:016x}{:02x}", self.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(i), i);
            if !self.parts.iter().any(|p| contains(&p.data, b.as_bytes())) {
                boundary = Some(b);
                break;
            }
        }
        let boundary = boundary.ok_or(Error::InvalidInput("no usable multipart boundary"))?;
        let mut out = Vec::with_capacity(self.parts.iter().map(|p| p.data.len() + 160).sum());
        for p in &self.parts {
            out.extend_from_slice(b"--");
            out.extend_from_slice(boundary.as_bytes());
            out.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
            out.extend_from_slice(p.name.as_bytes());
            out.push(b'"');
            if let Some(f) = &p.filename {
                out.extend_from_slice(b"; filename=\"");
                out.extend_from_slice(f.as_bytes());
                out.push(b'"');
            }
            out.extend_from_slice(b"\r\n");
            if let Some(ct) = &p.content_type {
                out.extend_from_slice(b"Content-Type: ");
                out.extend_from_slice(ct.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(&p.data);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"--\r\n");
        Ok((format!("multipart/form-data; boundary={boundary}"), Bytes::from(out)))
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && hay.windows(needle.len()).any(|w| w == needle)
}

/// multipart の 1 部分: (name, filename, content_type, data)。
pub type FormPart = (String, Option<String>, Option<String>, Vec<u8>);

/// 試験用: multipart の body を部分に分ける。
pub fn parse_multipart(content_type: &str, body: &[u8]) -> Result<Vec<FormPart>> {
    let boundary = content_type
        .split(';')
        .find_map(|p| p.trim().strip_prefix("boundary="))
        .ok_or(Error::Malformed("no boundary".into()))?;
    let delim = format!("--{boundary}");
    let mut out = Vec::new();
    let mut rest = body;
    let start = find(rest, delim.as_bytes()).ok_or(Error::Malformed("no first boundary".into()))?;
    rest = &rest[start + delim.len()..];
    loop {
        if rest.starts_with(b"--") {
            return Ok(out);
        }
        rest = rest.strip_prefix(b"\r\n").ok_or(Error::Malformed("boundary line".into()))?;
        let hend = find(rest, b"\r\n\r\n").ok_or(Error::Malformed("part headers".into()))?;
        let headers = std::str::from_utf8(&rest[..hend]).map_err(|_| Error::Malformed("part headers".into()))?;
        let body = &rest[hend + 4..];
        let next = find(body, format!("\r\n{delim}").as_bytes()).ok_or(Error::Malformed("no closing boundary".into()))?;
        let data = body[..next].to_vec();
        rest = &body[next + 2 + delim.len()..];
        let mut name = None;
        let mut filename = None;
        let mut ctype = None;
        for line in headers.split("\r\n") {
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("content-disposition:") {
                for param in line.split(';').skip(1) {
                    let param = param.trim();
                    if let Some(v) = param.strip_prefix("name=") {
                        name = Some(v.trim_matches('"').to_string());
                    } else if let Some(v) = param.strip_prefix("filename=") {
                        filename = Some(v.trim_matches('"').to_string());
                    }
                }
            } else if lower.starts_with("content-type:") {
                ctype = Some(line["content-type:".len()..].trim().to_string());
            }
        }
        out.push((name.ok_or(Error::Malformed("part without name".into()))?, filename, ctype, data));
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// JSON の body を作る。
pub(crate) fn json_body(v: &serde_json::Value) -> Bytes {
    Bytes::from(serde_json::to_vec(v).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parsing() {
        let e = Endpoint::parse("https://api.openai.com/v1/").unwrap();
        assert_eq!((e.scheme, e.host.as_str(), e.port, e.base_path.as_str()), (Scheme::Https, "api.openai.com", 443, "/v1"));
        assert_eq!(e.target("/audio/transcriptions"), "/v1/audio/transcriptions");
        assert_eq!(e.authority(), "api.openai.com");
        let e = Endpoint::parse("http://127.0.0.1:8080").unwrap();
        assert_eq!((e.port, e.base_path.as_str(), e.authority()), (8080, "", "127.0.0.1:8080".to_string()));
        let e = Endpoint::parse("http://[::1]:9000/x").unwrap();
        assert_eq!((e.host.as_str(), e.port, e.authority()), ("[::1]", 9000, "[::1]:9000".to_string()));
        for bad in [
            "ftp://x",
            "https://user:pw@host/",
            "https://host/?q=1",
            "https://host:0/",
            "https://host:99999/",
            "https://ho st/",
            "https:///x",
            "https://host/../x",
            "https://host#f",
        ] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn call_encoding_hides_secret_in_debug() {
        let e = Endpoint::parse("http://localhost:8080/base").unwrap();
        let c = HttpCall::post(&e, "/p", "application/json", Bytes::from_static(b"{}"))
            .secret_header("Authorization", Secret::new("Bearer sk-123456"));
        let head = c.encode_head().unwrap();
        let s = std::str::from_utf8(&head).unwrap();
        assert!(s.starts_with("POST /base/p HTTP/1.1\r\n"));
        assert!(s.contains("Host: localhost:8080\r\n"));
        assert!(s.contains("Authorization: Bearer sk-123456\r\n"));
        assert!(s.contains("Content-Length: 2\r\n"));
        assert!(!format!("{c:?}").contains("sk-123456"));
        assert_eq!(c.plain_header("authorization"), None);
        assert_eq!(c.plain_header("content-type"), Some("application/json"));
        // header に改行は入れられない
        let bad = HttpCall::post(&e, "/p", "a", Bytes::new()).header("X", "a\r\nb: c");
        assert!(bad.encode_head().is_err());
    }

    #[test]
    fn multipart_roundtrip_and_boundary_choice() {
        let mut m = Multipart::new(42);
        m.text("model", "whisper-1").unwrap();
        m.text("timestamp_granularities[]", "segment").unwrap();
        // 中身に最初の候補の境界を入れておく → 別の境界が選ばれる
        let first = format!("senca-{:016x}{:02x}", 42u64.wrapping_mul(0x9E37_79B9_7F4A_7C15), 0);
        let data = Bytes::from(format!("xx{first}yy").into_bytes());
        m.file("file", "chunk-0.m4a", "audio/mp4", data.clone()).unwrap();
        let (ct, body) = m.finish().unwrap();
        assert!(ct.starts_with("multipart/form-data; boundary=senca-"));
        assert!(!ct.contains(&first));
        let parts = parse_multipart(&ct, &body).unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].0, "model");
        assert_eq!(parts[0].3, b"whisper-1");
        assert_eq!(parts[1].0, "timestamp_granularities[]");
        assert_eq!(parts[2].1.as_deref(), Some("chunk-0.m4a"));
        assert_eq!(parts[2].2.as_deref(), Some("audio/mp4"));
        assert_eq!(parts[2].3, data.to_vec());
        assert!(Multipart::new(0).text("a\"b", "x").is_err());
        assert!(Multipart::new(0).file("f", "a\r\n", "x", Bytes::new()).is_err());
    }

    #[test]
    fn reply_reader_limits() {
        let mut r = ReplyReader::new(16);
        let reply = r.feed(b"HTTP/1.1 429 Too Many\r\nRetry-After: 3\r\nContent-Length: 2\r\n\r\nno").unwrap().unwrap();
        assert_eq!((reply.status, reply.retry_after_s, &reply.body[..]), (429, Some(3), &b"no"[..]));
        let mut r = ReplyReader::new(16);
        assert!(matches!(r.feed(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n"), Err(Error::TooLarge(_))));
        // chunked で上限を超える
        let mut r = ReplyReader::new(4);
        let res = r.feed(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n");
        assert!(res.is_err());
        // 閉じるまでの body
        let mut r = ReplyReader::new(16);
        assert!(r.feed(b"HTTP/1.1 200 OK\r\n\r\nhello").unwrap().is_none());
        assert_eq!(&r.feed_eof().unwrap().body[..], b"hello");
        // 途中で切れた
        let mut r = ReplyReader::new(16);
        r.feed(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nab").unwrap();
        assert!(r.feed_eof().is_err());
    }
}
