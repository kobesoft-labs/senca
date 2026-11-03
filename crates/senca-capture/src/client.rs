//! 親（ノード）側: capture worker を起動して使う（非同期、tokio）。
//!
//! ノードは機器の SDK を自分のプロセスに読み込まず、ここから `senca-capture-worker` を起動する。
//!
//! - [`list_devices`] — `list` を実行して機器の一覧を得る（`GET /devices` 用。時間切れ付き）。
//! - [`CaptureWorker::spawn`] — `serve` を起動し、`Hello` を確かめ、`SessionOpen` を送り、`Opened` を待つ。
//!   以後の出力は [`WorkerEvent`] として受け取る（上限付きの通り道。受け手が遅れれば worker のパイプが詰まり、
//!   worker の符号化が止まり、機器の側でフレームが落ちる）。
//! - [`CaptureWorker::close`] — 出し切って閉じる。[`CaptureWorker::kill`] — すぐ止める。
//!
//! worker が落ちても親には [`WorkerEvent::Exited`] が届くだけ（仕様 3.1、AC-17）。

use crate::error::{CaptureError, ErrorKind, Result};
use crate::spec::CaptureSpec;
use crate::worker::{DeviceListing, parse_error_body};
use bytes::Bytes;
use senca_codec::registry::Policy;
use senca_codec_worker::protocol::{
    self, Hello, Message, PROTOCOL_VERSION, SessionEventKind, SessionOpen, SessionOutputKind,
};
use senca_core::{AccessUnit, CodecConfig};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

/// `list` の出力の上限。
const MAX_LISTING: usize = 4 * 1024 * 1024;
/// 出力の通り道の長さ（メッセージ数）。
const EVENT_QUEUE: usize = 256;

/// worker の起動の仕方。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCommand {
    pub program: PathBuf,
    /// 追加の引数（`--synthetic`・`--no-seccomp` など）。
    pub args: Vec<String>,
    /// 追加の環境変数（`SENCA_CODEC_LIB_DIR`・`SENCA_CAPTURE_LIB_DIR` など）。
    pub env: Vec<(String, String)>,
    /// worker の標準エラーの行を渡す先（無ければ親の標準エラーに引き継ぐ）。
    pub stderr_log: Option<senca_codec_worker::pool::LogSink>,
}

impl WorkerCommand {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        WorkerCommand {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            stderr_log: None,
        }
    }

    fn command(&self, mode: &str) -> Command {
        let mut c = Command::new(&self.program);
        c.arg(mode)
            .args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if self.stderr_log.is_some() { Stdio::piped() } else { Stdio::inherit() })
            .kill_on_drop(true);
        c
    }

    fn spawn(&self, mode: &str) -> std::io::Result<Child> {
        let mut child = self.command(mode).spawn()?;
        if let (Some(l), Some(e)) = (&self.stderr_log, child.stderr.take()) {
            l.relay(e);
        }
        Ok(child)
    }
}

fn spawn_err(e: std::io::Error) -> CaptureError {
    CaptureError::unavailable(format!("cannot start capture worker: {e}"))
}

/// 機器の一覧を得る。
pub async fn list_devices(cmd: &WorkerCommand, timeout: Duration) -> Result<DeviceListing> {
    let mut child = cmd.spawn("list").map_err(spawn_err)?;
    drop(child.stdin.take());
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| CaptureError::internal("no stdout"))?;
    let read = async {
        let mut buf = Vec::new();
        (&mut stdout)
            .take(MAX_LISTING as u64 + 1)
            .read_to_end(&mut buf)
            .await
            .map_err(|e| CaptureError::internal(format!("read listing: {e}")))?;
        let status = child
            .wait()
            .await
            .map_err(|e| CaptureError::internal(format!("wait: {e}")))?;
        Ok::<_, CaptureError>((buf, status))
    };
    let (buf, status) = tokio::time::timeout(timeout, read)
        .await
        .map_err(|_| CaptureError::unavailable("capture worker did not answer in time"))??;
    if buf.len() > MAX_LISTING {
        return Err(CaptureError::internal("device listing too large"));
    }
    if !status.success() {
        return Err(CaptureError::internal(format!(
            "capture worker exited with {status}"
        )));
    }
    let l: DeviceListing = serde_json::from_slice(&buf)
        .map_err(|e| CaptureError::internal(format!("bad device listing: {e}")))?;
    if l.version != crate::worker::LISTING_VERSION {
        return Err(CaptureError::internal(format!(
            "unsupported listing version {}",
            l.version
        )));
    }
    Ok(l)
}

/// worker からの通知。
#[derive(Debug, Clone)]
pub enum WorkerEvent {
    /// 符号化済みの AU（トラックごと。`config` は最初と変わったときだけ）。
    Media {
        track: u32,
        config: Option<CodecConfig>,
        aus: Vec<AccessUnit>,
    },
    /// 統計など（JSON）。
    Event(serde_json::Value),
    /// 取り込みの誤り。
    Error(CaptureError),
    /// 出し切って閉じた（本体は統計）。
    Closed(serde_json::Value),
    /// プロセスが終わった（`Closed` の後、または異常終了）。
    Exited(Option<i32>),
}

async fn read_message<R: AsyncRead + Unpin>(
    r: &mut R,
) -> std::result::Result<Option<Message>, String> {
    let mut h = [0u8; protocol::HEADER_LEN];
    match r.read_exact(&mut h).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.to_string()),
    }
    let (len, ty) = protocol::parse_header(&h).map_err(|e| e.to_string())?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.map_err(|e| e.to_string())?;
    protocol::decode_body(ty, Bytes::from(body))
        .map(Some)
        .map_err(|e| e.to_string())
}

