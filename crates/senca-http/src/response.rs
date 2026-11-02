//! 応答の生成（サーバー側）と解析（クライアント側）。
//!
//! 生成は「head の `Bytes`」と「body の `Bytes`」を別々に返し、呼び出し側が連結せずに
//! `write_vectored` できるようにする。

use crate::error::HttpError;
use crate::headers::{HeaderMap, is_field_vchar};
use crate::parse::{
    BodyDecoder, BodyFraming, BodyPoll, Event, HeadReader, Limits, Version, content_length, transfer_encoding,
};
use crate::request::Method;
use bytes::{Bytes, BytesMut};
use std::collections::VecDeque;

/// 状態コードの既定の reason-phrase。
pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        422 => "Unprocessable Content",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

/// `Expect: 100-continue` に返す中間応答。
pub const CONTINUE_100: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/// 応答の body の枠組み（生成側）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// body なし・長さの header も付けない（1xx・204・304）。
    None,
    /// Content-Length を付ける（HEAD への応答でも同じ値を付け、body は送らない）。
    Length(u64),
    /// Transfer-Encoding: chunked（HTTP/1.1 の要求にだけ）。
    Chunked,
    /// 送り終えたら閉じる（Connection: close を付ける）。
    Close,
}

/// 送る応答の先頭部分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: HeaderMap,
}

/// head と body を分けたままの完成品（`write_vectored` 用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    pub head: Bytes,
    pub body: Bytes,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Response { status, headers: HeaderMap::new() }
    }

    /// header を足す（検査付き）。
    pub fn header(mut self, name: &str, value: impl Into<Bytes>) -> Result<Self, HttpError> {
        self.headers.append(name, value)?;
        Ok(self)
    }

    /// status line と header を書き出す。
    ///
    /// - Content-Length・Transfer-Encoding・Connection は `framing`・`keep_alive` から付ける（自分で入れたら `Err`）。
    /// - `request_version` が HTTP/1.0 なら chunked は使えず、keep-alive のときは `Connection: keep-alive` を付ける。
    pub fn encode_head(&self, framing: Framing, request_version: Version, keep_alive: bool) -> Result<Bytes, HttpError> {
        if !(100..=599).contains(&self.status) {
            return Err(HttpError::InvalidInput("status code"));
        }
        for n in ["content-length", "transfer-encoding", "connection"] {
            if self.headers.contains(n) {
                return Err(HttpError::InvalidInput("framing headers are generated"));
            }
        }
        let bodyless = self.status < 200 || self.status == 204;
        match framing {
            Framing::None => {}
            Framing::Length(_) if self.status == 304 => {}
            _ if bodyless || self.status == 304 => return Err(HttpError::InvalidInput("status forbids a body")),
            Framing::Chunked if request_version == Version::Http10 => {
                return Err(HttpError::InvalidInput("chunked to HTTP/1.0"));
            }
            _ => {}
        }
        let mut out = Vec::with_capacity(128 + self.headers.encoded_len());
        out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", self.status, reason_phrase(self.status)).as_bytes());
        self.headers.write_to(&mut out);
        match framing {
            Framing::None | Framing::Close => {}
            Framing::Length(n) => out.extend_from_slice(format!("Content-Length: {n}\r\n").as_bytes()),
            Framing::Chunked => out.extend_from_slice(b"Transfer-Encoding: chunked\r\n"),
        }
        if self.status >= 200 {
            if !keep_alive || framing == Framing::Close {
                out.extend_from_slice(b"Connection: close\r\n");
            } else if request_version == Version::Http10 {
                out.extend_from_slice(b"Connection: keep-alive\r\n");
            }
        }
        out.extend_from_slice(b"\r\n");
        Ok(Bytes::from(out))
    }

    /// body 全体が手元にある応答。`head_only`（HEAD 要求）なら body は空で、Content-Length は本来の長さ。
    pub fn encode(&self, body: Bytes, request_version: Version, keep_alive: bool, head_only: bool) -> Result<Encoded, HttpError> {
        let bodyless = self.status < 200 || self.status == 204 || self.status == 304;
        let framing = if bodyless {
            if !body.is_empty() {
                return Err(HttpError::InvalidInput("status forbids a body"));
            }
            Framing::None
        } else {
            Framing::Length(body.len() as u64)
        };
        let head = self.encode_head(framing, request_version, keep_alive)?;
        Ok(Encoded { head, body: if head_only { Bytes::new() } else { body } })
    }
}

