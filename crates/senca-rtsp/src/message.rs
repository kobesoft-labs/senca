//! RTSP 1.0 のメッセージ（RFC 2326 6・7）と、RTSP メッセージと interleaved バイナリ（10.12）が
//! 混ざったストリームの逐次復号。

use crate::error::{Error, Result};
use bytes::{Buf, Bytes, BytesMut};
use std::fmt;

/// RTSP のメソッド。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Method {
    Options,
    Describe,
    Announce,
    Setup,
    Play,
    Pause,
    Teardown,
    GetParameter,
    SetParameter,
    Redirect,
    Record,
    /// 上記以外（token として正しいもの）。
    Other(String),
}

impl Method {
    pub fn as_str(&self) -> &str {
        match self {
            Method::Options => "OPTIONS",
            Method::Describe => "DESCRIBE",
            Method::Announce => "ANNOUNCE",
            Method::Setup => "SETUP",
            Method::Play => "PLAY",
            Method::Pause => "PAUSE",
            Method::Teardown => "TEARDOWN",
            Method::GetParameter => "GET_PARAMETER",
            Method::SetParameter => "SET_PARAMETER",
            Method::Redirect => "REDIRECT",
            Method::Record => "RECORD",
            Method::Other(s) => s,
        }
    }

    /// メソッド名を解釈する（大文字小文字を区別する。RFC 2326 6.1）。
    pub fn parse(s: &str) -> Result<Method> {
        Ok(match s {
            "OPTIONS" => Method::Options,
            "DESCRIBE" => Method::Describe,
            "ANNOUNCE" => Method::Announce,
            "SETUP" => Method::Setup,
            "PLAY" => Method::Play,
            "PAUSE" => Method::Pause,
            "TEARDOWN" => Method::Teardown,
            "GET_PARAMETER" => Method::GetParameter,
            "SET_PARAMETER" => Method::SetParameter,
            "REDIRECT" => Method::Redirect,
            "RECORD" => Method::Record,
            _ => {
                if s.is_empty() || s.len() > 32 || !s.bytes().all(is_token_char) {
                    return Err(Error::Malformed("メソッド名"));
                }
                Method::Other(s.to_string())
            }
        })
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub(crate) fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// 1 つしか現れてはいけないヘッダー（重複は拒否する）。
const SINGLE_VALUED: &[&str] = &[
    "cseq",
    "content-length",
    "session",
    "content-type",
    "content-base",
    "content-location",
    "transport",
    "range",
    "location",
];

/// ヘッダーの並び（名前は大文字小文字を区別しない。順序を保つ）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    entries: Vec<(String, String)>,
}

impl Headers {
    pub fn new() -> Self {
        Headers::default()
    }

    /// 最初に見つかった値。
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 同じ名前の値をすべて。
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.entries
            .iter()
            .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 同じ名前を消してから入れる。
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        self.remove(name);
        self.entries.push((name.to_string(), value.into()));
    }

    /// 末尾に足す（同じ名前があっても残す）。
    pub fn append(&mut self, name: &str, value: impl Into<String>) {
        self.entries.push((name.to_string(), value.into()));
    }

    pub fn remove(&mut self, name: &str) {
        self.entries.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// CSeq の値。
    pub fn cseq(&self) -> Option<u32> {
        self.get("CSeq").and_then(|v| parse_u32_strict(v.trim()))
    }

    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        for (n, v) in &self.entries {
            if n.is_empty() || !n.bytes().all(is_token_char) {
                return Err(Error::Malformed("ヘッダー名"));
            }
            if v.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
                return Err(Error::Malformed("ヘッダー値に改行"));
            }
            out.extend_from_slice(n.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(v.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        Ok(())
    }
}

/// 十進数（符号・空白なし）。
pub(crate) fn parse_u32_strict(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > 10 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// 要求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    pub uri: String,
    pub headers: Headers,
    pub body: Bytes,
}

impl Request {
    pub fn new(method: Method, uri: impl Into<String>) -> Self {
        Request {
            method,
            uri: uri.into(),
            headers: Headers::new(),
            body: Bytes::new(),
        }
    }

    pub fn cseq(&self) -> Option<u32> {
        self.headers.cseq()
    }

    /// バイト列にする。本文があれば Content-Length を付け直す。
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.uri.is_empty() || self.uri.bytes().any(|b| b <= b' ' || b == 0x7f) {
            return Err(Error::Malformed("Request-URI"));
        }
        out.extend_from_slice(self.method.as_str().as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.uri.as_bytes());
        out.extend_from_slice(b" RTSP/1.0\r\n");
        encode_rest(&self.headers, &self.body, out)
    }
}

/// 応答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Headers,
    pub body: Bytes,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Response {
            status,
            reason: reason_phrase(status).to_string(),
            headers: Headers::new(),
            body: Bytes::new(),
        }
    }

    pub fn cseq(&self) -> Option<u32> {
        self.headers.cseq()
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        if !(100..=999).contains(&self.status) {
            return Err(Error::Malformed("status code"));
        }
        if self.reason.bytes().any(|b| b == b'\r' || b == b'\n') {
            return Err(Error::Malformed("reason phrase"));
        }
        out.extend_from_slice(format!("RTSP/1.0 {} {}\r\n", self.status, self.reason).as_bytes());
        encode_rest(&self.headers, &self.body, out)
    }
}

