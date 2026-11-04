//! 設定。コマンドライン（`--name value`）と、`--config FILE`（JSON）。同じ項目はコマンドラインが優先。
//! 無効な設定は起動しない（仕様 12 章: 部分反映しない）。

use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub data_dir: PathBuf,
    pub node_name: String,
    pub rtmp_listen: Option<SocketAddr>,
    pub rtmps_listen: Option<SocketAddr>,
    pub http_listen: SocketAddr,
    pub https_listen: Option<SocketAddr>,
    pub srt_listen: Option<SocketAddr>,
    pub rtsp_listen: Option<SocketAddr>,
    pub webrtc_udp: Option<SocketAddr>,
    /// QUIC v1 の UDP の待ち受け（HTTP/3・WebTransport・MoQ draft-22）。既定は無効。TLS の証明書が要る。
    pub quic_listen: Option<SocketAddr>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub public: PublicUrls,
    pub max_playback_ttl_secs: u32,
    pub reconnect_grace_ms: u64,
    pub record_queue: usize,
    pub retention_sweep_secs: u64,
    pub jwks: Option<PathBuf>,
    pub jwt_issuer: Option<String>,
    pub jwt_audience: Option<String>,
    pub jwt_tenant_claim: String,
    pub codec_workers: usize,
    pub codec_worker_bin: Option<PathBuf>,
    pub codec_lib_dir: Option<PathBuf>,
    pub zoneinfo_dir: PathBuf,
    pub console_dir: Option<PathBuf>,
    pub sender_dir: Option<PathBuf>,
    /// 試験用の合成のキャプチャ機器を使う（実機の無い環境の試験）。
    pub capture_synthetic: bool,
    pub verify_recordings: bool,
    pub log_json: bool,
    pub cluster: Option<ClusterConfig>,
    /// pull 入力・push 出力で接続してよい私設アドレスの範囲（CIDR）。既定では私設・ループバック・リンクローカルを拒否（仕様 9.4）。
    pub egress_allow: Vec<String>,
    /// このノードの帯域（Mbit/s、上り・下りそれぞれ）。グリッドの配置で数える（ADR-0013）。
    pub net_mbps: u64,
    /// このノードのラベル（`--label key=value`）。配置の条件（placement.labels）に使う。
    pub labels: std::collections::BTreeMap<String, String>,
    /// 仕事に貸す CPU（ミリコア）・メモリ（MB）の上書き。無ければ実機の 90%・85%。
    pub capacity_cpu_millis: Option<u64>,
    pub capacity_memory_mb: Option<u64>,
    /// 部屋のカスケードをノードの既定にする（部屋の設定 `cascade` が偽でも、別のノードの部屋の参加者をこのノードで受ける）。
    pub rooms_cascade: bool,
    /// 部屋の認証 Webhook（部屋の設定 `auth_webhook_url` が優先）。
    pub rooms_auth_webhook: Option<String>,
    /// 認証 Webhook の使い方（`after_token` = トークンを検証した後に問い合わせる、`only` = トークンの代わり）。
    pub rooms_auth_mode: RoomsAuthMode,
    /// 部屋の出来事を送る Webhook（少なくとも 1 回、署名つき）。
    pub rooms_event_webhook: Option<String>,
    /// Webhook の署名（HMAC-SHA256）の秘密のファイル（16 byte 以上）。
    pub rooms_webhook_secret_file: Option<PathBuf>,
    /// 録画の保存先（既定は手元のデータ置き場だけ）。
    pub recording_storage: RecordingStorageConfig,
    /// WebRTC の ICE の候補に載せる IP（`--webrtc-udp` の番号と組み合わせる）。空なら、待ち受けが 0.0.0.0・:: のときは
    /// ノードのインターフェースのアドレス、そうでなければ待ち受けのアドレス。
    pub webrtc_public_ips: Vec<std::net::IpAddr>,
    /// 要求の量の上限（`crate::ratelimit`）。0 で無効。IP ごと・主体ごとは 1 秒あたり（一度に 2 倍まで）。
    pub rate_limit_ip: u32,
    pub rate_limit_token: u32,
    /// 認証の失敗の上限（IP ごと、1 分あたり）。
    pub rate_limit_auth_failures: u32,
    /// 接続の開始（シグナリング・WHIP・WHEP・SSE）の上限（IP ごと、1 秒あたり）。
    pub rate_limit_connect: u32,
    /// SSE の再開のために残す直近のイベントの数。
    pub events_retain: usize,
    /// 監査ログ（`audit.log`）を回転する大きさ（バイト）。0 なら回転しない。
    pub audit_log_max_bytes: u64,
    /// 回転した監査ログを残す数（1〜1000）。
    pub audit_log_keep: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RoomsAuthMode {
    #[default]
    AfterToken,
    Only,
}

