//! クライアントの本体: 認証・Idempotency-Key・If-Match・送り直し・応答の読み取り。

use bytes::Bytes;
use senca_http::Method;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::error::{ApiError, Error, Result};
use crate::transport::{self, RawRequest, RawResponse, TlsRoots, Url};

/// SDK の名前と版（`User-Agent`）。
pub const USER_AGENT: &str = concat!("senca-client-rust/", env!("CARGO_PKG_VERSION"));

/// 送り直しの方針（指数バックオフ ± 20 % のゆらぎ。`Retry-After` があればそれ以上待つ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// 最初の要求の後に送り直す回数の上限（0 で送り直さない）。
    pub max_retries: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_retries: 3,
            initial_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(10),
        }
    }
}

impl RetryPolicy {
    /// 送り直さない。
    pub const NONE: RetryPolicy = RetryPolicy {
        max_retries: 0,
        initial_delay: Duration::from_millis(0),
        max_delay: Duration::from_millis(0),
    };

    /// `attempt` 回目（0 始まり）の送り直しの前に待つ時間（ゆらぎの前）。
    pub fn base_delay(&self, attempt: u32) -> Duration {
        let d = self.initial_delay.saturating_mul(1u32.checked_shl(attempt.min(20)).unwrap_or(u32::MAX));
        d.min(self.max_delay)
    }

    /// ゆらぎと `Retry-After` を入れた待ち時間。
    pub fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let base = self.base_delay(attempt);
        let jitter = random_unit() * 0.4 - 0.2; // -20 % 〜 +20 %
        let d = base.mul_f64((1.0 + jitter).max(0.0));
        match retry_after {
            Some(r) => d.max(r),
            None => d,
        }
    }
}

fn random_unit() -> f64 {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        return 0.5;
    }
    (u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64
}

/// 新しい Idempotency-Key（ランダムな 128 ビットの 16 進）。
pub fn new_idempotency_key() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    let mut s = String::with_capacity(36);
    s.push_str("sdk-");
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// クライアントの組み立て。
#[derive(Clone)]
pub struct ClientBuilder {
    base: String,
    token: Option<String>,
    timeout: Duration,
    retry: RetryPolicy,
    user_agent: String,
    roots: TlsRoots,
}

impl ClientBuilder {
    /// 要求ごとの時間切れ（接続から本文を読み終えるまで。SSE は応答の先頭まで）。既定 30 秒。
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    pub fn retry(mut self, p: RetryPolicy) -> Self {
        self.retry = p;
        self
    }

    pub fn token(mut self, t: impl Into<String>) -> Self {
        self.token = Some(t.into());
        self
    }

    /// `User-Agent` の前に付ける（`myapp/1.0 senca-client-rust/0.0.1`）。
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = format!("{} {USER_AGENT}", ua.into());
        self
    }

    /// https で信頼する認証局を PEM で与える（既定は OS の認証局）。
    pub fn root_certificates_pem(mut self, pem: impl Into<Vec<u8>>) -> Self {
        self.roots = TlsRoots::Pem(Arc::new(pem.into()));
        self
    }

    pub fn build(self) -> Result<SencaClient> {
        let mut base = self.base.trim_end_matches('/').to_string();
        // `https://host` だけなら `/api/v1` を足す
        let u = Url::parse(&base)?;
        if u.path == "/" {
            base.push_str("/api/v1");
        }
        let u = Url::parse(&base)?;
        if u.path.contains('?') {
            return Err(Error::InvalidUrl(base));
        }
        Ok(SencaClient {
            inner: Arc::new(Inner {
                origin: u.origin(),
                api_path: u.path.trim_end_matches('/').to_string(),
                token: self.token,
                timeout: self.timeout,
                retry: self.retry,
                user_agent: self.user_agent,
                roots: self.roots,
                tls: OnceLock::new(),
            }),
        })
    }
}

