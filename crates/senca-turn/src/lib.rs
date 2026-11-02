//! TURN（RFC 8656）のサーバーとクライアント。sans-io（ソケット・時計を持たない）。
//!
//! - [`server::TurnServer`] は UDP の 5-tuple ごとの allocation、長期資格情報（固定利用者と
//!   REST API の一時資格情報）、permission（300 秒）・channel（600 秒）、利用者・テナントごとの上限、
//!   relay ポート範囲、私設・loopback・link-local 宛の拒否（仕様 9.4 の open relay 防止）、
//!   未認証の応答の増幅制限。
//! - [`client::TurnClient`] は Allocate（認証付き）・Refresh・CreatePermission・ChannelBind・Send/Data。
//!   `senca-ice` が relayed 候補に使う。
//! - [`channel`] は ChannelData の枠。
//! - [`credentials`] は REST API（coturn の use-auth-secret 方式）の一時資格情報。
//! - [`policy`] は相手アドレスの許可・拒否。

pub mod channel;
pub mod client;
pub mod credentials;
pub mod policy;
pub mod server;

use std::net::SocketAddr;

use bytes::Bytes;

pub use channel::ChannelData;
pub use client::{TurnClient, TurnClientConfig, TurnClientEvent, TurnInput};
pub use credentials::{rest_api_credentials, rest_api_password};
pub use policy::PeerPolicy;
pub use server::{
    Quotas, RateLimit, RelayIp, RestSecret, StaticUser, TurnServer, TurnServerConfig,
    TurnServerEvent,
};

/// 送るデータグラム。`local` はどのソケットから送るか（サーバーなら待ち受けか relay のアドレス、
/// クライアントなら設定したローカルアドレス）。
#[derive(Clone, PartialEq, Eq)]
pub struct Transmit {
    pub local: SocketAddr,
    pub remote: SocketAddr,
    pub data: Bytes,
}

impl std::fmt::Debug for Transmit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Transmit({} -> {}, {} bytes)",
            self.local,
            self.remote,
            self.data.len()
        )
    }
}

/// UDP のプロトコル番号（REQUESTED-TRANSPORT）。
pub const PROTO_UDP: u8 = 17;
/// 既定の permission の寿命（秒）。
pub const PERMISSION_LIFETIME_SECS: u64 = 300;
/// 既定の channel binding の寿命（秒）。
pub const CHANNEL_LIFETIME_SECS: u64 = 600;
