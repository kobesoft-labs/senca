//! 利用者のログイン（ADR-0017）のノードの側: セッションの発行・最後に使った時刻の更新、短い間だけ使う封じた値
//! （MFA の途中・TOTP の登録の途中・OIDC の途中のクッキー）、パスワードのハッシュの同時実行の上限。
//!
//! 封じた値は秘密値と同じ鍵（分散ではクラスタ共通のデータ鍵）の AES-256-GCM で、用途ごとに関連データを分ける。
//! どのノードでも開けるので、ログインの途中で別のノードに要求が届いてもよい（合意ログに書かずに済む）。

use crate::app::App;
use senca_control::auth::{Principal, SessionRef};
use senca_control::secret::{self, Secret};
use senca_control::state::{Command, ControlState};
use senca_control::users::{LoginRecord, SESSION_PREFIX, Session, User};
use senca_control::{Problem, ProblemCode};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// 同時に計算するパスワードのハッシュ（Argon2id は 1 回 19 MiB）の上限。
const MAX_HASHING: usize = 4;

pub struct AuthRuntime {
    hashing: (Mutex<usize>, Condvar),
    /// 最後に使った時刻の更新を送ったセッション（ハッシュ → 送った時刻）。同じものを何度も送らない。
    touching: Mutex<HashMap<String, i64>>,
    /// OIDC の IdP（設定の順）。
    pub oidc: Vec<std::sync::Arc<crate::oidc::Provider>>,
}

impl AuthRuntime {
    pub fn new(cfg: &crate::config::Config) -> Result<AuthRuntime, String> {
        let mut oidc = Vec::new();
        for p in &cfg.oidc_providers {
            oidc.push(std::sync::Arc::new(crate::oidc::Provider::new(p.clone(), cfg.public.http.as_deref())?));
        }
        Ok(AuthRuntime { hashing: (Mutex::new(0), Condvar::new()), touching: Mutex::new(HashMap::new()), oidc })
    }

    pub fn provider(&self, id: &str) -> Option<std::sync::Arc<crate::oidc::Provider>> {
        self.oidc.iter().find(|p| p.cfg.id == id).cloned()
    }

    /// パスワードのハッシュ・照合を、同時実行の上限の中で行う。
    pub fn with_hashing<T>(&self, f: impl FnOnce() -> T) -> T {
        let (m, cv) = &self.hashing;
        {
            let mut n = m.lock().unwrap_or_else(|e| e.into_inner());
            while *n >= MAX_HASHING {
                n = cv.wait(n).unwrap_or_else(|e| e.into_inner());
            }
            *n += 1;
        }
        let r = f();
        *m.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        cv.notify_one();
        r
    }
}

/// 最後に使った時刻を進める間隔（待ち時間の 1/16 を 30 秒〜5 分に収めたもの。ただし待ち時間の 1/4 まで）。
/// 合意ログへの書き込みを要求ごとにしない。
pub fn touch_interval_ms(idle_ms: i64) -> i64 {
    (idle_ms / 16).clamp(30_000, 300_000).min(idle_ms / 4)
}

/// セッションで使ったとき、前回の記録から間隔が空いていれば、最後に使った時刻を進める（裏で送る）。
pub fn touch(app: &App, p: &Principal, now_ms: i64) {
    let Some(s) = &p.session else { return };
    let every = touch_interval_ms(s.idle_timeout_ms);
    if now_ms - s.last_seen_ms < every {
        return;
    }
    {
        let mut t = app.auth.touching.lock().unwrap_or_else(|e| e.into_inner());
        if t.get(&s.hash).is_some_and(|at| now_ms - at < every) {
            return;
        }
        if t.len() > 10_000 {
            t.retain(|_, at| now_ms - *at < 300_000);
        }
        t.insert(s.hash.clone(), now_ms);
    }
    let Some(rt) = app.rt.get() else { return };
    let a = app.arc();
    let hash = s.hash.clone();
    rt.spawn_blocking(move || {
        if let Err(e) = a.propose(Command::TouchSession { hash, now_ms }) {
            a.log(&format!("auth: session touch failed: {}", e.title));
        }
    });
}

