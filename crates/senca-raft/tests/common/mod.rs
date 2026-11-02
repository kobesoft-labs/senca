//! 決定的なシミュレーション（ネットワーク・故障・記憶域）と、各段で検査する不変条件。
//!
//! - 時間は単位時間（TICK = 10）で進む。全ノードは同じ時刻に tick する（時計のずれなし）。
//! - ネットワーク: 遅延（乱数）・喪失・重複・順序の入れ替わり・分断（片方向を含む）。
//! - 故障: ノードの停止と再起動（記憶域は残る）。Ready の永続化の途中での停止（fsync 前の書きかけ・
//!   最後のレコードの破れ・hard state 未書込み・送信の一部だけ）。
//! - 状態機械は KV。
//!
//! 毎段の検査: 選挙の安全（term ごとにリーダー 1 つ）、ログの一致（同じ (index, term) なら同じ中身と
//! 同じ直前の term）、リーダーの完全性（commit 済みの entry は後の term のリーダーのログにある）、
//! 状態機械の安全（同じ index で適用した entry と状態のハッシュが全ノードで同じ）、commit 済みの不変、
//! 構成変更の形（C → C_old,new → C_new の順で、古い側が直前の構成と一致）、ReadIndex の線形化可能性
//! （読み取りの index ≥ 開始時点の全体の commit）。

#![allow(dead_code)]

use bytes::Bytes;
use senca_raft::storage_file::FileStorage;
use senca_raft::*;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::path::PathBuf;

pub const TICK: u64 = 10;

// ---- 乱数 ----

#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0xD1B5_4A32_D192_ED03)
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    /// 千分率で当たり。
    pub fn permille(&mut self, p: u32) -> bool {
        self.below(1000) < p as u64
    }
    pub fn pick<T: Copy>(&mut self, v: &[T]) -> Option<T> {
        if v.is_empty() {
            None
        } else {
            Some(v[self.below(v.len() as u64) as usize])
        }
    }
}

fn fnv(h: &mut u64, b: &[u8]) {
    for &x in b {
        *h ^= x as u64;
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
}

pub fn entry_hash(e: &Entry) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325;
    fnv(&mut h, &[e.kind as u8]);
    fnv(&mut h, &e.data);
    h
}

// ---- KV ----

#[derive(Clone, Default, Debug)]
pub struct Kv {
    pub map: BTreeMap<String, String>,
    pub applied: u64,
}

impl Kv {
    pub fn apply(&mut self, e: &Entry) {
        assert_eq!(e.index, self.applied + 1, "apply out of order");
        self.applied = e.index;
        if e.kind == EntryKind::Normal {
            let s = std::str::from_utf8(&e.data).expect("utf8");
            let (k, v) = s.split_once('=').expect("k=v");
            self.map.insert(k.to_string(), v.to_string());
        }
    }
    pub fn hash(&self) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325;
        for (k, v) in &self.map {
            fnv(&mut h, k.as_bytes());
            fnv(&mut h, &[0]);
            fnv(&mut h, v.as_bytes());
            fnv(&mut h, &[1]);
        }
        h
    }
    pub fn encode(&self) -> Bytes {
        let mut b = Vec::new();
        for (k, v) in &self.map {
            b.extend_from_slice(&(k.len() as u32).to_be_bytes());
            b.extend_from_slice(k.as_bytes());
            b.extend_from_slice(&(v.len() as u32).to_be_bytes());
            b.extend_from_slice(v.as_bytes());
        }
        Bytes::from(b)
    }
    pub fn decode(b: &[u8], applied: u64) -> Kv {
        let mut map = BTreeMap::new();
        let mut p = 0;
        let rd = |p: &mut usize| {
            let l = u32::from_be_bytes(b[*p..*p + 4].try_into().unwrap()) as usize;
            let s = String::from_utf8(b[*p + 4..*p + 4 + l].to_vec()).unwrap();
            *p += 4 + l;
            s
        };
        while p < b.len() {
            let k = rd(&mut p);
            let v = rd(&mut p);
            map.insert(k, v);
        }
        Kv { map, applied }
    }
}

// ---- 記憶域 ----

pub enum Store {
    Mem(MemStorage),
    File(Option<Box<FileStorage>>, PathBuf),
}

impl Store {
    fn st(&mut self) -> &mut dyn Storage {
        match self {
            Store::Mem(m) => m,
            Store::File(f, p) => {
                if f.is_none() {
                    *f = Some(Box::new(
                        FileStorage::open_with(p, 4096).expect("open file storage"),
                    ));
                }
                f.as_mut().unwrap().as_mut()
            }
        }
    }

