//! 設定・出来事・拒否理由。

use std::net::SocketAddr;

use bytes::Bytes;

use crate::crypto::{CipherMode, KeyLen, Passphrase};

/// 送信帯域の上限（live 輸送制御のペーシング）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaxBandwidth {
    /// 上限なし（libsrt の既定と同じく 1 Gbit/s で抑える）。
    Unlimited,
    /// 固定値（バイト/秒）。
    Fixed(u64),
    /// 入力レート（指定値か推定値）に余裕（%）を足した値。
    Relative {
        /// 入力レート（バイト/秒）。None なら送信要求から推定する。
        input_bps: Option<u64>,
        /// 余裕（%）。
        overhead_pct: u32,
    },
}

/// 接続の設定（開始側・応答側で共通）。
#[derive(Clone, Debug)]
pub struct Config {
    /// 受信遅延（TSBPD、ms）。相手の申告と大きい方を使う。
    pub latency_ms: u16,
    /// 相手の受信に求める遅延（ms）。
    pub peer_latency_ms: u16,
    /// 暗号のパスフレーズ。None なら平文。片側だけ暗号のときは必ず接続を拒否する。
    pub passphrase: Option<Passphrase>,
    /// 開始側の鍵長。
    pub key_len: KeyLen,
    /// 開始側の暗号方式。
    pub cipher: CipherMode,
    /// 開始側が送る Stream ID。
    pub stream_id: Option<String>,
    /// 1 回の `send` に入れてよい最大のバイト数（live の既定 1316 = 7×188）。
    pub payload_size: usize,
    /// 受信バッファ（パケット数）。フロー窓として相手へ申告する。
    pub rcv_buf_pkts: usize,
    /// 送信バッファ（パケット数）。
    pub snd_buf_pkts: usize,
    /// 送信帯域の上限。
    pub max_bw: MaxBandwidth,
    /// 相手から何も届かないときに切る時間（ms）。
    pub peer_idle_timeout_ms: u64,
    /// 開始側のハンドシェイクの時間切れ（ms）。
    pub connect_timeout_ms: u64,
    /// 送信側で遅すぎるパケットを捨てる猶予（ms、相手の遅延に足す）。
    pub snd_drop_delay_ms: u64,
    /// 遅すぎるパケットを捨てる（TLPKTDROP）。
    pub tlpktdrop: bool,
    /// 周期 NAK を送る。
    pub nak_report: bool,
    /// 鍵を切り替えるまでのパケット数（0 で切り替えない）。
    pub km_refresh_rate: u64,
    /// 切り替えの前後に新旧の鍵を告知するパケット数。
    pub km_preannounce: u64,
    /// `close` で未確認のデータを送り切るまで待つ上限（ms）。
    pub linger_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            latency_ms: 120,
            peer_latency_ms: 0,
            passphrase: None,
            key_len: KeyLen::Aes128,
            cipher: CipherMode::AesCtr,
            stream_id: None,
            payload_size: 1316,
            rcv_buf_pkts: 8192,
            snd_buf_pkts: 8192,
            max_bw: MaxBandwidth::Unlimited,
            peer_idle_timeout_ms: 5000,
            connect_timeout_ms: 3000,
            snd_drop_delay_ms: 0,
            tlpktdrop: true,
            nak_report: true,
            km_refresh_rate: 1 << 24,
            km_preannounce: 1 << 12,
            linger_ms: 1000,
        }
    }
}

/// 設定の誤り。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    PayloadSize,
    BufferSize,
    StreamIdTooLong,
    Rng,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ConfigError::PayloadSize => "payload_size out of range",
            ConfigError::BufferSize => "buffer size out of range",
            ConfigError::StreamIdTooLong => "stream id longer than 512 bytes",
            ConfigError::Rng => "random source failed",
        })
    }
}

impl std::error::Error for ConfigError {}

/// SRT の拒否理由。0〜17 は内部理由（SRT_REJ_*）、1000〜 は標準の応答コード（SRT_REJX_*）、
/// 2000〜 は利用者定義。ハンドシェイクでは 1000 を足した値で送られる。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RejectReason(pub u32);

