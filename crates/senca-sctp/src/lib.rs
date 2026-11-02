//! SCTP over DTLS（RFC 8261）と WebRTC の DataChannel（RFC 8831・RFC 8832）。sans-io。
//!
//! 2 つの層がある。
//!
//! - [`Association`]: SCTP の association（RFC 9260 の一部、RFC 3758 PR-SCTP、RFC 6525 stream reset）。
//!   1 つの SCTP パケット = 1 つの DTLS の application data。
//! - [`DataChannels`]: その上の DataChannel（DCEP の OPEN/ACK、PPID、チャネルの開閉）。
//!
//! 駆動のしかた（DataChannels の例）:
//!
//! ```text
//! let mut dc = DataChannels::new(SctpConfig::default(), is_dtls_client, now)?;
//! dc.connect(now);                           // 片側（または両側）が INIT を送る
//! loop {
//!     dc.handle_packet(now, &dtls_app_data)?;  // DTLS から受けた SCTP パケット
//!     while let Some(p) = dc.poll_transmit() { dtls.send(&p) }
//!     while let Some(e) = dc.poll_event() { /* Open・Message・Closed */ }
//!     dc.handle_timeout(now)                   // dc.poll_timeout() の時刻に
//! }
//! ```
//!
//! 送るパケットは `poll_transmit` で取り出すときに組み立てる（時刻は直前に渡した `now`）。
//! `handle_packet` の `Err` はそのパケットを捨てたという意味で、association は続く
//! （閉じたかどうかは `Closed`・`Disconnected` の出来事で知らせる）。
//!
//! モジュール:
//!
//! - [`chunk`] — パケット・チャンクの読み書き（checksum を含む）。
//! - [`crc32c`] — CRC32c。
//! - [`dcep`] — DCEP のメッセージ。
//! - `association`・`reassembly`・`cookie`・`datachannel`: 状態機械（中身は非公開）。

use bytes::Bytes;
use std::time::Duration;

mod association;
pub mod chunk;
mod cookie;
pub mod crc32c;
mod datachannel;
pub mod dcep;
mod reassembly;
#[cfg(test)]
mod tests;

pub use association::{Association, AssociationStats};
pub use datachannel::DataChannels;

/// 設定。
#[derive(Debug, Clone)]
pub struct SctpConfig {
    /// 自分の SCTP ポート（SDP の a=sctp-port、既定 5000）。
    pub local_port: u16,
    /// 相手の SCTP ポート（既定 5000）。
    pub remote_port: u16,
    /// 受け取るメッセージの上限（自分の SDP の a=max-message-size、既定 256 KiB）。
    pub max_receive_message_size: usize,
    /// 送るメッセージの上限（相手の SDP の a=max-message-size、既定 256 KiB。0 は「上限なし」だが
    /// その場合も `max_send_buffer` を超えない）。
    pub max_send_message_size: usize,
    /// SCTP パケットの上限（DTLS の record に収まる大きさ、既定 1200）。
    pub mtu: usize,
    /// 受信窓（a_rwnd、既定 1 MiB）。組み立て中・未配送のデータの総量もこれで抑える。
    pub receive_window: u32,
    /// 送信待ち（未送信 + 未確認）の総量の上限（既定 4 MiB）。超える send は `Err(BufferFull)`。
    pub max_send_buffer: usize,
    /// ストリームの数（OS・MIS、既定 65535）。
    pub num_streams: u16,
    /// RTO の初期値・下限・上限（RFC 9260 15、WebRTC 向けに既定 1 s・200 ms・10 s）。
    pub rto_initial: Duration,
    pub rto_min: Duration,
    pub rto_max: Duration,
    /// INIT・COOKIE-ECHO の再送回数の上限（既定 8）。
    pub max_init_retransmits: u32,
    /// association の再送回数の上限（Association.Max.Retrans、既定 10）。超えたら閉じる。
    pub max_retransmits: u32,
    /// HEARTBEAT の間隔（`None` なら送らない。応答はいつも返す。既定 30 s）。
    pub heartbeat_interval: Option<Duration>,
    /// State Cookie の有効期間（既定 60 s）。
    pub cookie_lifetime: Duration,
}

impl Default for SctpConfig {
    fn default() -> Self {
        SctpConfig {
            local_port: 5000,
            remote_port: 5000,
            max_receive_message_size: 256 * 1024,
            max_send_message_size: 256 * 1024,
            mtu: 1200,
            receive_window: 1024 * 1024,
            max_send_buffer: 4 * 1024 * 1024,
            num_streams: 65535,
            rto_initial: Duration::from_secs(1),
            rto_min: Duration::from_millis(200),
            rto_max: Duration::from_secs(10),
            max_init_retransmits: 8,
            max_retransmits: 10,
            heartbeat_interval: Some(Duration::from_secs(30)),
            cookie_lifetime: Duration::from_secs(60),
        }
    }
}

