//! 認証（RFC 2326 は HTTP の認証を使う）。Digest（RFC 2617・RFC 7616: MD5・SHA-256、`-sess`、
//! qop=auth と qop なしの旧形式）と Basic（RFC 7617）。
//!
//! Basic は平文と同じなので、クライアントは呼び出し側が明示的に許したときだけ使う（仕様 6.6）。
//! パスワードは [`Credentials`] の Debug・エラーに出さない。

use crate::error::{Error, Result};
use base64::Engine;
use md5::Md5;
use sha2::{Digest as _, Sha256};
use std::fmt;
use subtle::ConstantTimeEq;

/// 利用者名とパスワード。Debug ではパスワードを出さない。
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    password: String,
}

impl Credentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Credentials {
            username: username.into(),
            password: password.into(),
        }
    }

    pub fn password(&self) -> &str {
        &self.password
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Digest のハッシュ方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Algorithm {
    Md5,
    Md5Sess,
    Sha256,
    Sha256Sess,
}

impl Algorithm {
    pub fn parse(s: &str) -> Option<Algorithm> {
        Some(match s.to_ascii_uppercase().as_str() {
            "MD5" => Algorithm::Md5,
            "MD5-SESS" => Algorithm::Md5Sess,
            "SHA-256" => Algorithm::Sha256,
            "SHA-256-SESS" => Algorithm::Sha256Sess,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Algorithm::Md5 => "MD5",
            Algorithm::Md5Sess => "MD5-sess",
            Algorithm::Sha256 => "SHA-256",
            Algorithm::Sha256Sess => "SHA-256-sess",
        }
    }

    fn is_sess(self) -> bool {
        matches!(self, Algorithm::Md5Sess | Algorithm::Sha256Sess)
    }

    /// 強さの順位（大きいほど優先）。
    fn strength(self) -> u8 {
        match self {
            Algorithm::Md5 | Algorithm::Md5Sess => 1,
            Algorithm::Sha256 | Algorithm::Sha256Sess => 2,
        }
    }

    fn hash(self, data: &str) -> String {
        match self {
            Algorithm::Md5 | Algorithm::Md5Sess => hex(&Md5::digest(data.as_bytes())),
            Algorithm::Sha256 | Algorithm::Sha256Sess => hex(&Sha256::digest(data.as_bytes())),
        }
    }
}

fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

/// Digest の challenge。
#[derive(Clone, PartialEq, Eq)]
pub struct DigestChallenge {
    pub realm: String,
    pub nonce: String,
    pub opaque: Option<String>,
    pub algorithm: Algorithm,
    /// `algorithm` を明示していたか（応答でも同じ形にする）。
    pub algorithm_explicit: bool,
    /// qop に `auth` が含まれる。偽なら RFC 2069 形式（qop なし）で応答する。
    pub qop_auth: bool,
    pub stale: bool,
}

impl fmt::Debug for DigestChallenge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DigestChallenge")
            .field("realm", &self.realm)
            .field("algorithm", &self.algorithm)
            .field("qop_auth", &self.qop_auth)
            .field("stale", &self.stale)
            .finish_non_exhaustive()
    }
}

/// WWW-Authenticate の 1 つの challenge。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Challenge {
    Basic {
        realm: String,
    },
    Digest(DigestChallenge),
    /// 未対応の方式（方式名だけ）。
    Other(String),
}

const MAX_CHALLENGES: usize = 16;
const MAX_PARAMS: usize = 32;
const MAX_VALUE: usize = 1024;

