//! 親（ノード）側のセッション（パイプラインを worker の中で動かし続ける、REQ-005・REQ-008・REQ-010）。
//!
//! # 使い方
//!
//! ```text
//! let pool = SessionPool::new(SessionPoolConfig::new(worker_program));
//! let mut s = pool.open_session("tenant", policy, &graph).await?;   // 検証 → worker を起動 → 開く
//! let mut outs = s.outputs().unwrap();                              // 符号化済みの AU（sink ごと）・静止画
//! let mut evs = s.events().unwrap();                                // 状態・メーター（約 10 Hz）・誤り・異常終了
//! s.send_input("cam1", Some(config), aus)?;                         // 待たない
//! s.control(&Control::SwitchProgram { .. }).await?;
//! s.close().await?;
//! ```
//!
//! # 振る舞い
//!
//! - **1 セッション = 1 プロセス。** worker を新しく起動し、Hello（版の確認）の後でセッションを開く。
//!   落ちても他のセッションに波及しない。サンドボックスはジョブの worker と同じ。
//! - **事前の検証。** Hello の能力でグラフを検証してから開く（H.264 のハードウェアが無い等は `CAPACITY_UNAVAILABLE`）。
//!   worker の中でも同じ検証をする（そちらが正）。
//! - **入力は待たない。** `send_input` は有界のキュー（AU の数・バイト数）に入れるだけ。worker が遅れて溢れたら、
//!   古い AU から捨てる。映像は捨てた後、その入力の次のキーフレームまで捨て続ける（壊れた GOP を送らない）。
//!   捨てた数は [`SessionEventItem::InputDropped`] で知らせる。設定（`CodecConfig`）と入力の終わりの印は捨てない。
//! - **操作は入力より先に送る**（別のキュー）。返事は要求番号で突き合わせ、時間切れがある。
//! - **異常終了の検出。** worker の出力が途切れた（`Closed` の前の EOF）・プロトコル違反なら
//!   [`SessionEventItem::Crashed`] を出し、待っている操作を失敗にする。作り直すのはノード（新しく `open_session`）。
//! - **テナントごとの上限**と全体の上限。ハンドルを捨てる（または閉じる）と枠を返す。

use crate::pool::{PoolConfig, ReadError, Worker, WorkerInfo, read_frame};
use crate::protocol::{
    self, MAX_SESSION_AUS, MAX_SESSION_JSON, Message, SessionControl, SessionEventKind,
    SessionInput, SessionOpen, SessionOutputKind,
};
use bytes::Bytes;
use senca_codec::registry::Policy;
use senca_core::{AccessUnit, CodecConfig, MediaKind};
use senca_pipeline::control::{Control, ControlReply};
use senca_pipeline::error::Issue;
use senca_pipeline::validate::{Plan, ValidationContext};
use senca_pipeline::{Event, Graph, PipelineError};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 設定。
#[derive(Debug, Clone)]
pub struct SessionPoolConfig {
    /// worker の起動（実行ファイル・引数・環境変数・Hello の待ち時間）。`size` などジョブ用の値は使わない。
    pub worker: PoolConfig,
    pub max_sessions_total: usize,
    pub max_sessions_per_tenant: usize,
    /// 開くまでの時間（worker の中でコーデックを開いて確かめる）。
    pub open_timeout: Duration,
    /// 操作の返事の時間（`commit` は切り替わるまで待つ）。
    pub control_timeout: Duration,
    /// 閉じる（出し切る）時間。
    pub close_timeout: Duration,
    /// 入力のキューの上限（AU の数とバイト数。仕様 5.3 の 1 セッション 32 MiB）。
    pub input_queue_aus: usize,
    pub input_queue_bytes: usize,
    /// 出力・通知のチャネルの長さ。出力が一杯なら worker の出力を読むのを待つ（背圧）。通知は一杯なら捨てる。
    pub output_capacity: usize,
    pub event_capacity: usize,
    /// 開く前に Hello の能力で検証する。
    pub prevalidate: bool,
}

