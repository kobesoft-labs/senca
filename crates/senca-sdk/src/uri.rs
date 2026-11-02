//! 送り先の設定文字列（REQ-009「入力の設定は 1 つの文字列で済むようにする」）。
//!
//! 2 つの書き方を受け付ける。
//!
//! 1. SENCA の QR 用の形（版付き、厳格）:
//!    - `senca://ingest?v=1&proto=srt&host=h&port=9000&streamid=publish:sk_x&passphrase=...&latency=200`
//!    - `senca://ingest?v=1&proto=rtmp&url=rtmps://h/live&key=sk_x`
//!    - `senca://ingest?v=1&proto=whip&url=https://h/whip/ID&token=...`
//! 2. 標準の URL（他社サーバーにも使える、R9-5）:
//!    - `srt://h:9000?streamid=publish:sk_x&passphrase=...&latency=200`（latency はミリ秒）
//!    - `rtmp://h/live/sk_x`・`rtmps://h/live/sk_x`（最後のパス要素が stream key）
//!    - `https://h/whip/ID`（token は別に渡す）
//!
//! 規則（`senca:` の形）:
//! - `v=1` は必須。知らない鍵は拒否する（先頭が `x-` の鍵だけは読み飛ばす、将来の拡張用）。
//! - 同じ鍵の重複、その proto で使わない鍵、空の値は拒否する。
//! - 値は RFC 3986 の percent-encoding。`+` は空白にしない。制御文字は拒否する。
//! - 全体は 2048 バイトまで（型番 10 の QR に入るのは 213 バイトまでなので、QR 化はさらに短くする必要がある）。
//!
//! stream key・passphrase・token・stream ID（鍵を含む）は [`Secret`] に入れ、Debug に出さない。

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

/// 設定文字列の最大長。
pub const MAX_URI_LEN: usize = 2048;
const SCHEME: &str = "senca://ingest?";

/// 秘密の文字列。Debug では中身を出さない。Display は実装しない。
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }

    /// 中身。送信に使うところでだけ呼ぶ。
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// 送り先。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// RTMP / RTMPS の publish。`url` は `rtmp://host[:port]/app` の形（key を含まない）。
    Rtmp { url: String, key: Secret },
    /// SRT の caller（live mode、MPEG-TS）。
    Srt {
        host: String,
        port: u16,
        /// 例: `publish:sk_...`。stream key を含みうるので秘密扱い。
        stream_id: Option<Secret>,
        passphrase: Option<Secret>,
        /// 受信遅延（ms）。
        latency_ms: Option<u16>,
        /// 鍵長（16・24・32 バイト）。passphrase があるときだけ。
        pbkeylen: Option<u8>,
    },
    /// WHIP（RFC 9725）。`token` は Bearer トークン。
    Whip { url: String, token: Option<Secret> },
}

impl Endpoint {
    /// proto の名前（`srt`・`rtmp`・`whip`）。
    pub fn proto(&self) -> &'static str {
        match self {
            Endpoint::Rtmp { .. } => "rtmp",
            Endpoint::Srt { .. } => "srt",
            Endpoint::Whip { .. } => "whip",
        }
    }
}

/// 送り先の設定（`senca:` の形の中身）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestUri {
    pub endpoint: Endpoint,
    /// 画面に出す名前（任意、64 文字まで）。
    pub name: Option<String>,
}

/// 設定文字列の誤り。値（秘密を含みうる）は入れず、鍵の名前と理由だけ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UriError {
    TooLong,
    /// 知らない scheme。
    Scheme,
    /// 形が壊れている（理由）。
    Malformed(&'static str),
    /// percent-encoding が壊れている・UTF-8 でない・制御文字がある。
    Encoding(String),
    MissingKey(&'static str),
    DuplicateKey(String),
    UnknownKey(String),
    /// 値が正しくない（鍵の名前・理由）。
    InvalidValue(String, &'static str),
    /// 対応していない版。
    Version,
}

impl fmt::Display for UriError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UriError::TooLong => write!(f, "longer than {MAX_URI_LEN} bytes"),
            UriError::Scheme => f.write_str(
                "unknown scheme (expected senca://ingest?, srt://, rtmp://, rtmps:// or https://)",
            ),
            UriError::Malformed(m) => write!(f, "malformed: {m}"),
            UriError::Encoding(k) => write!(f, "bad encoding in '{k}'"),
            UriError::MissingKey(k) => write!(f, "missing '{k}'"),
            UriError::DuplicateKey(k) => write!(f, "duplicate '{k}'"),
            UriError::UnknownKey(k) => write!(f, "unknown key '{k}'"),
            UriError::InvalidValue(k, m) => write!(f, "invalid '{k}': {m}"),
            UriError::Version => f.write_str("unsupported version (expected v=1)"),
        }
    }
}