pub(crate) struct Inner {
    origin: String,
    /// `/api/v1`。
    api_path: String,
    token: Option<String>,
    pub(crate) timeout: Duration,
    pub(crate) retry: RetryPolicy,
    user_agent: String,
    roots: TlsRoots,
    tls: OnceLock<std::result::Result<Arc<senca_tls::ClientConfig>, String>>,
}

/// SENCA の管理 API（`/api/v1`）のクライアント。複製は安い（中身は共有）。
///
/// ```no_run
/// # async fn f() -> senca_client::Result<()> {
/// use senca_client::{SencaClient, models::StreamCreate};
/// let client = SencaClient::new("https://senca.example", "senca_...")?;
/// let s = client.streams().create(&StreamCreate { name: "studio-a".into(), ..Default::default() }).await?;
/// println!("{} {:?}", s.id, s.etag);
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct SencaClient {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for SencaClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SencaClient").field("base", &format!("{}{}", self.inner.origin, self.inner.api_path)).finish()
    }
}

/// 版（ETag）つきの応答。`Deref` で中身に触れる。
#[derive(Debug, Clone, PartialEq)]
pub struct Versioned<T> {
    pub value: T,
    /// `ETag`（`"{generation}"`）。Idempotency の再送の応答には無い。
    pub etag: Option<String>,
    /// Idempotency-Key の再送で、覚えていた答えを返した（`Idempotency-Replayed: true`）。
    pub replayed: bool,
    /// `X-Request-Id`。
    pub request_id: Option<String>,
}

impl<T> Versioned<T> {
    pub fn into_inner(self) -> T {
        self.value
    }

    /// ETag の generation（`"3"` → 3）。
    pub fn generation(&self) -> Option<u64> {
        self.etag.as_deref().and_then(|e| e.trim_start_matches("W/").trim_matches('"').parse().ok())
    }

    /// この版を前提にした `If-Match`（ETag が無ければ `None`）。
    pub fn if_match(&self) -> Option<IfMatch> {
        self.etag.clone().map(IfMatch::Etag)
    }
}

impl<T> std::ops::Deref for Versioned<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

/// `If-Match` の値（楽観的排他）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IfMatch {
    /// 応答の `ETag` そのもの。
    Etag(String),
    /// 資源の `generation`（`"{n}"` にして送る）。
    Generation(u64),
}

impl IfMatch {
    pub fn header_value(&self) -> String {
        match self {
            IfMatch::Etag(e) => e.clone(),
            IfMatch::Generation(g) => format!("\"{g}\""),
        }
    }
}

impl From<u64> for IfMatch {
    fn from(g: u64) -> Self {
        IfMatch::Generation(g)
    }
}

impl From<&str> for IfMatch {
    fn from(e: &str) -> Self {
        IfMatch::Etag(e.to_string())
    }
}

impl From<String> for IfMatch {
    fn from(e: String) -> Self {
        IfMatch::Etag(e)
    }
}

impl<T> From<&Versioned<T>> for IfMatch {
    /// 応答の ETag（無ければ空の ETag になり、サーバーが 412 を返す）。
    fn from(v: &Versioned<T>) -> Self {
        IfMatch::Etag(v.etag.clone().unwrap_or_default())
    }
}

/// 要求ごとの指定。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestOptions {
    /// POST の Idempotency-Key（省けば SDK が作る。同じ鍵で送ると同じ答えが返る）。
    pub idempotency_key: Option<String>,
    /// 送り直しの方針をこの要求だけ変える。
    pub retry: Option<RetryPolicy>,
}

impl RequestOptions {
    pub fn idempotency_key(key: impl Into<String>) -> Self {
        RequestOptions { idempotency_key: Some(key.into()), retry: None }
    }
}

/// 組み立て中の要求（内部）。
pub(crate) struct Call {
    pub method: Method,
    /// `/api/v1` からの相対パス（`/streams/abc`）か、`/` で始まらなければ絶対 URL。
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Option<Bytes>,
    pub if_match: Option<IfMatch>,
    pub opts: RequestOptions,
    pub accept: &'static str,
    /// API の外（`/healthz` など。`/api/v1` を前に付けない・認証しない）。
    pub outside_api: bool,
    pub max_body: usize,
    /// 追加のヘッダー（`POST /auth/exchange` の Cookie など）。
    pub headers: Vec<(String, String)>,
}