impl SessionPoolConfig {
    pub fn new(program: impl Into<std::path::PathBuf>) -> Self {
        SessionPoolConfig {
            worker: PoolConfig::new(program),
            max_sessions_total: 64,
            max_sessions_per_tenant: 8,
            open_timeout: Duration::from_secs(30),
            control_timeout: Duration::from_secs(60),
            close_timeout: Duration::from_secs(30),
            input_queue_aus: 2048,
            input_queue_bytes: 32 * 1024 * 1024,
            output_capacity: 1024,
            event_capacity: 1024,
            prevalidate: true,
        }
    }
}

/// 失敗の原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionFailure {
    /// 検証・方針・能力で拒否（`issues` に理由）。
    Rejected,
    /// セッション数の上限。
    LimitReached,
    /// worker を起動できない。
    WorkerStart,
    /// worker が異常終了した。
    Crashed,
    /// worker の返事がプロトコルに違反した。
    ProtocolViolation,
    /// 時間切れ。
    Timeout,
    /// 閉じている。
    Closed,
    /// 操作が失敗した（worker の中の誤り、`code` に理由）。
    Control,
    /// 要求が不正（テナント名・大きすぎる操作など）。
    InvalidRequest,
}

/// セッションの誤り。`code` は制御 API のエラーコード（仕様 10.4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionError {
    pub cause: SessionFailure,
    pub code: String,
    pub message: String,
    pub issues: Vec<Issue>,
}

impl SessionError {
    fn new(cause: SessionFailure, code: &str, message: impl Into<String>) -> Self {
        SessionError {
            cause,
            code: code.to_string(),
            message: message.into(),
            issues: Vec::new(),
        }
    }

    fn from_pipeline(cause: SessionFailure, e: &PipelineError) -> Self {
        SessionError {
            cause,
            code: e.api_code().to_string(),
            message: e.to_string(),
            issues: e.issues().to_vec(),
        }
    }

    /// worker の誤りの JSON（`{"code","message","issues"}`）から。
    fn from_body(cause: SessionFailure, body: &str) -> Self {
        let v: serde_json::Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(_) => {
                return SessionError::new(cause, "INTERNAL_ERROR", "malformed error from worker");
            }
        };
        let (Some(code), Some(message)) = (v["code"].as_str(), v["message"].as_str()) else {
            return SessionError::new(cause, "INTERNAL_ERROR", "malformed error from worker");
        };
        let issues = serde_json::from_value::<Vec<Issue>>(v["issues"].clone()).unwrap_or_default();
        SessionError {
            cause,
            code: code.to_string(),
            message: message.to_string(),
            issues,
        }
    }

    pub fn http_status(&self) -> u16 {
        match self.code.as_str() {
            "CAPACITY_UNAVAILABLE" => 503,
            "CONFLICT" => 409,
            "INTERNAL_ERROR" => 500,
            _ => 422,
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} ({}): {}", self.cause, self.code, self.message)
    }
}

impl std::error::Error for SessionError {}

/// セッションの出力。
#[derive(Debug, Clone)]
pub enum SessionOutputItem {
    /// 符号化済みの AU（sink の 1 トラック）。`config` は最初・変わったとき・世代が切り替わったとき。
    Media {
        sink: String,
        stream: String,
        track: u32,
        generation: u64,
        config: Option<CodecConfig>,
        aus: Vec<AccessUnit>,
    },
    /// 板書の変化の静止画（JPEG、R6-8）。
    Snapshot {
        node: String,
        generation: u64,
        pts_us: i64,
        width: u32,
        height: u32,
        changed_fraction: f64,
        jpeg: Bytes,
    },
}

/// セッションの通知。
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEventItem {
    /// パイプラインの通知（状態・メーター・世代・誤り）。
    Pipeline(Event),
    /// 親の入力のキューで捨てた（worker が遅れた）。
    InputDropped { source: String, count: u64 },
    /// worker が異常終了した（ノードが作り直す）。
    Crashed { reason: String },
    /// 閉じた。
    Closed,
}

/// 統計。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionPoolStats {
    pub active: usize,
    pub per_tenant: BTreeMap<String, usize>,
    pub opened: u64,
    pub rejected: u64,
    pub crashed: u64,
}

#[derive(Default)]
struct PoolState {
    per_tenant: HashMap<String, usize>,
    total: usize,
    next_id: u64,
    info: Option<WorkerInfo>,
    stats: SessionPoolStats,
}

/// セッションの管理。
pub struct SessionPool {
    cfg: SessionPoolConfig,
    state: Arc<Mutex<PoolState>>,
}