impl std::error::Error for UriError {}

fn invalid(key: &str, why: &'static str) -> UriError {
    UriError::InvalidValue(key.to_owned(), why)
}

// ---- percent-encoding ----

fn is_unescaped(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b':' | b'/' | b'@')
}

fn encode(s: &str, out: &mut String) {
    for &b in s.as_bytes() {
        if is_unescaped(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(char::from(b"0123456789ABCDEF"[(b >> 4) as usize]));
            out.push(char::from(b"0123456789ABCDEF"[(b & 15) as usize]));
        }
    }
}

fn hexval(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode(key: &str, s: &str) -> Result<String, UriError> {
    let bad = || UriError::Encoding(key.to_owned());
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = b.get(i + 1).copied().and_then(hexval).ok_or_else(bad)?;
            let l = b.get(i + 2).copied().and_then(hexval).ok_or_else(bad)?;
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    let s = String::from_utf8(out).map_err(|_| bad())?;
    if s.chars().any(char::is_control) {
        return Err(bad());
    }
    Ok(s)
}

/// `k=v&k=v` を分ける（鍵の重複・空の値を拒否）。
fn parse_query(q: &str) -> Result<Vec<(String, String)>, UriError> {
    let mut out: Vec<(String, String)> = Vec::new();
    if q.is_empty() {
        return Ok(out);
    }
    for part in q.split('&') {
        let (k, v) = part
            .split_once('=')
            .ok_or(UriError::Malformed("query item without '='"))?;
        let k = decode("key", k)?;
        if k.is_empty()
            || k.len() > 32
            || !k
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
        {
            return Err(UriError::Malformed("bad key name"));
        }
        let v = decode(&k, v)?;
        if v.is_empty() {
            return Err(invalid(&k, "empty"));
        }
        if out.iter().any(|(e, _)| *e == k) {
            return Err(UriError::DuplicateKey(k));
        }
        out.push((k, v));
    }
    Ok(out)
}

// ---- 値の検査 ----

/// ホスト名・IPv4・IPv6（角括弧なし）。
fn valid_host(h: &str) -> bool {
    if h.parse::<Ipv4Addr>().is_ok() || h.parse::<Ipv6Addr>().is_ok() {
        return true;
    }
    if h.is_empty() || h.len() > 253 {
        return false;
    }
    let h = h.strip_suffix('.').unwrap_or(h);
    h.split('.').all(|l| {
        !l.is_empty()
            && l.len() <= 63
            && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    })
}

fn is_loopback(h: &str) -> bool {
    h.eq_ignore_ascii_case("localhost")
        || h.parse::<Ipv4Addr>().is_ok_and(|a| a.is_loopback())
        || h.parse::<Ipv6Addr>().is_ok_and(|a| a.is_loopback())
}

/// 空白・制御文字のない印字可能 ASCII で `1..=max` バイト。
fn printable(key: &str, v: &str, max: usize) -> Result<(), UriError> {
    if v.is_empty() || v.len() > max {
        return Err(invalid(key, "length out of range"));
    }
    if !v.bytes().all(|c| c.is_ascii_graphic()) {
        return Err(invalid(key, "must be printable ASCII without spaces"));
    }
    Ok(())
}

fn host_for_url(h: &str) -> String {
    if h.contains(':') {
        format!("[{h}]")
    } else {
        h.to_owned()
    }
}

/// 簡単な URL（`scheme://host[:port]/path[?query]`）。userinfo と fragment は拒否する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SimpleUrl {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    /// 先頭の `/` を含む（無ければ空）。
    pub path: String,
    pub query: Option<String>,
}

