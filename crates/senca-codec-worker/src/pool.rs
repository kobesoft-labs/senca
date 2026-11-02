//! 親（ノード）側の worker プール（REQ-010 R10-2、仕様 3.1・9.4、AC-17）。
//!
//! # 振る舞い
//!
//! - **数。** worker を `size` 個（既定はコア数）起動し、それぞれを「枠」として監督する。
//! - **使える worker だけに渡す。** 枠は「起動中 → 待機 → 処理中」と進み、Hello（版の一致を確認）を受け取った
//!   待機中の worker だけが待ち行列からジョブを取る。死んだ・起動中の worker にジョブを渡すことはない。
//! - **テナントの公平さ。** 待ち行列はテナントごと。空いた worker はテナントを順番に回して 1 つずつ取る（round robin）。
//!   1 つのテナントが大量に積んでも、他のテナントのジョブは自分の番が来れば処理される。
//! - **上限。** 待ち行列はテナントごと・全体で上限を持ち、溢れたら `Unavailable`（503）で断る。
//! - **時間切れ。** ジョブごとの時間（送信から結果まで）を超えたら worker を殺し、そのジョブは時間切れで失敗し、
//!   枠は新しい worker を起動する。
//! - **異常終了・不正な返事。** 入力の終わり・読み書きの失敗は「worker が落ちた」、壊れたフレーム・違う id・
//!   ジョブの種類と合わない出力・上限を超える長さは「プロトコル違反」として、worker を殺して作り直す。
//!   そのジョブは失敗にする（落とす入力で worker を順に倒されないよう、自動では再試行しない）。
//! - **起動できないとき。** 起動に失敗した枠は間隔を倍にしながら（上限まで）再試行する。生きている・起動中の
//!   worker が 1 つも無ければ、新しいジョブも待っているジョブも `Unavailable`（`CAPACITY_UNAVAILABLE`、503）で返す。
//! - **取り消し。** `submit` の future を捨てると、まだ待ち行列にあるジョブは取り出し時に捨てる（worker を使わない）。
//!   処理中なら最後まで処理し、結果は捨てる。どちらでも数え方（待ち数・枠の状態）は崩れない。
//! - **環境変数。** worker には既定で環境変数を引き継がない（秘密を渡さない）。`SENCA_CODEC_LIB_DIR`・
//!   `SENCA_CODEC_DISABLE`・`LD_LIBRARY_PATH` だけ、[`PoolConfig::new`] が親の値を写す。

use crate::protocol::{
    self, Hello, Job, JobKind, JobOutput, Message, PROTOCOL_VERSION, ProtocolError,
};
use bytes::Bytes;
use senca_codec::caps::{Capability, ProviderStatus};
use senca_codec::error::{CodecError, ErrorKind};
use senca_codec::registry::Policy;
use senca_core::Codec;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Notify, oneshot, watch};
use tokio::task::JoinHandle;

/// テナント名の長さの上限。
pub const MAX_TENANT_LEN: usize = 256;

/// プールの設定。
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// worker の数。
    pub size: usize,
    /// worker の実行ファイル。
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// worker に渡す環境変数（`env_clear` なら、これだけを渡す）。
    pub env: Vec<(OsString, OsString)>,
    pub env_clear: bool,
    /// ジョブ 1 つの時間（送信から結果まで）。
    pub job_timeout: Duration,
    /// 起動から Hello までの時間。
    pub hello_timeout: Duration,
    /// 起動に失敗したときの再試行の間隔（最初と上限）。
    pub restart_backoff_min: Duration,
    pub restart_backoff_max: Duration,
    pub max_queue_per_tenant: usize,
    pub max_queue_total: usize,
    /// worker の標準エラーを親に引き継ぐ（`false` なら捨てる）。
    pub inherit_stderr: bool,
}