    /// プロセスが落ちた（ファイルを閉じる）。
    fn crash(&mut self) {
        if let Store::File(f, _) = self {
            *f = None;
        }
    }

    fn log_bytes(&self) -> Option<(PathBuf, u64)> {
        let Store::File(_, p) = self else { return None };
        let mut segs: Vec<PathBuf> = std::fs::read_dir(p.join("log"))
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "seg"))
            .collect();
        segs.sort();
        let last = segs.pop()?;
        let len = std::fs::metadata(&last).ok()?.len();
        Some((last, len))
    }
}

// ---- ノード ----

pub struct SimNode {
    pub id: NodeId,
    pub raft: Option<RawNode>,
    pub store: Store,
    pub kv: Kv,
    pub waiting_reads: Vec<ReadState>,
    pub boots: u64,
    /// このノードで受け付けた提案（index, term, data）。
    pub proposals: Vec<(Proposal, Bytes)>,
}

impl SimNode {
    pub fn store_save_snapshot(&mut self, s: &Snapshot) {
        self.store.st().save_snapshot(s).unwrap();
    }
}

#[derive(Clone, Debug)]
pub struct Params {
    pub voters: usize,
    /// ID の総数（voters より多い分は空で起動し、構成変更で加わる）。
    pub pool: usize,
    pub max_delay: u64,
    pub loss: u32,
    pub dup: u32,
    pub long_delay: u32,
    pub crash: u32,
    pub crash_in_persist: u32,
    pub restart: u32,
    pub partition: u32,
    pub conf_change: u32,
    pub transfer: u32,
    pub propose: u32,
    pub read: u32,
    pub compact: u32,
    pub lease_read: bool,
    pub check_quorum: bool,
    pub pre_vote: bool,
    pub file_storage: bool,
    pub max_inflight: usize,
    pub max_msg_bytes: u64,
    pub snapshot_chunk: u64,
}

impl Params {
    pub fn chaos(voters: usize) -> Params {
        Params {
            voters,
            pool: voters + 2,
            max_delay: 15,
            loss: 30,
            dup: 20,
            long_delay: 10,
            crash: 3,
            crash_in_persist: 3,
            restart: 60,
            partition: 8,
            conf_change: 8,
            transfer: 4,
            propose: 400,
            read: 200,
            compact: 30,
            lease_read: false,
            check_quorum: true,
            pre_vote: true,
            file_storage: false,
            max_inflight: 4,
            max_msg_bytes: 200,
            snapshot_chunk: 64,
        }
    }

    pub fn calm(voters: usize) -> Params {
        Params {
            loss: 0,
            dup: 0,
            long_delay: 0,
            crash: 0,
            crash_in_persist: 0,
            restart: 0,
            partition: 0,
            conf_change: 0,
            transfer: 0,
            propose: 0,
            read: 0,
            compact: 0,
            pool: voters,
            ..Params::chaos(voters)
        }
    }
}

struct CEntry {
    term: u64,
    hash: u64,
    kind: EntryKind,
    conf: Option<ConfState>,
    commit_term: u64,
}

struct ReadInfo {
    node: NodeId,
    min_index: u64,
    key: String,
}

#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub proposed: u64,
    pub committed: u64,
    pub reads_ok: u64,
    pub elections: u64,
    pub crashes: u64,
    pub persist_crashes: u64,
    pub snapshots_installed: u64,
    pub conf_changes: u64,
    pub transfers: u64,
    pub messages: u64,
}

pub struct Sim {
    pub rng: Rng,
    pub p: Params,
    pub now: u64,
    pub nodes: BTreeMap<NodeId, SimNode>,
    queue: BinaryHeap<Reverse<(u64, u64)>>,
    inflight: HashMap<u64, Message>,
    seq: u64,
    pub blocked: BTreeSet<(NodeId, NodeId)>,
    pub loss: u32,
    pub dup: u32,
    /// 送るメッセージを書き換える・捨てる（None）。筋書きのある試験用。
    pub intercept: Option<Box<dyn Fn(Message) -> Option<Message>>>,
    // 検査の状態
    leaders: BTreeMap<u64, NodeId>,
    canon: BTreeMap<u64, CEntry>,
    log_ids: HashMap<(u64, u64), (u64, u64)>,
    state_hash: HashMap<u64, u64>,
    last_commit: BTreeMap<NodeId, u64>,
    last_role: BTreeMap<NodeId, Role>,
    pub max_commit: u64,
    reads: HashMap<u64, ReadInfo>,
    /// ACK 済みの書込み (index, term, hash)。
    pub acked: Vec<(u64, u64, u64)>,
    next_ctx: u64,
    counter: u64,
    pub stats: Stats,
    tmp: Option<PathBuf>,
}