impl SimpleUrl {
    pub(crate) fn parse(key: &str, s: &str) -> Result<SimpleUrl, UriError> {
        printable(key, s, MAX_URI_LEN)?;
        let (scheme, rest) = s
            .split_once("://")
            .ok_or_else(|| invalid(key, "not an absolute URL"))?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme.is_empty() || !scheme.bytes().all(|c| c.is_ascii_lowercase()) {
            return Err(invalid(key, "bad scheme"));
        }
        if rest.contains('#') {
            return Err(invalid(key, "fragment not allowed"));
        }
        let (auth, path_q) = match rest.find(['/', '?']) {
            Some(i) => rest.split_at(i),
            None => (rest, ""),
        };
        if auth.contains('@') {
            return Err(invalid(key, "credentials in URL are not allowed"));
        }
        let (host, port) = if let Some(r) = auth.strip_prefix('[') {
            let (h, after) = r
                .split_once(']')
                .ok_or_else(|| invalid(key, "bad IPv6 host"))?;
            if h.parse::<Ipv6Addr>().is_err() {
                return Err(invalid(key, "bad IPv6 host"));
            }
            let port = match after {
                "" => None,
                p => Some(
                    p.strip_prefix(':')
                        .ok_or_else(|| invalid(key, "bad port"))?,
                ),
            };
            (h.to_owned(), port)
        } else {
            match auth.split_once(':') {
                Some((h, p)) => (h.to_owned(), Some(p)),
                None => (auth.to_owned(), None),
            }
        };
        if !valid_host(&host) || host.contains(':') && !auth.starts_with('[') {
            return Err(invalid(key, "bad host"));
        }
        let port = match port {
            None => None,
            Some(p) => Some(parse_port(key, p)?),
        };
        let (path, query) = match path_q.split_once('?') {
            Some((p, q)) => (p.to_owned(), Some(q.to_owned())),
            None => (path_q.to_owned(), None),
        };
        Ok(SimpleUrl {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
            path,
            query,
        })
    }

    /// 文字列に戻す。
    pub(crate) fn to_url(&self) -> String {
        let mut s = format!("{}://{}", self.scheme, host_for_url(&self.host));
        if let Some(p) = self.port {
            s.push_str(&format!(":{p}"));
        }
        s.push_str(&self.path);
        if let Some(q) = &self.query {
            s.push('?');
            s.push_str(q);
        }
        s
    }
}

fn parse_port(key: &str, p: &str) -> Result<u16, UriError> {
    if p.is_empty() || p.len() > 5 || !p.bytes().all(|c| c.is_ascii_digit()) {
        return Err(invalid(key, "bad port"));
    }
    match p.parse::<u16>() {
        Ok(0) | Err(_) => Err(invalid(key, "port out of range")),
        Ok(v) => Ok(v),
    }
}

fn check_rtmp_url(url: &str) -> Result<String, UriError> {
    let u = SimpleUrl::parse("url", url)?;
    if u.scheme != "rtmp" && u.scheme != "rtmps" {
        return Err(invalid("url", "must be rtmp:// or rtmps://"));
    }
    if u.path.trim_matches('/').is_empty() {
        return Err(invalid("url", "missing application name"));
    }
    Ok(u.to_url())
}

fn check_whip_url(url: &str) -> Result<String, UriError> {
    let u = SimpleUrl::parse("url", url)?;
    match u.scheme.as_str() {
        "https" => {}
        // 平文の HTTP は同じ端末の中（試験）だけ
        "http" if is_loopback(&u.host) => {}
        "http" => {
            return Err(invalid(
                "url",
                "WHIP must use https:// (http:// only for loopback)",
            ));
        }
        _ => return Err(invalid("url", "must be https://")),
    }
    if u.path.trim_matches('/').is_empty() {
        return Err(invalid("url", "missing path"));
    }
    Ok(u.to_url())
}

fn check_stream_id(v: &str) -> Result<(), UriError> {
    if v.is_empty() || v.len() > 512 {
        return Err(invalid("streamid", "must be 1..512 bytes"));
    }
    if v.chars().any(char::is_control) {
        return Err(invalid("streamid", "control character"));
    }
    Ok(())
}

fn check_passphrase(v: &str) -> Result<(), UriError> {
    if !(10..=79).contains(&v.len()) {
        return Err(invalid("passphrase", "must be 10..79 bytes"));
    }
    if v.chars().any(char::is_control) {
        return Err(invalid("passphrase", "control character"));
    }
    Ok(())
}