struct Lexer<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Lexer<'a> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t') {
            self.i += 1;
        }
    }
    fn ws_commas(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b',') {
            self.i += 1;
        }
    }
    fn at_end(&self) -> bool {
        self.i >= self.s.len()
    }
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn token(&mut self) -> Option<&'a str> {
        let st = self.i;
        while self.i < self.s.len() && crate::message::is_token_char(self.s[self.i]) {
            self.i += 1;
        }
        if self.i == st || self.i - st > MAX_VALUE {
            return None;
        }
        std::str::from_utf8(&self.s[st..self.i]).ok()
    }
    /// token68（`1*( ALPHA / DIGIT / "-" / "." / "_" / "~" / "+" / "/" ) *"="`）が
    /// 単独で現れていれば読む。そうでなければ位置を戻して `None`。
    fn try_token68(&mut self) -> Option<String> {
        let st = self.i;
        while self.i < self.s.len()
            && (self.s[self.i].is_ascii_alphanumeric() || b"-._~+/".contains(&self.s[self.i]))
        {
            self.i += 1;
        }
        if self.i == st || self.i - st > MAX_VALUE {
            self.i = st;
            return None;
        }
        while self.peek() == Some(b'=') {
            self.i += 1;
        }
        let end = self.i;
        self.ws();
        if self.at_end() || self.peek() == Some(b',') {
            return std::str::from_utf8(&self.s[st..end])
                .ok()
                .map(str::to_string);
        }
        self.i = st;
        None
    }
    fn quoted(&mut self) -> Result<String> {
        if self.peek() != Some(b'"') {
            return Err(Error::Malformed("引用符"));
        }
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let b = self.peek().ok_or(Error::Malformed("引用符が閉じない"))?;
            self.i += 1;
            match b {
                b'"' => break,
                b'\\' => {
                    let c = self.peek().ok_or(Error::Malformed("引用符が閉じない"))?;
                    self.i += 1;
                    out.push(c);
                }
                b if b < b' ' && b != b'\t' => return Err(Error::Malformed("引用符内の制御文字")),
                b => out.push(b),
            }
            if out.len() > MAX_VALUE {
                return Err(Error::LimitExceeded("認証の値"));
            }
        }
        String::from_utf8(out).map_err(|_| Error::Malformed("UTF-8 ではない値"))
    }
    fn value(&mut self) -> Result<String> {
        if self.peek() == Some(b'"') {
            self.quoted()
        } else {
            self.token()
                .map(str::to_string)
                .ok_or(Error::Malformed("認証の値"))
        }
    }
}

/// 方式名と auth-param（名前は小文字）。
type AuthItem = (String, Vec<(String, String)>);

/// 方式名と auth-param の並びを解釈する。1 つの値に複数の challenge が並んでいてもよい。
/// 方式の直後の token68（Basic の資格情報など）は名前 "" の引数として持つ。
fn parse_auth_list(v: &str) -> Result<Vec<AuthItem>> {
    let mut lx = Lexer {
        s: v.as_bytes(),
        i: 0,
    };
    let mut out: Vec<AuthItem> = Vec::new();
    loop {
        lx.ws_commas();
        if lx.at_end() {
            break;
        }
        let scheme = lx.token().ok_or(Error::Malformed("認証方式"))?.to_string();
        if out.len() >= MAX_CHALLENGES {
            return Err(Error::LimitExceeded("challenge の数"));
        }
        let mut params = Vec::new();
        lx.ws();
        if let Some(t) = lx.try_token68() {
            params.push((String::new(), t));
        }
        loop {
            let save = lx.i;
            lx.ws_commas();
            if lx.at_end() {
                break;
            }
            let Some(name) = lx.token() else {
                return Err(Error::Malformed("認証の引数"));
            };
            lx.ws();
            if lx.peek() != Some(b'=') {
                // 次の challenge の方式名
                lx.i = save;
                break;
            }
            lx.i += 1;
            lx.ws();
            let val = lx.value()?;
            if params.len() >= MAX_PARAMS {
                return Err(Error::LimitExceeded("認証の引数"));
            }
            params.push((name.to_ascii_lowercase(), val));
        }
        out.push((scheme, params));
    }
    Ok(out)
}

fn param<'a>(p: &'a [(String, String)], name: &str) -> Option<&'a str> {
    p.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

