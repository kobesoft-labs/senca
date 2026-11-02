//! AWS Signature Version 4（署名と検証）。HMAC・SHA-256 は RustCrypto（ADR-0002）、正規化と手順は自前。
//!
//! 手順（AWS の文書「Create a signed AWS API request」）:
//! 1. 正規要求（canonical request）: メソッド・正規 URI・正規 query・正規 header・署名する header の名前・
//!    payload の SHA-256。
//! 2. 署名する文字列（string to sign）: `AWS4-HMAC-SHA256`・`x-amz-date`・資格の範囲・正規要求の SHA-256。
//! 3. 署名鍵: `HMAC("AWS4"+secret, date) → region → service → "aws4_request"`。
//! 4. `Authorization: AWS4-HMAC-SHA256 Credential=…, SignedHeaders=…, Signature=…`。
//!
//! path は復号した形で受け取り、segment ごとに 1 回だけ符号化する（AWS の試験集の `normalize` と同じ）。
//! S3 以外の AWS は「符号化済みの path をもう一度符号化」と定めるが、この crate が作る path は
//! unreserved の文字だけなので両者は一致する。

use crate::call::{HttpCall, uri_encode};
use crate::error::PaasError;
use crate::secret::Secret;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::fmt;

pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// AWS の資格情報。秘密の文字列 `access_key_id:secret_access_key[:session_token]` から作る。
#[derive(Clone)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: Secret,
    pub session_token: Option<Secret>,
}

impl AwsCredentials {
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<Secret>, session_token: Option<Secret>) -> Self {
        AwsCredentials { access_key_id: access_key_id.into(), secret_access_key: secret_access_key.into(), session_token }
    }

    /// `access_key_id:secret_access_key[:session_token]`。誤りに値は含めない。
    pub fn parse(s: &Secret) -> Result<Self, PaasError> {
        let mut it = s.expose().splitn(3, ':');
        let (Some(id), Some(key)) = (it.next(), it.next()) else {
            return Err(PaasError::config("AWS の資格情報は access_key_id:secret_access_key[:session_token]"));
        };
        let token = it.next();
        let ok = |v: &str| !v.is_empty() && v.bytes().all(|c| c.is_ascii_graphic());
        if !ok(id) || !id.bytes().all(|c| c.is_ascii_alphanumeric()) || !ok(key) || token.is_some_and(|t| !ok(t)) {
            return Err(PaasError::config("AWS の資格情報の形が不正"));
        }
        Ok(AwsCredentials::new(id, key, token.map(Secret::new)))
    }
}

impl fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("session_token", &self.session_token.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
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

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    // HMAC はどの長さの鍵も受け付ける。
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac accepts any key length");
    m.update(data);
    m.finalize().into_bytes().into()
}

/// UNIX 時刻（秒）を `YYYYMMDDTHHMMSSZ` にする。
pub fn format_amz_date(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", secs / 3600, secs / 60 % 60, secs % 60)
}

/// `YYYYMMDDTHHMMSSZ` を UNIX 時刻（秒）にする。
pub fn parse_amz_date(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = &s[r];
        if t.bytes().all(|c| c.is_ascii_digit()) { t.parse().ok() } else { None }
    };
    let (y, mo, d, h, mi, se) = (num(0..4)?, num(4..6)?, num(6..8)?, num(9..11)?, num(11..13)?, num(13..15)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 || y < 1970 {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + se).ok()
}

// H. Hinnant の暦の計算（proleptic Gregorian）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 正規要求の材料。
#[derive(Debug, Clone, Copy)]
pub struct CanonicalInput<'a> {
    pub method: &'a str,
    /// 復号した path。
    pub path: &'a str,
    /// 復号した query。
    pub query: &'a [(String, String)],
    /// 署名する header（すべて）。名前の大文字小文字は問わない。
    pub headers: &'a [(String, String)],
    /// payload の SHA-256（16 進小文字）。
    pub payload_hash: &'a str,
}

