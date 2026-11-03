//! 小さな HTTP/1.1 クライアント（HLS pull・PaaS の API・字幕のサービス）。解析は `senca-http`。
//! 宛先は egress の方針で検査する。https は TLS の統合（senca-tls）でつなぐ。

use bytes::{Bytes, BytesMut};
use senca_http::{BodyFraming, Event, Limits, Method, Request, ResponseParser};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[allow(dead_code)] // headers は PaaS の応答（Location など）で使う
trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Stream for T {}

/// 外向きの TLS の設定（一度だけ作る）。
pub fn client_tls() -> Result<std::sync::Arc<senca_tls::ClientConfig>, String> {
    static CFG: std::sync::OnceLock<Result<std::sync::Arc<senca_tls::ClientConfig>, String>> = std::sync::OnceLock::new();
    CFG.get_or_init(|| crate::tls_io::client_config(&[b"http/1.1"])).clone()
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

#[allow(dead_code)]
impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

pub struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

pub fn parse_url(u: &str) -> Option<Url> {
    let (scheme, rest) = u.split_once("://")?;
    let tls = match scheme {
        "http" => false,
        "https" => true,
        _ => return None,
    };
    let (auth, path) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if auth.contains('@') || auth.is_empty() {
        return None;
    }
    let (host, port) = match auth.rsplit_once(':') {
        Some((h, p)) if !p.contains(']') => (h.trim_matches(['[', ']']).to_string(), p.parse().ok()?),
        _ => (auth.trim_matches(['[', ']']).to_string(), if tls { 443 } else { 80 }),
    };
    let path = if path.starts_with('?') { format!("/{path}") } else { path.to_string() };
    Some(Url { tls, host, port, path })
}

/// 相対 URL を解決する（HLS のセグメントの URI 用）。
pub fn resolve(base: &str, rel: &str) -> String {
    if rel.contains("://") {
        return rel.to_string();
    }
    let (scheme, rest) = base.split_once("://").unwrap_or(("http", base));
    let auth_end = rest.find('/').unwrap_or(rest.len());
    let origin = format!("{scheme}://{}", &rest[..auth_end]);
    if rel.starts_with('/') {
        return format!("{origin}{rel}");
    }
    let path = &rest[auth_end..];
    let path = path.split('?').next().unwrap_or("");
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    format!("{origin}{dir}{rel}")
}

/// 1 回の要求（接続は使い捨て）。`max_body` を超えたら失敗。
pub async fn request(
    allow: &[String],
    method: Method,
    url: &str,
    headers: &[(&str, String)],
    body: Option<Bytes>,
    max_body: usize,
    timeout: Duration,
) -> Result<HttpResponse, String> {
    let u = parse_url(url).ok_or("url の形が違います")?;
    let addr = crate::egress::resolve(&u.host, u.port, allow).await?;
    exchange(u, addr, method, url, headers, body, max_body, timeout).await
}

/// 管理者が設定した宛先（録画の保存先など）への要求。宛先の制限（egress）を通さない。
/// 利用者が渡した URL には使わない。
pub async fn request_trusted(
    method: Method,
    url: &str,
    headers: &[(&str, String)],
    body: Option<Bytes>,
    max_body: usize,
    timeout: Duration,
) -> Result<HttpResponse, String> {
    let u = parse_url(url).ok_or("url の形が違います")?;
    let addr = tokio::time::timeout(timeout, tokio::net::lookup_host((u.host.as_str(), u.port)))
        .await
        .map_err(|_| format!("resolve {}: timeout", u.host))?
        .map_err(|e| format!("resolve {}: {e}", u.host))?
        .next()
        .ok_or_else(|| format!("resolve {}: no address", u.host))?;
    exchange(u, addr, method, url, headers, body, max_body, timeout).await
}

#[allow(clippy::too_many_arguments)]
async fn exchange(
    u: Url,
    addr: std::net::SocketAddr,
    method: Method,
    url: &str,
    headers: &[(&str, String)],
    body: Option<Bytes>,
    max_body: usize,
    timeout: Duration,
) -> Result<HttpResponse, String> {
    let fut = async {
        let tcp = tokio::net::TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
        let mut sock: Box<dyn Stream> = if u.tls { Box::new(crate::tls_io::connect(tcp, client_tls()?, &u.host).await?) } else { Box::new(tcp) };
        let host = if (u.port == 80 && !u.tls) || (u.port == 443 && u.tls) { u.host.clone() } else { format!("{}:{}", u.host, u.port) };
        let mut req = Request::new(method.clone(), u.path.clone()).header("Host", host).map_err(|e| e.to_string())?;
        req = req.header("User-Agent", "senca").map_err(|e| e.to_string())?;
        req = req.header("Connection", "close").map_err(|e| e.to_string())?;
        for (k, v) in headers {
            req = req.header(k, v.clone()).map_err(|e| e.to_string())?;
        }
        let framing = match &body {
            Some(b) => BodyFraming::Length(b.len() as u64),
            None => BodyFraming::None,
        };
        let head = req.encode_head(framing).map_err(|e| e.to_string())?;
        sock.write_all(&head).await.map_err(|e| e.to_string())?;
        if let Some(b) = &body {
            sock.write_all(b).await.map_err(|e| e.to_string())?;
        }
        let mut p = ResponseParser::new(Limits { max_body: max_body as u64, ..Limits::DEFAULT });
        p.push_request(method);
        let mut buf = vec![0u8; 64 * 1024];
        let mut status = 0;
        let mut hdrs = Vec::new();
        let mut out = BytesMut::new();
        loop {
            match p.poll().map_err(|e| e.to_string())? {
                Event::NeedMore => {
                    let n = sock.read(&mut buf).await.map_err(|e| e.to_string())?;
                    if n == 0 {
                        p.feed_eof();
                    } else {
                        p.feed(&buf[..n]).map_err(|e| e.to_string())?;
                    }
                }
                Event::Head(h) => {
                    status = h.status;
                    hdrs = h.headers.iter().map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v).into_owned())).collect();
                }
                Event::Data(d) => {
                    if out.len() + d.len() > max_body {
                        return Err("応答が大きすぎます".to_string());
                    }
                    out.extend_from_slice(&d);
                }
                Event::End(_) | Event::Closed => break,
            }
        }
        Ok(HttpResponse { status, headers: hdrs, body: out.freeze() })
    };
    tokio::time::timeout(timeout, fut).await.map_err(|_| format!("{url}: timeout"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let u = parse_url("http://a.b:8080/x/y.m3u8?q=1").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("a.b", 8080, "/x/y.m3u8?q=1"));
        assert!(parse_url("http://u:p@a.b/").is_none());
        assert_eq!(resolve("http://a.b/x/y.m3u8?t=1", "s1.ts"), "http://a.b/x/s1.ts");
        assert_eq!(resolve("http://a.b/x/y.m3u8", "/z/s1.ts"), "http://a.b/z/s1.ts");
        assert_eq!(resolve("http://a.b/x/y.m3u8", "https://c.d/s.ts"), "https://c.d/s.ts");
    }
}