/// 枠（捨てると返す）。
struct SlotGuard {
    state: Arc<Mutex<PoolState>>,
    tenant: String,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut st = lock(&self.state);
        st.total = st.total.saturating_sub(1);
        if let Some(n) = st.per_tenant.get_mut(&self.tenant) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                st.per_tenant.remove(&self.tenant);
            }
        }
    }
}

impl SessionPool {
    pub fn new(cfg: SessionPoolConfig) -> Self {
        SessionPool {
            cfg,
            state: Arc::new(Mutex::new(PoolState::default())),
        }
    }

    /// 最後に受け取った worker の Hello（能力。事前の検証に使える）。
    pub fn worker_info(&self) -> Option<WorkerInfo> {
        lock(&self.state).info.clone()
    }

    pub fn stats(&self) -> SessionPoolStats {
        let st = lock(&self.state);
        let mut s = st.stats.clone();
        s.active = st.total;
        s.per_tenant = st.per_tenant.iter().map(|(k, v)| (k.clone(), *v)).collect();
        s
    }

    /// セッションを開く。
    pub async fn open_session(
        &self,
        tenant: &str,
        policy: Policy,
        graph: &Graph,
    ) -> Result<SessionHandle, SessionError> {
        if tenant.is_empty() || tenant.len() > protocol::MAX_ID {
            return Err(SessionError::new(
                SessionFailure::InvalidRequest,
                "INVALID_CONFIG",
                "bad tenant name",
            ));
        }
        let graph_json = graph.to_json();
        if graph_json.len() > MAX_SESSION_JSON {
            return Err(SessionError::new(
                SessionFailure::InvalidRequest,
                "INVALID_CONFIG",
                "graph too large",
            ));
        }
        // 枠
        let (sid, slot) = {
            let mut st = lock(&self.state);
            let n = st.per_tenant.get(tenant).copied().unwrap_or(0);
            if st.total >= self.cfg.max_sessions_total || n >= self.cfg.max_sessions_per_tenant {
                st.stats.rejected += 1;
                return Err(SessionError::new(
                    SessionFailure::LimitReached,
                    "CAPACITY_UNAVAILABLE",
                    format!("session limit reached for tenant '{tenant}'"),
                ));
            }
            st.total += 1;
            *st.per_tenant.entry(tenant.to_string()).or_default() += 1;
            st.next_id += 1;
            (
                st.next_id,
                SlotGuard {
                    state: self.state.clone(),
                    tenant: tenant.to_string(),
                },
            )
        };
        let (mut w, info) = Worker::start(&self.cfg.worker).await.map_err(|e| {
            SessionError::new(SessionFailure::WorkerStart, "CAPACITY_UNAVAILABLE", e)
        })?;
        lock(&self.state).info = Some(info.clone());
        if self.cfg.prevalidate {
            let ctx = ValidationContext::new(policy.clone(), info.capabilities.clone());
            if let Err(e) = senca_pipeline::validate(graph, &ctx) {
                w.kill().await;
                lock(&self.state).stats.rejected += 1;
                return Err(SessionError::from_pipeline(SessionFailure::Rejected, &e));
            }
        }
        let open = protocol::encode_message(&Message::SessionOpen(SessionOpen {
            session_id: sid,
            tenant: tenant.to_string(),
            policy,
            graph: graph_json,
        }))
        .map_err(|e| {
            SessionError::new(
                SessionFailure::InvalidRequest,
                "INVALID_CONFIG",
                e.to_string(),
            )
        })?;
        let opened = async {
            w.stdin.write_all(&open).await.map_err(|e| {
                SessionError::new(SessionFailure::Crashed, "INTERNAL_ERROR", e.to_string())
            })?;
            w.stdin.flush().await.map_err(|e| {
                SessionError::new(SessionFailure::Crashed, "INTERNAL_ERROR", e.to_string())
            })?;
            loop {
                match w.read().await {
                    Ok(Message::SessionEvent(ev)) if ev.session_id == sid => match ev.kind {
                        SessionEventKind::Opened => {
                            return serde_json::from_str::<Plan>(&ev.body).map_err(|e| {
                                SessionError::new(
                                    SessionFailure::ProtocolViolation,
                                    "INTERNAL_ERROR",
                                    format!("bad plan from worker: {e}"),
                                )
                            });
                        }
                        SessionEventKind::Error => {
                            return Err(SessionError::from_body(
                                SessionFailure::Rejected,
                                &ev.body,
                            ));
                        }
                        _ => {}
                    },
                    Ok(Message::Pong(_)) => {}
                    Ok(other) => {
                        return Err(SessionError::new(
                            SessionFailure::ProtocolViolation,
                            "INTERNAL_ERROR",
                            format!("unexpected message while opening: {}", msg_name(&other)),
                        ));
                    }
                    Err(ReadError::Eof(m)) => {
                        return Err(SessionError::new(
                            SessionFailure::Crashed,
                            "INTERNAL_ERROR",
                            format!("worker exited while opening: {m}"),
                        ));
                    }
                    Err(ReadError::Violation(e)) => {
                        return Err(SessionError::new(
                            SessionFailure::ProtocolViolation,
                            "INTERNAL_ERROR",
                            e.to_string(),
                        ));
                    }
                }
            }
        };
        let plan = match tokio::time::timeout(self.cfg.open_timeout, opened).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                w.kill().await;
                let mut st = lock(&self.state);
                if e.cause == SessionFailure::Rejected {
                    st.stats.rejected += 1;
                }
                return Err(e);
            }
            Err(_) => {
                w.kill().await;
                return Err(SessionError::new(
                    SessionFailure::Timeout,
                    "CAPACITY_UNAVAILABLE",
                    "timed out opening the session",
                ));
            }
        };
        lock(&self.state).stats.opened += 1;
        Ok(SessionHandle::spawn(
            sid,
            tenant,
            plan,
            graph,
            w,
            &self.cfg,
            slot,
            self.state.clone(),
        ))
    }
}

