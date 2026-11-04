//! OIDC のログイン（ノードが relying party）の、通信を含まない部分（ADR-0017）。
//!
//! - ID トークンの検証は [`crate::jwt::JwtVerifier::verify_id_token`]（API の JWT と同じ署名の検証）。
//! - ここはクレームの読み方: 入れ子のクレーム（`realm_access.roles` のような `.` 区切り）、役割の対応付け、
//!   メールアドレスのドメインの制限、PKCE（S256）の challenge。

use crate::auth::Scope;
use crate::users::Role;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// クレームの値 → 役割（または権限）の対応。`claim` が文字列ならその値、配列なら要素に `value` があれば当たり。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleMapping {
    pub claim: String,
    pub value: String,
    /// `admin`・`operator`・`viewer` か、権限の名前（`recording:delete` など）。
    pub role: String,
}

/// 役割か権限の名前。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    Role(Role),
    Scope(Scope),
}

pub fn parse_grant(s: &str) -> Option<Grant> {
    Role::parse(s).map(Grant::Role).or_else(|| Scope::parse(s).map(Grant::Scope))
}

/// `.` 区切りの入れ子のクレーム（例: `realm_access.roles`）。そのままの名前のクレームがあればそれを優先する
/// （名前に `.` を含むクレーム・URL の形のクレーム名のため）。
pub fn claim<'a>(claims: &'a Value, path: &str) -> Option<&'a Value> {
    if let Some(v) = claims.get(path) {
        return Some(v);
    }
    let mut cur = claims;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

pub fn claim_str<'a>(claims: &'a Value, path: &str) -> Option<&'a str> {
    claim(claims, path).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// クレームが `value` か、配列で `value` を含むか。空白区切りの文字列（`scope` のような）も要素として見る。
pub fn claim_contains(claims: &Value, path: &str, value: &str) -> bool {
    match claim(claims, path) {
        Some(Value::String(s)) => s == value || s.split(' ').any(|x| x == value),
        Some(Value::Array(a)) => a.iter().any(|x| x.as_str() == Some(value)),
        Some(Value::Bool(b)) => (value == "true") == *b,
        _ => false,
    }
}

/// 対応表から役割と権限を決める（当たったものすべての和）。
pub fn map_grants(claims: &Value, mappings: &[RoleMapping]) -> (BTreeSet<Role>, BTreeSet<Scope>) {
    let mut roles = BTreeSet::new();
    let mut scopes = BTreeSet::new();
    for m in mappings {
        if claim_contains(claims, &m.claim, &m.value) {
            match parse_grant(&m.role) {
                Some(Grant::Role(r)) => {
                    roles.insert(r);
                }
                Some(Grant::Scope(s)) => {
                    scopes.insert(s);
                }
                None => {}
            }
        }
    }
    (roles, scopes)
}

/// メールアドレスのドメインが許されているか（大文字・小文字を区別しない。空の一覧は制限なし）。
pub fn email_domain_allowed(email: Option<&str>, domains: &[String]) -> bool {
    if domains.is_empty() {
        return true;
    }
    let Some(e) = email else { return false };
    let Some((local, d)) = e.rsplit_once('@') else { return false };
    !local.is_empty() && domains.iter().any(|x| x.trim_start_matches('@').eq_ignore_ascii_case(d))
}

