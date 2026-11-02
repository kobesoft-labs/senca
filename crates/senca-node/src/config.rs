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
    pub moq_listen: Option<SocketAddr>,
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
    pub verify_recordings: bool,
    pub log_json: bool,
    pub cluster: Option<ClusterConfig>,
    /// pull 入力・push 出力で接続してよい私設アドレスの範囲（CIDR）。既定では私設・ループバック・リンクローカルを拒否（仕様 9.4）。
    pub egress_allow: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PublicUrls {
    pub rtmp: String,
    pub http: Option<String>,
    pub srt: Option<String>,
}

impl Default for PublicUrls {
    fn default() -> Self {
        PublicUrls { rtmp: "rtmp://127.0.0.1:1935".into(), http: Some("http://127.0.0.1:8080".into()), srt: None }
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
            moq_listen: None,
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
            verify_recordings: false,
            log_json: false,
            cluster: None,
            egress_allow: Vec::new(),
        }
    }
}

pub const USAGE: &str = "senca-node [--config FILE] [--data-dir DIR] [--http ADDR] [--rtmp ADDR|off] [--rtmps ADDR] [--https ADDR]
  [--srt ADDR] [--rtsp ADDR] [--webrtc-udp ADDR] [--moq ADDR] [--tls-cert FILE --tls-key FILE]
  [--public-rtmp-url URL] [--public-http-url URL] [--public-srt-url URL]
  [--max-playback-ttl SECS] [--reconnect-grace MS] [--record-queue N] [--retention-sweep SECS]
  [--jwks FILE --jwt-issuer ISS --jwt-audience AUD [--jwt-tenant-claim NAME]]
  [--codec-workers N] [--codec-worker-bin FILE] [--codec-lib-dir DIR] [--zoneinfo DIR]
  [--console-dir DIR] [--verify-recordings] [--log-json] [--node-name NAME] [--egress-allow CIDR]...
  [--cluster-config FILE]";

impl Config {
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
                _ if flag("--moq") => cfg.moq_listen = opt_addr(val()?)?,
                _ if flag("--tls-cert") => cfg.tls_cert = Some(PathBuf::from(val()?)),
                _ if flag("--tls-key") => cfg.tls_key = Some(PathBuf::from(val()?)),
                _ if flag("--public-rtmp-url") => cfg.public.rtmp = val()?,
                _ if flag("--public-http-url") => cfg.public.http = Some(val()?),
                _ if flag("--public-srt-url") => cfg.public.srt = Some(val()?),
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
                _ if flag("--verify-recordings") => cfg.verify_recordings = true,
                _ if flag("--log-json") => cfg.log_json = true,
                _ if flag("--egress-allow") => cfg.egress_allow.push(val()?),
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
        if self.jwks.is_some() && (self.jwt_issuer.is_none() || self.jwt_audience.is_none()) {
            return Err("--jwks needs --jwt-issuer and --jwt-audience".into());
        }
        if (self.rtmps_listen.is_some() || self.https_listen.is_some()) && (self.tls_cert.is_none() || self.tls_key.is_none()) {
            return Err("--rtmps/--https need --tls-cert and --tls-key".into());
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
        }
        for c in &self.egress_allow {
            crate::egress::Cidr::parse(c).ok_or_else(|| format!("--egress-allow: bad CIDR {c}"))?;
        }
        if self.max_playback_ttl_secs == 0 {
            return Err("--max-playback-ttl must be > 0".into());
        }
        Ok(())
    }
}
