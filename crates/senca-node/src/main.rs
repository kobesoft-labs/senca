//! senca-node: 単一ノードの実行ファイル（gateway・media・origin・control を同居。仕様 3.1）。

mod api;
mod app;
mod codec;
mod config;
mod control;
mod events;
mod export;
mod http;
mod hub;
mod live;
mod media;
mod packager;
mod plan;
mod recorder;
mod recovery;
mod rtmp;
mod scheduler;
mod snapshot;
mod store;
mod thumbnail;
mod util;

use app::App;
use config::Config;
use control::ControlCore;
use senca_control::auth::Scope;
use senca_control::model::{ApiToken, NodeInfo};
use senca_control::state::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
    let mut core = ControlCore::new(state, Some(store), jwt);
    bootstrap(&cfg, &mut core, &node_id)?;
    let codec = match codec::CodecService::start(&cfg).await {
        Ok(c) => Some(Arc::new(c)),
        Err(e) => {
            eprintln!("codec worker unavailable: {e} (静止画・サムネイル・変換は 503 になります)");
            None
        }
    };
    let app = App::new(cfg.clone(), core, node_id, codec);
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
                let mut core = app.control.lock().unwrap();
                if core.state.idempotency.values().any(|r| r.created_at_ms < now - senca_control::state::IDEMPOTENCY_TTL_MS) {
                    let _ = core.propose(Command::ExpireIdempotency { before_ms: now - senca_control::state::IDEMPOTENCY_TTL_MS });
                }
            }
        });
    }
    app.set_ready();
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