impl Drop for Sim {
    fn drop(&mut self) {
        if let Some(t) = &self.tmp {
            let _ = std::fs::remove_dir_all(t);
        }
    }
}

impl Sim {
    pub fn new(seed: u64, p: Params) -> Sim {
        let mut rng = Rng::new(seed);
        let tmp = p.file_storage.then(|| {
            let d = std::env::temp_dir().join(format!(
                "senca-raft-sim-{}-{seed}-{}",
                std::process::id(),
                rng.next()
            ));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            d
        });
        let mut sim = Sim {
            rng,
            p: p.clone(),
            now: 0,
            nodes: BTreeMap::new(),
            queue: BinaryHeap::new(),
            inflight: HashMap::new(),
            seq: 0,
            blocked: BTreeSet::new(),
            loss: p.loss,
            dup: p.dup,
            intercept: None,
            leaders: BTreeMap::new(),
            canon: BTreeMap::new(),
            log_ids: HashMap::new(),
            state_hash: HashMap::from([(0, Kv::default().hash())]),
            last_commit: BTreeMap::new(),
            last_role: BTreeMap::new(),
            max_commit: 0,
            reads: HashMap::new(),
            acked: Vec::new(),
            next_ctx: 1,
            counter: 0,
            stats: Stats::default(),
            tmp,
        };
        let voters: Vec<u64> = (1..=p.voters as u64).collect();
        for id in 1..=p.pool as u64 {
            let mut store = match &sim.tmp {
                None => Store::Mem(MemStorage::new()),
                Some(d) => {
                    let dir = d.join(format!("n{id}"));
                    Store::File(None, dir)
                }
            };
            if id <= p.voters as u64 {
                store
                    .st()
                    .save_snapshot(&Snapshot::bootstrap(voters.clone()))
                    .unwrap();
            }
            sim.nodes.insert(
                id,
                SimNode {
                    id,
                    raft: None,
                    store,
                    kv: Kv::default(),
                    waiting_reads: Vec::new(),
                    boots: 0,
                    proposals: Vec::new(),
                },
            );
            sim.start(id);
        }
        sim
    }

    fn config(&mut self, id: NodeId) -> Config {
        let mut c = Config::new(id);
        c.seed = self.rng.next();
        c.lease_read = self.p.lease_read;
        c.check_quorum = self.p.check_quorum;
        c.pre_vote = self.p.pre_vote;
        c.max_inflight_msgs = self.p.max_inflight;
        c.max_msg_bytes = self.p.max_msg_bytes;
        c.snapshot_chunk_bytes = self.p.snapshot_chunk;
        c
    }

    /// 記憶域から起動する。
    pub fn start(&mut self, id: NodeId) {
        let cfg = self.config(id);
        let n = self.nodes.get_mut(&id).unwrap();
        assert!(n.raft.is_none());
        let init = n.store.st().load().expect("load");
        let (kv, si) = match &init.snapshot {
            Some(s) => (Kv::decode(&s.data, s.meta.index), s.meta.index),
            None => (Kv::default(), 0),
        };
        if let Some(h) = self.state_hash.get(&si) {
            assert_eq!(*h, kv.hash(), "node {id}: restored snapshot {si} differs");
        }
        n.kv = kv;
        n.raft = Some(RawNode::new(cfg, init).expect("raft new"));
        n.boots += 1;
        n.waiting_reads.clear();
        self.last_role.insert(id, Role::Follower);
    }

    /// ノードを止める（記憶域は残る）。
    pub fn kill(&mut self, id: NodeId) {
        let n = self.nodes.get_mut(&id).unwrap();
        if n.raft.take().is_some() {
            n.store.crash();
            n.waiting_reads.clear();
            self.stats.crashes += 1;
        }
    }

    pub fn alive(&self, id: NodeId) -> bool {
        self.nodes[&id].raft.is_some()
    }

    pub fn raft(&self, id: NodeId) -> &RawNode {
        self.nodes[&id].raft.as_ref().expect("node is down")
    }

    pub fn raft_mut(&mut self, id: NodeId) -> &mut RawNode {
        self.nodes
            .get_mut(&id)
            .unwrap()
            .raft
            .as_mut()
            .expect("node is down")
    }

    pub fn ids(&self) -> Vec<NodeId> {
        self.nodes.keys().copied().collect()
    }

    pub fn live_ids(&self) -> Vec<NodeId> {
        self.nodes
            .values()
            .filter(|n| n.raft.is_some())
            .map(|n| n.id)
            .collect()
    }

    /// 現在の（最大の term の）リーダー。
    pub fn leader(&self) -> Option<NodeId> {
        self.nodes
            .values()
            .filter_map(|n| n.raft.as_ref())
            .filter(|r| r.is_leader())
            .max_by_key(|r| r.term())
            .map(|r| r.id())
    }