/// 録画の保存先（ADR-0005）。録画はまず手元に書き、確定したら S3 互換の保存先へ上げる。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RecordingStorageConfig {
    /// `local` または `s3://bucket/prefix`。
    pub uri: String,
    /// S3 互換のサービスの宛先（`https://host[:port]`）。無ければ AWS の地域の宛先。
    pub endpoint: Option<String>,
    pub region: String,
    /// `path`（`host/bucket/key`）または `virtual`（`bucket.host/key`）。既定は宛先を指定したら path、AWS なら virtual。
    pub addressing: Option<String>,
    /// 資格情報のファイル（JSON: access_key_id・secret_access_key・session_token）。無ければ環境変数 AWS_ACCESS_KEY_ID など。
    pub credentials_file: Option<PathBuf>,
    /// 上げ終わってから手元の写しを残す秒数（0 ならすぐ消す）。
    pub keep_local_secs: u64,
    /// これより大きいものは分割して上げる（部分の大きさ。5 MiB 以上）。
    pub part_size: u64,
}

impl Default for RecordingStorageConfig {
    fn default() -> Self {
        RecordingStorageConfig {
            uri: "local".into(),
            endpoint: None,
            region: "us-east-1".into(),
            addressing: None,
            credentials_file: None,
            keep_local_secs: 86_400,
            part_size: 16 << 20,
        }
    }
}

impl RecordingStorageConfig {
    pub fn is_remote(&self) -> bool {
        self.uri != "local"
    }

