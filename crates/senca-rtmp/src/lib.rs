//! RTMP（sans-io）。I/O・スレッド・時計を持たない。
//!
//! - [`handshake`]: 単純ハンドシェイク（version 3）
//! - [`chunk`]: chunk stream の復号・符号化（fmt 0〜3、extended timestamp、上限つき）
//! - [`message`]: protocol control・user control・AMF0 command
//! - [`server`]: [`ServerSession`]（publish・play を受ける）
//! - [`client`]: [`ClientSession`]（外部へ publish、外部から play）
//!
//! ## 使い方（共通）
//!
//! ```text
//! loop {
//!     let n = socket.read(&mut buf)?;          // I/O は呼び出し側
//!     session.feed(&buf[..n])?;                // Err なら接続を閉じる
//!     while let Some(ev) = session.poll_event() {
//!         // ServerEvent::Connect / Publish / Play には accept_* / reject_* で応える
//!     }
//!     socket.write_all(&session.take_output())?;
//! }
//! ```
//!
//! accept・send などの呼び出しも出力を積むので、そのあとにも `take_output` を確認する。
//! 音声・映像の payload は FLV タグ本体（`senca-flv` で解釈・生成する）で、`Bytes` のまま渡す。
//! stream key・tcUrl・トークンは [`Secret`] に包み、Debug・エラーに出さない。

pub mod chunk;
pub mod client;
mod conn;
pub mod handshake;
pub mod message;
pub mod server;

pub use client::{ClientConfig, ClientEvent, ClientMode, ClientSession};
pub use message::{MediaMessage, MediaType};
pub use server::{CloseReason, ConnectInfo, ServerConfig, ServerEvent, ServerSession};

use std::fmt;

/// 秘密の文字列（stream key・tcUrl・トークン）。Debug では中身を出さない。Display は実装しない。
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }

    /// 中身。認可の判定など、本当に必要なところでだけ使う。
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// エラー。stream key などの秘密は含めない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Handshake(&'static str),
    Protocol(&'static str),
    /// 上限（message 長・chunk stream 数・保留中の量など）を超えた。
    LimitExceeded(&'static str),
    Amf(senca_amf0::Error),
    /// 今の状態では呼べない操作。
    InvalidState(&'static str),
    /// すでにエラーまたは終了したセッション。
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Handshake(m) => write!(f, "rtmp handshake: {m}"),
            Error::Protocol(m) => write!(f, "rtmp protocol: {m}"),
            Error::LimitExceeded(m) => write!(f, "rtmp limit exceeded: {m}"),
            Error::Amf(e) => write!(f, "rtmp: {e}"),
            Error::InvalidState(m) => write!(f, "rtmp invalid state: {m}"),
            Error::Closed => f.write_str("rtmp session closed"),
        }
    }
}

impl std::error::Error for Error {}

impl From<senca_amf0::Error> for Error {
    fn from(e: senca_amf0::Error) -> Self {
        Error::Amf(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_is_redacted() {
        let s = Secret::new("live_abc123");
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        let ev = ServerEvent::Publish {
            stream_id: 1,
            stream_key: s,
            publish_type: "live".into(),
        };
        assert!(!format!("{ev:?}").contains("abc123"));
    }
}