    // ---- ネットワーク ----

    pub fn isolate(&mut self, id: NodeId) {
        for o in self.ids() {
            if o != id {
                self.blocked.insert((id, o));
                self.blocked.insert((o, id));
            }
        }
    }

    pub fn partition(&mut self, groups: &[Vec<NodeId>]) {
        self.blocked.clear();
        for (i, a) in groups.iter().enumerate() {
            for (j, b) in groups.iter().enumerate() {
                if i != j {
                    for x in a {
                        for y in b {
                            self.blocked.insert((*x, *y));
                        }
                    }
                }
            }
        }
    }

    pub fn heal(&mut self) {
        self.blocked.clear();
    }

    fn send(&mut self, m: Message) {
        self.stats.messages += 1;
        let m = match &self.intercept {
            Some(f) => match f(m) {
                Some(m) => m,
                None => return,
            },
            None => m,
        };
        // 実際の転送と同じく符号化を通す（往復で同じになること）。
        let frame = senca_raft::codec::encode_message(&m);
        let back = senca_raft::codec::decode_message(
            Bytes::from(frame),
            senca_raft::codec::DEFAULT_MAX_FRAME,
        )
        .expect("codec roundtrip");
        assert_eq!(back, m, "codec roundtrip");
        let m = back;
        if self.blocked.contains(&(m.from, m.to)) || self.rng.permille(self.loss) {
            return;
        }
        let copies = if self.rng.permille(self.dup) { 2 } else { 1 };
        for _ in 0..copies {
            let mut d = 1 + self.rng.below(self.p.max_delay.max(1));
            if self.rng.permille(self.p.long_delay) {
                d += self.rng.below(5 * TICK);
            }
            self.seq += 1;
            self.queue.push(Reverse((self.now + d, self.seq)));
            self.inflight.insert(self.seq, m.clone());
        }
    }

    // ---- 実行 ----

    /// 1 tick 分進める（メッセージを届け、全ノードを tick し、負荷をかける）。
    pub fn step_tick(&mut self, workload: bool) {
        let until = self.now + TICK;
        while let Some(Reverse((t, s))) = self.queue.peek().copied() {
            if t > until {
                break;
            }
            self.queue.pop();
            self.now = t;
            let m = self.inflight.remove(&s).unwrap();
            let to = m.to;
            let Some(r) = self.nodes.get_mut(&to).and_then(|n| n.raft.as_mut()) else {
                continue;
            };
            r.step(m).expect("step");
            self.process(to);
            self.check();
        }
        self.now = until;
        let mut ids = self.live_ids();
        // 順番を混ぜる
        for i in (1..ids.len()).rev() {
            let j = self.rng.below(i as u64 + 1) as usize;
            ids.swap(i, j);
        }
        for id in ids {
            if let Some(r) = self.nodes.get_mut(&id).and_then(|n| n.raft.as_mut()) {
                r.tick();
                self.process(id);
                self.check();
            }
        }
        if workload {
            self.workload();
        }
    }

    pub fn run(&mut self, ticks: u64, workload: bool) {
        for _ in 0..ticks {
            self.step_tick(workload);
        }
    }

    /// Ready を処理し切る。
    pub fn process(&mut self, id: NodeId) {
        loop {
            let Some(rd) = self
                .nodes
                .get_mut(&id)
                .and_then(|n| n.raft.as_mut())
                .and_then(|r| r.ready())
            else {
                return;
            };
            if !self.handle_ready(id, rd) {
                return;
            }
        }
    }