    /// 宛先（S3 の場所と、つなぐ先）。`local` なら None。
    pub fn remote(&self) -> Result<Option<(senca_s3::Location, senca_s3::Endpoint)>, String> {
        if !self.is_remote() {
            return Ok(None);
        }
        let loc = senca_s3::Location::parse(&self.uri).map_err(|e| format!("--recording-storage: {e}"))?;
        let path_style = match self.addressing.as_deref() {
            None => self.endpoint.is_some(),
            Some("path") => true,
            Some("virtual") => false,
            Some(o) => return Err(format!("--s3-addressing は path か virtual: {o}")),
        };
        let ep = match &self.endpoint {
            Some(u) => senca_s3::Endpoint::parse(u, &self.region, path_style).map_err(|e| format!("--s3-endpoint: {e}"))?,
            None => {
                let e = senca_s3::Endpoint::parse(&format!("https://s3.{}.amazonaws.com", self.region), &self.region, path_style)
                    .map_err(|e| format!("--s3-region: {e}"))?;
                senca_s3::Endpoint { path_style, ..e }
            }
        };
        if !path_style && loc.bucket.contains('.') && ep.tls {
            return Err("バケット名に . があると仮想ホスト形式の証明書が合いません（--s3-addressing path）".into());
        }
        Ok(Some((loc, ep)))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PublicUrls {
    pub rtmp: String,
    pub http: Option<String>,
    pub srt: Option<String>,
    #[serde(default)]
    pub rtsp: Option<String>,
}

impl Default for PublicUrls {
    fn default() -> Self {
        PublicUrls { rtmp: "rtmp://127.0.0.1:1935".into(), http: Some("http://127.0.0.1:8080".into()), srt: None, rtsp: None }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    pub node_id: u64,
    pub listen: SocketAddr,
    /// 投票するノード（自分を含む）。
    pub peers: Vec<ClusterPeer>,
    /// クラスタの合言葉のファイル（全ノードで同じ。ノード間の接続を認める）。
    pub secret_file: PathBuf,
    /// mTLS（仕様 9.2「node 間は mTLS」）。クラスタの認証局と、このノードの証明書（SAN に `senca-cluster`）。
    #[serde(default)]
    pub tls_ca: Option<PathBuf>,
    #[serde(default)]
    pub tls_cert: Option<PathBuf>,
    #[serde(default)]
    pub tls_key: Option<PathBuf>,
    /// ノード間の中継（ADR-0014）の待ち受け。既定は `listen` の次の番号。
    #[serde(default)]
    pub relay_listen: Option<SocketAddr>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterPeer {
    pub id: u64,
    /// ノード間の通信（Raft）の宛先。
    pub addr: SocketAddr,
    /// 管理 API の宛先（転送・案内に使う）。
    #[serde(default)]
    pub api: Option<String>,
    /// 中継の宛先。既定は `addr` の次の番号。
    #[serde(default)]
    pub relay: Option<SocketAddr>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            data_dir: PathBuf::from("out"),
            node_name: "node-1".into(),
            rtmp_listen: Some("0.0.0.0:1935".parse().unwrap()),
            rtmps_listen: None,
            http_listen: "0.0.0.0:8080".parse().unwrap(),
            https_listen: None,
            srt_listen: None,
            rtsp_listen: None,
            webrtc_udp: None,
            quic_listen: None,
            tls_cert: None,
            tls_key: None,
            public: PublicUrls::default(),
            max_playback_ttl_secs: 86_400,
            reconnect_grace_ms: 30_000,
            record_queue: 4096,
            retention_sweep_secs: 60,
            jwks: None,
            jwt_issuer: None,
            jwt_audience: None,
            jwt_tenant_claim: "tenant".into(),
            codec_workers: 0,
            codec_worker_bin: None,
            codec_lib_dir: None,
            zoneinfo_dir: PathBuf::from("/usr/share/zoneinfo"),
            console_dir: None,
            sender_dir: None,
            capture_synthetic: false,
            verify_recordings: false,
            log_json: false,
            cluster: None,
            egress_allow: Vec::new(),
            net_mbps: crate::grid::DEFAULT_NET_MBPS,
            labels: Default::default(),
            capacity_cpu_millis: None,
            capacity_memory_mb: None,
            recording_storage: RecordingStorageConfig::default(),
            rooms_cascade: false,
            rooms_auth_webhook: None,
            rooms_auth_mode: RoomsAuthMode::AfterToken,
            rooms_event_webhook: None,
            rooms_webhook_secret_file: None,
            webrtc_public_ips: Vec::new(),
            rate_limit_ip: DEFAULT_RATE_LIMIT_IP,
            rate_limit_token: DEFAULT_RATE_LIMIT_TOKEN,
            rate_limit_auth_failures: DEFAULT_RATE_LIMIT_AUTH_FAILURES,
            rate_limit_connect: DEFAULT_RATE_LIMIT_CONNECT,
            events_retain: crate::control::DEFAULT_EVENT_LOG,
            audit_log_max_bytes: crate::audit::DEFAULT_MAX_BYTES,
            audit_log_keep: crate::audit::DEFAULT_KEEP,
        }
    }
}

/// 要求の量の上限の既定（`--rate-limit-*`）。
pub const DEFAULT_RATE_LIMIT_IP: u32 = 300;
pub const DEFAULT_RATE_LIMIT_TOKEN: u32 = 200;
pub const DEFAULT_RATE_LIMIT_AUTH_FAILURES: u32 = 30;
pub const DEFAULT_RATE_LIMIT_CONNECT: u32 = 30;

/// ノードのラベルの数の上限。
pub const MAX_LABELS: usize = 64;

/// ラベルの形: キーは 1〜128 文字、値は 0〜128 文字、どちらも制御文字を含まない。`senca.io/` で始まるキーは予約。
pub fn check_label(k: &str, v: &str) -> Result<(), &'static str> {
    if k.is_empty() || k.len() > 128 || v.len() > 128 {
        return Err("の長さが違います（キーは 1〜128 文字、値は 128 文字まで）");
    }
    if k.chars().chain(v.chars()).any(char::is_control) {
        return Err("に制御文字は使えません");
    }
    if k.starts_with("senca.io/") || k == "raft_id" {
        return Err("のキーは予約です（senca.io/ で始まるもの・raft_id）");
    }
    Ok(())
}

pub const USAGE: &str = "senca-node [--config FILE] [--data-dir DIR] [--http ADDR] [--rtmp ADDR|off] [--rtmps ADDR] [--https ADDR]
  [--srt ADDR] [--rtsp ADDR] [--webrtc-udp ADDR] [--quic ADDR] [--tls-cert FILE --tls-key FILE]
  [--public-rtmp-url URL] [--public-http-url URL] [--public-srt-url URL] [--public-rtsp-url URL] [--capture-synthetic]
  [--max-playback-ttl SECS] [--reconnect-grace MS] [--record-queue N] [--retention-sweep SECS]
  [--jwks FILE --jwt-issuer ISS --jwt-audience AUD [--jwt-tenant-claim NAME]]
  [--codec-workers N] [--codec-worker-bin FILE] [--codec-lib-dir DIR] [--zoneinfo DIR]
  [--console-dir DIR] [--sender-dir DIR] [--verify-recordings] [--log-json] [--node-name NAME] [--egress-allow CIDR]...
  [--cluster-config FILE] [--net-mbps N] [--label KEY=VALUE]...
  [--capacity-cpu-millis N] [--capacity-memory-mb N]
  [--rooms-cascade] [--rooms-auth-webhook URL] [--rooms-auth-mode after_token|only] [--rooms-event-webhook URL]
  [--rooms-webhook-secret-file FILE] [--webrtc-public-ip IP[,IP]...]
  [--rate-limit-ip N] [--rate-limit-token N] [--rate-limit-auth-failures N] [--rate-limit-connect N] [--events-retain N]
  [--audit-log-max-bytes BYTES] [--audit-log-keep N]
  [--recording-storage local|s3://BUCKET[/PREFIX] [--s3-endpoint URL] [--s3-region REGION] [--s3-addressing path|virtual]
   [--s3-credentials FILE] [--s3-part-size BYTES] [--recording-keep-local SECS]]";

impl Config {
    /// Webhook の署名の秘密（ファイルの中身、前後の空白を除く）。
    pub fn webhook_secret(&self) -> Result<Option<Vec<u8>>, String> {
        let Some(p) = &self.rooms_webhook_secret_file else { return Ok(None) };
        let s = std::fs::read_to_string(p).map_err(|e| format!("--rooms-webhook-secret-file {}: {e}", p.display()))?;
        let s = s.trim();
        if s.len() < 16 {
            return Err("--rooms-webhook-secret-file: 秘密は 16 byte 以上にしてください".into());
        }
        Ok(Some(s.as_bytes().to_vec()))
    }