fn encode_rest(headers: &Headers, body: &Bytes, out: &mut Vec<u8>) -> Result<()> {
    let mut h = headers.clone();
    h.remove("Content-Length");
    if !body.is_empty() {
        h.append("Content-Length", body.len().to_string());
    }
    h.encode(out)?;
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    Ok(())
}

/// RFC 2326 7.1.1 の理由句。
pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        250 => "Low on Storage Space",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Moved Temporarily",
        303 => "See Other",
        305 => "Use Proxy",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Time-out",
        413 => "Request Entity Too Large",
        414 => "Request-URI Too Large",
        415 => "Unsupported Media Type",
        451 => "Parameter Not Understood",
        454 => "Session Not Found",
        455 => "Method Not Valid in This State",
        456 => "Header Field Not Valid for Resource",
        457 => "Invalid Range",
        459 => "Aggregate operation not allowed",
        460 => "Only aggregate operation allowed",
        461 => "Unsupported transport",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        505 => "RTSP Version not supported",
        551 => "Option not supported",
        _ => "Unknown",
    }
}

/// 復号した 1 単位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Request(Request),
    Response(Response),
    /// `$` + channel + 長さ（16 bit）で囲まれた RTP/RTCP（RFC 2326 10.12）。
    Interleaved {
        channel: u8,
        data: Bytes,
    },
}

/// 復号の上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// 開始行とヘッダーの合計（空行まで）。
    pub max_header_bytes: usize,
    /// ヘッダーの数。
    pub max_headers: usize,
    /// 本文（Content-Length）。SDP は 256 KiB（仕様 9.4）。
    pub max_body: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_header_bytes: 16 * 1024,
            max_headers: 64,
            max_body: 256 * 1024,
        }
    }
}

impl Limits {
    /// 復号器が溜める量の上限（最大のメッセージ 1 つ + interleaved 1 つ + 余裕）。
    pub fn max_buffer(&self) -> usize {
        self.max_header_bytes
            .saturating_add(self.max_body)
            .saturating_add(4 + 65535)
            .saturating_add(FEED_CHUNK)
    }
}

/// [`Decoder::feed`] に一度に渡せる量の目安。セッションはこの大きさに分けて渡す。
pub const FEED_CHUNK: usize = 16 * 1024;

/// 逐次復号器。`feed` で足し、`decode` で 1 つずつ取り出す。
#[derive(Debug)]
pub struct Decoder {
    buf: BytesMut,
    limits: Limits,
    /// ヘッダー終端を探し終えた位置（同じ所を何度も探さない）。
    scanned: usize,
    /// ヘッダーを解析済みで本文待ちのもの。
    pending: Option<(Head, usize)>,
}

#[derive(Debug)]
enum Head {
    Request(Method, String, Headers),
    Response(u16, String, Headers),
}

impl Decoder {
    pub fn new(limits: Limits) -> Self {
        Decoder {
            buf: BytesMut::new(),
            limits,
            scanned: 0,
            pending: None,
        }
    }

    /// 溜まっている量。
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// バイト列を足す。上限を超えると `Err`（接続を閉じること）。
    pub fn feed(&mut self, data: &[u8]) -> Result<()> {
        if self.buf.len().saturating_add(data.len()) > self.limits.max_buffer() {
            return Err(Error::LimitExceeded("受信バッファ"));
        }
        self.buf.extend_from_slice(data);
        Ok(())
    }