    /// false ならノードが落ちた。
    fn handle_ready(&mut self, id: NodeId, rd: Ready) -> bool {
        self.check_ready_entries(id, &rd);
        let crash_mode = if self.rng.permille(self.p.crash_in_persist) {
            Some(self.rng.below(5))
        } else {
            None
        };
        let n = self.nodes.get_mut(&id).unwrap();
        match crash_mode {
            None | Some(3) | Some(4) => n.store.st().persist(&rd).expect("persist"),
            Some(0) => {}
            Some(m) => {
                // 1: snapshot と entry の途中まで（最後のレコードが破れる）。2: entry はすべて、hard state は書かない。
                if let Some(s) = &rd.snapshot {
                    n.store.st().save_snapshot(s).unwrap();
                }
                if !rd.entries.is_empty() {
                    let before = n.store.log_bytes();
                    if let Store::Mem(ms) = &mut n.store {
                        let k = if m == 1 {
                            self.rng.below(rd.entries.len() as u64 + 1) as usize
                        } else {
                            rd.entries.len()
                        };
                        if k > 0 {
                            ms.append(&rd.entries[..k]).unwrap();
                        }
                    } else {
                        n.store.st().append(&rd.entries).unwrap();
                        if m == 1
                            && let Some((path, len)) = n.store.log_bytes()
                        {
                            let start = match before {
                                Some((bp, bl)) if bp == path => bl,
                                _ => 0,
                            };
                            if len > start {
                                let cut = start + self.rng.below(len - start + 1);
                                let f =
                                    std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                                f.set_len(cut).unwrap();
                            }
                        }
                    }
                }
            }
        }
        if let Some(m) = crash_mode
            && m != 4
        {
            self.stats.persist_crashes += 1;
            self.kill(id);
            return false;
        }
        let partial = crash_mode == Some(4);
        for m in rd.messages {
            if partial && self.rng.permille(500) {
                continue;
            }
            self.send(m);
        }
        if partial {
            self.stats.persist_crashes += 1;
            self.kill(id);
            return false;
        }
        // 適用
        let n = self.nodes.get_mut(&id).unwrap();
        if let Some(s) = &rd.snapshot {
            n.kv = Kv::decode(&s.data, s.meta.index);
            self.stats.snapshots_installed += 1;
            let h = n.kv.hash();
            match self.state_hash.get(&s.meta.index) {
                Some(x) => assert_eq!(*x, h, "node {id}: snapshot {} state differs", s.meta.index),
                None => {
                    self.state_hash.insert(s.meta.index, h);
                }
            }
        }
        for e in &rd.committed_entries {
            let n = self.nodes.get_mut(&id).unwrap();
            n.kv.apply(e);
            let h = n.kv.hash();
            match self.state_hash.get(&e.index) {
                Some(x) => assert_eq!(*x, h, "node {id}: state after {} differs", e.index),
                None => {
                    self.state_hash.insert(e.index, h);
                }
            }
            let eh = entry_hash(e);
            let n = self.nodes.get_mut(&id).unwrap();
            if n.proposals
                .iter()
                .any(|(p, _)| p.index == e.index && p.term == e.term)
            {
                // 提案したノードで適用された = 利用者に成功を返した（ACK 済み）。
                self.acked.push((e.index, e.term, eh));
            }
            match self.canon.get_mut(&e.index) {
                Some(c) => assert!(
                    c.term == e.term && c.hash == eh,
                    "node {id}: applied entry {} differs",
                    e.index
                ),
                None => {
                    // 適用されたのに commit を観測していない（commit の観測は毎段なので起こらない）
                    let t = self.nodes[&id].raft.as_ref().unwrap().term();
                    self.record_commit(e.index, e.term, eh, e.kind, e.conf_state(), t);
                }
            }
        }
        let n = self.nodes.get_mut(&id).unwrap();
        n.waiting_reads.extend(rd.read_states.iter().copied());
        for rs in &rd.read_states {
            if let Some(info) = self.reads.get(&rs.ctx) {
                assert_eq!(info.node, id);
                assert!(
                    rs.index >= info.min_index,
                    "linearizability: read {} at index {} but {} was committed before it started",
                    rs.ctx,
                    rs.index,
                    info.min_index
                );
            }
        }
        self.serve_reads(id);
        true
    }

    fn serve_reads(&mut self, id: NodeId) {
        let n = self.nodes.get_mut(&id).unwrap();
        let applied = n.kv.applied;
        let mut keep = Vec::new();
        for rs in n.waiting_reads.drain(..) {
            if applied >= rs.index {
                if let Some(info) = self.reads.remove(&rs.ctx) {
                    let _v = n.kv.map.get(&info.key);
                    self.stats.reads_ok += 1;
                }
            } else {
                keep.push(rs);
            }
        }
        n.waiting_reads = keep;
    }

    // ---- 不変条件 ----

    fn check_ready_entries(&mut self, id: NodeId, rd: &Ready) {
        let r = self.nodes[&id].raft.as_ref().unwrap();
        for (i, e) in rd.entries.iter().enumerate() {
            let prev_term = if i > 0 {
                rd.entries[i - 1].term
            } else {
                r.log_term(e.index - 1).expect("prev term")
            };
            let v = (entry_hash(e), prev_term);
            match self.log_ids.get(&(e.index, e.term)) {
                Some(x) => assert_eq!(
                    *x, v,
                    "log matching: node {id} entry ({}, {})",
                    e.index, e.term
                ),
                None => {
                    self.log_ids.insert((e.index, e.term), v);
                }
            }
            if let Some(c) = e.conf_state() {
                // 直前の構成（このノードのログで）からの変化の形
                let mut prev = None;
                let mut j = e.index - 1;
                while j >= r.first_index() {
                    let pe = r.log_entry(j).expect("entry in log");
                    if let Some(pc) = pe.conf_state() {
                        prev = Some(pc);
                        break;
                    }
                    j -= 1;
                }
                if let Some(pc) = prev {
                    check_transition(&pc, &c);
                }
            }
        }
    }

