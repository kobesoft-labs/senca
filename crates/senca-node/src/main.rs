//! senca-node: 単一ノードの実行ファイル（gateway・media・origin・control を同居。仕様 3.1）。

mod api;
mod app;
mod cluster;
mod codec;
mod config;
mod control;
mod egress;
mod events;
mod export;
mod http;
mod hls_pull;
mod httpc;
mod hub;
mod io;
mod live;
mod media;
mod packager;
mod paas;
mod pipelines;
mod plan;
mod recorder;
mod recovery;
mod rtmp;
mod rtsp_pull;
mod srt;
mod scheduler;
mod secrets;
mod snapshot;
mod store;
mod thumbnail;
mod tls_io;
mod util;

use app::App;
use config::Config;
use control::ControlCore;
use senca_control::auth::Scope;
use senca_control::model::{ApiToken, NodeInfo};
use senca_control::state::{Command, ControlState};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // コンテナのヘルスチェック: `senca-node --healthcheck http://127.0.0.1:8080/readyz`
    if args.first().map(String::as_str) == Some("--healthcheck") {
        std::process::exit(healthcheck(args.get(1).map(String::as_str).unwrap_or("http://127.0.0.1:8080/readyz")));
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", config::USAGE);
        return;
    }
    let cfg = match Config::from_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("senca").build().expect("tokio runtime");
    if let Err(e) = rt.block_on(run(cfg)) {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
}

