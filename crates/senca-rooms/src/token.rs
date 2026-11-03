//! 参加トークン（JWT 互換、RFC 7519）。SENCA 自身の署名鍵（Ed25519 か P-256）で発行し、検証する。
//!
//! - 算法は鍵が決める（EdDSA・ES256）。ヘッダーの `alg` が鍵と違えば拒否。`none`・HS 系（共有鍵）・RS 系・
//!   `crit` は拒否する（senca-control の JWT 検証と同じ形）。
//! - 期限（`exp`）必須、`nbf`、`iss`・`aud`。時計のずれは `leeway_secs`（既定 30 秒）まで。
//! - 寿命（`exp - iat`、`iat` が無ければ `exp - now`）が `max_ttl_secs`（既定 24 時間）を超えるものは拒否。
//!
//! クレーム:
//!
//! ```json
//! {"iss":"senca","aud":"senca-rooms","exp":1767225600,"iat":1767222000,"jti":"...",
//!  "room":"daily","role":"sendrecv","client_id":"alice","metadata":{"name":"Alice"},
//!  "permissions":{"publish":["audio","video"],"subscribe":true,"data":["#chat"]}}
//! ```

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::{Kind, Role, label_matches};

/// 参加者に許すこと。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    /// 送ってよいトラックの種類。
    #[serde(default = "all_kinds")]
    pub publish: Vec<Kind>,
    /// 他の参加者のトラックを受けてよいか。
    #[serde(default = "yes")]
    pub subscribe: bool,
    /// 使ってよい DataChannel のラベルの型（`#*` で全部）。
    #[serde(default = "all_labels")]
    pub data: Vec<String>,
}

fn all_kinds() -> Vec<Kind> {
    vec![Kind::Audio, Kind::Video]
}

fn yes() -> bool {
    true
}

fn all_labels() -> Vec<String> {
    vec!["#*".into()]
}

impl Default for Permissions {
    fn default() -> Self {
        Permissions {
            publish: all_kinds(),
            subscribe: true,
            data: all_labels(),
        }
    }
}

impl Permissions {
    pub fn may_publish(&self, k: Kind) -> bool {
        self.publish.contains(&k)
    }

    pub fn may_use_label(&self, label: &str) -> bool {
        self.data.iter().any(|p| label_matches(p, label))
    }
}

/// `aud` は文字列か配列。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, a: &str) -> bool {
        match self {
            Audience::One(x) => x == a,
            Audience::Many(v) => v.iter().any(|x| x == a),
        }
    }
}

/// 参加トークンのクレーム。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JoinClaims {
    pub iss: String,
    pub aud: Audience,
    pub exp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nbf: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iat: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jti: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    /// 参加してよい部屋。
    pub room: String,
    /// 役割の上限（sendrecv ならどれでも）。
    pub role: Role,
    /// 決めた client_id（あれば connect の値より優先し、違えば拒否）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// 決めた bundle_id（同じ利用者の複数の接続をまとめる。転送フィルターの条件にも使う）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    /// 他の参加者に見せるメタデータ。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Permissions>,
}

/// 検証の誤り。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    Format,
    UnsupportedAlg,
    UnknownKey,
    BadSignature,
    Expired,
    NotYetValid,
    Issuer,
    Audience,
    TooLong,
    Critical,
    Claims(String),
    Jwks(String),
    Key(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Format => f.write_str("malformed token"),
            TokenError::UnsupportedAlg => f.write_str("unsupported or mismatched alg"),
            TokenError::UnknownKey => f.write_str("unknown kid"),
            TokenError::BadSignature => f.write_str("bad signature"),
            TokenError::Expired => f.write_str("expired"),
            TokenError::NotYetValid => f.write_str("not yet valid"),
            TokenError::Issuer => f.write_str("issuer mismatch"),
            TokenError::Audience => f.write_str("audience mismatch"),
            TokenError::TooLong => f.write_str("token lifetime too long"),
            TokenError::Critical => f.write_str("crit header not supported"),
            TokenError::Claims(e) => write!(f, "claims: {e}"),
            TokenError::Jwks(e) => write!(f, "jwks: {e}"),
            TokenError::Key(e) => write!(f, "key: {e}"),
        }
    }
}

impl std::error::Error for TokenError {}

fn b64(s: &str) -> Result<Vec<u8>, TokenError> {
    URL_SAFE_NO_PAD.decode(s).map_err(|_| TokenError::Format)
}