    fn record_commit(
        &mut self,
        index: u64,
        term: u64,
        hash: u64,
        kind: EntryKind,
        conf: Option<ConfState>,
        cterm: u64,
    ) {
        match self.canon.get_mut(&index) {
            Some(c) => {
                assert!(
                    c.term == term && c.hash == hash,
                    "committed entry {index} changed"
                );
                c.commit_term = c.commit_term.min(cterm);
            }
            None => {
                self.stats.committed += 1;
                // リーダーの完全性: これより後の term のリーダーはこの entry を持つ。
                for n in self.nodes.values() {
                    if let Some(r) = &n.raft
                        && r.is_leader()
                        && r.term() > cterm
                        && index >= r.first_index()
                    {
                        assert_eq!(
                            r.log_term(index),
                            Some(term),
                            "leader completeness: leader {} lacks {index}",
                            r.id()
                        );
                    }
                }
                self.canon.insert(
                    index,
                    CEntry {
                        term,
                        hash,
                        kind,
                        conf,
                        commit_term: cterm,
                    },
                );
            }
        }
    }

    pub fn check(&mut self) {
        let ids: Vec<NodeId> = self.live_ids();
        for id in ids {
            let r = self.nodes[&id].raft.as_ref().unwrap();
            let (role, term, commit) = (r.role(), r.term(), r.committed());
            if role == Role::Leader {
                let l = *self.leaders.entry(term).or_insert(id);
                assert_eq!(l, id, "election safety: two leaders in term {term}");
                if self.last_role.get(&id) != Some(&Role::Leader) {
                    self.stats.elections += 1;
                    // リーダーの完全性（就任時）
                    for (i, c) in self.canon.range(r.first_index()..) {
                        if c.commit_term < term {
                            assert_eq!(
                                r.log_term(*i),
                                Some(c.term),
                                "leader completeness: new leader {id} lacks {i}"
                            );
                        }
                    }
                }
            }
            self.last_role.insert(id, role);
            let old = self.last_commit.get(&id).copied().unwrap_or(0);
            if commit > old {
                let mut recs = Vec::new();
                for i in old + 1..=commit {
                    if let Some(e) = r.log_entry(i) {
                        recs.push((i, e.term, entry_hash(e), e.kind, e.conf_state()));
                    }
                }
                for (i, t, h, k, c) in recs {
                    self.record_commit(i, t, h, k, c, term);
                }
                self.max_commit = self.max_commit.max(commit);
            }
            self.last_commit.insert(id, commit);
        }
    }

    /// 読み取り `ctx` が許可され、値が返されたか。
    pub fn read_served(&self, ctx: u64) -> bool {
        !self.reads.contains_key(&ctx)
    }

    /// 全体の commit 済みの entry の (term, hash)。
    pub fn committed_entry(&self, index: u64) -> Option<(u64, u64)> {
        self.canon.get(&index).map(|c| (c.term, c.hash))
    }

    /// 生きているノードで、`index` 以上を適用していないものがなくなるまで進める。
    pub fn run_until(&mut self, bound: u64, mut f: impl FnMut(&Sim) -> bool) -> bool {
        for _ in 0..bound {
            if f(self) {
                return true;
            }
            self.step_tick(false);
        }
        f(self)
    }

    /// ACK 済みの書込みはすべて commit 済みの列に残っている（失われない・書き換わらない）。
    pub fn check_acked(&self) {
        for (i, t, h) in &self.acked {
            let c = self
                .canon
                .get(i)
                .unwrap_or_else(|| panic!("acked write {i} not committed"));
            assert!(c.term == *t && c.hash == *h, "acked write {i} was replaced");
        }
    }

    /// commit 済みの構成の列が C → C_old,new → C_new の形になっている。
    pub fn check_conf_history(&self) {
        let mut prev = ConfState::new(1..=self.p.voters as u64);
        for c in self.canon.values() {
            if let Some(conf) = &c.conf {
                check_transition(&prev, conf);
                prev = conf.clone();
            }
        }
    }

    // ---- 負荷 ----

    pub fn next_value(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }

    /// `id` で提案する（リーダーでなければ hint へ 1 回転送）。
    pub fn propose_at(&mut self, id: NodeId, data: Bytes) -> Result<Proposal, ProposeError> {
        let r = self.raft_mut(id);
        let res = r.propose(data.clone());
        if let Ok(p) = res {
            self.nodes.get_mut(&id).unwrap().proposals.push((p, data));
            self.stats.proposed += 1;
        }
        self.process(id);
        self.check();
        res
    }

