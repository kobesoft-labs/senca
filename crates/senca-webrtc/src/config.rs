//! [`PeerConnection`](crate::PeerConnection) の設定。

use std::net::SocketAddr;
use std::time::Duration;

use senca_dtls::DtlsCertificate;
use senca_ice::TurnServerSpec;
use senca_sdp::Capabilities;

/// 受信側の NACK の設定（仕様 6.2: 再生期限と往復時間から判断する）。
#[derive(Debug, Clone)]
pub struct NackConfig {
    /// 1 つの番号を求め直す回数の上限。
    pub max_retries: u32,
    /// 同じ番号を求め直す最短の間隔（往復時間が分かればその 1.5 倍とこの値の大きい方）。
    pub min_retry_interval: Duration,
    /// 覚えておく抜けの数の上限（超えたら古い抜けを諦め、キーフレームを求める）。
    pub max_missing: usize,
    /// 1 秒あたりに NACK で求める番号の数の上限（ストリームごと）。
    pub max_per_second: u32,
}

impl Default for NackConfig {
    fn default() -> Self {
        NackConfig {
            max_retries: 10,
            min_retry_interval: Duration::from_millis(20),
            max_missing: 512,
            max_per_second: 1000,
        }
    }
}

/// 送信側の帯域推定の設定。
#[derive(Debug, Clone)]
pub struct BweConfig {
    pub start_bps: u64,
    pub min_bps: u64,
    pub max_bps: u64,
}

impl Default for BweConfig {
    fn default() -> Self {
        BweConfig {
            start_bps: 1_000_000,
            min_bps: 50_000,
            max_bps: 20_000_000,
        }
    }
}

/// pacer（leaky bucket）の設定。
#[derive(Debug, Clone)]
pub struct PacerConfig {
    /// 平準化する。偽ならすぐ送る。
    pub enabled: bool,
    /// 推定帯域に掛ける倍率（送出の速さ = 推定 × 倍率）。
    pub multiplier: f64,
    /// 固定の速さ（bit/s）。指定すると推定を使わない。
    pub fixed_rate_bps: Option<u64>,
    /// 速さの下限（bit/s）。
    pub min_rate_bps: u64,
    /// 一度に続けて出してよい量（時間で表す）。
    pub burst: Duration,
    /// 待ち行列の上限（packet 数）。超えたら古い映像の packet を捨て、キーフレームを求める。
    pub max_queue_packets: usize,
    /// 待ち行列での待ち時間の上限。超えたら上と同じ扱い。
    pub max_queue_delay: Duration,
}

impl Default for PacerConfig {
    fn default() -> Self {
        PacerConfig {
            enabled: true,
            multiplier: 2.5,
            fixed_rate_bps: None,
            min_rate_bps: 300_000,
            burst: Duration::from_millis(5),
            max_queue_packets: 4096,
            max_queue_delay: Duration::from_secs(2),
        }
    }
}

/// 受信の頻度の上限（接続ごと、token bucket、1 秒分の burst）。
#[derive(Debug, Clone)]
pub struct RateLimit {
    pub packets_per_second: u32,
    pub bytes_per_second: u64,
}

impl Default for RateLimit {
    fn default() -> Self {
        RateLimit {
            packets_per_second: 20_000,
            bytes_per_second: 50_000_000,
        }
    }
}

/// 接続の設定。
#[derive(Clone)]
pub struct PeerConfig {
    /// DTLS の証明書（`None` なら自己署名を作る）。同じ node で使い回してよい。
    pub certificate: Option<DtlsCertificate>,
    /// ICE-lite（既定。公開 IP のサーバー向け）。偽なら full ICE。
    pub ice_lite: bool,
    /// full ICE の STUN・TURN サーバー。
    pub stun_servers: Vec<SocketAddr>,
    pub turn_servers: Vec<TurnServerSpec>,
    /// 扱うコーデックとヘッダー拡張。
    pub capabilities: Capabilities,
    /// DataChannel（SCTP）を受け付ける・offer に載せる。
    pub datachannel: bool,
    /// SCTP の設定（`max_send_message_size` は相手の SDP の値で上書きする）。
    pub sctp: senca_sctp::SctpConfig,
    /// ジッターバッファで抜けを待つ最長の時間（再生期限）。
    pub jitter_latency: Duration,
    /// ジッターバッファの packet 数の上限（ストリームごと）。
    pub jitter_capacity: usize,
    pub nack: NackConfig,
    /// キーフレーム要求（PLI/FIR）の最短の間隔（送信元ごと）。
    pub keyframe_min_interval: Duration,
    pub bwe: BweConfig,
    pub pacer: PacerConfig,
    pub rate_limit: RateLimit,
    /// 受信ストリーム（SSRC）の数の上限。
    pub max_inbound_streams: usize,
    /// 1 つの m 行で受ける simulcast の層の数の上限。
    pub max_simulcast_layers: usize,
    /// 出来事の待ち行列の上限（超えた AU は捨てて数える）。
    pub max_events: usize,
    /// 送る datagram の待ち行列の上限。
    pub max_transmits: usize,
    /// 再送の履歴（映像の送信トラックごとの packet 数）。
    pub retransmit_history: usize,
    /// RTP payload の最大長。
    pub mtu: usize,
    /// RTCP（SR・RR・SDES）を送る間隔。
    pub rtcp_interval: Duration,
    /// transport-wide-cc の feedback を送る間隔。
    pub twcc_interval: Duration,
    /// `new` に渡した `now` の時点の UNIX 時刻（ns）。SR の NTP 時刻に使う（`None` なら 0 起点。lip sync は保たれる）。
    pub unix_time_ns_at_start: Option<i64>,
    /// 送信に playout-delay 拡張を付ける（最小, 最大。10 ms 単位に丸める）。
    pub playout_delay: Option<(Duration, Duration)>,
    /// CNAME（`None` なら乱数）。
    pub cname: Option<String>,
}

impl std::fmt::Debug for PeerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerConfig")
            .field("ice_lite", &self.ice_lite)
            .field("datachannel", &self.datachannel)
            .field("jitter_latency", &self.jitter_latency)
            .finish_non_exhaustive()
    }
}

impl Default for PeerConfig {
    fn default() -> Self {
        PeerConfig {
            certificate: None,
            ice_lite: true,
            stun_servers: Vec::new(),
            turn_servers: Vec::new(),
            capabilities: Capabilities::default_server(),
            datachannel: true,
            sctp: senca_sctp::SctpConfig::default(),
            jitter_latency: Duration::from_millis(200),
            jitter_capacity: 2048,
            nack: NackConfig::default(),
            keyframe_min_interval: Duration::from_millis(500),
            bwe: BweConfig::default(),
            pacer: PacerConfig::default(),
            rate_limit: RateLimit::default(),
            max_inbound_streams: 32,
            max_simulcast_layers: 3,
            max_events: 8192,
            max_transmits: 16384,
            retransmit_history: 2048,
            mtu: senca_rtp::payload::DEFAULT_MTU,
            rtcp_interval: Duration::from_millis(1000),
            twcc_interval: Duration::from_millis(50),
            unix_time_ns_at_start: None,
            playout_delay: None,
            cname: None,
        }
    }
}