async fn run(cfg: Config) -> Result<(), String> {
    std::fs::create_dir_all(&cfg.data_dir).map_err(|e| format!("{}: {e}", cfg.data_dir.display()))?;
    let (store, mut state) = store::Store::open(&cfg.data_dir.join("control")).map_err(|e| format!("control store: {e}"))?;
    let jwt = match &cfg.jwks {
        Some(p) => {
            let b = std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
            Some(
                senca_control::jwt::JwtVerifier::from_jwks(
                    &b,
                    cfg.jwt_issuer.as_deref().unwrap_or(""),
                    cfg.jwt_audience.as_deref().unwrap_or(""),
                    &cfg.jwt_tenant_claim,
                )
                .map_err(|e| format!("jwks: {e}"))?,
            )
        }
        None => None,
    };
    // ノード ID はデータ置き場に固定
    let node_id_path = cfg.data_dir.join("node-id");
    let node_id = match std::fs::read_to_string(&node_id_path) {
        Ok(s) if s.trim().parse::<senca_core::Id128>().is_ok() => s.trim().to_string(),
        _ => {
            let id = senca_core::Id128::random().map_err(|e| e.to_string())?.to_string();
            store::write_atomic(&node_id_path, id.as_bytes()).map_err(|e| e.to_string())?;
            id
        }
    };
    let _ = &mut state;
    let clustered = cfg.cluster.is_some();
    // 分散では制御状態の永続化は Raft のログ（単一ノードのログは使わない）
    let mut core = if clustered { ControlCore::new(ControlState::default(), None, jwt) } else { ControlCore::new(state, Some(store), jwt) };
    if !clustered {
        bootstrap(&cfg, &mut core, &node_id)?;
    }
    let codec = match codec::CodecService::start(&cfg).await {
        Ok(c) => Some(Arc::new(c)),
        Err(e) => {
            eprintln!("codec worker unavailable: {e} (静止画・サムネイル・変換は 503 になります)");
            None
        }
    };
    let secret_box = secrets::SecretBox::open(&cfg.data_dir.join("control").join("secret-key"))?;
    let app = App::new(cfg.clone(), core, node_id, codec, secret_box);
    if let Some(cc) = &cfg.cluster {
        let secret = std::fs::read_to_string(&cc.secret_file).map_err(|e| format!("{}: {e}", cc.secret_file.display()))?;
        let secret = secret.trim().to_string();
        if secret.len() < 16 {
            return Err("cluster: 合言葉は 16 文字以上にしてください".into());
        }
        let peers = cc.peers.iter().map(|p| (p.id, p.addr)).collect();
        let c = cluster::start(app.clone(), cc.node_id, cc.listen, peers, secret)?;
        let _ = app.cluster.set(c);
        app.watch_ownership();
        app.log(&format!("cluster: node {} started ({} voters)", cc.node_id, cc.peers.len()));
        // リーダーが決まってから初期化（管理者トークンは ID が最小のノードだけが作る）
        let a = app.clone();
        let cc = cc.clone();
        let nid = a.node_id.clone();
        tokio::task::spawn_blocking(move || cluster_bootstrap(&a, &cc, &nid));
    }
    recovery::recover(&app);
    app.log(&format!("senca-node {} started (node {})", env!("CARGO_PKG_VERSION"), app.node_id));

    let http = TcpListener::bind(cfg.http_listen).await.map_err(|e| format!("http {}: {e}", cfg.http_listen))?;
    app.log(&format!("http listening on {}", cfg.http_listen));
    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, peer)) = http.accept().await else { continue };
                let _ = sock.set_nodelay(true);
                let app = app.clone();
                tokio::spawn(async move { http::serve(app, sock, peer.to_string()).await });
            }
        });
    }
    // TLS の受け口（HTTPS・RTMPS）
    if cfg.https_listen.is_some() || cfg.rtmps_listen.is_some() {
        let (cert, key) = (cfg.tls_cert.clone().unwrap(), cfg.tls_key.clone().unwrap());
        if let Some(addr) = cfg.https_listen {
            let tcfg = tls_io::server_config(&cert, &key, &[b"http/1.1"])?;
            let l = TcpListener::bind(addr).await.map_err(|e| format!("https {addr}: {e}"))?;
            app.log(&format!("https listening on {addr}"));
            let app = app.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((sock, peer)) = l.accept().await else { continue };
                    let _ = sock.set_nodelay(true);
                    let (app, tcfg) = (app.clone(), tcfg.clone());
                    tokio::spawn(async move {
                        match tls_io::accept(sock, tcfg).await {
                            Ok(s) => http::serve(app, s, peer.to_string()).await,
                            Err(e) => app.log(&format!("https {peer}: {e}")),
                        }
                    });
                }
            });
        }
        if let Some(addr) = cfg.rtmps_listen {
            let tcfg = tls_io::server_config(&cert, &key, &[])?;
            let l = TcpListener::bind(addr).await.map_err(|e| format!("rtmps {addr}: {e}"))?;
            app.log(&format!("rtmps listening on {addr}"));
            let app = app.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((sock, peer)) = l.accept().await else { continue };
                    let _ = sock.set_nodelay(true);
                    let (app, tcfg) = (app.clone(), tcfg.clone());
                    tokio::spawn(async move {
                        match tls_io::accept(sock, tcfg).await {
                            Ok(s) => rtmp::serve(app, s, peer.to_string()).await,
                            Err(e) => app.log(&format!("rtmps {peer}: {e}")),
                        }
                    });
                }
            });
        }
    }
    if let Some(addr) = cfg.rtmp_listen {
        let l = TcpListener::bind(addr).await.map_err(|e| format!("rtmp {addr}: {e}"))?;
        app.log(&format!("rtmp listening on {addr}"));
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, peer)) = l.accept().await else { continue };
                let _ = sock.set_nodelay(true);
                let app = app.clone();
                tokio::spawn(async move { rtmp::serve(app, sock, peer.to_string()).await });
            }
        });
    }
    if let Some(addr) = cfg.srt_listen {
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = srt::serve(app.clone(), addr).await {
                app.log(&format!("srt: {e}"));
            }
        });
    }
    tokio::spawn(scheduler::run(app.clone()));
    tokio::spawn(snapshot::run_snapshotter(app.clone()));
    {
        let app = app.clone();
        let every = Duration::from_secs(cfg.retention_sweep_secs.max(1));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let a = app.clone();
                let _ = tokio::task::spawn_blocking(move || recovery::retention_sweep(&a)).await;
                // Idempotency の記録の期限切れ
                let now = util::now_ms();
                let stale = app.control.lock().unwrap().state.idempotency.values().any(|r| r.created_at_ms < now - senca_control::state::IDEMPOTENCY_TTL_MS);
                if stale {
                    let a = app.clone();
                    let _ = tokio::task::spawn_blocking(move || a.propose(Command::ExpireIdempotency { before_ms: now - senca_control::state::IDEMPOTENCY_TTL_MS })).await;
                }
            }
        });
    }
    app.pipelines_reconcile();
    if !clustered {
        app.io_reconcile();
        app.set_ready();
    }
    // 終了の合図
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|e| e.to_string())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    app.log("shutting down");
    app.shutdown().await;
    app.log("bye");
    Ok(())
}

