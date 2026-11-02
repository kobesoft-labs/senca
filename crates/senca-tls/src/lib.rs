//! SENCA の TLS 1.3（RFC 8446）。sans-io の client / server と QUIC モード（RFC 9001）。
//!
//! ハンドシェイクの状態機械・記録層・鍵スケジュールはこの crate の実装。暗号の演算
//! （AES-GCM・ChaCha20-Poly1305・SHA-2・HMAC・HKDF・ECDHE・署名）は RustCrypto / dalek、
//! X.509 のパス検証は rustls-webpki に任せる（ADR-0002、仕様 2.1・2.2）。
//!
//! - 版: TLS 1.3 だけ。TLS 1.2 以下の相手とは接続しない（protocol_version）。
//! - 暗号スイート: TLS_AES_128_GCM_SHA256・TLS_AES_256_GCM_SHA384・TLS_CHACHA20_POLY1305_SHA256。
//! - group: x25519・secp256r1（必要なら HelloRetryRequest）。
//! - 署名: ecdsa_secp256r1_sha256・rsa_pss_rsae_sha256/384/512・ed25519
//!   （受信は ecdsa_secp384r1_sha384 も。rsa_pkcs1 は証明書の署名だけ）。
//! - 再開: stateless のセッションチケット（鍵を回す）、psk_dhe_ke だけ。0-RTT は使わない（仕様 6.4）。
//! - 再ネゴシエーション・post-handshake 認証はない。KeyUpdate・record_size_limit・close_notify に対応。
//!
//! TCP では [`Connection`]、QUIC では [`QuicConnection`] を使う。

mod cert;
mod client;
mod codec;
mod config;
mod conn;
mod crypto;
mod engine;
mod error;
mod key_schedule;
mod msgs;
pub mod quic;
mod server;

#[cfg(test)]
mod tests;

pub use cert::{CertifiedKey, ClientAuth, RootStore, ServerVerifier, scheme, spki_sha256};
pub use config::{
    ALPN_H3, ALPN_HTTP11, ALPN_MOQ00, ClientConfig, ClientSessionStore, MemorySessionStore, ServerConfig,
    StoredTicket, TicketKeys,
};
pub use conn::{Connection, HandshakeInfo, TlsSession};
pub use crypto::{CipherSuite, HashAlg, NamedGroup, Secret, hkdf_expand_label};
pub use engine::Level;
pub use error::{AlertDescription, Error};
pub use pki_types::CertificateDer;
pub use quic::{QuicConnection, QuicOutput};
