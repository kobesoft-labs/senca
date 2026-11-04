//! 利用者のアカウント・セッション・OIDC の一度きりのコード（ADR-0017）。
//!
//! - 利用者はテナントごと。パスワード（Argon2id の PHC 文字列）は任意（OIDC だけの利用者は持たない）。
//! - 役割（`admin`・`operator`・`viewer`）と個別の権限の和が、利用者の権限。
//! - セッションは不透明な値（`sencas_` + 32 バイトの乱数）。状態には SHA-256 だけを残す。待ち時間（idle）と
//!   最長の寿命は、作ったときの値をセッションに持たせる（ノードの設定がずれても全ノードで同じ判定になる）。
//! - ハッシュ・乱数・暗号はコマンドを作る側（ノード）で済ませる。ここ（`apply`）は決定的な検査と置き換えだけ。

use crate::auth::Scope;
use crate::problem::{Problem, ProblemCode};
use crate::secret;
use crate::state::ControlState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use subtle::ConstantTimeEq;

/// セッションの値の接頭辞（API トークンの `senca_` と区別できる）。
pub const SESSION_PREFIX: &str = "sencas_";
pub const MAX_USERS_PER_TENANT: usize = 1000;
pub const MAX_SESSIONS_PER_USER: usize = 20;
pub const PASSWORD_MIN_CHARS: usize = 12;
pub const PASSWORD_MAX_CHARS: usize = 256;
pub const RECOVERY_CODES: usize = 10;

/// 役割（決まった権限の組）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    Operator,
    Viewer,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Admin, Role::Operator, Role::Viewer];

    pub fn parse(s: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.as_str() == s)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Operator => "operator",
            Role::Viewer => "viewer",
        }
    }

    pub fn scopes(self) -> &'static [Scope] {
        match self {
            Role::Admin => &[Scope::TenantAdmin],
            Role::Operator => &[Scope::StreamRead, Scope::StreamWrite, Scope::Publish, Scope::Subscribe, Scope::RecordingRead],
            Role::Viewer => &[Scope::StreamRead, Scope::RecordingRead, Scope::Subscribe],
        }
    }
}

/// 利用者の多要素認証（TOTP と回復コード）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMfa {
    /// TOTP の秘密（20 バイト）を秘密値と同じ鍵で封じたもの（関連データは [`totp_aad`]）。
    pub totp_sealed: String,
    pub enrolled_at_ms: i64,
    /// 最後に受け付けた TOTP の時間の区切り（同じコードを 2 度使わせない）。
    #[serde(default)]
    pub last_step: u64,
    /// 回復コードのハッシュ（[`recovery_hash`]）。使ったものは消す。
    #[serde(default)]
    pub recovery: Vec<String>,
}

/// OIDC の IdP の利用者との結び付き。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OidcLink {
    pub provider: String,
    pub subject: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub tenant: String,
    pub username: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Argon2id の PHC 文字列。無ければパスワードでは入れない。
    #[serde(default)]
    pub password_hash: Option<String>,
    #[serde(default)]
    pub roles: BTreeSet<Role>,
    #[serde(default)]
    pub scopes: BTreeSet<Scope>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub must_change_password: bool,
    #[serde(default)]
    pub mfa: Option<UserMfa>,
    #[serde(default)]
    pub oidc: Vec<OidcLink>,
    pub generation: u64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    #[serde(default)]
    pub last_login_at_ms: Option<i64>,
    #[serde(default)]
    pub password_changed_at_ms: Option<i64>,
}

impl User {
    /// 役割と個別の権限の和（`tenant:admin` の展開はしない）。
    pub fn granted(&self) -> BTreeSet<Scope> {
        let mut s = self.scopes.clone();
        for r in &self.roles {
            s.extend(r.scopes().iter().copied());
        }
        s
    }

    /// 実際に持つ権限（`tenant:admin` は `node:admin` 以外を含む）。
    pub fn effective(&self) -> BTreeSet<Scope> {
        let g = self.granted();
        Scope::ALL
            .into_iter()
            .filter(|s| g.contains(s) || (*s != Scope::NodeAdmin && g.contains(&Scope::TenantAdmin)))
            .collect()
    }

    /// 有効な管理者か（最後の管理者の検査）。
    pub fn is_admin(&self) -> bool {
        !self.disabled && self.granted().contains(&Scope::TenantAdmin)
    }
}

/// 管理者・本人が変えられる項目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserProfile {
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub roles: BTreeSet<Role>,
    pub scopes: BTreeSet<Scope>,
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub tenant: String,
    pub user: String,
    /// 値の SHA-256（16 進）。
    pub hash: String,
    /// `password` か `oidc`。
    pub method: String,
    #[serde(default)]
    pub provider: Option<String>,
    /// 2 つ目の要素（TOTP・回復コード）を通ったか。
    #[serde(default)]
    pub mfa: bool,
    pub created_at_ms: i64,
    pub last_seen_ms: i64,
    pub idle_timeout_ms: i64,
    pub expires_at_ms: i64,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
}

impl Session {
    pub fn idle_expires_at_ms(&self) -> i64 {
        self.last_seen_ms.saturating_add(self.idle_timeout_ms)
    }

    pub fn expired(&self, now_ms: i64) -> bool {
        now_ms >= self.expires_at_ms || now_ms >= self.idle_expires_at_ms()
    }
}

/// OIDC のコールバックが渡す一度きりのコード（`POST /auth/exchange` でセッションに替える）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginCode {
    /// コードの SHA-256。
    pub hash: String,
    pub tenant: String,
    pub user: String,
    pub provider: String,
    /// ログインを始めたブラウザーの印（クッキーの中の乱数）の SHA-256。別のブラウザーでは替えられない。
    pub binding_hash: String,
    #[serde(default)]
    pub return_to: Option<String>,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
}

