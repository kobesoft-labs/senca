//! SRT（Secure Reliable Transport）live mode の sans-io 実装。
//!
//! Haivision の SRT（draft-sharabayko-srt と libsrt 1.5 系の挙動）に合わせる。対応範囲は
//! live mode の caller / listener、HSv5 ハンドシェイク（HSREQ/HSRSP・KMREQ/KMRSP・Stream ID）、
//! TSBPD、ACK/ACKACK と RTT、NAK（即時・周期）、再送、遅すぎるパケットの破棄（TLPKTDROP）、
//! フロー窓、keepalive、終了、相手の無応答、live 輸送制御のペーシング、AES-CTR / AES-GCM の
//! 暗号と鍵の更新。rendezvous、file mode、bonding、FEC は対象外（仕様 6.8）。
//!
//! # 使い方
//!
//! ```no_run
//! use senca_srt::{AcceptDecision, AcceptParams, Event, Listener, ListenerConfig};
//! # fn recv() -> (u64, std::net::SocketAddr, Vec<u8>) { unimplemented!() }
//! # fn send_udp(_: std::net::SocketAddr, _: &[u8]) {}
//! let mut l = Listener::new(ListenerConfig::default(), |req| {
//!     if req.parsed.is_some_and(|s| s.is_publish()) {
//!         AcceptDecision::Accept(AcceptParams::default())
//!     } else {
//!         AcceptDecision::Reject(senca_srt::RejectReason::X_FORBIDDEN)
//!     }
//! })
//! .unwrap();
//! loop {
//!     let (now, from, buf) = recv(); // poll_timeout() までに届いたもの
//!     l.handle_datagram(now, from, &buf);
//!     l.handle_timeout(now);
//!     while let Some((to, pkt)) = l.poll_transmit() {
//!         send_udp(to, &pkt);
//!     }
//!     while let Some((id, ev)) = l.poll_event() {
//!         if let Event::Data { payload, .. } = ev { let _ = (id, payload); }
//!     }
//! }
//! ```

pub mod config;
pub mod connection;
pub mod crypto;
pub mod handshake;
pub mod listener;
pub mod packet;
pub mod streamid;

#[cfg(test)]
mod sim_tests;

pub use config::{CloseReason, Config, ConfigError, ConnectedInfo, Event, MaxBandwidth, RejectReason};
pub use connection::{Connection, SendError, Stats, SRT_VERSION};
pub use crypto::{CipherMode, CryptoError, KeyLen, Passphrase};
pub use listener::{AcceptDecision, AcceptParams, AcceptRequest, ConnectionId, Listener, ListenerConfig, Rejection};
pub use streamid::{StreamId, StreamIdError};