async fn write_message(w: &mut ChildStdin, m: &Message) -> Result<()> {
    let buf = protocol::encode_message(m).map_err(|e| CaptureError::internal(e.to_string()))?;
    w.write_all(&buf)
        .await
        .map_err(|e| CaptureError::device(format!("capture worker pipe: {e}")))?;
    w.flush()
        .await
        .map_err(|e| CaptureError::device(format!("capture worker pipe: {e}")))
}

/// 動いている capture worker（1 つのセッション）。
#[derive(Debug)]
pub struct CaptureWorker {
    child: Child,
    stdin: Option<ChildStdin>,
    session_id: u64,
    /// worker の最初の通知（サンドボックスの状態・符号化の能力）。
    pub hello: Hello,
    /// 開いたときの通知（トラック・取り決めた形式）。
    pub opened: serde_json::Value,
}

impl CaptureWorker {
    /// worker を起動してセッションを開く。開けなければ worker の誤り（`CAPACITY_UNAVAILABLE` など）を返す。
    pub async fn spawn(
        cmd: &WorkerCommand,
        session_id: u64,
        tenant: &str,
        policy: Policy,
        spec: &CaptureSpec,
        start_timeout: Duration,
    ) -> Result<(CaptureWorker, mpsc::Receiver<WorkerEvent>)> {
        spec.validate()?;
        let mut child = cmd.spawn("serve").map_err(spawn_err)?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| CaptureError::internal("no stdin"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| CaptureError::internal("no stdout"))?;
        let handshake = async {
            let hello = match read_message(&mut stdout).await {
                Ok(Some(Message::Hello(h))) if h.version == PROTOCOL_VERSION => h,
                Ok(Some(Message::Hello(h))) => {
                    return Err(CaptureError::internal(format!(
                        "capture worker speaks protocol {}, expected {PROTOCOL_VERSION}",
                        h.version
                    )));
                }
                other => {
                    return Err(CaptureError::internal(format!(
                        "capture worker did not say hello: {other:?}"
                    )));
                }
            };
            write_message(
                &mut stdin,
                &Message::SessionOpen(SessionOpen {
                    session_id,
                    tenant: tenant.to_string(),
                    policy,
                    graph: spec.to_json(),
                }),
            )
            .await?;
            loop {
                match read_message(&mut stdout).await {
                    Ok(Some(Message::SessionEvent(ev))) if ev.session_id == session_id => {
                        match ev.kind {
                            SessionEventKind::Opened => {
                                let v = serde_json::from_str(&ev.body).unwrap_or_default();
                                return Ok((hello, v));
                            }
                            SessionEventKind::Error => return Err(parse_error_body(&ev.body)),
                            _ => {}
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        return Err(CaptureError::device("capture worker exited during open"));
                    }
                    Err(e) => {
                        return Err(CaptureError::internal(format!(
                            "capture worker protocol: {e}"
                        )));
                    }
                }
            }
        };
        let (hello, opened) = match tokio::time::timeout(start_timeout, handshake).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                let _ = child.kill().await;
                return Err(e);
            }
            Err(_) => {
                let _ = child.kill().await;
                return Err(CaptureError::unavailable(
                    "capture worker did not open the device in time",
                ));
            }
        };
        let (tx, rx) = mpsc::channel(EVENT_QUEUE);
        tokio::spawn(pump(stdout, session_id, tx));
        Ok((
            CaptureWorker {
                child,
                stdin: Some(stdin),
                session_id,
                hello,
                opened,
            },
            rx,
        ))
    }

    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// 閉じるよう求める（worker は出し切ってから `Closed` を送り、終わる）。
    pub async fn close(&mut self) -> Result<()> {
        if let Some(stdin) = self.stdin.as_mut() {
            write_message(stdin, &Message::SessionClose(self.session_id)).await?;
        }
        Ok(())
    }

    /// 終わるのを待つ（`close` の後に。時間切れなら止める）。
    pub async fn wait(&mut self, timeout: Duration) -> Option<i32> {
        match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(Ok(s)) => s.code(),
            _ => {
                let _ = self.child.kill().await;
                None
            }
        }
    }

    /// すぐ止める。
    pub async fn kill(&mut self) {
        self.stdin = None;
        let _ = self.child.kill().await;
    }
}

async fn pump<R: AsyncRead + Unpin>(mut stdout: R, sid: u64, tx: mpsc::Sender<WorkerEvent>) {
    loop {
        let ev = match read_message(&mut stdout).await {
            Ok(Some(Message::SessionOutput(o))) if o.session_id == sid => match o.kind {
                SessionOutputKind::Media {
                    track, config, aus, ..
                } => WorkerEvent::Media { track, config, aus },
                SessionOutputKind::Snapshot { .. } => continue,
            },
            Ok(Some(Message::SessionEvent(e))) if e.session_id == sid => match e.kind {
                SessionEventKind::Event => {
                    WorkerEvent::Event(serde_json::from_str(&e.body).unwrap_or_default())
                }
                SessionEventKind::Error => WorkerEvent::Error(parse_error_body(&e.body)),
                SessionEventKind::Closed => {
                    WorkerEvent::Closed(serde_json::from_str(&e.body).unwrap_or_default())
                }
                _ => continue,
            },
            Ok(Some(_)) => continue,
            Ok(None) => {
                let _ = tx.send(WorkerEvent::Exited(None)).await;
                return;
            }
            Err(e) => {
                let _ = tx
                    .send(WorkerEvent::Error(CaptureError::new(
                        ErrorKind::Internal,
                        format!("capture worker protocol violation: {e}"),
                    )))
                    .await;
                let _ = tx.send(WorkerEvent::Exited(None)).await;
                return;
            }
        };
        if tx.send(ev).await.is_err() {
            return;
        }
    }
}