impl PoolConfig {
    /// 既定値（数はコア数）。`SENCA_CODEC_LIB_DIR`・`SENCA_CODEC_DISABLE`・`LD_LIBRARY_PATH` を写す。
    pub fn new(program: impl Into<PathBuf>) -> Self {
        let size = std::thread::available_parallelism().map_or(1, |n| n.get());
        let env = [
            "SENCA_CODEC_LIB_DIR",
            "SENCA_CODEC_DISABLE",
            "LD_LIBRARY_PATH",
        ]
        .iter()
        .filter_map(|k| std::env::var_os(k).map(|v| (OsString::from(k), v)))
        .collect();
        PoolConfig {
            size,
            program: program.into(),
            args: Vec::new(),
            env,
            env_clear: true,
            job_timeout: Duration::from_secs(60),
            hello_timeout: Duration::from_secs(10),
            restart_backoff_min: Duration::from_millis(100),
            restart_backoff_max: Duration::from_secs(10),
            max_queue_per_tenant: 64,
            max_queue_total: 1024,
            inherit_stderr: true,
        }
    }
}

/// 失敗の原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureCause {
    /// worker の中のコーデックがエラーを返した（`kind` はその種類）。
    Codec,
    /// 待ち行列が一杯。
    QueueFull,
    /// 使える worker が無い（起動できない）。
    NoWorkers,
    /// 時間切れ（worker は作り直した）。
    Timeout,
    /// worker が異常終了した。
    WorkerCrashed,
    /// worker の返事がプロトコルに違反した。
    ProtocolViolation,
    /// プールを止めている。
    ShuttingDown,
    /// 要求が不正（テナント名、上限を超える大きさなど）。
    InvalidRequest,
}

/// プールのエラー。`kind` で制御 API のエラーコード（`kind.api_code()`）・HTTP 状態が決まる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolError {
    pub cause: FailureCause,
    pub kind: ErrorKind,
    pub message: String,
}

impl PoolError {
    fn new(cause: FailureCause, kind: ErrorKind, message: impl Into<String>) -> Self {
        PoolError {
            cause,
            kind,
            message: message.into(),
        }
    }
    fn unavailable(cause: FailureCause, message: impl Into<String>) -> Self {
        Self::new(cause, ErrorKind::Unavailable, message)
    }
    pub fn api_code(&self) -> &'static str {
        self.kind.api_code()
    }
    pub fn http_status(&self) -> u16 {
        self.kind.http_status()
    }
}

impl From<CodecError> for PoolError {
    fn from(e: CodecError) -> Self {
        PoolError::new(FailureCause::Codec, e.kind, e.message)
    }
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} ({}): {}",
            self.cause,
            self.kind.as_str(),
            self.message
        )
    }
}

impl std::error::Error for PoolError {}

/// 統計。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolStats {
    pub size: usize,
    /// Hello を受け取った worker（待機中 + 処理中）。
    pub alive: usize,
    pub busy: usize,
    pub starting: usize,
    pub down: usize,
    pub queued: usize,
    /// 生きていた worker を作り直した回数（異常終了・時間切れ・違反）。
    pub restarts: u64,
    pub start_failures: u64,
    pub completed: u64,
    pub failed: u64,
    pub timeouts: u64,
    pub crashes: u64,
    pub violations: u64,
}

/// worker の Hello の内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerInfo {
    pub capabilities: Vec<Capability>,
    pub statuses: Vec<ProviderStatus>,
    pub sandbox: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Starting,
    Ready,
    Busy,
    Down,
}

/// 結果の検査に使う、ジョブから期待する出力。
#[derive(Debug, Clone, Copy)]
enum Expect {
    Image { max_w: u32, max_h: u32 },
    Frames,
    Encoded { codec: Codec },
    SelfTest,
}

type Reply = Result<JobOutput, PoolError>;

struct Pending {
    id: u64,
    frame: Bytes,
    expect: Expect,
    tx: oneshot::Sender<Reply>,
}

#[derive(Default)]
struct State {
    queues: HashMap<String, VecDeque<Pending>>,
    rr: VecDeque<String>,
    queued: usize,
    slots: Vec<Option<SlotState>>,
    info: Option<WorkerInfo>,
    next_id: u64,
    stats: PoolStats,
}

impl State {
    fn count(&self, s: SlotState) -> usize {
        self.slots.iter().filter(|x| **x == Some(s)).count()
    }

    fn any_viable(&self) -> bool {
        self.slots.iter().any(|s| {
            matches!(
                s,
                Some(SlotState::Starting | SlotState::Ready | SlotState::Busy)
            )
        })
    }

