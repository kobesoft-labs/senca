//! sans-io の WebSocket（RFC 6455）。I/O・時計・スレッドを持たない。
//!
//! - [`handshake`] — HTTP/1.1 の Upgrade の検査（サーバー: `Sec-WebSocket-Key`・版 13・Origin の検査・
//!   subprotocol の選択、クライアント: 要求の生成と `Sec-WebSocket-Accept` の照合）
//! - [`frame`] — frame の見出しの解析と符号化（mask の規則、長さの最小の符号化）
//! - [`conn`] — 接続の状態機械（分割、制御 frame、ping/pong、close の手順、UTF-8 の逐次検査、上限）
//!
//! 拡張（permessage-deflate など）は交渉しない。サーバーは `Sec-WebSocket-Extensions` を無視して返さず、
//! クライアントは応答に拡張があれば拒否する。RSV ビットの立った frame は 1002 で閉じる。
//!
//! ```text
//! // サーバー: senca_http で要求の head を読んだら
//! let acc = senca_ws::handshake::accept(&head, &AcceptOptions::default())?;   // 失敗は err.response_bytes() を返す
//! socket.write_all(&acc.response_bytes());
//! let mut ws = senca_ws::Connection::new(Role::Server, Limits::default());
//! loop {
//!     ws.feed(&buf[..n]);
//!     while let Some(m) = ws.poll()? { match m { Message::Text(s) => .., Message::Close(_) => .., _ => {} } }
//!     while let Some(b) = ws.poll_transmit() { socket.write_all(&b) }
//!     if ws.is_closed() { break }   // TCP を閉じる（サーバーが先に閉じる）
//! }
//! ```

pub mod conn;
pub mod frame;
pub mod handshake;

pub use conn::{CloseFrame, Connection, Limits, Message, Role, State, WsError};
pub use frame::{FrameHeader, OpCode, close_code_is_valid, encode_frame};
pub use handshake::{AcceptOptions, Accepted, ClientHandshake, HandshakeError, OriginCheck, OriginPolicy, accept_key};
