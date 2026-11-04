//! 利用者の管理（`/users`、`tenant:admin`。ADR-0017）。
//!
//! - 渡せる権限は、自分が持っている権限まで（トークンと同じ）。`node:admin` は `node:admin` を持つ主体だけが渡せる。
//! - 自分より強い権限を持つ利用者（例: `node:admin` を持つ利用者）は、変更・削除・パスワードの再設定ができない（403）。
//! - テナントの最後の管理者は、削除・無効化・降格できない（409。状態の適用でも検査する）。

use super::{ApiResponse, ApiResult, Ctx, etag, new_id, paginate};
use crate::util::rfc3339;
use senca_control::auth::Scope;
use senca_control::state::Command;
use senca_control::users::{self, Role, Session, User, UserProfile};
use senca_control::{Problem, ProblemCode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub fn render_user(u: &User) -> Value {
    let mfa = u.mfa.as_ref();
    json!({
        "id": u.id,
        "username": u.username,
        "email": u.email,
        "display_name": u.display_name,
        "roles": u.roles.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
        "scopes": u.scopes,
        "effective_scopes": u.effective(),
        "disabled": u.disabled,
        "has_password": u.password_hash.is_some(),
        "must_change_password": u.must_change_password,
        "mfa": {
            "enabled": mfa.is_some(),
            "enrolled_at": mfa.map(|m| rfc3339(m.enrolled_at_ms)),
            "recovery_codes_remaining": mfa.map(|m| m.recovery.len()).unwrap_or(0),
        },
        "oidc": u.oidc.iter().map(|l| json!({ "provider": l.provider, "subject": l.subject })).collect::<Vec<_>>(),
        "generation": u.generation,
        "created_at": rfc3339(u.created_at_ms),
        "updated_at": rfc3339(u.updated_at_ms),
        "last_login_at": u.last_login_at_ms.map(rfc3339),
        "password_changed_at": u.password_changed_at_ms.map(rfc3339),
    })
}

pub fn render_session(s: &Session, current: Option<&str>) -> Value {
    json!({
        "id": s.id,
        "user": s.user,
        "method": s.method,
        "provider": s.provider,
        "mfa": s.mfa,
        "created_at": rfc3339(s.created_at_ms),
        "last_seen_at": rfc3339(s.last_seen_ms),
        "idle_expires_at": rfc3339(s.idle_expires_at_ms().min(s.expires_at_ms)),
        "expires_at": rfc3339(s.expires_at_ms),
        "ip": s.ip,
        "user_agent": s.user_agent,
        "current": current == Some(s.id.as_str()),
    })
}

/// 役割と権限の名前を読む。
pub fn parse_grants(roles: &[String], scopes: &[String]) -> Result<(BTreeSet<Role>, BTreeSet<Scope>), Problem> {
    let mut r = BTreeSet::new();
    for x in roles {
        r.insert(Role::parse(x).ok_or_else(|| Problem::new(ProblemCode::InvalidConfig, format!("知らない役割: {x}")))?);
    }
    let mut s = BTreeSet::new();
    for x in scopes {
        s.insert(Scope::parse(x).ok_or_else(|| Problem::new(ProblemCode::InvalidConfig, format!("知らない権限: {x}")))?);
    }
    Ok((r, s))
}

/// 自分が持っている権限しか渡せない。
fn can_grant(ctx: &Ctx, roles: &BTreeSet<Role>, scopes: &BTreeSet<Scope>) -> Result<(), Problem> {
    let mut all: BTreeSet<Scope> = scopes.clone();
    for r in roles {
        all.extend(r.scopes().iter().copied());
    }
    for s in all {
        if !ctx.principal.has(s) {
            return Err(Problem::new(ProblemCode::Forbidden, format!("持っていない権限は渡せません: {}", s.as_str())));
        }
    }
    Ok(())
}

/// 自分より強い利用者は扱えない。
fn can_manage(ctx: &Ctx, u: &User) -> Result<(), Problem> {
    for s in u.granted() {
        if !ctx.principal.has(s) {
            return Err(Problem::new(ProblemCode::Forbidden, format!("自分の持たない権限（{}）を持つ利用者は変更できません", s.as_str())));
        }
    }
    Ok(())
}

pub fn check_email(e: &str) -> Result<(), Problem> {
    let ok = (3..=254).contains(&e.len())
        && e.contains('@')
        && !e.starts_with('@')
        && !e.ends_with('@')
        && !e.chars().any(|c| c.is_whitespace() || c.is_control());
    if ok { Ok(()) } else { Err(Problem::new(ProblemCode::InvalidConfig, "email の形が違います")) }
}

pub fn check_display_name(d: &str) -> Result<(), Problem> {
    if d.is_empty() || d.chars().count() > 128 || d.chars().any(char::is_control) {
        return Err(Problem::new(ProblemCode::InvalidConfig, "display_name は 1〜128 文字（制御文字は不可）"));
    }
    Ok(())
}

fn policy(pw: &str, username: &str, email: Option<&str>) -> Result<(), Problem> {
    users::check_password_policy(pw, username, email).map_err(|e| Problem::new(ProblemCode::InvalidConfig, e))
}

/// 仮のパスワード（20 文字。紛らわしい文字を除いた英数字。約 113 bit）。
pub fn temporary_password() -> Result<String, Problem> {
    const A: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
    let mut out = String::new();
    while out.len() < 20 {
        let mut b = [0u8; 1];
        getrandom::fill(&mut b).map_err(|_| Problem::internal())?;
        // 偏りを出さないように A.len() の倍数の範囲だけ使う
        if (b[0] as usize) < 256 / A.len() * A.len() {
            out.push(A[b[0] as usize % A.len()] as char);
        }
    }
    Ok(out)
}

fn hash(ctx: &Ctx, pw: &str) -> Result<String, Problem> {
    ctx.app.auth.with_hashing(|| users::hash_password(pw))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserCreate {
    username: String,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    must_change_password: bool,
    #[serde(default)]
    disabled: bool,
}

fn user_response(status: u16, u: &User) -> ApiResponse {
    ApiResponse::json(status, &render_user(u)).with_header("ETag", etag(u.generation))
}

fn current(ctx: &Ctx, id: &str) -> Result<User, Problem> {
    ctx.control().state.tenant_user(ctx.tenant(), id).cloned()
}

pub fn create(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    let b: UserCreate = ctx.body()?;
    if !users::valid_username(&b.username) {
        return Err(Problem::new(ProblemCode::InvalidConfig, "username は 1〜64 文字の英数字と . _ @ + -"));
    }
    if let Some(e) = &b.email {
        check_email(e)?;
    }
    if let Some(d) = &b.display_name {
        check_display_name(d)?;
    }
    let (roles, scopes) = parse_grants(&b.roles, &b.scopes)?;
    if roles.is_empty() && scopes.is_empty() {
        return Err(Problem::new(ProblemCode::InvalidConfig, "roles か scopes を 1 つ以上指定してください"));
    }
    can_grant(ctx, &roles, &scopes)?;
    if let Some(pw) = &b.password {
        policy(pw, &b.username, b.email.as_deref())?;
    }
    if ctx.control().state.user_by_name(ctx.tenant(), &b.username).is_some() {
        return Err(Problem::new(ProblemCode::Conflict, "同じ利用者名がいます"));
    }
    let password_hash = match &b.password {
        Some(pw) => Some(hash(ctx, pw)?),
        None => None,
    };
    let u = User {
        id: new_id()?,
        tenant: ctx.tenant().to_string(),
        username: b.username,
        email: b.email,
        display_name: b.display_name,
        must_change_password: b.must_change_password && password_hash.is_some(),
        password_changed_at_ms: password_hash.as_ref().map(|_| ctx.now_ms),
        password_hash,
        roles,
        scopes,
        disabled: b.disabled,
        mfa: None,
        oidc: vec![],
        generation: 1,
        created_at_ms: ctx.now_ms,
        updated_at_ms: ctx.now_ms,
        last_login_at_ms: None,
    };
    ctx.app.propose(Command::CreateUser { user: u.clone() })?;
    ctx.app.audit_with(ctx, "user.create", &u.id, json!({ "roles": u.roles, "scopes": u.scopes }));
    Ok(user_response(201, &u).with_header("Location", format!("/api/v1/users/{}", u.id)))
}

pub fn list(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    let (limit, cursor) = ctx.page()?;
    let items: Vec<(String, User)> =
        ctx.control().state.users.values().filter(|u| u.tenant == ctx.tenant()).map(|u| (u.id.clone(), u.clone())).collect();
    let (page, next) = paginate(items, limit, cursor.as_deref());
    let items: Vec<Value> = page.iter().map(render_user).collect();
    Ok(ApiResponse::json(200, &json!({ "items": items, "next_cursor": next })))
}

pub fn get(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    Ok(user_response(200, &current(ctx, id)?))
}

/// PATCH の本文（`email`・`display_name` は null で消す）。
fn opt_string(v: &Value, name: &str) -> Result<Option<String>, Problem> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        _ => Err(Problem::invalid(format!("{name} は文字列か null"))),
    }
}

