//! ログインと本人の操作（`/auth/...`。ADR-0017）。
//!
//! 認証なしで使えるもの: `GET /auth/providers`・`POST /auth/login`・`POST /auth/login/mfa`・`POST /auth/exchange`・
//! `GET /auth/oidc/{provider}/start`・`GET /auth/oidc/{provider}/callback`。それ以外は `Authorization: Bearer` が要る。
//!
//! - 失敗の応答は、理由（利用者がいない・パスワードが違う・無効）を分けない（401、同じ本文）。利用者がいないときも
//!   同じだけハッシュの計算をする。失敗は送り元の IP ごと・アカウントごとに数え、使い切ると 429。
//! - OIDC の途中の値（state・nonce・PKCE の verifier・戻り先）は、封じたクッキー（`senca_oidc`、HttpOnly・SameSite=Lax・
//!   Path=/api/v1/auth、10 分）に入れる。どのノードでも開けるので、コールバックが別のノードに届いてもよい。
//!   コールバックは一度きりのコード（60 秒）を管理画面へ渡し、管理画面は同じブラウザーから `POST /auth/exchange`
//!   でセッションに替える（コードはクッキーの中の印と組。別のブラウザーへ渡されても使えない）。

use super::users::{render_session, render_user, user_sessions};
use super::{ApiResponse, ApiResult, Ctx, paginate};
use crate::auth::{self as na, NewSession};
use crate::util::rfc3339;
use senca_control::auth::Principal;
use senca_control::secret;
use senca_control::state::Command;
use senca_control::users::{self, LoginCode, LoginRecord, OidcLink, User, UserMfa, UserProfile};
use senca_control::{Problem, ProblemCode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use subtle::ConstantTimeEq;

const MFA_TOKEN_TTL_MS: i64 = 5 * 60_000;
const ENROLL_TTL_MS: i64 = 10 * 60_000;
const OIDC_FLOW_TTL_MS: i64 = 10 * 60_000;
const LOGIN_CODE_TTL_MS: i64 = 60_000;
const COOKIE: &str = "senca_oidc";

fn ip<'a>(ctx: &'a Ctx) -> &'a str {
    &ctx.req.peer_ip
}

fn sha(s: &str) -> String {
    secret::hash(s)
}

fn forbidden(t: &str) -> Problem {
    Problem::new(ProblemCode::Forbidden, t)
}

/// 主体の利用者（ログインのセッションでなければ 403）。
fn session_user(ctx: &Ctx) -> Result<(User, String), Problem> {
    let s = ctx.principal.session.as_ref().ok_or_else(|| forbidden("ログインのセッションでだけ使えます"))?;
    let u = ctx.control().state.tenant_user(ctx.tenant(), &s.user).cloned().map_err(|_| na::auth_failed())?;
    Ok((u, s.id.clone()))
}

fn pending_list(p: &Principal) -> Vec<&'static str> {
    p.session.as_ref().map(|s| s.pending.iter().map(|x| x.as_str()).collect()).unwrap_or_default()
}

// ---------------------------------------------------------------- 公開

