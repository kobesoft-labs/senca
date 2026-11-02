//! 送る HTTP 要求（[`HttpCall`]）と受け取った応答（[`HttpReply`]）。書き出しと応答の組み立てには
//! senca-http を使う。通信はしない。

use crate::error::PaasError;
use bytes::{Bytes, BytesMut};
use senca_http::{BodyFraming, Event, Limits, Method, Request, ResponseParser};
use std::fmt;

/// 接続の種類。本番は常に `Https`（TLS は呼び出し側）。`Http` は試験用の模擬サーバー向け。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }
}

/// 接続先（scheme・host・port）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    pub fn https(host: impl Into<String>) -> Self {
        Endpoint { scheme: Scheme::Https, host: host.into(), port: 443 }
    }

    /// `http(s)://host[:port]`（path は持たない）を読む。試験や VPC endpoint 向けの上書きに使う。
    pub fn parse(url: &str) -> Result<Self, PaasError> {
        let (scheme, rest) = url.split_once("://").ok_or_else(|| PaasError::config("endpoint は http(s)://host[:port]"))?;
        let scheme = match scheme {
            "https" => Scheme::Https,
            "http" => Scheme::Http,
            _ => return Err(PaasError::config("endpoint の scheme は http か https")),
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.is_empty() || rest.contains(['/', '?', '#', '@', ' ']) {
            return Err(PaasError::config("endpoint は http(s)://host[:port] の形"));
        }
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() && !h.contains(':') => {
                (h.to_string(), p.parse::<u16>().map_err(|_| PaasError::config("endpoint の port"))?)
            }
            _ => (rest.to_string(), scheme.default_port()),
        };
        if !host.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-') {
            return Err(PaasError::config("endpoint の host"));
        }
        Ok(Endpoint { scheme, host, port })
    }

    /// Host header の値（既定の port なら省く）。
    pub fn host_header(&self) -> String {
        if self.port == self.scheme.default_port() { self.host.clone() } else { format!("{}:{}", self.host, self.port) }
    }

    /// `scheme://host[:port]`
    pub fn origin(&self) -> String {
        format!("{}://{}", self.scheme.as_str(), self.host_header())
    }
}

/// RFC 3986 の unreserved 以外を `%XX`（大文字）にする。`keep_slash` なら `/` を残す（path 用）。
/// AWS SigV4 の正規化と同じ規則。
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// header の値が秘密か（`Debug` で伏せる）。
pub(crate) fn is_secret_header(name: &str) -> bool {
    ["authorization", "x-amz-security-token", "cookie", "x-api-key"].iter().any(|s| s.eq_ignore_ascii_case(name))
}