/// 初回起動: 再生 URL の鍵と、最初の管理者トークン。
fn bootstrap(cfg: &Config, core: &mut ControlCore, node_id: &str) -> Result<(), String> {
    if core.state.playback_key.is_none() {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).map_err(|e| e.to_string())?;
        core.propose(Command::SetPlaybackKey { key_hex: senca_control::secret::hex(&k) }).map_err(|e| e.to_string())?;
    }
    if !core.state.nodes.contains_key(node_id) {
        core.propose(Command::PutNode {
            node: NodeInfo {
                id: node_id.into(),
                name: cfg.node_name.clone(),
                draining: false,
                registered_at_ms: util::now_ms(),
                media_addr: None,
                labels: Default::default(),
            },
        })
        .map_err(|e| e.to_string())?;
    }
    if core.state.tokens.is_empty() {
        let value = senca_control::secret::generate(senca_control::auth::TOKEN_PREFIX).map_err(|e| e.to_string())?;
        let path = cfg.data_dir.join("control").join("bootstrap-admin-token");
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            f.write_all(value.expose().as_bytes()).and_then(|_| f.sync_all()).map_err(|e| e.to_string())?;
        }
        let mut scopes = std::collections::BTreeSet::new();
        scopes.insert(Scope::TenantAdmin);
        scopes.insert(Scope::NodeAdmin);
        core.propose(Command::CreateToken {
            token: ApiToken {
                id: senca_core::Id128::random().map_err(|e| e.to_string())?.to_string(),
                tenant: "default".into(),
                name: "bootstrap-admin".into(),
                hash: senca_control::secret::hash(value.expose()),
                scopes,
                created_at_ms: util::now_ms(),
                expires_at_ms: None,
            },
        })
        .map_err(|e| e.to_string())?;
        eprintln!("created the first admin token in {} (mode 0600)", path.display());
    }
    Ok(())
}

/// ループバックへの平文 HTTP で 200 が返れば 0。
fn healthcheck(url: &str) -> i32 {
    use std::io::{Read, Write};
    let Some(rest) = url.strip_prefix("http://") else { return 2 };
    let (hp, path) = rest.split_once('/').map(|(a, b)| (a, format!("/{b}"))).unwrap_or((rest, "/".into()));
    let Ok(mut s) = std::net::TcpStream::connect(hp) else { return 1 };
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    if s.write_all(format!("GET {path} HTTP/1.1\r\nHost: {hp}\r\nConnection: close\r\n\r\n").as_bytes()).is_err() {
        return 1;
    }
    let mut b = [0u8; 16];
    match s.read(&mut b) {
        Ok(n) if b[..n].starts_with(b"HTTP/1.1 200") => 0,
        _ => 1,
    }
}

/// 分散の初期化。何度呼ばれても同じ結果になるように、状態を見てから提案する。
fn cluster_bootstrap(app: &std::sync::Arc<App>, cc: &config::ClusterConfig, node_id: &str) {
    let first = cc.peers.iter().map(|p| p.id).min() == Some(cc.node_id);
    for _ in 0..600 {
        if app.cluster.get().and_then(|c| c.leader()).is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut tries = 0;
    loop {
        tries += 1;
        let (has_node, has_key, has_tokens) = {
            let core = app.control.lock().unwrap();
            (core.state.nodes.contains_key(node_id), core.state.playback_key.is_some(), !core.state.tokens.is_empty())
        };
        let mut ok = true;
        if !has_node {
            ok &= app
                .propose(Command::PutNode {
                    node: NodeInfo {
                        id: node_id.into(),
                        name: app.cfg.node_name.clone(),
                        draining: false,
                        registered_at_ms: util::now_ms(),
                        media_addr: cc.peers.iter().find(|p| p.id == cc.node_id).and_then(|p| p.api.clone()),
                        labels: [("raft_id".to_string(), cc.node_id.to_string())].into_iter().collect(),
                    },
                })
                .is_ok();
        }
        if first && !has_key {
            let mut k = [0u8; 32];
            let _ = getrandom::fill(&mut k);
            ok &= app.propose(Command::SetPlaybackKey { key_hex: senca_control::secret::hex(&k) }).is_ok();
        }
        if first && !has_tokens {
            match senca_control::secret::generate(senca_control::auth::TOKEN_PREFIX) {
                Ok(value) => {
                    let mut scopes = std::collections::BTreeSet::new();
                    scopes.insert(Scope::TenantAdmin);
                    scopes.insert(Scope::NodeAdmin);
                    let t = ApiToken {
                        id: senca_core::Id128::random().map(|i| i.to_string()).unwrap_or_default(),
                        tenant: "default".into(),
                        name: "bootstrap-admin".into(),
                        hash: senca_control::secret::hash(value.expose()),
                        scopes,
                        created_at_ms: util::now_ms(),
                        expires_at_ms: None,
                    };
                    if app.propose(Command::CreateToken { token: t }).is_ok() {
                        let path = app.cfg.data_dir.join("control").join("bootstrap-admin-token");
                        use std::io::Write;
                        use std::os::unix::fs::OpenOptionsExt;
                        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
                            let _ = f.write_all(value.expose().as_bytes()).and_then(|_| f.sync_all());
                            app.log(&format!("created the first admin token in {} (mode 0600)", path.display()));
                        }
                    } else {
                        ok = false;
                    }
                }
                Err(_) => ok = false,
            }
        }
        if ok {
            app.set_ready();
            app.io_reconcile();
            return;
        }
        if tries > 600 {
            app.log("cluster: bootstrap did not complete");
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