/// 誤り。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SctpError {
    InvalidConfig(&'static str),
    /// OS の乱数源が使えない。
    Random,
    /// 壊れたパケット（checksum・長さ・形式）。association の状態は変わらない。
    Malformed(&'static str),
    /// まだつながっていない。
    NotConnected,
    /// 閉じている。
    Closed,
    /// メッセージが大きすぎる（max-message-size）。
    TooLarge,
    /// 送信待ちが上限を超える。
    BufferFull,
    /// ストリーム番号が範囲外・リセット中。
    InvalidStream,
    /// DataChannel が無い・開いていない。
    UnknownChannel,
    /// DataChannel の番号を使い切った・重複。
    ChannelIdUnavailable,
}

impl core::fmt::Display for SctpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for SctpError {}

/// association の状態（RFC 9260 4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssociationState {
    Closed,
    CookieWait,
    CookieEchoed,
    Established,
    ShutdownPending,
    ShutdownSent,
    ShutdownReceived,
    ShutdownAckSent,
}

/// 送り方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SendOptions {
    /// 順序を保たない（U ビット）。
    pub unordered: bool,
    /// 部分信頼（RFC 3758）: 再送回数の上限（0 なら再送しない）。
    pub max_retransmits: Option<u32>,
    /// 部分信頼（RFC 3758）: 送ってよい期間。
    pub max_lifetime: Option<Duration>,
}

/// association の出来事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SctpEvent {
    /// association ができた。
    Connected,
    /// メッセージを受け取った（組み立て済み）。
    Message {
        stream: u16,
        ppid: u32,
        data: Bytes,
        unordered: bool,
    },
    /// 相手が自分の送信ストリーム（＝こちらの受信ストリーム）をリセットした（RFC 6525 の Outgoing SSN Reset Request）。
    /// `streams` が空なら全ストリーム。
    IncomingStreamsReset { streams: Vec<u16> },
    /// こちらが求めた送信ストリームのリセットが終わった。
    OutgoingStreamsReset { streams: Vec<u16> },
    /// 送信待ちが閾値（[`Association::set_buffered_amount_low_threshold`]）以下になった。
    BufferedAmountLow { stream: u16 },
    /// 閉じた（SHUTDOWN 完了・ABORT・再送の上限・cookie の失敗など）。
    Closed { reason: String },
}

/// DCEP（RFC 8832）の PPID と、DataChannel の PPID（RFC 8831 8）。
pub mod ppid {
    pub const DCEP: u32 = 50;
    pub const STRING: u32 = 51;
    pub const BINARY: u32 = 53;
    pub const STRING_EMPTY: u32 = 56;
    pub const BINARY_EMPTY: u32 = 57;
}

/// DataChannel の信頼性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reliability {
    #[default]
    Reliable,
    /// 再送回数の上限（DATA_CHANNEL_PARTIAL_RELIABLE_REXMIT）。
    MaxRetransmits(u32),
    /// 送ってよい期間（ミリ秒、DATA_CHANNEL_PARTIAL_RELIABLE_TIMED）。
    MaxLifetimeMs(u32),
}

/// DataChannel を開く設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelOptions {
    pub label: String,
    pub protocol: String,
    pub ordered: bool,
    pub reliability: Reliability,
    pub priority: u16,
    /// 事前に合意した番号（`negotiated: true`、DCEP を使わない）。
    pub negotiated_id: Option<u16>,
}

impl Default for ChannelOptions {
    fn default() -> Self {
        ChannelOptions {
            label: String::new(),
            protocol: String::new(),
            ordered: true,
            reliability: Reliability::Reliable,
            priority: 256,
            negotiated_id: None,
        }
    }
}

/// DataChannel の情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfo {
    pub id: u16,
    pub label: String,
    pub protocol: String,
    pub ordered: bool,
    pub reliability: Reliability,
    pub negotiated: bool,
    /// 開いた（DCEP の ACK を受けた・相手の OPEN に ACK を返した・negotiated）。
    pub open: bool,
}

/// DataChannel の出来事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataChannelEvent {
    /// SCTP の association ができた。
    Connected,
    /// チャネルが開いた（自分が開いたものは ACK を受けたとき、相手が開いたものは OPEN を受けたとき）。
    Open(ChannelInfo),
    /// メッセージ（`binary` が偽なら UTF-8 の文字列。空のメッセージも届く）。
    Message {
        id: u16,
        data: Bytes,
        binary: bool,
    },
    /// チャネルが閉じた（stream reset が両方向で終わった。association が閉じたときは
    /// `Disconnected` の前に残っていたチャネルごとに出す）。
    Closed {
        id: u16,
    },
    BufferedAmountLow {
        id: u16,
    },
    /// association が閉じた。
    Disconnected {
        reason: String,
    },
}