/// 送る HTTP 要求。path と query は **復号した形** で持ち、書き出し（[`HttpCall::target`]）と
/// SigV4 の正規化で同じ規則で符号化する。
#[derive(Clone, PartialEq, Eq)]
pub struct HttpCall {
    pub method: Method,
    pub endpoint: Endpoint,
    /// 復号した path（`/` で始まる。`.`・`..`・空の segment を含まない）。
    pub path: String,
    /// 復号した (名前, 値) の列。この順で書き出す。
    pub query: Vec<(String, String)>,
    /// header（Host を含む。Content-Length は書き出し時に付ける）。
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl HttpCall {
    /// Host header を付けて作る。
    pub fn new(method: Method, endpoint: Endpoint, path: impl Into<String>) -> Self {
        let host = endpoint.host_header();
        HttpCall { method, endpoint, path: path.into(), query: Vec::new(), headers: vec![("Host".into(), host)], body: Bytes::new() }
    }

    pub fn query(mut self, name: &str, value: &str) -> Self {
        self.query.push((name.to_string(), value.to_string()));
        self
    }

    /// 同じ名前の header を消してから足す。
    pub fn set_header(&mut self, name: &str, value: impl Into<String>) {
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        self.headers.push((name.to_string(), value.into()));
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// JSON の body を入れる（Content-Type も付ける）。
    pub fn json(mut self, v: &serde_json::Value) -> Self {
        self.body = Bytes::from(v.to_string());
        self.set_header("Content-Type", "application/json");
        self
    }

    /// 符号化した request-target（origin-form）。
    pub fn target(&self) -> String {
        let mut t = uri_encode(&self.path, true);
        if !self.query.is_empty() {
            t.push('?');
            let q: Vec<String> = self.query.iter().map(|(k, v)| format!("{}={}", uri_encode(k, false), uri_encode(v, false))).collect();
            t.push_str(&q.join("&"));
        }
        t
    }

    /// ログに出してよい URL（path と query に秘密は入れない）。
    pub fn url(&self) -> String {
        format!("{}{}", self.endpoint.origin(), self.target())
    }

    /// request line・header・body を HTTP/1.1 のバイト列にする。
    pub fn encode(&self) -> Result<Bytes, PaasError> {
        if !self.path.starts_with('/') || self.path.split('/').skip(1).any(|s| s == "." || s == "..") {
            return Err(PaasError::config("path が正規化されていない"));
        }
        let mut req = Request::new(self.method.clone(), self.target());
        for (n, v) in &self.headers {
            req = req.header(n, v.clone()).map_err(|_| PaasError::config("header の名前か値が不正"))?;
        }
        let framing = if self.body.is_empty() && matches!(self.method, Method::Get | Method::Head | Method::Delete) {
            BodyFraming::None
        } else {
            BodyFraming::Length(self.body.len() as u64)
        };
        let head = req.encode_head(framing).map_err(|_| PaasError::config("要求を書き出せない"))?;
        let mut out = BytesMut::with_capacity(head.len() + self.body.len());
        out.extend_from_slice(&head);
        out.extend_from_slice(&self.body);
        Ok(out.freeze())
    }
}

impl fmt::Debug for HttpCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<String> = self
            .headers
            .iter()
            .map(|(n, v)| if is_secret_header(n) { format!("{n}: <redacted>") } else { format!("{n}: {v}") })
            .collect();
        f.debug_struct("HttpCall")
            .field("method", &self.method.as_str())
            .field("url", &self.url())
            .field("headers", &headers)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// 受け取った応答。body は stream key などの秘密を含みうるので `Debug` では長さだけ出す。
#[derive(Clone, PartialEq, Eq)]
pub struct HttpReply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl HttpReply {
    pub fn new(status: u16, body: impl Into<Bytes>) -> Self {
        HttpReply { status, headers: Vec::new(), body: body.into() }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// body を JSON として読む（空なら `Null`）。
    pub fn json(&self) -> Result<serde_json::Value, PaasError> {
        if self.body.iter().all(u8::is_ascii_whitespace) {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&self.body).map_err(|_| PaasError::malformed("応答が JSON ではない"))
    }
}

impl fmt::Debug for HttpReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpReply").field("status", &self.status).field("body_len", &self.body.len()).finish()
    }
}

/// 応答の上限（一覧の応答も収まる大きさ）。
pub const REPLY_LIMITS: Limits = Limits {
    max_start_line: 8 * 1024,
    max_header_bytes: 64 * 1024,
    max_headers: 200,
    max_body: 4 * 1024 * 1024,
    max_chunk_line: 4 * 1024,
    max_buffer: 256 * 1024,
};

/// 受信したバイト列から [`HttpReply`] を組み立てる（senca-http の `ResponseParser` を使う）。
/// 1xx は読み飛ばす。
#[derive(Debug)]
pub struct ReplyReader {
    parser: ResponseParser,
    head: Option<(u16, Vec<(String, String)>)>,
    body: BytesMut,
    interim: bool,
}

impl ReplyReader {
    /// `method` は送った要求のメソッド（HEAD の応答は body を持たない）。
    pub fn new(method: &Method) -> Self {
        let mut parser = ResponseParser::new(REPLY_LIMITS);
        parser.push_request(method.clone());
        ReplyReader { parser, head: None, body: BytesMut::new(), interim: false }
    }