/// ログインで一緒に記録すること（同じコマンドで適用して、二重使用・古いパスワードでのログインを防ぐ）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginRecord {
    /// パスワードで入ったとき、確かめたパスワードのハッシュ（その間に変わっていれば拒否）。
    #[serde(default)]
    pub password_hash: Option<String>,
    /// 受け付けた TOTP の区切り（前回より後でなければ拒否）。
    #[serde(default)]
    pub totp_step: Option<u64>,
    /// 使った回復コードのハッシュ（無ければ拒否、あれば消す）。
    #[serde(default)]
    pub recovery_code: Option<String>,
    /// パスワードのハッシュの作り直し（パラメーターを上げたとき）。
    #[serde(default)]
    pub rehash: Option<String>,
}

/// 制限付きのセッションで先に済ませること。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Pending {
    /// 仮のパスワード: 変えるまでほかの API は使えない。
    PasswordChange,
    /// `--auth-require-mfa`: TOTP を登録するまでほかの API は使えない。
    MfaEnrollment,
}

impl Pending {
    pub fn as_str(self) -> &'static str {
        match self {
            Pending::PasswordChange => "password_change",
            Pending::MfaEnrollment => "mfa_enrollment",
        }
    }
}

/// セッションで先に済ませること（パスワードで入ったときだけ）。
pub fn pending(user: &User, session: &Session, require_mfa: bool) -> Vec<Pending> {
    let mut v = Vec::new();
    if session.method == "password" {
        if user.must_change_password && user.password_hash.is_some() {
            v.push(Pending::PasswordChange);
        }
        if require_mfa && user.mfa.is_none() {
            v.push(Pending::MfaEnrollment);
        }
    }
    v
}

// ---------------------------------------------------------------- 検査

/// 利用者名: 1〜64 文字の ASCII の英数字と `._@+-`。大文字と小文字は区別しない（同じテナントで重ならない）。
pub fn valid_username(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'@' | b'+' | b'-'))
}

/// テナントの名前（OIDC のクレームから決めるとき）: 1〜64 文字の ASCII の英数字と `._-`。
pub fn valid_tenant(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// パスワードの決まり: 12〜256 文字（Unicode の文字数）。利用者名・メールアドレスと同じものは不可。
/// 正規化はしない（受け取った文字列のまま扱う）。
pub fn check_password_policy(password: &str, username: &str, email: Option<&str>) -> Result<(), &'static str> {
    let n = password.chars().count();
    if n < PASSWORD_MIN_CHARS {
        return Err("パスワードは 12 文字以上にしてください");
    }
    if n > PASSWORD_MAX_CHARS {
        return Err("パスワードは 256 文字までです");
    }
    if password.eq_ignore_ascii_case(username) || email.is_some_and(|e| password.eq_ignore_ascii_case(e)) {
        return Err("利用者名・メールアドレスと同じパスワードは使えません");
    }
    if password.chars().any(|c| c == '\0') {
        return Err("パスワードに NUL 文字は使えません");
    }
    Ok(())
}

// ---------------------------------------------------------------- パスワード（Argon2id）

/// Argon2id のパラメーター（OWASP の推奨の 1 つ: 19 MiB・2 回・並列 1）。PHC 文字列に入るので後から上げられる。
pub const ARGON2_M_KIB: u32 = 19_456;
pub const ARGON2_T: u32 = 2;
pub const ARGON2_P: u32 = 1;

fn argon2() -> argon2::Argon2<'static> {
    let params = argon2::Params::new(ARGON2_M_KIB, ARGON2_T, ARGON2_P, None).expect("valid argon2 params");
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
}

/// パスワードのハッシュ（PHC 文字列。塩は利用者ごとの 16 バイトの乱数）。
pub fn hash_password(password: &str) -> Result<String, Problem> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|_| Problem::internal())?;
    let salt = SaltString::encode_b64(&salt).map_err(|_| Problem::internal())?;
    argon2().hash_password(password.as_bytes(), &salt).map(|h| h.to_string()).map_err(|_| Problem::internal())
}

/// 照合する（パラメーターは PHC 文字列のもの）。形の壊れたハッシュは不一致。
pub fn verify_password(password: &str, phc: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let Ok(h) = PasswordHash::new(phc) else { return false };
    if h.algorithm.as_str() != "argon2id" {
        return false;
    }
    argon2::Argon2::default().verify_password(password.as_bytes(), &h).is_ok()
}

/// 今のパラメーターより弱いハッシュか（ログインのときに作り直す）。
pub fn needs_rehash(phc: &str) -> bool {
    let Ok(h) = argon2::password_hash::PasswordHash::new(phc) else { return false };
    let Ok(p) = argon2::Params::try_from(&h) else { return true };
    h.algorithm.as_str() != "argon2id" || p.m_cost() < ARGON2_M_KIB || p.t_cost() < ARGON2_T || p.p_cost() < ARGON2_P
}

/// 知らない利用者・パスワードの無い利用者でも同じだけ時間をかけるための、照合用のハッシュ（起動後に 1 度だけ作る）。
pub fn dummy_hash() -> &'static str {
    static H: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    H.get_or_init(|| hash_password("senca-dummy-password-for-timing").unwrap_or_default())
}

// ---------------------------------------------------------------- TOTP（RFC 6238。HOTP の計算は totp-rs）

pub const TOTP_STEP_SECS: u64 = 30;
pub const TOTP_DIGITS: usize = 6;

fn totp(secret: &[u8]) -> Option<totp_rs::TOTP> {
    totp_rs::TOTP::new(totp_rs::Algorithm::SHA1, TOTP_DIGITS, 0, TOTP_STEP_SECS, secret.to_vec()).ok()
}

/// 新しい TOTP の秘密（20 バイト = 160 bit。RFC 4226 の推奨）。
pub fn totp_new_secret() -> Result<Vec<u8>, Problem> {
    let mut b = vec![0u8; 20];
    getrandom::fill(&mut b).map_err(|_| Problem::internal())?;
    Ok(b)
}

/// 認証アプリに手で入れるための base32（パディングなし）。
pub fn totp_base32(secret: &[u8]) -> String {
    match totp_rs::Secret::Raw(secret.to_vec()).to_encoded() {
        totp_rs::Secret::Encoded(s) => s,
        totp_rs::Secret::Raw(_) => String::new(),
    }
}

