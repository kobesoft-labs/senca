//! 管理 API の JWT の検証（RFC 7519）と JWKS の読み込み。
//!
//! 先頭 1 バイトで方式を選ぶ:
//! - 0: 残りをそのまま token として検証する（壊れた token・署名の偽造）。
//! - 1: 残りを JWKS として読む。
//! - 2: 残りを 0x00 で header と claims の JSON に分け、試験用の鍵で正しく署名してから検証する
//!   （署名の先の header・claims の検査まで届かせる）。
#![no_main]
use std::sync::OnceLock;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, SigningKey};
use libfuzzer_sys::fuzz_target;
use senca_control::jwt::JwtVerifier;

fn key() -> &'static SigningKey {
    static K: OnceLock<SigningKey> = OnceLock::new();
    K.get_or_init(|| SigningKey::from_bytes(&[7u8; 32]))
}

fn verifier() -> &'static JwtVerifier {
    static V: OnceLock<JwtVerifier> = OnceLock::new();
    V.get_or_init(|| {
        let x = URL_SAFE_NO_PAD.encode(key().verifying_key().to_bytes());
        let jwks = format!(r#"{{"keys":[{{"kty":"OKP","crv":"Ed25519","kid":"k1","x":"{x}","alg":"EdDSA","use":"sig"}}]}}"#);
        JwtVerifier::from_jwks(jwks.as_bytes(), "iss1", "senca", "tenant").expect("jwks")
    })
}

const NOW: i64 = 1_800_000_000;

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else { return };
    match sel % 3 {
        0 => {
            if let Ok(t) = std::str::from_utf8(rest) {
                let _ = verifier().verify(t, NOW);
            }
        }
        1 => {
            let _ = JwtVerifier::from_jwks(rest, "iss1", "senca", "tenant");
        }
        _ => {
            let (h, c) = match rest.iter().position(|&b| b == 0) {
                Some(i) => (&rest[..i], &rest[i + 1..]),
                None => (&b"{\"alg\":\"EdDSA\",\"kid\":\"k1\"}"[..], rest),
            };
            let h = URL_SAFE_NO_PAD.encode(h);
            let p = URL_SAFE_NO_PAD.encode(c);
            let sig = key().sign(format!("{h}.{p}").as_bytes());
            let token = format!("{h}.{p}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()));
            let _ = verifier().verify(&token, NOW);
        }
    }
});
