//! SENCA の MoQ Transport（Media over QUIC Transport）。sans-io。
//!
//! # 固定した版（仕様 6.5、`DRAFT.md`）
//!
//! - **draft-ietf-moq-transport-22**（2026-10-01、datatracker の最新版）。ALPN は `moqt-22`、
//!   WebTransport では `WT-Available-Protocols` に `moqt-22`。版の交渉は ALPN だけで行い、
//!   他の版（未知の ALPN）は TLS の no_application_protocol で拒否する（推測で接続しない）。
//! - 可変長整数は draft-22 の「先頭の 1 の数」形式（1〜9 バイト、[`varint`]）で、QUIC の varint とは別物。
//!
//! # 構成
//!
//! - [`message`]・[`params`]・[`data`]: ワイヤー形式（制御メッセージ・パラメーター・データ stream と datagram）。
//! - [`Session`]: 一つの MoQ セッションの状態機械。stream の読み書きは [`Transport`] を通す
//!   （native QUIC は [`transport::QuicTransport`]、WebTransport は [`transport::WtTransport`]）。
//! - [`Relay`]: 複数のセッションをつなぐ中継。namespace ごとの認可（[`auth`]）と、
//!   不変の cache（同じ object ID には同じ中身、書き換えを拒否、[`cache`]）を持つ。
//! - [`mapping`]: メディアの対応づけ（hang の catalog JSON + CMAF の fragment、`DRAFT.md`）。
//!
//! relay は object の中身（payload）を読まず・変えない。QUIC の区間暗号化（hop-by-hop）とは別に、
//! E2EE の payload は relay を素通りする（仕様 6.5）。

pub mod auth;
pub mod cache;
mod client;
mod coding;
pub mod data;
mod error;
pub mod mapping;
pub mod message;
pub mod params;
mod relay;
mod server;
mod session;
pub mod transport;
pub mod varint;

pub use error::{MoqError, RequestErrorCode, SessionErrorCode, publish_done, reset};
pub use client::MoqClient;
pub use relay::{Relay, RelayConfig, RelayStats, SessionId};
pub use server::{MoqServer, offers_protocol};
pub use session::{Delivery, Object, ObjectStatus, RangeEnd, Role, Session, SessionConfig, SessionEvent, SubscribeArgs, filter_end, filter_start};
pub use transport::Transport;

/// 固定した draft の番号。
pub const DRAFT: u32 = 22;
/// native QUIC の ALPN（draft-22 6.2）。
pub const ALPN: &[u8] = b"moqt-22";
/// WebTransport の WT-Available-Protocols に入れる値。
pub const WT_PROTOCOL: &str = "moqt-22";

/// Track Namespace（tuple、各要素は 1 バイト以上、最大 32 要素）。
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Namespace(pub Vec<bytes::Bytes>);

impl Namespace {
    /// `/` 区切りの文字列から（空要素は除く）。
    pub fn from_path(s: &str) -> Self {
        Self(
            s.split('/')
                .filter(|p| !p.is_empty())
                .map(|p| bytes::Bytes::copy_from_slice(p.as_bytes()))
                .collect(),
        )
    }

    /// 要素単位の前方一致（`["a","b"]` は `["a","bc"]` の前置ではない）。
    pub fn starts_with(&self, prefix: &Namespace) -> bool {
        prefix.0.len() <= self.0.len() && self.0.iter().zip(prefix.0.iter()).all(|(a, b)| a == b)
    }

    /// `prefix` を取り除いた残り。
    pub fn suffix_after(&self, prefix: &Namespace) -> Option<Namespace> {
        self.starts_with(prefix).then(|| Namespace(self.0[prefix.0.len()..].to_vec()))
    }

    pub fn join(&self, suffix: &Namespace) -> Namespace {
        let mut v = self.0.clone();
        v.extend(suffix.0.iter().cloned());
        Namespace(v)
    }

    /// 記録用の表現（draft-22 8.8: 英数字と `_` 以外は `.xx`、要素は `-` 区切り）。
    pub fn render(&self) -> String {
        self.0.iter().map(|f| render_field(f)).collect::<Vec<_>>().join("-")
    }

    pub fn total_len(&self) -> usize {
        self.0.iter().map(|f| f.len()).sum()
    }
}

/// 名前を安全な文字列に（ログ用、draft-22 8.8）。
pub fn render_field(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len());
    for &c in b {
        if c.is_ascii_alphanumeric() || c == b'_' {
            s.push(c as char);
        } else {
            s.push_str(&format!(".{c:02x}"));
        }
    }
    s
}

impl std::fmt::Debug for Namespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Namespace({})", self.render())
    }
}

/// Location（group, object）。辞書順で比べる（draft-22 8.2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Location {
    pub group: u64,
    pub object: u64,
}

impl Location {
    pub const fn new(group: u64, object: u64) -> Self {
        Self { group, object }
    }
}

/// 完全な track 名（namespace + track 名）。
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct FullTrackName {
    pub namespace: Namespace,
    pub name: bytes::Bytes,
}

impl FullTrackName {
    pub fn new(namespace: Namespace, name: impl Into<bytes::Bytes>) -> Self {
        Self {
            namespace,
            name: name.into(),
        }
    }

    pub fn render(&self) -> String {
        format!("{}--{}", self.namespace.render(), render_field(&self.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_prefix_is_per_field() {
        let a = Namespace::from_path("tenantA/live");
        let ab = Namespace::from_path("tenantAB/live");
        let p = Namespace::from_path("tenantA");
        assert!(a.starts_with(&p));
        assert!(!ab.starts_with(&p));
        assert_eq!(
            FullTrackName::new(Namespace::from_path("example.net/team2/project_x"), "report").render(),
            "example.2enet-team2-project_x--report"
        );
    }
}
