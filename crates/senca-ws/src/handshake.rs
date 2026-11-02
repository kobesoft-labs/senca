//! 開始の handshake（RFC 6455 4 章）。HTTP の解析は senca-http に任せ、ここは header の検査と生成だけ。

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use senca_http::{Method, RequestHead, ResponseHead, Version};
use sha1::{Digest, Sha1};

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// `Sec-WebSocket-Accept` の値。
pub fn accept_key(key: &str) -> String {
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(GUID.as_bytes());
    STANDARD.encode(h.finalize())
}

/// Origin を検査する関数（Origin の値か `None`）。
pub type OriginCheck = std::sync::Arc<dyn Fn(Option<&str>) -> bool + Send + Sync>;

/// Origin の検査。ブラウザーは必ず Origin を付ける（付けないのはブラウザー以外）。
#[derive(Clone, Default)]
pub enum OriginPolicy {
    /// 検査しない。
    #[default]
    Any,
    /// この一覧（`https://example.com` の形、大文字小文字を区別しない）だけ。Origin の無い要求は
    /// `allow_missing` なら通す（ブラウザー以外のクライアント）。
    List { origins: Vec<String>, allow_missing: bool },
    /// 呼び出し側の関数（Origin の値か `None`）。
    Custom(OriginCheck),
}

impl std::fmt::Debug for OriginPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OriginPolicy::Any => f.write_str("Any"),
            OriginPolicy::List { origins, allow_missing } => f
                .debug_struct("List")
                .field("origins", origins)
                .field("allow_missing", allow_missing)
                .finish(),
            OriginPolicy::Custom(_) => f.write_str("Custom"),
        }
    }
}

impl OriginPolicy {
    fn allows(&self, origin: Option<&str>) -> bool {
        match self {
            OriginPolicy::Any => true,
            OriginPolicy::List { origins, allow_missing } => match origin {
                None => *allow_missing,
                Some(o) => origins.iter().any(|x| x.eq_ignore_ascii_case(o)),
            },
            OriginPolicy::Custom(f) => f(origin),
        }
    }
}

/// サーバーの handshake の設定。
#[derive(Debug, Clone, Default)]
pub struct AcceptOptions {
    /// こちらが話せる subprotocol（好みの順）。相手が申し出た中で最初に一致したものを選ぶ。
    pub subprotocols: Vec<String>,
    /// 一致する subprotocol が無ければ拒否する（400）。偽なら subprotocol なしで受ける。
    pub require_subprotocol: bool,
    pub origin: OriginPolicy,
}

/// handshake の誤り。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeError {
    /// GET ・HTTP/1.1 でない、body がある、Host が無いなど（400）。
    BadRequest(&'static str),
    /// `Sec-WebSocket-Version` が 13 でない（426 と `Sec-WebSocket-Version: 13`）。
    UnsupportedVersion,
    /// Origin を受け付けない（403）。
    ForbiddenOrigin,
    /// 一致する subprotocol が無い（400）。
    NoSubprotocol,
    /// クライアント側: 応答が 101 でない（状態コード）。
    NotSwitching(u16),
    /// クライアント側: 応答の header が不正。
    BadResponse(&'static str),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::BadRequest(s) => write!(f, "bad websocket request: {s}"),
            HandshakeError::UnsupportedVersion => f.write_str("unsupported websocket version"),
            HandshakeError::ForbiddenOrigin => f.write_str("origin not allowed"),
            HandshakeError::NoSubprotocol => f.write_str("no acceptable subprotocol"),
            HandshakeError::NotSwitching(s) => write!(f, "server answered {s}"),
            HandshakeError::BadResponse(s) => write!(f, "bad websocket response: {s}"),
        }
    }
}

impl std::error::Error for HandshakeError {}

impl HandshakeError {
    /// サーバーが返す状態コード。
    pub fn status(&self) -> u16 {
        match self {
            HandshakeError::UnsupportedVersion => 426,
            HandshakeError::ForbiddenOrigin => 403,
            _ => 400,
        }
    }