    /// 公平に 1 つ取り出す（取り消されたジョブは捨てる）。
    fn pop_fair(&mut self) -> Option<(String, Pending)> {
        while let Some(t) = self.rr.pop_front() {
            let Some(q) = self.queues.get_mut(&t) else {
                continue;
            };
            let mut found = None;
            while let Some(p) = q.pop_front() {
                self.queued -= 1;
                if !p.tx.is_closed() {
                    found = Some(p);
                    break;
                }
            }
            if q.is_empty() {
                self.queues.remove(&t);
            } else {
                self.rr.push_back(t.clone());
            }
            if let Some(p) = found {
                return Some((t, p));
            }
        }
        None
    }

    fn drain_all(&mut self) -> Vec<Pending> {
        self.rr.clear();
        self.queued = 0;
        self.queues.drain().flat_map(|(_, q)| q).collect()
    }
}

struct Shared {
    cfg: PoolConfig,
    state: Mutex<State>,
    job_ready: Notify,
    shutdown: watch::Receiver<bool>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // 中の処理はパニックしないので、毒されていても状態は一貫している。
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_slot(&self, i: usize, s: SlotState) {
        let failed = {
            let mut st = self.lock();
            st.slots[i] = Some(s);
            if s == SlotState::Down && !st.any_viable() {
                st.drain_all()
            } else {
                Vec::new()
            }
        };
        for p in failed {
            let _ = p.tx.send(Err(PoolError::unavailable(
                FailureCause::NoWorkers,
                "no codec worker could be started",
            )));
        }
    }

    fn is_shutdown(&self) -> bool {
        *self.shutdown.borrow()
    }
}