/// chunked の符号化。
pub mod chunked {
    use super::*;

    /// 1 つの chunk を（size 行, data, CRLF）の 3 つに分けて返す。空の data は終端と紛れるので `None`。
    pub fn encode_chunk(data: Bytes) -> Option<[Bytes; 3]> {
        if data.is_empty() {
            return None;
        }
        Some([Bytes::from(format!("{:x}\r\n", data.len())), data, Bytes::from_static(b"\r\n")])
    }

    /// 終端の chunk（と trailer）。
    pub fn last_chunk(trailers: Option<&HeaderMap>) -> Bytes {
        match trailers {
            None => Bytes::from_static(b"0\r\n\r\n"),
            Some(t) => {
                let mut out = b"0\r\n".to_vec();
                t.write_to(&mut out);
                out.extend_from_slice(b"\r\n");
                Bytes::from(out)
            }
        }
    }
}

/// 解析済みの応答の先頭部分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub version: Version,
    pub status: u16,
    pub reason: String,
    pub headers: HeaderMap,
    pub body: BodyFraming,
    /// この応答の後も接続を使えるか。
    pub keep_alive: bool,
}

pub type ResponseEvent = Event<ResponseHead>;

fn parse_status_line(line: &[u8]) -> Result<(Version, u16, String), HttpError> {
    let (v, rest) = line.split_at(line.iter().position(|&c| c == b' ').ok_or(HttpError::Malformed("status line"))?);
    let version = Version::parse(v)?;
    let rest = &rest[1..];
    if rest.len() < 3 || !rest[..3].iter().all(u8::is_ascii_digit) {
        return Err(HttpError::Malformed("status code"));
    }
    let status = rest[..3].iter().fold(0u16, |a, &c| a * 10 + (c - b'0') as u16);
    if status < 100 {
        return Err(HttpError::Malformed("status code"));
    }
    let reason = match &rest[3..] {
        [] => &[][..],
        [b' ', r @ ..] => r,
        _ => return Err(HttpError::Malformed("status line")),
    };
    if !reason.iter().all(|&c| is_field_vchar(c)) {
        return Err(HttpError::Malformed("reason phrase"));
    }
    Ok((version, status, String::from_utf8_lossy(reason).into_owned()))
}

#[derive(Debug)]
enum State {
    Head,
    BodyStart(BodyFraming),
    Body(BodyDecoder),
    Upgraded,
    Failed(HttpError),
}

/// 応答の逐次解析器（クライアント側）。送った要求のメソッドを `push_request` で順に登録する
/// （HEAD・CONNECT の応答は body の扱いが違うため。登録がなければ GET とみなす）。
#[derive(Debug)]
pub struct ResponseParser {
    buf: BytesMut,
    limits: Limits,
    head: HeadReader,
    state: State,
    pending: VecDeque<Method>,
    eof: bool,
}

impl ResponseParser {
    pub fn new(limits: Limits) -> Self {
        ResponseParser {
            buf: BytesMut::new(),
            limits,
            head: HeadReader::default(),
            state: State::Head,
            pending: VecDeque::new(),
            eof: false,
        }
    }

    /// 送った要求のメソッド（pipelining の順に）。
    pub fn push_request(&mut self, method: Method) {
        self.pending.push_back(method);
    }

    pub fn feed(&mut self, data: &[u8]) -> Result<(), HttpError> {
        if self.buf.len().saturating_add(data.len()) > self.limits.max_buffer {
            return Err(HttpError::Malformed("receive buffer overflow"));
        }
        self.buf.extend_from_slice(data);
        Ok(())
    }

    pub fn feed_eof(&mut self) {
        self.eof = true;
    }

    /// 101・CONNECT 成功の後に残ったバイト（切り替え先のプロトコルのもの）。
    pub fn take_remaining(&mut self) -> BytesMut {
        self.buf.split()
    }