    pub fn propose_kv(&mut self, id: NodeId, k: &str) -> Result<Proposal, ProposeError> {
        let v = self.next_value();
        self.propose_at(id, Bytes::from(format!("{k}={v}")))
    }

    pub fn read_at(&mut self, id: NodeId, key: &str) -> Result<u64, ProposeError> {
        let ctx = self.next_ctx;
        self.next_ctx += 1;
        self.reads.insert(
            ctx,
            ReadInfo {
                node: id,
                min_index: self.max_commit,
                key: key.to_string(),
            },
        );
        let res = self.raft_mut(id).read_index(ctx);
        if res.is_err() {
            self.reads.remove(&ctx);
        }
        self.process(id);
        self.check();
        res.map(|_| ctx)
    }

    fn target(&mut self) -> Option<NodeId> {
        let live = self.live_ids();
        let id = self.rng.pick(&live)?;
        let r = self.raft(id);
        if r.is_leader() {
            return Some(id);
        }
        r.leader().filter(|l| self.alive(*l))
    }

    fn workload(&mut self) {
        let p = self.p.clone();
        if self.rng.permille(p.propose)
            && let Some(id) = self.target()
        {
            let k = format!("k{}", self.rng.below(8));
            let _ = self.propose_kv(id, &k);
        }
        if self.rng.permille(p.read)
            && let Some(id) = self.target()
        {
            let k = format!("k{}", self.rng.below(8));
            let _ = self.read_at(id, &k);
        }
        if self.rng.permille(p.conf_change)
            && let Some(id) = self.leader()
        {
            self.random_conf_change(id);
        }
        if self.rng.permille(p.transfer)
            && let Some(id) = self.leader()
        {
            let voters: Vec<u64> = self.raft(id).conf_state().voters.iter().copied().collect();
            if let Some(to) = self.rng.pick(&voters) {
                if self.raft_mut(id).transfer_leader(to).is_ok() {
                    self.stats.transfers += 1;
                }
                self.process(id);
                self.check();
            }
        }
        for id in self.live_ids() {
            if self.rng.permille(p.compact) {
                let n = self.nodes.get_mut(&id).unwrap();
                let r = n.raft.as_mut().unwrap();
                if n.kv.applied >= r.first_index() + 20
                    && let Ok(snap) = r.compact(n.kv.applied, n.kv.encode())
                {
                    n.store.st().save_snapshot(&snap).unwrap();
                }
            }
        }
        if self.rng.permille(p.crash)
            && let Some(id) = self.rng.pick(&self.live_ids())
        {
            self.kill(id);
        }
        for id in self.ids() {
            if !self.alive(id) && self.rng.permille(p.restart) {
                self.start(id);
                self.process(id);
                self.check();
            }
        }
        if self.rng.permille(p.partition) {
            self.random_partition();
        }
    }

    fn random_conf_change(&mut self, leader: NodeId) {
        let conf = self.raft(leader).conf_state().clone();
        let pool = self.ids();
        let id = self.rng.pick(&pool).unwrap();
        let mut ops = Vec::new();
        if conf.voters.contains(&id) {
            if conf.voters.len() > 1 {
                ops.push(ConfChangeOp::RemoveVoter(id));
            }
        } else if conf.learners.contains(&id) {
            ops.push(if self.rng.permille(700) {
                ConfChangeOp::AddVoter(id)
            } else {
                ConfChangeOp::RemoveLearner(id)
            });
        } else {
            ops.push(if self.rng.permille(500) {
                ConfChangeOp::AddLearner(id)
            } else {
                ConfChangeOp::AddVoter(id)
            });
        }
        // ときどき 2 つまとめる（投票ノードを 1 つ足して 1 つ外す）
        if self.rng.permille(300) {
            let other = self.rng.pick(&pool).unwrap();
            if other != id && !conf.voters.contains(&other) && !conf.learners.contains(&other) {
                ops.push(ConfChangeOp::AddVoter(other));
            }
        }
        if ops.is_empty() {
            return;
        }
        if self.raft_mut(leader).propose_conf_change(&ops).is_ok() {
            self.stats.conf_changes += 1;
        }
        self.process(leader);
        self.check();
    }