impl RejectReason {
    pub const UNKNOWN: Self = Self(0);
    pub const SYSTEM: Self = Self(1);
    pub const PEER: Self = Self(2);
    pub const RESOURCE: Self = Self(3);
    pub const ROGUE: Self = Self(4);
    pub const BACKLOG: Self = Self(5);
    pub const IPE: Self = Self(6);
    pub const CLOSE: Self = Self(7);
    pub const VERSION: Self = Self(8);
    pub const RDVCOOKIE: Self = Self(9);
    pub const BADSECRET: Self = Self(10);
    pub const UNSECURE: Self = Self(11);
    pub const MESSAGEAPI: Self = Self(12);
    pub const CONGESTION: Self = Self(13);
    pub const FILTER: Self = Self(14);
    pub const GROUP: Self = Self(15);
    pub const TIMEOUT: Self = Self(16);
    pub const CRYPTO: Self = Self(17);
    pub const X_BAD_REQUEST: Self = Self(1400);
    pub const X_UNAUTHORIZED: Self = Self(1401);
    pub const X_OVERLOAD: Self = Self(1402);
    pub const X_FORBIDDEN: Self = Self(1403);
    pub const X_NOTFOUND: Self = Self(1404);
    pub const X_BAD_MODE: Self = Self(1405);
    pub const X_UNACCEPTABLE: Self = Self(1406);
    pub const X_CONFLICT: Self = Self(1409);
    pub const X_ISE: Self = Self(1500);
    pub const X_UNIMPLEMENTED: Self = Self(1501);

    /// ハンドシェイクの要求種別の値（1000 + 理由）。
    pub fn to_req_type(self) -> u32 {
        1000u32.saturating_add(self.0)
    }

    pub fn from_req_type(t: u32) -> Option<Self> {
        if (1000..=1000 + 0xFFFF).contains(&t) { Some(Self(t - 1000)) } else { None }
    }
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.0 {
            0 => "unknown",
            1 => "system",
            2 => "peer",
            3 => "resource",
            4 => "rogue",
            5 => "backlog",
            6 => "internal error",
            7 => "close",
            8 => "version",
            9 => "cookie",
            10 => "bad secret",
            11 => "unsecure",
            12 => "message api",
            13 => "congestion",
            14 => "filter",
            15 => "group",
            16 => "timeout",
            17 => "crypto",
            1401 => "unauthorized",
            1403 => "forbidden",
            1404 => "not found",
            1409 => "conflict",
            _ => "",
        };
        write!(f, "{} {}", self.0, name)
    }
}

/// 接続が閉じた理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// こちらから閉じた。
    Local,
    /// 相手が SHUTDOWN を送った。
    PeerShutdown,
    /// 相手から一定時間何も届かない。
    PeerIdle,
    /// ハンドシェイクが時間内に終わらない。
    ConnectTimeout,
    /// ハンドシェイクで拒否された（相手から、またはこちらの検査で）。
    Rejected(RejectReason),
}

/// 確立した接続の情報。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectedInfo {
    pub peer: SocketAddr,
    pub stream_id: Option<String>,
    /// 合意した受信遅延（ms）。
    pub latency_ms: u16,
    /// 合意した相手の受信遅延（ms）。
    pub peer_latency_ms: u16,
    /// 暗号（方式・鍵長）。None は平文。
    pub encryption: Option<(CipherMode, KeyLen)>,
    /// 相手の SRT 版（0x00MMmmpp）。
    pub peer_version: u32,
}

/// 接続から取り出す出来事。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Connected(ConnectedInfo),
    /// 受信したメッセージ。TSBPD の時刻になった順に、順序どおりに出る。
    Data {
        payload: Bytes,
        /// 送信側の時刻（接続開始からの µs、32 ビットの巻き戻りを解いた値）。
        src_time_us: u64,
        /// 渡した時刻（こちらの時計、µs）。
        deliver_at: u64,
    },
    Closed { reason: CloseReason },
}