/// worker のプール。
pub struct WorkerPool {
    shared: Arc<Shared>,
    shutdown_tx: watch::Sender<bool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl WorkerPool {
    /// 起動する。最初の起動の結果（Hello か失敗）がそろうまで待つ（最長 `hello_timeout` 程度）。
    /// worker が 1 つも起動できなくても `Ok` を返し、`submit` が `Unavailable` を返す（設定の誤りだけ `Err`）。
    pub async fn start(cfg: PoolConfig) -> Result<WorkerPool, PoolError> {
        if cfg.size == 0 || cfg.size > 1024 {
            return Err(PoolError::new(
                FailureCause::InvalidRequest,
                ErrorKind::InvalidConfig,
                "pool size must be 1..=1024",
            ));
        }
        let (shutdown_tx, shutdown) = watch::channel(false);
        let size = cfg.size;
        let wait = cfg.hello_timeout + Duration::from_millis(500);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                slots: vec![None; size],
                stats: PoolStats {
                    size,
                    ..Default::default()
                },
                ..Default::default()
            }),
            cfg,
            job_ready: Notify::new(),
            shutdown,
        });
        let tasks = (0..size)
            .map(|i| tokio::spawn(slot_loop(shared.clone(), i)))
            .collect();
        let pool = WorkerPool {
            shared,
            shutdown_tx,
            tasks: Mutex::new(tasks),
        };
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            {
                let st = pool.shared.lock();
                let settled = st
                    .slots
                    .iter()
                    .all(|s| matches!(s, Some(SlotState::Ready | SlotState::Down)));
                if st.count(SlotState::Ready) > 0 || settled {
                    break;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(pool)
    }

    /// ジョブを出して結果を待つ。
    pub async fn submit(
        &self,
        tenant: &str,
        policy: Policy,
        kind: JobKind,
    ) -> Result<JobOutput, PoolError> {
        if tenant.is_empty() || tenant.len() > MAX_TENANT_LEN {
            return Err(PoolError::new(
                FailureCause::InvalidRequest,
                ErrorKind::InvalidConfig,
                "bad tenant name",
            ));
        }
        let expect = match &kind {
            JobKind::DecodeToImage { decoder, .. } => Expect::Image {
                max_w: decoder.max_width,
                max_h: decoder.max_height,
            },
            JobKind::Decode { .. } => Expect::Frames,
            JobKind::Encode { encoder, .. } | JobKind::Transcode { encoder, .. } => {
                Expect::Encoded {
                    codec: encoder.codec(),
                }
            }
            JobKind::SelfTest => Expect::SelfTest,
        };
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.shared.lock();
            if self.shared.is_shutdown() {
                return Err(PoolError::unavailable(
                    FailureCause::ShuttingDown,
                    "worker pool is shutting down",
                ));
            }
            if !st.any_viable() {
                return Err(PoolError::unavailable(
                    FailureCause::NoWorkers,
                    "no codec worker could be started",
                ));
            }
            if st.queued >= self.shared.cfg.max_queue_total
                || st
                    .queues
                    .get(tenant)
                    .is_some_and(|q| q.len() >= self.shared.cfg.max_queue_per_tenant)
            {
                return Err(PoolError::unavailable(
                    FailureCause::QueueFull,
                    "codec job queue is full",
                ));
            }
            st.next_id += 1;
            let id = st.next_id;
            let frame =
                protocol::encode_message(&Message::Job(Job { id, policy, kind })).map_err(|e| {
                    PoolError::new(
                        FailureCause::InvalidRequest,
                        ErrorKind::InvalidInput,
                        e.to_string(),
                    )
                })?;
            let q = st.queues.entry(tenant.to_string()).or_default();
            let new_tenant = q.is_empty();
            q.push_back(Pending {
                id,
                frame: Bytes::from(frame),
                expect,
                tx,
            });
            if new_tenant {
                st.rr.push_back(tenant.to_string());
            }
            st.queued += 1;
        }
        self.shared.job_ready.notify_one();
        match rx.await {
            Ok(r) => r,
            Err(_) => Err(PoolError::unavailable(
                FailureCause::ShuttingDown,
                "worker pool stopped",
            )),
        }
    }

    /// worker の能力（最後に受け取った Hello から）。まだ無ければ空。
    pub fn capabilities(&self) -> Vec<Capability> {
        self.shared
            .lock()
            .info
            .as_ref()
            .map(|i| i.capabilities.clone())
            .unwrap_or_default()
    }

    /// 最後に受け取った Hello の内容。
    pub fn worker_info(&self) -> Option<WorkerInfo> {
        self.shared.lock().info.clone()
    }

    pub fn stats(&self) -> PoolStats {
        let st = self.shared.lock();
        let mut s = st.stats.clone();
        s.ready_counts(&st);
        s
    }

    /// 止める。待っているジョブは `Unavailable` で返し、worker に終了を求め（応じなければ殺す）、監督の task を待つ。
    pub async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        self.shared.job_ready.notify_waiters();
        let pending = self.shared.lock().drain_all();
        for p in pending {
            let _ = p.tx.send(Err(PoolError::unavailable(
                FailureCause::ShuttingDown,
                "worker pool is shutting down",
            )));
        }
        let tasks: Vec<_> =
            std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|e| e.into_inner()));
        for t in tasks {
            let _ = t.await;
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        // 監督の task を止める（子プロセスは kill_on_drop で殺される）。
        let _ = self.shutdown_tx.send(true);
        self.shared.job_ready.notify_waiters();
    }
}

impl PoolStats {
    fn ready_counts(&mut self, st: &State) {
        let ready = st.count(SlotState::Ready);
        self.busy = st.count(SlotState::Busy);
        self.alive = ready + self.busy;
        self.starting = st.count(SlotState::Starting);
        self.down = st.count(SlotState::Down);
        self.queued = st.queued;
    }
}

// ---------------------------------------------------------------------------
// 1 つの枠の監督