fn parse_latency(v: &str) -> Result<u16, UriError> {
    match v.parse::<u16>() {
        Ok(n) if (20..=8000).contains(&n) && v.bytes().all(|c| c.is_ascii_digit()) => Ok(n),
        _ => Err(invalid("latency", "must be 20..8000 (ms)")),
    }
}

fn parse_pbkeylen(v: &str) -> Result<u8, UriError> {
    match v {
        "16" => Ok(16),
        "24" => Ok(24),
        "32" => Ok(32),
        _ => Err(invalid("pbkeylen", "must be 16, 24 or 32")),
    }
}

fn check_name(v: &str) -> Result<(), UriError> {
    if v.chars().count() > 64 {
        return Err(invalid("name", "longer than 64 characters"));
    }
    Ok(())
}

/// 鍵と値の表から取り出す（取り出した鍵は消す。最後に残ったものは知らない鍵）。
struct Fields(Vec<(String, String)>);

impl Fields {
    fn take(&mut self, k: &str) -> Option<String> {
        let i = self.0.iter().position(|(e, _)| e == k)?;
        Some(self.0.remove(i).1)
    }

    fn need(&mut self, k: &'static str) -> Result<String, UriError> {
        self.take(k).ok_or(UriError::MissingKey(k))
    }

    /// 残った鍵が `x-` で始まるものだけであること。
    fn finish(self) -> Result<(), UriError> {
        match self.0.into_iter().find(|(k, _)| !k.starts_with("x-")) {
            Some((k, _)) => Err(UriError::UnknownKey(k)),
            None => Ok(()),
        }
    }
}

fn srt_from_fields(f: &mut Fields, host: String, port: u16) -> Result<Endpoint, UriError> {
    let stream_id = f.take("streamid");
    if let Some(s) = &stream_id {
        check_stream_id(s)?;
    }
    let passphrase = f.take("passphrase");
    if let Some(p) = &passphrase {
        check_passphrase(p)?;
    }
    let latency_ms = f.take("latency").map(|v| parse_latency(&v)).transpose()?;
    let pbkeylen = f.take("pbkeylen").map(|v| parse_pbkeylen(&v)).transpose()?;
    if pbkeylen.is_some() && passphrase.is_none() {
        return Err(invalid("pbkeylen", "requires passphrase"));
    }
    Ok(Endpoint::Srt {
        host,
        port,
        stream_id: stream_id.map(Secret),
        passphrase: passphrase.map(Secret),
        latency_ms,
        pbkeylen,
    })
}

impl IngestUri {
    /// `senca:` の形、または標準の URL を読む。
    pub fn parse(s: &str) -> Result<IngestUri, UriError> {
        if s.len() > MAX_URI_LEN {
            return Err(UriError::TooLong);
        }
        let s = s.trim();
        let lower = s.get(..SCHEME.len()).map(|p| p.to_ascii_lowercase());
        if lower.as_deref() == Some(SCHEME) {
            return Self::parse_senca(&s[SCHEME.len()..]);
        }
        let scheme = s
            .split_once("://")
            .map(|(a, _)| a.to_ascii_lowercase())
            .unwrap_or_default();
        match scheme.as_str() {
            "srt" => Self::parse_srt_url(s),
            "rtmp" | "rtmps" => Self::parse_rtmp_url(s),
            "https" | "http" => Ok(IngestUri {
                endpoint: Endpoint::Whip {
                    url: check_whip_url(s)?,
                    token: None,
                },
                name: None,
            }),
            _ => Err(UriError::Scheme),
        }
    }

