//! sans-io の TLS（senca-tls）を tokio のストリームとして使う。
//!
//! TCP と、アプリ側に渡す `DuplexStream` の間で、TLS の記録を暗号化・復号して中継するタスクを動かす。
//! 平文が 1 回だけ余分にコピーされるが、使うのは制御 API・RTMPS・外向きの HTTPS で、メディアの大量配信
//! （平文の HLS・SRT・WebRTC）は通らない。

use senca_tls::{ClientConfig, Connection, ServerConfig, TlsSession};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::TcpStream;

const BUF: usize = 64 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// サーバーとして受ける。ハンドシェイクが済んだら、アプリ側の端を返す。
pub async fn accept(tcp: TcpStream, cfg: Arc<ServerConfig>) -> Result<DuplexStream, String> {
    let conn = Connection::server(cfg, SystemTime::now()).map_err(|e| format!("{e:?}"))?;
    run(tcp, conn).await
}

/// クライアントとしてつなぐ。
pub async fn connect(tcp: TcpStream, cfg: Arc<ClientConfig>, server_name: &str) -> Result<DuplexStream, String> {
    let conn = Connection::client(cfg, server_name, SystemTime::now()).map_err(|e| format!("{e:?}"))?;
    run(tcp, conn).await
}

async fn run(mut tcp: TcpStream, mut conn: Connection) -> Result<DuplexStream, String> {
    let mut buf = vec![0u8; BUF];
    // ハンドシェイク中に届いた平文（相手がすぐ送ってきた場合）
    let mut early: Vec<Vec<u8>> = Vec::new();
    // ハンドシェイク
    let hs = async {
        loop {
            while conn.wants_write() {
                let out = conn.take_tls();
                tcp.write_all(&out).await.map_err(|e| e.to_string())?;
            }
            if !conn.is_handshaking() {
                return Ok::<(), String>(());
            }
            let n = tcp.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("ハンドシェイク中に切れました".into());
            }
            let mut off = 0;
            while off < n {
                let k = conn.read_tls(&buf[off..n]).map_err(|e| format!("{e:?}"))?;
                if k == 0 {
                    match conn.take_plaintext() {
                        Some(p) => early.push(p),
                        None => return Err("TLS が入力を受け付けません".into()),
                    }
                    continue;
                }
                off += k;
            }
        }
    };
    if let Err(e) = tokio::time::timeout(HANDSHAKE_TIMEOUT, hs).await.unwrap_or_else(|_| Err("ハンドシェイクの時間切れ".into())) {
        // 致命的なエラーのアラートは送ってから閉じる
        let out = conn.take_tls();
        let _ = tcp.write_all(&out).await;
        return Err(e);
    }
    let (app_side, mut pump_side) = tokio::io::duplex(BUF);
    tokio::spawn(async move {
        let mut nbuf = vec![0u8; BUF];
        let mut pbuf = vec![0u8; BUF];
        // ハンドシェイク中に届いた平文
        for p in early {
            if pump_side.write_all(&p).await.is_err() {
                return;
            }
        }
        while let Some(p) = conn.take_plaintext() {
            if pump_side.write_all(&p).await.is_err() {
                return;
            }
        }
        loop {
            tokio::select! {
                r = tcp.read(&mut nbuf) => {
                    let n = match r { Ok(0) | Err(_) => break, Ok(n) => n };
                    let mut off = 0;
                    let mut bad = false;
                    while off < n {
                        let stalled = match conn.read_tls(&nbuf[off..n]) {
                            Ok(0) => true,
                            Ok(k) => { off += k; false }
                            Err(_) => { bad = true; break; }
                        };
                        let mut drained = false;
                        while let Some(p) = conn.take_plaintext() {
                            drained = true;
                            if pump_side.write_all(&p).await.is_err() { return; }
                        }
                        if stalled && !drained { bad = true; break; }
                    }
                    if bad || conn.peer_closed() { break; }
                }
                r = pump_side.read(&mut pbuf) => {
                    let n = match r { Ok(0) | Err(_) => { conn.send_close_notify(); 0 } Ok(n) => n };
                    if n > 0 && conn.write_plaintext(&pbuf[..n]).is_err() { break; }
                    if n == 0 {
                        let out = conn.take_tls();
                        let _ = tcp.write_all(&out).await;
                        break;
                    }
                }
            }
            while conn.wants_write() {
                let out = conn.take_tls();
                if tcp.write_all(&out).await.is_err() {
                    return;
                }
            }
        }
        let _ = pump_side.shutdown().await;
    });
    Ok(app_side)
}

/// ノードの TLS 設定（--tls-cert / --tls-key）。
pub fn server_config(cert: &std::path::Path, key: &std::path::Path, alpn: &[&[u8]]) -> Result<Arc<ServerConfig>, String> {
    let ck = senca_tls::CertifiedKey::from_pem_files(cert, key).map_err(|e| format!("tls cert/key: {e:?}"))?;
    let mut cfg = ServerConfig::new(Arc::new(ck)).map_err(|e| format!("tls config: {e:?}"))?;
    cfg.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    Ok(Arc::new(cfg))
}

/// 外向きの TLS（システムの信頼する認証局で検証）。
pub fn client_config(alpn: &[&[u8]]) -> Result<Arc<ClientConfig>, String> {
    let roots = senca_tls::RootStore::system().map_err(|e| format!("trust roots: {e:?}"))?;
    let mut cfg = ClientConfig::new(senca_tls::ServerVerifier::WebPki { roots: Arc::new(roots), spki_pins: vec![] });
    cfg.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    Ok(Arc::new(cfg))
}
