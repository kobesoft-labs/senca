//! OIDC の IdP との通信（ADR-0017）。ノードは relying party（認可コードフロー、PKCE S256・state・nonce）。
//!
//! - 取りに行くのは、設定した発行者の `/.well-known/openid-configuration` と、そこに書かれた `jwks_uri`・
//!   `token_endpoint` だけ（トークンの中の URL へは行かない）。発行者は管理者が設定した宛先なので、
//!   外向きの接続の制限（`--egress-allow`、仕様 9.4）は通さない（録画の保存先と同じ扱い）。発行者が https なら、
//!   discovery に書かれた宛先もすべて https でなければ使わない。
//! - discovery と JWKS は 1 時間覚える。知らない `kid` のトークンが来たら JWKS を取り直す（60 秒に 1 回まで）。
//! - 通信は 10 秒で打ち切る。応答は 1 MiB まで。

use crate::config::OidcProviderConfig;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use senca_control::jwt::JwtVerifier;
use senca_http::Method;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CACHE: Duration = Duration::from_secs(3600);
const REFRESH_MIN: Duration = Duration::from_secs(60);
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BODY: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct Metadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    /// `client_secret_basic` を使う（`token_endpoint_auth_methods_supported` が無い・含むとき）。無ければ `client_secret_post`。
    pub basic_auth: bool,
}

pub struct Provider {
    pub cfg: OidcProviderConfig,
    secret: Option<String>,
    pub redirect_uri: String,
    meta: Mutex<Option<(Instant, Metadata)>>,
    keys: Mutex<Option<(Instant, Arc<JwtVerifier>)>>,
    last_fetch: Mutex<Option<Instant>>,
}