fn string_list(v: &Value, name: &str) -> Result<Vec<String>, Problem> {
    v.as_array()
        .and_then(|a| a.iter().map(|x| x.as_str().map(str::to_string)).collect::<Option<Vec<_>>>())
        .ok_or_else(|| Problem::invalid(format!("{name} は文字列の配列")))
}

pub fn patch(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    let expect = ctx.if_match(true)?;
    let body: Value = ctx.body()?;
    let obj = body.as_object().ok_or_else(|| Problem::invalid("本文はオブジェクト"))?;
    let cur = current(ctx, id)?;
    can_manage(ctx, &cur)?;
    let mut p = UserProfile {
        email: cur.email.clone(),
        display_name: cur.display_name.clone(),
        roles: cur.roles.clone(),
        scopes: cur.scopes.clone(),
        disabled: cur.disabled,
    };
    let (mut roles_s, mut scopes_s) = (None, None);
    for (k, v) in obj {
        match k.as_str() {
            "email" => {
                p.email = opt_string(v, k)?;
                if let Some(e) = &p.email {
                    check_email(e)?;
                }
            }
            "display_name" => {
                p.display_name = opt_string(v, k)?;
                if let Some(d) = &p.display_name {
                    check_display_name(d)?;
                }
            }
            "roles" => roles_s = Some(string_list(v, k)?),
            "scopes" => scopes_s = Some(string_list(v, k)?),
            "disabled" => p.disabled = v.as_bool().ok_or_else(|| Problem::invalid("disabled は真偽値"))?,
            _ => return Err(Problem::invalid(format!("知らないフィールド: {k}"))),
        }
    }
    if roles_s.is_some() || scopes_s.is_some() {
        let (r, s) = parse_grants(
            &roles_s.unwrap_or_else(|| cur.roles.iter().map(|r| r.as_str().to_string()).collect()),
            &scopes_s.unwrap_or_else(|| cur.scopes.iter().map(|s| s.as_str().to_string()).collect()),
        )?;
        if r.is_empty() && s.is_empty() {
            return Err(Problem::new(ProblemCode::InvalidConfig, "roles か scopes を 1 つ以上指定してください"));
        }
        can_grant(ctx, &r, &s)?;
        p.roles = r;
        p.scopes = s;
    }
    let changed_grants = p.roles != cur.roles || p.scopes != cur.scopes;
    let changed_disabled = p.disabled != cur.disabled;
    ctx.app.propose(Command::UpdateUser { tenant: ctx.tenant().to_string(), id: id.to_string(), profile: p, expect_generation: expect, now_ms: ctx.now_ms })?;
    let u = current(ctx, id)?;
    ctx.app.audit_with(
        ctx,
        "user.update",
        id,
        json!({ "roles_changed": changed_grants, "disabled": changed_disabled.then_some(u.disabled), "roles": u.roles, "scopes": u.scopes }),
    );
    Ok(user_response(200, &u))
}