/// header の値を正規化する（前後の空白を除き、続く空白を 1 つにする）。
fn normalize_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for w in v.split([' ', '\t']).filter(|w| !w.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

/// 正規要求と `SignedHeaders` の値を作る。
pub fn canonical_request(input: &CanonicalInput<'_>) -> (String, String) {
    let uri = if input.path.is_empty() { "/".to_string() } else { uri_encode(input.path, true) };
    let mut q: Vec<(String, String)> = input.query.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
    q.sort();
    let query = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    // 名前で安定に並べ、同じ名前の値は出てきた順に `,` でつなぐ。
    let mut hs: Vec<(String, String)> = input.headers.iter().map(|(n, v)| (n.to_ascii_lowercase(), normalize_value(v))).collect();
    hs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut canon_headers = String::new();
    let mut names: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < hs.len() {
        let name = &hs[i].0;
        let mut vals = vec![hs[i].1.as_str()];
        let mut j = i + 1;
        while j < hs.len() && hs[j].0 == *name {
            vals.push(&hs[j].1);
            j += 1;
        }
        canon_headers.push_str(name);
        canon_headers.push(':');
        canon_headers.push_str(&vals.join(","));
        canon_headers.push('\n');
        names.push(name);
        i = j;
    }
    let signed = names.join(";");
    (format!("{}\n{uri}\n{query}\n{canon_headers}\n{signed}\n{}", input.method, input.payload_hash), signed)
}

/// `date/region/service/aws4_request`
pub fn credential_scope(date: &str, region: &str, service: &str) -> String {
    format!("{date}/{region}/{service}/aws4_request")
}

pub fn string_to_sign(amz_date: &str, scope: &str, canonical_request: &str) -> String {
    format!("{ALGORITHM}\n{amz_date}\n{scope}\n{}", sha256_hex(canonical_request.as_bytes()))
}

/// 署名鍵。`date` は `YYYYMMDD`。
pub fn signing_key(secret_access_key: &Secret, date: &str, region: &str, service: &str) -> [u8; 32] {
    let k = hmac(format!("AWS4{}", secret_access_key.expose()).as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    hmac(&k, b"aws4_request")
}

/// 署名（16 進小文字）。
pub fn signature(key: &[u8; 32], string_to_sign: &str) -> String {
    hex(&hmac(key, string_to_sign.as_bytes()))
}

/// 署名の計算結果（試験と検証用に途中の値も返す）。
#[derive(Debug, Clone)]
pub struct Signed {
    pub canonical_request: String,
    pub string_to_sign: String,
    pub signed_headers: String,
    pub signature: String,
    /// `Authorization` の値。
    pub authorization: String,
}

/// 与えた header（`x-amz-date` など署名に要るものを含めておく）で署名を計算する。
pub fn compute(input: &CanonicalInput<'_>, creds: &AwsCredentials, region: &str, service: &str, amz_date: &str) -> Signed {
    let (canonical_request, signed_headers) = canonical_request(input);
    let scope = credential_scope(&amz_date[..8.min(amz_date.len())], region, service);
    let sts = string_to_sign(amz_date, &scope, &canonical_request);
    let key = signing_key(&creds.secret_access_key, &amz_date[..8.min(amz_date.len())], region, service);
    let signature = signature(&key, &sts);
    let authorization =
        format!("{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}", creds.access_key_id);
    Signed { canonical_request, string_to_sign: sts, signed_headers, signature, authorization }
}

/// [`HttpCall`] に署名する。`x-amz-date`・`x-amz-content-sha256`・（あれば）`x-amz-security-token` を付け、
/// Host を含むすべての header に署名して `Authorization` を付ける。
pub fn sign_call(call: &mut HttpCall, creds: &AwsCredentials, region: &str, service: &str, now_unix: u64) {
    let amz_date = format_amz_date(now_unix);
    let payload_hash = sha256_hex(&call.body);
    call.headers.retain(|(n, _)| !n.eq_ignore_ascii_case("authorization"));
    call.set_header("X-Amz-Date", amz_date.clone());
    call.set_header("X-Amz-Content-Sha256", payload_hash.clone());
    if let Some(t) = &creds.session_token {
        call.set_header("X-Amz-Security-Token", t.expose());
    }
    let method = call.method.as_str().to_string();
    let s = compute(
        &CanonicalInput { method: &method, path: &call.path, query: &call.query, headers: &call.headers, payload_hash: &payload_hash },
        creds,
        region,
        service,
        &amz_date,
    );
    call.set_header("Authorization", s.authorization);
}

/// 検証で拒否した理由（値は含めない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    MissingAuthorization,
    MalformedAuthorization,
    UnknownAccessKey,
    WrongScope,
    MissingDate,
    ClockSkew,
    UnsignedRequiredHeader,
    PayloadHashMismatch,
    SessionTokenMismatch,
    SignatureMismatch,
}

/// 検証に通った要求の主体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub access_key_id: String,
    pub region: String,
    pub service: String,
}

