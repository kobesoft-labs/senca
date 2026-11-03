//! 分散の制御面（M9、仕様 8.1、ADR-0009）。制御状態のコマンドを自前の Raft（senca-raft）で複製する。
//!
//! - 駆動役は専用のスレッド: 100 ms ごとに tick、Ready を「永続化（fsync）→ 送信 → 適用」の順に処理する。
//! - 提案はどのノードからでもできる。リーダーでなければリーダーへ転送し、適用の結果を受け取る。
//!   リーダーが分からない・過半数と連絡が取れないときは 503（retryable）。
//! - 適用は全ノードで同じ順に `ControlState::apply`。失敗するコマンドも同じ結果になる（決定的）。
//! - スナップショット: 1000 件ごとに制御状態を写して古いログを捨てる。
//! - ノード間の通信は TCP（`tls_*` があれば mTLS）の上の枠（種類 1 バイト＋長さ 4 バイト）。枠の前に、クラスタの
//!   合言葉による相互の挑戦応答（`cluster_auth`）で互いの番号を確かめる（合言葉も固定のハッシュも送らない）。
//! - 過半数との連絡: リーダーは過半数の投票者から最後に返事を受けた時刻、フォロワーはリーダーから最後に受けた時刻を
//!   「確かめた時刻」として持つ。これが古くなったノードは `/readyz` を 503 にし、グリッドの仕事を止める（ADR-0013 fencing）。

use crate::app::App;
use bytes::{Buf, Bytes};
use senca_control::state::{Command, ControlState};
use senca_control::{Problem, ProblemCode};
use senca_raft::storage_file::FileStorage;
use senca_raft::{Config, EntryKind, InitialState, Message, ProposeError, RawNode, Snapshot, Storage};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as smpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

const TICK: Duration = Duration::from_millis(100);
const PROPOSE_TIMEOUT: Duration = Duration::from_secs(5);
const SNAPSHOT_EVERY: u64 = 1000;
const MAX_FRAME: usize = 64 << 20;

const K_RAFT: u8 = 1;
const K_FORWARD: u8 = 2;
const K_RESULT: u8 = 3;
/// グリッドの心拍（ADR-0013。合意ログに載せない。リーダーへ送る）。
const K_HEARTBEAT: u8 = 4;
/// 監査ログの読み出しの問い合わせと答え（合意ログに載せない。[`Cluster::audit_query`]）。
const K_AUDIT_QUERY: u8 = 5;
const K_AUDIT_RESULT: u8 = 6;
/// 監査ログの答えを待つ上限。
pub const AUDIT_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// 別のノードへの監査ログの問い合わせ（絞り込みのテナントは、問い合わせる側が呼び手の権限から決めたもの）。
#[derive(serde::Serialize, serde::Deserialize)]
pub struct AuditQuery {
    pub filter: crate::audit::Filter,
    pub start: Option<crate::audit::Pos>,
    pub limit: usize,
}

/// 問い合わせ中の答えの受け口（番号 → (問い合わせたノード, 送り先)）。
type Pending = Arc<std::sync::Mutex<HashMap<u64, (u64, tokio::sync::oneshot::Sender<Bytes>)>>>;

type Reply = smpsc::Sender<Result<(), Problem>>;

enum DriverMsg {
    Step(Message),
    Propose { cmd: Command, reply: Reply },
    Forwarded { from: u64, req: u64, cmd: Command },
    ForwardResult { req: u64, result: Result<(), Problem> },
}

pub struct Cluster {
    #[allow(dead_code)] // 運用の API（GET /nodes の raft の状態）で使う
    pub me: u64,
    tx: smpsc::Sender<DriverMsg>,
    leader: Arc<AtomicU64>,
    #[allow(dead_code)]
    pub peers: HashMap<u64, SocketAddr>,
    /// 相手ごとの送信のキュー（心拍のように Raft の外で送るもの）。
    out: HashMap<u64, mpsc::Sender<Vec<u8>>>,
    /// 過半数（またはリーダー）との連絡を最後に確かめた時刻（`util::mono_ms() + 1`。0 = まだ無い）。
    contact: Arc<AtomicU64>,
    /// 監査ログの問い合わせ（答え待ち）。
    pending: Pending,
    next_query: AtomicU64,
}

