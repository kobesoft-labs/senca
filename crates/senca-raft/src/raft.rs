//! Raft の核（sans-io）。
//!
//! 構成は「ログに書かれた時点で効く」（論文 4.3 / 4.1）。最新の構成 entry（なければ snapshot の構成）が
//! 現在の構成で、切り詰めで消えたら一つ前に戻る。joint（C_old,new）の entry が commit されたら、
//! リーダーが自動で C_new の entry を書く。C_new に入っていないリーダーは C_new の commit 後に退く。

use crate::config::{Config, ConfigError};
use crate::log::RaftLog;
use crate::message::{Message, MsgBody};
use crate::progress::{Progress, ProgressState};
use crate::quorum::{VoteResult, quorum_active, quorum_value, vote_result};
use crate::storage::InitialState;
use crate::types::*;
use bytes::{Bytes, BytesMut};
use std::collections::{BTreeMap, VecDeque};

/// 待たせておける読み取りの数の上限。
const MAX_PENDING_READS: usize = 1 << 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Follower,
    PreCandidate,
    Candidate,
    Leader,
}

/// 提案・読み取り・構成変更・移譲が受け付けられなかった理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposeError {
    /// リーダーではない。分かっていればリーダーの ID（呼び手はそこへ転送するか、利用者に返す）。
    NotLeader { leader: Option<NodeId> },
    /// entry が `max_entry_bytes` を超える。
    TooLarge,
    /// commit 前の量・待ちの数が上限（過負荷・分断中）。後でやり直す。
    Dropped,
    /// リーダー移譲中。
    TransferInProgress,
    /// 前の構成変更が終わっていない（または就任後の最初の commit 前）。後でやり直す。
    ConfChangePending,
    /// 構成変更・移譲の内容が不正。
    Invalid(&'static str),
}

impl std::fmt::Display for ProposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProposeError::NotLeader { leader: Some(l) } => write!(f, "not leader (leader is {l})"),
            ProposeError::NotLeader { leader: None } => write!(f, "not leader (leader unknown)"),
            ProposeError::TooLarge => write!(f, "entry too large"),
            ProposeError::Dropped => write!(f, "proposal dropped (flow control)"),
            ProposeError::TransferInProgress => write!(f, "leadership transfer in progress"),
            ProposeError::ConfChangePending => write!(f, "configuration change in progress"),
            ProposeError::Invalid(w) => write!(f, "invalid request: {w}"),
        }
    }
}

impl std::error::Error for ProposeError {}

/// 入力の誤り（状態は変えない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepError {
    /// 宛先が自分ではない。
    WrongTarget,
    /// メッセージの中身が矛盾している（壊れた・悪意ある相手）。
    Malformed(&'static str),
    /// 呼び出しが不正（compact の位置など）。
    Invalid(&'static str),
}

impl std::fmt::Display for StepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StepError::WrongTarget => write!(f, "message is not addressed to this node"),
            StepError::Malformed(w) => write!(f, "malformed message: {w}"),
            StepError::Invalid(w) => write!(f, "invalid call: {w}"),
        }
    }
}

impl std::error::Error for StepError {}

/// 提案がログに入った位置。この index の entry が同じ term で適用されたら成功、別の term なら失敗（消えた）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Proposal {
    pub index: u64,
    pub term: u64,
}

/// 線形化可能な読み取りの許可。適用済み index が `index` 以上になったら状態機械を読んでよい。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadState {
    pub index: u64,
    pub ctx: u64,
}

/// 呼び手が処理すること（順番を守る）:
/// 1. `snapshot`・`entries`・`hard_state` を永続化して fsync（[`crate::Storage::persist`]）。
/// 2. `messages` を送る（1 が終わる前に送ってはいけない）。
/// 3. `snapshot` があれば状態機械を置き換え、`committed_entries` を順に適用する。
/// 4. `read_states` を、適用済みが追いついたら返す。
#[derive(Debug, Default)]
pub struct Ready {
    pub hard_state: Option<HardState>,
    /// リーダーから受け取った snapshot。永続化し、状態機械を置き換える。
    pub snapshot: Option<Snapshot>,
    /// 追記する entry。記憶域は `entries[0].index` 以降を捨ててから足す。
    pub entries: Vec<Entry>,
    pub messages: Vec<Message>,
    pub committed_entries: Vec<Entry>,
    pub read_states: Vec<ReadState>,
}

impl Ready {
    /// fsync が要るか。
    pub fn must_sync(&self) -> bool {
        self.hard_state.is_some() || self.snapshot.is_some() || !self.entries.is_empty()
    }
}

/// 観察用の状態。
#[derive(Debug, Clone)]
pub struct Status {
    pub id: NodeId,
    pub term: u64,
    pub vote: NodeId,
    pub role: Role,
    pub leader: Option<NodeId>,
    pub commit: u64,
    pub applied: u64,
    pub first_index: u64,
    pub last_index: u64,
    pub last_term: u64,
    pub conf: ConfState,
    pub conf_index: u64,
    pub transferee: Option<NodeId>,
}

#[derive(Debug)]
struct PendingRead {
    seq: u64,
    index: u64,
    ctx: u64,
}

#[derive(Debug)]
struct IncomingSnapshot {
    meta: SnapshotMeta,
    total: u64,
    buf: BytesMut,
}

