//! HTTP/1.1 の輸送（senca-http の sans-io 解析 + tokio の TCP、https は senca-tls）。
//!
//! 1 要求 1 接続（`Connection: close`）。応答の本文は [`ResponseBody::chunk`] で少しずつ読める（SSE・大きな書き出し）。

use bytes::{Bytes, BytesMut};
use senca_http::{BodyFraming, Event, Limits, Method, Request, ResponseParser};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::net::TcpStream;

use crate::error::Error;

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// 解析した URL（`http`・`https` だけ。利用者情報は拒否）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// `/` で始まるパス（クエリを含みうる）。
    pub path: String,
}

impl Url {
    pub fn parse(u: &str) -> Result<Url, Error> {
        let bad = || Error::InvalidUrl(u.to_string());
        let (scheme, rest) = u.split_once("://").ok_or_else(bad)?;
        let tls = match scheme.to_ascii_lowercase().as_str() {
            "http" => false,
            "https" => true,
            _ => return Err(bad()),
        };
        let (auth, path) = match rest.find(['/', '?', '#']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if auth.is_empty() || auth.contains('@') {
            return Err(bad());
        }
        let (host, port) = if let Some(r) = auth.strip_prefix('[') {
            let (h, after) = r.split_once(']').ok_or_else(bad)?;
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse().map_err(|_| bad())?,
                None if after.is_empty() => default_port(tls),
                None => return Err(bad()),
            };
            (h.to_string(), port)
        } else {
            match auth.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse().map_err(|_| bad())?),
                None => (auth.to_string(), default_port(tls)),
            }
        };
        if host.is_empty() {
            return Err(bad());
        }
        let path = path.split('#').next().unwrap_or("");
        let path = if path.is_empty() {
            "/".to_string()
        } else if path.starts_with('?') {
            format!("/{path}")
        } else {
            path.to_string()
        };
        Ok(Url { tls, host, port, path })
    }

    /// `Host` ヘッダーの値。
    pub fn host_header(&self) -> String {
        let h = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == default_port(self.tls) { h } else { format!("{h}:{}", self.port) }
    }

    /// `scheme://host:port`。
    pub fn origin(&self) -> String {
        format!("{}://{}", if self.tls { "https" } else { "http" }, self.host_header())
    }
}

fn default_port(tls: bool) -> u16 {
    if tls { 443 } else { 80 }
}

/// 送る要求。
#[derive(Debug, Clone)]
pub struct RawRequest {
    pub method: Method,
    pub url: Url,
    pub headers: Vec<(String, String)>,
    pub body: Option<Bytes>,
}

/// 応答の先頭（本文はまだ読んでいない）。
pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ResponseBody,
}

impl RawResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }

    /// 本文をすべて読む（`max` を超えたら誤り）。
    pub async fn read_all(mut self, max: usize) -> Result<Bytes, Error> {
        let mut out = BytesMut::new();
        while let Some(c) = self.body.chunk().await? {
            if out.len() + c.len() > max {
                return Err(Error::Transport(format!("応答の本文が {max} バイトを超えました")));
            }
            out.extend_from_slice(&c);
        }
        Ok(out.freeze())
    }
}

pub(crate) fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// 応答の本文の読み手。
pub struct ResponseBody {
    io: Box<dyn Io>,
    parser: ResponseParser,
    buf: Vec<u8>,
    done: bool,
}

impl ResponseBody {
    /// 次の本文の断片（終われば `None`）。
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, Error> {
        if self.done {
            return Ok(None);
        }
        loop {
            match self.parser.poll().map_err(|e| Error::Transport(format!("応答の解析: {e:?}")))? {
                Event::NeedMore => {
                    let n = self.io.read(&mut self.buf).await.map_err(|e| Error::Transport(e.to_string()))?;
                    if n == 0 {
                        self.parser.feed_eof();
                    } else {
                        self.parser.feed(&self.buf[..n]).map_err(|e| Error::Transport(format!("{e:?}")))?;
                    }
                }
                Event::Head(_) => {}
                Event::Data(d) => {
                    if !d.is_empty() {
                        return Ok(Some(d));
                    }
                }
                Event::End(_) | Event::Closed => {
                    self.done = true;
                    return Ok(None);
                }
            }
        }
    }
}