impl Call {
    pub fn new(method: Method, path: impl Into<String>) -> Call {
        Call {
            method,
            path: path.into(),
            query: Vec::new(),
            body: None,
            if_match: None,
            opts: RequestOptions::default(),
            accept: "application/json",
            outside_api: false,
            max_body: 16 * 1024 * 1024,
            headers: Vec::new(),
        }
    }

    pub fn json<B: Serialize + ?Sized>(mut self, b: &B) -> Result<Call> {
        self.body = Some(Bytes::from(serde_json::to_vec(b).map_err(|e| Error::InvalidArgument(e.to_string()))?));
        Ok(self)
    }

    pub fn query(mut self, k: &str, v: impl ToString) -> Call {
        self.query.push((k.to_string(), v.to_string()));
        self
    }

    pub fn opts(mut self, o: RequestOptions) -> Call {
        self.opts = o;
        self
    }

    pub fn if_match(mut self, m: Option<IfMatch>) -> Call {
        self.if_match = m;
        self
    }
}

/// パスの 1 区切り（ID など）を百分率符号化する。
pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b':' | b'@') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl SencaClient {
    /// `base_url` は `https://host[:port]`（`/api/v1` は足す）か `https://host/api/v1`。`token` は API トークン・JWT・
    /// ログインのセッションの値（`sencas_...`）。ログインだけをするなら [`SencaClient::builder`] でトークンなしに作る。
    pub fn new(base_url: &str, token: impl Into<String>) -> Result<SencaClient> {
        SencaClient::builder(base_url).token(token).build()
    }

    pub fn builder(base_url: &str) -> ClientBuilder {
        ClientBuilder {
            base: base_url.to_string(),
            token: None,
            timeout: Duration::from_secs(30),
            retry: RetryPolicy::default(),
            user_agent: USER_AGENT.to_string(),
            roots: TlsRoots::System,
        }
    }

    /// 同じ宛先・設定で、認証の値だけを替えたクライアント（ログインで得たセッションの値 `sencas_...` を使うとき）。
    pub fn with_token(&self, token: impl Into<String>) -> SencaClient {
        let i = &self.inner;
        SencaClient {
            inner: Arc::new(Inner {
                origin: i.origin.clone(),
                api_path: i.api_path.clone(),
                token: Some(token.into()),
                timeout: i.timeout,
                retry: i.retry,
                user_agent: i.user_agent.clone(),
                roots: i.roots.clone(),
                tls: OnceLock::new(),
            }),
        }
    }

    /// API の base（`https://host/api/v1`）。
    pub fn base_url(&self) -> String {
        format!("{}{}", self.inner.origin, self.inner.api_path)
    }

    pub(crate) fn url_for(&self, call: &Call) -> Result<Url> {
        let mut s = if call.path.starts_with('/') {
            if call.outside_api {
                format!("{}{}", self.inner.origin, call.path)
            } else {
                format!("{}{}{}", self.inner.origin, self.inner.api_path, call.path)
            }
        } else {
            call.path.clone()
        };
        if !call.query.is_empty() {
            s.push(if s.contains('?') { '&' } else { '?' });
            let q: Vec<String> = call.query.iter().map(|(k, v)| format!("{}={}", encode_query(k), encode_query(v))).collect();
            s.push_str(&q.join("&"));
        }
        Url::parse(&s)
    }

    fn tls(&self, url: &Url) -> Result<Option<Arc<senca_tls::ClientConfig>>> {
        if !url.tls {
            return Ok(None);
        }
        self.inner
            .tls
            .get_or_init(|| transport::tls_config(&self.inner.roots).map_err(|e| e.to_string()))
            .clone()
            .map(Some)
            .map_err(Error::Transport)
    }

    pub(crate) fn raw_request(&self, call: &Call, idem: Option<&str>, extra: &[(String, String)]) -> Result<RawRequest> {
        let url = self.url_for(call)?;
        let mut headers: Vec<(String, String)> = vec![
            ("User-Agent".into(), self.inner.user_agent.clone()),
            ("Accept".into(), call.accept.into()),
        ];
        let same_origin = url.origin() == self.inner.origin;
        if !call.outside_api
            && same_origin
            && let Some(t) = &self.inner.token
        {
            headers.push(("Authorization".into(), format!("Bearer {t}")));
        }
        if call.body.is_some() {
            headers.push(("Content-Type".into(), "application/json".into()));
        }
        if let Some(k) = idem {
            headers.push(("Idempotency-Key".into(), k.to_string()));
        }
        if let Some(m) = &call.if_match {
            headers.push(("If-Match".into(), m.header_value()));
        }
        headers.extend(extra.iter().cloned());
        Ok(RawRequest { method: call.method.clone(), url, headers, body: call.body.clone() })
    }

    /// 送り、応答の先頭を返す（送り直しなし。SSE 用）。
    pub(crate) async fn open(&self, call: &Call, extra: &[(String, String)], head_timeout: Duration) -> Result<RawResponse> {
        let req = self.raw_request(call, None, extra)?;
        let tls = self.tls(&req.url)?;
        transport::send(req, tls, head_timeout, u64::MAX).await
    }

    /// 送って本文まで読む。2xx でなければ [`ApiError`]。冪等な要求は方針に従って送り直す。
    pub(crate) async fn execute(&self, mut call: Call) -> Result<(u16, Vec<(String, String)>, Bytes)> {
        let policy = call.opts.retry.unwrap_or(self.inner.retry);
        // POST は Idempotency-Key を付ける（送り直しても 1 回だけ効く）
        let idem = if call.method == Method::Post && !call.outside_api {
            Some(call.opts.idempotency_key.take().unwrap_or_else(new_idempotency_key))
        } else {
            None
        };
        let idempotent = matches!(call.method, Method::Get | Method::Head | Method::Put | Method::Delete) || idem.is_some();
        let mut attempt = 0u32;
        loop {
            let r = self.execute_once(&call, idem.as_deref()).await;
            let err = match r {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            let may_retry = attempt < policy.max_retries
                && match &err {
                    // 届いていないので、どの要求も送り直してよい
                    Error::Connect(_) => true,
                    Error::Timeout | Error::Transport(_) => idempotent,
                    // サーバーが「効いていない」と言ったもの（PATCH も含む）
                    Error::Api(a) => a.is_retryable() && (idempotent || a.retryable || matches!(a.status, 429 | 503)),
                    _ => false,
                };
            if !may_retry {
                return Err(err);
            }
            let ra = err.api().and_then(|a| a.retry_after);
            tokio::time::sleep(policy.delay(attempt, ra)).await;
            attempt += 1;
        }
    }

    async fn execute_once(&self, call: &Call, idem: Option<&str>) -> Result<(u16, Vec<(String, String)>, Bytes)> {
        let req = self.raw_request(call, idem, &call.headers)?;
        let tls = self.tls(&req.url)?;
        let timeout = self.inner.timeout;
        let max = call.max_body;
        let fut = async move {
            let resp = transport::send(req, tls, timeout, max as u64).await?;
            let status = resp.status;
            let headers = resp.headers.clone();
            let body = resp.read_all(max).await?;
            Ok::<_, Error>((status, headers, body))
        };
        let (status, headers, body) = tokio::time::timeout(timeout, fut).await.map_err(|_| Error::Timeout)??;
        if !(200..300).contains(&status) {
            return Err(ApiError::from_response(status, &headers, &body).into());
        }
        Ok((status, headers, body))
    }

    /// JSON の応答を型に読む。
    pub(crate) async fn json<T: DeserializeOwned>(&self, call: Call) -> Result<Versioned<T>> {
        let (_, headers, body) = self.execute(call).await?;
        let value = decode(&body)?;
        Ok(versioned(value, &headers))
    }

    /// 本文の無い応答（204）。
    pub(crate) async fn empty(&self, call: Call) -> Result<()> {
        self.execute(call).await.map(|_| ())
    }

    /// 生のバイト（画像・MP4・署名付きの再生パス）。
    pub(crate) async fn bytes(&self, mut call: Call, accept: &'static str) -> Result<Bytes> {
        call.accept = accept;
        call.max_body = 4 * 1024 * 1024 * 1024;
        self.execute(call).await.map(|(_, _, b)| b)
    }
}

pub(crate) fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    serde_json::from_slice(body).map_err(|e| {
        let head = String::from_utf8_lossy(&body[..body.len().min(160)]).into_owned();
        Error::Decode(format!("{e} (本文: {head})"))
    })
}