/// `otpauth://totp/{issuer}:{account}?secret=...&issuer=...&algorithm=SHA1&digits=6&period=30`（QR コード用）。
pub fn totp_uri(secret: &[u8], issuer: &str, account: &str) -> String {
    format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={TOTP_DIGITS}&period={TOTP_STEP_SECS}",
        pct(issuer),
        pct(account),
        totp_base32(secret),
        pct(issuer)
    )
}

fn pct(s: &str) -> String {
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

/// その区切りのコード（試験・確認用）。
pub fn totp_code(secret: &[u8], step: u64) -> String {
    totp(secret).map(|t| t.generate(step * TOTP_STEP_SECS)).unwrap_or_default()
}

/// コードを確かめる（前後 1 区切りのずれを許す）。合った区切りが `last_step` より後なら、その区切り。
pub fn totp_check(secret: &[u8], code: &str, now_secs: i64, last_step: u64) -> Option<u64> {
    let code = code.trim();
    if code.len() != TOTP_DIGITS || !code.bytes().all(|b| b.is_ascii_digit()) || now_secs < 0 {
        return None;
    }
    let t = totp(secret)?;
    let cur = now_secs as u64 / TOTP_STEP_SECS;
    let mut hit = None;
    // どの区切りでも同じだけ計算する（合ったかどうかで時間を変えない）
    for step in [cur.saturating_sub(1), cur, cur + 1] {
        let ok: bool = t.generate(step * TOTP_STEP_SECS).as_bytes().ct_eq(code.as_bytes()).into();
        if ok && step > last_step && hit.is_none() {
            hit = Some(step);
        }
    }
    hit
}

/// TOTP の秘密を封じるときの関連データ（別の利用者・別の用途へ写しても開けない）。
pub fn totp_aad(tenant: &str, user: &str) -> String {
    format!("senca/user-totp/v1/{tenant}/{user}")
}

// ---------------------------------------------------------------- 回復コード

const RECOVERY_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

/// 回復コード 10 個（各 16 文字 = 約 79 bit、`xxxx-xxxx-xxxx-xxxx`）。
pub fn recovery_generate() -> Result<Vec<String>, Problem> {
    let mut out = Vec::new();
    for _ in 0..RECOVERY_CODES {
        let mut s = String::new();
        let mut i = 0;
        while i < 16 {
            let mut b = [0u8; 1];
            getrandom::fill(&mut b).map_err(|_| Problem::internal())?;
            // 偏りを出さないように 31 の倍数の範囲だけ使う
            if (b[0] as usize) >= 248 {
                continue;
            }
            if i > 0 && i % 4 == 0 {
                s.push('-');
            }
            s.push(RECOVERY_ALPHABET[b[0] as usize % RECOVERY_ALPHABET.len()] as char);
            i += 1;
        }
        out.push(s);
    }
    Ok(out)
}

/// 回復コードの正規化（`-`・空白を除き、小文字に）。
pub fn recovery_normalize(code: &str) -> String {
    code.chars().filter(|c| !c.is_whitespace() && *c != '-').flat_map(char::to_lowercase).collect()
}

/// 保存用のハッシュ。値は約 79 bit の乱数なので SHA-256 で足りる（利用者 ID を混ぜて別の利用者へ写せないようにする）。
pub fn recovery_hash(user: &str, code: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"senca-recovery-code/v1\0");
    h.update(user.as_bytes());
    h.update(b"\0");
    h.update(recovery_normalize(code).as_bytes());
    secret::hex(&h.finalize())
}

/// 回復コードが使えるなら、そのハッシュ（全件を定数時間で比べる）。
pub fn recovery_match(mfa: &UserMfa, user: &str, code: &str) -> Option<String> {
    let n = recovery_normalize(code);
    if n.len() != 16 {
        return None;
    }
    let h = recovery_hash(user, &n);
    let mut found = None;
    for x in &mfa.recovery {
        let ok: bool = x.as_bytes().ct_eq(h.as_bytes()).into();
        if ok && found.is_none() {
            found = Some(x.clone());
        }
    }
    found
}

// ---------------------------------------------------------------- 状態の変更

fn conflict(t: &str) -> Problem {
    Problem::new(ProblemCode::Conflict, t)
}

impl ControlState {
    pub fn tenant_user(&self, tenant: &str, id: &str) -> Result<&User, Problem> {
        self.users.get(id).filter(|u| u.tenant == tenant).ok_or_else(Problem::not_found)
    }

    pub fn user_by_name(&self, tenant: &str, username: &str) -> Option<&User> {
        self.users.values().find(|u| u.tenant == tenant && u.username.eq_ignore_ascii_case(username))
    }

    /// セッションの値から（ハッシュで引く。値は 256 bit の乱数なので、引く時間から分かることは無い）。
    pub fn session_by_value(&self, value: &str) -> Option<&Session> {
        let s = self.sessions.get(&secret::hash(value))?;
        // 念のため定数時間でも照合する
        secret::verify(value, &s.hash).then_some(s)
    }

    pub fn session_by_id(&self, tenant: &str, id: &str) -> Option<&Session> {
        self.sessions.values().find(|s| s.id == id && s.tenant == tenant)
    }

    /// 変更の後で、テナントに有効な管理者が残るか（`except` を除いた利用者、`after` を足して数える）。
    fn admins_after(&self, tenant: &str, except: &str, after: Option<&User>) -> usize {
        self.users.values().filter(|u| u.tenant == tenant && u.id != except && u.is_admin()).count() + after.is_some_and(User::is_admin) as usize
    }

    fn check_last_admin(&self, cur: &User, after: Option<&User>) -> Result<(), Problem> {
        if cur.is_admin() && self.admins_after(&cur.tenant, &cur.id, after) == 0 {
            return Err(conflict("テナントの最後の管理者は、削除・無効化・降格できません"));
        }
        Ok(())
    }

    fn revoke_user_sessions(&mut self, user: &str, except: Option<&str>) {
        self.sessions.retain(|_, s| s.user != user || except.is_some_and(|e| s.id == e));
    }