/// TLS の設定。
#[derive(Clone)]
pub enum TlsRoots {
    /// OS の信頼する認証局。
    System,
    /// 与えた PEM の認証局だけ（自己署名の試験環境など）。
    Pem(Arc<Vec<u8>>),
}

pub(crate) fn tls_config(roots: &TlsRoots) -> Result<Arc<senca_tls::ClientConfig>, Error> {
    let store = match roots {
        TlsRoots::System => senca_tls::RootStore::system().map_err(|e| Error::Transport(format!("信頼する認証局: {e:?}")))?,
        TlsRoots::Pem(p) => {
            let mut s = senca_tls::RootStore::new();
            s.add_pem(p).map_err(|e| Error::Transport(format!("認証局の PEM: {e:?}")))?;
            s
        }
    };
    let mut cfg = senca_tls::ClientConfig::new(senca_tls::ServerVerifier::WebPki { roots: Arc::new(store), spki_pins: vec![] });
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

/// 要求を送り、応答の先頭まで読む。`head_timeout` は接続から応答の先頭までの時間。
pub async fn send(
    req: RawRequest,
    tls: Option<Arc<senca_tls::ClientConfig>>,
    head_timeout: Duration,
    max_body: u64,
) -> Result<RawResponse, Error> {
    let fut = async {
        let tcp = TcpStream::connect((req.url.host.as_str(), req.url.port))
            .await
            .map_err(|e| Error::Connect(format!("{}: {e}", req.url.origin())))?;
        let _ = tcp.set_nodelay(true);
        let mut io: Box<dyn Io> = if req.url.tls {
            let cfg = tls.ok_or_else(|| Error::Transport("TLS の設定がありません".into()))?;
            Box::new(tls_connect(tcp, cfg, &req.url.host).await.map_err(Error::Connect)?)
        } else {
            Box::new(tcp)
        };
        let enc = |e: senca_http::HttpError| Error::Transport(format!("要求の組み立て: {e:?}"));
        let mut r = Request::new(req.method.clone(), req.url.path.clone()).header("Host", req.url.host_header()).map_err(enc)?;
        r = r.header("Connection", "close").map_err(enc)?;
        for (k, v) in &req.headers {
            r = r.header(k, v.clone()).map_err(enc)?;
        }
        let framing = match &req.body {
            Some(b) => BodyFraming::Length(b.len() as u64),
            None if matches!(req.method, Method::Post | Method::Put | Method::Patch) => BodyFraming::Length(0),
            None => BodyFraming::None,
        };
        let head = r.encode_head(framing).map_err(enc)?;
        io.write_all(&head).await.map_err(|e| Error::Transport(e.to_string()))?;
        if let Some(b) = &req.body {
            io.write_all(b).await.map_err(|e| Error::Transport(e.to_string()))?;
        }
        io.flush().await.map_err(|e| Error::Transport(e.to_string()))?;
        let mut parser = ResponseParser::new(Limits { max_body, ..Limits::DEFAULT });
        parser.push_request(req.method.clone());
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match parser.poll().map_err(|e| Error::Transport(format!("応答の解析: {e:?}")))? {
                Event::NeedMore => {
                    let n = io.read(&mut buf).await.map_err(|e| Error::Transport(e.to_string()))?;
                    if n == 0 {
                        parser.feed_eof();
                    } else {
                        parser.feed(&buf[..n]).map_err(|e| Error::Transport(format!("{e:?}")))?;
                    }
                }
                Event::Head(h) => {
                    if (100..200).contains(&h.status) {
                        continue;
                    }
                    let headers = h.headers.iter().map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v).into_owned())).collect();
                    return Ok(RawResponse {
                        status: h.status,
                        headers,
                        body: ResponseBody { io, parser, buf, done: false },
                    });
                }
                Event::Data(_) | Event::End(_) => {}
                Event::Closed => return Err(Error::Transport("応答の前に接続が閉じました".into())),
            }
        }
    };
    tokio::time::timeout(head_timeout, fut).await.map_err(|_| Error::Timeout)?
}

// ------------------------------------------------------------------ TLS（senca-tls を tokio のストリームとして使う）

