//! 送り方（transport）の差し替え口。
//!
//! 送信スレッドは [`Connector::connect`] で [`Transport`] を作り、設定と AU を順に渡す。
//! I/O はすべてブロッキング（tokio を使わない）。書き込みが詰まったら送信スレッドがそこで待ち、
//! その間に入ったフレームは送信キューがライブ優先で捨てる。
//!
//! - [`rtmp`]: RTMP（TCP）。RTMPS は [`TlsConnector`] を差し込むと使える（TLS の部品は別 crate）。
//! - [`srt`]: SRT caller（UDP、MPEG-TS）。
//!
//! WHIP（WebRTC）はここには無い（WebRTC の部品ができてから足す）。

pub mod rtmp;
pub mod srt;

use senca_core::{AccessUnit, CodecConfig};
use std::fmt;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// 送り方の失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    pub kind: TransportErrorKind,
    /// 理由（秘密を含めない）。
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportErrorKind {
    /// 回線・相手の問題。間隔を空けてつなぎ直す。
    Network,
    /// 相手が断った（鍵・パスフレーズ・権限）。つなぎ直しても同じなので止める。
    Rejected,
    /// 相手の応答が規格に合わない。つなぎ直す。
    Protocol,
    /// この送り方では運べない（コーデックなど）。止める。
    Unsupported,
}

impl TransportError {
    pub fn new(kind: TransportErrorKind, message: impl Into<String>) -> Self {
        TransportError {
            kind,
            message: message.into(),
        }
    }

    pub fn network(e: impl fmt::Display) -> Self {
        Self::new(TransportErrorKind::Network, e.to_string())
    }

    /// つなぎ直す価値があるか。
    pub fn retryable(&self) -> bool {
        matches!(
            self.kind,
            TransportErrorKind::Network | TransportErrorKind::Protocol
        )
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for TransportError {}

/// つながった送り方。
pub trait Transport: Send {
    /// コーデックの設定（最初の AU の前と、変わったとき）。
    fn set_config(&mut self, config: &CodecConfig) -> Result<(), TransportError>;
    /// AU を送る（書き込みが詰まればここで待つ）。
    fn send(&mut self, au: &AccessUnit) -> Result<(), TransportError>;
    /// 受信の処理とタイマー（待たない）。
    fn poll(&mut self) -> Result<(), TransportError>;
    /// 次に [`poll`](Self::poll) を呼ぶまでの最長の間隔。
    fn poll_interval(&self) -> Duration;
    /// 往復時間（分かる送り方だけ）。
    fn rtt_us(&self) -> Option<u64> {
        None
    }
    /// これまでに送ったバイト数（ヘッダー込み）。
    fn bytes_sent(&self) -> u64;
    /// 終わりを知らせて閉じる（送り残しは `linger` まで送る）。
    fn close(&mut self, linger: Duration);
}

/// 送り方を作るもの（つなぎ直しのたびに呼ぶ）。
pub trait Connector: Send {
    fn connect(&mut self, timeout: Duration) -> Result<Box<dyn Transport>, TransportError>;
}

/// TCP・TLS の上のバイト列。読み込みは時間切れ（`WouldBlock` / `TimedOut`）を返しうる。
pub trait ByteStream: Read + Write + Send {
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()>;
    fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()>;
    /// 書き込み側を閉じる（送り残しを送ってから FIN）。
    fn shutdown(&self) {}
}

impl ByteStream for TcpStream {
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_read_timeout(self, d)
    }
    fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_write_timeout(self, d)
    }
    fn shutdown(&self) {
        let _ = TcpStream::shutdown(self, std::net::Shutdown::Write);
    }
}

/// RTMPS 用の TLS（クライアント）。TLS の実装は SDK の外から差し込む（仕様 2.1・ADR-0002）。
pub trait TlsConnector: Send + Sync {
    /// `tcp` の上で TLS を始める（`server_name` で証明書を検証すること）。
    fn connect(&self, server_name: &str, tcp: TcpStream) -> std::io::Result<Box<dyn ByteStream>>;
}