    pub(crate) fn create_user(&mut self, user: User) -> Result<(), Problem> {
        if self.users.contains_key(&user.id) {
            return Err(conflict("同じ ID の利用者がいます"));
        }
        if !valid_username(&user.username) || user.tenant.is_empty() {
            return Err(Problem::invalid("利用者名の形が違います"));
        }
        if self.user_by_name(&user.tenant, &user.username).is_some() {
            return Err(conflict("同じ利用者名がいます"));
        }
        if user.email.as_deref().is_some_and(|e| self.user_by_email(&user.tenant, e).is_some()) {
            return Err(conflict("同じメールアドレスの利用者がいます"));
        }
        if self.users.values().filter(|u| u.tenant == user.tenant).count() >= MAX_USERS_PER_TENANT {
            return Err(Problem::new(ProblemCode::QuotaExceeded, "利用者の数が上限です"));
        }
        if user.oidc.iter().any(|l| self.user_by_oidc(&user.tenant, l).is_some()) {
            return Err(conflict("この IdP の利用者は、すでに別の利用者に結び付いています"));
        }
        self.users.insert(user.id.clone(), User { generation: 1, ..user });
        Ok(())
    }

    /// メールアドレスの利用者（大文字・小文字を区別しない。テナントの中で重ならない）。
    pub fn user_by_email(&self, tenant: &str, email: &str) -> Option<&User> {
        self.users.values().find(|u| u.tenant == tenant && u.email.as_deref().is_some_and(|e| e.eq_ignore_ascii_case(email)))
    }

    pub fn user_by_oidc(&self, tenant: &str, link: &OidcLink) -> Option<&User> {
        self.users.values().find(|u| u.tenant == tenant && u.oidc.contains(link))
    }

    pub(crate) fn update_user(&mut self, tenant: &str, id: &str, p: UserProfile, expect: Option<u64>, now_ms: i64) -> Result<(), Problem> {
        let cur = self.tenant_user(tenant, id)?.clone();
        if expect.is_some_and(|g| g != cur.generation) {
            return Err(Problem::new(ProblemCode::PreconditionFailed, "資源が更新されています（If-Match が古い）"));
        }
        if let Some(e) = &p.email
            && self.user_by_email(tenant, e).is_some_and(|o| o.id != id)
        {
            return Err(conflict("同じメールアドレスの利用者がいます"));
        }
        let mut next = cur.clone();
        next.email = p.email;
        next.display_name = p.display_name;
        next.roles = p.roles;
        next.scopes = p.scopes;
        next.disabled = p.disabled;
        next.generation += 1;
        next.updated_at_ms = now_ms;
        self.check_last_admin(&cur, Some(&next))?;
        // 権限が変わった・無効にした: セッションを失効させる
        let revoke = next.disabled || next.effective() != cur.effective();
        self.users.insert(id.to_string(), next);
        if revoke {
            self.revoke_user_sessions(id, None);
        }
        Ok(())
    }

    pub(crate) fn delete_user(&mut self, tenant: &str, id: &str) -> Result<(), Problem> {
        let cur = self.tenant_user(tenant, id)?.clone();
        self.check_last_admin(&cur, None)?;
        self.users.remove(id);
        self.revoke_user_sessions(id, None);
        self.login_codes.retain(|_, c| c.user != id);
        Ok(())
    }

    pub(crate) fn set_user_password(
        &mut self,
        tenant: &str,
        id: &str,
        hash: Option<String>,
        must_change: bool,
        keep_session: Option<&str>,
        now_ms: i64,
    ) -> Result<(), Problem> {
        let u = self.users.get_mut(id).filter(|u| u.tenant == tenant).ok_or_else(Problem::not_found)?;
        if hash.as_deref().is_some_and(|h| !h.starts_with("$argon2id$")) {
            return Err(Problem::invalid("パスワードのハッシュの形が違います"));
        }
        u.password_hash = hash;
        u.must_change_password = must_change && u.password_hash.is_some();
        u.password_changed_at_ms = Some(now_ms);
        u.generation += 1;
        u.updated_at_ms = now_ms;
        self.revoke_user_sessions(id, keep_session);
        Ok(())
    }

    pub(crate) fn set_user_mfa(&mut self, tenant: &str, id: &str, mfa: Option<UserMfa>, now_ms: i64) -> Result<(), Problem> {
        let u = self.users.get_mut(id).filter(|u| u.tenant == tenant).ok_or_else(Problem::not_found)?;
        u.mfa = mfa;
        u.generation += 1;
        u.updated_at_ms = now_ms;
        Ok(())
    }

    pub(crate) fn link_user_oidc(&mut self, tenant: &str, id: &str, link: OidcLink, now_ms: i64) -> Result<(), Problem> {
        if let Some(o) = self.user_by_oidc(tenant, &link) {
            if o.id == id {
                return Ok(());
            }
            return Err(conflict("この IdP の利用者は、すでに別の利用者に結び付いています"));
        }
        let u = self.users.get_mut(id).filter(|u| u.tenant == tenant).ok_or_else(Problem::not_found)?;
        if u.oidc.len() >= 8 {
            return Err(Problem::new(ProblemCode::QuotaExceeded, "IdP との結び付きは 8 つまでです"));
        }
        u.oidc.push(link);
        u.generation += 1;
        u.updated_at_ms = now_ms;
        Ok(())
    }

    /// ログイン（セッションを作る）。利用者が有効で、ログインの記録（パスワード・TOTP・回復コード）が今の状態と合うときだけ。
    pub(crate) fn create_session(&mut self, session: Session, login: LoginRecord) -> Result<(), Problem> {
        let denied = || Problem::new(ProblemCode::AuthFailed, "認証に失敗しました");
        let u = self.users.get(&session.user).filter(|u| u.tenant == session.tenant).ok_or_else(denied)?;
        if u.disabled {
            return Err(denied());
        }
        if self.sessions.contains_key(&session.hash) || self.sessions.values().any(|s| s.id == session.id) {
            return Err(conflict("同じセッションがあります"));
        }
        if let Some(h) = &login.password_hash
            && u.password_hash.as_ref() != Some(h)
        {
            return Err(denied());
        }
        if login.totp_step.is_some() || login.recovery_code.is_some() {
            let Some(m) = &u.mfa else { return Err(denied()) };
            if login.totp_step.is_some_and(|s| s <= m.last_step) {
                return Err(conflict("この確認コードは使用済みです"));
            }
            if login.recovery_code.as_ref().is_some_and(|c| !m.recovery.contains(c)) {
                return Err(conflict("この回復コードは使用済みです"));
            }
        }
        let u = self.users.get_mut(&session.user).expect("checked above");
        if let Some(m) = u.mfa.as_mut() {
            if let Some(s) = login.totp_step {
                m.last_step = s;
            }
            if let Some(c) = &login.recovery_code {
                m.recovery.retain(|x| x != c);
            }
        }
        if let Some(h) = login.rehash
            && h.starts_with("$argon2id$")
            && u.password_hash.is_some()
        {
            u.password_hash = Some(h);
        }
        u.last_login_at_ms = Some(session.created_at_ms);
        // 利用者ごとのセッションは 20 まで（古いものから消す）
        let mut mine: Vec<(i64, String, String)> =
            self.sessions.values().filter(|s| s.user == session.user).map(|s| (s.created_at_ms, s.id.clone(), s.hash.clone())).collect();
        mine.sort();
        while mine.len() >= MAX_SESSIONS_PER_USER {
            let (_, _, h) = mine.remove(0);
            self.sessions.remove(&h);
        }
        self.sessions.insert(session.hash.clone(), session);
        Ok(())
    }