/// 手元の制御状態が `pred` を満たすまで待つ（分散で、別のノードの書き込みがまだ届いていないとき）。
pub fn wait_state(app: &App, timeout: Duration, pred: impl Fn(&ControlState) -> bool) -> bool {
    let end = std::time::Instant::now() + timeout;
    loop {
        if pred(&app.control.lock().unwrap_or_else(|e| e.into_inner()).state) {
            return true;
        }
        if app.cluster.get().is_none() || std::time::Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// セッションの元（値・状態に置く形）。
pub struct NewSession {
    pub value: Secret,
    pub session: Session,
}

/// 新しいセッション（まだ提案しない）。
#[allow(clippy::too_many_arguments)]
pub fn new_session(
    app: &App,
    user: &User,
    method: &str,
    provider: Option<&str>,
    mfa: bool,
    ip: &str,
    user_agent: Option<&str>,
    now_ms: i64,
) -> Result<NewSession, Problem> {
    let value = secret::generate(SESSION_PREFIX).map_err(|_| Problem::internal())?;
    let session = Session {
        id: crate::api::new_id()?,
        tenant: user.tenant.clone(),
        user: user.id.clone(),
        hash: secret::hash(value.expose()),
        method: method.into(),
        provider: provider.map(str::to_string),
        mfa,
        created_at_ms: now_ms,
        last_seen_ms: now_ms,
        idle_timeout_ms: app.cfg.auth_session_idle_secs as i64 * 1000,
        expires_at_ms: now_ms + app.cfg.auth_session_lifetime_secs as i64 * 1000,
        ip: (!ip.is_empty()).then(|| ip.chars().take(64).collect()),
        user_agent: user_agent.map(|u| u.chars().filter(|c| !c.is_control()).take(256).collect()),
    };
    Ok(NewSession { value, session })
}

/// セッションを作る（合意を取って、手元に届くまで待つ。直後の要求がこのノードで通るように）。
pub fn create_session(app: &App, ns: &NewSession, login: LoginRecord) -> Result<(), Problem> {
    app.propose(Command::CreateSession { session: ns.session.clone(), login })?;
    let h = ns.session.hash.clone();
    wait_state(app, Duration::from_secs(3), |st| st.sessions.contains_key(&h));
    Ok(())
}

/// 主体（作ったばかりのセッション）。
pub fn principal_for(app: &App, ns: &NewSession) -> Option<Principal> {
    let core = app.control.lock().unwrap_or_else(|e| e.into_inner());
    let u = core.state.users.get(&ns.session.user)?;
    Some(Principal {
        tenant: u.tenant.clone(),
        subject: format!("user:{}", u.id),
        scopes: u.granted(),
        token_id: None,
        session: Some(SessionRef {
            id: ns.session.id.clone(),
            user: u.id.clone(),
            hash: ns.session.hash.clone(),
            last_seen_ms: ns.session.last_seen_ms,
            idle_timeout_ms: ns.session.idle_timeout_ms,
            pending: senca_control::users::pending(u, &ns.session, app.cfg.auth_require_mfa),
        }),
    })
}

// ---------------------------------------------------------------- 封じた値

fn aad(kind: &str) -> String {
    format!("senca/auth-{kind}/v1")
}

/// 短い間だけ使う値を封じる（`exp` は UNIX ミリ秒。中身に入れる）。
pub fn seal(app: &App, kind: &str, mut v: Value, exp_ms: i64) -> Result<String, Problem> {
    v["exp"] = json!(exp_ms);
    app.seal_secret(v.to_string().as_bytes(), &aad(kind))
}

/// 開く（期限切れ・別の用途のもの・壊れたものは None）。
pub fn open(app: &App, kind: &str, sealed: &str, now_ms: i64) -> Option<Value> {
    if sealed.is_empty() || sealed.len() > 8192 {
        return None;
    }
    let b = app.open_secret(sealed, &aad(kind))?;
    let v: Value = serde_json::from_slice(&b).ok()?;
    (v.get("exp").and_then(Value::as_i64)? > now_ms).then_some(v)
}

/// 32 バイトの乱数（base64url）。
pub fn random_token() -> Result<String, Problem> {
    let s = secret::generate("").map_err(|_| Problem::internal())?;
    Ok(s.expose().to_string())
}

/// ログインの失敗の上限（IP ごと・アカウントごと）を使い切っていれば 429。
pub fn check_login_limits(app: &App, ip: &str, account: Option<&str>) -> Result<(), Problem> {
    let now = std::time::Instant::now();
    if let Some(w) = app.limits.login.exhausted(ip, now) {
        return Err(Problem::rate_limited("login", crate::ratelimit::retry_after_secs(w)));
    }
    if let Some(a) = account
        && let Some(w) = app.limits.login_accounts.check(a, now)
    {
        return Err(Problem::rate_limited("login_account", crate::ratelimit::retry_after_secs(w)));
    }
    Ok(())
}

pub fn login_failed(app: &App, ip: &str, account: Option<&str>) {
    let now = std::time::Instant::now();
    let _ = app.limits.login.take(ip, now);
    if let Some(a) = account {
        app.limits.login_accounts.fail(a, now);
    }
}

/// アカウントの鍵（テナントと小文字の利用者名。利用者がいてもいなくても同じに数える）。
pub fn account_key(tenant: &str, username: &str) -> String {
    format!("{tenant}\u{0}{}", username.to_ascii_lowercase())
}

pub fn auth_failed() -> Problem {
    Problem::new(ProblemCode::AuthFailed, "認証に失敗しました")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_interval_is_coarse() {
        assert_eq!(touch_interval_ms(8 * 3600 * 1000), 300_000);
        assert_eq!(touch_interval_ms(60_000), 15_000, "待ち時間の 1/4 まで");
        assert_eq!(touch_interval_ms(16 * 60_000), 60_000);
        assert_eq!(touch_interval_ms(10_000), 2_500);
    }
}