/// Raft のノード（sans-io）。
#[derive(Debug)]
pub struct RawNode {
    id: NodeId,
    cfg: Config,
    term: u64,
    vote: NodeId,
    lead: NodeId,
    role: Role,
    log: RaftLog,
    conf: ConfState,
    conf_index: u64,
    prs: BTreeMap<NodeId, Progress>,
    votes: BTreeMap<NodeId, bool>,
    election_elapsed: u32,
    heartbeat_elapsed: u32,
    rand_timeout: u32,
    rng: u64,
    /// 起動直後は、リーダーを知らなくても election_tick の間は投票に応じない（lease の安全のため）。
    startup_guard: bool,
    now: u64,
    msgs: Vec<Message>,
    read_states: Vec<ReadState>,
    reads_wait_commit: Vec<u64>,
    pending_reads: VecDeque<PendingRead>,
    read_seq: u64,
    seq_sent: VecDeque<(u64, u64)>,
    lease_until: u64,
    lease_ok_this_term: bool,
    transferee: Option<NodeId>,
    transfer_elapsed: u32,
    uncommitted_bytes: u64,
    last_hs: HardState,
    incoming_snapshot: Option<IncomingSnapshot>,
}

fn splitmix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl RawNode {
    /// 記憶域から読んだ状態で作る。新しいクラスタは投票ノードの記憶域に [`Snapshot::bootstrap`] を
    /// 書いてから読む。後から加わるノードは空の状態で起動し、リーダーからの複製を待つ。
    pub fn new(cfg: Config, init: InitialState) -> Result<RawNode, ConfigError> {
        cfg.validate()?;
        let InitialState {
            hard_state,
            snapshot,
            entries,
        } = init;
        let (meta, snap_data) = match snapshot {
            Some(s) => {
                let keep = if s.meta.index > 0 || !s.data.is_empty() {
                    Some(s.clone())
                } else {
                    None
                };
                (s.meta, keep)
            }
            None => (SnapshotMeta::default(), None),
        };
        for (i, e) in entries.iter().enumerate() {
            if e.index != meta.index + 1 + i as u64 {
                return Err(ConfigError(
                    "initial log is not contiguous with the snapshot",
                ));
            }
            if (i == 0 && e.term < meta.term) || (i > 0 && e.term < entries[i - 1].term) {
                return Err(ConfigError("initial log terms decrease"));
            }
        }
        let last_index = meta.index + entries.len() as u64;
        let last_term = entries.last().map(|e| e.term).unwrap_or(meta.term);
        let commit = hard_state.commit.max(meta.index);
        if commit > last_index {
            return Err(ConfigError("hard state commit is beyond the log"));
        }
        let applied = cfg.applied.max(meta.index);
        if applied > commit {
            return Err(ConfigError("applied index is beyond commit"));
        }
        let (term, vote) = if last_term > hard_state.term {
            (last_term, 0)
        } else {
            (hard_state.term, hard_state.vote)
        };
        let log = RaftLog {
            snap_index: meta.index,
            snap_term: meta.term,
            snap_conf: meta.conf_state.clone(),
            snapshot: snap_data,
            entries,
            committed: commit,
            applied,
            unstable_from: last_index + 1,
            pending_snapshot: None,
        };
        let (conf, conf_index) = log.conf_at(last_index);
        let mut rng = cfg.seed ^ 0x5EC4_0000_0000_0000 ^ cfg.id.rotate_left(32);
        splitmix(&mut rng);
        let mut n = RawNode {
            id: cfg.id,
            term,
            vote,
            lead: 0,
            role: Role::Follower,
            log,
            conf,
            conf_index,
            prs: BTreeMap::new(),
            votes: BTreeMap::new(),
            election_elapsed: 0,
            heartbeat_elapsed: 0,
            rand_timeout: cfg.election_tick,
            rng,
            startup_guard: true,
            now: 0,
            msgs: Vec::new(),
            read_states: Vec::new(),
            reads_wait_commit: Vec::new(),
            pending_reads: VecDeque::new(),
            read_seq: 0,
            seq_sent: VecDeque::new(),
            lease_until: 0,
            lease_ok_this_term: false,
            transferee: None,
            transfer_elapsed: 0,
            uncommitted_bytes: 0,
            last_hs: hard_state,
            incoming_snapshot: None,
            cfg,
        };
        n.reset_timeout();
        Ok(n)
    }

    // ---- 観察 ----

    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn term(&self) -> u64 {
        self.term
    }

    pub fn leader(&self) -> Option<NodeId> {
        (self.lead != 0).then_some(self.lead)
    }

    pub fn is_leader(&self) -> bool {
        self.role == Role::Leader
    }

    pub fn conf_state(&self) -> &ConfState {
        &self.conf
    }

    pub fn committed(&self) -> u64 {
        self.log.committed
    }

    pub fn first_index(&self) -> u64 {
        self.log.first_index()
    }

    pub fn last_index(&self) -> u64 {
        self.log.last_index()
    }

    pub fn log_term(&self, index: u64) -> Option<u64> {
        self.log.term(index)
    }

    pub fn log_entry(&self, index: u64) -> Option<&Entry> {
        self.log.entry(index)
    }

    pub fn status(&self) -> Status {
        Status {
            id: self.id,
            term: self.term,
            vote: self.vote,
            role: self.role,
            leader: self.leader(),
            commit: self.log.committed,
            applied: self.log.applied,
            first_index: self.log.first_index(),
            last_index: self.log.last_index(),
            last_term: self.log.last_term(),
            conf: self.conf.clone(),
            conf_index: self.conf_index,
            transferee: self.transferee,
        }
    }

    /// リーダーで、自分の term の entry を commit 済みなら、その時点の (term, commit) を返す。
    /// 同じクラスタで、後のリーダーの token は必ず大きい（term が大きい）。
    pub fn leader_token(&self) -> Option<FencingToken> {
        (self.role == Role::Leader && self.log.term(self.log.committed) == Some(self.term))
            .then_some(FencingToken {
                term: self.term,
                index: self.log.committed,
            })
    }

    // ---- 入力 ----

    /// 時間を 1 つ進める。
    pub fn tick(&mut self) {
        self.now += 1;
        if self.transferee.is_some() {
            self.transfer_elapsed += 1;
            if self.transfer_elapsed >= self.cfg.election_tick {
                self.transferee = None;
            }
        }
        if self.role == Role::Leader {
            self.tick_leader();
        } else {
            self.tick_election();
        }
    }

    /// 利用者のコマンドを提案する。
    pub fn propose(&mut self, data: Bytes) -> Result<Proposal, ProposeError> {
        self.check_proposable()?;
        if data.len() as u64 > self.cfg.max_entry_bytes {
            return Err(ProposeError::TooLarge);
        }
        let size = data.len() as u64 + 24;
        if self.uncommitted_bytes > 0
            && self.uncommitted_bytes + size > self.cfg.max_uncommitted_bytes
        {
            return Err(ProposeError::Dropped);
        }
        let p = self.append_as_leader(vec![(EntryKind::Normal, data)]);
        self.bcast_append();
        Ok(p)
    }

    /// 構成を変える（joint consensus）。投票ノードが変わるなら C_old,new を書き、commit 後に自動で C_new を書く。
    /// learner だけの変更は 1 段で書く。新しいノードは learner で加えて追いつかせてから AddVoter で上げるとよい。
    pub fn propose_conf_change(&mut self, ops: &[ConfChangeOp]) -> Result<Proposal, ProposeError> {
        self.check_proposable()?;
        if self.log.term(self.log.committed) != Some(self.term)
            || self.conf.is_joint()
            || self.conf_index > self.log.committed
        {
            return Err(ProposeError::ConfChangePending);
        }
        let mut voters = self.conf.voters.clone();
        let mut learners = self.conf.learners.clone();
        for op in ops {
            match *op {
                ConfChangeOp::AddVoter(id) => {
                    if id == 0 {
                        return Err(ProposeError::Invalid("node id 0"));
                    }
                    learners.remove(&id);
                    voters.insert(id);
                }
                ConfChangeOp::RemoveVoter(id) => {
                    if !voters.remove(&id) {
                        return Err(ProposeError::Invalid("not a voter"));
                    }
                }
                ConfChangeOp::AddLearner(id) => {
                    if id == 0 {
                        return Err(ProposeError::Invalid("node id 0"));
                    }
                    if voters.contains(&id) {
                        return Err(ProposeError::Invalid("already a voter"));
                    }
                    learners.insert(id);
                }
                ConfChangeOp::RemoveLearner(id) => {
                    if !learners.remove(&id) {
                        return Err(ProposeError::Invalid("not a learner"));
                    }
                }
            }
        }
        if voters.len() > crate::codec::MAX_IDS || learners.len() > crate::codec::MAX_IDS {
            return Err(ProposeError::Invalid("too many nodes"));
        }
        let new = if voters == self.conf.voters {
            ConfState {
                voters,
                voters_outgoing: Default::default(),
                learners,
            }
        } else {
            ConfState {
                voters,
                voters_outgoing: self.conf.voters.clone(),
                learners,
            }
        };
        new.validate().map_err(ProposeError::Invalid)?;
        if new == self.conf {
            return Err(ProposeError::Invalid("no change"));
        }
        let data = crate::codec::encode_conf_state(&new);
        let p = self.append_as_leader(vec![(EntryKind::ConfChange, data)]);
        self.bcast_append();
        Ok(p)
    }

    /// すぐに選挙を始める（PreVote が有効ならその手順で）。通常は tick に任せる。運用の手動切替や試験用。
    pub fn campaign(&mut self) {
        self.hup(false);
    }

    /// 線形化可能な読み取りを始める（ReadIndex）。許可は `Ready::read_states` に `ctx` 付きで出る。
    /// リーダーが替わった・退いた場合は出ない（呼び手は時間切れで扱う）。
    pub fn read_index(&mut self, ctx: u64) -> Result<(), ProposeError> {
        if self.role != Role::Leader {
            return Err(ProposeError::NotLeader {
                leader: self.leader(),
            });
        }
        if self.pending_reads.len() + self.reads_wait_commit.len() >= MAX_PENDING_READS {
            return Err(ProposeError::Dropped);
        }
        if self.log.term(self.log.committed) != Some(self.term) {
            // 就任後、自分の term の entry が commit されるまでは commit index が最新とは限らない。
            self.reads_wait_commit.push(ctx);
            return Ok(());
        }
        self.start_reads(vec![ctx]);
        Ok(())
    }

    /// リーダーを `to` に移す。`to` が追いついたら TimeoutNow を送る。election_tick で諦める。
    pub fn transfer_leader(&mut self, to: NodeId) -> Result<(), ProposeError> {
        if self.role != Role::Leader {
            return Err(ProposeError::NotLeader {
                leader: self.leader(),
            });
        }
        if to == self.id || self.transferee == Some(to) {
            return Ok(());
        }
        if !self.conf.voters.contains(&to) {
            return Err(ProposeError::Invalid("transferee is not a voter"));
        }
        self.transferee = Some(to);
        self.transfer_elapsed = 0;
        // 移譲の選挙は lease を無視するので、この term では lease read をやめる。
        self.lease_ok_this_term = false;
        let caught_up = self
            .prs
            .get(&to)
            .is_some_and(|p| p.match_ == self.log.last_index());
        if caught_up {
            self.send(to, self.term, MsgBody::TimeoutNow);
        } else {
            self.send_append(to, true);
        }
        Ok(())
    }

    /// 適用済みの `index` までを snapshot `data` で置き換えてログを詰める。返り値を記憶域に保存する
    /// （[`crate::Storage::save_snapshot`]）。`index` は呼び手の状態機械が適用済みであること。
    pub fn compact(&mut self, index: u64, data: Bytes) -> Result<Snapshot, StepError> {
        if index <= self.log.snap_index {
            return Err(StepError::Invalid("already compacted"));
        }
        if index > self.log.applied {
            return Err(StepError::Invalid("index is not applied"));
        }
        let term = self
            .log
            .term(index)
            .ok_or(StepError::Invalid("index out of range"))?;
        let (conf, _) = self.log.conf_at(index);
        let snap = Snapshot {
            meta: SnapshotMeta {
                index,
                term,
                conf_state: conf,
            },
            data,
        };
        self.log.compact(snap.clone());
        Ok(snap)
    }

    /// 相手からのメッセージを処理する。
    pub fn step(&mut self, m: Message) -> Result<(), StepError> {
        if m.to != self.id {
            return Err(StepError::WrongTarget);
        }
        if m.from == self.id || m.from == 0 {
            return Err(StepError::Malformed("bad sender"));
        }
        if let MsgBody::Append {
            prev_index,
            prev_term,
            entries,
            ..
        } = &m.body
        {
            let mut t = *prev_term;
            for (i, e) in entries.iter().enumerate() {
                if Some(e.index) != prev_index.checked_add(1 + i as u64) {
                    return Err(StepError::Malformed("entries are not contiguous"));
                }
                if e.term < t || e.term > m.term {
                    return Err(StepError::Malformed("entry terms out of order"));
                }
                t = e.term;
            }
        }
        if m.term > self.term {
            if let MsgBody::PreVote { .. } | MsgBody::Vote { .. } = m.body {
                let force = matches!(m.body, MsgBody::Vote { transfer: true, .. });
                if !force && self.in_lease() {
                    // 健在なリーダーがいる間は、分断から戻ったノードなどの選挙に応じない。
                    return Ok(());
                }
            }
            match m.body {
                MsgBody::PreVote { .. } => {}
                MsgBody::PreVoteResp { reject: false } => {}
                MsgBody::Append { .. } | MsgBody::Heartbeat { .. } | MsgBody::Snapshot { .. } => {
                    self.become_follower(m.term, m.from)
                }
                _ => self.become_follower(m.term, 0),
            }
        } else if m.term < self.term {
            match m.body {
                MsgBody::Append { .. } | MsgBody::Heartbeat { .. } | MsgBody::Snapshot { .. } => {
                    // 古いリーダーに新しい term を知らせて退かせる。
                    self.send(
                        m.from,
                        self.term,
                        MsgBody::AppendResp {
                            reject: false,
                            index: 0,
                            reject_hint: 0,
                            hint_term: 0,
                        },
                    );
                }
                MsgBody::PreVote { .. } => {
                    self.send(m.from, self.term, MsgBody::PreVoteResp { reject: true })
                }
                // 遅れた候補者に今の term を知らせる（知らないままだと、term の低い候補者が当選できずに残る）。
                MsgBody::Vote { .. } => {
                    self.send(m.from, self.term, MsgBody::VoteResp { reject: true })
                }
                _ => {}
            }
            return Ok(());
        }

        match m.body {
            MsgBody::PreVote {
                last_index,
                last_term,
            } => self.handle_vote(m.from, m.term, true, last_index, last_term),
            MsgBody::Vote {
                last_index,
                last_term,
                ..
            } => self.handle_vote(m.from, m.term, false, last_index, last_term),
            _ => match self.role {
                Role::Leader => self.step_leader(m),
                Role::PreCandidate | Role::Candidate => self.step_candidate(m),
                Role::Follower => self.step_follower(m),
            },
        }
        Ok(())
    }

    // ---- Ready ----

    fn hard_state(&self) -> HardState {
        HardState {
            term: self.term,
            vote: self.vote,
            commit: self.log.committed,
        }
    }

    pub fn has_ready(&self) -> bool {
        !self.msgs.is_empty()
            || !self.read_states.is_empty()
            || self.log.pending_snapshot.is_some()
            || self.log.unstable_from <= self.log.last_index()
            || self.hard_state() != self.last_hs
            || self.log.applied < self.log.committed
    }

    /// 出力を取り出す（取り出した時点で「渡した」とみなす）。次の `ready` の前に、この Ready の
    /// 永続化・送信・適用を終えること。
    pub fn ready(&mut self) -> Option<Ready> {
        if !self.has_ready() {
            return None;
        }
        let mut rd = Ready::default();
        let hs = self.hard_state();
        if hs != self.last_hs {
            self.last_hs = hs;
            rd.hard_state = Some(hs);
        }
        rd.snapshot = self.log.pending_snapshot.take();
        let from = self.log.unstable_from.max(self.log.first_index());
        let last = self.log.last_index();
        if from <= last {
            rd.entries = self.log.slice(from, last, u64::MAX);
        }
        self.log.unstable_from = last + 1;
        if self.log.applied < self.log.committed {
            let ents = self.log.slice(
                self.log.applied + 1,
                self.log.committed,
                self.cfg.max_committed_bytes_per_ready,
            );
            if let Some(e) = ents.last() {
                self.log.applied = e.index;
            }
            rd.committed_entries = ents;
        }
        rd.messages = std::mem::take(&mut self.msgs);
        rd.read_states = std::mem::take(&mut self.read_states);
        Some(rd)
    }

    // ---- 内部 ----

    /// 選挙を始めてよいか。最新の構成の投票ノード、または自分を外す構成変更がまだ commit されていない
    /// （commit 済みの構成では投票ノード）とき。後者を許さないと、外される側のリーダーが C_new の commit 前に
    /// 退いたとき、C_new の entry を持つのが自分だけで誰も当選できなくなることがある。票は自分の最新の構成で
    /// 数える（自分は数えない）ので安全性は変わらない。
    fn promotable(&self) -> bool {
        self.conf.is_voter(self.id)
            || (self.conf_index > self.log.committed
                && self.log.conf_at(self.log.committed).0.is_voter(self.id))
    }

    fn in_lease(&self) -> bool {
        self.cfg.check_quorum
            && (self.lead != 0 || self.startup_guard)
            && self.election_elapsed < self.cfg.election_tick
    }

    fn check_proposable(&self) -> Result<(), ProposeError> {
        if self.role != Role::Leader {
            return Err(ProposeError::NotLeader {
                leader: self.leader(),
            });
        }
        if self.transferee.is_some() {
            return Err(ProposeError::TransferInProgress);
        }
        Ok(())
    }

    fn send(&mut self, to: NodeId, term: u64, body: MsgBody) {
        self.msgs.push(Message {
            from: self.id,
            to,
            term,
            body,
        });
    }

    fn reset_timeout(&mut self) {
        let et = self.cfg.election_tick;
        self.rand_timeout = et + (splitmix(&mut self.rng) % et as u64) as u32;
    }

    fn reset(&mut self, term: u64) {
        if self.term != term {
            self.term = term;
            self.vote = 0;
        }
        self.lead = 0;
        self.election_elapsed = 0;
        self.heartbeat_elapsed = 0;
        self.reset_timeout();
        self.transferee = None;
        self.votes.clear();
        self.prs.clear();
        self.pending_reads.clear();
        self.reads_wait_commit.clear();
        self.seq_sent.clear();
        self.lease_until = 0;
        self.lease_ok_this_term = false;
        self.uncommitted_bytes = 0;
    }

    fn become_follower(&mut self, term: u64, lead: NodeId) {
        self.reset(term);
        self.role = Role::Follower;
        self.lead = lead;
    }

    fn become_pre_candidate(&mut self) {
        self.role = Role::PreCandidate;
        self.lead = 0;
        self.votes.clear();
        self.prs.clear();
        self.pending_reads.clear();
        self.reads_wait_commit.clear();
        self.reset_timeout();
    }

    fn become_candidate(&mut self) {
        self.reset(self.term + 1);
        self.vote = self.id;
        self.role = Role::Candidate;
    }

    fn become_leader(&mut self) {
        self.reset(self.term);
        self.role = Role::Leader;
        self.lead = self.id;
        self.startup_guard = false;
        self.lease_ok_this_term = true;
        let next = self.log.last_index() + 1;
        for id in self.conf.all_ids() {
            self.prs.insert(id, Progress::new(next));
        }
        let me = self
            .prs
            .entry(self.id)
            .or_insert_with(|| Progress::new(next));
        me.match_ = next - 1;
        me.become_replicate();
        self.append_as_leader(vec![(EntryKind::Noop, Bytes::new())]);
        self.bcast_append();
    }

    fn tick_election(&mut self) {
        self.election_elapsed = self.election_elapsed.saturating_add(1);
        if self.election_elapsed >= self.cfg.election_tick {
            self.startup_guard = false;
        }
        if self.promotable() && self.election_elapsed >= self.rand_timeout {
            self.election_elapsed = 0;
            self.hup(false);
        }
    }

    fn tick_leader(&mut self) {
        self.heartbeat_elapsed += 1;
        self.election_elapsed += 1;
        if self.election_elapsed >= self.cfg.election_tick {
            self.election_elapsed = 0;
            if self.cfg.check_quorum {
                let id = self.id;
                let prs = &self.prs;
                let ok = quorum_active(&self.conf, |n| {
                    n == id || prs.get(&n).is_some_and(|p| p.recent_active)
                });
                if !ok {
                    self.become_follower(self.term, 0);
                    return;
                }
                for (n, p) in self.prs.iter_mut() {
                    if *n != id {
                        p.recent_active = false;
                    }
                }
            }
        }
        if self.heartbeat_elapsed >= self.cfg.heartbeat_tick {
            self.heartbeat_elapsed = 0;
            self.bcast_heartbeat();
        }
    }

    fn hup(&mut self, transfer: bool) {
        if self.role == Role::Leader || !self.promotable() {
            return;
        }
        if transfer || !self.cfg.pre_vote {
            self.run_campaign(false, transfer);
        } else {
            self.run_campaign(true, false);
        }
    }

    fn run_campaign(&mut self, pre: bool, transfer: bool) {
        let vote_term = if pre {
            self.become_pre_candidate();
            self.term + 1
        } else {
            self.become_candidate();
            self.term
        };
        self.votes.insert(self.id, true);
        if vote_result(&self.conf, &self.votes) == VoteResult::Won {
            if pre {
                self.run_campaign(false, false);
            } else {
                self.become_leader();
            }
            return;
        }
        let (li, lt) = (self.log.last_index(), self.log.last_term());
        for id in self.conf.all_voters() {
            if id == self.id {
                continue;
            }
            let body = if pre {
                MsgBody::PreVote {
                    last_index: li,
                    last_term: lt,
                }
            } else {
                MsgBody::Vote {
                    last_index: li,
                    last_term: lt,
                    transfer,
                }
            };
            self.send(id, vote_term, body);
        }
    }

    fn handle_vote(&mut self, from: NodeId, mterm: u64, pre: bool, li: u64, lt: u64) {
        let can_vote =
            self.vote == from || (self.vote == 0 && self.lead == 0) || (pre && mterm > self.term);
        if can_vote && self.log.is_up_to_date(li, lt) {
            if !pre {
                self.election_elapsed = 0;
                self.vote = from;
            }
            let body = if pre {
                MsgBody::PreVoteResp { reject: false }
            } else {
                MsgBody::VoteResp { reject: false }
            };
            self.send(from, mterm, body);
        } else {
            let body = if pre {
                MsgBody::PreVoteResp { reject: true }
            } else {
                MsgBody::VoteResp { reject: true }
            };
            self.send(from, self.term, body);
        }
    }

    fn step_candidate(&mut self, m: Message) {
        let granted = match m.body {
            MsgBody::Append { .. } | MsgBody::Heartbeat { .. } | MsgBody::Snapshot { .. } => {
                self.become_follower(m.term, m.from);
                self.step_follower(m);
                return;
            }
            MsgBody::PreVoteResp { reject } if self.role == Role::PreCandidate => {
                if !reject && m.term != self.term + 1 {
                    return;
                }
                !reject
            }
            MsgBody::VoteResp { reject } if self.role == Role::Candidate => !reject,
            _ => return,
        };
        self.votes.entry(m.from).or_insert(granted);
        match vote_result(&self.conf, &self.votes) {
            VoteResult::Won => {
                if self.role == Role::PreCandidate {
                    self.run_campaign(false, false);
                } else {
                    self.become_leader();
                }
            }
            VoteResult::Lost => self.become_follower(self.term, 0),
            VoteResult::Pending => {}
        }
    }

    fn step_follower(&mut self, m: Message) {
        match m.body {
            MsgBody::Append {
                prev_index,
                prev_term,
                entries,
                commit,
            } => {
                self.election_elapsed = 0;
                self.lead = m.from;
                self.handle_append(m.from, prev_index, prev_term, entries, commit);
            }
            MsgBody::Heartbeat { commit, read_seq } => {
                self.election_elapsed = 0;
                self.lead = m.from;
                self.log.commit_to(commit);
                self.send(m.from, self.term, MsgBody::HeartbeatResp { read_seq });
            }
            MsgBody::Snapshot {
                meta,
                total_len,
                offset,
                data,
                done: _,
            } => {
                self.election_elapsed = 0;
                self.lead = m.from;
                self.handle_snapshot(m.from, meta, total_len, offset, data);
            }
            MsgBody::TimeoutNow if self.promotable() => self.hup(true),
            _ => {}
        }
    }

    fn handle_append(
        &mut self,
        from: NodeId,
        prev_index: u64,
        prev_term: u64,
        entries: Vec<Entry>,
        commit: u64,
    ) {
        let committed = self.log.committed;
        if prev_index < committed {
            self.send(
                from,
                self.term,
                MsgBody::AppendResp {
                    reject: false,
                    index: committed,
                    reject_hint: 0,
                    hint_term: 0,
                },
            );
            return;
        }
        if self.log.match_term(prev_index, prev_term) {
            let last_new = prev_index + entries.len() as u64;
            let ci = self.log.find_conflict(&entries);
            if ci != 0 {
                if ci <= committed {
                    // 安全性の破れ（起こらないはず）。commit 済みは書き換えない。
                    debug_assert!(false, "conflict with committed entry");
                    return;
                }
                let off = (ci - prev_index - 1) as usize;
                self.log.truncate_append(&entries[off..]);
                self.refresh_conf(ci);
            }
            self.log.commit_to(commit.min(last_new));
            self.send(
                from,
                self.term,
                MsgBody::AppendResp {
                    reject: false,
                    index: last_new,
                    reject_hint: 0,
                    hint_term: 0,
                },
            );
        } else {
            let hint = self
                .log
                .find_conflict_by_term(prev_index.min(self.log.last_index()), prev_term);
            let hint_term = self.log.term(hint).unwrap_or(0);
            self.send(
                from,
                self.term,
                MsgBody::AppendResp {
                    reject: true,
                    index: prev_index,
                    reject_hint: hint,
                    hint_term,
                },
            );
        }
    }

    fn handle_snapshot(
        &mut self,
        from: NodeId,
        meta: SnapshotMeta,
        total: u64,
        offset: u64,
        data: Bytes,
    ) {
        let (si, st) = (meta.index, meta.term);
        if si <= self.log.committed {
            self.incoming_snapshot = None;
            let c = self.log.committed;
            self.send(
                from,
                self.term,
                MsgBody::AppendResp {
                    reject: false,
                    index: c,
                    reject_hint: 0,
                    hint_term: 0,
                },
            );
            return;
        }
        if total > self.cfg.max_snapshot_bytes || meta.conf_state.validate().is_err() {
            self.send(
                from,
                self.term,
                MsgBody::SnapshotResp {
                    snap_index: si,
                    snap_term: st,
                    next_offset: 0,
                    reject: true,
                },
            );
            return;
        }
        let same = self
            .incoming_snapshot
            .as_ref()
            .is_some_and(|i| i.meta.index == si && i.meta.term == st && i.total == total);
        if !same {
            if offset != 0 {
                self.incoming_snapshot = None;
                self.send(
                    from,
                    self.term,
                    MsgBody::SnapshotResp {
                        snap_index: si,
                        snap_term: st,
                        next_offset: 0,
                        reject: false,
                    },
                );
                return;
            }
            self.incoming_snapshot = Some(IncomingSnapshot {
                meta,
                total,
                buf: BytesMut::new(),
            });
        }
        let Some(inc) = self.incoming_snapshot.as_mut() else {
            return;
        };
        if offset == inc.buf.len() as u64 && inc.buf.len() as u64 + data.len() as u64 <= inc.total {
            inc.buf.extend_from_slice(&data);
        }
        if inc.buf.len() as u64 == inc.total {
            let Some(inc) = self.incoming_snapshot.take() else {
                return;
            };
            let snap = Snapshot {
                meta: inc.meta,
                data: inc.buf.freeze(),
            };
            let index = if self.restore(snap) {
                self.log.last_index()
            } else {
                self.log.committed
            };
            self.send(
                from,
                self.term,
                MsgBody::AppendResp {
                    reject: false,
                    index,
                    reject_hint: 0,
                    hint_term: 0,
                },
            );
        } else {
            let next = inc.buf.len() as u64;
            self.send(
                from,
                self.term,
                MsgBody::SnapshotResp {
                    snap_index: si,
                    snap_term: st,
                    next_offset: next,
                    reject: false,
                },
            );
        }
    }

    fn restore(&mut self, snap: Snapshot) -> bool {
        if snap.meta.index <= self.log.committed {
            return false;
        }
        if self.log.match_term(snap.meta.index, snap.meta.term) {
            // 同じ entry を持っているので commit を進めるだけ（後ろの entry を普通に適用する）。
            self.log.commit_to(snap.meta.index);
            return false;
        }
        self.log.restore(snap);
        let (c, ci) = self.log.conf_at(self.log.last_index());
        self.conf = c;
        self.conf_index = ci;
        true
    }

    /// ログが `at` 以降で変わった後に構成を更新する。
    fn refresh_conf(&mut self, at: u64) {
        let before = self.conf.clone();
        if at <= self.conf_index {
            let (c, ci) = self.log.conf_at(self.log.last_index());
            self.conf = c;
            self.conf_index = ci;
        } else {
            for i in at..=self.log.last_index() {
                if let Some(c) = self.log.entry(i).and_then(|e| e.conf_state()) {
                    self.conf = c;
                    self.conf_index = i;
                }
            }
        }
        if self.role == Role::Leader && self.conf != before {
            self.sync_progress();
        }
    }

    fn sync_progress(&mut self) {
        let ids = self.conf.all_ids();
        let next = self.log.last_index() + 1;
        for id in &ids {
            self.prs.entry(*id).or_insert_with(|| Progress::new(next));
        }
        let me = self.id;
        self.prs.retain(|id, _| ids.contains(id) || *id == me);
        if self
            .transferee
            .is_some_and(|t| !self.conf.voters.contains(&t))
        {
            self.transferee = None;
        }
        self.try_release_reads();
    }

    fn append_as_leader(&mut self, items: Vec<(EntryKind, Bytes)>) -> Proposal {
        let mut idx = self.log.last_index();
        let ents: Vec<Entry> = items
            .into_iter()
            .map(|(kind, data)| {
                idx += 1;
                Entry {
                    term: self.term,
                    index: idx,
                    kind,
                    data,
                }
            })
            .collect();
        for e in &ents {
            self.uncommitted_bytes += e.size();
        }
        let at = ents[0].index;
        self.log.truncate_append(&ents);
        self.refresh_conf(at);
        if let Some(me) = self.prs.get_mut(&self.id) {
            me.maybe_update(idx);
        }
        self.maybe_commit();
        Proposal {
            index: idx,
            term: self.term,
        }
    }

    fn maybe_commit(&mut self) -> bool {
        if self.role != Role::Leader {
            return false;
        }
        let prs = &self.prs;
        let mci = quorum_value(&self.conf, |id| prs.get(&id).map(|p| p.match_).unwrap_or(0));
        if mci > self.log.committed && self.log.term(mci) == Some(self.term) {
            let old = self.log.committed;
            self.log.commit_to(mci);
            self.on_commit_advanced(old);
            true
        } else {
            false
        }
    }

    fn on_commit_advanced(&mut self, old: u64) {
        for i in old + 1..=self.log.committed {
            if let Some(e) = self.log.entry(i) {
                self.uncommitted_bytes = self.uncommitted_bytes.saturating_sub(e.size());
            }
        }
        if self.role != Role::Leader {
            return;
        }
        if !self.reads_wait_commit.is_empty()
            && self.log.term(self.log.committed) == Some(self.term)
        {
            let ctxs = std::mem::take(&mut self.reads_wait_commit);
            self.start_reads(ctxs);
        }
        if self.conf_index <= self.log.committed {
            if self.conf.is_joint() {
                // C_old,new が commit された → C_new を書く。
                let new = ConfState {
                    voters: self.conf.voters.clone(),
                    voters_outgoing: Default::default(),
                    learners: self.conf.learners.clone(),
                };
                let data = crate::codec::encode_conf_state(&new);
                self.append_as_leader(vec![(EntryKind::ConfChange, data)]);
                self.bcast_append();
            } else if !self.conf.voters.contains(&self.id) {
                // C_new に入っていないリーダーは退く（commit を知らせてから）。
                self.bcast_append();
                self.become_follower(self.term, 0);
            }
        }
    }

    fn bcast_append(&mut self) {
        if self.role != Role::Leader {
            return;
        }
        let ids: Vec<NodeId> = self.prs.keys().copied().filter(|&i| i != self.id).collect();
        for id in ids {
            self.send_append(id, true);
        }
    }

    fn send_append(&mut self, to: NodeId, send_if_empty: bool) -> bool {
        let last = self.log.last_index();
        let max_inflight = self.cfg.max_inflight_msgs;
        let Some(pr) = self.prs.get_mut(&to) else {
            return false;
        };
        if pr.is_paused(max_inflight) {
            return false;
        }
        if pr.next > last + 1 {
            pr.next = last + 1;
        }
        let prev = pr.next - 1;
        let Some(prev_term) = self.log.term(prev) else {
            // 相手が必要とする entry は圧縮済み → snapshot を送る。
            let Some(snap) = &self.log.snapshot else {
                return false;
            };
            let (si, st) = (snap.meta.index, snap.meta.term);
            pr.become_snapshot(si, st);
            self.send_snapshot_chunk(to);
            return true;
        };
        let ents = if pr.next <= last {
            self.log.slice(pr.next, last, self.cfg.max_msg_bytes)
        } else {
            Vec::new()
        };
        if ents.is_empty() && !send_if_empty {
            return false;
        }
        match pr.state {
            ProgressState::Replicate => {
                if let Some(l) = ents.last() {
                    pr.next = l.index + 1;
                    pr.inflights.push_back(l.index);
                }
            }
            ProgressState::Probe => pr.paused = true,
            ProgressState::Snapshot => {}
        }
        let commit = self.log.committed;
        self.send(
            to,
            self.term,
            MsgBody::Append {
                prev_index: prev,
                prev_term,
                entries: ents,
                commit,
            },
        );
        true
    }

    fn send_snapshot_chunk(&mut self, to: NodeId) {
        let chunk = self.cfg.snapshot_chunk_bytes;
        let Some(pr) = self.prs.get_mut(&to) else {
            return;
        };
        let Some(snap) = &self.log.snapshot else {
            pr.become_probe();
            return;
        };
        if pr.snap_index != snap.meta.index || pr.snap_term != snap.meta.term {
            pr.become_snapshot(snap.meta.index, snap.meta.term);
        }
        let total = snap.data.len() as u64;
        let off = pr.snap_offset.min(total);
        let end = (off + chunk).min(total);
        let data = snap.data.slice(off as usize..end as usize);
        let body = MsgBody::Snapshot {
            meta: snap.meta.clone(),
            total_len: total,
            offset: off,
            data,
            done: end == total,
        };
        self.send(to, self.term, body);
    }

    fn bcast_heartbeat(&mut self) {
        self.read_seq += 1;
        self.seq_sent.push_back((self.read_seq, self.now));
        if self.seq_sent.len() > 4096 {
            self.seq_sent.pop_front();
        }
        let seq = self.read_seq;
        let commit = self.log.committed;
        let targets: Vec<(NodeId, u64)> = self
            .prs
            .iter()
            .filter(|(id, _)| **id != self.id)
            .map(|(id, p)| (*id, p.match_.min(commit)))
            .collect();
        for (id, c) in targets {
            self.send(
                id,
                self.term,
                MsgBody::Heartbeat {
                    commit: c,
                    read_seq: seq,
                },
            );
        }
        self.try_release_reads();
    }

    fn start_reads(&mut self, ctxs: Vec<u64>) {
        let index = self.log.committed;
        if self.cfg.lease_read
            && self.lease_ok_this_term
            && self.transferee.is_none()
            && self.now < self.lease_until
        {
            for ctx in ctxs {
                self.read_states.push(ReadState { index, ctx });
            }
            return;
        }
        let seq = self.read_seq + 1;
        for ctx in ctxs {
            self.pending_reads
                .push_back(PendingRead { seq, index, ctx });
        }
        self.bcast_heartbeat();
    }

    fn try_release_reads(&mut self) {
        if self.role != Role::Leader {
            return;
        }
        let (id, seq) = (self.id, self.read_seq);
        let prs = &self.prs;
        let acked = quorum_value(&self.conf, |n| {
            if n == id {
                seq
            } else {
                prs.get(&n).map_or(0, |p| p.read_ack)
            }
        });
        let margin = 2u64;
        while let Some(&(s, t)) = self.seq_sent.front() {
            if s > acked {
                break;
            }
            // 過半数が、時刻 t 以降に自分を受け取った → 彼らは t + election_tick までは他に投票しない。
            self.lease_until = self
                .lease_until
                .max((t + self.cfg.election_tick as u64).saturating_sub(margin));
            self.seq_sent.pop_front();
        }
        while self.pending_reads.front().is_some_and(|r| r.seq <= acked) {
            if let Some(r) = self.pending_reads.pop_front() {
                self.read_states.push(ReadState {
                    index: r.index,
                    ctx: r.ctx,
                });
            }
        }
    }

    fn step_leader(&mut self, m: Message) {
        let from = m.from;
        let last = self.log.last_index();
        let max_inflight = self.cfg.max_inflight_msgs;
        let Some(pr) = self.prs.get_mut(&from) else {
            return;
        };
        pr.recent_active = true;
        match m.body {
            MsgBody::AppendResp {
                reject,
                index,
                reject_hint,
                hint_term,
            } => {
                if reject {
                    let probe = if hint_term > 0 {
                        self.log.find_conflict_by_term(reject_hint, hint_term)
                    } else {
                        reject_hint
                    };
                    let Some(pr) = self.prs.get_mut(&from) else {
                        return;
                    };
                    if pr.maybe_decr_to(index, probe) {
                        if pr.state == ProgressState::Replicate {
                            pr.become_probe();
                        }
                        self.send_append(from, true);
                    }
                    return;
                }
                if !pr.maybe_update(index) {
                    return;
                }
                match pr.state {
                    ProgressState::Probe => pr.become_replicate(),
                    ProgressState::Snapshot if pr.match_ >= pr.snap_index => {
                        pr.become_probe();
                        pr.become_replicate();
                    }
                    ProgressState::Replicate => pr.free_le(index),
                    ProgressState::Snapshot => {}
                }
                let caught_up = pr.match_ == last;
                if self.maybe_commit() {
                    self.bcast_append();
                }
                while self.send_append(from, false) {}
                if caught_up && self.transferee == Some(from) && self.role == Role::Leader {
                    self.send(from, self.term, MsgBody::TimeoutNow);
                }
            }
            MsgBody::HeartbeatResp { read_seq } => {
                pr.paused = false;
                pr.read_ack = pr.read_ack.max(read_seq);
                if pr.state == ProgressState::Replicate && pr.inflights.len() >= max_inflight {
                    pr.inflights.pop_front();
                }
                if pr.state == ProgressState::Snapshot {
                    // 片が失われたときだけ送り直す（前の心拍から進んでいない）。
                    if pr.snap_hb_offset == pr.snap_offset {
                        self.send_snapshot_chunk(from);
                    } else {
                        pr.snap_hb_offset = pr.snap_offset;
                    }
                } else if pr.match_ < last {
                    self.send_append(from, true);
                }
                self.try_release_reads();
            }
            MsgBody::SnapshotResp {
                snap_index,
                snap_term,
                next_offset,
                reject,
            } => {
                if pr.state != ProgressState::Snapshot
                    || pr.snap_index != snap_index
                    || pr.snap_term != snap_term
                {
                    return;
                }
                if reject {
                    pr.become_probe();
                    pr.paused = true;
                    return;
                }
                pr.snap_offset = next_offset;
                self.send_snapshot_chunk(from);
            }
            _ => {}
        }
    }
}