#[derive(Clone)]
enum SignKey {
    Ed25519(Box<ed25519_dalek::SigningKey>),
    Es256(Box<p256::ecdsa::SigningKey>),
}

#[derive(Clone)]
enum VerifyKey {
    Ed25519(ed25519_dalek::VerifyingKey),
    Es256(Box<p256::ecdsa::VerifyingKey>),
}

impl VerifyKey {
    fn alg(&self) -> &'static str {
        match self {
            VerifyKey::Ed25519(_) => "EdDSA",
            VerifyKey::Es256(_) => "ES256",
        }
    }

    fn jwk(&self, kid: &str) -> Value {
        match self {
            VerifyKey::Ed25519(k) => json!({
                "kty": "OKP", "crv": "Ed25519", "kid": kid, "use": "sig", "alg": "EdDSA",
                "x": URL_SAFE_NO_PAD.encode(k.to_bytes()),
            }),
            VerifyKey::Es256(k) => {
                let p = k.to_encoded_point(false);
                json!({
                    "kty": "EC", "crv": "P-256", "kid": kid, "use": "sig", "alg": "ES256",
                    "x": URL_SAFE_NO_PAD.encode(p.x().map(|x| x.to_vec()).unwrap_or_default()),
                    "y": URL_SAFE_NO_PAD.encode(p.y().map(|y| y.to_vec()).unwrap_or_default()),
                })
            }
        }
    }
}

/// トークンを発行する（SENCA の署名鍵）。
#[derive(Clone)]
pub struct TokenSigner {
    kid: String,
    key: SignKey,
}

impl std::fmt::Debug for TokenSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSigner").field("kid", &self.kid).finish_non_exhaustive()
    }
}

impl TokenSigner {
    /// Ed25519 の秘密鍵（32 byte の seed）から。
    pub fn ed25519(kid: &str, seed: &[u8; 32]) -> TokenSigner {
        TokenSigner {
            kid: kid.into(),
            key: SignKey::Ed25519(Box::new(ed25519_dalek::SigningKey::from_bytes(seed))),
        }
    }

    /// P-256 の秘密鍵（32 byte のスカラー）から。
    pub fn es256(kid: &str, secret: &[u8; 32]) -> Result<TokenSigner, TokenError> {
        let k = p256::ecdsa::SigningKey::from_slice(secret).map_err(|e| TokenError::Key(e.to_string()))?;
        Ok(TokenSigner {
            kid: kid.into(),
            key: SignKey::Es256(Box::new(k)),
        })
    }

    /// 新しい Ed25519 の鍵を作る。
    pub fn generate_ed25519(kid: &str) -> Result<TokenSigner, TokenError> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| TokenError::Key(e.to_string()))?;
        Ok(TokenSigner::ed25519(kid, &seed))
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    fn verify_key(&self) -> VerifyKey {
        match &self.key {
            SignKey::Ed25519(k) => VerifyKey::Ed25519(k.verifying_key()),
            SignKey::Es256(k) => VerifyKey::Es256(Box::new(*k.verifying_key())),
        }
    }

    /// 公開鍵の JWKS（`{"keys":[...]}`）。
    pub fn jwks(&self) -> Value {
        json!({"keys": [self.verify_key().jwk(&self.kid)]})
    }

    /// 任意のクレームに署名する（試験・他の用途）。
    pub fn sign_value(&self, claims: &Value) -> Result<String, TokenError> {
        let alg = self.verify_key().alg();
        let h = URL_SAFE_NO_PAD.encode(json!({"alg": alg, "kid": self.kid, "typ": "JWT"}).to_string());
        let p = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).map_err(|e| TokenError::Claims(e.to_string()))?);
        let input = format!("{h}.{p}");
        let sig = match &self.key {
            SignKey::Ed25519(k) => {
                use ed25519_dalek::Signer;
                k.sign(input.as_bytes()).to_bytes().to_vec()
            }
            SignKey::Es256(k) => {
                use p256::ecdsa::signature::Signer;
                let s: p256::ecdsa::Signature = k.sign(input.as_bytes());
                s.to_bytes().to_vec()
            }
        };
        Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig)))
    }

    /// 参加トークンを発行する。
    pub fn sign(&self, claims: &JoinClaims) -> Result<String, TokenError> {
        self.sign_value(&serde_json::to_value(claims).map_err(|e| TokenError::Claims(e.to_string()))?)
    }

    /// この鍵だけを信頼する検証器。
    pub fn verifier(&self, issuer: &str, audience: &str) -> TokenVerifier {
        let mut keys = BTreeMap::new();
        keys.insert(self.kid.clone(), self.verify_key());
        TokenVerifier {
            keys,
            issuer: issuer.into(),
            audience: audience.into(),
            leeway_secs: 30,
            max_ttl_secs: 86_400,
        }
    }
}