fn unavailable(t: &str) -> Problem {
    Problem::new(ProblemCode::CapacityUnavailable, t.to_string()).with_retryable(true)
}

impl Cluster {
    pub fn leader(&self) -> Option<u64> {
        match self.leader.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    /// 過半数との連絡を最後に確かめてからの時間（ミリ秒。まだ無ければ `u64::MAX`）。
    pub fn contact_age_ms(&self) -> u64 {
        match self.contact.load(Ordering::Relaxed) {
            0 => u64::MAX,
            t => (crate::util::mono_ms() + 1).saturating_sub(t),
        }
    }

    /// グリッドの心拍を送る（キューがあふれていれば捨てる。次の心拍が来る）。
    pub fn send_heartbeat(&self, to: u64, body: &[u8]) {
        if let Some(q) = self.out.get(&to) {
            let _ = q.try_send(frame(K_HEARTBEAT, body));
        }
    }

    /// 別のノード（Raft の番号 `to`）の監査ログを読む。ノード間の接続（合言葉の相互認証・mTLS）で送り、呼び手の資格情報は
    /// 送らない（テナントの絞り込みは `q.filter.tenant` に入れて送る）。届かない・時間内に答えが無ければ Err。
    pub async fn audit_query(&self, to: u64, q: &AuditQuery) -> Result<crate::audit::Page, String> {
        let out = self.out.get(&to).ok_or_else(|| format!("unknown node {to}"))?;
        let id = self.next_query.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, (to, tx));
        let mut p = id.to_be_bytes().to_vec();
        p.extend_from_slice(&serde_json::to_vec(q).map_err(|e| e.to_string())?);
        let r = match out.try_send(frame(K_AUDIT_QUERY, &p)) {
            Err(_) => Err("send queue full or closed".to_string()),
            Ok(()) => match tokio::time::timeout(AUDIT_QUERY_TIMEOUT, rx).await {
                Ok(Ok(b)) => serde_json::from_slice::<Result<crate::audit::Page, String>>(&b).map_err(|e| e.to_string()).and_then(|r| r),
                Ok(Err(_)) => Err("closed".into()),
                Err(_) => Err("timeout".into()),
            },
        };
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        r
    }

    /// 提案して、自分のノードで適用されるまで待つ（ブロックしてよいスレッドから呼ぶ）。
    pub fn propose(&self, cmd: Command) -> Result<(), Problem> {
        let (reply, rx) = smpsc::channel();
        self.tx.send(DriverMsg::Propose { cmd, reply }).map_err(|_| unavailable("制御面が止まっています"))?;
        rx.recv_timeout(PROPOSE_TIMEOUT).unwrap_or_else(|_| Err(unavailable("合意が時間内に取れませんでした（過半数と連絡が取れない可能性）")))
    }
}

fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.push(kind);
    v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// 起動する。`peers` は投票するノード（自分を含む）。データは `data_dir/control/raft/`。
/// ノード間の mTLS の設定（無ければ平文の TCP。閉じたネットワークでだけ使う）。
#[derive(Clone)]
pub struct ClusterTls {
    pub client: Arc<senca_tls::ClientConfig>,
    pub server: Arc<senca_tls::ServerConfig>,
}

/// ノード間の証明書の名前（全ノードの証明書の SAN に入れる）。
pub const CLUSTER_SERVER_NAME: &str = "senca-cluster";

impl ClusterTls {
    pub fn load(ca: &std::path::Path, cert: &std::path::Path, key: &std::path::Path) -> Result<ClusterTls, String> {
        let mut roots = senca_tls::RootStore::new();
        roots.add_pem_file(ca).map_err(|e| format!("cluster ca: {e:?}"))?;
        let roots = Arc::new(roots);
        let ck = Arc::new(senca_tls::CertifiedKey::from_pem_files(cert, key).map_err(|e| format!("cluster cert: {e:?}"))?);
        let mut s = senca_tls::ServerConfig::new(ck.clone()).map_err(|e| format!("{e:?}"))?;
        s.client_auth = senca_tls::ClientAuth::Required(roots.clone());
        let mut c = senca_tls::ClientConfig::new(senca_tls::ServerVerifier::WebPki { roots, spki_pins: vec![] });
        c.client_cert = Some(ck);
        Ok(ClusterTls { client: Arc::new(c), server: Arc::new(s) })
    }
}