const TLS_BUF: usize = 64 * 1024;

/// TLS でつなぎ、ハンドシェイクが済んだらアプリ側の端（平文）を返す。記録の暗号化・復号は別のタスクが中継する。
async fn tls_connect(mut tcp: TcpStream, cfg: Arc<senca_tls::ClientConfig>, server_name: &str) -> Result<DuplexStream, String> {
    use senca_tls::{Connection, TlsSession};
    let mut conn = Connection::client(cfg, server_name, SystemTime::now()).map_err(|e| format!("TLS: {e:?}"))?;
    let mut buf = vec![0u8; TLS_BUF];
    let mut early: Vec<Vec<u8>> = Vec::new();
    loop {
        while conn.wants_write() {
            let out = conn.take_tls();
            tcp.write_all(&out).await.map_err(|e| e.to_string())?;
        }
        if !conn.is_handshaking() {
            break;
        }
        let n = tcp.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("TLS のハンドシェイク中に切れました".into());
        }
        let mut off = 0;
        while off < n {
            let k = conn.read_tls(&buf[off..n]).map_err(|e| format!("TLS: {e:?}"))?;
            if k == 0 {
                match conn.take_plaintext() {
                    Some(p) => early.push(p),
                    None => return Err("TLS が入力を受け付けません".into()),
                }
                continue;
            }
            off += k;
        }
    }
    let (app, mut pump) = tokio::io::duplex(TLS_BUF);
    tokio::spawn(async move {
        let mut nbuf = vec![0u8; TLS_BUF];
        let mut pbuf = vec![0u8; TLS_BUF];
        for p in early {
            if pump.write_all(&p).await.is_err() {
                return;
            }
        }
        while let Some(p) = conn.take_plaintext() {
            if pump.write_all(&p).await.is_err() {
                return;
            }
        }
        loop {
            tokio::select! {
                r = tcp.read(&mut nbuf) => {
                    let n = match r { Ok(0) | Err(_) => break, Ok(n) => n };
                    let mut off = 0;
                    let mut bad = false;
                    while off < n {
                        let stalled = match conn.read_tls(&nbuf[off..n]) {
                            Ok(0) => true,
                            Ok(k) => { off += k; false }
                            Err(_) => { bad = true; break; }
                        };
                        let mut drained = false;
                        while let Some(p) = conn.take_plaintext() {
                            drained = true;
                            if pump.write_all(&p).await.is_err() { return; }
                        }
                        if stalled && !drained { bad = true; break; }
                    }
                    if bad || conn.peer_closed() { break; }
                }
                r = pump.read(&mut pbuf) => {
                    let n = match r { Ok(0) | Err(_) => { conn.send_close_notify(); 0 } Ok(n) => n };
                    if n > 0 && conn.write_plaintext(&pbuf[..n]).is_err() { break; }
                    if n == 0 {
                        let out = conn.take_tls();
                        let _ = tcp.write_all(&out).await;
                        break;
                    }
                }
            }
            while conn.wants_write() {
                let out = conn.take_tls();
                if tcp.write_all(&out).await.is_err() {
                    return;
                }
            }
        }
        let _ = pump.shutdown().await;
    });
    Ok(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let u = Url::parse("http://127.0.0.1:8080/api/v1?x=1").unwrap();
        assert_eq!((u.tls, u.host.as_str(), u.port, u.path.as_str()), (false, "127.0.0.1", 8080, "/api/v1?x=1"));
        assert_eq!(u.host_header(), "127.0.0.1:8080");
        let u = Url::parse("https://senca.example").unwrap();
        assert_eq!((u.tls, u.port, u.path.as_str(), u.host_header()), (true, 443, "/", "senca.example".to_string()));
        let u = Url::parse("http://[::1]:9/x#frag").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str(), u.host_header()), ("::1", 9, "/x", "[::1]:9".to_string()));
        assert_eq!(Url::parse("HTTPS://h?q").unwrap().path, "/?q");
        for bad in ["ftp://h/", "http://u:p@h/", "http://", "h/x", "http://h:99999/", "http://[::1/"] {
            assert!(Url::parse(bad).is_err(), "{bad}");
        }
    }
}