/// `email_verified` が真か（文字列の `"true"` を返す IdP もある）。
pub fn email_verified(claims: &Value) -> bool {
    match claims.get("email_verified") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

/// PKCE の `code_challenge`（S256: base64url(SHA-256(verifier))。RFC 7636）。
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// 利用者名の候補（`preferred_username`・`email`・`sub` の順に、使える文字だけを残す）。
pub fn username_candidate(claims: &Value, claim_name: Option<&str>) -> String {
    let pick = [claim_name.unwrap_or("preferred_username"), "preferred_username", "email", "sub"];
    for c in pick {
        if let Some(v) = claim_str(claims, c) {
            let s: String = v.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '+' | '-')).take(56).collect();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "user".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwt::{JwtError, JwtVerifier};
    use ed25519_dalek::Signer;
    use serde_json::json;

    #[test]
    fn claims_and_mappings() {
        let c = json!({
            "groups": ["senca-admins", "staff"],
            "realm_access": { "roles": ["senca-operator"] },
            "hd": "example.com",
            "https://example.com/roles": "x",
            "email": "alice@Example.com",
            "email_verified": "true",
        });
        assert!(claim_contains(&c, "groups", "senca-admins"));
        assert!(claim_contains(&c, "realm_access.roles", "senca-operator"));
        assert!(claim_contains(&c, "hd", "example.com"));
        assert!(claim_contains(&c, "https://example.com/roles", "x"), "名前に . を含むクレーム");
        assert!(!claim_contains(&c, "groups", "senca"));
        let m = vec![
            RoleMapping { claim: "groups".into(), value: "senca-admins".into(), role: "admin".into() },
            RoleMapping { claim: "realm_access.roles".into(), value: "senca-operator".into(), role: "operator".into() },
            RoleMapping { claim: "groups".into(), value: "staff".into(), role: "recording:delete".into() },
            RoleMapping { claim: "groups".into(), value: "none".into(), role: "viewer".into() },
        ];
        let (r, s) = map_grants(&c, &m);
        assert_eq!(r, [Role::Admin, Role::Operator].into_iter().collect());
        assert_eq!(s, [Scope::RecordingDelete].into_iter().collect());
        assert!(email_verified(&c));
        assert!(email_domain_allowed(Some("alice@Example.com"), &["example.com".into()]));
        assert!(email_domain_allowed(Some("a@x.org"), &[]));
        assert!(!email_domain_allowed(Some("a@evil.com"), &["example.com".into()]));
        assert!(!email_domain_allowed(Some("a@example.com.evil.com"), &["example.com".into()]));
        assert!(!email_domain_allowed(None, &["example.com".into()]));
        assert!(!email_domain_allowed(Some("@example.com"), &["example.com".into()]));
        assert_eq!(username_candidate(&json!({"preferred_username": "Alice Smith", "email": "a@b"}), None), "AliceSmith");
        assert_eq!(username_candidate(&json!({"email": "a@b"}), None), "a@b");
        assert_eq!(username_candidate(&json!({"sub": "日本"}), None), "user");
    }

    #[test]
    fn pkce_rfc7636_vector() {
        // RFC 7636 付録 B
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    fn sign(sk: &ed25519_dalek::SigningKey, header: &Value, claims: &Value) -> String {
        let h = URL_SAFE_NO_PAD.encode(header.to_string());
        let p = URL_SAFE_NO_PAD.encode(claims.to_string());
        let sig = sk.sign(format!("{h}.{p}").as_bytes());
        format!("{h}.{p}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
    }

    /// ID トークンの検証: 正しいもの 1 つと、悪いものを 1 つずつ。
    #[test]
    fn id_token_validation() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        let x = URL_SAFE_NO_PAD.encode(sk.verifying_key().to_bytes());
        // IdP の JWKS: 暗号化用の鍵・対応しない鍵・kid の無い鍵は飛ばす
        let jwks = json!({"keys": [
            {"kty": "RSA", "use": "enc", "kid": "enc1", "n": "AQAB", "e": "AQAB", "alg": "RSA-OAEP"},
            {"kty": "EC", "crv": "P-384", "kid": "p384", "x": "AA", "y": "AA"},
            {"kty": "OKP", "crv": "Ed25519", "x": x},
            {"kty": "OKP", "crv": "Ed25519", "kid": "k1", "x": x, "use": "sig"},
        ]});
        assert!(JwtVerifier::from_jwks(jwks.to_string().as_bytes(), "https://idp", "cid", "t").is_err(), "厳しい読み方では拒否");
        let v = JwtVerifier::from_jwks_lenient(jwks.to_string().as_bytes(), "https://idp", "cid").unwrap();
        assert!(v.has_kid("k1") && !v.has_kid("enc1"));
        assert!(JwtVerifier::from_jwks_lenient(br#"{"keys":[{"kty":"RSA","use":"enc","kid":"e","n":"AQAB","e":"AQAB"}]}"#, "i", "a").is_err());
        let now = 1_800_000_000i64;
        let h = json!({"alg": "EdDSA", "kid": "k1"});
        let good = json!({"iss": "https://idp", "aud": "cid", "sub": "u1", "exp": now + 300, "iat": now - 5, "nonce": "n0nce", "email": "a@b"});
        let with = |k: &str, val: Value| {
            let mut c = good.clone();
            if val.is_null() {
                c.as_object_mut().unwrap().remove(k);
            } else {
                c[k] = val;
            }
            c
        };
        let c = v.verify_id_token(&sign(&sk, &h, &good), now, "n0nce", None).unwrap();
        assert_eq!(c["sub"], "u1");
        assert_eq!(JwtVerifier::peek_kid(&sign(&sk, &h, &good)).as_deref(), Some("k1"));
        let bad = |claims: &Value, header: &Value, nonce: &str, max_age: Option<u64>| v.verify_id_token(&sign(&sk, header, claims), now, nonce, max_age).unwrap_err();
        assert_eq!(bad(&good, &h, "other", None), JwtError::Nonce);
        assert_eq!(bad(&good, &h, "", None), JwtError::Nonce);
        assert_eq!(bad(&with("nonce", Value::Null), &h, "n0nce", None), JwtError::Nonce);
        assert_eq!(bad(&with("iss", json!("https://evil")), &h, "n0nce", None), JwtError::Issuer);
        assert_eq!(bad(&with("aud", json!("other")), &h, "n0nce", None), JwtError::Audience);
        assert_eq!(bad(&with("aud", json!(["cid", "other"])), &h, "n0nce", None), JwtError::AuthorizedParty, "aud が複数なら azp が要る");
        assert!(v.verify_id_token(&sign(&sk, &h, &{ let mut c = with("aud", json!(["cid", "other"])); c["azp"] = json!("cid"); c }), now, "n0nce", None).is_ok());
        assert_eq!(bad(&with("azp", json!("other")), &h, "n0nce", None), JwtError::AuthorizedParty);
        assert_eq!(bad(&with("exp", json!(now - 61)), &h, "n0nce", None), JwtError::Expired);
        assert_eq!(bad(&with("exp", Value::Null), &h, "n0nce", None), JwtError::Format);
        assert_eq!(bad(&with("nbf", json!(now + 120)), &h, "n0nce", None), JwtError::NotYetValid);
        assert_eq!(bad(&with("iat", Value::Null), &h, "n0nce", None), JwtError::IssuedAt);
        assert_eq!(bad(&with("iat", json!(now + 120)), &h, "n0nce", None), JwtError::IssuedAt);
        assert_eq!(bad(&with("iat", json!(now - 3600)), &h, "n0nce", None), JwtError::IssuedAt);
        assert_eq!(bad(&with("sub", Value::Null), &h, "n0nce", None), JwtError::MissingSubject);
        assert_eq!(bad(&good, &h, "n0nce", Some(300)), JwtError::AuthTime, "max_age を求めたら auth_time が要る");
        assert_eq!(bad(&with("auth_time", json!(now - 1000)), &h, "n0nce", Some(300)), JwtError::AuthTime);
        assert!(v.verify_id_token(&sign(&sk, &h, &with("auth_time", json!(now - 100))), now, "n0nce", Some(300)).is_ok());
        assert_eq!(bad(&good, &json!({"alg": "none", "kid": "k1"}), "n0nce", None), JwtError::UnsupportedAlg);
        assert_eq!(bad(&good, &json!({"alg": "HS256", "kid": "k1"}), "n0nce", None), JwtError::UnsupportedAlg);
        assert_eq!(bad(&good, &json!({"alg": "EdDSA", "kid": "nope"}), "n0nce", None), JwtError::UnknownKey);
        let other = ed25519_dalek::SigningKey::from_bytes(&[4u8; 32]);
        assert_eq!(v.verify_id_token(&sign(&other, &h, &good), now, "n0nce", None).unwrap_err(), JwtError::BadSignature);
        // 署名の後で中身を差し替えたもの
        let t = sign(&sk, &h, &good);
        let parts: Vec<&str> = t.split('.').collect();
        let forged = format!("{}.{}.{}", parts[0], URL_SAFE_NO_PAD.encode(with("sub", json!("admin")).to_string()), parts[2]);
        assert_eq!(v.verify_id_token(&forged, now, "n0nce", None).unwrap_err(), JwtError::BadSignature);
    }
}
