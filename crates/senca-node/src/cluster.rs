//! 分散の制御面（M9、仕様 8.1、ADR-0009）。制御状態のコマンドを自前の Raft（senca-raft）で複製する。
//!
//! - 駆動役は専用のスレッド: 100 ms ごとに tick、Ready を「永続化（fsync）→ 送信 → 適用」の順に処理する。
//! - 提案はどのノードからでもできる。リーダーでなければリーダーへ転送し、適用の結果を受け取る。
//!   リーダーが分からない・過半数と連絡が取れないときは 503（retryable）。
//! - 適用は全ノードで同じ順に `ControlState::apply`。失敗するコマンドも同じ結果になる（決定的）。
//! - スナップショット: 1000 件ごとに制御状態を写して古いログを捨てる。
//! - ノード間の通信は TCP の枠（種類 1 バイト＋長さ 4 バイト）。最初の枠でクラスタの合言葉（共有の秘密）を確かめる。
//!   mTLS は TLS の統合で入れる（ADR-0009 の未了の項目）。

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

const K_HELLO: u8 = 0;
const K_RAFT: u8 = 1;
const K_FORWARD: u8 = 2;
const K_RESULT: u8 = 3;

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
    // 相手ごとの送信のキュー（あふれたら落とす。Raft は落ちても安全）
    let mut outq: HashMap<u64, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let rt = tokio::runtime::Handle::current();
    for (&id, &addr) in &peers {
        if id == me {
            continue;
        }
        let (otx, orx) = mpsc::channel(4096);
        outq.insert(id, otx);
        rt.spawn(dial(app.clone(), me, id, addr, secret.clone(), orx, tls.clone()));
    }
    rt.spawn(accept(app.clone(), listen, secret.clone(), tx.clone(), tls.clone()));
    let c = Arc::new(Cluster { me, tx: tx.clone(), leader: leader.clone(), peers });
    let app2 = app.clone();
    std::thread::Builder::new()
        .name("senca-raft".into())
        .spawn(move || drive(app2, raw, storage, rx, outq, leader, me))
        .map_err(|e| e.to_string())?;
    Ok(c)
}

async fn dial(app: Arc<App>, me: u64, peer: u64, addr: SocketAddr, secret: String, mut rx: mpsc::Receiver<Vec<u8>>, tls: Option<ClusterTls>) {
    let hello = {
        let mut h = me.to_be_bytes().to_vec();
        h.extend_from_slice(senca_control::secret::hash(&secret).as_bytes());
        frame(K_HELLO, &h)
    };
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
        if s.write_all(&hello).await.is_err() {
            continue;
        }
        loop {
            let Some(f) = rx.recv().await else { return };
            if s.write_all(&f).await.is_err() {
                break;
            }
        }
        app.log(&format!("cluster: connection to node {peer} lost"));
    }
}

async fn accept(app: Arc<App>, listen: SocketAddr, secret: String, tx: smpsc::Sender<DriverMsg>, tls: Option<ClusterTls>) {
    let l = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            app.log(&format!("cluster: bind {listen}: {e}"));
            return;
        }
    };
    let want = senca_control::secret::hash(&secret);
    loop {
        let Ok((tcp, peer)) = l.accept().await else { continue };
        let tx = tx.clone();
        let want = want.clone();
        let app = app.clone();
        let tls = tls.clone();
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
            let mut hdr = [0u8; 5];
            let mut from = 0u64;
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
                    K_HELLO => {
                        use subtle::ConstantTimeEq;
                        if body.len() != 8 + 64 || !bool::from(body[8..].ct_eq(want.as_bytes())) {
                            app.log(&format!("cluster: rejected connection from {peer} (bad cluster secret)"));
                            return;
                        }
                        from = (&body[..8]).get_u64();
                    }
                    _ if from == 0 => return, // 名乗る前の枠は受けない
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
                    _ => return,
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

fn drive(
    app: Arc<App>,
    mut raw: RawNode,
    mut storage: FileStorage,
    rx: smpsc::Receiver<DriverMsg>,
    outq: HashMap<u64, mpsc::Sender<Vec<u8>>>,
    leader: Arc<AtomicU64>,
    me: u64,
) {
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
                    Ok(cmd) => app.control.lock().unwrap().apply_committed(cmd, e.index),
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