pub fn providers(ctx: &Ctx) -> ApiResult {
    let setup_required = ctx.control().state.users.is_empty();
    let oidc: Vec<Value> = ctx
        .app
        .auth
        .oidc
        .iter()
        .map(|p| json!({ "id": p.cfg.id, "display_name": p.display_name(), "start_url": format!("/api/v1/auth/oidc/{}/start", p.cfg.id) }))
        .collect();
    Ok(ApiResponse::json(
        200,
        &json!({
            "password": ctx.app.cfg.auth_password_login,
            "mfa_required": ctx.app.cfg.auth_require_mfa,
            "setup_required": setup_required,
            "oidc": oidc,
        }),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginBody {
    username: String,
    password: String,
    #[serde(default)]
    tenant: Option<String>,
}

/// ログインの成功（セッションの値はこの応答でだけ返す）。
fn login_ok(ctx: &Ctx, ns: &NewSession, return_to: Option<String>) -> ApiResult {
    let p = na::principal_for(ctx.app, ns).ok_or_else(na::auth_failed)?;
    let u = ctx.control().state.tenant_user(&p.tenant, &ns.session.user).cloned()?;
    Ok(ApiResponse::json(
        200,
        &json!({
            "status": "ok",
            "token": ns.value.expose(),
            "session": render_session(&ns.session, Some(&ns.session.id)),
            "user": render_user(&u),
            "tenant": u.tenant,
            "scopes": p.effective(),
            "pending": pending_list(&p),
            "return_to": return_to,
        }),
    ))
}

fn audit_login(ctx: &Ctx, tenant: &str, user: Option<&str>, outcome: &str, method: &str, reason: Option<&str>) {
    let subject = user.map(|u| format!("user:{u}")).unwrap_or_else(|| "anonymous".into());
    ctx.app.audit_entry(
        ctx.now_ms,
        tenant,
        &subject,
        "auth.login",
        user.unwrap_or(""),
        &ctx.req.request_id,
        json!({ "outcome": outcome, "method": method, "reason": reason, "ip": ip(ctx) }),
    );
}

fn user_agent<'a>(ctx: &'a Ctx) -> Option<&'a str> {
    ctx.req.header("user-agent")
}

pub fn login(ctx: &Ctx) -> ApiResult {
    if !ctx.app.cfg.auth_password_login {
        return Err(forbidden("パスワードでのログインは使えません"));
    }
    let b: LoginBody = ctx.body()?;
    let tenant = b.tenant.clone().unwrap_or_else(|| "default".into());
    if b.username.len() > 256 || b.password.len() > 4 * users::PASSWORD_MAX_CHARS || tenant.len() > 64 {
        return Err(Problem::invalid("username・password が長すぎます"));
    }
    let account = na::account_key(&tenant, &b.username);
    na::check_login_limits(ctx.app, ip(ctx), Some(&account))?;
    let user = ctx.control().state.user_by_name(&tenant, &b.username).cloned();
    // 利用者がいない・パスワードが無いときも、同じだけ計算する
    let target = user.as_ref().and_then(|u| u.password_hash.clone()).unwrap_or_else(|| users::dummy_hash().to_string());
    let matched = ctx.app.auth.with_hashing(|| users::verify_password(&b.password, &target));
    let ok = matched && user.as_ref().is_some_and(|u| u.password_hash.is_some() && !u.disabled);
    let Some(u) = user.filter(|_| ok) else {
        na::login_failed(ctx.app, ip(ctx), Some(&account));
        ctx.app.log(&format!("auth: password login failed request_id={}", ctx.req.request_id));
        audit_login(ctx, &tenant, None, "failure", "password", None);
        return Err(na::auth_failed());
    };
    ctx.app.limits.login_accounts.reset(&account);
    let phc = u.password_hash.clone().unwrap_or_default();
    if u.mfa.is_some() {
        let exp = ctx.now_ms + MFA_TOKEN_TTL_MS;
        let tok = na::seal(ctx.app, "mfa", json!({ "t": u.tenant, "u": u.id, "ph": sha(&phc) }), exp)?;
        audit_login(ctx, &tenant, Some(&u.id), "mfa_required", "password", None);
        return Ok(ApiResponse::json(
            200,
            &json!({ "status": "mfa_required", "mfa_token": tok, "expires_at": rfc3339(exp), "methods": ["totp", "recovery_code"] }),
        ));
    }
    let rehash = if users::needs_rehash(&phc) { ctx.app.auth.with_hashing(|| users::hash_password(&b.password)).ok() } else { None };
    let ns = na::new_session(ctx.app, &u, "password", None, false, ip(ctx), user_agent(ctx), ctx.now_ms)?;
    na::create_session(ctx.app, &ns, LoginRecord { password_hash: Some(phc), rehash, ..Default::default() }).map_err(login_conflict)?;
    audit_login(ctx, &tenant, Some(&u.id), "success", "password", None);
    login_ok(ctx, &ns, None)
}

/// ログインの記録が合わなかった（使用済みのコード・その間にパスワードが変わった）: 401 にそろえる。
fn login_conflict(p: Problem) -> Problem {
    if p.status == 409 || p.status == 401 || p.status == 404 { na::auth_failed() } else { p }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MfaBody {
    mfa_token: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    recovery_code: Option<String>,
}

/// TOTP のコードか回復コードを確かめる。合えば (TOTP の区切り, 回復コードのハッシュ)。
fn check_second_factor(ctx: &Ctx, u: &User, code: Option<&str>, recovery: Option<&str>) -> Option<(Option<u64>, Option<String>)> {
    let m = u.mfa.as_ref()?;
    if let Some(c) = code {
        let secret = ctx.app.open_secret(&m.totp_sealed, &users::totp_aad(&u.tenant, &u.id))?;
        return users::totp_check(&secret, c, ctx.now_ms / 1000, m.last_step).map(|s| (Some(s), None));
    }
    let r = recovery?;
    users::recovery_match(m, &u.id, r).map(|h| (None, Some(h)))
}

pub fn login_mfa(ctx: &Ctx) -> ApiResult {
    let b: MfaBody = ctx.body()?;
    if b.code.is_some() == b.recovery_code.is_some() {
        return Err(Problem::invalid("code か recovery_code のどちらか 1 つを指定してください"));
    }
    na::check_login_limits(ctx.app, ip(ctx), None)?;
    let tok = na::open(ctx.app, "mfa", &b.mfa_token, ctx.now_ms).ok_or_else(|| {
        na::login_failed(ctx.app, ip(ctx), None);
        na::auth_failed()
    })?;
    let (t, uid, ph) = (tok["t"].as_str().unwrap_or(""), tok["u"].as_str().unwrap_or(""), tok["ph"].as_str().unwrap_or(""));
    let u = ctx.control().state.tenant_user(t, uid).cloned().map_err(|_| na::auth_failed())?;
    let account = na::account_key(&u.tenant, &u.username);
    na::check_login_limits(ctx.app, ip(ctx), Some(&account))?;
    let phc = u.password_hash.clone().unwrap_or_default();
    let same_pw: bool = sha(&phc).as_bytes().ct_eq(ph.as_bytes()).into();
    let factor = if same_pw && !u.disabled { check_second_factor(ctx, &u, b.code.as_deref(), b.recovery_code.as_deref()) } else { None };
    let method = if b.code.is_some() { "password+totp" } else { "password+recovery_code" };
    let Some((step, rc)) = factor else {
        na::login_failed(ctx.app, ip(ctx), Some(&account));
        audit_login(ctx, &u.tenant, Some(&u.id), "failure", method, Some("second_factor"));
        return Err(na::auth_failed());
    };
    ctx.app.limits.login_accounts.reset(&account);
    let ns = na::new_session(ctx.app, &u, "password", None, true, ip(ctx), user_agent(ctx), ctx.now_ms)?;
    let login = LoginRecord { password_hash: Some(phc), totp_step: step, recovery_code: rc.clone(), rehash: None };
    if let Err(e) = na::create_session(ctx.app, &ns, login) {
        audit_login(ctx, &u.tenant, Some(&u.id), "failure", method, Some("replayed"));
        return Err(login_conflict(e));
    }
    audit_login(ctx, &u.tenant, Some(&u.id), "success", method, None);
    if rc.is_some() {
        ctx.app.audit_entry(ctx.now_ms, &u.tenant, &format!("user:{}", u.id), "auth.mfa.recovery_code_used", &u.id, &ctx.req.request_id, Value::Null);
    }
    login_ok(ctx, &ns, None)
}

// ---------------------------------------------------------------- 本人

pub fn me(ctx: &Ctx) -> ApiResult {
    let p = &ctx.principal;
    let (method, user, session) = match &p.session {
        Some(s) => {
            let st = &ctx.control().state;
            let u = st.users.get(&s.user).map(render_user);
            let sess = st.sessions.get(&s.hash).map(|x| render_session(x, Some(&s.id)));
            ("session", u, sess)
        }
        None if p.token_id.is_some() => ("api_token", None, None),
        None => ("jwt", None, None),
    };
    Ok(ApiResponse::json(
        200,
        &json!({
            "tenant": p.tenant,
            "subject": p.subject,
            "auth_method": method,
            "scopes": p.effective(),
            "user": user,
            "session": session,
            "pending": pending_list(p),
        }),
    ))
}

pub fn logout(ctx: &Ctx) -> ApiResult {
    let (u, sid) = session_user(ctx)?;
    ctx.app.propose(Command::DeleteSession { tenant: ctx.tenant().to_string(), id: sid.clone() })?;
    ctx.app.audit(ctx, "auth.logout", &u.id);
    Ok(ApiResponse::empty(204))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordChange {
    current_password: String,
    new_password: String,
}

/// 本人確認（パスワード）。違えばアカウントの失敗として数えて 403。
fn verify_current_password(ctx: &Ctx, u: &User, pw: &str) -> Result<(), Problem> {
    let account = na::account_key(&u.tenant, &u.username);
    na::check_login_limits(ctx.app, ip(ctx), Some(&account))?;
    let h = u.password_hash.clone().unwrap_or_else(|| users::dummy_hash().to_string());
    let ok = ctx.app.auth.with_hashing(|| users::verify_password(pw, &h)) && u.password_hash.is_some();
    if !ok {
        na::login_failed(ctx.app, ip(ctx), Some(&account));
        return Err(forbidden("パスワードが違います"));
    }
    Ok(())
}

pub fn change_password(ctx: &Ctx) -> ApiResult {
    let (u, sid) = session_user(ctx)?;
    let b: PasswordChange = ctx.body()?;
    if u.password_hash.is_none() {
        return Err(Problem::new(ProblemCode::Conflict, "この利用者にはパスワードがありません（OIDC でログインします）"));
    }
    verify_current_password(ctx, &u, &b.current_password)?;
    users::check_password_policy(&b.new_password, &u.username, u.email.as_deref()).map_err(|e| Problem::new(ProblemCode::InvalidConfig, e))?;
    if b.new_password == b.current_password {
        return Err(Problem::new(ProblemCode::InvalidConfig, "新しいパスワードが今のものと同じです"));
    }
    let h = ctx.app.auth.with_hashing(|| users::hash_password(&b.new_password))?;
    ctx.app.propose(Command::SetUserPassword {
        tenant: u.tenant.clone(),
        id: u.id.clone(),
        password_hash: Some(h),
        must_change_password: false,
        keep_session: Some(sid),
        now_ms: ctx.now_ms,
    })?;
    ctx.app.audit(ctx, "auth.password.change", &u.id);
    Ok(ApiResponse::empty(204))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reauth {
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

pub fn mfa_setup(ctx: &Ctx) -> ApiResult {
    let (u, _) = session_user(ctx)?;
    let b: Reauth = ctx.body_or_default()?;
    if b.code.is_some() {
        return Err(Problem::invalid("ここでは code は使いません"));
    }
    if u.mfa.is_some() {
        return Err(Problem::new(ProblemCode::Conflict, "MFA はすでに有効です（作り直すには先に解除してください）"));
    }
    if u.password_hash.is_none() {
        return Err(Problem::new(ProblemCode::Conflict, "MFA はパスワードでログインする利用者だけが使います（OIDC の利用者は IdP の MFA を使います）"));
    }
    let pw = b.password.as_deref().ok_or_else(|| Problem::invalid("password（今のパスワード）が要ります"))?;
    verify_current_password(ctx, &u, pw)?;
    let s = users::totp_new_secret()?;
    let exp = ctx.now_ms + ENROLL_TTL_MS;
    let tok = na::seal(ctx.app, "totp-enroll", json!({ "t": u.tenant, "u": u.id, "s": secret::hex(&s) }), exp)?;
    let account = u.email.clone().unwrap_or_else(|| u.username.clone());
    Ok(ApiResponse::json(
        200,
        &json!({
            "secret": users::totp_base32(&s),
            "otpauth_uri": users::totp_uri(&s, &ctx.app.cfg.auth_mfa_issuer, &account),
            "enrollment_token": tok,
            "expires_at": rfc3339(exp),
            "algorithm": "SHA1", "digits": users::TOTP_DIGITS, "period": users::TOTP_STEP_SECS,
        }),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MfaConfirm {
    enrollment_token: String,
    code: String,
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    (s.len().is_multiple_of(2)).then_some(())?;
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn new_recovery(u: &User) -> Result<(Vec<String>, Vec<String>), Problem> {
    let codes = users::recovery_generate()?;
    let hashes = codes.iter().map(|c| users::recovery_hash(&u.id, c)).collect();
    Ok((codes, hashes))
}

pub fn mfa_confirm(ctx: &Ctx) -> ApiResult {
    let (u, _) = session_user(ctx)?;
    let b: MfaConfirm = ctx.body()?;
    let tok = na::open(ctx.app, "totp-enroll", &b.enrollment_token, ctx.now_ms)
        .filter(|t| t["t"] == u.tenant.as_str() && t["u"] == u.id.as_str())
        .ok_or_else(|| Problem::invalid("enrollment_token が違うか、期限が切れています（登録をやり直してください）"))?;
    if u.mfa.is_some() {
        return Err(Problem::new(ProblemCode::Conflict, "MFA はすでに有効です"));
    }
    let s = tok["s"].as_str().and_then(from_hex).ok_or_else(Problem::internal)?;
    let step = users::totp_check(&s, &b.code, ctx.now_ms / 1000, 0).ok_or_else(|| Problem::invalid("確認コードが違います"))?;
    let sealed = ctx.app.seal_secret(&s, &users::totp_aad(&u.tenant, &u.id))?;
    let (codes, hashes) = new_recovery(&u)?;
    let mfa = UserMfa { totp_sealed: sealed, enrolled_at_ms: ctx.now_ms, last_step: step, recovery: hashes };
    ctx.app.propose(Command::SetUserMfa { tenant: u.tenant.clone(), id: u.id.clone(), mfa: Some(mfa), now_ms: ctx.now_ms })?;
    ctx.app.audit(ctx, "auth.mfa.enable", &u.id);
    let u = ctx.control().state.tenant_user(ctx.tenant(), &u.id).cloned()?;
    Ok(ApiResponse::json(200, &json!({ "recovery_codes": codes, "user": render_user(&u) })))
}

/// MFA の操作の本人確認（パスワード、または TOTP のコード・回復コード）。
fn reauth(ctx: &Ctx, u: &User, b: &Reauth) -> Result<Option<u64>, Problem> {
    match (&b.password, &b.code) {
        (Some(pw), None) => verify_current_password(ctx, u, pw).map(|_| None),
        (None, Some(c)) => {
            let account = na::account_key(&u.tenant, &u.username);
            na::check_login_limits(ctx.app, ip(ctx), Some(&account))?;
            let is_totp = c.trim().len() == users::TOTP_DIGITS;
            let r = if is_totp { check_second_factor(ctx, u, Some(c), None) } else { check_second_factor(ctx, u, None, Some(c)) };
            match r {
                Some((step, _)) => Ok(step),
                None => {
                    na::login_failed(ctx.app, ip(ctx), Some(&account));
                    Err(forbidden("確認コードが違います"))
                }
            }
        }
        _ => Err(Problem::invalid("password か code のどちらか 1 つを指定してください")),
    }
}

pub fn mfa_disable(ctx: &Ctx) -> ApiResult {
    let (u, _) = session_user(ctx)?;
    let b: Reauth = ctx.body()?;
    if u.mfa.is_none() {
        return Err(Problem::new(ProblemCode::Conflict, "MFA は有効ではありません"));
    }
    if ctx.app.cfg.auth_require_mfa {
        return Err(Problem::new(ProblemCode::Conflict, "このノードでは MFA が必須です（解除できません）"));
    }
    reauth(ctx, &u, &b)?;
    ctx.app.propose(Command::SetUserMfa { tenant: u.tenant.clone(), id: u.id.clone(), mfa: None, now_ms: ctx.now_ms })?;
    ctx.app.audit(ctx, "auth.mfa.disable", &u.id);
    Ok(ApiResponse::empty(204))
}

pub fn mfa_recovery_codes(ctx: &Ctx) -> ApiResult {
    let (u, _) = session_user(ctx)?;
    let b: Reauth = ctx.body()?;
    let Some(m) = u.mfa.clone() else { return Err(Problem::new(ProblemCode::Conflict, "MFA は有効ではありません")) };
    let step = reauth(ctx, &u, &b)?;
    let (codes, hashes) = new_recovery(&u)?;
    let mfa = UserMfa { recovery: hashes, last_step: step.unwrap_or(0).max(m.last_step), ..m };
    ctx.app.propose(Command::SetUserMfa { tenant: u.tenant.clone(), id: u.id.clone(), mfa: Some(mfa), now_ms: ctx.now_ms })?;
    ctx.app.audit(ctx, "auth.mfa.recovery_codes", &u.id);
    Ok(ApiResponse::json(200, &json!({ "recovery_codes": codes })))
}

pub fn sessions(ctx: &Ctx) -> ApiResult {
    let (u, sid) = session_user(ctx)?;
    let (limit, cursor) = ctx.page()?;
    let (page, next) = paginate(user_sessions(ctx, &u.id), limit, cursor.as_deref());
    let items: Vec<Value> = page.iter().map(|s| render_session(s, Some(&sid))).collect();
    Ok(ApiResponse::json(200, &json!({ "items": items, "next_cursor": next })))
}

pub fn revoke_session(ctx: &Ctx, id: &str) -> ApiResult {
    let (u, _) = session_user(ctx)?;
    if !ctx.control().state.session_by_id(ctx.tenant(), id).is_some_and(|s| s.user == u.id) {
        return Err(Problem::not_found());
    }
    ctx.app.propose(Command::DeleteSession { tenant: ctx.tenant().to_string(), id: id.to_string() })?;
    ctx.app.audit(ctx, "auth.session.revoke", id);
    Ok(ApiResponse::empty(204))
}

// ---------------------------------------------------------------- OIDC

fn cookie_value<'a>(ctx: &'a Ctx<'a>) -> Option<&'a str> {
    let h = ctx.req.header("cookie")?;
    h.split(';').map(str::trim).find_map(|kv| kv.strip_prefix(COOKIE).and_then(|r| r.strip_prefix('=')))
}

fn set_cookie(value: &str, max_age_secs: i64, secure: bool) -> String {
    format!("{COOKIE}={value}; Path=/api/v1/auth; Max-Age={max_age_secs}; HttpOnly; SameSite=Lax{}", if secure { "; Secure" } else { "" })
}

/// 戻り先: 管理画面（`/console/...`）のパスだけ。
fn valid_return_to(r: &str) -> bool {
    (r == "/console" || r.starts_with("/console/"))
        && r.len() <= 512
        && !r.contains("//")
        && !r.contains('\\')
        && !r.contains("/..")
        && !r.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// 管理画面の完了の画面へ（`code` か `error`）。
fn to_console(kind: &str, value: &str) -> ApiResponse {
    ApiResponse::empty(302).with_header("Location", format!("/console/#/auth/complete?{kind}={}", crate::oidc::form_encode(value)))
}

fn query(ctx: &Ctx, name: &str) -> Option<String> {
    ctx.req.query_pairs().into_iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

pub fn oidc_start(ctx: &Ctx, provider: &str) -> ApiResult {
    let p = ctx.app.auth.provider(provider).ok_or_else(Problem::not_found)?;
    for (k, _) in ctx.req.query_pairs() {
        if k != "return_to" {
            return Err(Problem::invalid(format!("知らないクエリ: {k}")));
        }
    }
    let return_to = query(ctx, "return_to");
    if return_to.as_deref().is_some_and(|r| !valid_return_to(r)) {
        return Err(Problem::invalid("return_to は管理画面（/console/...）のパスだけです"));
    }
    na::check_login_limits(ctx.app, ip(ctx), None)?;
    let Some(rt) = ctx.app.rt.get() else { return Err(Problem::internal()) };
    let meta = match crate::util::block_on(rt, p.metadata()) {
        Ok(m) => m,
        Err(e) => {
            ctx.app.log(&format!("oidc {provider}: discovery failed: {e}"));
            return Ok(to_console("error", "provider_unavailable"));
        }
    };
    let (state, nonce, verifier, binding) = (na::random_token()?, na::random_token()?, na::random_token()?, na::random_token()?);
    let redirect_uri = p.redirect_uri.clone();
    let flow = json!({ "p": provider, "st": state, "n": nonce, "v": verifier, "r": return_to, "b": binding, "ru": redirect_uri });
    let sealed = na::seal(ctx.app, "oidc", flow, ctx.now_ms + OIDC_FLOW_TTL_MS)?;
    let url = p.authorize_url(&meta, &redirect_uri, &state, &nonce, &senca_control::oidc::pkce_challenge(&verifier));
    Ok(ApiResponse::empty(302)
        .with_header("Location", url)
        .with_header("Set-Cookie", set_cookie(&sealed, OIDC_FLOW_TTL_MS / 1000, redirect_uri.starts_with("https://"))))
}

/// コールバックの失敗: 管理画面へ `error` を渡し、途中のクッキーを消す。
fn callback_error(ctx: &Ctx, provider: &str, code: &str, detail: &str, tenant: &str, count: bool) -> ApiResult {
    if count {
        na::login_failed(ctx.app, ip(ctx), None);
    }
    ctx.app.log(&format!("oidc {provider}: login failed ({code}): {detail} request_id={}", ctx.req.request_id));
    audit_login(ctx, tenant, None, "failure", &format!("oidc:{provider}"), Some(code));
    Ok(to_console("error", code).with_header("Set-Cookie", set_cookie("", 0, false)))
}

pub fn oidc_callback(ctx: &Ctx, provider: &str) -> ApiResult {
    let p = ctx.app.auth.provider(provider).ok_or_else(Problem::not_found)?;
    if na::check_login_limits(ctx.app, ip(ctx), None).is_err() {
        return callback_error(ctx, provider, "rate_limited", "too many failures", "", false);
    }
    let flow = cookie_value(ctx).and_then(|c| na::open(ctx.app, "oidc", c, ctx.now_ms));
    let Some(flow) = flow.filter(|f| f["p"] == provider && f.get("st").is_some()) else {
        return callback_error(ctx, provider, "invalid_state", "flow cookie missing, expired or for another provider", "", true);
    };
    let got_state = query(ctx, "state").unwrap_or_default();
    let want_state = flow["st"].as_str().unwrap_or("");
    let state_ok: bool = got_state.as_bytes().ct_eq(want_state.as_bytes()).into();
    if !state_ok || want_state.is_empty() {
        return callback_error(ctx, provider, "invalid_state", "state mismatch", "", true);
    }
    if let Some(e) = query(ctx, "error") {
        let code = if e == "access_denied" { "access_denied" } else { "idp_error" };
        return callback_error(ctx, provider, code, &format!("idp error={}", e.chars().take(64).collect::<String>()), "", false);
    }
    let Some(code) = query(ctx, "code").filter(|c| !c.is_empty() && c.len() <= 4096) else {
        return callback_error(ctx, provider, "invalid_request", "code missing", "", true);
    };
    let Some(rt) = ctx.app.rt.get() else { return Err(Problem::internal()) };
    let meta = match crate::util::block_on(rt, p.metadata()) {
        Ok(m) => m,
        Err(e) => return callback_error(ctx, provider, "provider_unavailable", &e, "", false),
    };
    // RFC 9207: 応答の iss（あれば）は発行者と同じでなければならない（IdP の取り違えを防ぐ）
    if let Some(iss) = query(ctx, "iss")
        && iss != meta.issuer
    {
        return callback_error(ctx, provider, "invalid_response", "iss parameter mismatch", "", true);
    }
    let (verifier, redirect_uri, nonce) = (flow["v"].as_str().unwrap_or(""), flow["ru"].as_str().unwrap_or(""), flow["n"].as_str().unwrap_or(""));
    let id_token = match crate::util::block_on(rt, p.exchange(&meta, &code, verifier, redirect_uri)) {
        Ok(t) => t,
        Err(e) => return callback_error(ctx, provider, "token_exchange_failed", &e, "", true),
    };
    let claims = match crate::util::block_on(rt, p.validate(&meta, &id_token, nonce, ctx.now_ms)) {
        Ok(c) => c,
        Err(e) => return callback_error(ctx, provider, "invalid_id_token", &e, "", true),
    };
    let u = match resolve_user(ctx, &p, &claims) {
        Ok(u) => u,
        Err((code, detail, tenant)) => return callback_error(ctx, provider, code, &detail, &tenant, false),
    };
    // 一度きりのコード（60 秒）。ブラウザーの印（クッキーの中の乱数）と組にする
    let binding = flow["b"].as_str().unwrap_or("").to_string();
    let one_time = na::random_token()?;
    let lc = LoginCode {
        hash: sha(&one_time),
        tenant: u.tenant.clone(),
        user: u.id.clone(),
        provider: provider.to_string(),
        binding_hash: sha(&binding),
        return_to: flow["r"].as_str().map(str::to_string),
        created_at_ms: ctx.now_ms,
        expires_at_ms: ctx.now_ms + LOGIN_CODE_TTL_MS,
    };
    ctx.app.propose(Command::PutLoginCode { code: lc })?;
    // 途中のクッキーは、印だけを残して作り直す（同じ state で 2 度コールバックさせない）
    let sealed = na::seal(ctx.app, "oidc", json!({ "p": provider, "b": binding, "done": true }), ctx.now_ms + 5 * 60_000)?;
    audit_login(ctx, &u.tenant, Some(&u.id), "callback", &format!("oidc:{provider}"), None);
    Ok(to_console("code", &one_time).with_header("Set-Cookie", set_cookie(&sealed, 300, p.redirect_uri.starts_with("https://"))))
}

type Denied = (&'static str, String, String);

/// ID トークンのクレームから利用者を決める（結び付いた利用者 → 同じメールアドレスの利用者 → 自動で作る）。
fn resolve_user(ctx: &Ctx, p: &crate::oidc::Provider, c: &Value) -> Result<User, Denied> {
    use senca_control::oidc as o;
    let cfg = &p.cfg;
    let sub = c["sub"].as_str().unwrap_or("").to_string();
    let tenant = match (&cfg.tenant_claim, &cfg.tenant) {
        (Some(claim), _) => match o::claim_str(c, claim).filter(|t| users::valid_tenant(t)) {
            Some(t) => t.to_string(),
            None => return Err(("tenant_not_allowed", format!("claim {claim} missing or invalid"), String::new())),
        },
        (None, Some(t)) => t.clone(),
        (None, None) => "default".into(),
    };
    let deny = |code: &'static str, d: String| (code, d, tenant.clone());
    let email = o::claim_str(c, "email").map(str::to_string);
    let email_trusted = email.is_some() && (o::email_verified(c) || !cfg.require_email_verified);
    if !cfg.allowed_email_domains.is_empty() {
        if !email_trusted {
            return Err(deny("email_not_verified", "email missing or not verified".into()));
        }
        if !o::email_domain_allowed(email.as_deref(), &cfg.allowed_email_domains) {
            return Err(deny("email_domain_not_allowed", "email domain not allowed".into()));
        }
    }
    let (mut roles, mut scopes) = o::map_grants(c, &cfg.role_mappings);
    if roles.is_empty() && scopes.is_empty() {
        match cfg.default_role.as_deref().and_then(o::parse_grant) {
            Some(o::Grant::Role(r)) => {
                roles.insert(r);
            }
            Some(o::Grant::Scope(s)) => {
                scopes.insert(s);
            }
            None => {}
        }
    }
    let link = OidcLink { provider: cfg.id.clone(), subject: sub.clone() };
    let now = ctx.now_ms;
    let (linked, by_email) = {
        let st = &ctx.control().state;
        let linked = st.user_by_oidc(&tenant, &link).cloned();
        let by_email = if linked.is_none() && email_trusted { email.as_deref().and_then(|e| st.user_by_email(&tenant, e)).cloned() } else { None };
        (linked, by_email)
    };
    let propose = |cmd: Command| ctx.app.propose(cmd).map_err(|e| deny("server_error", e.title));
    let existing = match (linked, by_email) {
        (Some(u), _) => Some(u),
        (None, Some(u)) => {
            if u.oidc.iter().any(|l| l.provider == cfg.id) {
                return Err(deny("account_conflict", "the user with this email is linked to another account of this provider".into()));
            }
            if u.disabled {
                return Err(deny("account_disabled", "user disabled".into()));
            }
            propose(Command::LinkUserOidc { tenant: tenant.clone(), id: u.id.clone(), link: link.clone(), now_ms: now })?;
            ctx.app.audit_entry(now, &tenant, &format!("user:{}", u.id), "user.oidc.link", &u.id, &ctx.req.request_id, json!({ "provider": cfg.id }));
            Some(u)
        }
        (None, None) => None,
    };
    let user = match existing {
        Some(u) => {
            if u.disabled {
                return Err(deny("account_disabled", "user disabled".into()));
            }
            if !cfg.role_mappings.is_empty() && cfg.sync_roles && (u.roles != roles || u.scopes != scopes) {
                if roles.is_empty() && scopes.is_empty() {
                    return Err(deny("no_role", "no role mapping matched".into()));
                }
                let profile = UserProfile { email: u.email.clone(), display_name: u.display_name.clone(), roles: roles.clone(), scopes: scopes.clone(), disabled: false };
                match ctx.app.propose(Command::UpdateUser { tenant: tenant.clone(), id: u.id.clone(), profile, expect_generation: None, now_ms: now }) {
                    Ok(()) => ctx.app.audit_entry(
                        now,
                        &tenant,
                        &format!("user:{}", u.id),
                        "user.update",
                        &u.id,
                        &ctx.req.request_id,
                        json!({ "roles_changed": true, "roles": roles, "scopes": scopes, "by": format!("oidc:{}", cfg.id) }),
                    ),
                    // 最後の管理者の降格などで断られた: 今の役割のままにする
                    Err(e) => ctx.app.log(&format!("oidc {}: role sync for user {} skipped: {}", cfg.id, u.id, e.title)),
                }
            }
            u.id
        }
        None => {
            if !cfg.auto_provision {
                return Err(deny("no_account", "no linked user and auto_provision is off".into()));
            }
            if roles.is_empty() && scopes.is_empty() {
                return Err(deny("no_role", "no role mapping matched".into()));
            }
            let base = o::username_candidate(c, cfg.username_claim.as_deref());
            let username = {
                let st = &ctx.control().state;
                let mut n = base.clone();
                let mut i = 2;
                while st.user_by_name(&tenant, &n).is_some() {
                    n = format!("{base}-{i}");
                    i += 1;
                }
                n
            };
            let id = super::new_id().map_err(|e| deny("server_error", e.title))?;
            let display_name = o::claim_str(c, "name").map(|n| n.chars().filter(|c| !c.is_control()).take(128).collect::<String>()).filter(|n| !n.is_empty());
            let u = User {
                id: id.clone(),
                tenant: tenant.clone(),
                username,
                email: email.filter(|_| email_trusted),
                display_name,
                password_hash: None,
                roles,
                scopes,
                disabled: false,
                must_change_password: false,
                mfa: None,
                oidc: vec![link],
                generation: 1,
                created_at_ms: now,
                updated_at_ms: now,
                last_login_at_ms: None,
                password_changed_at_ms: None,
            };
            propose(Command::CreateUser { user: u.clone() })?;
            ctx.app.audit_entry(now, &tenant, &format!("user:{id}"), "user.create", &id, &ctx.req.request_id, json!({ "by": format!("oidc:{}", cfg.id), "roles": u.roles, "scopes": u.scopes }));
            id
        }
    };
    na::wait_state(ctx.app, Duration::from_secs(3), |st| st.users.get(&user).is_some_and(|u| u.oidc.iter().any(|l| l.provider == cfg.id)));
    let u = ctx.control().state.tenant_user(&tenant, &user).cloned().map_err(|_| deny("server_error", "user vanished".into()))?;
    if u.granted().is_empty() {
        return Err(deny("no_role", "user has no roles".into()));
    }
    Ok(u)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExchangeBody {
    code: String,
}

pub fn exchange(ctx: &Ctx) -> ApiResult {
    let b: ExchangeBody = ctx.body()?;
    na::check_login_limits(ctx.app, ip(ctx), None)?;
    let fail = || {
        na::login_failed(ctx.app, ip(ctx), None);
        na::auth_failed()
    };
    let flow = cookie_value(ctx).and_then(|c| na::open(ctx.app, "oidc", c, ctx.now_ms)).filter(|f| f["done"] == true);
    let Some(flow) = flow else { return Err(fail()) };
    let binding = flow["b"].as_str().unwrap_or("");
    let h = sha(&b.code);
    // 分散: コールバックを受けたノードの書き込みがまだ届いていなければ少し待つ
    na::wait_state(ctx.app, Duration::from_secs(2), |st| st.login_codes.contains_key(&h));
    let Some(lc) = ctx.control().state.login_codes.get(&h).cloned() else { return Err(fail()) };
    let u = ctx.control().state.tenant_user(&lc.tenant, &lc.user).cloned().map_err(|_| fail())?;
    let ns = na::new_session(ctx.app, &u, "oidc", Some(&lc.provider), false, ip(ctx), user_agent(ctx), ctx.now_ms)?;
    let r = ctx.app.propose(Command::RedeemLoginCode { hash: h, binding_hash: sha(binding), now_ms: ctx.now_ms, session: ns.session.clone() });
    if let Err(e) = r {
        if e.status >= 500 {
            return Err(e);
        }
        audit_login(ctx, &u.tenant, Some(&u.id), "failure", &format!("oidc:{}", lc.provider), Some("exchange"));
        return Err(fail());
    }
    let hh = ns.session.hash.clone();
    na::wait_state(ctx.app, Duration::from_secs(3), |st| st.sessions.contains_key(&hh));
    audit_login(ctx, &u.tenant, Some(&u.id), "success", &format!("oidc:{}", lc.provider), None);
    Ok(login_ok(ctx, &ns, lc.return_to)?.with_header("Set-Cookie", set_cookie("", 0, false)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_to_is_restricted_to_the_console() {
        assert!(valid_return_to("/console/"));
        assert!(valid_return_to("/console/#/streams?x=1"));
        assert!(!valid_return_to("https://evil.example/"));
        assert!(!valid_return_to("//evil.example/console/"));
        assert!(!valid_return_to("/console//evil.example"));
        assert!(!valid_return_to("/console/../api"));
        assert!(!valid_return_to("/console\\evil"));
        assert!(!valid_return_to("/consolex"));
        assert!(!valid_return_to("/console/\nx"));
        assert_eq!(from_hex("0aff"), Some(vec![10, 255]));
        assert_eq!(from_hex("0"), None);
    }
}
