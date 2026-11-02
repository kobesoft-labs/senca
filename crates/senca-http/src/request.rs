//! 要求の解析（サーバー側）と生成（クライアント側）。

use crate::error::HttpError;
use crate::headers::{HeaderMap, is_token};
use crate::parse::{
    BodyDecoder, BodyFraming, BodyPoll, Event, HeadReader, Limits, Version, content_length, transfer_encoding,
};
use bytes::{Bytes, BytesMut};
use std::fmt;

/// 要求メソッド。大文字小文字を区別する（RFC 9110 9.1）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Options,
    Connect,
    Trace,
    /// 上記以外の token。
    Other(String),
}

impl Method {
    pub fn as_str(&self) -> &str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
            Method::Options => "OPTIONS",
            Method::Connect => "CONNECT",
            Method::Trace => "TRACE",
            Method::Other(s) => s,
        }
    }

    /// token でなければ `Err`。
    pub fn parse(s: &[u8]) -> Result<Method, HttpError> {
        Ok(match s {
            b"GET" => Method::Get,
            b"HEAD" => Method::Head,
            b"POST" => Method::Post,
            b"PUT" => Method::Put,
            b"PATCH" => Method::Patch,
            b"DELETE" => Method::Delete,
            b"OPTIONS" => Method::Options,
            b"CONNECT" => Method::Connect,
            b"TRACE" => Method::Trace,
            _ if is_token(s) && s.len() <= 32 => Method::Other(String::from_utf8_lossy(s).into_owned()),
            _ => return Err(HttpError::Malformed("method")),
        })
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 解析済みの要求の先頭部分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: Method,
    /// request-target そのまま（検査済み、ASCII）。
    pub target: String,
    pub version: Version,
    pub headers: HeaderMap,
    /// body の枠組み（要求では `CloseDelimited` にならない）。
    pub body: BodyFraming,
    /// この要求の後も接続を使い続けてよいか（版と Connection header による）。
    pub keep_alive: bool,
    /// `Expect: 100-continue`（HTTP/1.1 のみ。HTTP/1.0 では無視する）。
    pub expect_continue: bool,
}

impl RequestHead {
    /// origin-form の path 部分（`?` の前）。それ以外の形では target 全体。
    pub fn path(&self) -> &str {
        self.target.split_once('?').map_or(self.target.as_str(), |(p, _)| p)
    }

    /// `?` の後ろ。
    pub fn query(&self) -> Option<&str> {
        self.target.split_once('?').map(|(_, q)| q)
    }

    /// Content-Length（chunked なら `None`）。
    pub fn content_length(&self) -> Option<u64> {
        match self.body {
            BodyFraming::Length(n) => Some(n),
            _ => None,
        }
    }

    /// body を持つか。
    pub fn has_body(&self) -> bool {
        matches!(self.body, BodyFraming::Chunked) || matches!(self.body, BodyFraming::Length(n) if n > 0)
    }
}

/// request-target に使える文字（RFC 3986 の unreserved・sub-delims・`:@/?[]` と `%`）。
fn is_target_char(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            b'-' | b'.' | b'_' | b'~' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'='
                | b':' | b'@' | b'/' | b'?' | b'[' | b']' | b'%'
        )
}