    fn random_partition(&mut self) {
        let ids = self.ids();
        match self.rng.below(5) {
            0 | 1 => self.heal(),
            2 => {
                let id = self.rng.pick(&ids).unwrap();
                self.heal();
                self.isolate(id);
            }
            3 => {
                let mut a = Vec::new();
                let mut b = Vec::new();
                for id in ids {
                    if self.rng.permille(500) {
                        a.push(id)
                    } else {
                        b.push(id)
                    }
                }
                self.partition(&[a, b]);
            }
            _ => {
                // 片方向
                for _ in 0..1 + self.rng.below(3) {
                    let x = self.rng.pick(&ids).unwrap();
                    let y = self.rng.pick(&ids).unwrap();
                    if x != y {
                        self.blocked.insert((x, y));
                    }
                }
            }
        }
    }

    /// 構成の全員（生きているもの）が `index` を同じ term で適用したか。
    fn all_applied(&self, members: &BTreeSet<NodeId>, p: Proposal) -> bool {
        members.iter().all(|m| {
            let n = &self.nodes[m];
            n.raft.is_some() && n.kv.applied >= p.index
        }) && self.canon.get(&p.index).is_some_and(|c| c.term == p.term)
    }

    /// 分断を直し、全ノードを起こし、`bound` tick 以内にリーダーが決まり、新しい提案が構成の全員に
    /// 適用されることを確かめる。かかった tick 数を返す。
    pub fn heal_and_check_liveness(&mut self, bound: u64) -> u64 {
        self.heal();
        self.loss = 0;
        self.dup = 0;
        let saved = (self.p.crash, self.p.crash_in_persist);
        self.p.crash = 0;
        self.p.crash_in_persist = 0;
        for id in self.ids() {
            if !self.alive(id) {
                self.start(id);
                self.process(id);
            }
        }
        let start = self.now;
        let mut marker: Option<Proposal> = None;
        let mut t = 0;
        let ok = loop {
            if t > bound {
                break false;
            }
            self.step_tick(false);
            t += 1;
            if let Some(p) = marker
                && let Some(l) = self.leader()
            {
                let members = self.raft(l).conf_state().all_ids();
                if self.all_applied(&members, p) && !self.raft(l).conf_state().is_joint() {
                    break true;
                }
                let lt = self.raft(l).log_term(p.index);
                let lost = self.canon.get(&p.index).is_some_and(|c| c.term != p.term)
                    || (lt.is_some() && lt != Some(p.term))
                    || (lt.is_none() && p.index > self.raft(l).last_index());
                if lost {
                    marker = None; // 消えた → 出し直す
                }
            }
            if marker.is_none()
                && let Some(l) = self.leader()
            {
                let v = self.next_value();
                match self.propose_at(l, Bytes::from(format!("marker={v}"))) {
                    Ok(p) => marker = Some(p),
                    Err(e) => {
                        if std::env::var("SIM_DEBUG").is_ok() {
                            eprintln!("marker propose at {l}: {e:?}")
                        }
                    }
                }
            }
        };
        (self.p.crash, self.p.crash_in_persist) = saved;
        self.loss = self.p.loss;
        self.dup = self.p.dup;
        if !ok {
            self.dump();
        }
        assert!(
            ok,
            "liveness: no progress within {bound} ticks after heal (now {})",
            self.now
        );
        (self.now - start) / TICK
    }
}

impl Sim {
    pub fn dump(&self) {
        for n in self.nodes.values() {
            match &n.raft {
                Some(r) => eprintln!(
                    "node {}: {:?} kv.applied={}",
                    n.id,
                    r.status(),
                    n.kv.applied
                ),
                None => eprintln!("node {}: down", n.id),
            }
        }
        eprintln!("marker/canon last: {:?}", self.canon.keys().last());
    }
}

/// 構成の変化が joint consensus の形か。
pub fn check_transition(prev: &ConfState, next: &ConfState) {
    if prev.is_joint() {
        assert!(!next.is_joint(), "joint -> joint: {prev:?} -> {next:?}");
        assert_eq!(
            next.voters, prev.voters,
            "leave joint must keep incoming voters"
        );
        assert_eq!(next.learners, prev.learners);
    } else if next.is_joint() {
        assert_eq!(
            next.voters_outgoing, prev.voters,
            "enter joint must keep old voters as outgoing"
        );
    } else {
        assert_eq!(
            next.voters, prev.voters,
            "voters changed without joint consensus: {prev:?} -> {next:?}"
        );
    }
}

/// 1 つの種で、混乱（chaos_ticks）→ 回復の確認 を `rounds` 回。
pub fn run_seed(seed: u64, p: Params, rounds: u32, chaos_ticks: u64) -> Stats {
    let mut sim = Sim::new(seed, p);
    for _ in 0..rounds {
        sim.run(chaos_ticks, true);
        sim.heal_and_check_liveness(400);
    }
    sim.check_conf_history();
    sim.check_acked();
    sim.stats.clone()
}