    pub(crate) fn touch_session(&mut self, hash: &str, now_ms: i64) {
        if let Some(s) = self.sessions.get_mut(hash)
            && !s.expired(now_ms)
            && now_ms > s.last_seen_ms
        {
            s.last_seen_ms = now_ms;
        }
    }

    pub(crate) fn delete_session(&mut self, tenant: &str, id: &str) -> Result<(), Problem> {
        let h = self.session_by_id(tenant, id).map(|s| s.hash.clone()).ok_or_else(Problem::not_found)?;
        self.sessions.remove(&h);
        Ok(())
    }

    pub(crate) fn delete_user_sessions(&mut self, tenant: &str, user: &str, except: Option<&str>) -> Result<(), Problem> {
        self.tenant_user(tenant, user)?;
        self.revoke_user_sessions(user, except);
        Ok(())
    }

    pub(crate) fn expire_sessions(&mut self, now_ms: i64) {
        self.sessions.retain(|_, s| !s.expired(now_ms));
        self.login_codes.retain(|_, c| c.expires_at_ms > now_ms);
    }

    pub(crate) fn put_login_code(&mut self, code: LoginCode) -> Result<(), Problem> {
        self.tenant_user(&code.tenant, &code.user)?;
        if self.login_codes.contains_key(&code.hash) {
            return Err(conflict("同じコードがあります"));
        }
        // 未使用のコードが溜まりすぎないように（期限切れは消す・1 万まで）
        let now = code.created_at_ms;
        self.login_codes.retain(|_, c| c.expires_at_ms > now);
        if self.login_codes.len() >= 10_000 {
            return Err(Problem::new(ProblemCode::QuotaExceeded, "ログインの途中のものが多すぎます").with_retryable(true));
        }
        self.login_codes.insert(code.hash.clone(), code);
        Ok(())
    }