/// フォームの値の符号化（application/x-www-form-urlencoded）。
pub fn form_encode(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

async fn get_json(url: &str) -> Result<Value, String> {
    let r = crate::httpc::request_trusted(Method::Get, url, &[("Accept", "application/json".into())], None, MAX_BODY, TIMEOUT).await?;
    if r.status != 200 {
        return Err(format!("{url}: HTTP {}", r.status));
    }
    serde_json::from_slice(&r.body).map_err(|e| format!("{url}: {e}"))
}

impl Provider {
    pub fn new(cfg: OidcProviderConfig, public_http: Option<&str>) -> Result<Provider, String> {
        let secret = cfg.client_secret()?;
        let redirect_uri = cfg.redirect(public_http).ok_or_else(|| format!("oidc {}: redirect_url が要ります", cfg.id))?;
        Ok(Provider { cfg, secret, redirect_uri, meta: Mutex::new(None), keys: Mutex::new(None), last_fetch: Mutex::new(None) })
    }

    pub fn display_name(&self) -> String {
        self.cfg.display_name.clone().unwrap_or_else(|| self.cfg.id.clone())
    }

    fn https_only(&self) -> bool {
        self.cfg.issuer.starts_with("https://")
    }

    /// discovery（覚えていればそれ）。
    pub async fn metadata(&self) -> Result<Metadata, String> {
        if let Some((at, m)) = self.meta.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
            && at.elapsed() < CACHE
        {
            return Ok(m.clone());
        }
        let url = format!("{}/.well-known/openid-configuration", self.cfg.issuer.trim_end_matches('/'));
        let d = get_json(&url).await?;
        let s = |k: &str| d.get(k).and_then(Value::as_str).map(str::to_string);
        let issuer = s("issuer").ok_or("discovery: issuer がありません")?;
        // 発行者は設定と同じでなければならない（OpenID Connect Discovery 4.3。末尾の / の違いだけは許す）
        if issuer.trim_end_matches('/') != self.cfg.issuer.trim_end_matches('/') {
            return Err(format!("discovery: issuer {issuer} が設定 {} と違います", self.cfg.issuer));
        }
        let ep = |k: &str| -> Result<String, String> {
            let u = s(k).ok_or_else(|| format!("discovery: {k} がありません"))?;
            if crate::httpc::parse_url(&u).is_none() || (self.https_only() && !u.starts_with("https://")) {
                return Err(format!("discovery: {k} は使えない URL です: {u}"));
            }
            Ok(u)
        };
        let methods: Vec<String> = d
            .get("token_endpoint_auth_methods_supported")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let m = Metadata {
            issuer,
            authorization_endpoint: ep("authorization_endpoint")?,
            token_endpoint: ep("token_endpoint")?,
            jwks_uri: ep("jwks_uri")?,
            basic_auth: methods.is_empty() || methods.iter().any(|x| x == "client_secret_basic"),
        };
        *self.meta.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), m.clone()));
        Ok(m)
    }

    /// 検証器（`kid` を知らなければ取り直す。60 秒に 1 回まで）。
    async fn verifier(&self, meta: &Metadata, kid: Option<&str>) -> Result<Arc<JwtVerifier>, String> {
        let cached = self.keys.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some((at, v)) = &cached {
            let known = kid.is_none_or(|k| v.has_kid(k));
            let may_refresh = self.last_fetch.lock().unwrap_or_else(|e| e.into_inner()).is_none_or(|t| t.elapsed() >= REFRESH_MIN);
            if at.elapsed() < CACHE && (known || !may_refresh) {
                return Ok(v.clone());
            }
        }
        *self.last_fetch.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        let jwks = get_json(&meta.jwks_uri).await?;
        let v = Arc::new(
            JwtVerifier::from_jwks_lenient(jwks.to_string().as_bytes(), &meta.issuer, &self.cfg.client_id).map_err(|e| format!("jwks: {e}"))?,
        );
        *self.keys.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v.clone()));
        Ok(v)
    }

    /// 認可の要求の URL。
    pub fn authorize_url(&self, meta: &Metadata, redirect_uri: &str, state: &str, nonce: &str, challenge: &str) -> String {
        let mut q = vec![
            ("response_type", "code".to_string()),
            ("client_id", self.cfg.client_id.clone()),
            ("redirect_uri", redirect_uri.to_string()),
            ("scope", self.cfg.scopes.join(" ")),
            ("state", state.to_string()),
            ("nonce", nonce.to_string()),
            ("code_challenge", challenge.to_string()),
            ("code_challenge_method", "S256".to_string()),
        ];
        if let Some(p) = &self.cfg.prompt {
            q.push(("prompt", p.clone()));
        }
        if let Some(m) = self.cfg.max_age {
            q.push(("max_age", m.to_string()));
        }
        let qs: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", form_encode(v))).collect();
        let sep = if meta.authorization_endpoint.contains('?') { '&' } else { '?' };
        format!("{}{sep}{}", meta.authorization_endpoint, qs.join("&"))
    }

    /// 認可コードを ID トークンに替える（トークンエンドポイント）。
    pub async fn exchange(&self, meta: &Metadata, code: &str, verifier: &str, redirect_uri: &str) -> Result<String, String> {
        let mut form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", redirect_uri.to_string()),
            ("code_verifier", verifier.to_string()),
        ];
        let mut headers: Vec<(&str, String)> =
            vec![("Content-Type", "application/x-www-form-urlencoded".into()), ("Accept", "application/json".into())];
        match &self.secret {
            Some(sec) if meta.basic_auth => {
                // RFC 6749 2.3.1: client_id と secret をフォームの形で符号化してから Basic にする
                let cred = format!("{}:{}", form_encode(&self.cfg.client_id), form_encode(sec));
                headers.push(("Authorization", format!("Basic {}", STANDARD.encode(cred))));
            }
            Some(sec) => {
                form.push(("client_id", self.cfg.client_id.clone()));
                form.push(("client_secret", sec.clone()));
            }
            None => form.push(("client_id", self.cfg.client_id.clone())),
        }
        let body: Vec<String> = form.iter().map(|(k, v)| format!("{k}={}", form_encode(v))).collect();
        let r = crate::httpc::request_trusted(Method::Post, &meta.token_endpoint, &headers, Some(bytes::Bytes::from(body.join("&"))), MAX_BODY, TIMEOUT)
            .await?;
        let v: Value = serde_json::from_slice(&r.body).map_err(|_| format!("token endpoint: HTTP {} (not JSON)", r.status))?;
        if r.status != 200 {
            // error と error_description だけを出す（トークンは出さない）
            let e = v.get("error").and_then(Value::as_str).unwrap_or("?");
            return Err(format!("token endpoint: HTTP {} error={}", r.status, e.chars().take(64).collect::<String>()));
        }
        v.get("id_token").and_then(Value::as_str).map(str::to_string).ok_or_else(|| "token endpoint: id_token がありません".to_string())
    }

    /// ID トークンを確かめる。
    pub async fn validate(&self, meta: &Metadata, id_token: &str, nonce: &str, now_ms: i64) -> Result<Value, String> {
        let kid = JwtVerifier::peek_kid(id_token);
        let v = self.verifier(meta, kid.as_deref()).await?;
        v.verify_id_token(id_token, now_ms / 1000, nonce, self.cfg.max_age).map_err(|e| format!("id_token: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_and_authorize_url() {
        assert_eq!(form_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        let cfg: OidcProviderConfig = serde_json::from_value(serde_json::json!({
            "id": "kc", "issuer": "https://idp.example.com/realms/x", "client_id": "senca", "prompt": "login", "max_age": 600
        }))
        .unwrap();
        let p = Provider::new(cfg, Some("https://senca.example.com")).unwrap();
        assert_eq!(p.redirect_uri, "https://senca.example.com/api/v1/auth/oidc/kc/callback");
        let m = Metadata {
            issuer: "https://idp.example.com/realms/x".into(),
            authorization_endpoint: "https://idp.example.com/auth?kc=1".into(),
            token_endpoint: "https://idp.example.com/token".into(),
            jwks_uri: "https://idp.example.com/certs".into(),
            basic_auth: true,
        };
        let u = p.authorize_url(&m, &p.redirect_uri, "st", "nn", "ch");
        assert!(u.starts_with("https://idp.example.com/auth?kc=1&response_type=code&client_id=senca&redirect_uri=https%3A%2F%2Fsenca"), "{u}");
        assert!(u.contains("&scope=openid%20email%20profile&state=st&nonce=nn&code_challenge=ch&code_challenge_method=S256&prompt=login&max_age=600"), "{u}");
    }
}
