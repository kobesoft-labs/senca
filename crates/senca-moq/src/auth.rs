//! namespace ごとの認可（仕様 6.5・9.2）。
//!
//! publish 権限と subscribe 権限を要求ごとに確かめる。tenant の境界は namespace の **要素単位**の
//! 前方一致で判定し、文字列の前方一致だけでは判定しない（`tenantA` は `tenantAB` の前置ではない）。

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::Namespace;
use crate::params::Token;

/// 確かめる操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// PUBLISH_NAMESPACE・PUBLISH（出版）。
    Publish,
    /// SUBSCRIBE・FETCH・TRACK_STATUS（購読）。
    Subscribe,
    /// SUBSCRIBE_NAMESPACE（namespace の一覧）。
    Discover,
}

/// 認可の材料。
#[derive(Debug)]
pub struct AuthContext<'a> {
    /// SETUP の AUTHORIZATION TOKEN。
    pub setup_tokens: &'a [Token],
    /// 要求の AUTHORIZATION TOKEN。
    pub request_tokens: &'a [Token],
    /// SETUP の PATH・WebTransport の :path。
    pub path: Option<&'a str>,
}

impl AuthContext<'_> {
    fn tokens(&self) -> impl Iterator<Item = &Token> {
        self.request_tokens.iter().chain(self.setup_tokens.iter())
    }
}

/// 認可の判断。
pub trait Authorizer: Send + Sync {
    /// 許すなら `true`。`track` は track 単位の操作のときだけ。
    fn authorize(&self, ctx: &AuthContext<'_>, action: Action, namespace: &Namespace, track: Option<&[u8]>) -> bool;
}

/// すべて許す（試験・閉じた環境用。公開環境では使わない）。
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAll;

impl Authorizer for AllowAll {
    fn authorize(&self, _: &AuthContext<'_>, _: Action, _: &Namespace, _: Option<&[u8]>) -> bool {
        true
    }
}

/// 一つの規則: この namespace の前置（要素単位）の下で、トークンを持つ者に許す操作。
#[derive(Clone, Debug)]
pub struct Rule {
    pub prefix: Namespace,
    /// 出版を許すトークンの SHA-256。
    pub publish: Vec<[u8; 32]>,
    /// 購読・一覧を許すトークンの SHA-256。
    pub subscribe: Vec<[u8; 32]>,
    /// トークンなしで購読・一覧を許す。
    pub public_subscribe: bool,
}

/// namespace の前置ごとの規則による認可。最も長く一致した規則だけを使う（既定は拒否）。
#[derive(Clone, Debug, Default)]
pub struct NamespaceAcl {
    pub rules: Vec<Rule>,
}

/// トークンの値の SHA-256（規則に入れる値）。
pub fn token_hash(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}

impl NamespaceAcl {
    fn rule_for(&self, ns: &Namespace) -> Option<&Rule> {
        self.rules.iter().filter(|r| ns.starts_with(&r.prefix)).max_by_key(|r| r.prefix.0.len())
    }
}

fn holds(ctx: &AuthContext<'_>, allowed: &[[u8; 32]]) -> bool {
    ctx.tokens().any(|t| {
        let h = token_hash(&t.value);
        // 定数時間で比べる
        allowed.iter().fold(false, |acc, a| acc | bool::from(a.ct_eq(&h)))
    })
}

impl Authorizer for NamespaceAcl {
    fn authorize(&self, ctx: &AuthContext<'_>, action: Action, namespace: &Namespace, _track: Option<&[u8]>) -> bool {
        let Some(rule) = self.rule_for(namespace) else { return false };
        match action {
            Action::Publish => holds(ctx, &rule.publish),
            Action::Subscribe | Action::Discover => rule.public_subscribe || holds(ctx, &rule.subscribe) || holds(ctx, &rule.publish),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn acl_is_per_field_and_per_action() {
        let acl = NamespaceAcl {
            rules: vec![
                Rule {
                    prefix: Namespace::from_path("tenantA"),
                    publish: vec![token_hash(b"pubA")],
                    subscribe: vec![token_hash(b"subA")],
                    public_subscribe: false,
                },
                Rule {
                    prefix: Namespace::from_path("public"),
                    publish: vec![token_hash(b"pubP")],
                    subscribe: vec![],
                    public_subscribe: true,
                },
            ],
        };
        let t = |v: &'static [u8]| {
            vec![Token {
                token_type: 0,
                value: Bytes::from_static(v),
            }]
        };
        fn c(toks: &[Token]) -> AuthContext<'_> {
            AuthContext {
                setup_tokens: &[],
                request_tokens: toks,
                path: None,
            }
        }
        let pub_a = t(b"pubA");
        let sub_a = t(b"subA");
        let a = Namespace::from_path("tenantA/live");
        let ab = Namespace::from_path("tenantAB/live");
        assert!(acl.authorize(&c(&pub_a), Action::Publish, &a, None));
        assert!(!acl.authorize(&c(&sub_a), Action::Publish, &a, None));
        assert!(acl.authorize(&c(&sub_a), Action::Subscribe, &a, Some(b"video")));
        // 文字列の前方一致では通らない
        assert!(!acl.authorize(&c(&pub_a), Action::Publish, &ab, None));
        assert!(!acl.authorize(&c(&sub_a), Action::Subscribe, &ab, None));
        // 公開の namespace は購読だけトークンなしで
        let none = Vec::new();
        assert!(acl.authorize(&c(&none), Action::Subscribe, &Namespace::from_path("public/x"), None));
        assert!(!acl.authorize(&c(&none), Action::Publish, &Namespace::from_path("public/x"), None));
    }
}