fn msg_name(m: &Message) -> &'static str {
    match m {
        Message::Hello(_) => "hello",
        Message::Job(_) => "job",
        Message::JobResult(_) => "job-result",
        Message::Shutdown => "shutdown",
        Message::Ping(_) => "ping",
        Message::Pong(_) => "pong",
        Message::SessionOpen(_) => "session-open",
        Message::SessionInput(_) => "session-input",
        Message::SessionControl(_) => "session-control",
        Message::SessionOutput(_) => "session-output",
        Message::SessionEvent(_) => "session-event",
        Message::SessionClose(_) => "session-close",
    }
}

/// 入力のキューの 1 つ。
struct QItem {
    source: String,
    config: Option<CodecConfig>,
    aus: Vec<AccessUnit>,
    end: bool,
}

#[derive(Default)]
struct InputQueue {
    q: VecDeque<QItem>,
    aus: usize,
    bytes: usize,
    /// 次の映像のキーフレームまで捨てる入力。
    await_key: BTreeSet<String>,
    closed: bool,
}

type ControlWaiter = oneshot::Sender<Result<ControlReply, SessionError>>;

struct HandleShared {
    sid: u64,
    input: Mutex<InputQueue>,
    input_ready: Notify,
    controls: Mutex<HashMap<u64, ControlWaiter>>,
    ctl_tx: mpsc::UnboundedSender<Vec<u8>>,
    next_req: AtomicU64,
    alive: AtomicBool,
    closing: AtomicBool,
    close_notify: Notify,
    events: mpsc::Sender<SessionEventItem>,
    dropped_events: AtomicU64,
    max_aus: usize,
    max_bytes: usize,
}