/// 受け取った要求の材料（サーバー側。模擬サーバーで使う）。
#[derive(Debug, Clone, Copy)]
pub struct VerifyInput<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a [(String, String)],
    /// 受け取った header すべて。
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
}

/// 署名を検証する。`lookup` は access key id から (secret, session token) を返す。
/// `service` は期待するサービス名、`max_skew` は時計のずれの許容（秒。AWS は 5 分）。
pub fn verify(
    req: &VerifyInput<'_>,
    lookup: impl Fn(&str) -> Option<(Secret, Option<Secret>)>,
    service: &str,
    now_unix: u64,
    max_skew: u64,
) -> Result<Verified, VerifyError> {
    fn header_values<'h>(headers: &'h [(String, String)], name: &'static str) -> impl Iterator<Item = &'h str> {
        headers.iter().filter(move |(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    let get = |name: &'static str| header_values(req.headers, name);
    let mut auths = get("authorization");
    let auth = auths.next().ok_or(VerifyError::MissingAuthorization)?;
    if auths.next().is_some() {
        return Err(VerifyError::MalformedAuthorization);
    }
    let rest = auth.strip_prefix(ALGORITHM).and_then(|r| r.strip_prefix(' ')).ok_or(VerifyError::MalformedAuthorization)?;
    let (mut cred, mut signed, mut sig) = (None, None, None);
    for part in rest.split(',') {
        let (k, v) = part.trim().split_once('=').ok_or(VerifyError::MalformedAuthorization)?;
        let slot = match k {
            "Credential" => &mut cred,
            "SignedHeaders" => &mut signed,
            "Signature" => &mut sig,
            _ => return Err(VerifyError::MalformedAuthorization),
        };
        if slot.replace(v).is_some() {
            return Err(VerifyError::MalformedAuthorization);
        }
    }
    let (Some(cred), Some(signed), Some(sig)) = (cred, signed, sig) else {
        return Err(VerifyError::MalformedAuthorization);
    };
    let parts: Vec<&str> = cred.split('/').collect();
    let [akid, date, region, svc, term] = parts[..] else {
        return Err(VerifyError::MalformedAuthorization);
    };
    if term != "aws4_request" || svc != service || region.is_empty() {
        return Err(VerifyError::WrongScope);
    }
    let (secret, token) = lookup(akid).ok_or(VerifyError::UnknownAccessKey)?;
    let amz_date = get("x-amz-date").next().ok_or(VerifyError::MissingDate)?;
    let t = parse_amz_date(amz_date).ok_or(VerifyError::MissingDate)?;
    if !amz_date.starts_with(date) {
        return Err(VerifyError::WrongScope);
    }
    if t.abs_diff(now_unix) > max_skew {
        return Err(VerifyError::ClockSkew);
    }
    let names: Vec<&str> = signed.split(';').collect();
    if names.iter().any(|n| n.is_empty() || *n != n.to_ascii_lowercase()) || !names.is_sorted() {
        return Err(VerifyError::MalformedAuthorization);
    }
    let must = ["host", "x-amz-date"];
    let token_header = get("x-amz-security-token").next();
    if must.iter().any(|m| !names.contains(m)) || (token_header.is_some() && !names.contains(&"x-amz-security-token")) {
        return Err(VerifyError::UnsignedRequiredHeader);
    }
    match (&token, token_header) {
        (None, None) => {}
        (Some(want), Some(got)) if want.ct_eq(got) => {}
        _ => return Err(VerifyError::SessionTokenMismatch),
    }
    let body_hash = sha256_hex(req.body);
    if let Some(h) = get("x-amz-content-sha256").next()
        && h != body_hash
    {
        return Err(VerifyError::PayloadHashMismatch);
    }
    let headers: Vec<(String, String)> =
        req.headers.iter().filter(|(n, _)| names.contains(&n.to_ascii_lowercase().as_str())).cloned().collect();
    let creds = AwsCredentials::new(akid, secret, token);
    let s = compute(
        &CanonicalInput { method: req.method, path: req.path, query: req.query, headers: &headers, payload_hash: &body_hash },
        &creds,
        region,
        service,
        amz_date,
    );
    if s.signed_headers != signed {
        return Err(VerifyError::UnsignedRequiredHeader);
    }
    if !Secret::new(s.signature).ct_eq(sig) {
        return Err(VerifyError::SignatureMismatch);
    }
    Ok(Verified { access_key_id: akid.to_string(), region: region.to_string(), service: service.to_string() })
}

#[cfg(test)]
mod tests {
    //! AWS の SigV4 試験集（awslabs/aws-c-auth `tests/aws-signing-test-suite/v4`。資格情報 AKIDEXAMPLE、
    //! region us-east-1、service "service"、時刻 2015-08-30T12:36:00Z）の値を埋め込んで確かめる。
    use super::*;

    const AKID: &str = "AKIDEXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const DATE: &str = "20150830T123600Z";
    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    struct Vector {
        name: &'static str,
        /// 試験集の request.txt（x-amz-date は試験側で足す）。
        request: &'static str,
        canonical: &'static str,
        sts_hash: Option<&'static str>,
        signature: &'static str,
        token: Option<&'static str>,
        /// sign_body（x-amz-content-sha256 を足して署名する）。
        sign_body: bool,
    }

    const VECTORS: &[Vector] = &[
        Vector {
            name: "get-vanilla",
            request: "GET / HTTP/1.1\nHost:example.amazonaws.com\n\n",
            canonical: "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: Some("bb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"),
            signature: "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
            token: None,
            sign_body: false,
        },
        Vector {
            name: "post-vanilla",
            request: "POST / HTTP/1.1\nHost:example.amazonaws.com\n\n",
            canonical: "POST\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: None,
            signature: "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b",
            token: None,
            sign_body: false,
        },
        Vector {
            name: "get-vanilla-query-order-key-case",
            request: "GET /?Param2=value2&Param1=value1 HTTP/1.1\nHost:example.amazonaws.com\n\n",
            canonical: "GET\n/\nParam1=value1&Param2=value2\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: None,
            signature: "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500",
            token: None,
            sign_body: false,
        },
        Vector {
            name: "get-vanilla-query-unreserved",
            request: "GET /?-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz=-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz HTTP/1.1\nHost:example.amazonaws.com\n\n",
            canonical: "GET\n/\n-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz=-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: None,
            signature: "9c3e54bfcdf0b19771a7f523ee5669cdf59bc7cc0884027167c21bb143a40197",
            token: None,
            sign_body: false,
        },
        Vector {
            name: "get-vanilla-utf8-query",
            request: "GET /?\u{1234}=bar HTTP/1.1\nHost:example.amazonaws.com\n\n",
            canonical: "GET\n/\n%E1%88%B4=bar\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: None,
            signature: "2cdec8eed098649ff3a119c94853b13c643bcf08f8b0a1d91e12c9027818dd04",
            token: None,
            sign_body: false,
        },
        Vector {
            name: "get-vanilla-empty-query-key",
            request: "GET /?Param1=value1 HTTP/1.1\nHost:example.amazonaws.com\n\n",
            canonical: "GET\n/\nParam1=value1\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: None,
            signature: "a67d582fa61cc504c4bae71f336f98b97f1ea3c7a6bfe1b6e45aec72011b9aeb",
            token: None,
            sign_body: false,
        },
        Vector {
            name: "get-space-normalized",
            request: "GET /example space/ HTTP/1.1\nHost:example.amazonaws.com\n\n",
            canonical: "GET\n/example%20space/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: None,
            signature: "652487583200325589f1fba4c7e578f72c47cb61beeca81406b39ddec1366741",
            token: None,
            sign_body: false,
        },
        Vector {
            name: "get-header-value-trim",
            request: "GET / HTTP/1.1\nHost:example.amazonaws.com\nMy-Header1: value1\nMy-Header2: \"a   b   c\"\n\n",
            canonical: "GET\n/\n\nhost:example.amazonaws.com\nmy-header1:value1\nmy-header2:\"a b c\"\nx-amz-date:20150830T123600Z\n\nhost;my-header1;my-header2;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: None,
            signature: "acc3ed3afb60bb290fc8d2dd0098b9911fcaa05412b367055dee359757a9c736",
            token: None,
            sign_body: false,
        },
        Vector {
            name: "get-header-key-duplicate",
            request: "GET / HTTP/1.1\nHost:example.amazonaws.com\nMy-Header1:value2\nMy-Header1:value2\nMy-Header1:value1\n\n",
            canonical: "GET\n/\n\nhost:example.amazonaws.com\nmy-header1:value2,value2,value1\nx-amz-date:20150830T123600Z\n\nhost;my-header1;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: None,
            signature: "c9d5ea9f3f72853aea855b47ea873832890dbdd183b4468f858259531a5138ea",
            token: None,
            sign_body: false,
        },
        Vector {
            name: "get-header-value-order",
            request: "GET / HTTP/1.1\nHost:example.amazonaws.com\nMy-Header1:value4\nMy-Header1:value1\nMy-Header1:value3\nMy-Header1:value2\n\n",
            canonical: "GET\n/\n\nhost:example.amazonaws.com\nmy-header1:value4,value1,value3,value2\nx-amz-date:20150830T123600Z\n\nhost;my-header1;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: None,
            signature: "08c7e5a9acfcfeb3ab6b2185e75ce8b1deb5e634ec47601a50643f830c755c01",
            token: None,
            sign_body: false,
        },
        Vector {
            name: "post-header-key-sort",
            request: "POST / HTTP/1.1\nHost:example.amazonaws.com\nMy-Header1:value1\n\n",
            canonical: "POST\n/\n\nhost:example.amazonaws.com\nmy-header1:value1\nx-amz-date:20150830T123600Z\n\nhost;my-header1;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: None,
            signature: "c5410059b04c1ee005303aed430f6e6645f61f4dc9e1461ec8f8916fdf18852c",
            token: None,
            sign_body: false,
        },
        Vector {
            name: "post-header-value-case",
            request: "POST / HTTP/1.1\nHost:example.amazonaws.com\nMy-Header1:VALUE1\n\n",
            canonical: "POST\n/\n\nhost:example.amazonaws.com\nmy-header1:VALUE1\nx-amz-date:20150830T123600Z\n\nhost;my-header1;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: None,
            signature: "cdbc9802e29d2942e5e10b5bccfdd67c5f22c7c4e8ae67b53629efa58b974b7d",
            token: None,
            sign_body: false,
        },
        Vector {
            name: "post-x-www-form-urlencoded",
            request: "POST / HTTP/1.1\nContent-Type:application/x-www-form-urlencoded\nHost:example.amazonaws.com\nContent-Length:13\n\nParam1=value1",
            canonical: "POST\n/\n\ncontent-length:13\ncontent-type:application/x-www-form-urlencoded\nhost:example.amazonaws.com\nx-amz-content-sha256:9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e\nx-amz-date:20150830T123600Z\n\ncontent-length;content-type;host;x-amz-content-sha256;x-amz-date\n9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e",
            sts_hash: Some("b1edd1d03544c25390e32085d55b57acc9a3961bb59415ff86c45c3d89d16cfb"),
            signature: "d3875051da38690788ef43de4db0d8f280229d82040bfac253562e56c3f20e0b",
            token: None,
            sign_body: true,
        },
        Vector {
            name: "get-vanilla-with-session-token",
            request: "GET / HTTP/1.1\nHost:example.amazonaws.com\n\n",
            canonical: "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\nx-amz-security-token:6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267\n\nhost;x-amz-date;x-amz-security-token\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            sts_hash: Some("067b36aa60031588cea4a4cde1f21215227a047690c72247f1d70b32fbbfad2b"),
            signature: "07ec1639c89043aa0e3e2de82b96708f198cceab042d4a97044c66dd9f74e7f8",
            token: Some("6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267"),
            sign_body: false,
        },
    ];

    /// 試験集の request.txt を (method, path, query, headers, body) に分ける。
    type Pairs = Vec<(String, String)>;

    fn parse_request(r: &str) -> (String, String, Pairs, Pairs, Vec<u8>) {
        let (head, body) = r.split_once("\n\n").unwrap();
        let mut lines = head.lines();
        let line = lines.next().unwrap();
        let method = line.split(' ').next().unwrap().to_string();
        let target = line[method.len() + 1..line.len() - " HTTP/1.1".len()].to_string();
        let (path, q) = target.split_once('?').unwrap_or((&target, ""));
        let query = q
            .split('&')
            .filter(|s| !s.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (k.to_string(), v.to_string())
            })
            .collect();
        let headers = lines
            .map(|l| {
                let (n, v) = l.split_once(':').unwrap();
                (n.to_string(), v.to_string())
            })
            .collect();
        (method, path.to_string(), query, headers, body.as_bytes().to_vec())
    }

    #[test]
    fn aws_test_suite_vectors() {
        for v in VECTORS {
            let (method, path, query, mut headers, body) = parse_request(v.request);
            let payload_hash = sha256_hex(&body);
            if v.sign_body {
                headers.push(("X-Amz-Content-Sha256".into(), payload_hash.clone()));
            }
            headers.push(("X-Amz-Date".into(), DATE.into()));
            if let Some(t) = v.token {
                headers.push(("X-Amz-Security-Token".into(), t.into()));
            }
            let creds = AwsCredentials::new(AKID, SECRET, v.token.map(Secret::new));
            let input = CanonicalInput { method: &method, path: &path, query: &query, headers: &headers, payload_hash: &payload_hash };
            let s = compute(&input, &creds, "us-east-1", "service", DATE);
            assert_eq!(s.canonical_request, v.canonical, "{} canonical", v.name);
            if let Some(h) = v.sts_hash {
                assert_eq!(s.string_to_sign.lines().last().unwrap(), h, "{} sts", v.name);
            }
            assert_eq!(s.signature, v.signature, "{} signature", v.name);
            assert_eq!(
                s.authorization,
                format!(
                    "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders={}, Signature={}",
                    s.signed_headers, v.signature
                ),
                "{}",
                v.name
            );
            // 自前の検証器も同じ要求を受け付ける。
            let lookup = |id: &str| (id == AKID).then(|| (Secret::new(SECRET), v.token.map(Secret::new)));
            let mut with_auth = headers.clone();
            with_auth.push(("Authorization".into(), s.authorization.clone()));
            let now = parse_amz_date(DATE).unwrap();
            let req = VerifyInput { method: &method, path: &path, query: &query, headers: &with_auth, body: &body };
            assert!(verify(&req, lookup, "service", now, 300).is_ok(), "{} verify", v.name);
        }
    }

    #[test]
    fn signing_key_matches_aws_docs_example() {
        // AWS の文書の例（secret wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY、20120215、us-east-1、iam）
        let k = signing_key(&Secret::new(SECRET), "20120215", "us-east-1", "iam");
        assert_eq!(hex(&k), "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d");
    }

    #[test]
    fn amz_date_round_trip() {
        assert_eq!(format_amz_date(1_440_938_160), DATE);
        assert_eq!(parse_amz_date(DATE), Some(1_440_938_160));
        assert_eq!(format_amz_date(0), "19700101T000000Z");
        assert_eq!(format_amz_date(951_782_400), "20000229T000000Z");
        for t in [0u64, 86_399, 951_782_400, 4_102_444_800, 1_767_225_599] {
            assert_eq!(parse_amz_date(&format_amz_date(t)), Some(t));
        }
        for bad in ["20150830T123600", "2015083XT123600Z", "20151330T123600Z", "20150830T253600Z"] {
            assert_eq!(parse_amz_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn sign_call_and_verify_rejections() {
        let creds = AwsCredentials::new(AKID, SECRET, Some(Secret::new("TOKEN123")));
        let mut c = HttpCall::new(senca_http::Method::Post, crate::call::Endpoint::https("ivs.us-west-2.amazonaws.com"), "/CreateChannel")
            .json(&serde_json::json!({"name": "a"}));
        let now = 1_700_000_000;
        sign_call(&mut c, &creds, "us-west-2", "ivs", now);
        assert_eq!(c.header("x-amz-date"), Some(format_amz_date(now).as_str()));
        assert_eq!(c.header("x-amz-content-sha256"), Some(sha256_hex(&c.body).as_str()));
        assert_eq!(c.header("x-amz-security-token"), Some("TOKEN123"));
        let auth = c.header("authorization").unwrap().to_string();
        assert!(auth.contains("/us-west-2/ivs/aws4_request"));
        assert!(auth.contains("SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token,"));
        assert!(!format!("{c:?}").contains("TOKEN123"));
        assert!(!format!("{creds:?}").contains("TOKEN123") && !format!("{creds:?}").contains("EXAMPLEKEY"));

        let lookup = |id: &str| (id == AKID).then(|| (Secret::new(SECRET), Some(Secret::new("TOKEN123"))));
        let run = |headers: &[(String, String)], body: &[u8], now: u64| {
            let req = VerifyInput { method: "POST", path: "/CreateChannel", query: &[], headers, body };
            verify(&req, lookup, "ivs", now, 300)
        };
        assert_eq!(run(&c.headers, &c.body, now).unwrap().region, "us-west-2");
        assert_eq!(run(&c.headers, b"{\"name\":\"b\"}", now), Err(VerifyError::PayloadHashMismatch));
        assert_eq!(run(&c.headers, &c.body, now + 1000), Err(VerifyError::ClockSkew));
        let mut h = c.headers.clone();
        h.iter_mut().find(|(n, _)| n == "Content-Type").unwrap().1 = "text/plain".into();
        assert_eq!(run(&h, &c.body, now), Err(VerifyError::SignatureMismatch));
        let mut h = c.headers.clone();
        h.retain(|(n, _)| n != "X-Amz-Security-Token");
        assert_eq!(run(&h, &c.body, now), Err(VerifyError::SessionTokenMismatch));
        let mut h = c.headers.clone();
        h.retain(|(n, _)| n != "Authorization");
        assert_eq!(run(&h, &c.body, now), Err(VerifyError::MissingAuthorization));
        let req = VerifyInput { method: "POST", path: "/CreateChannel", query: &[], headers: &c.headers, body: &c.body };
        assert_eq!(verify(&req, lookup, "medialive", now, 300), Err(VerifyError::WrongScope));
        assert_eq!(verify(&req, |_| None, "ivs", now, 300), Err(VerifyError::UnknownAccessKey));
    }

    #[test]
    fn credentials_parse() {
        let c = AwsCredentials::parse(&Secret::new("AKIA123:sec/ret+x")).unwrap();
        assert_eq!(c.access_key_id, "AKIA123");
        assert!(c.session_token.is_none());
        let c = AwsCredentials::parse(&Secret::new("AKIA123:sec:tok:with:colons")).unwrap();
        assert_eq!(c.session_token.unwrap().expose(), "tok:with:colons");
        for bad in ["", "AKIA", "AKIA:", ":x", "AK IA:x", "AKIA:x y"] {
            let e = AwsCredentials::parse(&Secret::new(bad)).unwrap_err();
            assert_eq!(e.kind, crate::ErrorKind::Config);
            assert!(bad.is_empty() || !e.to_string().contains(bad));
        }
        assert_eq!(EMPTY, sha256_hex(b""));
    }
}