/// WWW-Authenticate ヘッダー（複数可）から challenge を取り出す。壊れたヘッダー値は飛ばす。
pub fn parse_challenges<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<Challenge> {
    let mut out = Vec::new();
    for v in values {
        let Ok(list) = parse_auth_list(v) else {
            continue;
        };
        for (scheme, params) in list {
            if out.len() >= MAX_CHALLENGES {
                return out;
            }
            if scheme.eq_ignore_ascii_case("basic") {
                out.push(Challenge::Basic {
                    realm: param(&params, "realm").unwrap_or("").to_string(),
                });
            } else if scheme.eq_ignore_ascii_case("digest") {
                let (Some(realm), Some(nonce)) = (param(&params, "realm"), param(&params, "nonce"))
                else {
                    continue;
                };
                let (algorithm, explicit) = match param(&params, "algorithm") {
                    None => (Algorithm::Md5, false),
                    Some(a) => match Algorithm::parse(a) {
                        Some(a) => (a, true),
                        None => {
                            out.push(Challenge::Other(format!("Digest/{a}")));
                            continue;
                        }
                    },
                };
                let qop = param(&params, "qop");
                let qop_auth = qop
                    .is_some_and(|q| q.split(',').any(|x| x.trim().eq_ignore_ascii_case("auth")));
                if qop.is_some() && !qop_auth {
                    // auth-int だけは扱わない
                    out.push(Challenge::Other("Digest/auth-int".into()));
                    continue;
                }
                out.push(Challenge::Digest(DigestChallenge {
                    realm: realm.to_string(),
                    nonce: nonce.to_string(),
                    opaque: param(&params, "opaque").map(str::to_string),
                    algorithm,
                    algorithm_explicit: explicit,
                    qop_auth,
                    stale: param(&params, "stale").is_some_and(|s| s.eq_ignore_ascii_case("true")),
                }));
            } else {
                out.push(Challenge::Other(scheme));
            }
        }
    }
    out
}

/// 使う challenge を選ぶ。Digest の SHA-256 > MD5、Basic は `allow_basic` のときだけ。
pub fn select_challenge(challenges: &[Challenge], allow_basic: bool) -> Option<&Challenge> {
    let digest = challenges
        .iter()
        .filter_map(|c| match c {
            Challenge::Digest(d) => Some((c, d.algorithm.strength())),
            _ => None,
        })
        .max_by_key(|(_, s)| *s)
        .map(|(c, _)| c);
    digest.or_else(|| {
        allow_basic
            .then(|| {
                challenges
                    .iter()
                    .find(|c| matches!(c, Challenge::Basic { .. }))
            })
            .flatten()
    })
}

/// Digest の response を計算する（RFC 7616 3.4.1）。`qop` が `None` なら RFC 2069 形式。
#[allow(clippy::too_many_arguments)]
pub fn digest_response(
    algorithm: Algorithm,
    username: &str,
    realm: &str,
    password: &str,
    nonce: &str,
    nc: &str,
    cnonce: &str,
    qop: Option<&str>,
    method: &str,
    uri: &str,
) -> String {
    let mut ha1 = algorithm.hash(&format!("{username}:{realm}:{password}"));
    if algorithm.is_sess() {
        ha1 = algorithm.hash(&format!("{ha1}:{nonce}:{cnonce}"));
    }
    let ha2 = algorithm.hash(&format!("{method}:{uri}"));
    match qop {
        Some(q) => algorithm.hash(&format!("{ha1}:{nonce}:{nc}:{cnonce}:{q}:{ha2}")),
        None => algorithm.hash(&format!("{ha1}:{nonce}:{ha2}")),
    }
}

fn quote(s: &str) -> Result<String> {
    if s.bytes().any(|b| b < b' ' || b == 0x7f) {
        return Err(Error::Auth("認証の値に制御文字"));
    }
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            o.push('\\');
        }
        o.push(c);
    }
    o.push('"');
    Ok(o)
}

/// Digest の Authorization ヘッダー値を作る。`nc` は 1 から数える。
pub fn digest_authorization(
    cred: &Credentials,
    ch: &DigestChallenge,
    method: &str,
    uri: &str,
    nc: u32,
    cnonce: &str,
) -> Result<String> {
    let ncs = format!("{nc:08x}");
    let qop = ch.qop_auth.then_some("auth");
    let resp = digest_response(
        ch.algorithm,
        &cred.username,
        &ch.realm,
        cred.password(),
        &ch.nonce,
        &ncs,
        cnonce,
        qop,
        method,
        uri,
    );
    let mut s = format!(
        "Digest username={}, realm={}, nonce={}, uri={}, response=\"{resp}\"",
        quote(&cred.username)?,
        quote(&ch.realm)?,
        quote(&ch.nonce)?,
        quote(uri)?
    );
    if ch.algorithm_explicit || ch.algorithm != Algorithm::Md5 {
        s.push_str(&format!(", algorithm={}", ch.algorithm.as_str()));
    }
    if let Some(o) = &ch.opaque {
        s.push_str(&format!(", opaque={}", quote(o)?));
    }
    if qop.is_some() {
        s.push_str(&format!(", qop=auth, nc={ncs}, cnonce={}", quote(cnonce)?));
    }
    Ok(s)
}

