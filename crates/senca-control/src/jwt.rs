//! JWT の検証（仕様 9.2）。署名の検証は監査済みのライブラリ（ed25519-dalek・p256・rsa）。
//!
//! - 鍵はローカルの JWKS だけ（トークンの中身で URL を取りに行かない）。
//! - 対応: EdDSA（Ed25519）、ES256、RS256。鍵が算法を決め、ヘッダーの alg が鍵と違えば拒否。
//! - `alg=none`・HS 系・`crit`・期限切れ・nbf 前・iss/aud 不一致・テナント欠落を拒否。
//! - 時計のずれは `leeway_secs`（既定 60 秒）まで許す。

use crate::auth::Scope;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwtError {
    Format,
    UnsupportedAlg,
    UnknownKey,
    BadSignature,
    Expired,
    NotYetValid,
    Issuer,
    Audience,
    MissingTenant,
    Critical,
    Jwks(String),
}

impl std::fmt::Display for JwtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            JwtError::Format => "malformed token",
            JwtError::UnsupportedAlg => "unsupported or mismatched alg",
            JwtError::UnknownKey => "unknown kid",
            JwtError::BadSignature => "bad signature",
            JwtError::Expired => "expired",
            JwtError::NotYetValid => "not yet valid",
            JwtError::Issuer => "issuer mismatch",
            JwtError::Audience => "audience mismatch",
            JwtError::MissingTenant => "tenant claim missing",
            JwtError::Critical => "crit header not supported",
            JwtError::Jwks(e) => return write!(f, "jwks: {e}"),
        };
        f.write_str(s)
    }
}

#[derive(Clone)]
enum Key {
    Ed25519(ed25519_dalek::VerifyingKey),
    Es256(p256::ecdsa::VerifyingKey),
    Rs256(rsa::pkcs1v15::VerifyingKey<sha2::Sha256>),
}