/// トークンを検証する。
#[derive(Clone)]
pub struct TokenVerifier {
    keys: BTreeMap<String, VerifyKey>,
    pub issuer: String,
    pub audience: String,
    pub leeway_secs: i64,
    pub max_ttl_secs: i64,
}

impl std::fmt::Debug for TokenVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenVerifier")
            .field("kids", &self.keys.keys().collect::<Vec<_>>())
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .finish_non_exhaustive()
    }
}

impl TokenVerifier {
    /// JWKS から作る。EdDSA（OKP/Ed25519）と ES256（EC/P-256）だけ。共有鍵（oct）・RSA は拒否。
    pub fn from_jwks(jwks: &[u8], issuer: &str, audience: &str) -> Result<TokenVerifier, TokenError> {
        let v: Value = serde_json::from_slice(jwks).map_err(|e| TokenError::Jwks(e.to_string()))?;
        let arr = v
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| TokenError::Jwks("no keys".into()))?;
        let mut keys = BTreeMap::new();
        for k in arr {
            let s = |n: &str| k.get(n).and_then(Value::as_str);
            let kid = s("kid").ok_or_else(|| TokenError::Jwks("key without kid".into()))?.to_string();
            if s("use").is_some_and(|u| u != "sig") {
                return Err(TokenError::Jwks(format!("key {kid}: not a signing key")));
            }
            let key = match (s("kty"), s("crv")) {
                (Some("OKP"), Some("Ed25519")) => {
                    let x: [u8; 32] = b64(s("x").unwrap_or(""))?
                        .try_into()
                        .map_err(|_| TokenError::Jwks("bad x".into()))?;
                    VerifyKey::Ed25519(
                        ed25519_dalek::VerifyingKey::from_bytes(&x).map_err(|e| TokenError::Jwks(e.to_string()))?,
                    )
                }
                (Some("EC"), Some("P-256")) => {
                    let x = b64(s("x").unwrap_or(""))?;
                    let y = b64(s("y").unwrap_or(""))?;
                    if x.len() != 32 || y.len() != 32 {
                        return Err(TokenError::Jwks("bad ec point".into()));
                    }
                    let mut sec1 = vec![4u8];
                    sec1.extend_from_slice(&x);
                    sec1.extend_from_slice(&y);
                    VerifyKey::Es256(Box::new(
                        p256::ecdsa::VerifyingKey::from_sec1_bytes(&sec1).map_err(|e| TokenError::Jwks(e.to_string()))?,
                    ))
                }
                (kty, _) => {
                    return Err(TokenError::Jwks(format!(
                        "key {kid}: kty {} not allowed (EdDSA and ES256 only)",
                        kty.unwrap_or("?")
                    )));
                }
            };
            if s("alg").is_some_and(|a| a != key.alg()) {
                return Err(TokenError::Jwks(format!("key {kid}: alg does not match key type")));
            }
            keys.insert(kid, key);
        }
        if keys.is_empty() {
            return Err(TokenError::Jwks("no keys".into()));
        }
        Ok(TokenVerifier {
            keys,
            issuer: issuer.into(),
            audience: audience.into(),
            leeway_secs: 30,
            max_ttl_secs: 86_400,
        })
    }

    /// 検証してクレームを返す。
    pub fn verify(&self, token: &str, now_secs: i64) -> Result<JoinClaims, TokenError> {
        if token.len() > 16 * 1024 {
            return Err(TokenError::Format);
        }
        let mut parts = token.split('.');
        let (h, p, sig) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(p), Some(s), None) => (h, p, s),
            _ => return Err(TokenError::Format),
        };
        let header: Value = serde_json::from_slice(&b64(h)?).map_err(|_| TokenError::Format)?;
        if header.get("crit").is_some() {
            return Err(TokenError::Critical);
        }
        let alg = header.get("alg").and_then(Value::as_str).ok_or(TokenError::Format)?;
        if !matches!(alg, "EdDSA" | "ES256") {
            return Err(TokenError::UnsupportedAlg);
        }
        let kid = header.get("kid").and_then(Value::as_str).ok_or(TokenError::UnknownKey)?;
        let key = self.keys.get(kid).ok_or(TokenError::UnknownKey)?;
        if alg != key.alg() {
            return Err(TokenError::UnsupportedAlg);
        }
        let signed = &token.as_bytes()[..h.len() + 1 + p.len()];
        let sig = b64(sig)?;
        let ok = match key {
            VerifyKey::Ed25519(k) => {
                let s = ed25519_dalek::Signature::from_slice(&sig).map_err(|_| TokenError::BadSignature)?;
                k.verify_strict(signed, &s).is_ok()
            }
            VerifyKey::Es256(k) => {
                use p256::ecdsa::signature::Verifier;
                let s = p256::ecdsa::Signature::from_slice(&sig).map_err(|_| TokenError::BadSignature)?;
                k.verify(signed, &s).is_ok()
            }
        };
        if !ok {
            return Err(TokenError::BadSignature);
        }
        let claims: JoinClaims =
            serde_json::from_slice(&b64(p)?).map_err(|e| TokenError::Claims(e.to_string()))?;
        if now_secs >= claims.exp + self.leeway_secs {
            return Err(TokenError::Expired);
        }
        if claims.nbf.is_some_and(|nbf| now_secs + self.leeway_secs < nbf) {
            return Err(TokenError::NotYetValid);
        }
        // 寿命は「今から exp まで」で測る（iat を未来に偽っても延ばせない）。iat が未来すぎるものも拒む
        if claims.iat.is_some_and(|iat| iat > now_secs.saturating_add(self.leeway_secs)) {
            return Err(TokenError::NotYetValid);
        }
        if claims.exp.saturating_sub(now_secs) > self.max_ttl_secs {
            return Err(TokenError::TooLong);
        }
        if claims.iss != self.issuer {
            return Err(TokenError::Issuer);
        }
        if !claims.aud.contains(&self.audience) {
            return Err(TokenError::Audience);
        }
        if !crate::model::valid_room_id(&claims.room) {
            return Err(TokenError::Claims("room".into()));
        }
        Ok(claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(exp: i64) -> JoinClaims {
        JoinClaims {
            iss: "senca".into(),
            aud: Audience::One("senca-rooms".into()),
            exp,
            nbf: None,
            iat: Some(exp - 600),
            jti: Some("j1".into()),
            sub: None,
            room: "daily".into(),
            role: Role::Sendrecv,
            client_id: Some("alice".into()),
            bundle_id: None,
            metadata: Some(json!({"name": "Alice"})),
            permissions: Some(Permissions {
                publish: vec![Kind::Audio],
                subscribe: true,
                data: vec!["#chat".into()],
            }),
        }
    }

    #[test]
    fn ed25519_and_es256_round_trip_through_jwks() {
        let ed = TokenSigner::ed25519("k-ed", &[3u8; 32]);
        let es = TokenSigner::es256("k-es", &[5u8; 32]).unwrap();
        for s in [&ed, &es] {
            let jwks = serde_json::to_vec(&s.jwks()).unwrap();
            let v = TokenVerifier::from_jwks(&jwks, "senca", "senca-rooms").unwrap();
            let t = s.sign(&claims(2000)).unwrap();
            let c = v.verify(&t, 1500).unwrap();
            assert_eq!(c.room, "daily");
            assert_eq!(c.client_id.as_deref(), Some("alice"));
            assert!(c.permissions.as_ref().unwrap().may_publish(Kind::Audio));
            assert!(!c.permissions.as_ref().unwrap().may_publish(Kind::Video));
            assert!(c.permissions.as_ref().unwrap().may_use_label("#chat"));
            assert!(!c.permissions.as_ref().unwrap().may_use_label("#other"));
            assert_eq!(v.verify(&t, 2030), Err(TokenError::Expired));
            assert!(v.verify(&t, 2029).is_ok());
        }
        // 鍵が違う
        let other = TokenSigner::ed25519("k-ed", &[4u8; 32]);
        let t = other.sign(&claims(2000)).unwrap();
        assert_eq!(ed.verifier("senca", "senca-rooms").verify(&t, 1500), Err(TokenError::BadSignature));
    }

    #[test]
    fn lifetime_is_measured_from_now_not_iat() {
        let s = TokenSigner::ed25519("k1", &[9u8; 32]);
        let v = s.verifier("senca", "senca-rooms");
        let now = 1_000_000;
        // iat を exp の直前に偽った 10 年のトークン
        let mut c = claims(now + 10 * 365 * 86_400);
        c.iat = Some(c.exp - 60);
        let t = s.sign(&c).unwrap();
        assert!(v.verify(&t, now).is_err());
        // iat が未来すぎる
        let mut c = claims(now + 600);
        c.iat = Some(now + 3600);
        let t = s.sign(&c).unwrap();
        assert!(v.verify(&t, now).is_err());
        // 普通のもの
        let mut c = claims(now + 600);
        c.iat = Some(now);
        assert!(v.verify(&s.sign(&c).unwrap(), now).is_ok());
    }

    #[test]
    fn rejects_bad_tokens() {
        let s = TokenSigner::ed25519("k1", &[9u8; 32]);
        let v = s.verifier("senca", "senca-rooms");
        let enc = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
        let body = serde_json::to_value(claims(2000)).unwrap();
        let good = s.sign(&claims(2000)).unwrap();
        let sig = good.rsplit('.').next().unwrap();
        // alg=none・HS256・ES256 と鍵の違い・crit・kid の無いもの
        for (h, want) in [
            (json!({"alg":"none","kid":"k1"}), TokenError::UnsupportedAlg),
            (json!({"alg":"HS256","kid":"k1"}), TokenError::UnsupportedAlg),
            (json!({"alg":"RS256","kid":"k1"}), TokenError::UnsupportedAlg),
            (json!({"alg":"ES256","kid":"k1"}), TokenError::UnsupportedAlg),
            (json!({"alg":"EdDSA","kid":"k1","crit":["x"]}), TokenError::Critical),
            (json!({"alg":"EdDSA"}), TokenError::UnknownKey),
            (json!({"alg":"EdDSA","kid":"k2"}), TokenError::UnknownKey),
        ] {
            let t = format!("{}.{}.{sig}", enc(&h), enc(&body));
            assert_eq!(v.verify(&t, 1500), Err(want), "{h}");
        }
        // 中身を書き換えると署名が合わない
        let mut tampered = body.clone();
        tampered["role"] = json!("sendonly");
        let parts: Vec<&str> = good.split('.').collect();
        let t = format!("{}.{}.{}", parts[0], enc(&tampered), parts[2]);
        assert_eq!(v.verify(&t, 1500), Err(TokenError::BadSignature));
        assert_eq!(v.verify("a.b", 1500), Err(TokenError::Format));
        // iss・aud・nbf・寿命
        let mut c = claims(2000);
        c.iss = "other".into();
        assert_eq!(v.verify(&s.sign(&c).unwrap(), 1500), Err(TokenError::Issuer));
        let mut c = claims(2000);
        c.aud = Audience::Many(vec!["x".into(), "senca-rooms".into()]);
        assert!(v.verify(&s.sign(&c).unwrap(), 1500).is_ok());
        c.aud = Audience::Many(vec!["x".into()]);
        assert_eq!(v.verify(&s.sign(&c).unwrap(), 1500), Err(TokenError::Audience));
        let mut c = claims(2000);
        c.nbf = Some(1600);
        assert_eq!(v.verify(&s.sign(&c).unwrap(), 1500), Err(TokenError::NotYetValid));
        let mut c = claims(200_000);
        c.iat = Some(1000);
        assert_eq!(v.verify(&s.sign(&c).unwrap(), 1500), Err(TokenError::TooLong));
        let mut c = claims(2000);
        c.room = "../x".into();
        assert!(matches!(v.verify(&s.sign(&c).unwrap(), 1500), Err(TokenError::Claims(_))));
    }

    #[test]
    fn jwks_rejects_shared_and_rsa_keys() {
        for jwks in [
            r#"{"keys":[{"kty":"oct","kid":"h","k":"c2VjcmV0"}]}"#,
            r#"{"keys":[{"kty":"RSA","kid":"r","n":"AQAB","e":"AQAB"}]}"#,
            r#"{"keys":[{"kty":"OKP","crv":"Ed25519","x":"AAAA"}]}"#,
            r#"{"keys":[]}"#,
        ] {
            assert!(TokenVerifier::from_jwks(jwks.as_bytes(), "i", "a").is_err(), "{jwks}");
        }
        let s = TokenSigner::ed25519("k", &[1u8; 32]);
        let mut j = s.jwks();
        j["keys"][0]["alg"] = json!("ES256");
        assert!(TokenVerifier::from_jwks(j.to_string().as_bytes(), "i", "a").is_err());
    }
}