pub fn delete(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    let expect = ctx.if_match(false)?;
    let cur = current(ctx, id)?;
    can_manage(ctx, &cur)?;
    if expect.is_some_and(|g| g != cur.generation) {
        return Err(Problem::new(ProblemCode::PreconditionFailed, "資源が更新されています（If-Match が古い）"));
    }
    ctx.app.propose(Command::DeleteUser { tenant: ctx.tenant().to_string(), id: id.to_string() })?;
    ctx.app.audit(ctx, "user.delete", id);
    Ok(ApiResponse::empty(204))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetPassword {
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    must_change_password: Option<bool>,
}

pub fn reset_password(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    let b: ResetPassword = ctx.body_or_default()?;
    let cur = current(ctx, id)?;
    can_manage(ctx, &cur)?;
    let (pw, generated) = match b.password {
        Some(p) => {
            policy(&p, &cur.username, cur.email.as_deref())?;
            (p, false)
        }
        None => (temporary_password()?, true),
    };
    let must = b.must_change_password.unwrap_or(true);
    let h = hash(ctx, &pw)?;
    ctx.app.propose(Command::SetUserPassword {
        tenant: ctx.tenant().to_string(),
        id: id.to_string(),
        password_hash: Some(h),
        must_change_password: must,
        keep_session: None,
        now_ms: ctx.now_ms,
    })?;
    ctx.app.audit_with(ctx, "user.password.reset", id, json!({ "must_change_password": must }));
    let u = current(ctx, id)?;
    Ok(ApiResponse::json(200, &json!({ "user": render_user(&u), "temporary_password": generated.then_some(pw) })).with_header("ETag", etag(u.generation)))
}

pub fn reset_mfa(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    let cur = current(ctx, id)?;
    can_manage(ctx, &cur)?;
    if cur.mfa.is_some() {
        ctx.app.propose(Command::SetUserMfa { tenant: ctx.tenant().to_string(), id: id.to_string(), mfa: None, now_ms: ctx.now_ms })?;
        ctx.app.audit(ctx, "user.mfa.reset", id);
    }
    Ok(ApiResponse::empty(204))
}

pub fn sessions(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    let (limit, cursor) = ctx.page()?;
    current(ctx, id)?;
    let items = user_sessions(ctx, id);
    let (page, next) = paginate(items, limit, cursor.as_deref());
    let cur = ctx.principal.session.as_ref().map(|s| s.id.as_str());
    let items: Vec<Value> = page.iter().map(|s| render_session(s, cur)).collect();
    Ok(ApiResponse::json(200, &json!({ "items": items, "next_cursor": next })))
}

/// 利用者の有効なセッション（ID 順）。
pub fn user_sessions(ctx: &Ctx, user: &str) -> Vec<(String, Session)> {
    let mut v: Vec<(String, Session)> = ctx
        .control()
        .state
        .sessions
        .values()
        .filter(|s| s.user == user && s.tenant == ctx.tenant() && !s.expired(ctx.now_ms))
        .map(|s| (s.id.clone(), s.clone()))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

pub fn revoke_sessions(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    let cur = current(ctx, id)?;
    can_manage(ctx, &cur)?;
    ctx.app.propose(Command::DeleteUserSessions { tenant: ctx.tenant().to_string(), user: id.to_string(), except: None })?;
    ctx.app.audit(ctx, "user.sessions.revoke", id);
    Ok(ApiResponse::empty(204))
}