async fn slot_loop(shared: Arc<Shared>, i: usize) {
    let mut shutdown = shared.shutdown.clone();
    let mut fails: u32 = 0;
    while !shared.is_shutdown() {
        shared.set_slot(i, SlotState::Starting);
        let mut w = match Worker::start(&shared.cfg).await {
            Ok((w, info)) => {
                fails = 0;
                let mut st = shared.lock();
                st.info = Some(info);
                st.slots[i] = Some(SlotState::Ready);
                drop(st);
                w
            }
            Err(e) => {
                fails = fails.saturating_add(1);
                eprintln!("senca codec pool: worker slot {i} failed to start: {e}");
                shared.lock().stats.start_failures += 1;
                shared.set_slot(i, SlotState::Down);
                let mult = 1u32 << (fails - 1).min(16);
                let backoff =
                    (shared.cfg.restart_backoff_min * mult).min(shared.cfg.restart_backoff_max);
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown.wait_for(|v| *v) => {}
                }
                continue;
            }
        };
        // 待機と処理
        loop {
            let Some(job) = next_job(&shared, &mut shutdown).await else {
                w.stop().await;
                shared.set_slot(i, SlotState::Down);
                return;
            };
            shared.set_slot(i, SlotState::Busy);
            let outcome = tokio::select! {
                r = tokio::time::timeout(shared.cfg.job_timeout, w.run(&job)) => match r {
                    Ok(r) => r,
                    Err(_) => Err(Fatal::Timeout),
                },
                _ = shutdown.wait_for(|v| *v) => Err(Fatal::Shutdown),
            };
            match outcome {
                Ok(reply) => {
                    {
                        let mut st = shared.lock();
                        st.slots[i] = Some(SlotState::Ready);
                        if reply.is_ok() {
                            st.stats.completed += 1;
                        } else {
                            st.stats.failed += 1;
                        }
                    }
                    let _ = job.tx.send(reply);
                }
                Err(f) => {
                    w.kill().await;
                    let err = {
                        let mut st = shared.lock();
                        st.stats.failed += 1;
                        st.stats.restarts += 1;
                        // 次のジョブが「使える worker が無い」と断られないよう、結果を返す前に起動中にする。
                        st.slots[i] = Some(SlotState::Starting);
                        match &f {
                            Fatal::Timeout => {
                                st.stats.timeouts += 1;
                                PoolError::new(
                                    FailureCause::Timeout,
                                    ErrorKind::Unavailable,
                                    "codec job timed out; worker restarted",
                                )
                            }
                            Fatal::Crashed(m) => {
                                st.stats.crashes += 1;
                                PoolError::new(
                                    FailureCause::WorkerCrashed,
                                    ErrorKind::Internal,
                                    format!("codec worker crashed: {m}"),
                                )
                            }
                            Fatal::Violation(m) => {
                                st.stats.violations += 1;
                                PoolError::new(
                                    FailureCause::ProtocolViolation,
                                    ErrorKind::Internal,
                                    format!("codec worker sent an invalid reply: {m}"),
                                )
                            }
                            Fatal::Shutdown => PoolError::unavailable(
                                FailureCause::ShuttingDown,
                                "worker pool is shutting down",
                            ),
                        }
                    };
                    let _ = job.tx.send(Err(err));
                    break;
                }
            }
        }
    }
    shared.set_slot(i, SlotState::Down);
}

/// 次のジョブを待つ。止めるときは `None`。
async fn next_job(shared: &Shared, shutdown: &mut watch::Receiver<bool>) -> Option<Pending> {
    loop {
        let notified = shared.job_ready.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if shared.is_shutdown() {
            return None;
        }
        let picked = shared.lock().pop_fair();
        if let Some((_, p)) = picked {
            // まだ待っているジョブがあれば、ほかの空いた worker も起こす。
            if shared.lock().queued > 0 {
                shared.job_ready.notify_one();
            }
            return Some(p);
        }
        tokio::select! {
            _ = &mut notified => {}
            _ = shutdown.wait_for(|v| *v) => return None,
        }
    }
}

// ---------------------------------------------------------------------------
// 子プロセス