/// Basic の Authorization ヘッダー値。
pub fn basic_authorization(cred: &Credentials) -> Result<String> {
    if cred.username.contains(':') {
        return Err(Error::Auth("Basic の利用者名に ':'"));
    }
    let raw = format!("{}:{}", cred.username, cred.password());
    Ok(format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    ))
}

/// クライアントの認証状態（選んだ challenge と nonce count）。
#[derive(Debug, Clone)]
pub struct ClientAuth {
    challenge: Challenge,
    nc: u32,
}

impl ClientAuth {
    /// challenge の一覧から方針に従って選ぶ。受け入れられるものがなければ `Err`。
    pub fn from_challenges(challenges: &[Challenge], allow_basic: bool) -> Result<ClientAuth> {
        let c = select_challenge(challenges, allow_basic).ok_or(Error::Auth(
            "受け入れられる認証方式がない（Basic は明示的に許可したときだけ）",
        ))?;
        Ok(ClientAuth {
            challenge: c.clone(),
            nc: 0,
        })
    }

    pub fn challenge(&self) -> &Challenge {
        &self.challenge
    }

    /// cnonce が要るか（qop=auth または -sess）。
    pub fn needs_cnonce(&self) -> bool {
        matches!(&self.challenge, Challenge::Digest(d) if d.qop_auth || d.algorithm.is_sess())
    }

    /// 1 つの要求の Authorization ヘッダー値。
    pub fn authorization(
        &mut self,
        cred: &Credentials,
        method: &str,
        uri: &str,
        cnonce: &str,
    ) -> Result<String> {
        match &self.challenge {
            Challenge::Basic { .. } => basic_authorization(cred),
            Challenge::Digest(d) => {
                self.nc = self.nc.checked_add(1).ok_or(Error::Auth("nonce count"))?;
                digest_authorization(cred, d, method, uri, self.nc, cnonce)
            }
            Challenge::Other(_) => Err(Error::Auth("未対応の認証方式")),
        }
    }
}

/// 要求の Authorization ヘッダー（サーバー側で解釈したもの）。
#[derive(Clone, PartialEq, Eq)]
pub enum Authorization {
    Basic { username: String, password: String },
    Digest(DigestParams),
}

impl fmt::Debug for Authorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Authorization::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .finish_non_exhaustive(),
            Authorization::Digest(d) => f
                .debug_struct("Digest")
                .field("username", &d.username)
                .field("algorithm", &d.algorithm)
                .finish_non_exhaustive(),
        }
    }
}

/// Digest の Authorization の引数。
#[derive(Clone, PartialEq, Eq)]
pub struct DigestParams {
    pub username: String,
    pub realm: String,
    pub nonce: String,
    pub uri: String,
    pub response: String,
    pub algorithm: Algorithm,
    pub qop: Option<String>,
    pub nc: Option<String>,
    pub cnonce: Option<String>,
    pub opaque: Option<String>,
}