pub fn start(app: Arc<App>, me: u64, listen: SocketAddr, peers: HashMap<u64, SocketAddr>, secret: String) -> Result<Arc<Cluster>, String> {
    let tls = match app.cfg.cluster.as_ref().map(|c| (c.tls_ca.clone(), c.tls_cert.clone(), c.tls_key.clone())) {
        Some((Some(ca), Some(cert), Some(key))) => Some(ClusterTls::load(&ca, &cert, &key)?),
        Some((None, None, None)) | None => None,
        _ => return Err("cluster: tls_ca・tls_cert・tls_key は 3 つそろえてください".into()),
    };
    let dir = app.cfg.data_dir.join("control").join("raft");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut storage = FileStorage::open(&dir).map_err(|e| format!("raft storage: {e}"))?;
    let mut init = storage.load().map_err(|e| format!("raft storage: {e}"))?;
    if init.snapshot.is_none() && init.entries.is_empty() && init.hard_state.term == 0 {
        // 新しいクラスタ: 全投票者が同じ bootstrap を書く
        let snap = Snapshot::bootstrap(peers.keys().copied());
        storage.save_snapshot(&snap).map_err(|e| e.to_string())?;
        init = InitialState { hard_state: init.hard_state, snapshot: Some(snap), entries: vec![] };
    }
    // 状態機械の復元（snapshot のデータ）
    if let Some(s) = &init.snapshot
        && !s.data.is_empty()
    {
        let st: ControlState = serde_json::from_slice(&s.data).map_err(|e| format!("raft snapshot: {e}"))?;
        app.control.lock().unwrap().replace_state(st);
    }
    let mut cfg = Config::new(me);
    let mut seed = [0u8; 8];
    let _ = getrandom::fill(&mut seed);
    cfg.seed = u64::from_le_bytes(seed) | 1;
    let raw = RawNode::new(cfg, init).map_err(|e| e.to_string())?;
    let (tx, rx) = smpsc::channel();
    let leader = Arc::new(AtomicU64::new(0));
    let contact = Arc::new(AtomicU64::new(0));
    let key = crate::cluster_auth::AuthKey::derive(&secret, crate::cluster_auth::Purpose::Raft);
    let voters: Vec<u64> = peers.keys().copied().collect();
    // 相手ごとの送信のキュー（あふれたら落とす。Raft は落ちても安全）
    let mut outq: HashMap<u64, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let rt = tokio::runtime::Handle::current();
    for (&id, &addr) in &peers {
        if id == me {
            continue;
        }
        let (otx, orx) = mpsc::channel(4096);
        outq.insert(id, otx);
        rt.spawn(dial(app.clone(), me, id, addr, key.clone(), orx, tls.clone()));
    }
    let l = std::net::TcpListener::bind(listen).map_err(|e| format!("cluster: bind {listen}: {e}"))?;
    l.set_nonblocking(true).map_err(|e| e.to_string())?;
    let l = tokio::net::TcpListener::from_std(l).map_err(|e| e.to_string())?;
    let pending: Pending = Default::default();
    rt.spawn(accept(app.clone(), l, me, voters.clone(), key, tx.clone(), tls.clone(), outq.clone(), pending.clone()));
    let c = Arc::new(Cluster {
        me,
        tx: tx.clone(),
        leader: leader.clone(),
        peers,
        out: outq.clone(),
        contact: contact.clone(),
        pending,
        next_query: AtomicU64::new(1),
    });
    let app2 = app.clone();
    std::thread::Builder::new()
        .name("senca-raft".into())
        .spawn(move || drive(app2, raw, storage, rx, outq, leader, contact, voters, me))
        .map_err(|e| e.to_string())?;
    Ok(c)
}