    fn parse_senca(q: &str) -> Result<IngestUri, UriError> {
        if q.contains('#') {
            return Err(UriError::Malformed("fragment not allowed"));
        }
        let mut f = Fields(parse_query(q)?);
        if f.need("v")? != "1" {
            return Err(UriError::Version);
        }
        let proto = f.need("proto")?;
        let name = f.take("name");
        if let Some(n) = &name {
            check_name(n)?;
        }
        let endpoint = match proto.as_str() {
            "srt" => {
                let host = f.need("host")?;
                let host = host
                    .strip_prefix('[')
                    .and_then(|h| h.strip_suffix(']'))
                    .unwrap_or(&host)
                    .to_ascii_lowercase();
                if !valid_host(&host) {
                    return Err(invalid("host", "bad host"));
                }
                let port = parse_port("port", &f.need("port")?)?;
                srt_from_fields(&mut f, host, port)?
            }
            "rtmp" => {
                let url = check_rtmp_url(&f.need("url")?)?;
                let key = f.need("key")?;
                printable("key", &key, 512)?;
                Endpoint::Rtmp {
                    url,
                    key: Secret(key),
                }
            }
            "whip" => {
                let url = check_whip_url(&f.need("url")?)?;
                let token = f.take("token");
                if let Some(t) = &token {
                    printable("token", t, 4096)?;
                }
                Endpoint::Whip {
                    url,
                    token: token.map(Secret),
                }
            }
            _ => return Err(invalid("proto", "must be srt, rtmp or whip")),
        };
        f.finish()?;
        Ok(IngestUri { endpoint, name })
    }

    fn parse_srt_url(s: &str) -> Result<IngestUri, UriError> {
        let u = SimpleUrl::parse("url", s)?;
        if !u.path.is_empty() && u.path != "/" {
            return Err(invalid("url", "srt:// takes no path"));
        }
        let port = u.port.ok_or(UriError::MissingKey("port"))?;
        let mut f = Fields(parse_query(u.query.as_deref().unwrap_or(""))?);
        // 他の道具が付ける値のうち、意味の変わらないものだけ認める
        if let Some(m) = f.take("mode")
            && m != "caller"
        {
            return Err(invalid("mode", "only caller is supported"));
        }
        if let Some(t) = f.take("transtype")
            && t != "live"
        {
            return Err(invalid("transtype", "only live is supported"));
        }
        let ep = srt_from_fields(&mut f, u.host, port)?;
        f.finish()?;
        Ok(IngestUri {
            endpoint: ep,
            name: None,
        })
    }

    fn parse_rtmp_url(s: &str) -> Result<IngestUri, UriError> {
        let u = SimpleUrl::parse("url", s)?;
        if u.query.is_some() {
            return Err(invalid(
                "url",
                "query not allowed in rtmp URL with key; use senca: form",
            ));
        }
        let path = u.path.trim_end_matches('/');
        let (app, key) = path.rsplit_once('/').ok_or(UriError::MissingKey("key"))?;
        if app.trim_matches('/').is_empty() {
            return Err(UriError::MissingKey("key"));
        }
        let key = decode("key", key)?;
        printable("key", &key, 512)?;
        let base = SimpleUrl {
            path: app.to_owned(),
            ..u
        };
        Ok(IngestUri {
            endpoint: Endpoint::Rtmp {
                url: base.to_url(),
                key: Secret(key),
            },
            name: None,
        })
    }

    /// `senca://ingest?v=1&...` の形に書く（鍵の順番は固定）。
    pub fn to_senca_uri(&self) -> String {
        let mut s = String::from(SCHEME);
        s.push_str("v=1&proto=");
        s.push_str(self.endpoint.proto());
        let mut kv = |k: &str, v: &str| {
            s.push('&');
            s.push_str(k);
            s.push('=');
            encode(v, &mut s);
        };
        match &self.endpoint {
            Endpoint::Srt {
                host,
                port,
                stream_id,
                passphrase,
                latency_ms,
                pbkeylen,
            } => {
                kv("host", host);
                kv("port", &port.to_string());
                if let Some(v) = stream_id {
                    kv("streamid", &v.0);
                }
                if let Some(v) = passphrase {
                    kv("passphrase", &v.0);
                }
                if let Some(v) = latency_ms {
                    kv("latency", &v.to_string());
                }
                if let Some(v) = pbkeylen {
                    kv("pbkeylen", &v.to_string());
                }
            }
            Endpoint::Rtmp { url, key } => {
                kv("url", url);
                kv("key", &key.0);
            }
            Endpoint::Whip { url, token } => {
                kv("url", url);
                if let Some(t) = token {
                    kv("token", &t.0);
                }
            }
        }
        if let Some(n) = &self.name {
            kv("name", n);
        }
        s
    }