/// Authorization ヘッダーを解釈する。
pub fn parse_authorization(v: &str) -> Result<Authorization> {
    let list = parse_auth_list(v)?;
    let [(scheme, params)] = list.as_slice() else {
        return Err(Error::Malformed("Authorization"));
    };
    if scheme.eq_ignore_ascii_case("basic") {
        let t = param(params, "").ok_or(Error::Malformed("Basic の資格情報"))?;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(t)
            .map_err(|_| Error::Malformed("Basic の base64"))?;
        let s = String::from_utf8(raw).map_err(|_| Error::Malformed("Basic の UTF-8"))?;
        let (u, p) = s
            .split_once(':')
            .ok_or(Error::Malformed("Basic の資格情報"))?;
        return Ok(Authorization::Basic {
            username: u.to_string(),
            password: p.to_string(),
        });
    }
    if !scheme.eq_ignore_ascii_case("digest") {
        return Err(Error::Unsupported("認証方式"));
    }
    let req = |n: &str| {
        param(params, n)
            .map(str::to_string)
            .ok_or(Error::Malformed("Digest の必須引数"))
    };
    let algorithm = match param(params, "algorithm") {
        None => Algorithm::Md5,
        Some(a) => Algorithm::parse(a).ok_or(Error::Unsupported("Digest の algorithm"))?,
    };
    Ok(Authorization::Digest(DigestParams {
        username: req("username")?,
        realm: req("realm")?,
        nonce: req("nonce")?,
        uri: req("uri")?,
        response: req("response")?,
        algorithm,
        qop: param(params, "qop").map(str::to_string),
        nc: param(params, "nc").map(str::to_string),
        cnonce: param(params, "cnonce").map(str::to_string),
        opaque: param(params, "opaque").map(str::to_string),
    }))
}

/// サーバーの Digest challenge（WWW-Authenticate の値）。
pub fn digest_challenge(
    realm: &str,
    nonce: &str,
    algorithm: Algorithm,
    qop: bool,
    stale: bool,
) -> String {
    let mut s = format!(
        "Digest realm={}, nonce={}",
        quote(realm).unwrap_or_else(|_| "\"\"".into()),
        quote(nonce).unwrap_or_else(|_| "\"\"".into())
    );
    if algorithm != Algorithm::Md5 {
        s.push_str(&format!(", algorithm={}", algorithm.as_str()));
    }
    if qop {
        s.push_str(", qop=\"auth\"");
    }
    if stale {
        s.push_str(", stale=TRUE");
    }
    s
}

/// サーバーの Basic challenge。
pub fn basic_challenge(realm: &str) -> String {
    format!(
        "Basic realm={}",
        quote(realm).unwrap_or_else(|_| "\"\"".into())
    )
}

/// Digest の Authorization を検証する。成功なら利用者名。
///
/// - `request_uri` は要求行の URI。Authorization の `uri` と一致しなければ拒否する。
/// - `nonce_ok` はサーバーが発行した nonce か（期限を含む）を判定する。
/// - `password_of` は利用者名からパスワードを引く。
pub fn verify_digest(
    method: &str,
    request_uri: &str,
    header: &str,
    realm: &str,
    nonce_ok: impl Fn(&str) -> bool,
    password_of: impl Fn(&str) -> Option<String>,
) -> Result<String> {
    let Authorization::Digest(d) = parse_authorization(header)? else {
        return Err(Error::Auth("Digest ではない"));
    };
    if d.realm != realm {
        return Err(Error::Auth("realm が違う"));
    }
    if d.uri != request_uri {
        return Err(Error::Auth("uri が要求と違う"));
    }
    if !nonce_ok(&d.nonce) {
        return Err(Error::Auth("nonce が無効"));
    }
    let qop = match d.qop.as_deref() {
        None => None,
        Some(q) if q.eq_ignore_ascii_case("auth") => Some("auth"),
        Some(_) => return Err(Error::Auth("qop が未対応")),
    };
    let (nc, cnonce) = match qop {
        Some(_) => (
            d.nc.as_deref().ok_or(Error::Auth("nc がない"))?,
            d.cnonce.as_deref().ok_or(Error::Auth("cnonce がない"))?,
        ),
        None => ("", d.cnonce.as_deref().unwrap_or("")),
    };
    let pass = password_of(&d.username).ok_or(Error::Auth("利用者がいない"))?;
    let expect = digest_response(
        d.algorithm,
        &d.username,
        realm,
        &pass,
        &d.nonce,
        nc,
        cnonce,
        qop,
        method,
        &d.uri,
    );
    let got = d.response.to_ascii_lowercase();
    if expect.len() == got.len() && bool::from(expect.as_bytes().ct_eq(got.as_bytes())) {
        Ok(d.username)
    } else {
        Err(Error::Auth("response が一致しない"))
    }
}

