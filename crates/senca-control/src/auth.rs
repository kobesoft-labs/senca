//! 認証と権限（仕様 9.2）。
//!
//! `Authorization: Bearer` の値が `senca_` で始まれば API トークン、`sencas_` で始まればログインのセッション
//! （ADR-0017）、それ以外は JWT。
//! 失敗の理由は応答に出さない（呼び出し側がログに残す）。

use crate::jwt::{JwtError, JwtVerifier};
use crate::secret;
use crate::state::ControlState;
use crate::users::{Pending, SESSION_PREFIX};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const TOKEN_PREFIX: &str = "senca_";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Scope {
    #[serde(rename = "stream:read")]
    StreamRead,
    #[serde(rename = "stream:write")]
    StreamWrite,
    #[serde(rename = "publish")]
    Publish,
    #[serde(rename = "subscribe")]
    Subscribe,
    #[serde(rename = "recording:read")]
    RecordingRead,
    #[serde(rename = "recording:delete")]
    RecordingDelete,
    #[serde(rename = "tenant:admin")]
    TenantAdmin,
    /// テナントの外の運用権限（ノード・デバイス）。
    #[serde(rename = "node:admin")]
    NodeAdmin,
}

impl Scope {
    pub const ALL: [Scope; 8] = [
        Scope::StreamRead,
        Scope::StreamWrite,
        Scope::Publish,
        Scope::Subscribe,
        Scope::RecordingRead,
        Scope::RecordingDelete,
        Scope::TenantAdmin,
        Scope::NodeAdmin,
    ];

    pub fn parse(s: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|x| x.as_str() == s)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::StreamRead => "stream:read",
            Scope::StreamWrite => "stream:write",
            Scope::Publish => "publish",
            Scope::Subscribe => "subscribe",
            Scope::RecordingRead => "recording:read",
            Scope::RecordingDelete => "recording:delete",
            Scope::TenantAdmin => "tenant:admin",
            Scope::NodeAdmin => "node:admin",
        }
    }
}

/// 認証済みの主体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub tenant: String,
    pub subject: String,
    pub scopes: BTreeSet<Scope>,
    /// API トークンのとき、その ID。
    pub token_id: Option<String>,
    /// ログインのセッションのとき。
    pub session: Option<SessionRef>,
}

/// 主体がログインのセッションのときの情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    pub id: String,
    pub user: String,
    /// 値の SHA-256（最後に使った時刻を進めるコマンドに使う）。
    pub hash: String,
    pub last_seen_ms: i64,
    pub idle_timeout_ms: i64,
    /// 先に済ませること（空でなければ、ほかの API は使えない）。
    pub pending: Vec<Pending>,
}

impl Principal {
    /// `tenant:admin` は `node:admin` 以外のすべてを含む。
    pub fn has(&self, s: Scope) -> bool {
        self.scopes.contains(&s) || (s != Scope::NodeAdmin && self.scopes.contains(&Scope::TenantAdmin))
    }

    /// 実際に持っている権限の全体（tenant:admin を展開）。
    pub fn effective(&self) -> BTreeSet<Scope> {
        Scope::ALL.into_iter().filter(|s| self.has(*s)).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    Missing,
    Malformed,
    UnknownToken,
    Expired,
    /// セッションが無い・期限切れ・利用者が無効。
    Session,
    Jwt(JwtError),
    JwtDisabled,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Missing => f.write_str("missing credentials"),
            AuthError::Malformed => f.write_str("malformed authorization header"),
            AuthError::UnknownToken => f.write_str("unknown api token"),
            AuthError::Expired => f.write_str("api token expired"),
            AuthError::Session => f.write_str("unknown or expired session"),
            AuthError::Jwt(e) => write!(f, "jwt: {e}"),
            AuthError::JwtDisabled => f.write_str("jwt not configured"),
        }
    }
}

/// `Authorization` ヘッダーの値から主体を決める。`require_mfa` は `--auth-require-mfa`（セッションの制限に使う）。
pub fn authenticate(
    header: Option<&str>,
    state: &ControlState,
    jwt: Option<&JwtVerifier>,
    now_ms: i64,
    require_mfa: bool,
) -> Result<Principal, AuthError> {
    let h = header.ok_or(AuthError::Missing)?;
    let (scheme, value) = h.split_once(' ').ok_or(AuthError::Malformed)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(AuthError::Malformed);
    }
    let value = value.trim_matches(' ');
    if value.is_empty() || value.len() > 8192 || value.contains(' ') {
        return Err(AuthError::Malformed);
    }
    if value.starts_with(SESSION_PREFIX) {
        // 利用者の権限はその時点のもの（役割の変更はすぐ効く）
        let s = state.session_by_value(value).ok_or(AuthError::Session)?;
        if s.expired(now_ms) {
            return Err(AuthError::Session);
        }
        let u = state.users.get(&s.user).filter(|u| u.tenant == s.tenant && !u.disabled).ok_or(AuthError::Session)?;
        return Ok(Principal {
            tenant: s.tenant.clone(),
            subject: format!("user:{}", u.id),
            scopes: u.granted(),
            token_id: None,
            session: Some(SessionRef {
                id: s.id.clone(),
                user: u.id.clone(),
                hash: s.hash.clone(),
                last_seen_ms: s.last_seen_ms,
                idle_timeout_ms: s.idle_timeout_ms,
                pending: crate::users::pending(u, s, require_mfa),
            }),
        });
    }
    if value.starts_with(TOKEN_PREFIX) {
        // 全件を定数時間で比べる（一致したかどうかで時間を変えない）。
        let mut found = None;
        for t in state.tokens.values() {
            if secret::verify(value, &t.hash) && found.is_none() {
                found = Some(t);
            }
        }
        let t = found.ok_or(AuthError::UnknownToken)?;
        if let Some(exp) = t.expires_at_ms
            && now_ms >= exp
        {
            return Err(AuthError::Expired);
        }
        return Ok(Principal {
            tenant: t.tenant.clone(),
            subject: format!("token:{}", t.id),
            scopes: t.scopes.clone(),
            token_id: Some(t.id.clone()),
            session: None,
        });
    }
    let v = jwt.ok_or(AuthError::JwtDisabled)?;
    let claims = v.verify(value, now_ms / 1000).map_err(AuthError::Jwt)?;
    Ok(Principal {
        tenant: claims.tenant,
        subject: claims.subject,
        scopes: claims.scopes,
        token_id: None,
        session: None,
    })
}