    /// 受信したバイトを渡す。応答が揃えば返す。
    pub fn feed(&mut self, data: &[u8]) -> Result<Option<HttpReply>, PaasError> {
        for chunk in data.chunks(16 * 1024) {
            self.parser.feed(chunk).map_err(|_| PaasError::malformed("応答が大きすぎる"))?;
            if let Some(r) = self.poll()? {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    /// 相手が閉じた。
    pub fn feed_eof(&mut self) -> Result<Option<HttpReply>, PaasError> {
        self.parser.feed_eof();
        match self.poll()? {
            Some(r) => Ok(Some(r)),
            None => Err(PaasError::transport("応答の途中で切れた")),
        }
    }

    fn poll(&mut self) -> Result<Option<HttpReply>, PaasError> {
        loop {
            let ev = self.parser.poll().map_err(|e| match e {
                senca_http::HttpError::Incomplete => PaasError::transport("応答の途中で切れた"),
                _ => PaasError::malformed("HTTP 応答の形が不正"),
            })?;
            match ev {
                Event::NeedMore => return Ok(None),
                Event::Head(h) => {
                    if (100..200).contains(&h.status) {
                        // 1xx は head だけで終わる（続く End も捨てる）
                        self.interim = true;
                        continue;
                    }
                    let headers = h.headers.iter().map(|(n, v)| (n.to_string(), String::from_utf8_lossy(v).into_owned())).collect();
                    self.head = Some((h.status, headers));
                }
                Event::Data(d) => {
                    if self.body.len() + d.len() > REPLY_LIMITS.max_body as usize {
                        return Err(PaasError::malformed("応答が大きすぎる"));
                    }
                    self.body.extend_from_slice(&d);
                }
                Event::End(_) if self.interim => self.interim = false,
                Event::End(_) => {
                    let (status, headers) = self.head.take().ok_or_else(|| PaasError::malformed("応答に head が無い"))?;
                    return Ok(Some(HttpReply { status, headers, body: std::mem::take(&mut self.body).freeze() }));
                }
                Event::Closed => return Err(PaasError::transport("応答の前に切れた")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parse() {
        let e = Endpoint::parse("http://127.0.0.1:8080").unwrap();
        assert_eq!((e.scheme, e.host.as_str(), e.port), (Scheme::Http, "127.0.0.1", 8080));
        assert_eq!(e.host_header(), "127.0.0.1:8080");
        let e = Endpoint::parse("https://ivs.ap-northeast-1.amazonaws.com/").unwrap();
        assert_eq!(e.port, 443);
        assert_eq!(e.host_header(), "ivs.ap-northeast-1.amazonaws.com");
        for bad in ["ftp://x", "https://", "https://a/b", "https://user@x", "https://x:99999", "x"] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn encode_and_redact() {
        let mut c = HttpCall::new(Method::Post, Endpoint::https("api.example.com"), "/a b/c")
            .query("k", "v w")
            .json(&serde_json::json!({"x": 1}));
        c.set_header("Authorization", "Bearer TOPSECRET");
        assert_eq!(c.target(), "/a%20b/c?k=v%20w");
        let wire = c.encode().unwrap();
        let s = std::str::from_utf8(&wire).unwrap();
        assert!(s.starts_with("POST /a%20b/c?k=v%20w HTTP/1.1\r\nHost: api.example.com\r\n"));
        assert!(s.contains("Content-Length: 7\r\n"));
        assert!(s.ends_with("\r\n\r\n{\"x\":1}"));
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("TOPSECRET"));
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn reader_assembles_reply() {
        let mut r = ReplyReader::new(&Method::Get);
        assert!(r.feed(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: b\r\n\r\nhe").unwrap().is_none());
        let rep = r.feed(b"llo").unwrap().unwrap();
        assert_eq!(rep.status, 200);
        assert_eq!(rep.header("x-a"), Some("b"));
        assert_eq!(&rep.body[..], b"hello");
        assert_eq!(format!("{rep:?}"), "HttpReply { status: 200, body_len: 5 }");

        let mut r = ReplyReader::new(&Method::Get);
        assert!(r.feed(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhe").unwrap().is_none());
        assert!(r.feed_eof().unwrap_err().is_retryable());
    }
}
