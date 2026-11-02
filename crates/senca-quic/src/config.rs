//! 設定と資源の上限（仕様 6.4・9.4）。

use std::sync::Arc;
use std::time::Duration;

/// 接続ごとの transport の設定と上限。
#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// 自分が広告する idle timeout（0 は使わない。公開環境では 0 にしない）。
    pub max_idle_timeout: Duration,
    /// 接続全体の受信窓（initial_max_data、読んだ分だけ前へ進める）。
    pub receive_window: u64,
    /// stream ごとの受信窓。
    pub stream_receive_window: u64,
    /// 相手が開ける双方向 stream の同時数。
    pub max_concurrent_bidi_streams: u64,
    /// 相手が開ける単方向 stream の同時数。
    pub max_concurrent_uni_streams: u64,
    /// stream ごとに未送信・未 ACK で抱えてよい送信データの量（`stream_send` が受け付ける上限）。
    pub stream_send_buffer: usize,
    /// 自分の max_ack_delay。
    pub max_ack_delay: Duration,
    /// 受け付ける相手の connection ID の数（active_connection_id_limit）。
    pub active_connection_id_limit: u64,
    /// 受け付ける DATAGRAM フレームの最大長（0 で DATAGRAM 拡張を広告しない）。
    pub max_datagram_frame_size: u64,
    /// 送信待ちの datagram の数（超えたら古いものから捨てる）。
    pub datagram_send_queue: usize,
    /// アプリケーションが読んでいない受信 datagram の数（超えたら古いものから捨てる）。
    pub datagram_recv_queue: usize,
    /// 送る UDP payload の上限（経路 MTU 探索をしないので既定 1200）。
    pub max_udp_payload_size: usize,
    /// RTT の初期値（RFC 9002 6.2.2）。
    pub initial_rtt: Duration,
    /// 相手のアドレス変更を受け入れない（disable_active_migration を広告する）。
    pub disable_active_migration: bool,
    /// この数のパケットを一つの 1-RTT 鍵で送ったら鍵を更新する（AEAD の機密性の上限より十分小さく）。
    pub key_update_interval: u64,
    /// 暗号化段階ごとに、並べ直し待ちで抱える CRYPTO データの上限。
    pub crypto_buffer: usize,
    /// stream ごとの並べ直し待ちの断片の数（細切れの攻撃を防ぐ）。
    pub max_stream_fragments: usize,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            max_idle_timeout: Duration::from_secs(30),
            receive_window: 16 << 20,
            stream_receive_window: 1 << 20,
            max_concurrent_bidi_streams: 100,
            max_concurrent_uni_streams: 100,
            stream_send_buffer: 1 << 20,
            max_ack_delay: Duration::from_millis(25),
            active_connection_id_limit: 4,
            max_datagram_frame_size: 65535,
            datagram_send_queue: 256,
            datagram_recv_queue: 256,
            max_udp_payload_size: crate::DEFAULT_MAX_DATAGRAM,
            initial_rtt: Duration::from_millis(333),
            disable_active_migration: false,
            key_update_interval: 1 << 22,
            crypto_buffer: 64 << 10,
            max_stream_fragments: 2048,
        }
    }
}

/// client の設定。
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub tls: Arc<senca_tls::ClientConfig>,
    pub transport: Arc<TransportConfig>,
}

impl ClientConfig {
    pub fn new(tls: Arc<senca_tls::ClientConfig>) -> Self {
        Self {
            tls,
            transport: Arc::new(TransportConfig::default()),
        }
    }
}

/// server の設定。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub tls: Arc<senca_tls::ServerConfig>,
    pub transport: Arc<TransportConfig>,
    /// 新しい接続に Retry でアドレスの検証を求める（RFC 9000 8.1.2）。
    pub retry: bool,
    /// Retry トークンの有効期間。
    pub retry_token_lifetime: Duration,
    /// 同時に持つ接続の上限（超えた Initial には CONNECTION_REFUSED で応える）。
    pub max_connections: usize,
}

impl ServerConfig {
    pub fn new(tls: Arc<senca_tls::ServerConfig>) -> Self {
        Self {
            tls,
            transport: Arc::new(TransportConfig::default()),
            retry: false,
            retry_token_lifetime: Duration::from_secs(10),
            max_connections: 10_000,
        }
    }
}

/// endpoint の設定。
#[derive(Clone)]
pub struct EndpointConfig {
    /// 自分が発行する connection ID の長さ（4〜20）。
    pub cid_len: usize,
    /// stateless reset token と Retry トークンの鍵（再起動をまたいで同じにすると、
    /// 再起動前の接続へも stateless reset を送れる）。
    pub secret_key: [u8; 32],
    /// 未知の connection ID の short header に stateless reset を返す。
    pub stateless_reset: bool,
}

impl std::fmt::Debug for EndpointConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointConfig")
            .field("cid_len", &self.cid_len)
            .field("stateless_reset", &self.stateless_reset)
            .finish_non_exhaustive()
    }
}

impl EndpointConfig {
    /// 乱数の鍵で作る。
    pub fn new() -> Result<Self, getrandom::Error> {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k)?;
        Ok(Self {
            cid_len: 8,
            secret_key: k,
            stateless_reset: true,
        })
    }
}