    /// サーバーが返す応答（接続は閉じる）。
    pub fn response_bytes(&self) -> Bytes {
        let (status, reason) = match self.status() {
            426 => (426, "Upgrade Required"),
            403 => (403, "Forbidden"),
            _ => (400, "Bad Request"),
        };
        let extra = if status == 426 {
            "Sec-WebSocket-Version: 13\r\nUpgrade: websocket\r\n"
        } else {
            ""
        };
        let body = self.to_string();
        Bytes::from(format!(
            "HTTP/1.1 {status} {reason}\r\n{extra}Content-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ))
    }
}

/// 受け付けた handshake。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub accept: String,
    pub subprotocol: Option<String>,
    pub origin: Option<String>,
}

impl Accepted {
    /// 101 の応答（この後は WebSocket の frame）。
    pub fn response_bytes(&self) -> Bytes {
        let mut s = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n",
            self.accept
        );
        if let Some(p) = &self.subprotocol {
            s.push_str(&format!("Sec-WebSocket-Protocol: {p}\r\n"));
        }
        s.push_str("\r\n");
        Bytes::from(s)
    }
}

fn header_str<'a>(h: &'a senca_http::HeaderMap, name: &str) -> Result<Option<&'a str>, HandshakeError> {
    match h.get_unique(name) {
        Ok(Some(v)) => std::str::from_utf8(v)
            .map(|s| Some(s.trim()))
            .map_err(|_| HandshakeError::BadRequest("non-ASCII header")),
        Ok(None) => Ok(None),
        Err(_) => Err(HandshakeError::BadRequest("duplicate header")),
    }
}

fn is_token(s: &[u8]) -> bool {
    senca_http::headers::is_token(s)
}

/// 要求の head を検査し、受け付ける。
pub fn accept(head: &RequestHead, opts: &AcceptOptions) -> Result<Accepted, HandshakeError> {
    if head.method != Method::Get {
        return Err(HandshakeError::BadRequest("method must be GET"));
    }
    if head.version != Version::Http11 {
        return Err(HandshakeError::BadRequest("HTTP/1.1 required"));
    }
    if head.has_body() {
        return Err(HandshakeError::BadRequest("request body not allowed"));
    }
    let h = &head.headers;
    if header_str(h, "host")?.is_none_or(str::is_empty) {
        return Err(HandshakeError::BadRequest("Host missing"));
    }
    if !h.has_token("upgrade", "websocket") {
        return Err(HandshakeError::BadRequest("Upgrade: websocket missing"));
    }
    if !h.has_token("connection", "upgrade") {
        return Err(HandshakeError::BadRequest("Connection: Upgrade missing"));
    }
    match header_str(h, "sec-websocket-version")? {
        Some("13") => {}
        _ => return Err(HandshakeError::UnsupportedVersion),
    }
    let key = header_str(h, "sec-websocket-key")?.ok_or(HandshakeError::BadRequest("Sec-WebSocket-Key missing"))?;
    match STANDARD.decode(key) {
        Ok(k) if k.len() == 16 => {}
        _ => return Err(HandshakeError::BadRequest("Sec-WebSocket-Key must be 16 bytes in base64")),
    }
    let origin = header_str(h, "origin")?.map(str::to_string);
    if !opts.origin.allows(origin.as_deref()) {
        return Err(HandshakeError::ForbiddenOrigin);
    }
    let mut offered = Vec::new();
    for item in h.list_items("sec-websocket-protocol") {
        if !is_token(item) {
            return Err(HandshakeError::BadRequest("Sec-WebSocket-Protocol is not a token list"));
        }
        offered.push(String::from_utf8_lossy(item).into_owned());
    }
    let subprotocol = opts
        .subprotocols
        .iter()
        .find(|p| offered.iter().any(|o| o == *p))
        .cloned();
    if subprotocol.is_none() && opts.require_subprotocol {
        return Err(HandshakeError::NoSubprotocol);
    }
    // Sec-WebSocket-Extensions（permessage-deflate など）は応答に載せない = 使わない
    Ok(Accepted {
        accept: accept_key(key),
        subprotocol,
        origin,
    })
}

/// クライアントの handshake。
#[derive(Debug, Clone)]
pub struct ClientHandshake {
    key: String,
    protocols: Vec<String>,
}