    /// 一度きりのコードを使ってセッションにする（コードは消える。期限切れ・別のブラウザー・使用済みは 401）。
    pub(crate) fn redeem_login_code(&mut self, hash: &str, binding_hash: &str, now_ms: i64, session: Session) -> Result<(), Problem> {
        let denied = || Problem::new(ProblemCode::AuthFailed, "認証に失敗しました");
        let c = self.login_codes.get(hash).cloned().ok_or_else(denied)?;
        let bind_ok: bool = c.binding_hash.as_bytes().ct_eq(binding_hash.as_bytes()).into();
        if c.expires_at_ms <= now_ms || !bind_ok || c.user != session.user || c.tenant != session.tenant {
            if c.expires_at_ms <= now_ms {
                self.login_codes.remove(hash);
            }
            return Err(denied());
        }
        self.create_session(session, LoginRecord::default())?;
        self.login_codes.remove(hash);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Command;

    fn user(id: &str, tenant: &str, name: &str, roles: &[Role]) -> User {
        User {
            id: id.into(),
            tenant: tenant.into(),
            username: name.into(),
            email: None,
            display_name: None,
            password_hash: None,
            roles: roles.iter().copied().collect(),
            scopes: BTreeSet::new(),
            disabled: false,
            must_change_password: false,
            mfa: None,
            oidc: vec![],
            generation: 0,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_login_at_ms: None,
            password_changed_at_ms: None,
        }
    }

    fn session(id: &str, tenant: &str, user: &str, at: i64) -> Session {
        Session {
            id: id.into(),
            tenant: tenant.into(),
            user: user.into(),
            hash: format!("h-{id}"),
            method: "password".into(),
            provider: None,
            mfa: false,
            created_at_ms: at,
            last_seen_ms: at,
            idle_timeout_ms: 100,
            expires_at_ms: at + 1000,
            ip: None,
            user_agent: None,
        }
    }

    fn profile(u: &User) -> UserProfile {
        UserProfile { email: u.email.clone(), display_name: u.display_name.clone(), roles: u.roles.clone(), scopes: u.scopes.clone(), disabled: u.disabled }
    }

    #[test]
    fn roles_and_effective_scopes() {
        let mut u = user("u", "t", "a", &[Role::Viewer]);
        assert_eq!(u.effective(), [Scope::StreamRead, Scope::Subscribe, Scope::RecordingRead].into_iter().collect());
        u.scopes.insert(Scope::RecordingDelete);
        assert!(u.effective().contains(&Scope::RecordingDelete));
        assert!(!u.is_admin());
        u.roles.insert(Role::Admin);
        assert!(u.is_admin() && !u.effective().contains(&Scope::NodeAdmin) && u.effective().contains(&Scope::StreamWrite));
        u.disabled = true;
        assert!(!u.is_admin());
        assert_eq!(Role::parse("operator"), Some(Role::Operator));
        assert_eq!(Role::parse("root"), None);
    }

    #[test]
    fn user_commands_and_uniqueness() {
        let mut st = ControlState::default();
        st.apply(Command::CreateUser { user: user("u1", "t", "Alice", &[Role::Admin]) }).unwrap();
        assert_eq!(st.users["u1"].generation, 1);
        // 大文字・小文字を区別しない
        assert_eq!(st.apply(Command::CreateUser { user: user("u2", "t", "alice", &[]) }).unwrap_err().status, 409);
        // 別のテナントなら同じ名前でよい
        st.apply(Command::CreateUser { user: user("u2", "t2", "alice", &[]) }).unwrap();
        assert_eq!(st.apply(Command::CreateUser { user: user("u3", "t", "bad name", &[]) }).unwrap_err().status, 400);
        // メールアドレスもテナントの中で重ならない
        let mut e = user("u4", "t", "e1", &[]);
        e.email = Some("E@x.org".into());
        st.apply(Command::CreateUser { user: e.clone() }).unwrap();
        e.id = "u5".into();
        e.username = "e2".into();
        e.email = Some("e@X.org".into());
        assert_eq!(st.apply(Command::CreateUser { user: e }).unwrap_err().status, 409);
        let mut pe = profile(&st.users["u1"]);
        pe.email = Some("e@x.org".into());
        assert_eq!(
            st.apply(Command::UpdateUser { tenant: "t".into(), id: "u1".into(), profile: pe, expect_generation: None, now_ms: 2 }).unwrap_err().status,
            409
        );
        // 別テナントからは 404
        assert_eq!(st.tenant_user("t2", "u1").unwrap_err().status, 404);
        assert_eq!(st.apply(Command::DeleteUser { tenant: "t2".into(), id: "u1".into() }).unwrap_err().status, 404);
        // If-Match
        let mut p = profile(&st.users["u1"]);
        p.display_name = Some("A".into());
        assert_eq!(
            st.apply(Command::UpdateUser { tenant: "t".into(), id: "u1".into(), profile: p.clone(), expect_generation: Some(9), now_ms: 5 })
                .unwrap_err()
                .status,
            412
        );
        st.apply(Command::UpdateUser { tenant: "t".into(), id: "u1".into(), profile: p, expect_generation: Some(1), now_ms: 5 }).unwrap();
        assert_eq!((st.users["u1"].generation, st.users["u1"].updated_at_ms), (2, 5));
    }

    #[test]
    fn last_admin_is_protected() {
        let mut st = ControlState::default();
        st.apply(Command::CreateUser { user: user("a", "t", "a", &[Role::Admin]) }).unwrap();
        st.apply(Command::CreateUser { user: user("v", "t", "v", &[Role::Viewer]) }).unwrap();
        // 削除・無効化・降格は 409
        assert_eq!(st.apply(Command::DeleteUser { tenant: "t".into(), id: "a".into() }).unwrap_err().status, 409);
        let mut p = profile(&st.users["a"]);
        p.disabled = true;
        assert_eq!(
            st.apply(Command::UpdateUser { tenant: "t".into(), id: "a".into(), profile: p, expect_generation: None, now_ms: 2 }).unwrap_err().status,
            409
        );
        let mut p = profile(&st.users["a"]);
        p.roles = [Role::Operator].into_iter().collect();
        assert_eq!(
            st.apply(Command::UpdateUser { tenant: "t".into(), id: "a".into(), profile: p.clone(), expect_generation: None, now_ms: 2 })
                .unwrap_err()
                .status,
            409
        );
        // 個別の権限で tenant:admin を持たせても管理者
        let mut pv = profile(&st.users["v"]);
        pv.scopes.insert(Scope::TenantAdmin);
        st.apply(Command::UpdateUser { tenant: "t".into(), id: "v".into(), profile: pv, expect_generation: None, now_ms: 3 }).unwrap();
        st.apply(Command::UpdateUser { tenant: "t".into(), id: "a".into(), profile: p, expect_generation: None, now_ms: 4 }).unwrap();
        st.apply(Command::DeleteUser { tenant: "t".into(), id: "a".into() }).unwrap();
        assert_eq!(st.apply(Command::DeleteUser { tenant: "t".into(), id: "v".into() }).unwrap_err().status, 409);
        // 管理者のいないテナントでは制限しない
        st.apply(Command::CreateUser { user: user("x", "t3", "x", &[Role::Viewer]) }).unwrap();
        st.apply(Command::DeleteUser { tenant: "t3".into(), id: "x".into() }).unwrap();
    }

    #[test]
    fn session_lifecycle() {
        let mut st = ControlState::default();
        let mut u = user("u", "t", "u", &[Role::Operator]);
        u.password_hash = Some("$argon2id$a".into());
        st.apply(Command::CreateUser { user: u }).unwrap();
        st.apply(Command::CreateUser { user: user("w", "t", "w", &[Role::Admin]) }).unwrap();
        // 確かめたパスワードが今のものと違えば拒否（その間に変わった）
        let rec = |h: &str| LoginRecord { password_hash: Some(h.into()), ..Default::default() };
        assert_eq!(st.apply(Command::CreateSession { session: session("s0", "t", "u", 10), login: rec("$argon2id$old") }).unwrap_err().status, 401);
        st.apply(Command::CreateSession { session: session("s1", "t", "u", 10), login: rec("$argon2id$a") }).unwrap();
        st.apply(Command::CreateSession { session: session("s2", "t", "u", 20), login: rec("$argon2id$a") }).unwrap();
        assert_eq!(st.users["u"].last_login_at_ms, Some(20));
        // 別テナントのセッションは消せない
        assert_eq!(st.apply(Command::DeleteSession { tenant: "x".into(), id: "s1".into() }).unwrap_err().status, 404);
        // 待ち時間（100 ms）と寿命（1000 ms）
        let s1 = st.sessions["h-s1"].clone();
        assert!(!s1.expired(109) && s1.expired(110));
        st.apply(Command::TouchSession { hash: "h-s1".into(), now_ms: 100 }).unwrap();
        assert!(!st.sessions["h-s1"].expired(150));
        // 時刻は戻さない
        st.apply(Command::TouchSession { hash: "h-s1".into(), now_ms: 50 }).unwrap();
        assert_eq!(st.sessions["h-s1"].last_seen_ms, 100);
        st.apply(Command::ExpireSessions { now_ms: 125 }).unwrap();
        assert!(st.sessions.contains_key("h-s1") && !st.sessions.contains_key("h-s2"));
        // パスワードを変えると、ほかのセッションは失効（変えたセッションは残せる）
        st.apply(Command::CreateSession { session: session("s3", "t", "u", 120), login: rec("$argon2id$a") }).unwrap();
        st.apply(Command::SetUserPassword {
            tenant: "t".into(),
            id: "u".into(),
            password_hash: Some("$argon2id$b".into()),
            must_change_password: false,
            keep_session: Some("s3".into()),
            now_ms: 130,
        })
        .unwrap();
        assert!(!st.sessions.contains_key("h-s1") && st.sessions.contains_key("h-s3"));
        // 権限の変更・無効化でも失効
        let mut p = profile(&st.users["u"]);
        p.roles = [Role::Viewer].into_iter().collect();
        st.apply(Command::UpdateUser { tenant: "t".into(), id: "u".into(), profile: p.clone(), expect_generation: None, now_ms: 140 }).unwrap();
        assert!(st.sessions.is_empty());
        // 表示名だけなら失効させない
        st.apply(Command::CreateSession { session: session("s4", "t", "u", 150), login: rec("$argon2id$b") }).unwrap();
        p.display_name = Some("U".into());
        st.apply(Command::UpdateUser { tenant: "t".into(), id: "u".into(), profile: p.clone(), expect_generation: None, now_ms: 150 }).unwrap();
        assert_eq!(st.sessions.len(), 1);
        p.disabled = true;
        st.apply(Command::UpdateUser { tenant: "t".into(), id: "u".into(), profile: p, expect_generation: None, now_ms: 160 }).unwrap();
        assert!(st.sessions.is_empty());
        // 無効な利用者はログインできない
        assert_eq!(st.apply(Command::CreateSession { session: session("s5", "t", "u", 170), login: rec("$argon2id$b") }).unwrap_err().status, 401);
        // 削除でも失効
        st.apply(Command::CreateSession { session: session("s6", "t", "w", 170), login: LoginRecord::default() }).unwrap();
        st.apply(Command::CreateUser { user: user("w2", "t", "w2", &[Role::Admin]) }).unwrap();
        st.apply(Command::DeleteUser { tenant: "t".into(), id: "w".into() }).unwrap();
        assert!(st.sessions.is_empty());
    }

    #[test]
    fn sessions_per_user_are_bounded() {
        let mut st = ControlState::default();
        st.apply(Command::CreateUser { user: user("u", "t", "u", &[]) }).unwrap();
        for i in 0..(MAX_SESSIONS_PER_USER + 3) {
            st.apply(Command::CreateSession { session: session(&format!("s{i:02}"), "t", "u", i as i64), login: LoginRecord::default() }).unwrap();
        }
        assert_eq!(st.sessions.len(), MAX_SESSIONS_PER_USER);
        assert!(!st.sessions.contains_key("h-s00") && st.sessions.contains_key(&format!("h-s{:02}", MAX_SESSIONS_PER_USER + 2)));
    }

    #[test]
    fn mfa_steps_and_recovery_codes_are_single_use() {
        let mut st = ControlState::default();
        st.apply(Command::CreateUser { user: user("u", "t", "u", &[]) }).unwrap();
        let mfa = UserMfa { totp_sealed: "x".into(), enrolled_at_ms: 1, last_step: 0, recovery: vec!["r1".into(), "r2".into()] };
        st.apply(Command::SetUserMfa { tenant: "t".into(), id: "u".into(), mfa: Some(mfa), now_ms: 1 }).unwrap();
        let totp = |s| LoginRecord { totp_step: Some(s), ..Default::default() };
        st.apply(Command::CreateSession { session: session("a", "t", "u", 1), login: totp(5) }).unwrap();
        assert_eq!(st.apply(Command::CreateSession { session: session("b", "t", "u", 2), login: totp(5) }).unwrap_err().status, 409);
        assert_eq!(st.apply(Command::CreateSession { session: session("b", "t", "u", 2), login: totp(4) }).unwrap_err().status, 409);
        st.apply(Command::CreateSession { session: session("b", "t", "u", 2), login: totp(6) }).unwrap();
        let rc = |c: &str| LoginRecord { recovery_code: Some(c.into()), ..Default::default() };
        st.apply(Command::CreateSession { session: session("c", "t", "u", 3), login: rc("r1") }).unwrap();
        assert_eq!(st.apply(Command::CreateSession { session: session("d", "t", "u", 4), login: rc("r1") }).unwrap_err().status, 409);
        assert_eq!(st.users["u"].mfa.as_ref().unwrap().recovery, vec!["r2".to_string()]);
        // MFA の無い利用者に MFA の記録は通らない
        st.apply(Command::SetUserMfa { tenant: "t".into(), id: "u".into(), mfa: None, now_ms: 5 }).unwrap();
        assert_eq!(st.apply(Command::CreateSession { session: session("e", "t", "u", 5), login: totp(9) }).unwrap_err().status, 401);
    }

    #[test]
    fn login_codes_are_single_use_and_bound() {
        let mut st = ControlState::default();
        st.apply(Command::CreateUser { user: user("u", "t", "u", &[]) }).unwrap();
        let code = LoginCode {
            hash: "c1".into(),
            tenant: "t".into(),
            user: "u".into(),
            provider: "p".into(),
            binding_hash: "b1".into(),
            return_to: None,
            created_at_ms: 0,
            expires_at_ms: 60_000,
        };
        st.apply(Command::PutLoginCode { code: code.clone() }).unwrap();
        let redeem = |st: &mut ControlState, b: &str, now: i64, id: &str| {
            st.apply(Command::RedeemLoginCode { hash: "c1".into(), binding_hash: b.into(), now_ms: now, session: session(id, "t", "u", now) })
        };
        assert_eq!(redeem(&mut st, "other-browser", 10, "s1").unwrap_err().status, 401);
        redeem(&mut st, "b1", 10, "s1").unwrap();
        assert_eq!(redeem(&mut st, "b1", 11, "s2").unwrap_err().status, 401, "2 度目は使えない");
        st.apply(Command::PutLoginCode { code: code.clone() }).unwrap();
        assert_eq!(redeem(&mut st, "b1", 60_000, "s3").unwrap_err().status, 401, "期限切れ");
        assert!(st.login_codes.is_empty());
        st.apply(Command::PutLoginCode { code }).unwrap();
        st.apply(Command::ExpireSessions { now_ms: 60_001 }).unwrap();
        assert!(st.login_codes.is_empty());
    }

    #[test]
    fn oidc_links_are_unique() {
        let mut st = ControlState::default();
        st.apply(Command::CreateUser { user: user("a", "t", "a", &[]) }).unwrap();
        st.apply(Command::CreateUser { user: user("b", "t", "b", &[]) }).unwrap();
        let l = OidcLink { provider: "g".into(), subject: "123".into() };
        st.apply(Command::LinkUserOidc { tenant: "t".into(), id: "a".into(), link: l.clone(), now_ms: 1 }).unwrap();
        st.apply(Command::LinkUserOidc { tenant: "t".into(), id: "a".into(), link: l.clone(), now_ms: 1 }).unwrap();
        assert_eq!(st.users["a"].oidc.len(), 1);
        assert_eq!(st.apply(Command::LinkUserOidc { tenant: "t".into(), id: "b".into(), link: l.clone(), now_ms: 1 }).unwrap_err().status, 409);
        let mut c = user("c", "t", "c", &[]);
        c.oidc.push(l);
        assert_eq!(st.apply(Command::CreateUser { user: c }).unwrap_err().status, 409);
    }

    #[test]
    fn password_policy() {
        assert!(check_password_policy("short", "u", None).is_err());
        assert!(check_password_policy("correct horse battery", "u", None).is_ok());
        assert!(check_password_policy("alice-example1", "Alice-Example1", None).is_err());
        assert!(check_password_policy("a@example.com", "x", Some("A@example.com")).is_err());
        // 文字数で数える（12 文字の日本語は通る）
        assert!(check_password_policy("あいうえおかきくけこさし", "u", None).is_ok());
        assert!(check_password_policy("あいうえおかきくけこさ", "u", None).is_err());
        assert!(check_password_policy(&"x".repeat(257), "u", None).is_err());
        assert!(check_password_policy(&"x".repeat(256), "u", None).is_ok());
    }

    #[test]
    fn argon2_hash_and_verify() {
        let h = hash_password("correct horse battery").unwrap();
        assert!(h.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"), "{h}");
        assert!(verify_password("correct horse battery", &h));
        assert!(!verify_password("correct horse batterY", &h));
        assert!(!verify_password("x", "not a hash"));
        // 塩は毎回違う
        assert_ne!(h, hash_password("correct horse battery").unwrap());
        assert!(!needs_rehash(&h));
        // 弱いパラメーターのハッシュは作り直す
        use argon2::password_hash::{PasswordHasher, SaltString};
        let weak = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, argon2::Params::new(8, 1, 1, None).unwrap())
            .hash_password(b"pw", &SaltString::encode_b64(&[1u8; 16]).unwrap())
            .unwrap()
            .to_string();
        assert!(verify_password("pw", &weak) && needs_rehash(&weak));
        assert!(!dummy_hash().is_empty());
    }

    #[test]
    fn totp_rfc6238_vectors_and_window() {
        // RFC 6238 付録 B（SHA-1、秘密 "12345678901234567890"）の 8 桁の値の下 6 桁
        let secret = b"12345678901234567890";
        assert_eq!(totp_code(secret, 59 / 30), "287082");
        assert_eq!(totp_code(secret, 1111111109 / 30), "081804");
        assert_eq!(totp_code(secret, 1234567890 / 30), "005924");
        assert_eq!(totp_code(secret, 2000000000 / 30), "279037");
        let now = 1234567890;
        let cur = now as u64 / 30;
        assert_eq!(totp_check(secret, "005924", now, 0), Some(cur));
        // 前後 1 区切りは通る、2 区切りは通らない
        assert_eq!(totp_check(secret, &totp_code(secret, cur - 1), now, 0), Some(cur - 1));
        assert_eq!(totp_check(secret, &totp_code(secret, cur + 1), now, 0), Some(cur + 1));
        assert_eq!(totp_check(secret, &totp_code(secret, cur + 2), now, 0), None);
        // 使った区切り以前は通らない（再使用の防止）
        assert_eq!(totp_check(secret, "005924", now, cur), None);
        assert_eq!(totp_check(secret, "00592", now, 0), None);
        assert_eq!(totp_check(secret, "abcdef", now, 0), None);
        assert_eq!(totp_base32(secret), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        let uri = totp_uri(secret, "SENCA", "alice@acme");
        assert!(uri.starts_with("otpauth://totp/SENCA:alice%40acme?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&issuer=SENCA"), "{uri}");
    }

    #[test]
    fn recovery_codes() {
        let codes = recovery_generate().unwrap();
        assert_eq!(codes.len(), RECOVERY_CODES);
        assert!(codes.iter().all(|c| c.len() == 19 && c.matches('-').count() == 3));
        let mfa = UserMfa { totp_sealed: String::new(), enrolled_at_ms: 0, last_step: 0, recovery: codes.iter().map(|c| recovery_hash("u", c)).collect() };
        // 大文字・空白・区切りの違いは許す
        let typed = codes[3].to_uppercase().replace('-', " ");
        assert_eq!(recovery_match(&mfa, "u", &typed), Some(recovery_hash("u", &codes[3])));
        assert_eq!(recovery_match(&mfa, "other-user", &codes[3]), None);
        assert_eq!(recovery_match(&mfa, "u", "aaaa-aaaa-aaaa-aaaa"), None);
    }

    #[test]
    fn pending_requirements() {
        let mut u = user("u", "t", "u", &[]);
        let mut s = session("s", "t", "u", 0);
        assert!(pending(&u, &s, false).is_empty());
        u.must_change_password = true;
        assert!(pending(&u, &s, false).is_empty(), "パスワードの無い利用者には求めない");
        u.password_hash = Some("$argon2id$x".into());
        assert_eq!(pending(&u, &s, true), vec![Pending::PasswordChange, Pending::MfaEnrollment]);
        // OIDC のセッションには求めない（IdP の側の決まり）
        s.method = "oidc".into();
        assert!(pending(&u, &s, true).is_empty());
    }
}