impl Key {
    fn alg(&self) -> &'static str {
        match self {
            Key::Ed25519(_) => "EdDSA",
            Key::Es256(_) => "ES256",
            Key::Rs256(_) => "RS256",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Claims {
    pub subject: String,
    pub tenant: String,
    pub scopes: BTreeSet<Scope>,
}

#[derive(Clone)]
pub struct JwtVerifier {
    keys: BTreeMap<String, Key>,
    pub issuer: String,
    pub audience: String,
    pub tenant_claim: String,
    pub leeway_secs: i64,
}

fn b64(s: &str) -> Result<Vec<u8>, JwtError> {
    URL_SAFE_NO_PAD.decode(s).map_err(|_| JwtError::Format)
}

impl JwtVerifier {
    /// JWKS（JSON）から作る。`kid` の無い鍵・未対応の鍵は拒否する（黙って飛ばさない）。
    pub fn from_jwks(jwks: &[u8], issuer: &str, audience: &str, tenant_claim: &str) -> Result<Self, JwtError> {
        let v: Value = serde_json::from_slice(jwks).map_err(|e| JwtError::Jwks(e.to_string()))?;
        let arr = v.get("keys").and_then(Value::as_array).ok_or_else(|| JwtError::Jwks("no keys".into()))?;
        let mut keys = BTreeMap::new();
        for k in arr {
            let s = |n: &str| k.get(n).and_then(Value::as_str);
            let kid = s("kid").ok_or_else(|| JwtError::Jwks("key without kid".into()))?.to_string();
            if let Some(u) = s("use")
                && u != "sig"
            {
                return Err(JwtError::Jwks(format!("key {kid}: use={u}")));
            }
            let key = match (s("kty"), s("crv")) {
                (Some("OKP"), Some("Ed25519")) => {
                    let x: [u8; 32] = b64(s("x").unwrap_or(""))?.try_into().map_err(|_| JwtError::Jwks("bad x".into()))?;
                    Key::Ed25519(ed25519_dalek::VerifyingKey::from_bytes(&x).map_err(|e| JwtError::Jwks(e.to_string()))?)
                }
                (Some("EC"), Some("P-256")) => {
                    let x = b64(s("x").unwrap_or(""))?;
                    let y = b64(s("y").unwrap_or(""))?;
                    if x.len() != 32 || y.len() != 32 {
                        return Err(JwtError::Jwks("bad ec point".into()));
                    }
                    let mut sec1 = vec![4u8];
                    sec1.extend_from_slice(&x);
                    sec1.extend_from_slice(&y);
                    Key::Es256(p256::ecdsa::VerifyingKey::from_sec1_bytes(&sec1).map_err(|e| JwtError::Jwks(e.to_string()))?)
                }
                (Some("RSA"), _) => {
                    let n = rsa::BigUint::from_bytes_be(&b64(s("n").unwrap_or(""))?);
                    let e = rsa::BigUint::from_bytes_be(&b64(s("e").unwrap_or(""))?);
                    let pk = rsa::RsaPublicKey::new(n, e).map_err(|e| JwtError::Jwks(e.to_string()))?;
                    if rsa::traits::PublicKeyParts::size(&pk) < 256 {
                        return Err(JwtError::Jwks(format!("key {kid}: RSA key shorter than 2048 bits")));
                    }
                    Key::Rs256(rsa::pkcs1v15::VerifyingKey::new(pk))
                }
                _ => return Err(JwtError::Jwks(format!("key {kid}: unsupported kty/crv"))),
            };
            if let Some(a) = s("alg")
                && a != key.alg()
            {
                return Err(JwtError::Jwks(format!("key {kid}: alg {a} does not match key type")));
            }
            keys.insert(kid, key);
        }
        if keys.is_empty() {
            return Err(JwtError::Jwks("no keys".into()));
        }
        Ok(JwtVerifier {
            keys,
            issuer: issuer.into(),
            audience: audience.into(),
            tenant_claim: tenant_claim.into(),
            leeway_secs: 60,
        })
    }

    pub fn verify(&self, token: &str, now_secs: i64) -> Result<Claims, JwtError> {
        let mut parts = token.split('.');
        let (h, p, sig) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(p), Some(s), None) => (h, p, s),
            _ => return Err(JwtError::Format),
        };
        let header: Value = serde_json::from_slice(&b64(h)?).map_err(|_| JwtError::Format)?;
        if header.get("crit").is_some() {
            return Err(JwtError::Critical);
        }
        let alg = header.get("alg").and_then(Value::as_str).ok_or(JwtError::Format)?;
        let kid = header.get("kid").and_then(Value::as_str).ok_or(JwtError::UnknownKey)?;
        let key = self.keys.get(kid).ok_or(JwtError::UnknownKey)?;
        if alg != key.alg() {
            return Err(JwtError::UnsupportedAlg);
        }
        let signed = &token.as_bytes()[..h.len() + 1 + p.len()];
        let sig = b64(sig)?;
        let ok = match key {
            Key::Ed25519(k) => {
                let s = ed25519_dalek::Signature::from_slice(&sig).map_err(|_| JwtError::BadSignature)?;
                k.verify_strict(signed, &s).is_ok()
            }
            Key::Es256(k) => {
                use p256::ecdsa::signature::Verifier;
                let s = p256::ecdsa::Signature::from_slice(&sig).map_err(|_| JwtError::BadSignature)?;
                k.verify(signed, &s).is_ok()
            }
            Key::Rs256(k) => {
                use rsa::signature::Verifier;
                let s = rsa::pkcs1v15::Signature::try_from(sig.as_slice()).map_err(|_| JwtError::BadSignature)?;
                k.verify(signed, &s).is_ok()
            }
        };
        if !ok {
            return Err(JwtError::BadSignature);
        }
        let c: Value = serde_json::from_slice(&b64(p)?).map_err(|_| JwtError::Format)?;
        let num = |n: &str| c.get(n).and_then(Value::as_i64);
        let exp = num("exp").ok_or(JwtError::Format)?;
        if now_secs >= exp + self.leeway_secs {
            return Err(JwtError::Expired);
        }
        if let Some(nbf) = num("nbf")
            && now_secs + self.leeway_secs < nbf
        {
            return Err(JwtError::NotYetValid);
        }
        if c.get("iss").and_then(Value::as_str) != Some(self.issuer.as_str()) {
            return Err(JwtError::Issuer);
        }
        let aud_ok = match c.get("aud") {
            Some(Value::String(a)) => *a == self.audience,
            Some(Value::Array(a)) => a.iter().any(|x| x.as_str() == Some(self.audience.as_str())),
            _ => false,
        };
        if !aud_ok {
            return Err(JwtError::Audience);
        }
        let tenant = c
            .get(&self.tenant_claim)
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or(JwtError::MissingTenant)?
            .to_string();
        let subject = c.get("sub").and_then(Value::as_str).unwrap_or("").to_string();
        // scope は空白区切りの文字列（RFC 8693 の scope）か配列。知らない scope は無視する。
        let scopes = match c.get("scope").or_else(|| c.get("scp")) {
            Some(Value::String(s)) => s.split(' ').filter_map(Scope::parse).collect(),
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).filter_map(Scope::parse).collect(),
            _ => BTreeSet::new(),
        };
        Ok(Claims { subject, tenant, scopes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;

    fn make(sk: &ed25519_dalek::SigningKey, header: &str, claims: &str) -> String {
        let h = URL_SAFE_NO_PAD.encode(header);
        let p = URL_SAFE_NO_PAD.encode(claims);
        let sig = sk.sign(format!("{h}.{p}").as_bytes());
        format!("{h}.{p}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
    }

    fn verifier(sk: &ed25519_dalek::SigningKey) -> JwtVerifier {
        let x = URL_SAFE_NO_PAD.encode(sk.verifying_key().to_bytes());
        let jwks = format!(r#"{{"keys":[{{"kty":"OKP","crv":"Ed25519","kid":"k1","x":"{x}","alg":"EdDSA"}}]}}"#);
        JwtVerifier::from_jwks(jwks.as_bytes(), "iss1", "senca", "tenant").unwrap()
    }

    #[test]
    fn ed25519_accept_and_reject() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let v = verifier(&sk);
        let ok = make(&sk, r#"{"alg":"EdDSA","kid":"k1"}"#, r#"{"iss":"iss1","aud":"senca","exp":2000,"sub":"u","tenant":"t1","scope":"stream:read publish bogus"}"#);
        let c = v.verify(&ok, 1000).unwrap();
        assert_eq!(c.tenant, "t1");
        assert_eq!(c.scopes.len(), 2);
        assert_eq!(v.verify(&ok, 2061).unwrap_err(), JwtError::Expired);
        assert!(v.verify(&ok, 2059).is_ok());
        let none = make(&sk, r#"{"alg":"none","kid":"k1"}"#, r#"{"iss":"iss1","aud":"senca","exp":2000,"tenant":"t1"}"#);
        assert_eq!(v.verify(&none, 1000).unwrap_err(), JwtError::UnsupportedAlg);
        let hs = make(&sk, r#"{"alg":"HS256","kid":"k1"}"#, r#"{"iss":"iss1","aud":"senca","exp":2000,"tenant":"t1"}"#);
        assert_eq!(v.verify(&hs, 1000).unwrap_err(), JwtError::UnsupportedAlg);
        let crit = make(&sk, r#"{"alg":"EdDSA","kid":"k1","crit":["x"]}"#, r#"{"iss":"iss1","aud":"senca","exp":2000,"tenant":"t1"}"#);
        assert_eq!(v.verify(&crit, 1000).unwrap_err(), JwtError::Critical);
        let aud = make(&sk, r#"{"alg":"EdDSA","kid":"k1"}"#, r#"{"iss":"iss1","aud":["x","senca"],"exp":2000,"tenant":"t1"}"#);
        assert!(v.verify(&aud, 1000).is_ok());
        let noten = make(&sk, r#"{"alg":"EdDSA","kid":"k1"}"#, r#"{"iss":"iss1","aud":"senca","exp":2000}"#);
        assert_eq!(v.verify(&noten, 1000).unwrap_err(), JwtError::MissingTenant);
        let other = ed25519_dalek::SigningKey::from_bytes(&[8u8; 32]);
        let forged = make(&other, r#"{"alg":"EdDSA","kid":"k1"}"#, r#"{"iss":"iss1","aud":"senca","exp":2000,"tenant":"t1"}"#);
        assert_eq!(v.verify(&forged, 1000).unwrap_err(), JwtError::BadSignature);
        assert_eq!(v.verify("a.b", 1000).unwrap_err(), JwtError::Format);
    }
}