    /// 次の 1 単位。足りなければ `Ok(None)`。
    pub fn decode(&mut self) -> Result<Option<Frame>> {
        if self.pending.is_none() {
            // メッセージ間の空行は読み飛ばす。
            while self.scanned == 0 && matches!(self.buf.first(), Some(b'\r' | b'\n')) {
                self.buf.advance(1);
            }
            let Some(&first) = self.buf.first() else {
                return Ok(None);
            };
            if first == b'$' {
                if self.buf.len() < 4 {
                    return Ok(None);
                }
                let channel = self.buf[1];
                let len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                if self.buf.len() < 4 + len {
                    return Ok(None);
                }
                self.buf.advance(4);
                let data = self.buf.split_to(len).freeze();
                return Ok(Some(Frame::Interleaved { channel, data }));
            }
            let Some(end) = self.find_header_end()? else {
                return Ok(None);
            };
            let head_bytes = self.buf.split_to(end);
            self.scanned = 0;
            let (head, body_len) = parse_head(&head_bytes, &self.limits)?;
            self.pending = Some((head, body_len));
        }
        let body_len = self.pending.as_ref().map_or(0, |p| p.1);
        if self.buf.len() < body_len {
            return Ok(None);
        }
        let body = self.buf.split_to(body_len).freeze();
        let Some((head, _)) = self.pending.take() else {
            return Ok(None);
        };
        Ok(Some(match head {
            Head::Request(method, uri, headers) => Frame::Request(Request {
                method,
                uri,
                headers,
                body,
            }),
            Head::Response(status, reason, headers) => Frame::Response(Response {
                status,
                reason,
                headers,
                body,
            }),
        }))
    }

    /// 空行（CRLF CRLF または LF LF）の直後の位置。
    fn find_header_end(&mut self) -> Result<Option<usize>> {
        let buf = &self.buf[..];
        let start = self.scanned.saturating_sub(3);
        let mut i = start;
        while i < buf.len() {
            if buf[i] == b'\n' {
                if i + 1 < buf.len() && buf[i + 1] == b'\n' {
                    return self.check_end(i + 2);
                }
                if i + 2 < buf.len() && buf[i + 1] == b'\r' && buf[i + 2] == b'\n' {
                    return self.check_end(i + 3);
                }
            }
            i += 1;
        }
        self.scanned = buf.len();
        if buf.len() > self.limits.max_header_bytes {
            return Err(Error::LimitExceeded("ヘッダー部"));
        }
        Ok(None)
    }

    fn check_end(&self, end: usize) -> Result<Option<usize>> {
        if end > self.limits.max_header_bytes {
            return Err(Error::LimitExceeded("ヘッダー部"));
        }
        Ok(Some(end))
    }
}

fn parse_head(raw: &[u8], limits: &Limits) -> Result<(Head, usize)> {
    let text = std::str::from_utf8(raw).map_err(|_| Error::Malformed("UTF-8 ではないヘッダー"))?;
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let start = lines.next().ok_or(Error::Malformed("開始行がない"))?;
    if start.bytes().any(|b| b < b' ' && b != b'\t') {
        return Err(Error::Malformed("開始行に制御文字"));
    }
    let mut headers = Headers::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.bytes().any(|b| (b < b' ' && b != b'\t') || b == 0x7f) {
            return Err(Error::Malformed("ヘッダーに制御文字"));
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            // 折り返し（RFC 2326 4.2・RFC 822）: 直前の値に続ける。
            let last = headers
                .entries
                .last_mut()
                .ok_or(Error::Malformed("先頭の行が折り返し"))?;
            last.1.push(' ');
            last.1.push_str(line.trim());
            continue;
        }
        let (name, value) = line.split_once(':').ok_or(Error::Malformed("ヘッダー行"))?;
        if name.is_empty() || !name.bytes().all(is_token_char) {
            return Err(Error::Malformed("ヘッダー名"));
        }
        let lower = name.to_ascii_lowercase();
        if SINGLE_VALUED.contains(&lower.as_str()) && headers.get(name).is_some() {
            return Err(Error::Malformed("重複したヘッダー"));
        }
        if headers.len() >= limits.max_headers {
            return Err(Error::LimitExceeded("ヘッダー数"));
        }
        headers.append(name, value.trim());
    }
    let body_len = match headers.get("Content-Length") {
        None => 0,
        Some(v) => {
            let n = parse_u32_strict(v.trim()).ok_or(Error::Malformed("Content-Length"))? as usize;
            if n > limits.max_body {
                return Err(Error::LimitExceeded("本文"));
            }
            n
        }
    };
    if let Some(cseq) = headers.get("CSeq")
        && parse_u32_strict(cseq.trim()).is_none()
    {
        return Err(Error::Malformed("CSeq"));
    }
    if let Some(rest) = start.strip_prefix("RTSP/") {
        let mut it = rest.splitn(3, ' ');
        let ver = it.next().unwrap_or("");
        if ver != "1.0" {
            return Err(Error::Unsupported("RTSP の版"));
        }
        let code = it.next().unwrap_or("");
        if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::Malformed("status code"));
        }
        let status: u16 = code.parse().map_err(|_| Error::Malformed("status code"))?;
        if status < 100 {
            return Err(Error::Malformed("status code"));
        }
        let reason = it.next().unwrap_or("").to_string();
        Ok((Head::Response(status, reason, headers), body_len))
    } else {
        let mut it = start.split(' ');
        let (Some(m), Some(uri), Some(ver), None) = (it.next(), it.next(), it.next(), it.next())
        else {
            return Err(Error::Malformed("要求行"));
        };
        if ver != "RTSP/1.0" {
            return Err(Error::Unsupported("RTSP の版"));
        }
        if uri.is_empty() {
            return Err(Error::Malformed("Request-URI"));
        }
        Ok((
            Head::Request(Method::parse(m)?, uri.to_string(), headers),
            body_len,
        ))
    }
}