pub(crate) fn versioned<T>(value: T, headers: &[(String, String)]) -> Versioned<T> {
    let h = |n: &str| transport::header(headers, n).map(str::to_string);
    Versioned {
        value,
        etag: h("etag"),
        replayed: h("idempotency-replayed").is_some_and(|v| v.eq_ignore_ascii_case("true")),
        request_id: h("x-request-id"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_urls() {
        let c = SencaClient::new("http://127.0.0.1:8080", "t").unwrap();
        assert_eq!(c.base_url(), "http://127.0.0.1:8080/api/v1");
        let call = Call::new(Method::Get, format!("/streams/{}", encode_segment("a b/c"))).query("limit", 10).query("cursor", "x&y");
        assert_eq!(c.url_for(&call).unwrap().path, "/api/v1/streams/a%20b%2Fc?limit=10&cursor=x%26y");
        let c2 = SencaClient::new("https://h.example/api/v1/", "t").unwrap();
        assert_eq!(c2.base_url(), "https://h.example/api/v1");
        let mut out = Call::new(Method::Get, "/healthz");
        out.outside_api = true;
        assert_eq!(c2.url_for(&out).unwrap().path, "/healthz");
        // 絶対 URL（署名付きの再生パス）。別の origin には Bearer を付けない
        let abs = Call::new(Method::Get, "http://media.example/s/1/abc/live/x/index.m3u8");
        let r = c.raw_request(&abs, None, &[]).unwrap();
        assert!(!r.headers.iter().any(|(k, _)| k == "Authorization"));
        let r = c.raw_request(&Call::new(Method::Get, "/streams"), None, &[]).unwrap();
        assert!(r.headers.iter().any(|(k, v)| k == "Authorization" && v == "Bearer t"));
        assert!(SencaClient::new("ftp://x", "t").is_err());
    }

    #[test]
    fn retry_delays() {
        let p = RetryPolicy { max_retries: 5, initial_delay: Duration::from_millis(100), max_delay: Duration::from_secs(1) };
        assert_eq!(p.base_delay(0), Duration::from_millis(100));
        assert_eq!(p.base_delay(2), Duration::from_millis(400));
        assert_eq!(p.base_delay(10), Duration::from_secs(1));
        for a in 0..4 {
            let d = p.delay(a, None);
            let b = p.base_delay(a);
            assert!(d >= b.mul_f64(0.79) && d <= b.mul_f64(1.21), "{d:?} {b:?}");
        }
        assert!(p.delay(0, Some(Duration::from_secs(3))) >= Duration::from_secs(3));
    }

    #[test]
    fn versioned_and_if_match() {
        let h = vec![("ETag".to_string(), "\"7\"".to_string()), ("Idempotency-Replayed".to_string(), "true".to_string())];
        let v = versioned(1u8, &h);
        assert_eq!((v.generation(), v.replayed), (Some(7), true));
        assert_eq!(IfMatch::from(&v).header_value(), "\"7\"");
        assert_eq!(IfMatch::from(3u64).header_value(), "\"3\"");
        let k = new_idempotency_key();
        assert_eq!(k.len(), 36);
        assert_ne!(k, new_idempotency_key());
    }
}