enum Fatal {
    Timeout,
    Crashed(String),
    Violation(String),
    Shutdown,
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

enum ReadError {
    Eof(String),
    Violation(ProtocolError),
}

impl Worker {
    async fn start(cfg: &PoolConfig) -> Result<(Worker, WorkerInfo), String> {
        let mut cmd = Command::new(&cfg.program);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if cfg.inherit_stderr {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .kill_on_drop(true);
        if cfg.env_clear {
            cmd.env_clear();
        }
        cmd.envs(cfg.env.iter().map(|(k, v)| (k, v)));
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn {}: {e}", cfg.program.display()))?;
        let stdin = child.stdin.take().ok_or("no stdin pipe")?;
        let stdout =
            BufReader::with_capacity(1 << 16, child.stdout.take().ok_or("no stdout pipe")?);
        let mut w = Worker {
            child,
            stdin,
            stdout,
        };
        let hello = match tokio::time::timeout(cfg.hello_timeout, w.read()).await {
            Err(_) => {
                w.kill().await;
                return Err("timed out waiting for hello".into());
            }
            Ok(Err(ReadError::Eof(m))) => {
                let status = w
                    .child
                    .wait()
                    .await
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                return Err(format!("worker exited before hello ({m}; {status})"));
            }
            Ok(Err(ReadError::Violation(e))) => {
                w.kill().await;
                return Err(format!("bad hello: {e}"));
            }
            Ok(Ok(m)) => m,
        };
        let Message::Hello(Hello {
            version,
            capabilities,
            statuses,
            sandbox,
            ..
        }) = hello
        else {
            w.kill().await;
            return Err("first message was not hello".into());
        };
        if version != PROTOCOL_VERSION {
            w.kill().await;
            return Err(format!(
                "protocol version mismatch (worker {version}, parent {PROTOCOL_VERSION})"
            ));
        }
        Ok((
            w,
            WorkerInfo {
                capabilities,
                statuses,
                sandbox,
            },
        ))
    }

    async fn read(&mut self) -> Result<Message, ReadError> {
        let mut h = [0u8; protocol::HEADER_LEN];
        self.stdout
            .read_exact(&mut h)
            .await
            .map_err(|e| ReadError::Eof(e.to_string()))?;
        let (len, ty) = protocol::parse_header(&h).map_err(ReadError::Violation)?;
        let mut body = vec![0u8; len];
        self.stdout
            .read_exact(&mut body)
            .await
            .map_err(|e| ReadError::Eof(e.to_string()))?;
        protocol::decode_body(ty, Bytes::from(body)).map_err(ReadError::Violation)
    }

    async fn run(&mut self, job: &Pending) -> Result<Reply, Fatal> {
        let io = |e: std::io::Error| Fatal::Crashed(format!("write: {e}"));
        self.stdin.write_all(&job.frame).await.map_err(io)?;
        self.stdin.flush().await.map_err(io)?;
        let m = self.read().await.map_err(|e| match e {
            ReadError::Eof(m) => Fatal::Crashed(m),
            ReadError::Violation(e) => Fatal::Violation(e.to_string()),
        })?;
        let Message::JobResult(r) = m else {
            return Err(Fatal::Violation(
                "unexpected message instead of job result".into(),
            ));
        };
        if r.id != job.id {
            return Err(Fatal::Violation(format!(
                "result for job {} while waiting for {}",
                r.id, job.id
            )));
        }
        match r.result {
            Err(e) => Ok(Err(e.into())),
            Ok(out) => {
                check_output(&out, job.expect).map_err(Fatal::Violation)?;
                Ok(Ok(out))
            }
        }
    }

    async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
    }

    async fn stop(&mut self) {
        if let Ok(f) = protocol::encode_message(&Message::Shutdown) {
            let _ = self.stdin.write_all(&f).await;
            let _ = self.stdin.flush().await;
        }
        if tokio::time::timeout(Duration::from_secs(2), self.child.wait())
            .await
            .is_err()
        {
            self.kill().await;
        }
    }
}

/// 出力がジョブの種類と合っているか。
fn check_output(out: &JobOutput, expect: Expect) -> Result<(), String> {
    match (out, expect) {
        (JobOutput::Image(f), Expect::Image { max_w, max_h }) => {
            if f.width > max_w || f.height > max_h {
                return Err("image larger than requested maximum".into());
            }
            Ok(())
        }
        (JobOutput::Frames(_), Expect::Frames) | (JobOutput::SelfTest(_), Expect::SelfTest) => {
            Ok(())
        }
        (JobOutput::Encoded { config, aus }, Expect::Encoded { codec }) => {
            if config.as_ref().is_some_and(|c| c.codec != codec)
                || aus.iter().any(|a| a.codec != codec)
            {
                return Err("encoded output has the wrong codec".into());
            }
            Ok(())
        }
        _ => Err("output kind does not match the job".into()),
    }
}
