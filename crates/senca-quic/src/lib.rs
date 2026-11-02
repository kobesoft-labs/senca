//! SENCA の QUIC v1（RFC 9000・9001・9002）と DATAGRAM 拡張（RFC 9221）。sans-io。
//!
//! - 版: QUIC v1（0x00000001）だけ。未知の版の Initial には Version Negotiation を返し、
//!   client は v1 以外へ推測で接続しない。
//! - TLS 1.3 は `senca-tls` の QUIC モード（[`senca_tls::QuicConnection`]）を使う。0-RTT は無効
//!   （仕様 6.4）。0-RTT パケットは読まずに捨てる。
//! - パケット保護（AEAD・header protection）は RustCrypto（aes-gcm・chacha20poly1305・aes・chacha20）、
//!   Retry のトークンと stateless reset token は HMAC-SHA256（ADR-0002）。
//! - 損失検出・PTO・NewReno・pacing は RFC 9002 に従う。
//! - I/O・時計・スレッドを持たない。時刻（[`Instant`](std::time::Instant)）は引数で受け取る。
//!
//! 使い方: [`Endpoint`] に UDP の datagram を渡し（[`Endpoint::handle_datagram`]）、
//! [`Endpoint::poll_transmit`] が返すものを送り、[`Endpoint::poll_timeout`] の時刻に
//! [`Endpoint::handle_timeout`] を呼ぶ。接続ごとの操作は [`Endpoint::connection_mut`] で得た
//! [`Connection`] に対して行う（stream の読み書き・datagram・イベント）。

mod cid;
mod coding;
mod config;
mod conn;
mod crypto;
mod endpoint;
mod error;
mod frame;
mod packet;
mod ranges;
mod recovery;
mod streams;
mod transport_params;
pub mod varint;

pub use cid::{ConnectionId, MAX_CID_LEN};
pub use config::{ClientConfig, EndpointConfig, ServerConfig, TransportConfig};
pub use conn::{Connection, ConnectionStats, DatagramError, Event, RecvChunk, Side, Transmit};
pub use endpoint::{ConnectError, ConnectionHandle, Endpoint, EndpointEvent};
pub use error::{ConnectionError, StreamError, TransportErrorCode};
pub use streams::{Dir, StreamId};
pub use transport_params::TransportParameters;

/// QUIC v1 の版番号。
pub const VERSION_1: u32 = 0x0000_0001;

/// 送信する UDP payload の既定の上限（経路 MTU 探索はしない、RFC 9000 14）。
pub const DEFAULT_MAX_DATAGRAM: usize = 1200;