    /// 標準の URL に書く（他社の道具に渡す用）。WHIP のトークンは URL に入らないので落ちる。
    pub fn to_standard_url(&self) -> String {
        match &self.endpoint {
            Endpoint::Srt {
                host,
                port,
                stream_id,
                passphrase,
                latency_ms,
                pbkeylen,
            } => {
                let mut s = format!("srt://{}:{port}", host_for_url(host));
                let mut sep = '?';
                let mut kv = |k: &str, v: &str| {
                    s.push(sep);
                    sep = '&';
                    s.push_str(k);
                    s.push('=');
                    encode(v, &mut s);
                };
                if let Some(v) = stream_id {
                    kv("streamid", &v.0);
                }
                if let Some(v) = passphrase {
                    kv("passphrase", &v.0);
                }
                if let Some(v) = latency_ms {
                    kv("latency", &v.to_string());
                }
                if let Some(v) = pbkeylen {
                    kv("pbkeylen", &v.to_string());
                }
                s
            }
            Endpoint::Rtmp { url, key } => {
                let mut s = url.trim_end_matches('/').to_owned();
                s.push('/');
                encode(&key.0, &mut s);
                s
            }
            Endpoint::Whip { url, .. } => url.clone(),
        }
    }
}

impl std::str::FromStr for IngestUri {
    type Err = UriError;
    fn from_str(s: &str) -> Result<Self, UriError> {
        IngestUri::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(s: &str) -> IngestUri {
        let u = IngestUri::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        let again = IngestUri::parse(&u.to_senca_uri()).unwrap();
        assert_eq!(u, again, "senca round-trip of {s}");
        u
    }

    #[test]
    fn srt_senca_round_trip() {
        let s = "senca://ingest?v=1&proto=srt&host=media.example.com&port=9000&streamid=publish:sk_abc&passphrase=0123456789abcdef&latency=200&pbkeylen=32";
        let u = rt(s);
        assert_eq!(u.to_senca_uri(), s);
        match &u.endpoint {
            Endpoint::Srt {
                host,
                port,
                stream_id,
                latency_ms,
                pbkeylen,
                ..
            } => {
                assert_eq!((host.as_str(), *port), ("media.example.com", 9000));
                assert_eq!(stream_id.as_ref().unwrap().expose(), "publish:sk_abc");
                assert_eq!((*latency_ms, *pbkeylen), (Some(200), Some(32)));
            }
            _ => panic!(),
        }
        assert_eq!(
            u.to_standard_url(),
            "srt://media.example.com:9000?streamid=publish:sk_abc&passphrase=0123456789abcdef&latency=200&pbkeylen=32"
        );
        assert_eq!(IngestUri::parse(&u.to_standard_url()).unwrap(), u);
    }

    #[test]
    fn rtmp_and_whip_round_trip() {
        let u = rt(
            "senca://ingest?v=1&proto=rtmp&url=rtmps://h.example:443/live&key=sk_x%26y&name=%E3%82%AB%E3%83%A1%E3%83%A91",
        );
        assert_eq!(u.name.as_deref(), Some("カメラ1"));
        assert_eq!(u.to_standard_url(), "rtmps://h.example:443/live/sk_x%26y");
        let back = IngestUri::parse(&u.to_standard_url()).unwrap();
        assert_eq!(back.endpoint, u.endpoint);
        let w = rt("senca://ingest?v=1&proto=whip&url=https://h/whip/abc&token=eyJ.abc.def");
        assert_eq!(w.to_standard_url(), "https://h/whip/abc");
        rt("senca://ingest?v=1&proto=whip&url=http://127.0.0.1:8080/whip/x");
        rt("senca://ingest?v=1&proto=srt&host=::1&port=9000");
        let v6 = IngestUri::parse("srt://[::1]:9000").unwrap();
        assert_eq!(v6.to_standard_url(), "srt://[::1]:9000");
    }

    #[test]
    fn standard_urls() {
        let u = IngestUri::parse("rtmp://h/live/key1").unwrap();
        assert_eq!(
            u.endpoint,
            Endpoint::Rtmp {
                url: "rtmp://h/live".into(),
                key: Secret::new("key1")
            }
        );
        let u = IngestUri::parse("srt://10.0.0.1:9000?mode=caller&streamid=publish:k").unwrap();
        assert!(matches!(u.endpoint, Endpoint::Srt { port: 9000, .. }));
        assert!(IngestUri::parse("srt://h:9000?mode=listener").is_err());
        // app だけで key が無い
        assert_eq!(
            IngestUri::parse("rtmp://h/live"),
            Err(UriError::MissingKey("key"))
        );
    }

    #[test]
    fn strict_rejections() {
        let bad = [
            (
                "senca://ingest?proto=srt&host=h&port=1",
                UriError::MissingKey("v"),
            ),
            (
                "senca://ingest?v=2&proto=srt&host=h&port=1",
                UriError::Version,
            ),
            (
                "senca://ingest?v=1&v=1&proto=srt&host=h&port=1",
                UriError::DuplicateKey("v".into()),
            ),
            (
                "senca://ingest?v=1&proto=srt&host=h&port=1&bogus=1",
                UriError::UnknownKey("bogus".into()),
            ),
            (
                "senca://ingest?v=1&proto=srt&host=h&port=1&key=x",
                UriError::UnknownKey("key".into()),
            ),
            (
                "senca://ingest?v=1&proto=srt&host=h&port=0",
                invalid("port", "port out of range"),
            ),
            (
                "senca://ingest?v=1&proto=srt&host=h&port=65536",
                invalid("port", "port out of range"),
            ),
            (
                "senca://ingest?v=1&proto=srt&host=h_x&port=1",
                invalid("host", "bad host"),
            ),
            (
                "senca://ingest?v=1&proto=srt&host=h&port=1&passphrase=short",
                invalid("passphrase", "must be 10..79 bytes"),
            ),
            (
                "senca://ingest?v=1&proto=srt&host=h&port=1&pbkeylen=32",
                invalid("pbkeylen", "requires passphrase"),
            ),
            (
                "senca://ingest?v=1&proto=srt&host=h&port=1&latency=5",
                invalid("latency", "must be 20..8000 (ms)"),
            ),
            (
                "senca://ingest?v=1&proto=srt&host=h&port=1&streamid=",
                invalid("streamid", "empty"),
            ),
            (
                "senca://ingest?v=1&proto=srt&host=h&port=1&streamid=%0a",
                UriError::Encoding("streamid".into()),
            ),
            (
                "senca://ingest?v=1&proto=srt&host=h&port=1&streamid=%zz",
                UriError::Encoding("streamid".into()),
            ),
            (
                "senca://ingest?v=1&proto=rtmp&url=http://h/live&key=k",
                invalid("url", "must be rtmp:// or rtmps://"),
            ),
            (
                "senca://ingest?v=1&proto=rtmp&url=rtmp://u:p@h/live&key=k",
                invalid("url", "credentials in URL are not allowed"),
            ),
            (
                "senca://ingest?v=1&proto=rtmp&url=rtmp://h/&key=k",
                invalid("url", "missing application name"),
            ),
            (
                "senca://ingest?v=1&proto=rtmp&url=rtmp://h/live&key=a%20b",
                invalid("key", "must be printable ASCII without spaces"),
            ),
            (
                "senca://ingest?v=1&proto=whip&url=http://h/whip/x",
                invalid("url", "WHIP must use https:// (http:// only for loopback)"),
            ),
            (
                "senca://ingest?v=1&proto=whip&url=https://h/whip/x#t",
                UriError::Malformed("fragment not allowed"),
            ),
            (
                "senca://ingest?v=1&proto=ftp",
                invalid("proto", "must be srt, rtmp or whip"),
            ),
            ("ftp://h/x", UriError::Scheme),
        ];
        for (s, want) in bad {
            assert_eq!(IngestUri::parse(s), Err(want), "{s}");
        }
        // 拡張用の x- 鍵は読み飛ばす
        assert!(IngestUri::parse("senca://ingest?v=1&proto=srt&host=h&port=1&x-future=1").is_ok());
        assert_eq!(IngestUri::parse(&"x".repeat(3000)), Err(UriError::TooLong));
    }

    #[test]
    fn secrets_not_in_debug_or_errors() {
        let u = IngestUri::parse("senca://ingest?v=1&proto=srt&host=h&port=1&streamid=publish:sk_SECRET&passphrase=SECRETSECRET").unwrap();
        let d = format!("{u:?}");
        assert!(!d.contains("SECRET"), "{d}");
        let e =
            IngestUri::parse("senca://ingest?v=1&proto=rtmp&url=rtmp://h/live&key=sk_SECRET%20x")
                .unwrap_err();
        assert!(!e.to_string().contains("SECRET"));
    }
}