    pub fn from_args(args: &[String]) -> Result<Config, String> {
        let mut cfg = Config::default();
        // まず --config
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--config" {
                let p = args.get(i + 1).ok_or("--config needs a value")?;
                let b = std::fs::read(p).map_err(|e| format!("{p}: {e}"))?;
                cfg = serde_json::from_slice(&b).map_err(|e| format!("{p}: {e}"))?;
            }
            i += 1;
        }
        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_str();
            let flag = |name: &str| a == name;
            let mut val = || -> Result<String, String> {
                i += 1;
                args.get(i).cloned().ok_or_else(|| format!("{a} needs a value"))
            };
            let addr = |s: String| -> Result<SocketAddr, String> { s.parse().map_err(|_| format!("{a}: bad address {s}")) };
            let opt_addr = |s: String| -> Result<Option<SocketAddr>, String> {
                if s == "off" { Ok(None) } else { s.parse().map(Some).map_err(|_| format!("{a}: bad address {s}")) }
            };
            let num = |s: String| -> Result<u64, String> { s.parse().map_err(|_| format!("{a}: bad number {s}")) };
            let small = |n: u64| -> Result<u32, String> { u32::try_from(n).ok().filter(|n| *n <= 1_000_000).ok_or_else(|| format!("{a}: 0〜1000000")) };
            match a {
                "--config" => {
                    i += 1;
                }
                _ if flag("--data-dir") => cfg.data_dir = PathBuf::from(val()?),
                _ if flag("--node-name") => cfg.node_name = val()?,
                _ if flag("--http") => cfg.http_listen = addr(val()?)?,
                _ if flag("--https") => cfg.https_listen = opt_addr(val()?)?,
                _ if flag("--rtmp") => cfg.rtmp_listen = opt_addr(val()?)?,
                _ if flag("--rtmps") => cfg.rtmps_listen = opt_addr(val()?)?,
                _ if flag("--srt") => cfg.srt_listen = opt_addr(val()?)?,
                _ if flag("--rtsp") => cfg.rtsp_listen = opt_addr(val()?)?,
                _ if flag("--webrtc-udp") => cfg.webrtc_udp = opt_addr(val()?)?,
                _ if flag("--quic") => cfg.quic_listen = opt_addr(val()?)?,
                _ if flag("--tls-cert") => cfg.tls_cert = Some(PathBuf::from(val()?)),
                _ if flag("--tls-key") => cfg.tls_key = Some(PathBuf::from(val()?)),
                _ if flag("--public-rtmp-url") => cfg.public.rtmp = val()?,
                _ if flag("--public-http-url") => cfg.public.http = Some(val()?),
                _ if flag("--public-srt-url") => cfg.public.srt = Some(val()?),
                _ if flag("--public-rtsp-url") => cfg.public.rtsp = Some(val()?),
                _ if flag("--max-playback-ttl") => cfg.max_playback_ttl_secs = num(val()?)? as u32,
                _ if flag("--reconnect-grace") => cfg.reconnect_grace_ms = num(val()?)?,
                _ if flag("--record-queue") => cfg.record_queue = num(val()?)? as usize,
                _ if flag("--retention-sweep") => cfg.retention_sweep_secs = num(val()?)?,
                _ if flag("--jwks") => cfg.jwks = Some(PathBuf::from(val()?)),
                _ if flag("--jwt-issuer") => cfg.jwt_issuer = Some(val()?),
                _ if flag("--jwt-audience") => cfg.jwt_audience = Some(val()?),
                _ if flag("--jwt-tenant-claim") => cfg.jwt_tenant_claim = val()?,
                _ if flag("--codec-workers") => cfg.codec_workers = num(val()?)? as usize,
                _ if flag("--codec-worker-bin") => cfg.codec_worker_bin = Some(PathBuf::from(val()?)),
                _ if flag("--codec-lib-dir") => cfg.codec_lib_dir = Some(PathBuf::from(val()?)),
                _ if flag("--zoneinfo") => cfg.zoneinfo_dir = PathBuf::from(val()?),
                _ if flag("--console-dir") => cfg.console_dir = Some(PathBuf::from(val()?)),
                _ if flag("--sender-dir") => cfg.sender_dir = Some(PathBuf::from(val()?)),
                _ if flag("--capture-synthetic") => cfg.capture_synthetic = true,
                _ if flag("--verify-recordings") => cfg.verify_recordings = true,
                _ if flag("--log-json") => cfg.log_json = true,
                _ if flag("--egress-allow") => cfg.egress_allow.push(val()?),
                _ if flag("--net-mbps") => cfg.net_mbps = num(val()?)?,
                _ if flag("--capacity-cpu-millis") => cfg.capacity_cpu_millis = Some(num(val()?)?),
                _ if flag("--capacity-memory-mb") => cfg.capacity_memory_mb = Some(num(val()?)?),
                _ if flag("--recording-storage") => cfg.recording_storage.uri = val()?,
                _ if flag("--s3-endpoint") => cfg.recording_storage.endpoint = Some(val()?),
                _ if flag("--s3-region") => cfg.recording_storage.region = val()?,
                _ if flag("--s3-addressing") => cfg.recording_storage.addressing = Some(val()?),
                _ if flag("--s3-credentials") => cfg.recording_storage.credentials_file = Some(PathBuf::from(val()?)),
                _ if flag("--s3-part-size") => cfg.recording_storage.part_size = num(val()?)?,
                _ if flag("--recording-keep-local") => cfg.recording_storage.keep_local_secs = num(val()?)?,
                _ if flag("--rooms-cascade") => cfg.rooms_cascade = true,
                _ if flag("--rooms-auth-webhook") => cfg.rooms_auth_webhook = Some(val()?),
                _ if flag("--rooms-auth-mode") => {
                    cfg.rooms_auth_mode = match val()?.as_str() {
                        "after_token" | "after-token" => RoomsAuthMode::AfterToken,
                        "only" => RoomsAuthMode::Only,
                        v => return Err(format!("--rooms-auth-mode は after_token か only: {v}")),
                    }
                }
                _ if flag("--rooms-event-webhook") => cfg.rooms_event_webhook = Some(val()?),
                _ if flag("--rooms-webhook-secret-file") => cfg.rooms_webhook_secret_file = Some(PathBuf::from(val()?)),
                _ if flag("--webrtc-public-ip") => {
                    for x in val()?.split(',').map(str::trim).filter(|x| !x.is_empty()) {
                        let ip: std::net::IpAddr = x.parse().map_err(|_| format!("--webrtc-public-ip: IP アドレスではありません: {x}"))?;
                        cfg.webrtc_public_ips.push(ip);
                    }
                }
                _ if flag("--rate-limit-ip") => cfg.rate_limit_ip = small(num(val()?)?)?,
                _ if flag("--rate-limit-token") => cfg.rate_limit_token = small(num(val()?)?)?,
                _ if flag("--rate-limit-auth-failures") => cfg.rate_limit_auth_failures = small(num(val()?)?)?,
                _ if flag("--rate-limit-connect") => cfg.rate_limit_connect = small(num(val()?)?)?,
                _ if flag("--events-retain") => cfg.events_retain = num(val()?)? as usize,
                _ if flag("--audit-log-max-bytes") => cfg.audit_log_max_bytes = num(val()?)?,
                _ if flag("--audit-log-keep") => cfg.audit_log_keep = num(val()?)? as usize,
                _ if flag("--label") => {
                    let v = val()?;
                    let (k, x) = v.split_once('=').ok_or_else(|| format!("--label は key=value: {v}"))?;
                    check_label(k, x).map_err(|e| format!("--label {e}: {v}"))?;
                    cfg.labels.insert(k.to_string(), x.to_string());
                }
                _ if flag("--cluster-config") => {
                    let p = val()?;
                    let b = std::fs::read(&p).map_err(|e| format!("{p}: {e}"))?;
                    cfg.cluster = Some(serde_json::from_slice(&b).map_err(|e| format!("{p}: {e}"))?);
                }
                _ => return Err(format!("unknown argument: {a}\n{USAGE}")),
            }
            i += 1;
        }
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        // 設定ファイルの labels も --label と同じ規則で確かめる
        for (k, v) in &self.labels {
            check_label(k, v).map_err(|e| format!("labels {e}: {k}={v}"))?;
        }
        if self.labels.len() > MAX_LABELS {
            return Err(format!("labels は {MAX_LABELS} 個まで"));
        }
        if self.jwks.is_some() && (self.jwt_issuer.is_none() || self.jwt_audience.is_none()) {
            return Err("--jwks needs --jwt-issuer and --jwt-audience".into());
        }
        if (self.rtmps_listen.is_some() || self.https_listen.is_some()) && (self.tls_cert.is_none() || self.tls_key.is_none()) {
            return Err("--rtmps/--https need --tls-cert and --tls-key".into());
        }
        if self.quic_listen.is_some() && (self.tls_cert.is_none() || self.tls_key.is_none()) {
            return Err("--quic needs --tls-cert and --tls-key".into());
        }
        if !(16..=1 << 20).contains(&self.record_queue) {
            return Err("--record-queue must be 16..=1048576".into());
        }
        if let Some(c) = &self.cluster {
            if c.node_id == 0 || !c.peers.iter().any(|p| p.id == c.node_id) {
                return Err("cluster: node_id は 1 以上で、peers に自分を含めてください".into());
            }
            if !(1..=7).contains(&c.peers.len()) {
                return Err("cluster: 投票するノードは 1〜7 台".into());
            }
            let mut ids = std::collections::HashSet::new();
            if !c.peers.iter().all(|p| p.id != 0 && ids.insert(p.id)) {
                return Err("cluster: peers の id は 1 以上で重ならないように".into());
            }
            crate::relay::check_addrs(c)?;
        }
        for c in &self.egress_allow {
            crate::egress::Cidr::parse(c).ok_or_else(|| format!("--egress-allow: bad CIDR {c}"))?;
        }
        self.recording_storage.remote()?;
        if !(senca_s3::responses::MIN_PART..=senca_s3::responses::MAX_PART).contains(&self.recording_storage.part_size) {
            return Err("--s3-part-size は 5 MiB〜5 GiB".into());
        }
        for (name, u) in [("--rooms-auth-webhook", &self.rooms_auth_webhook), ("--rooms-event-webhook", &self.rooms_event_webhook)] {
            if let Some(u) = u {
                if !senca_rooms::model::valid_webhook_url(u) {
                    return Err(format!("{name}: http:// か https:// の URL を渡してください: {u}"));
                }
                if self.rooms_webhook_secret_file.is_none() {
                    return Err(format!("{name} には --rooms-webhook-secret-file（署名の秘密）が要ります"));
                }
            }
        }
        if self.rooms_webhook_secret_file.is_some() {
            self.webhook_secret()?;
        }
        if self.webrtc_public_ips.len() > 8 {
            return Err("--webrtc-public-ip は 8 個まで".into());
        }
        if self.webrtc_public_ips.iter().any(|ip| ip.is_unspecified() || ip.is_multicast()) {
            return Err("--webrtc-public-ip に 0.0.0.0・::・マルチキャストのアドレスは使えません".into());
        }
        if self.events_retain > 1_000_000 {
            return Err("--events-retain は 0〜1000000".into());
        }
        if self.audit_log_max_bytes != 0 && self.audit_log_max_bytes < 4096 {
            return Err("--audit-log-max-bytes は 0（回転しない）か 4096 以上".into());
        }
        if !(1..=1000).contains(&self.audit_log_keep) {
            return Err("--audit-log-keep は 1〜1000".into());
        }
        if self.max_playback_ttl_secs == 0 {
            return Err("--max-playback-ttl must be > 0".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn labels_from_the_file_are_validated_like_the_flag() {
        let d = std::env::temp_dir().join(format!("senca-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("c.json");
        let file = f.to_str().unwrap();
        std::fs::write(&f, r#"{"labels": {"senca.io/name": "x"}}"#).unwrap();
        let e = Config::from_args(&args(&["--config", file])).unwrap_err();
        assert!(e.contains("予約"), "{e}");
        std::fs::write(&f, format!(r#"{{"labels": {{"{}": "x"}}}}"#, "k".repeat(129))).unwrap();
        assert!(Config::from_args(&args(&["--config", file])).is_err());
        std::fs::write(&f, "{\"labels\": {\"zone\": \"a\\u0007b\"}}").unwrap();
        assert!(Config::from_args(&args(&["--config", file])).is_err());
        std::fs::write(&f, r#"{"labels": {"zone": "a"}}"#).unwrap();
        let c = Config::from_args(&args(&["--config", file])).unwrap();
        assert_eq!(c.labels.get("zone").map(String::as_str), Some("a"));
        assert!(Config::from_args(&args(&["--label", "senca.io/webrtc=true"])).is_err());
        assert!(Config::from_args(&args(&["--label", "zone=b"])).is_ok());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn audit_log_rotation_flags() {
        let c = Config::from_args(&[]).unwrap();
        assert_eq!((c.audit_log_max_bytes, c.audit_log_keep), (64 << 20, 8));
        let c = Config::from_args(&args(&["--audit-log-max-bytes", "0", "--audit-log-keep", "3"])).unwrap();
        assert_eq!((c.audit_log_max_bytes, c.audit_log_keep), (0, 3));
        assert!(Config::from_args(&args(&["--audit-log-max-bytes", "100"])).is_err());
        assert!(Config::from_args(&args(&["--audit-log-keep", "0"])).is_err());
        assert!(Config::from_args(&args(&["--audit-log-keep", "1001"])).is_err());
    }
}