    pub fn poll(&mut self) -> Result<ResponseEvent, HttpError> {
        let r = self.poll_inner();
        if let Err(e) = &r {
            self.state = State::Failed(e.clone());
        }
        r
    }

    fn poll_inner(&mut self) -> Result<ResponseEvent, HttpError> {
        loop {
            match &mut self.state {
                State::Failed(e) => return Err(e.clone()),
                State::Upgraded => return Ok(Event::Closed),
                State::Head => {
                    if self.eof && self.head.is_idle(&self.buf) {
                        return Ok(Event::Closed);
                    }
                    let Some((line, headers)) =
                        self.head.poll(&mut self.buf, &self.limits, HttpError::Malformed("status line too long"))?
                    else {
                        return if self.eof { Err(HttpError::Incomplete) } else { Ok(Event::NeedMore) };
                    };
                    let (version, status, reason) = parse_status_line(&line)?;
                    let method = self.pending.front().cloned().unwrap_or(Method::Get);
                    let cl = content_length(&headers)?;
                    let te = transfer_encoding(&headers, false)?;
                    if cl.is_some() && te.is_some() {
                        return Err(HttpError::Malformed("both content-length and transfer-encoding"));
                    }
                    let interim = (100..200).contains(&status);
                    let tunnel = (status == 101) || (method == Method::Connect && (200..300).contains(&status));
                    let body = if interim || tunnel || status == 204 || status == 304 || method == Method::Head {
                        BodyFraming::None
                    } else {
                        match (te, cl) {
                            (Some(true), _) => BodyFraming::Chunked,
                            (Some(false), _) => BodyFraming::CloseDelimited,
                            (None, Some(n)) => BodyFraming::Length(n),
                            (None, None) => BodyFraming::CloseDelimited,
                        }
                    };
                    let close = headers.has_token("connection", "close");
                    let keep_alive = body != BodyFraming::CloseDelimited
                        && !tunnel
                        && match version {
                            Version::Http11 => !close,
                            Version::Http10 => !close && headers.has_token("connection", "keep-alive"),
                        };
                    if tunnel {
                        self.pending.pop_front();
                        self.state = State::Upgraded;
                    } else if interim {
                        // 中間応答: 同じ要求の最終応答がまだ来る。body はない。
                        self.state = State::BodyStart(BodyFraming::None);
                    } else {
                        self.pending.pop_front();
                        self.state = State::BodyStart(body);
                    }
                    return Ok(Event::Head(ResponseHead { version, status, reason, headers, body, keep_alive }));
                }
                State::BodyStart(framing) => {
                    let d = BodyDecoder::new(*framing, self.limits.max_body)?;
                    self.state = State::Body(d);
                }
                State::Body(d) => match d.poll(&mut self.buf, self.eof, &self.limits)? {
                    BodyPoll::NeedMore => return Ok(Event::NeedMore),
                    BodyPoll::Data(b) => return Ok(Event::Data(b)),
                    BodyPoll::End(t) => {
                        self.state = State::Head;
                        return Ok(Event::End(t));
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(p: &mut ResponseParser) -> Result<Vec<ResponseEvent>, HttpError> {
        let mut out = Vec::new();
        loop {
            let e = p.poll()?;
            if matches!(e, Event::NeedMore | Event::Closed) {
                out.push(e);
                return Ok(out);
            }
            out.push(e);
        }
    }

    #[test]
    fn encode_basic() {
        let r = Response::new(200).header("Content-Type", "application/json").unwrap();
        let e = r.encode(Bytes::from_static(b"{}"), Version::Http11, true, false).unwrap();
        assert_eq!(&e.head[..], b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n");
        assert_eq!(&e.body[..], b"{}");
        let e = r.encode(Bytes::from_static(b"{}"), Version::Http10, true, true).unwrap();
        assert!(e.head.ends_with(b"Content-Length: 2\r\nConnection: keep-alive\r\n\r\n"));
        assert!(e.body.is_empty());
        let e = Response::new(204).encode(Bytes::new(), Version::Http11, false, false).unwrap();
        assert_eq!(&e.head[..], b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
    }

    #[test]
    fn encode_rejects() {
        assert!(Response::new(204).encode_head(Framing::Length(1), Version::Http11, true).is_err());
        assert!(Response::new(200).encode_head(Framing::Chunked, Version::Http10, true).is_err());
        let r = Response::new(200).header("Content-Length", "1").unwrap();
        assert!(r.encode_head(Framing::Length(1), Version::Http11, true).is_err());
        assert!(Response::new(200).header("X", "a\r\nb").is_err());
        assert!(Response::new(42).encode_head(Framing::None, Version::Http11, true).is_err());
    }

    #[test]
    fn chunked_roundtrip() {
        let r = Response::new(200);
        let mut wire = r.encode_head(Framing::Chunked, Version::Http11, true).unwrap().to_vec();
        for part in [&b"hello "[..], b"", b"world"] {
            if let Some(c) = chunked::encode_chunk(Bytes::copy_from_slice(part)) {
                for b in c {
                    wire.extend_from_slice(&b);
                }
            }
        }
        let mut t = HeaderMap::new();
        t.append("X-Checksum", "1").unwrap();
        wire.extend_from_slice(&chunked::last_chunk(Some(&t)));
        let mut p = ResponseParser::new(Limits::DEFAULT);
        p.feed(&wire).unwrap();
        let ev = collect(&mut p).unwrap();
        let body: Vec<u8> = ev.iter().filter_map(|e| if let Event::Data(d) = e { Some(d.to_vec()) } else { None }).flatten().collect();
        assert_eq!(body, b"hello world");
        assert!(ev.iter().any(|e| matches!(e, Event::End(t) if t.get("x-checksum").is_some())));
    }

    #[test]
    fn parse_close_delimited_and_head() {
        let mut p = ResponseParser::new(Limits::DEFAULT);
        p.feed(b"HTTP/1.1 200 OK\r\n\r\npartial body").unwrap();
        let ev = collect(&mut p).unwrap();
        assert!(matches!(&ev[0], Event::Head(h) if h.body == BodyFraming::CloseDelimited && !h.keep_alive));
        p.feed(b" more").unwrap();
        p.feed_eof();
        let ev = collect(&mut p).unwrap();
        assert!(matches!(&ev[0], Event::Data(d) if &d[..] == b" more"));
        assert!(matches!(&ev[1], Event::End(_)));

        let mut p = ResponseParser::new(Limits::DEFAULT);
        p.push_request(Method::Head);
        p.push_request(Method::Get);
        p.feed(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nHTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\nno").unwrap();
        let ev = collect(&mut p).unwrap();
        let statuses: Vec<u16> = ev.iter().filter_map(|e| if let Event::Head(h) = e { Some(h.status) } else { None }).collect();
        assert_eq!(statuses, vec![200, 100, 404]);
        assert!(matches!(ev.last(), Some(Event::NeedMore)));
    }

    #[test]
    fn parse_rejects() {
        for input in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
            b"HTTP/1.1 20 OK\r\n\r\n",
            b"HTTP/1.1 200OK\r\n\r\n",
            b"HTTX/1.1 200 OK\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
        ] {
            let mut p = ResponseParser::new(Limits::DEFAULT);
            p.feed(input).unwrap();
            assert!(collect(&mut p).is_err(), "{:?}", String::from_utf8_lossy(input));
        }
        let mut p = ResponseParser::new(Limits::DEFAULT);
        p.feed(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nab").unwrap();
        p.feed_eof();
        assert!(collect(&mut p).is_err());
    }

    #[test]
    fn empty_reason_and_upgrade() {
        let mut p = ResponseParser::new(Limits::DEFAULT);
        p.push_request(Method::Get);
        p.feed(b"HTTP/1.1 101 \r\nUpgrade: x\r\n\r\nrawbytes").unwrap();
        let ev = collect(&mut p).unwrap();
        assert!(matches!(&ev[0], Event::Head(h) if h.status == 101 && h.reason.is_empty()));
        assert!(matches!(ev.last(), Some(Event::Closed)));
        assert_eq!(&p.take_remaining()[..], b"rawbytes");
    }
}