impl ClientHandshake {
    /// 要求を作る（`key` は 16 byte の乱数。`None` なら getrandom）。
    pub fn new(
        host: &str,
        path: &str,
        origin: Option<&str>,
        protocols: &[&str],
        key: Option<[u8; 16]>,
    ) -> Result<(ClientHandshake, Bytes), HandshakeError> {
        let k = match key {
            Some(k) => k,
            None => {
                let mut k = [0u8; 16];
                getrandom::fill(&mut k).map_err(|_| HandshakeError::BadRequest("no randomness"))?;
                k
            }
        };
        for v in [host, path].iter().chain(origin.iter()) {
            if v.bytes().any(|b| b < 0x21 || b == 0x7f) {
                return Err(HandshakeError::BadRequest("invalid characters"));
            }
        }
        if protocols.iter().any(|p| !is_token(p.as_bytes())) {
            return Err(HandshakeError::BadRequest("subprotocol is not a token"));
        }
        let key = STANDARD.encode(k);
        let mut s = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
        );
        if let Some(o) = origin {
            s.push_str(&format!("Origin: {o}\r\n"));
        }
        if !protocols.is_empty() {
            s.push_str(&format!("Sec-WebSocket-Protocol: {}\r\n", protocols.join(", ")));
        }
        s.push_str("\r\n");
        Ok((
            ClientHandshake {
                key,
                protocols: protocols.iter().map(|p| p.to_string()).collect(),
            },
            Bytes::from(s),
        ))
    }

    /// 応答を検査する。選ばれた subprotocol を返す。
    pub fn verify(&self, resp: &ResponseHead) -> Result<Option<String>, HandshakeError> {
        if resp.status != 101 {
            return Err(HandshakeError::NotSwitching(resp.status));
        }
        let h = &resp.headers;
        if !h.has_token("upgrade", "websocket") {
            return Err(HandshakeError::BadResponse("Upgrade: websocket missing"));
        }
        if !h.has_token("connection", "upgrade") {
            return Err(HandshakeError::BadResponse("Connection: Upgrade missing"));
        }
        let acc = h
            .get_unique("sec-websocket-accept")
            .map_err(|_| HandshakeError::BadResponse("duplicate Sec-WebSocket-Accept"))?
            .ok_or(HandshakeError::BadResponse("Sec-WebSocket-Accept missing"))?;
        if acc.as_ref() != accept_key(&self.key).as_bytes() {
            return Err(HandshakeError::BadResponse("Sec-WebSocket-Accept mismatch"));
        }
        if h.contains("sec-websocket-extensions") {
            return Err(HandshakeError::BadResponse("extension not offered"));
        }
        match h.get_unique("sec-websocket-protocol") {
            Ok(None) => Ok(None),
            Ok(Some(p)) => {
                let p = String::from_utf8_lossy(p).trim().to_string();
                if self.protocols.contains(&p) {
                    Ok(Some(p))
                } else {
                    Err(HandshakeError::BadResponse("subprotocol not offered"))
                }
            }
            Err(_) => Err(HandshakeError::BadResponse("duplicate Sec-WebSocket-Protocol")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use senca_http::{Event, Limits, RequestParser, ResponseParser};

    fn head(raw: &str) -> RequestHead {
        let mut p = RequestParser::new(Limits::DEFAULT);
        p.feed(raw.as_bytes()).unwrap();
        match p.poll().unwrap() {
            Event::Head(h) => h,
            e => panic!("{e:?}"),
        }
    }

    const OK: &str = "GET /signaling HTTP/1.1\r\nHost: example.com\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nOrigin: https://app.example.com\r\nSec-WebSocket-Protocol: chat, senca.rooms.v1\r\nSec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n\r\n";

    #[test]
    fn rfc_accept_key() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn accepts_and_negotiates() {
        let opts = AcceptOptions {
            subprotocols: vec!["senca.rooms.v1".into()],
            require_subprotocol: true,
            origin: OriginPolicy::List {
                origins: vec!["https://APP.example.com".into()],
                allow_missing: false,
            },
        };
        let a = accept(&head(OK), &opts).unwrap();
        assert_eq!(a.subprotocol.as_deref(), Some("senca.rooms.v1"));
        let r = String::from_utf8(a.response_bytes().to_vec()).unwrap();
        assert!(r.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
        assert!(r.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
        assert!(r.contains("Sec-WebSocket-Protocol: senca.rooms.v1\r\n"));
        // 拡張は返さない
        assert!(!r.to_ascii_lowercase().contains("extensions"));
    }

    #[test]
    fn rejections() {
        let o = AcceptOptions::default();
        let cases: Vec<(String, u16)> = vec![
            (OK.replace("GET", "POST"), 400),
            (OK.replace("HTTP/1.1", "HTTP/1.0"), 400),
            (OK.replace("Version: 13", "Version: 8"), 426),
            (OK.replace("Upgrade: websocket\r\n", ""), 400),
            (OK.replace("keep-alive, Upgrade", "keep-alive"), 400),
            (OK.replace("dGhlIHNhbXBsZSBub25jZQ==", "c2hvcnQ="), 400),
            (OK.replace("Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n", ""), 400),
            (
                OK.replace("Host: example.com\r\n", "Host: example.com\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"),
                400,
            ),
            (OK.replace("\r\n\r\n", "\r\nContent-Length: 3\r\n\r\nabc"), 400),
        ];
        for (raw, status) in cases {
            let e = accept(&head(&raw), &o).unwrap_err();
            assert_eq!(e.status(), status, "{raw}");
            let r = String::from_utf8(e.response_bytes().to_vec()).unwrap();
            assert!(r.starts_with(&format!("HTTP/1.1 {status} ")));
            if status == 426 {
                assert!(r.contains("Sec-WebSocket-Version: 13\r\n"));
            }
        }
        // Origin
        let list = |allow_missing| AcceptOptions {
            origin: OriginPolicy::List {
                origins: vec!["https://other.example".into()],
                allow_missing,
            },
            ..Default::default()
        };
        assert_eq!(accept(&head(OK), &list(true)), Err(HandshakeError::ForbiddenOrigin));
        let no_origin = OK.replace("Origin: https://app.example.com\r\n", "");
        assert!(accept(&head(&no_origin), &list(true)).is_ok());
        assert_eq!(accept(&head(&no_origin), &list(false)), Err(HandshakeError::ForbiddenOrigin));
        let custom = AcceptOptions {
            origin: OriginPolicy::Custom(std::sync::Arc::new(|o| o.is_some_and(|o| o.ends_with(".example.com")))),
            ..Default::default()
        };
        assert!(accept(&head(OK), &custom).is_ok());
        // subprotocol が要るのに一致しない
        let req = AcceptOptions {
            subprotocols: vec!["other".into()],
            require_subprotocol: true,
            ..Default::default()
        };
        assert_eq!(accept(&head(OK), &req), Err(HandshakeError::NoSubprotocol));
        // 要らなければ subprotocol なし
        let opt = AcceptOptions {
            subprotocols: vec!["other".into()],
            ..Default::default()
        };
        assert_eq!(accept(&head(OK), &opt).unwrap().subprotocol, None);
    }

    fn resp(raw: &str) -> ResponseHead {
        let mut p = ResponseParser::new(Limits::DEFAULT);
        p.feed(raw.as_bytes()).unwrap();
        match p.poll().unwrap() {
            Event::Head(h) => h,
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn client_round_trip() {
        let (c, req) = ClientHandshake::new("h:1", "/s?x=1", Some("http://h"), &["senca.rooms.v1"], Some([7u8; 16])).unwrap();
        let a = accept(
            &head(std::str::from_utf8(&req).unwrap()),
            &AcceptOptions {
                subprotocols: vec!["senca.rooms.v1".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let r = resp(std::str::from_utf8(&a.response_bytes()).unwrap());
        assert_eq!(c.verify(&r).unwrap().as_deref(), Some("senca.rooms.v1"));
        // 鍵の違い・拡張・申し出ていない subprotocol・101 でない
        let good = String::from_utf8(a.response_bytes().to_vec()).unwrap();
        let bad_key = good.replace(&a.accept, "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        assert!(c.verify(&resp(&bad_key)).is_err());
        let ext = good.replace("\r\n\r\n", "\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n");
        assert!(c.verify(&resp(&ext)).is_err());
        let proto = good.replace("senca.rooms.v1", "other");
        assert!(c.verify(&resp(&proto)).is_err());
        assert_eq!(
            c.verify(&resp("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")),
            Err(HandshakeError::NotSwitching(403))
        );
        assert!(ClientHandshake::new("h", "/a b", None, &[], None).is_err());
    }
}