/// interleaved のフレームを書く（RFC 2326 10.12）。65535 バイトを超えれば `Err`。
pub fn write_interleaved(channel: u8, data: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let len = u16::try_from(data.len()).map_err(|_| Error::LimitExceeded("interleaved の長さ"))?;
    out.push(b'$');
    out.push(channel);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(data);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(d: &mut Decoder) -> Result<Vec<Frame>> {
        let mut v = Vec::new();
        while let Some(f) = d.decode()? {
            v.push(f);
        }
        Ok(v)
    }

    #[test]
    fn rfc2326_describe_response() {
        // RFC 2326 14.2 の例（本文を短くしたもの）。
        let sdp = "v=0\r\no=- 2890844526 2890842807 IN IP4 192.16.24.202\r\ns=RTSP Session\r\n";
        let msg = format!(
            "RTSP/1.0 200 OK\r\nCSeq: 312\r\nDate: 23 Jan 1997 15:35:06 GMT\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
            sdp.len()
        );
        let mut d = Decoder::new(Limits::default());
        d.feed(msg.as_bytes()).unwrap();
        let f = decode_all(&mut d).unwrap();
        assert_eq!(f.len(), 1);
        let Frame::Response(r) = &f[0] else { panic!() };
        assert_eq!(r.status, 200);
        assert_eq!(r.cseq(), Some(312));
        assert_eq!(r.headers.get("content-type"), Some("application/sdp"));
        assert_eq!(&r.body[..], sdp.as_bytes());
    }

    #[test]
    fn request_roundtrip() {
        let mut r = Request::new(Method::Setup, "rtsp://example.com/foo/bar/baz.rm");
        r.headers.set("CSeq", "302");
        r.headers
            .set("Transport", "RTP/AVP;unicast;client_port=4588-4589");
        let mut out = Vec::new();
        r.encode(&mut out).unwrap();
        let mut d = Decoder::new(Limits::default());
        d.feed(&out).unwrap();
        assert_eq!(d.decode().unwrap(), Some(Frame::Request(r)));
        assert_eq!(d.buffered(), 0);
    }

    #[test]
    fn mixed_interleaved_split_everywhere() {
        let mut stream = Vec::new();
        write_interleaved(0, &[0x80, 1, 2, 3], &mut stream).unwrap();
        let mut resp = Response::new(200);
        resp.headers.set("CSeq", "5");
        resp.body = Bytes::from_static(b"abc");
        resp.encode(&mut stream).unwrap();
        write_interleaved(1, &[], &mut stream).unwrap();
        write_interleaved(3, &[9u8; 300], &mut stream).unwrap();
        stream.extend_from_slice(b"\r\nRTSP/1.0 401 Unauthorized\nCSeq: 6\n\n");
        let mut whole = Decoder::new(Limits::default());
        whole.feed(&stream).unwrap();
        let expect = decode_all(&mut whole).unwrap();
        assert_eq!(expect.len(), 5);
        for split in 0..=stream.len() {
            let mut d = Decoder::new(Limits::default());
            let mut got = Vec::new();
            d.feed(&stream[..split]).unwrap();
            got.extend(decode_all(&mut d).unwrap());
            d.feed(&stream[split..]).unwrap();
            got.extend(decode_all(&mut d).unwrap());
            assert_eq!(got, expect, "split {split}");
        }
        // 1 バイトずつ
        let mut d = Decoder::new(Limits::default());
        let mut got = Vec::new();
        for b in &stream {
            d.feed(&[*b]).unwrap();
            got.extend(decode_all(&mut d).unwrap());
        }
        assert_eq!(got, expect);
    }

    #[test]
    fn rejects_bad_input() {
        let cases: &[&[u8]] = &[
            b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nCSeq: 2\r\n\r\n",
            b"RTSP/1.0 200 OK\r\nContent-Length: -1\r\n\r\n",
            b"RTSP/1.0 200 OK\r\nContent-Length: 99999999\r\n\r\n",
            b"RTSP/2.0 200 OK\r\n\r\n",
            b"RTSP/1.0 2x0 OK\r\n\r\n",
            b"OPTIONS * HTTP/1.1\r\n\r\n",
            b"OPTIONS  RTSP/1.0\r\n\r\n",
            b"OPTIONS * RTSP/1.0\r\nBad Header\r\n\r\n",
            b"OPTIONS * RTSP/1.0\r\n continued\r\n\r\n",
            b"OPTIONS * RTSP/1.0\r\nCSeq: abc\r\n\r\n",
            b"OPTIONS * RTSP/1.0\r\nX: \x01\r\n\r\n",
            b"OPT(ONS * RTSP/1.0\r\n\r\n",
        ];
        for c in cases {
            let mut d = Decoder::new(Limits::default());
            d.feed(c).unwrap();
            assert!(d.decode().is_err(), "{:?}", String::from_utf8_lossy(c));
        }
    }

    #[test]
    fn header_limits() {
        let mut d = Decoder::new(Limits::default());
        let mut big = b"OPTIONS * RTSP/1.0\r\n".to_vec();
        while big.len() < 17 * 1024 {
            big.extend_from_slice(b"X-Pad: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
        }
        d.feed(&big).unwrap();
        assert!(d.decode().is_err());
        let mut d = Decoder::new(Limits::default());
        let mut many = b"OPTIONS * RTSP/1.0\r\n".to_vec();
        for i in 0..65 {
            many.extend_from_slice(format!("X-{i}: v\r\n").as_bytes());
        }
        many.extend_from_slice(b"\r\n");
        d.feed(&many).unwrap();
        assert_eq!(d.decode(), Err(Error::LimitExceeded("ヘッダー数")));
        // バッファの上限
        let mut d = Decoder::new(Limits::default());
        assert!(
            d.feed(&vec![b'$'; Limits::default().max_buffer() + 1])
                .is_err()
        );
    }

    #[test]
    fn folding_and_case() {
        let mut d = Decoder::new(Limits::default());
        d.feed(b"RTSP/1.0 200 OK\r\ncseq: 3\r\nPublic: OPTIONS,\r\n DESCRIBE\r\n\r\n")
            .unwrap();
        let Some(Frame::Response(r)) = d.decode().unwrap() else {
            panic!()
        };
        assert_eq!(r.cseq(), Some(3));
        assert_eq!(r.headers.get("PUBLIC"), Some("OPTIONS, DESCRIBE"));
    }

    #[test]
    fn header_injection_rejected() {
        let mut r = Request::new(Method::Options, "*");
        r.headers.set("X", "a\r\nInjected: 1");
        assert!(r.encode(&mut Vec::new()).is_err());
        let r = Request::new(Method::Options, "rtsp://a b");
        assert!(r.encode(&mut Vec::new()).is_err());
    }

    #[test]
    fn fuzz_never_panics() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let base: &[u8] = b"RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: 4\r\nSession: 12;timeout=60\r\n\r\nabcd$\x00\x00\x04\x80\x60\x00\x01DESCRIBE rtsp://h/x RTSP/1.0\r\nCSeq: 3\r\n\r\n";
        for _ in 0..20000 {
            let mut m = base.to_vec();
            let n = (rnd() % 6) as usize;
            for _ in 0..=n {
                let i = (rnd() as usize) % m.len();
                match rnd() % 3 {
                    0 => m[i] = rnd() as u8,
                    1 => {
                        m.remove(i);
                    }
                    _ => m.insert(i, rnd() as u8),
                }
                if m.is_empty() {
                    m.push(b'$');
                }
            }
            let mut d = Decoder::new(Limits::default());
            let cut = (rnd() as usize) % (m.len() + 1);
            for part in [&m[..cut], &m[cut..]] {
                if d.feed(part).is_err() {
                    break;
                }
                while let Ok(Some(_)) = d.decode() {}
                assert!(d.buffered() <= Limits::default().max_buffer());
            }
        }
    }
}