impl HandleShared {
    fn event(&self, e: SessionEventItem) {
        if self.events.try_send(e).is_err() {
            self.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn fail_controls(&self, err: &SessionError) {
        let waiters: Vec<_> = lock(&self.controls).drain().map(|(_, w)| w).collect();
        for w in waiters {
            let _ = w.send(Err(err.clone()));
        }
    }
}

/// 入力の受け付けの結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputAck {
    pub queued: usize,
    /// 捨てた AU（この呼び出しで入れたもの・溢れで捨てた古いものを含む）。
    pub dropped: u64,
}

/// 開いたセッション。
pub struct SessionHandle {
    id: u64,
    tenant: String,
    plan: Plan,
    shared: Arc<HandleShared>,
    outputs: Option<mpsc::Receiver<SessionOutputItem>>,
    events: Option<mpsc::Receiver<SessionEventItem>>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    close_timeout: Duration,
    control_timeout: Duration,
    _slot: SlotGuard,
}

impl SessionHandle {
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        sid: u64,
        tenant: &str,
        plan: Plan,
        graph: &Graph,
        w: Worker,
        cfg: &SessionPoolConfig,
        slot: SlotGuard,
        pool: Arc<Mutex<PoolState>>,
    ) -> SessionHandle {
        let (out_tx, out_rx) = mpsc::channel(cfg.output_capacity.max(1));
        let (ev_tx, ev_rx) = mpsc::channel(cfg.event_capacity.max(1));
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(HandleShared {
            sid,
            input: Mutex::new(InputQueue::default()),
            input_ready: Notify::new(),
            controls: Mutex::new(HashMap::new()),
            ctl_tx,
            next_req: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            closing: AtomicBool::new(false),
            close_notify: Notify::new(),
            events: ev_tx,
            dropped_events: AtomicU64::new(0),
            max_aus: cfg.input_queue_aus.max(1),
            max_bytes: cfg.input_queue_bytes.max(1),
        });
        let Worker {
            child,
            stdin,
            stdout,
        } = w;
        // 出力を検査するための sink の一覧（世代が変わっても同じ id を使う前提。新しい sink は prepare の計画から足す）。
        let sinks: BTreeSet<String> = graph
            .nodes
            .iter()
            .filter_map(|n| match n {
                senca_pipeline::model::Node::Sink(s) => Some(s.id.clone()),
                senca_pipeline::model::Node::SnapshotOnChange(s) => Some(s.id.clone()),
                _ => None,
            })
            .collect();
        let reader = tokio::spawn(reader_task(
            shared.clone(),
            child,
            stdout,
            out_tx,
            sinks,
            pool,
        ));
        let writer = tokio::spawn(writer_task(shared.clone(), stdin, ctl_rx));
        SessionHandle {
            id: sid,
            tenant: tenant.to_string(),
            plan,
            shared,
            outputs: Some(out_rx),
            events: Some(ev_rx),
            reader: Some(reader),
            writer: Some(writer),
            close_timeout: cfg.close_timeout,
            control_timeout: cfg.control_timeout,
            _slot: slot,
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// worker が返した最初の世代の計画。
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// worker が動いている（異常終了・閉じた後は `false`）。
    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }

    /// 出力の受け口（1 回だけ取り出せる）。
    pub fn outputs(&mut self) -> Option<mpsc::Receiver<SessionOutputItem>> {
        self.outputs.take()
    }

    /// 通知の受け口（1 回だけ取り出せる）。
    pub fn events(&mut self) -> Option<mpsc::Receiver<SessionEventItem>> {
        self.events.take()
    }

    /// 入力の AU を入れる（待たない）。`config` はトラックの設定（最初と、変わったとき）。
    pub fn send_input(
        &self,
        source: &str,
        config: Option<CodecConfig>,
        aus: Vec<AccessUnit>,
    ) -> Result<InputAck, SessionError> {
        self.enqueue(source, config, aus, false)
    }

    /// 入力の終わり。
    pub fn end_input(&self, source: &str) -> Result<(), SessionError> {
        self.enqueue(source, None, Vec::new(), true).map(|_| ())
    }

    fn enqueue(
        &self,
        source: &str,
        config: Option<CodecConfig>,
        aus: Vec<AccessUnit>,
        end: bool,
    ) -> Result<InputAck, SessionError> {
        if !self.is_alive() || self.shared.closing.load(Ordering::SeqCst) {
            return Err(SessionError::new(
                SessionFailure::Closed,
                "CAPACITY_UNAVAILABLE",
                "session is not running",
            ));
        }
        if source.is_empty() || source.len() > protocol::MAX_ID {
            return Err(SessionError::new(
                SessionFailure::InvalidRequest,
                "INVALID_CONFIG",
                "bad source id",
            ));
        }
        let mut dropped: HashMap<String, u64> = HashMap::new();
        let queued;
        {
            let mut q = lock(&self.shared.input);
            if q.closed {
                return Err(SessionError::new(
                    SessionFailure::Closed,
                    "CAPACITY_UNAVAILABLE",
                    "session is closing",
                ));
            }
            // キーフレーム待ちの入力の映像は捨てる
            let mut keep = Vec::with_capacity(aus.len());
            for a in aus {
                if a.kind() == MediaKind::Video && q.await_key.contains(source) {
                    if a.keyframe {
                        q.await_key.remove(source);
                    } else {
                        *dropped.entry(source.to_string()).or_default() += 1;
                        continue;
                    }
                }
                keep.push(a);
            }
            q.aus += keep.len();
            q.bytes += keep.iter().map(|a| a.payload.len()).sum::<usize>();
            q.q.push_back(QItem {
                source: source.to_string(),
                config,
                aus: keep,
                end,
            });
            // 溢れ: 古いものから AU を捨てる（設定と終わりの印は残す）
            if q.aus > self.shared.max_aus || q.bytes > self.shared.max_bytes {
                let (max_aus, max_bytes) = (self.shared.max_aus, self.shared.max_bytes);
                let InputQueue {
                    q: items,
                    aus: n_aus,
                    bytes,
                    await_key,
                    ..
                } = &mut *q;
                for it in items.iter_mut() {
                    if *n_aus <= max_aus && *bytes <= max_bytes {
                        break;
                    }
                    for a in it.aus.drain(..) {
                        *n_aus -= 1;
                        *bytes -= a.payload.len();
                        *dropped.entry(it.source.clone()).or_default() += 1;
                        if a.kind() == MediaKind::Video {
                            await_key.insert(it.source.clone());
                        }
                    }
                }
                // 残っている映像も、キーフレームまで捨てる（壊れた GOP を送らない）
                for it in items.iter_mut() {
                    if !await_key.contains(&it.source) {
                        continue;
                    }
                    let mut kept = Vec::with_capacity(it.aus.len());
                    for a in it.aus.drain(..) {
                        if a.kind() == MediaKind::Video && await_key.contains(&it.source) {
                            if a.keyframe {
                                await_key.remove(&it.source);
                            } else {
                                *n_aus -= 1;
                                *bytes -= a.payload.len();
                                *dropped.entry(it.source.clone()).or_default() += 1;
                                continue;
                            }
                        }
                        kept.push(a);
                    }
                    it.aus = kept;
                }
                items.retain(|it| !it.aus.is_empty() || it.config.is_some() || it.end);
            }
            queued = q.aus;
        }
        self.shared.input_ready.notify_one();
        let total: u64 = dropped.values().sum();
        for (source, count) in dropped {
            self.shared
                .event(SessionEventItem::InputDropped { source, count });
        }
        Ok(InputAck {
            queued,
            dropped: total,
        })
    }

    /// 操作を送って返事を待つ（`commit` は切り替わるか失敗するまで）。
    pub async fn control(&self, c: &Control) -> Result<ControlReply, SessionError> {
        if !self.is_alive() {
            return Err(SessionError::new(
                SessionFailure::Closed,
                "CAPACITY_UNAVAILABLE",
                "session is not running",
            ));
        }
        let rid = self.shared.next_req.fetch_add(1, Ordering::Relaxed);
        let frame = protocol::encode_message(&Message::SessionControl(SessionControl {
            session_id: self.shared.sid,
            request_id: rid,
            control: c.to_json(),
        }))
        .map_err(|e| {
            SessionError::new(
                SessionFailure::InvalidRequest,
                "INVALID_CONFIG",
                e.to_string(),
            )
        })?;
        let (tx, rx) = oneshot::channel();
        lock(&self.shared.controls).insert(rid, tx);
        if self.shared.ctl_tx.send(frame).is_err() {
            lock(&self.shared.controls).remove(&rid);
            return Err(SessionError::new(
                SessionFailure::Closed,
                "CAPACITY_UNAVAILABLE",
                "session is not running",
            ));
        }
        match tokio::time::timeout(self.control_timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(SessionError::new(
                SessionFailure::Crashed,
                "INTERNAL_ERROR",
                "session ended before the control reply",
            )),
            Err(_) => {
                lock(&self.shared.controls).remove(&rid);
                Err(SessionError::new(
                    SessionFailure::Timeout,
                    "CAPACITY_UNAVAILABLE",
                    "control timed out",
                ))
            }
        }
    }

    /// 閉じる（残りの入力を送り、worker が出し切って `Closed` を返すまで待つ）。出力・通知のチャネルは閉じる。
    pub async fn close(mut self) -> Result<(), SessionError> {
        self.shared.closing.store(true, Ordering::SeqCst);
        self.shared.close_notify.notify_one();
        self.shared.input_ready.notify_one();
        let reader = self.reader.take();
        let writer = self.writer.take();
        let r = match reader {
            Some(h) => tokio::time::timeout(self.close_timeout, h).await,
            None => Ok(Ok(())),
        };
        if let Some(w) = writer {
            w.abort();
        }
        match r {
            Ok(_) => Ok(()),
            Err(_) => Err(SessionError::new(
                SessionFailure::Timeout,
                "INTERNAL_ERROR",
                "timed out closing the session; worker killed",
            )),
        }
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        // 閉じずに捨てた: 仕事を止める（子プロセスは kill_on_drop で殺される）。
        if let Some(r) = self.reader.take() {
            r.abort();
        }
        if let Some(w) = self.writer.take() {
            w.abort();
        }
    }
}

/// 入力を送る（操作を先に）。閉じる合図で残りを送ってから `SessionClose` を書く。
async fn writer_task(
    sh: Arc<HandleShared>,
    mut stdin: ChildStdin,
    mut ctl_rx: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    loop {
        // 操作を先に
        while let Ok(f) = ctl_rx.try_recv() {
            if stdin.write_all(&f).await.is_err() {
                return;
            }
        }
        let item = lock(&sh.input).q.pop_front();
        if let Some(it) = item {
            {
                let mut q = lock(&sh.input);
                q.aus -= it.aus.len();
                q.bytes -= it.aus.iter().map(|a| a.payload.len()).sum::<usize>();
            }
            let n = it.aus.len();
            let chunks: Vec<&[AccessUnit]> = if n == 0 {
                vec![&[]]
            } else {
                it.aus.chunks(MAX_SESSION_AUS).collect()
            };
            let last = chunks.len() - 1;
            let mut config = it.config.clone();
            for (k, c) in chunks.into_iter().enumerate() {
                let frame = protocol::encode_message(&Message::SessionInput(SessionInput {
                    session_id: sh.sid,
                    source: it.source.clone(),
                    config: config.take(),
                    aus: c.to_vec(),
                    end: it.end && k == last,
                }));
                let Ok(frame) = frame else { continue };
                if stdin.write_all(&frame).await.is_err() {
                    return;
                }
            }
            continue;
        }
        if stdin.flush().await.is_err() {
            return;
        }
        if sh.closing.load(Ordering::SeqCst) {
            // 待っている操作を送ってから閉じる
            while let Ok(f) = ctl_rx.try_recv() {
                if stdin.write_all(&f).await.is_err() {
                    return;
                }
            }
            lock(&sh.input).closed = true;
            if let Ok(f) = protocol::encode_message(&Message::SessionClose(sh.sid)) {
                let _ = stdin.write_all(&f).await;
                let _ = stdin.flush().await;
            }
            // 子の stdin は reader が子を待つ間も開けておく（ここで落とすと EOF で閉じる）
            std::future::pending::<()>().await;
            return;
        }
        tokio::select! {
            f = ctl_rx.recv() => {
                match f {
                    Some(f) => if stdin.write_all(&f).await.is_err() { return },
                    None => return,
                }
            }
            _ = sh.input_ready.notified() => {}
            _ = sh.close_notify.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}

/// worker の出力を読む。`Closed` の前に途切れたら異常終了として知らせる。
async fn reader_task(
    sh: Arc<HandleShared>,
    mut child: Child,
    mut stdout: BufReader<ChildStdout>,
    out_tx: mpsc::Sender<SessionOutputItem>,
    mut sinks: BTreeSet<String>,
    pool: Arc<Mutex<PoolState>>,
) {
    let violation =
        |m: String| SessionError::new(SessionFailure::ProtocolViolation, "INTERNAL_ERROR", m);
    let outcome: Result<(), SessionError> = loop {
        let m = match read_frame(&mut stdout).await {
            Ok(m) => m,
            Err(ReadError::Eof(e)) => {
                break Err(SessionError::new(
                    SessionFailure::Crashed,
                    "INTERNAL_ERROR",
                    format!("worker exited: {e}"),
                ));
            }
            Err(ReadError::Violation(e)) => break Err(violation(e.to_string())),
        };
        match m {
            Message::SessionOutput(o) if o.session_id == sh.sid => {
                if !sinks.contains(&o.node) {
                    break Err(violation(format!("output for unknown node '{}'", o.node)));
                }
                let item = match o.kind {
                    SessionOutputKind::Media {
                        track,
                        generation,
                        config,
                        aus,
                    } => {
                        if track > 1 {
                            break Err(violation("output track out of range".into()));
                        }
                        if let Some(c) = &config
                            && aus.iter().any(|a| a.codec != c.codec)
                        {
                            break Err(violation("output AU codec differs from its config".into()));
                        }
                        SessionOutputItem::Media {
                            sink: o.node,
                            stream: o.stream,
                            track,
                            generation,
                            config,
                            aus,
                        }
                    }
                    SessionOutputKind::Snapshot {
                        generation,
                        pts_us,
                        width,
                        height,
                        changed_fraction,
                        jpeg,
                    } => {
                        if jpeg.len() < 4 || jpeg[0] != 0xff || jpeg[1] != 0xd8 {
                            break Err(violation("snapshot is not a JPEG".into()));
                        }
                        SessionOutputItem::Snapshot {
                            node: o.node,
                            generation,
                            pts_us,
                            width,
                            height,
                            changed_fraction,
                            jpeg,
                        }
                    }
                };
                // 受け手が遅ければ待つ（worker の書き込みが止まり、パイプで背圧になる）。受け手がいなければ捨てる。
                let _ = out_tx.send(item).await;
            }
            Message::SessionEvent(ev) if ev.session_id == sh.sid => match ev.kind {
                SessionEventKind::Event => match serde_json::from_str::<Event>(&ev.body) {
                    Ok(e) => sh.event(SessionEventItem::Pipeline(e)),
                    Err(e) => break Err(violation(format!("bad event JSON: {e}"))),
                },
                SessionEventKind::ControlOk | SessionEventKind::ControlErr => {
                    let Some(w) = lock(&sh.controls).remove(&ev.request_id) else {
                        continue;
                    };
                    let r = if ev.kind == SessionEventKind::ControlOk {
                        match serde_json::from_str::<ControlReply>(&ev.body) {
                            Ok(rep) => {
                                // 新しい世代の sink を出力の検査に足す
                                if let Some(p) = &rep.plan {
                                    sinks.extend(p.sinks.iter().map(|s| s.id.clone()));
                                    sinks.extend(
                                        p.nodes
                                            .iter()
                                            .filter(|n| n.node_type == "snapshot_on_change")
                                            .map(|n| n.id.clone()),
                                    );
                                }
                                Ok(rep)
                            }
                            Err(e) => Err(violation(format!("bad control reply: {e}"))),
                        }
                    } else {
                        Err(SessionError::from_body(SessionFailure::Control, &ev.body))
                    };
                    let _ = w.send(r);
                }
                SessionEventKind::Closed => break Ok(()),
                SessionEventKind::Error => {
                    let e = SessionError::from_body(SessionFailure::Control, &ev.body);
                    sh.event(SessionEventItem::Pipeline(Event::Error {
                        node: None,
                        code: e.code,
                        message: e.message,
                    }));
                }
                SessionEventKind::Opened => break Err(violation("second Opened".into())),
            },
            Message::Pong(_) => {}
            other => {
                break Err(violation(format!(
                    "unexpected message: {}",
                    msg_name(&other)
                )));
            }
        }
    };
    sh.alive.store(false, Ordering::SeqCst);
    lock(&sh.input).closed = true;
    match outcome {
        Ok(()) => {
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            let _ = child.start_kill();
            sh.fail_controls(&SessionError::new(
                SessionFailure::Closed,
                "CAPACITY_UNAVAILABLE",
                "session closed",
            ));
            sh.event(SessionEventItem::Closed);
        }
        Err(e) => {
            let _ = child.start_kill();
            let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .ok()
                .and_then(|r| r.ok())
                .map(|s| s.to_string())
                .unwrap_or_default();
            lock(&pool).stats.crashed += 1;
            sh.fail_controls(&e);
            // 異常終了の知らせは捨てない（一杯なら待つ）
            let reason = format!("{} ({status})", e.message);
            let _ = sh.events.send(SessionEventItem::Crashed { reason }).await;
        }
    }
}