/// Basic の Authorization を検証する。成功なら利用者名。
pub fn verify_basic(header: &str, password_of: impl Fn(&str) -> Option<String>) -> Result<String> {
    let Authorization::Basic { username, password } = parse_authorization(header)? else {
        return Err(Error::Auth("Basic ではない"));
    };
    let pass = password_of(&username).ok_or(Error::Auth("利用者がいない"))?;
    if pass.len() == password.len() && bool::from(pass.as_bytes().ct_eq(password.as_bytes())) {
        Ok(username)
    } else {
        Err(Error::Auth("パスワードが一致しない"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc2617_example() {
        let r = digest_response(
            Algorithm::Md5,
            "Mufasa",
            "testrealm@host.com",
            "Circle Of Life",
            "dcd98b7102dd2f0e8b11d0f600bfb0c093",
            "00000001",
            "0a4f113b",
            Some("auth"),
            "GET",
            "/dir/index.html",
        );
        assert_eq!(r, "6629fae49393a05397450978507c4ef1");
    }

    #[test]
    fn rfc7616_examples() {
        let hdr = r#"Digest realm="http-auth@example.org", qop="auth, auth-int", algorithm=SHA-256, nonce="7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v", opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS""#;
        let hdr2 = r#"Digest realm="http-auth@example.org", qop="auth, auth-int", algorithm=MD5, nonce="7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v", opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS""#;
        let chs = parse_challenges([hdr, hdr2]);
        assert_eq!(chs.len(), 2);
        let cred = Credentials::new("Mufasa", "Circle of Life");
        let cnonce = "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ";
        let mut a = ClientAuth::from_challenges(&chs, false).unwrap();
        let Challenge::Digest(d) = a.challenge() else {
            panic!()
        };
        assert_eq!(d.algorithm, Algorithm::Sha256);
        let v = a
            .authorization(&cred, "GET", "/dir/index.html", cnonce)
            .unwrap();
        assert!(v.contains(
            "response=\"753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1\""
        ));
        assert!(v.contains("nc=00000001"));
        let Challenge::Digest(md5) = &chs[1] else {
            panic!()
        };
        let v = digest_authorization(&cred, md5, "GET", "/dir/index.html", 1, cnonce).unwrap();
        assert!(
            v.contains("response=\"8ca523f5e9506fed4657c9700eebdbec\""),
            "{v}"
        );
        // 2 回目は nc が増える
        let v = a
            .authorization(&cred, "GET", "/dir/index.html", cnonce)
            .unwrap();
        assert!(v.contains("nc=00000002"));
        // サーバー側で検証できる
        let user = verify_digest(
            "GET",
            "/dir/index.html",
            &v,
            "http-auth@example.org",
            |n| n == "7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v",
            |u| (u == "Mufasa").then(|| "Circle of Life".to_string()),
        )
        .unwrap();
        assert_eq!(user, "Mufasa");
    }

    #[test]
    fn mediamtx_style_no_qop() {
        // mediamtx・多くのカメラは qop なしの MD5 と Basic を並べる
        let chs = parse_challenges([
            r#"Basic realm="IPCAM""#,
            r#"Digest realm="IPCAM", nonce="abcdef0123""#,
        ]);
        assert_eq!(chs.len(), 2);
        let mut a = ClientAuth::from_challenges(&chs, false).unwrap();
        assert!(!a.needs_cnonce());
        let cred = Credentials::new("user", "pass");
        let v = a
            .authorization(&cred, "DESCRIBE", "rtsp://h/cam", "")
            .unwrap();
        assert!(!v.contains("qop"));
        assert!(!v.contains("algorithm"));
        assert_eq!(
            verify_digest(
                "DESCRIBE",
                "rtsp://h/cam",
                &v,
                "IPCAM",
                |n| n == "abcdef0123",
                |_| Some("pass".into())
            ),
            Ok("user".into())
        );
        assert!(
            verify_digest(
                "DESCRIBE",
                "rtsp://h/cam",
                &v,
                "IPCAM",
                |_| true,
                |_| Some("wrong".into())
            )
            .is_err()
        );
        assert!(
            verify_digest(
                "PLAY",
                "rtsp://h/cam",
                &v,
                "IPCAM",
                |_| true,
                |_| Some("pass".into())
            )
            .is_err()
        );
        assert!(
            verify_digest(
                "DESCRIBE",
                "rtsp://h/other",
                &v,
                "IPCAM",
                |_| true,
                |_| Some("pass".into())
            )
            .is_err()
        );
        assert!(
            verify_digest(
                "DESCRIBE",
                "rtsp://h/cam",
                &v,
                "IPCAM",
                |_| false,
                |_| Some("pass".into())
            )
            .is_err()
        );
    }

    #[test]
    fn basic_policy() {
        let chs = parse_challenges([r#"Basic realm="x""#]);
        assert!(ClientAuth::from_challenges(&chs, false).is_err());
        let mut a = ClientAuth::from_challenges(&chs, true).unwrap();
        let cred = Credentials::new("Aladdin", "open sesame");
        let v = a.authorization(&cred, "DESCRIBE", "rtsp://x", "").unwrap();
        assert_eq!(v, "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
        assert_eq!(
            verify_basic(&v, |u| (u == "Aladdin").then(|| "open sesame".into())),
            Ok("Aladdin".into())
        );
        assert!(verify_basic(&v, |_| Some("x".into())).is_err());
    }

    #[test]
    fn multiple_in_one_header_and_unknown() {
        let chs = parse_challenges([
            r#"Negotiate, Digest realm="r", nonce="n", algorithm=SHA-512-256, Digest realm="r", nonce="n2", stale=true, Basic realm="b""#,
        ]);
        assert_eq!(chs.len(), 4, "{chs:?}");
        assert!(matches!(&chs[0], Challenge::Other(s) if s == "Negotiate"));
        assert!(matches!(&chs[1], Challenge::Other(_)));
        let Challenge::Digest(d) = &chs[2] else {
            panic!()
        };
        assert!(d.stale);
        assert_eq!(d.nonce, "n2");
        assert!(matches!(&chs[3], Challenge::Basic { realm } if realm == "b"));
        // auth-int だけは選ばない
        let chs = parse_challenges([r#"Digest realm="r", nonce="n", qop="auth-int""#]);
        assert!(ClientAuth::from_challenges(&chs, true).is_err());
    }

    #[test]
    fn debug_redacts() {
        let c = Credentials::new("u", "topsecret");
        assert!(!format!("{c:?}").contains("topsecret"));
        let a = parse_authorization("Basic dTp0b3BzZWNyZXQ=").unwrap();
        assert!(!format!("{a:?}").contains("topsecret"));
        let chs = parse_challenges([r#"Digest realm="r", nonce="secretnonce""#]);
        assert!(!format!("{chs:?}").contains("secretnonce"));
    }

    #[test]
    fn fuzz_auth_parsers() {
        let base = r#"Digest username="u\"x", realm="r", nonce="n", uri="rtsp://h/a", response="00", qop=auth, nc=00000001, cnonce="c", Basic dTpw"#;
        let mut seed = 99u64;
        for _ in 0..20000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let mut b = base.as_bytes().to_vec();
            let i = (seed >> 33) as usize % b.len();
            b[i] = (seed >> 13) as u8;
            if let Ok(s) = std::str::from_utf8(&b) {
                let _ = parse_challenges([s]);
                let _ = parse_authorization(s);
                let _ = verify_digest("A", "b", s, "r", |_| true, |_| Some("p".into()));
            }
        }
    }

    #[test]
    fn quoting() {
        let cred = Credentials::new("a\"b", "p");
        let ch = DigestChallenge {
            realm: "r".into(),
            nonce: "n".into(),
            opaque: None,
            algorithm: Algorithm::Md5,
            algorithm_explicit: false,
            qop_auth: false,
            stale: false,
        };
        let v = digest_authorization(&cred, &ch, "M", "u", 1, "").unwrap();
        let Authorization::Digest(d) = parse_authorization(&v).unwrap() else {
            panic!()
        };
        assert_eq!(d.username, "a\"b");
        let bad = Credentials::new("a\r\nX: y", "p");
        assert!(digest_authorization(&bad, &ch, "M", "u", 1, "").is_err());
    }
}
