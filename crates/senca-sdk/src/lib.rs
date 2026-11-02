//! SENCA の送り手 SDK（仕様 11 章、REQ-009）。
//!
//! 端末で符号化済みの音声・映像（H.264 / H.265 / AAC / Opus）を受け取り、RTMP（publish）または
//! SRT（caller、MPEG-TS）でサーバーへ送る。tokio を使わず、内部の送信スレッド 1 本でブロッキング I/O を行う。
//!
//! - [`sender`]: 送り手 [`Sender`]（作成・開始・フレームの投入・統計・停止）
//! - [`uri`]: 送り先の設定文字列（`senca://ingest?...` と標準の `srt://`・`rtmp://`・`https://`）
//! - [`qr`]: QR コードの生成（バイト・数字モード、誤り訂正 M、型番 1〜10）と SVG / PNG
//! - [`visca`]: VISCA over IP のコマンドの組み立てと解釈（PTZ カメラ・操作盤）
//! - [`transport`]: 送り方の差し替え口（RTMP・SRT、RTMPS は TLS の部品を差し込む）
//! - [`ffi`]: C ABI（`include/senca.h`）。安定境界はこちら。

pub mod ffi;
pub mod frame;
pub mod png;
pub mod qr;
pub mod queue;
pub mod sender;
pub mod transport;
pub mod uri;
pub mod visca;

pub use frame::{AudioCodec, AudioFormat, VideoCodec, VideoFormat};
pub use sender::{Event, PushOutcome, Sender, SenderConfig, SenderState, Stats};
pub use uri::{Endpoint, IngestUri, Secret, UriError};

use std::fmt;

/// SDK のエラー。秘密（stream key・passphrase・token）は含めない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// 引数が正しくない。
    InvalidArgument(&'static str),
    /// 設定文字列が正しくない。
    InvalidConfig(UriError),
    /// この版ではできない（WHIP の native 送信、TLS の部品なしの RTMPS など）。
    Unsupported(&'static str),
    /// 今の状態ではできない（開始前・停止後など）。
    InvalidState(&'static str),
    /// フレームが壊れている・設定と合わない。
    BadFrame(&'static str),
    /// 内部の失敗（スレッドが作れないなど）。
    Internal(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidArgument(m) => write!(f, "invalid argument: {m}"),
            Error::InvalidConfig(e) => write!(f, "invalid config: {e}"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
            Error::InvalidState(m) => write!(f, "invalid state: {m}"),
            Error::BadFrame(m) => write!(f, "bad frame: {m}"),
            Error::Internal(m) => write!(f, "internal: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<UriError> for Error {
    fn from(e: UriError) -> Self {
        Error::InvalidConfig(e)
    }
}