async fn dial(app: Arc<App>, me: u64, peer: u64, addr: SocketAddr, key: crate::cluster_auth::AuthKey, mut rx: mpsc::Receiver<Vec<u8>>, tls: Option<ClusterTls>) {
    // 同じ失敗を 500 ms ごとに書かない
    let mut last_auth_error = String::new();
    loop {
        let tcp = match tokio::net::TcpStream::connect(addr).await {
            Ok(s) => s,
            Err(_) => {
                // つながるまで溜めない（古いメッセージは捨てる）
                while rx.try_recv().is_ok() {}
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        let _ = tcp.set_nodelay(true);
        let mut s: Box<dyn crate::io::IoStream> = match &tls {
            None => Box::new(tcp),
            Some(t) => match crate::tls_io::connect(tcp, t.client.clone(), CLUSTER_SERVER_NAME).await {
                Ok(s) => Box::new(s),
                Err(e) => {
                    app.log(&format!("cluster: mTLS to node {peer} failed: {e}"));
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                    continue;
                }
            },
        };
        if let Err(e) = crate::cluster_auth::client(&mut s, &key, me, peer).await {
            if e != last_auth_error {
                app.log(&format!("cluster: authentication to node {peer} failed: {e}"));
                last_auth_error = e;
            }
            while rx.try_recv().is_ok() {}
            tokio::time::sleep(Duration::from_millis(1000)).await;
            continue;
        }
        last_auth_error.clear();
        loop {
            let Some(f) = rx.recv().await else { return };
            if s.write_all(&f).await.is_err() {
                break;
            }
        }
        app.log(&format!("cluster: connection to node {peer} lost"));
    }
}

#[allow(clippy::too_many_arguments)]
async fn accept(
    app: Arc<App>,
    l: tokio::net::TcpListener,
    me: u64,
    voters: Vec<u64>,
    key: crate::cluster_auth::AuthKey,
    tx: smpsc::Sender<DriverMsg>,
    tls: Option<ClusterTls>,
    outq: HashMap<u64, mpsc::Sender<Vec<u8>>>,
    pending: Pending,
) {
    let voters = Arc::new(voters);
    let outq = Arc::new(outq);
    loop {
        let Ok((tcp, peer)) = l.accept().await else {
            // EMFILE などで回り続けない
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let tx = tx.clone();
        let app = app.clone();
        let tls = tls.clone();
        let key = key.clone();
        let voters = voters.clone();
        let outq = outq.clone();
        let pending = pending.clone();
        tokio::spawn(async move {
            let mut s: Box<dyn crate::io::IoStream> = match tls {
                None => Box::new(tcp),
                Some(t) => match crate::tls_io::accept(tcp, t.server.clone()).await {
                    Ok(s) => Box::new(s),
                    Err(e) => {
                        app.log(&format!("cluster: mTLS from {peer} refused: {e}"));
                        return;
                    }
                },
            };
            // 相互の挑戦応答（名乗った番号を合言葉で確かめる。名乗る前の枠は受けない）
            let from = match crate::cluster_auth::server(&mut s, &key, me, &|id| voters.contains(&id)).await {
                Ok(id) => id,
                Err(e) => {
                    app.log(&format!("cluster: rejected connection from {peer}: {e}"));
                    return;
                }
            };
            let mut hdr = [0u8; 5];
            loop {
                if s.read_exact(&mut hdr).await.is_err() {
                    return;
                }
                let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
                if len > MAX_FRAME {
                    return;
                }
                let mut body = vec![0u8; len];
                if s.read_exact(&mut body).await.is_err() {
                    return;
                }
                let body = Bytes::from(body);
                match hdr[0] {
                    K_RAFT => {
                        if let Ok(m) = senca_raft::codec::decode_message_body(body)
                            && m.from == from
                        {
                            let _ = tx.send(DriverMsg::Step(m));
                        }
                    }
                    K_FORWARD => {
                        if body.len() < 8 {
                            return;
                        }
                        let req = (&body[..8]).get_u64();
                        if let Ok(cmd) = serde_json::from_slice::<Command>(&body[8..]) {
                            let _ = tx.send(DriverMsg::Forwarded { from, req, cmd });
                        }
                    }
                    K_HEARTBEAT => {
                        // 名乗った Raft の番号と、心拍のノードの raft_id が合うものだけ
                        if let Ok(hb) = serde_json::from_slice::<senca_grid::Heartbeat>(&body) {
                            let ok = app.control.lock().unwrap().state.nodes.get(&hb.node).and_then(|n| n.labels.get("raft_id")).is_some_and(|r| r == &from.to_string());
                            if ok {
                                crate::grid::on_heartbeat(&app, hb);
                            }
                        }
                    }
                    K_RESULT => {
                        if body.len() < 8 {
                            return;
                        }
                        let req = (&body[..8]).get_u64();
                        let result = match serde_json::from_slice::<Option<Problem2>>(&body[8..]) {
                            Ok(None) => Ok(()),
                            Ok(Some(p)) => Err(p.into()),
                            Err(_) => Err(Problem::internal()),
                        };
                        let _ = tx.send(DriverMsg::ForwardResult { req, result });
                    }
                    K_AUDIT_QUERY => {
                        if body.len() < 8 {
                            return;
                        }
                        let id = (&body[..8]).get_u64();
                        let Some(out) = outq.get(&from).cloned() else { continue };
                        let q = serde_json::from_slice::<AuditQuery>(&body[8..]);
                        let app = app.clone();
                        // ファイルを読むのでブロックしてよいスレッドで。答えは自分から相手への接続で返す
                        tokio::task::spawn_blocking(move || {
                            let r: Result<crate::audit::Page, String> = match q {
                                Ok(q) => Ok(app.audit_log.query(&q.filter, q.start, q.limit.clamp(1, 1000), crate::audit::SCAN_BUDGET)),
                                Err(e) => Err(format!("bad query: {e}")),
                            };
                            let mut p = id.to_be_bytes().to_vec();
                            p.extend_from_slice(&serde_json::to_vec(&r).unwrap_or_default());
                            let _ = out.try_send(frame(K_AUDIT_RESULT, &p));
                        });
                    }
                    K_AUDIT_RESULT => {
                        if body.len() < 8 {
                            return;
                        }
                        let id = (&body[..8]).get_u64();
                        // 問い合わせた相手からの答えだけを受ける
                        let w = {
                            let mut p = pending.lock().unwrap_or_else(|e| e.into_inner());
                            if p.get(&id).is_some_and(|(to, _)| *to == from) { p.remove(&id) } else { None }
                        };
                        if let Some((_, w)) = w {
                            let _ = w.send(body.slice(8..));
                        }
                    }
                    // 知らない種類の枠は読み飛ばす（新しい版のノードと混ざっても接続を切らない）
                    _ => continue,
                }
            }
        });
    }
}

/// 転送の結果（Problem は Deserialize を持たないので写しの型）。
#[derive(serde::Serialize, serde::Deserialize)]
struct Problem2 {
    status: u16,
    code: String,
    title: String,
    retryable: bool,
}

impl From<&Problem> for Problem2 {
    fn from(p: &Problem) -> Self {
        Problem2 { status: p.status, code: serde_json::to_value(p.code).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(), title: p.title.clone(), retryable: p.retryable }
    }
}

impl From<Problem2> for Problem {
    fn from(p: Problem2) -> Problem {
        let code = serde_json::from_value(serde_json::Value::String(p.code)).unwrap_or(ProblemCode::InternalError);
        Problem::new(code, p.title).with_status(p.status).with_retryable(p.retryable)
    }
}

enum Waiter {
    Local(Reply),
    Remote { from: u64, req: u64 },
}

#[allow(clippy::too_many_arguments)]
fn drive(
    app: Arc<App>,
    mut raw: RawNode,
    mut storage: FileStorage,
    rx: smpsc::Receiver<DriverMsg>,
    outq: HashMap<u64, mpsc::Sender<Vec<u8>>>,
    leader: Arc<AtomicU64>,
    contact: Arc<AtomicU64>,
    voters: Vec<u64>,
    me: u64,
) {
    // 相手ごとに、最後に Raft のメッセージを受けた時刻（mono_ms + 1）
    let mut heard: HashMap<u64, u64> = HashMap::new();
    let mut next_tick = Instant::now() + TICK;
    // index → (term, waiter)
    let mut waiting: HashMap<u64, (u64, Waiter)> = HashMap::new();
    // 転送中の提案（自分が出した）
    let mut forwarded: HashMap<u64, (Reply, Instant)> = HashMap::new();
    let mut next_req = 1u64;
    let mut applied_since_snap = 0u64;
    let send = |to: u64, f: Vec<u8>| {
        if let Some(q) = outq.get(&to) {
            let _ = q.try_send(f);
        }
    };
    loop {
        let wait = next_tick.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(DriverMsg::Step(m)) => {
                heard.insert(m.from, crate::util::mono_ms() + 1);
                let _ = raw.step(m);
            }
            Ok(DriverMsg::Propose { cmd, reply }) => match raw.status().leader {
                Some(l) if l == me => propose_local(&mut raw, &cmd, Waiter::Local(reply), &mut waiting),
                Some(l) => {
                    let req = next_req;
                    next_req += 1;
                    let mut p = req.to_be_bytes().to_vec();
                    p.extend_from_slice(&serde_json::to_vec(&cmd).unwrap_or_default());
                    send(l, frame(K_FORWARD, &p));
                    forwarded.insert(req, (reply, Instant::now()));
                }
                None => {
                    let _ = reply.send(Err(unavailable("リーダーが決まっていません")));
                }
            },
            Ok(DriverMsg::Forwarded { from, req, cmd }) => {
                if raw.status().leader == Some(me) {
                    propose_local(&mut raw, &cmd, Waiter::Remote { from, req }, &mut waiting);
                } else {
                    let mut p = req.to_be_bytes().to_vec();
                    p.extend_from_slice(&serde_json::to_vec(&Some(Problem2::from(&unavailable("リーダーではありません")))).unwrap_or_default());
                    send(from, frame(K_RESULT, &p));
                }
            }
            Ok(DriverMsg::ForwardResult { req, result }) => {
                if let Some((r, _)) = forwarded.remove(&req) {
                    let _ = r.send(result);
                }
            }
            Err(smpsc::RecvTimeoutError::Timeout) => {}
            Err(smpsc::RecvTimeoutError::Disconnected) => return,
        }
        if Instant::now() >= next_tick {
            raw.tick();
            next_tick += TICK;
            if Instant::now() > next_tick {
                next_tick = Instant::now() + TICK;
            }
            forwarded.retain(|_, (_, t)| t.elapsed() < PROPOSE_TIMEOUT);
        }
        while let Some(rd) = raw.ready() {
            // 1. 永続化（失敗したらノードを止める: 送信も適用もしない）
            if let Err(e) = storage.persist(&rd) {
                app.log(&format!("cluster: raft storage failed: {e}; stopping the control plane"));
                std::process::exit(70);
            }
            // 2. 送信
            for m in &rd.messages {
                let f = senca_raft::codec::encode_message(m);
                // encode_message は先頭に 4 バイトの長さを持つ。枠の中身はその後ろ
                send(m.to, frame(K_RAFT, &f[4..]));
            }
            // 3. 適用
            if let Some(s) = &rd.snapshot
                && !s.data.is_empty()
                && let Ok(st) = serde_json::from_slice::<ControlState>(&s.data)
            {
                app.control.lock().unwrap().replace_state(st);
            }
            for e in &rd.committed_entries {
                if e.kind != EntryKind::Normal {
                    continue;
                }
                let result = match serde_json::from_slice::<Command>(&e.data) {
                    Ok(cmd) => app.control.lock().unwrap().apply_committed(cmd),
                    Err(_) => Err(Problem::internal()),
                };
                applied_since_snap += 1;
                if let Some((term, w)) = waiting.remove(&e.index) {
                    // 同じ term なら自分の提案。違えば上書きされた（失われた）
                    let r = if term == e.term { result } else { Err(unavailable("提案が失われました（リーダーが替わった）")) };
                    match w {
                        Waiter::Local(reply) => {
                            let _ = reply.send(r);
                        }
                        Waiter::Remote { from, req } => {
                            let mut p = req.to_be_bytes().to_vec();
                            let pr: Option<Problem2> = r.as_ref().err().map(Problem2::from);
                            p.extend_from_slice(&serde_json::to_vec(&pr).unwrap_or_default());
                            send(from, frame(K_RESULT, &p));
                        }
                    }
                }
            }
            if applied_since_snap >= SNAPSHOT_EVERY
                && let Some(last) = rd.committed_entries.last()
            {
                let data = serde_json::to_vec(&app.control.lock().unwrap().state).unwrap_or_default();
                if let Ok(snap) = raw.compact(last.index, Bytes::from(data))
                    && storage.save_snapshot(&snap).is_ok()
                {
                    applied_since_snap = 0;
                }
            }
        }
        let st = raw.status();
        leader.store(st.leader.unwrap_or(0), Ordering::Relaxed);
        if let Some(t) = confirmed_at(st.leader, me, &voters, &heard, crate::util::mono_ms() + 1) {
            contact.fetch_max(t, Ordering::Relaxed);
        }
        // リーダーでなくなったら、待っている提案は失敗させる
        if st.leader != Some(me) && !waiting.is_empty() {
            for (_, (_, w)) in waiting.drain() {
                if let Waiter::Local(r) = w {
                    let _ = r.send(Err(unavailable("リーダーが替わりました")));
                }
            }
        }
    }
}

/// 過半数との連絡を確かめた時刻。リーダーなら、過半数（自分を含む）から返事を受けた最も古い時刻
/// （= 投票者の時刻を新しい順に並べた過半数番目）。フォロワーならリーダーから最後に受けた時刻。リーダーが分からなければ None。
fn confirmed_at(leader: Option<u64>, me: u64, voters: &[u64], heard: &HashMap<u64, u64>, now: u64) -> Option<u64> {
    match leader {
        Some(l) if l == me => {
            let mut ts: Vec<u64> = voters.iter().map(|v| if *v == me { now } else { heard.get(v).copied().unwrap_or(0) }).collect();
            ts.sort_unstable_by(|a, b| b.cmp(a));
            ts.get(voters.len() / 2).copied().filter(|t| *t > 0)
        }
        Some(l) => heard.get(&l).copied(),
        None => None,
    }
}

fn propose_local(raw: &mut RawNode, cmd: &Command, w: Waiter, waiting: &mut HashMap<u64, (u64, Waiter)>) {
    let data = Bytes::from(serde_json::to_vec(cmd).unwrap_or_default());
    match raw.propose(data) {
        Ok(p) => {
            waiting.insert(p.index, (p.term, w));
        }
        Err(e) => {
            let pr = match e {
                ProposeError::TooLarge => Problem::new(ProblemCode::PayloadTooLarge, "コマンドが大きすぎます"),
                _ => unavailable("提案を受け付けられません（リーダーが替わった・混雑）"),
            };
            match w {
                Waiter::Local(r) => {
                    let _ = r.send(Err(pr));
                }
                Waiter::Remote { .. } => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmed_at_needs_a_quorum() {
        let voters = [1, 2, 3];
        let mut heard = HashMap::new();
        // リーダー 1: 誰からも聞いていない → 過半数を確かめていない
        assert_eq!(confirmed_at(Some(1), 1, &voters, &heard, 100), None);
        heard.insert(2, 90);
        // 自分 + 2 で過半数。確かめた時刻は 2 の時刻
        assert_eq!(confirmed_at(Some(1), 1, &voters, &heard, 100), Some(90));
        heard.insert(3, 95);
        assert_eq!(confirmed_at(Some(1), 1, &voters, &heard, 100), Some(95));
        // フォロワー: リーダーから最後に受けた時刻
        assert_eq!(confirmed_at(Some(3), 1, &voters, &heard, 100), Some(95));
        assert_eq!(confirmed_at(None, 1, &voters, &heard, 100), None);
        // 1 台のクラスタのリーダーはいつも確か
        assert_eq!(confirmed_at(Some(1), 1, &[1], &HashMap::new(), 7), Some(7));
    }
}