/// request-target を検査する（文字・%HH・形式とメソッドの組み合わせ）。
pub(crate) fn validate_target(method: &Method, t: &[u8]) -> Result<(), HttpError> {
    if t.is_empty() {
        return Err(HttpError::Malformed("empty target"));
    }
    if !t.iter().all(|&c| is_target_char(c)) {
        return Err(HttpError::Malformed("target character"));
    }
    let mut i = 0;
    while i < t.len() {
        if t[i] == b'%' {
            if i + 2 >= t.len() || !t[i + 1].is_ascii_hexdigit() || !t[i + 2].is_ascii_hexdigit() {
                return Err(HttpError::Malformed("target percent-encoding"));
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    match (method, t[0]) {
        (Method::Connect, _) => {
            // authority-form: host:port
            if t.contains(&b'/') || !t.contains(&b':') {
                return Err(HttpError::Malformed("connect target"));
            }
        }
        (_, b'/') => {}
        (Method::Options, b'*') if t.len() == 1 => {}
        (_, c) if c.is_ascii_alphabetic() => {
            // absolute-form: scheme "://" ...
            let scheme_end = t.iter().position(|&c| c == b':').ok_or(HttpError::Malformed("target form"))?;
            let scheme = &t[..scheme_end];
            if !scheme.iter().all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
                || !t[scheme_end..].starts_with(b"://")
            {
                return Err(HttpError::Malformed("target form"));
            }
        }
        _ => return Err(HttpError::Malformed("target form")),
    }
    Ok(())
}

fn parse_request_line(line: &[u8]) -> Result<(Method, String, Version), HttpError> {
    let mut parts = line.split(|&c| c == b' ');
    let (Some(m), Some(t), Some(v), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(HttpError::Malformed("request line"));
    };
    let method = Method::parse(m)?;
    let version = Version::parse(v)?;
    validate_target(&method, t)?;
    let target = String::from_utf8_lossy(t).into_owned();
    Ok((method, target, version))
}

/// 要求の header から枠組み・keep-alive・Expect を決める（RFC 9112 6.1・6.3・9.3、RFC 9110 10.1.1）。
fn build_head(method: Method, target: String, version: Version, headers: HeaderMap) -> Result<RequestHead, HttpError> {
    let hosts = headers.get_all("host").count();
    if hosts > 1 || (version == Version::Http11 && hosts == 0) {
        return Err(HttpError::Malformed("host header"));
    }
    let cl = content_length(&headers)?;
    let te = transfer_encoding(&headers, true)?;
    if te.is_some() && cl.is_some() {
        return Err(HttpError::Malformed("both content-length and transfer-encoding"));
    }
    if te.is_some() && version == Version::Http10 {
        return Err(HttpError::Malformed("transfer-encoding in HTTP/1.0"));
    }
    let body = match (te, cl) {
        (Some(_), _) => BodyFraming::Chunked,
        (None, Some(n)) => BodyFraming::Length(n),
        (None, None) => BodyFraming::None,
    };
    let close = headers.has_token("connection", "close");
    let keep_alive = match version {
        Version::Http11 => !close,
        Version::Http10 => !close && headers.has_token("connection", "keep-alive"),
    };
    let mut expect_continue = false;
    if headers.contains("expect") {
        for item in headers.list_items("expect") {
            if !item.eq_ignore_ascii_case(b"100-continue") {
                return Err(HttpError::ExpectationFailed);
            }
        }
        expect_continue = version == Version::Http11;
    }
    Ok(RequestHead { method, target, version, headers, body, keep_alive, expect_continue })
}

#[derive(Debug)]
enum State {
    Head,
    BodyStart(BodyFraming),
    Body(BodyDecoder),
    Failed(HttpError),
}

/// 要求の逐次解析器（サーバー側）。`feed` でバイトを入れ、`poll` で出来事を取り出す。
///
/// 1 つの要求について `Head` → `Data`* → `End` の順に返し、続けて次の要求（pipelining）を読む。
/// 誤りを返した後は同じ誤りを返し続ける（応答して接続を閉じること）。
#[derive(Debug)]
pub struct RequestParser {
    buf: BytesMut,
    limits: Limits,
    body_limit: u64,
    head: HeadReader,
    state: State,
    eof: bool,
}

pub type RequestEvent = Event<RequestHead>;

impl RequestParser {
    pub fn new(limits: Limits) -> Self {
        RequestParser {
            buf: BytesMut::new(),
            limits,
            body_limit: limits.max_body,
            head: HeadReader::default(),
            state: State::Head,
            eof: false,
        }
    }

    /// 受信したバイトを足す。未処理の量が `max_buffer` を超えると `Err`。
    pub fn feed(&mut self, data: &[u8]) -> Result<(), HttpError> {
        if self.buf.len().saturating_add(data.len()) > self.limits.max_buffer {
            return Err(match self.state {
                State::Head => HttpError::HeadersTooLarge,
                _ => HttpError::Malformed("receive buffer overflow"),
            });
        }
        self.buf.extend_from_slice(data);
        Ok(())
    }

    /// 相手が送信を閉じた。
    pub fn feed_eof(&mut self) {
        self.eof = true;
    }

    /// 次の body に使う上限（`Head` を受け取った後、最初の `poll` より前に呼べばその要求に効く）。
    /// 要求が終わると `Limits::max_body` に戻る。
    pub fn set_body_limit(&mut self, limit: u64) {
        self.body_limit = limit;
    }

    /// 未処理のバイト数。
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    pub fn poll(&mut self) -> Result<RequestEvent, HttpError> {
        let r = self.poll_inner();
        if let Err(e) = &r {
            self.state = State::Failed(e.clone());
        }
        r
    }

    fn poll_inner(&mut self) -> Result<RequestEvent, HttpError> {
        loop {
            match &mut self.state {
                State::Failed(e) => return Err(e.clone()),
                State::Head => {
                    if self.eof && self.head.is_idle(&self.buf) {
                        return Ok(Event::Closed);
                    }
                    match self.head.poll(&mut self.buf, &self.limits, HttpError::UriTooLong)? {
                        None => {
                            return if self.eof { Err(HttpError::Incomplete) } else { Ok(Event::NeedMore) };
                        }
                        Some((line, headers)) => {
                            let (method, target, version) = parse_request_line(&line)?;
                            let head = build_head(method, target, version, headers)?;
                            self.state = State::BodyStart(head.body);
                            return Ok(Event::Head(head));
                        }
                    }
                }
                State::BodyStart(framing) => {
                    let d = BodyDecoder::new(*framing, self.body_limit)?;
                    self.state = State::Body(d);
                }
                State::Body(d) => match d.poll(&mut self.buf, self.eof, &self.limits)? {
                    BodyPoll::NeedMore => return Ok(Event::NeedMore),
                    BodyPoll::Data(b) => return Ok(Event::Data(b)),
                    BodyPoll::End(t) => {
                        self.state = State::Head;
                        self.body_limit = self.limits.max_body;
                        return Ok(Event::End(t));
                    }
                },
            }
        }
    }
}

/// 送る要求（クライアント側）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    pub target: String,
    pub headers: HeaderMap,
}

impl Request {
    pub fn new(method: Method, target: impl Into<String>) -> Self {
        Request { method, target: target.into(), headers: HeaderMap::new() }
    }

    /// header を足す（検査付き）。
    pub fn header(mut self, name: &str, value: impl Into<Bytes>) -> Result<Self, HttpError> {
        self.headers.append(name, value)?;
        Ok(self)
    }

    /// request line と header を書き出す。Content-Length・Transfer-Encoding は `framing` から付ける
    /// （自分で入れていたら `Err`）。Host は呼び出し側が入れる（なければ `Err`）。
    pub fn encode_head(&self, framing: BodyFraming) -> Result<Bytes, HttpError> {
        validate_target(&self.method, self.target.as_bytes()).map_err(|_| HttpError::InvalidInput("target"))?;
        if self.headers.contains("content-length") || self.headers.contains("transfer-encoding") {
            return Err(HttpError::InvalidInput("framing headers are generated"));
        }
        if self.headers.get_all("host").count() != 1 {
            return Err(HttpError::InvalidInput("exactly one host header required"));
        }
        let mut out = Vec::with_capacity(64 + self.target.len() + self.headers.encoded_len());
        out.extend_from_slice(self.method.as_str().as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.target.as_bytes());
        out.extend_from_slice(b" HTTP/1.1\r\n");
        self.headers.write_to(&mut out);
        match framing {
            BodyFraming::None => {}
            BodyFraming::Length(n) => out.extend_from_slice(format!("Content-Length: {n}\r\n").as_bytes()),
            BodyFraming::Chunked => out.extend_from_slice(b"Transfer-Encoding: chunked\r\n"),
            BodyFraming::CloseDelimited => return Err(HttpError::InvalidInput("requests cannot be close-delimited")),
        }
        out.extend_from_slice(b"\r\n");
        Ok(Bytes::from(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(input: &[u8]) -> Result<Vec<RequestEvent>, HttpError> {
        let mut p = RequestParser::new(Limits::DEFAULT);
        p.feed(input)?;
        p.feed_eof();
        let mut out = Vec::new();
        loop {
            let e = p.poll()?;
            let done = matches!(e, Event::Closed);
            out.push(e);
            if done {
                return Ok(out);
            }
            if out.len() > 10_000 {
                panic!("too many events");
            }
        }
    }

    fn head(input: &[u8]) -> Result<RequestHead, HttpError> {
        let mut p = RequestParser::new(Limits::DEFAULT);
        p.feed(input)?;
        match p.poll()? {
            Event::Head(h) => Ok(h),
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn simple_get() {
        let h = head(b"GET /api/v1/streams?limit=10 HTTP/1.1\r\nHost: a\r\nX-A:  b \r\n\r\n").unwrap();
        assert_eq!(h.method, Method::Get);
        assert_eq!(h.path(), "/api/v1/streams");
        assert_eq!(h.query(), Some("limit=10"));
        assert_eq!(h.headers.get("x-a").unwrap(), "b");
        assert!(h.keep_alive);
        assert_eq!(h.body, BodyFraming::None);
    }

    #[test]
    fn pipelined_with_bodies() {
        let ev = parse_all(
            b"POST /a HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhelloPOST /b HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3;ext=1\r\nabc\r\n2\r\nde\r\n0\r\nX-T: 1\r\n\r\nGET /c HTTP/1.0\r\n\r\n",
        )
        .unwrap();
        let mut bodies = Vec::new();
        let mut cur = Vec::new();
        let mut heads = 0;
        let mut trailers = Vec::new();
        for e in ev {
            match e {
                Event::Head(_) => heads += 1,
                Event::Data(d) => cur.extend_from_slice(&d),
                Event::End(t) => {
                    bodies.push(std::mem::take(&mut cur));
                    trailers.push(t);
                }
                _ => {}
            }
        }
        assert_eq!(heads, 3);
        assert_eq!(bodies, vec![b"hello".to_vec(), b"abcde".to_vec(), vec![]]);
        assert_eq!(trailers[1].get("x-t").unwrap(), "1");
    }

    #[test]
    fn byte_by_byte_equals_whole() {
        let input = b"PUT /x HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let mut p = RequestParser::new(Limits::DEFAULT);
        let mut body = Vec::new();
        let mut got_end = false;
        let mut expect = false;
        for &b in input.iter() {
            p.feed(&[b]).unwrap();
            loop {
                match p.poll().unwrap() {
                    Event::NeedMore => break,
                    Event::Head(h) => expect = h.expect_continue,
                    Event::Data(d) => body.extend_from_slice(&d),
                    Event::End(_) => got_end = true,
                    Event::Closed => unreachable!(),
                }
            }
        }
        assert!(expect);
        assert!(got_end);
        assert_eq!(body, b"hello");
    }

    #[test]
    fn keep_alive_rules() {
        assert!(!head(b"GET / HTTP/1.0\r\n\r\n").unwrap().keep_alive);
        assert!(head(b"GET / HTTP/1.0\r\nConnection: Keep-Alive\r\n\r\n").unwrap().keep_alive);
        assert!(!head(b"GET / HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n").unwrap().keep_alive);
        // HTTP/1.0 では 100-continue を無視する
        assert!(!head(b"POST / HTTP/1.0\r\nExpect: 100-continue\r\nContent-Length: 1\r\n\r\n").unwrap().expect_continue);
    }

    #[test]
    fn rejects() {
        let cases: &[(&[u8], u16)] = &[
            (b"GET / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\nContent-Length: 3\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost: a\r\nContent-Length: 3, 3\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost: a\r\nContent-Length: +3\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: gzip\r\n\r\n", 501),
            (b"GET / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked, chunked\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n", 0),
            (b"POST / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost: a\r\nX: a\r\n b\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost : a\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost: a\rb\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost: a\0\r\n\r\n", 400),
            (b"GET / HTTP/1.1\nHost: a\n\n", 400),
            (b"GET  / HTTP/1.1\r\nHost: a\r\n\r\n", 400),
            (b"GET / HTTP/2.0\r\nHost: a\r\n\r\n", 505),
            (b"GET / HTTP/1.1\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n", 400),
            (b"GET /a#frag HTTP/1.1\r\nHost: a\r\n\r\n", 400),
            (b"GET /a%zz HTTP/1.1\r\nHost: a\r\n\r\n", 400),
            (b"GET a HTTP/1.1\r\nHost: a\r\n\r\n", 400),
            (b"G(T / HTTP/1.1\r\nHost: a\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost: a\r\nExpect: foo\r\n\r\n", 417),
            (b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 2000000\r\n\r\n", 413),
        ];
        for (input, status) in cases {
            let r = parse_all(input);
            match (r, status) {
                (Ok(_), 0) => {}
                (Err(e), s) if *s != 0 => {
                    assert_eq!(e.status(), *s, "{:?} -> {e:?}", String::from_utf8_lossy(input))
                }
                (r, _) => panic!("{:?} -> {r:?}", String::from_utf8_lossy(input)),
            }
        }
    }

    #[test]
    fn bad_chunks() {
        for body in [
            &b"zz\r\n"[..],
            b"5\r\nhelloXX0\r\n\r\n",
            b"5\r\nhel",
            b"FFFFFFFFFFFFFFFF\r\n",
            b"1 x\r\na\r\n0\r\n\r\n",
            b"1\r\na\r\n0\r\nbad trailer\r\n\r\n",
        ] {
            let mut input = b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
            input.extend_from_slice(body);
            assert!(parse_all(&input).is_err(), "{:?}", String::from_utf8_lossy(body));
        }
    }

    #[test]
    fn chunked_body_limit() {
        let mut p = RequestParser::new(Limits::DEFAULT);
        p.feed(b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        assert!(matches!(p.poll().unwrap(), Event::Head(_)));
        p.set_body_limit(4);
        p.feed(b"3\r\nabc\r\n3\r\n").unwrap();
        assert!(matches!(p.poll().unwrap(), Event::Data(_)));
        assert_eq!(p.poll(), Err(HttpError::BodyTooLarge));
        assert_eq!(p.poll(), Err(HttpError::BodyTooLarge));
    }

    #[test]
    fn header_limits() {
        let mut big = b"GET / HTTP/1.1\r\nHost: a\r\n".to_vec();
        for i in 0..101 {
            big.extend_from_slice(format!("X-{i}: v\r\n").as_bytes());
        }
        big.extend_from_slice(b"\r\n");
        assert_eq!(parse_all(&big).unwrap_err().status(), 431);

        let mut long = b"GET / HTTP/1.1\r\nHost: a\r\nX: ".to_vec();
        long.extend(std::iter::repeat_n(b'a', 33 * 1024));
        long.extend_from_slice(b"\r\n\r\n");
        let mut p = RequestParser::new(Limits { max_buffer: 1 << 20, ..Limits::DEFAULT });
        p.feed(&long).unwrap();
        assert_eq!(p.poll().unwrap_err().status(), 431);

        let mut uri = b"GET /".to_vec();
        uri.extend(std::iter::repeat_n(b'a', 9000));
        uri.extend_from_slice(b" HTTP/1.1\r\nHost: a\r\n\r\n");
        assert_eq!(parse_all(&uri).unwrap_err().status(), 414);
    }

    #[test]
    fn eof_handling() {
        let mut p = RequestParser::new(Limits::DEFAULT);
        p.feed_eof();
        assert_eq!(p.poll().unwrap(), Event::Closed);
        let mut p = RequestParser::new(Limits::DEFAULT);
        p.feed(b"GET / HT").unwrap();
        p.feed_eof();
        assert_eq!(p.poll(), Err(HttpError::Incomplete));
    }

    #[test]
    fn target_forms() {
        assert!(head(b"OPTIONS * HTTP/1.1\r\nHost: a\r\n\r\n").is_ok());
        assert!(head(b"GET * HTTP/1.1\r\nHost: a\r\n\r\n").is_err());
        assert!(head(b"GET http://a.example/x HTTP/1.1\r\nHost: a\r\n\r\n").is_ok());
        assert!(head(b"CONNECT a.example:443 HTTP/1.1\r\nHost: a\r\n\r\n").is_ok());
        assert!(head(b"GET /\xff HTTP/1.1\r\nHost: a\r\n\r\n").is_err());
    }

    #[test]
    fn encode_request() {
        let r = Request::new(Method::Post, "/whip").header("Host", "x").unwrap().header("Content-Type", "application/sdp").unwrap();
        let b = r.encode_head(BodyFraming::Length(3)).unwrap();
        assert_eq!(&b[..], b"POST /whip HTTP/1.1\r\nHost: x\r\nContent-Type: application/sdp\r\nContent-Length: 3\r\n\r\n");
        let h = head(&b).unwrap();
        assert_eq!(h.body, BodyFraming::Length(3));
        assert!(Request::new(Method::Get, "/a b").header("Host", "x").unwrap().encode_head(BodyFraming::None).is_err());
    }
}
