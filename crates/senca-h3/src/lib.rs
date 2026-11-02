//! SENCA の HTTP/3（RFC 9114）・QPACK（RFC 9204）・拡張 CONNECT（RFC 9220）・
//! HTTP Datagrams と Capsule（RFC 9297）・WebTransport over HTTP/3。sans-io。
//!
//! [`senca_quic::Connection`] の上で動き、QUIC のイベントを [`H3Connection::process`] で取り込んで
//! [`H3Event`] に変える。I/O・時計は持たない。
//!
//! # 固定した版（仕様 6.4「採用仕様を台帳に固定する」）
//!
//! - HTTP/3: RFC 9114。server push は使わない（MAX_PUSH_ID を送らない）。
//! - QPACK: RFC 9204。**送信側は静的テーブルとリテラルだけ**（動的テーブルに挿入しない）。
//!   **受信側は動的テーブルに対応**し、広告する容量（既定 4096）・blocked streams（既定 16）を上限とする。
//!   Huffman は復号に対応、送信では使わない。
//! - WebTransport over HTTP/3: **draft-ietf-webtrans-http3-02 と -07**（[`WtVersion`]）。
//!   Chrome 156（2026-11 確認）の WebTransport client は既定で **draft-02** だけを広告し
//!   （SETTINGS_ENABLE_WEBTRANSPORT 0x2b603742、`net/quic/dedicated_web_transport_http3_client.cc`）、
//!   draft-07（SETTINGS_WEBTRANSPORT_MAX_SESSIONS 0xc671706a）は機能フラグ
//!   `EnableWebTransportDraft07` の裏にある。そのため両方の SETTINGS を送り、両者が広告した新しい方を
//!   使う（QUICHE と同じ規則）。draft-02 では応答に `sec-webtransport-http3-draft: draft02` を付ける。
//!   以下の形式は両版で共通（この実装の範囲）:
//!   - 双方向 stream: signal 0x41 + session ID、単方向 stream: 種別 0x54 + session ID。
//!   - datagram: HTTP Datagram（Quarter Stream ID + payload）。
//!   - 終了: CLOSE_WEBTRANSPORT_SESSION capsule（0x2843）、DRAIN_WEBTRANSPORT_SESSION（0x78ae）。
//!   - stream のエラーコードは 0x52e4a40fa8db からの写像（[`wt_error_to_h3`]）。

mod capsule;
mod conn;
mod error;
mod frame;
mod headers;
mod huffman;
mod huffman_table;
pub mod qpack;
mod static_table;

pub use capsule::Capsule;
pub use conn::{ApiError, H3Config, H3Connection, H3Event, WtStreamKind, WtVersion};
pub use error::{H3Error, H3ErrorCode, h3_to_wt_error, wt_error_to_h3};
pub use headers::{Field, Headers};
pub use frame::Settings;

/// HTTP/3 の ALPN。
pub const ALPN: &[u8] = b"h3";
