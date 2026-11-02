//! ICE エージェント（RFC 8445）。sans-io。
//!
//! 1 つのデータストリーム・1 つの component（BUNDLE と rtcp-mux を前提）を扱う。
//!
//! 駆動のしかた（I/O 層）:
//!
//! ```text
//! let mut agent = IceAgent::new(config)?;
//! agent.add_local_socket(now, bound_addr)?;          // インターフェースごと（IPv6 も）
//! agent.set_remote_credentials(ufrag, pwd)?;          // シグナリングから
//! agent.add_remote_candidate_str(now, "candidate:...")?; // trickle で何度でも
//! agent.end_of_remote_candidates(now);
//! loop {
//!     // ソケットで受けたら（local は受けたソケットのアドレス）
//!     if let Some(app) = agent.handle_input(now, local, from, bytes) { /* DTLS・RTP へ */ }
//!     while let Some(t) = agent.poll_transmit() { socket(t.local).send_to(&t.data, t.remote) }
//!     while let Some(e) = agent.poll_event() { /* LocalCandidate を相手へ送る・状態の変化 */ }
//!     sleep_until(agent.poll_timeout()); agent.handle_timeout(now);
//!     // アプリのデータは agent.send(bytes) が返す Transmit を送る
//! }
//! ```
//!
//! 役割:
//!
//! - full ICE（既定）。controlling なら regular nomination（最良の valid pair に USE-CANDIDATE を付けて
//!   検査し直す）。controlled なら相手の USE-CANDIDATE を受けて選ぶ（最後に指名された pair を使う）。
//! - ICE-lite（`IceConfig::lite`）。検査を送らず、host 候補だけ。USE-CANDIDATE 付きの検査で選ぶ。
//! - role conflict は tie-breaker で解き、487 を返す・受ける（7.3.1.1・7.2.5.1）。
//! - consent freshness（RFC 7675）: 選んだ pair に 4〜6 秒ごとに Binding を 1 回だけ送り、30 秒応答が
//!   無ければ Failed にして送信を止める。lite では相手からの検査を同意の印にする。
//! - trickle ICE（RFC 8838）: 候補は何度でも足せ、end-of-candidates の後で全部失敗したら Failed。
//! - mDNS（`.local`）の候補は解決器（[`IceAgent::set_mdns_resolver`]）が無ければ
//!   [`IceError::UnresolvableHostname`] を返す。

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use bytes::Bytes;
use senca_stun::{
    Attribute, Class, Message, MessageBuilder, Method, Retransmission, RetransmitAction, RtoConfig,
    StunError, TransactionId, binding_success, error_response, is_stun,
};
use senca_turn::{Transmit, TurnClient, TurnClientConfig, TurnClientEvent, TurnInput};

use crate::candidate::{
    Candidate, CandidateAddress, CandidateKind, CandidateParseError, Transport, compute_foundation,
    compute_priority,
};
use crate::demux::{PacketKind, classify};

/// ICE の役割。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IceRole {
    Controlling,
    Controlled,
}

impl IceRole {
    fn flip(self) -> IceRole {
        match self {
            IceRole::Controlling => IceRole::Controlled,
            IceRole::Controlled => IceRole::Controlling,
        }
    }
}

/// 接続の状態（W3C の RTCIceConnectionState に合わせた粒度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IceState {
    New,
    Checking,
    Connected,
    Completed,
    Disconnected,
    Failed,
    Closed,
}

/// ufrag と pwd。
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub ufrag: String,
    pub pwd: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("ufrag", &self.ufrag)
            .field("pwd", &"<redacted>")
            .finish()
    }
}

fn is_ice_chars(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

impl Credentials {
    /// OS の乱数源から作る（ufrag 8 文字・pwd 24 文字、RFC 8839 の ice-char）。
    pub fn random() -> Result<Self, IceError> {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).map_err(|_| IceError::Random)?;
        let s: String = b
            .iter()
            .map(|x| ALPHABET[(*x & 63) as usize] as char)
            .collect();
        Ok(Credentials {
            ufrag: s[..8].to_string(),
            pwd: s[8..].to_string(),
        })
    }

    /// RFC 8839 5.4 の長さ（ufrag 4〜256、pwd 22〜256）と文字種を確かめる。
    pub fn validate(&self) -> Result<(), IceError> {
        let ok = (4..=256).contains(&self.ufrag.len())
            && (22..=256).contains(&self.pwd.len())
            && is_ice_chars(&self.ufrag)
            && is_ice_chars(&self.pwd);
        if ok {
            Ok(())
        } else {
            Err(IceError::BadCredentials)
        }
    }
}

/// relayed 候補を取る TURN サーバー。
#[derive(Clone)]
pub struct TurnServerSpec {
    pub server: SocketAddr,
    pub username: String,
    pub password: String,
}

impl fmt::Debug for TurnServerSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnServerSpec")
            .field("server", &self.server)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// エージェントの設定。
#[derive(Debug, Clone)]
pub struct IceConfig {
    pub role: IceRole,
    /// ICE-lite（常に controlled、検査を送らない、host 候補だけ）。
    pub lite: bool,
    /// server-reflexive 候補を取る STUN サーバー。
    pub stun_servers: Vec<SocketAddr>,
    /// relayed 候補を取る TURN サーバー。
    pub turn_servers: Vec<TurnServerSpec>,
    /// 自分の資格情報（`None` なら乱数で作る）。
    pub local_credentials: Option<Credentials>,
    /// tie-breaker（`None` なら乱数）。
    pub tie_breaker: Option<u64>,
    /// 検査の間隔 Ta（既定 50 ms）。
    pub ta: Duration,
    /// 検査の最初の RTO（既定 500 ms）と送る回数（既定 7）。
    pub check_rto: Duration,
    pub check_max_transmits: u32,
    /// controlling が最初の valid pair を見つけてから指名するまで待つ上限。
    pub nomination_delay: Duration,
    /// consent の基本間隔（既定 5 秒、0.8〜1.2 倍でばらす。4 秒未満にはしない）。
    pub consent_interval: Duration,
    /// consent の期限（既定 30 秒）。
    pub consent_timeout: Duration,
    /// これだけ consent の応答が無ければ Disconnected（既定 10 秒）。
    pub disconnected_timeout: Duration,
    /// 相手の候補の数の上限。
    pub max_remote_candidates: usize,
    /// checklist の pair の数の上限（RFC 8445 6.1.2.5 の既定 100）。
    pub max_pairs: usize,
    pub software: Option<String>,
}

impl Default for IceConfig {
    fn default() -> Self {
        IceConfig {
            role: IceRole::Controlled,
            lite: false,
            stun_servers: Vec::new(),
            turn_servers: Vec::new(),
            local_credentials: None,
            tie_breaker: None,
            ta: Duration::from_millis(50),
            check_rto: Duration::from_millis(500),
            check_max_transmits: 7,
            nomination_delay: Duration::from_millis(1000),
            consent_interval: Duration::from_secs(5),
            consent_timeout: Duration::from_secs(30),
            disconnected_timeout: Duration::from_secs(10),
            max_remote_candidates: 100,
            max_pairs: 100,
            software: None,
        }
    }
}

/// エージェントのイベント。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IceEvent {
    /// 自分の候補ができた（trickle で相手へ送る）。
    LocalCandidate(Candidate),
    /// 候補の収集が終わった（end-of-candidates を相手へ送る）。
    GatheringComplete,
    StateChanged(IceState),
    /// 選んだ pair が変わった。
    SelectedPair {
        local: Candidate,
        remote: Candidate,
    },
    /// role conflict で役割が変わった。
    RoleChanged(IceRole),
}

/// エージェントの誤り。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IceError {
    Random,
    BadCredentials,
    BadCandidate(CandidateParseError),
    /// mDNS 名を解決できない（解決器が無い・解決できなかった）。
    UnresolvableHostname(String),
    TooManyCandidates,
    /// 既に登録したソケット。
    DuplicateSocket,
    Stun(StunError),
    Closed,
}

impl fmt::Display for IceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IceError::Random => write!(f, "乱数源が使えない"),
            IceError::BadCredentials => write!(f, "ICE の ufrag・pwd が不正"),
            IceError::BadCandidate(e) => write!(f, "{e}"),
            IceError::UnresolvableHostname(h) => write!(f, "候補のホスト名 {h} を解決できない"),
            IceError::TooManyCandidates => write!(f, "候補が多すぎる"),
            IceError::DuplicateSocket => write!(f, "同じソケットが既にある"),
            IceError::Stun(e) => write!(f, "{e}"),
            IceError::Closed => write!(f, "ICE エージェントは閉じている"),
        }
    }
}

impl std::error::Error for IceError {}

impl From<StunError> for IceError {
    fn from(e: StunError) -> Self {
        IceError::Stun(e)
    }
}

impl From<CandidateParseError> for IceError {
    fn from(e: CandidateParseError) -> Self {
        IceError::BadCandidate(e)
    }
}

/// mDNS の解決器。名前（小文字）から IP を返す。
pub type MdnsResolver = Box<dyn FnMut(&str) -> Option<IpAddr> + Send>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairState {
    Frozen,
    Waiting,
    InProgress,
    Succeeded,
    Failed,
}

struct LocalCand {
    cand: Candidate,
    base: SocketAddr,
    /// relayed なら TURN の枠の番号。
    turn: Option<usize>,
    local_pref: u16,
    dead: bool,
}

struct RemoteCand {
    cand: Candidate,
    addr: SocketAddr,
}

struct Pair {
    local: usize,
    remote: usize,
    priority: u64,
    state: PairState,
    foundation: (String, String),
    nominated: bool,
    /// controlled: 相手から USE-CANDIDATE を受けた。
    use_candidate_received: bool,
    /// controlling: USE-CANDIDATE 付きで検査する。
    nominating: bool,
    txn: Option<TransactionId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TxnKind {
    Check {
        pair: usize,
        use_candidate: bool,
        role: IceRole,
    },
    Consent,
}

struct Txn {
    kind: TxnKind,
    local: usize,
    remote: SocketAddr,
    data: Bytes,
    key: Vec<u8>,
    retrans: Option<Retransmission>,
    /// 再送しない transaction（consent・取り消した検査）の応答を待つ期限。
    expires: Instant,
    /// 再送しないよう取り消された。
    cancelled: bool,
}

struct GatherTxn {
    local: usize,
    server: SocketAddr,
    data: Bytes,
    retrans: Retransmission,
}

struct TurnSlot {
    client: TurnClient,
    base_local: usize,
    relay_local: Option<usize>,
    done: bool,
}

struct Selected {
    local: usize,
    remote: Candidate,
    remote_addr: SocketAddr,
    /// consent に使う相手の資格情報（ICE restart の間も古い pair の consent を続ける）。
    remote_creds: Credentials,
    last_consent: Instant,
    next_consent: Instant,
    consent_lost: bool,
}

/// ICE エージェント。
pub struct IceAgent {
    cfg: IceConfig,
    role: IceRole,
    tie_breaker: u64,
    local_creds: Credentials,
    /// ICE restart 前の資格情報（新しい pair を選ぶまで consent を受け付ける）。
    prev_local_creds: Option<Credentials>,
    remote_creds: Option<Credentials>,
    locals: Vec<LocalCand>,
    remotes: Vec<RemoteCand>,
    pairs: Vec<Pair>,
    triggered: VecDeque<usize>,
    txns: HashMap<TransactionId, Txn>,
    gathers: HashMap<TransactionId, GatherTxn>,
    turns: Vec<TurnSlot>,
    authorized: HashSet<(usize, SocketAddr)>,
    selected: Option<Selected>,
    state: IceState,
    next_check: Option<Instant>,
    nomination_at: Option<Instant>,
    remote_eoc: bool,
    gathering_started: bool,
    gathering_complete: bool,
    checking_since: Option<Instant>,
    prflx_counter: u32,
    mdns_resolver: Option<MdnsResolver>,
    transmits: VecDeque<Transmit>,
    events: VecDeque<IceEvent>,
    closed: bool,
    /// ICE restart 中（新しい pair を選ぶまで通常の検査を続ける）。
    restarting: bool,
    /// 最後に受け取った時刻（時刻を受け取らない操作で timer を張るのに使う）。
    last_now: Option<Instant>,
}

impl fmt::Debug for IceAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IceAgent")
            .field("role", &self.role)
            .field("state", &self.state)
            .field("local_candidates", &self.locals.len())
            .field("remote_candidates", &self.remotes.len())
            .field(
                "pairs",
                &self.pairs.iter().map(|p| p.state).collect::<Vec<_>>(),
            )
            .field("remote_eoc", &self.remote_eoc)
            .field("triggered", &self.triggered)
            .field(
                "next_check",
                &self
                    .next_check
                    .map(|t| self.last_now.map(|n| t.saturating_duration_since(n))),
            )
            .field("txns", &self.txns.len())
            .finish_non_exhaustive()
    }
}

/// 同じ範囲（族・IPv6 の link-local か否か）の候補だけを対にする。
fn compatible(a: &SocketAddr, b: &SocketAddr) -> bool {
    match (a.ip(), b.ip()) {
        (IpAddr::V4(_), IpAddr::V4(_)) => true,
        (IpAddr::V6(x), IpAddr::V6(y)) => x.is_unicast_link_local() == y.is_unicast_link_local(),
        _ => false,
    }
}

fn pair_priority(role: IceRole, local: u32, remote: u32) -> u64 {
    let (g, d) = match role {
        IceRole::Controlling => (local as u64, remote as u64),
        IceRole::Controlled => (remote as u64, local as u64),
    };
    (1u64 << 32) * g.min(d) + 2 * g.max(d) + u64::from(g > d)
}

fn random_u64() -> Result<u64, IceError> {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).map_err(|_| IceError::Random)?;
    Ok(u64::from_be_bytes(b))
}

impl IceAgent {
    pub fn new(cfg: IceConfig) -> Result<IceAgent, IceError> {
        let local_creds = match &cfg.local_credentials {
            Some(c) => {
                c.validate()?;
                c.clone()
            }
            None => Credentials::random()?,
        };
        let tie_breaker = match cfg.tie_breaker {
            Some(t) => t,
            None => random_u64()?,
        };
        let role = if cfg.lite {
            IceRole::Controlled
        } else {
            cfg.role
        };
        Ok(IceAgent {
            role,
            tie_breaker,
            local_creds,
            prev_local_creds: None,
            remote_creds: None,
            locals: Vec::new(),
            remotes: Vec::new(),
            pairs: Vec::new(),
            triggered: VecDeque::new(),
            txns: HashMap::new(),
            gathers: HashMap::new(),
            turns: Vec::new(),
            authorized: HashSet::new(),
            selected: None,
            state: IceState::New,
            next_check: None,
            nomination_at: None,
            remote_eoc: false,
            gathering_started: false,
            gathering_complete: false,
            checking_since: None,
            prflx_counter: 0,
            mdns_resolver: None,
            transmits: VecDeque::new(),
            events: VecDeque::new(),
            closed: false,
            last_now: None,
            restarting: false,
            cfg,
        })
    }

    pub fn role(&self) -> IceRole {
        self.role
    }

    pub fn state(&self) -> IceState {
        self.state
    }

    pub fn is_lite(&self) -> bool {
        self.cfg.lite
    }

    pub fn local_credentials(&self) -> &Credentials {
        &self.local_creds
    }

    /// これまでに作った自分の候補（SDP に並べる用、peer-reflexive は除く）。
    pub fn local_candidates(&self) -> Vec<Candidate> {
        self.locals
            .iter()
            .filter(|l| !l.dead && l.cand.kind != CandidateKind::PeerReflexive)
            .map(|l| l.cand.clone())
            .collect()
    }

    /// 収集が終わったか。
    pub fn is_gathering_complete(&self) -> bool {
        self.gathering_complete
    }

    /// 選んだ pair（自分の候補, 相手の候補）。
    pub fn selected_pair(&self) -> Option<(&Candidate, &Candidate)> {
        self.selected
            .as_ref()
            .map(|s| (&self.locals[s.local].cand, &s.remote))
    }

    /// mDNS の解決器を設定する。
    pub fn set_mdns_resolver(&mut self, r: MdnsResolver) {
        self.mdns_resolver = Some(r);
    }

    /// 相手の ufrag・pwd を設定する。
    pub fn set_remote_credentials(&mut self, ufrag: &str, pwd: &str) -> Result<(), IceError> {
        let c = Credentials {
            ufrag: ufrag.to_string(),
            pwd: pwd.to_string(),
        };
        c.validate()?;
        self.remote_creds = Some(c);
        self.kick();
        Ok(())
    }

    /// ローカルソケットを足す（host 候補になる）。STUN・TURN サーバーがあれば srflx・relayed の収集を始める。
    pub fn add_local_socket(
        &mut self,
        now: Instant,
        addr: SocketAddr,
    ) -> Result<Candidate, IceError> {
        self.last_now = Some(now);
        if self.closed {
            return Err(IceError::Closed);
        }
        if self
            .locals
            .iter()
            .any(|l| l.cand.kind == CandidateKind::Host && l.base == addr)
        {
            return Err(IceError::DuplicateSocket);
        }
        if self.locals.len() >= 64 {
            return Err(IceError::TooManyCandidates);
        }
        self.gathering_started = true;
        let hosts = self
            .locals
            .iter()
            .filter(|l| l.cand.kind == CandidateKind::Host)
            .count() as u16;
        // IPv6 を IPv4 より先に（RFC 8421）。インターフェースごとに違う値にする。
        let local_pref = if addr.is_ipv6() { 0xFFFFu16 } else { 0xBFFF }.saturating_sub(hosts * 2);
        let cand = Candidate {
            foundation: compute_foundation(CandidateKind::Host, addr.ip(), None, &Transport::Udp),
            component: 1,
            transport: Transport::Udp,
            priority: compute_priority(CandidateKind::Host, local_pref, 1),
            address: CandidateAddress::Ip(addr),
            kind: CandidateKind::Host,
            related: None,
            tcptype: None,
            extensions: Vec::new(),
        };
        let idx = self.push_local(cand.clone(), addr, None, local_pref);
        self.pair_new_local(now, idx);
        if !self.cfg.lite {
            for server in self.cfg.stun_servers.clone() {
                if server.is_ipv4() == addr.is_ipv4() {
                    self.start_gather(now, idx, server)?;
                }
            }
            for spec in self.cfg.turn_servers.clone() {
                if spec.server.is_ipv4() == addr.is_ipv4() {
                    let mut client = TurnClient::new(TurnClientConfig::new(
                        addr,
                        spec.server,
                        &spec.username,
                        &spec.password,
                    ));
                    client.allocate(now)?;
                    self.turns.push(TurnSlot {
                        client,
                        base_local: idx,
                        relay_local: None,
                        done: false,
                    });
                    let t = self.turns.len() - 1;
                    self.drain_turn(now, t);
                }
            }
        }
        Ok(cand)
    }

    fn push_local(
        &mut self,
        cand: Candidate,
        base: SocketAddr,
        turn: Option<usize>,
        local_pref: u16,
    ) -> usize {
        if cand.kind != CandidateKind::PeerReflexive {
            self.events
                .push_back(IceEvent::LocalCandidate(cand.clone()));
        }
        self.locals.push(LocalCand {
            cand,
            base,
            turn,
            local_pref,
            dead: false,
        });
        self.locals.len() - 1
    }

    fn start_gather(
        &mut self,
        now: Instant,
        local: usize,
        server: SocketAddr,
    ) -> Result<(), IceError> {
        let tid = TransactionId::random()?;
        let data = MessageBuilder::new(Class::Request, Method::BINDING, tid)
            .fingerprint()
            .build()?;
        let base = self.locals[local].base;
        self.transmits.push_back(Transmit {
            local: base,
            remote: server,
            data: data.clone(),
        });
        let retrans = Retransmission::new(now, RtoConfig::default(), None);
        self.gathers.insert(
            tid,
            GatherTxn {
                local,
                server,
                data,
                retrans,
            },
        );
        Ok(())
    }

    /// 相手の候補を足す（trickle）。対にしなかった（component 2・TCP・重複）なら `Ok(false)`。
    pub fn add_remote_candidate(
        &mut self,
        now: Instant,
        cand: Candidate,
    ) -> Result<bool, IceError> {
        self.last_now = Some(now);
        if self.closed {
            return Err(IceError::Closed);
        }
        if cand.component != 1 || cand.transport != Transport::Udp {
            return Ok(false);
        }
        let addr = match &cand.address {
            CandidateAddress::Ip(a) => *a,
            CandidateAddress::Mdns { name, port } => {
                let ip = self.mdns_resolver.as_mut().and_then(|r| r(name));
                match ip {
                    Some(ip) => SocketAddr::new(ip, *port),
                    None => return Err(IceError::UnresolvableHostname(name.clone())),
                }
            }
        };
        if addr.port() == 0 || addr.ip().is_unspecified() || addr.ip().is_multicast() {
            return Ok(false);
        }
        if let Some(r) = self.remotes.iter_mut().find(|r| r.addr == addr) {
            // peer-reflexive として先に学んでいたら、シグナリングの情報で置き換える（RFC 8445 7.3.1.3）。
            if r.cand.kind == CandidateKind::PeerReflexive
                && cand.kind != CandidateKind::PeerReflexive
            {
                r.cand = cand;
            }
            return Ok(false);
        }
        if self.remotes.len() >= self.cfg.max_remote_candidates {
            return Err(IceError::TooManyCandidates);
        }
        self.remotes.push(RemoteCand { cand, addr });
        let ridx = self.remotes.len() - 1;
        for l in 0..self.locals.len() {
            self.add_pair(now, l, ridx, false);
        }
        self.kick();
        self.update_state(now);
        Ok(true)
    }

    /// 文字列の候補を足す。
    pub fn add_remote_candidate_str(&mut self, now: Instant, s: &str) -> Result<bool, IceError> {
        let c: Candidate = s.parse()?;
        self.add_remote_candidate(now, c)
    }

    /// 相手の end-of-candidates（RFC 8838）。
    pub fn end_of_remote_candidates(&mut self, now: Instant) {
        self.last_now = Some(now);
        self.remote_eoc = true;
        self.update_state(now);
    }

    /// ICE restart。新しい自分の資格情報を返す。相手の資格情報と候補は改めて設定すること。
    /// 選んでいた pair は新しい pair を選ぶまで使い続け、consent も続ける（RFC 7675 5.1）。
    pub fn restart(&mut self, now: Instant) -> Result<Credentials, IceError> {
        self.last_now = Some(now);
        if self.closed {
            return Err(IceError::Closed);
        }
        let new = Credentials::random()?;
        self.prev_local_creds = Some(std::mem::replace(&mut self.local_creds, new));
        self.remote_creds = None;
        self.remotes.clear();
        self.pairs.clear();
        self.triggered.clear();
        self.txns.retain(|_, t| t.kind == TxnKind::Consent);
        self.authorized.retain(|(l, r)| {
            self.selected
                .as_ref()
                .is_some_and(|s| s.local == *l && s.remote_addr == *r)
        });
        self.remote_eoc = false;
        self.nomination_at = None;
        self.checking_since = None;
        self.role = if self.cfg.lite {
            IceRole::Controlled
        } else {
            self.cfg.role
        };
        self.restarting = self.selected.is_some();
        self.update_state(now);
        Ok(self.local_creds.clone())
    }

    /// 閉じる（TURN の allocation を解放する）。
    pub fn close(&mut self, now: Instant) {
        if self.closed {
            return;
        }
        self.closed = true;
        for i in 0..self.turns.len() {
            let _ = self.turns[i].client.deallocate(now);
            while let Some(t) = self.turns[i].client.poll_transmit() {
                self.transmits.push_back(t);
            }
        }
        self.selected = None;
        self.set_state(IceState::Closed);
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmits.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<IceEvent> {
        if self.gathering_started
            && !self.gathering_complete
            && self.gathers.is_empty()
            && self.turns.iter().all(|t| t.done)
        {
            self.gathering_complete = true;
            self.events.push_back(IceEvent::GatheringComplete);
        }
        self.events.pop_front()
    }

    /// 選んだ pair でアプリのデータ（DTLS・SRTP）を送る。pair が無い・consent を失ったら `None`。
    pub fn send(&mut self, data: Bytes) -> Option<Transmit> {
        let s = self.selected.as_ref()?;
        if s.consent_lost || self.closed {
            return None;
        }
        let (local, remote) = (s.local, s.remote_addr);
        let l = &self.locals[local];
        match l.turn {
            None => Some(Transmit {
                local: l.base,
                remote,
                data,
            }),
            Some(t) => self.turns[t].client.send(remote, &data),
        }
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        if self.closed {
            return self
                .turns
                .iter()
                .filter_map(|t| t.client.poll_timeout())
                .min();
        }
        let mut t: Option<Instant> = None;
        let mut upd = |x: Option<Instant>| {
            if let Some(x) = x {
                t = Some(t.map_or(x, |t| t.min(x)));
            }
        };
        if self.has_check_work() {
            upd(self.next_check);
        }
        upd(self.nomination_at);
        for x in self.txns.values() {
            let r = if x.cancelled {
                None
            } else {
                x.retrans.as_ref().and_then(|r| r.poll_timeout())
            };
            upd(Some(r.unwrap_or(x.expires)));
        }
        for g in self.gathers.values() {
            upd(g.retrans.poll_timeout());
        }
        for s in &self.turns {
            upd(s.client.poll_timeout());
        }
        if let Some(s) = &self.selected
            && !s.consent_lost
        {
            if !self.cfg.lite {
                upd(Some(s.next_consent));
            }
            if self.state != IceState::Disconnected {
                upd(Some(s.last_consent + self.cfg.disconnected_timeout));
            }
            upd(Some(s.last_consent + self.cfg.consent_timeout));
        }
        t
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        self.last_now = Some(now);
        // TURN
        for i in 0..self.turns.len() {
            self.turns[i].client.handle_timeout(now);
            self.drain_turn(now, i);
        }
        if self.closed {
            return;
        }
        // srflx の収集
        let mut done = Vec::new();
        for (tid, g) in self.gathers.iter_mut() {
            match g.retrans.handle_timeout(now) {
                RetransmitAction::Wait => {}
                RetransmitAction::Retransmit => self.transmits.push_back(Transmit {
                    local: self.locals[g.local].base,
                    remote: g.server,
                    data: g.data.clone(),
                }),
                RetransmitAction::TimedOut => done.push(*tid),
            }
        }
        for tid in done {
            self.gathers.remove(&tid);
        }
        // 検査の再送・期限
        let mut resend = Vec::new();
        let mut expired = Vec::new();
        for (tid, x) in self.txns.iter_mut() {
            match &mut x.retrans {
                Some(r) if !x.cancelled => match r.handle_timeout(now) {
                    RetransmitAction::Wait => {}
                    RetransmitAction::Retransmit => {
                        resend.push((x.local, x.remote, x.data.clone()))
                    }
                    RetransmitAction::TimedOut => expired.push(*tid),
                },
                _ => {
                    if now >= x.expires {
                        expired.push(*tid);
                    }
                }
            }
        }
        for (l, r, d) in resend {
            self.send_on(l, r, d);
        }
        for tid in expired {
            if let Some(x) = self.txns.remove(&tid)
                && let TxnKind::Check { pair, .. } = x.kind
                && !x.cancelled
                && let Some(p) = self.pairs.get_mut(pair)
                && p.txn == Some(tid)
            {
                p.txn = None;
                p.state = PairState::Failed;
                if p.nominating {
                    p.nominating = false;
                    self.nomination_at = Some(now);
                }
            }
        }
        // 検査の送信（Ta ごとに 1 つ）
        if self.has_check_work() && self.next_check.is_none_or(|t| now >= t) {
            self.send_next_check(now);
            self.next_check = Some(now + self.cfg.ta);
        }
        // controlling の指名
        if self.role == IceRole::Controlling && !self.cfg.lite {
            self.maybe_nominate(now);
        }
        // consent
        self.consent_tick(now);
        self.update_state(now);
    }

    /// ソケットで受けたデータグラムを処理する。`local` は受けたソケットのアドレス。
    /// ICE のものでなく、認められた相手からのアプリのデータ（DTLS・RTP など）なら返す。
    pub fn handle_input(
        &mut self,
        now: Instant,
        local: SocketAddr,
        remote: SocketAddr,
        data: Bytes,
    ) -> Option<Bytes> {
        self.last_now = Some(now);
        if self.closed {
            return None;
        }
        // TURN サーバーからのもの
        for i in 0..self.turns.len() {
            if self.locals[self.turns[i].base_local].base != local
                || self.turns[i].client.server() != remote
            {
                continue;
            }
            match self.turns[i].client.handle_input(now, remote, data.clone()) {
                TurnInput::NotMine => {}
                TurnInput::Consumed => {
                    self.drain_turn(now, i);
                    return None;
                }
                TurnInput::Data { peer, data } => {
                    self.drain_turn(now, i);
                    let relay = self.turns[i].relay_local?;
                    return self.handle_path(now, relay, peer, data);
                }
            }
        }
        let host = self
            .locals
            .iter()
            .position(|l| l.cand.kind == CandidateKind::Host && l.base == local)?;
        self.handle_path(now, host, remote, data)
    }

    fn handle_path(
        &mut self,
        now: Instant,
        local: usize,
        remote: SocketAddr,
        data: Bytes,
    ) -> Option<Bytes> {
        match classify(&data) {
            PacketKind::Stun => {
                if is_stun(&data)
                    && let Ok(msg) = Message::parse(data)
                {
                    self.handle_stun(now, local, remote, msg);
                }
                self.update_state(now);
                None
            }
            PacketKind::Unknown | PacketKind::TurnChannel => None,
            _ => {
                if self.authorized.contains(&(local, remote)) {
                    Some(data)
                } else {
                    None
                }
            }
        }
    }

    fn handle_stun(&mut self, now: Instant, local: usize, remote: SocketAddr, msg: Message) {
        if msg.method != Method::BINDING {
            return;
        }
        match msg.class {
            Class::Request => self.handle_request(now, local, remote, &msg),
            Class::Success | Class::Error => {
                if let Some(g) = self.gathers.get(&msg.transaction_id) {
                    if g.server == remote && g.local == local {
                        let g = self.gathers.remove(&msg.transaction_id).expect("present");
                        if msg.class == Class::Success
                            && let Some(mapped) = msg.mapped_address()
                        {
                            self.add_srflx(g.local, mapped, Some(g.server.ip()));
                        }
                    }
                    return;
                }
                self.handle_response(now, local, remote, &msg);
            }
            Class::Indication => {}
        }
    }

    fn add_srflx(&mut self, base_local: usize, mapped: SocketAddr, server: Option<IpAddr>) {
        let base = self.locals[base_local].base;
        if mapped == base
            || self.locals.iter().any(|l| {
                l.cand.kind == CandidateKind::ServerReflexive && l.cand.addr() == Some(mapped)
            })
        {
            return;
        }
        let local_pref = self.locals[base_local].local_pref;
        let cand = Candidate {
            foundation: compute_foundation(
                CandidateKind::ServerReflexive,
                base.ip(),
                server,
                &Transport::Udp,
            ),
            component: 1,
            transport: Transport::Udp,
            priority: compute_priority(CandidateKind::ServerReflexive, local_pref, 1),
            address: CandidateAddress::Ip(mapped),
            kind: CandidateKind::ServerReflexive,
            related: Some(base),
            tcptype: None,
            extensions: Vec::new(),
        };
        // srflx は base に置き換えて刈り込まれるので対にしない（RFC 8445 6.1.2.4）。
        self.push_local(cand, base, None, local_pref);
    }

    fn respond(&mut self, local: usize, remote: SocketAddr, b: MessageBuilder, key: Option<&[u8]>) {
        let b = if let Some(k) = key { b.integrity(k) } else { b };
        if let Ok(data) = b.fingerprint().build() {
            self.send_on(local, remote, data);
        }
    }

    fn handle_request(&mut self, now: Instant, local: usize, remote: SocketAddr, msg: &Message) {
        let Some(username) = msg.username() else {
            self.respond(local, remote, error_response(msg, 400, "Bad Request"), None);
            return;
        };
        let lfrag = username.split(':').next().unwrap_or("");
        let (key, old) = if lfrag == self.local_creds.ufrag {
            (self.local_creds.pwd.clone().into_bytes(), false)
        } else if let Some(p) = self.prev_local_creds.as_ref().filter(|p| p.ufrag == lfrag) {
            (p.pwd.clone().into_bytes(), true)
        } else {
            self.respond(
                local,
                remote,
                error_response(msg, 401, "Unauthenticated"),
                None,
            );
            return;
        };
        if msg.verify_integrity(&key).is_err() {
            self.respond(
                local,
                remote,
                error_response(msg, 401, "Unauthenticated"),
                None,
            );
            return;
        }
        if !msg.unknown_comprehension_required().is_empty() {
            self.respond(
                local,
                remote,
                error_response(msg, 420, "Unknown Attribute"),
                Some(&key),
            );
            return;
        }
        let Some(priority) = msg.priority() else {
            self.respond(
                local,
                remote,
                error_response(msg, 400, "Bad Request"),
                Some(&key),
            );
            return;
        };
        // role conflict（RFC 8445 7.3.1.1）
        if !self.cfg.lite && !old {
            match (self.role, msg.ice_controlling(), msg.ice_controlled()) {
                (IceRole::Controlling, Some(t), _) => {
                    if self.tie_breaker >= t {
                        self.respond(
                            local,
                            remote,
                            error_response(msg, 487, "Role Conflict"),
                            Some(&key),
                        );
                        return;
                    }
                    self.switch_role(IceRole::Controlled);
                }
                (IceRole::Controlled, _, Some(t)) => {
                    if self.tie_breaker >= t {
                        self.switch_role(IceRole::Controlling);
                    } else {
                        self.respond(
                            local,
                            remote,
                            error_response(msg, 487, "Role Conflict"),
                            Some(&key),
                        );
                        return;
                    }
                }
                _ => {}
            }
        }
        let mut b = binding_success(msg, remote);
        if let Some(s) = &self.cfg.software {
            b = b.attr(Attribute::Software(s.clone()));
        }
        self.respond(local, remote, b, Some(&key));
        self.authorize(local, remote);

        // 古い資格情報での検査は、選んでいた pair の consent としてだけ扱う。
        if old {
            if self.cfg.lite
                && let Some(s) = self.selected.as_mut()
                && s.local == local
                && s.remote_addr == remote
                && !s.consent_lost
            {
                s.last_consent = now;
            }
            return;
        }

        // 相手の候補を学ぶ（peer-reflexive、RFC 8445 7.3.1.3）
        let ridx = match self.remotes.iter().position(|r| r.addr == remote) {
            Some(i) => i,
            None => {
                if self.remotes.len() >= self.cfg.max_remote_candidates * 2 {
                    return;
                }
                self.prflx_counter += 1;
                let cand = Candidate {
                    foundation: format!("prflx{}", self.prflx_counter),
                    component: 1,
                    transport: Transport::Udp,
                    priority,
                    address: CandidateAddress::Ip(remote),
                    kind: CandidateKind::PeerReflexive,
                    related: None,
                    tcptype: None,
                    extensions: Vec::new(),
                };
                self.remotes.push(RemoteCand { cand, addr: remote });
                self.remotes.len() - 1
            }
        };
        if self.checking_since.is_none() {
            self.checking_since = Some(now);
        }
        let use_candidate = msg.use_candidate() && self.role == IceRole::Controlled;
        let Some(pidx) = self.add_pair(now, local, ridx, true) else {
            return;
        };

        if self.cfg.lite {
            // lite: 検査を送らず、受けた検査で pair を valid とみなす。
            let p = &mut self.pairs[pidx];
            p.state = PairState::Succeeded;
            if use_candidate {
                self.nominate(now, pidx);
            }
            if let Some(s) = self.selected.as_mut()
                && s.local == local
                && s.remote_addr == remote
                && !s.consent_lost
            {
                s.last_consent = now;
            }
            return;
        }

        // triggered check（RFC 8445 7.3.1.4）
        let p = &mut self.pairs[pidx];
        if use_candidate {
            p.use_candidate_received = true;
        }
        match p.state {
            PairState::Succeeded => {
                if use_candidate {
                    self.nominate(now, pidx);
                }
            }
            PairState::InProgress => {
                if let Some(tid) = p.txn.take()
                    && let Some(x) = self.txns.get_mut(&tid)
                {
                    // 取り消し: 再送しないが、期限までは応答を受け付ける。
                    x.cancelled = true;
                    x.expires = now + self.cfg.check_rto * 16;
                }
                p.state = PairState::Waiting;
                self.enqueue_triggered(pidx);
            }
            PairState::Waiting | PairState::Frozen | PairState::Failed => {
                p.state = PairState::Waiting;
                self.enqueue_triggered(pidx);
            }
        }
    }

    fn handle_response(&mut self, now: Instant, local: usize, remote: SocketAddr, msg: &Message) {
        let Some(x) = self.txns.get(&msg.transaction_id) else {
            return;
        };
        // 認証できない応答は捨てる（偽の応答で consent や pair を作らせない）。
        if msg.verify_integrity(&x.key).is_err() {
            return;
        }
        let x = self.txns.remove(&msg.transaction_id).expect("present");
        match x.kind {
            TxnKind::Consent => {
                if msg.class == Class::Success
                    && let Some(s) = self.selected.as_mut()
                    && s.local == local
                    && s.remote_addr == remote
                    && !s.consent_lost
                {
                    s.last_consent = now;
                }
            }
            TxnKind::Check {
                pair,
                use_candidate,
                role,
            } => {
                let Some(p) = self.pairs.get_mut(pair) else {
                    return;
                };
                if p.txn == Some(msg.transaction_id) {
                    p.txn = None;
                } else if p.txn.is_some() {
                    // 新しい検査が進行中。古い（取り消した）応答の成功だけは使う。
                    if msg.class != Class::Success {
                        return;
                    }
                }
                // 送り先と受け元が対称でなければ失敗（RFC 8445 7.2.5.2.1）。
                if p.local != local || self.remotes[p.remote].addr != remote {
                    p.state = PairState::Failed;
                    return;
                }
                if msg.class == Class::Error {
                    if msg.error_code().map(|(c, _)| c) == Some(487) {
                        // 送ったときの役割から反対へ（RFC 8445 7.2.5.1）。
                        if self.role == role {
                            self.switch_role(role.flip());
                        }
                        let p = &mut self.pairs[pair];
                        p.state = PairState::Waiting;
                        self.enqueue_triggered(pair);
                    } else {
                        p.state = PairState::Failed;
                        if p.nominating {
                            p.nominating = false;
                            self.nomination_at = Some(now);
                        }
                    }
                    return;
                }
                p.state = PairState::Succeeded;
                let use_candidate_received = p.use_candidate_received;
                let foundation = p.foundation.clone();
                self.authorize(local, remote);
                // 自分の peer-reflexive 候補（RFC 8445 7.2.5.3.1）
                if let Some(mapped) = msg.mapped_address()
                    && !self.locals.iter().any(|l| l.cand.addr() == Some(mapped))
                {
                    let lp = self.locals[local].local_pref;
                    let base = self.locals[local].base;
                    let cand = Candidate {
                        foundation: compute_foundation(
                            CandidateKind::PeerReflexive,
                            base.ip(),
                            None,
                            &Transport::Udp,
                        ),
                        component: 1,
                        transport: Transport::Udp,
                        priority: compute_priority(CandidateKind::PeerReflexive, lp, 1),
                        address: CandidateAddress::Ip(mapped),
                        kind: CandidateKind::PeerReflexive,
                        related: Some(base),
                        tcptype: None,
                        extensions: Vec::new(),
                    };
                    if self.locals.len() < 256 {
                        self.push_local(cand, base, None, lp);
                    }
                }
                // 同じ foundation の Frozen を Waiting に（RFC 8445 7.2.5.3.3）
                for q in self.pairs.iter_mut() {
                    if q.state == PairState::Frozen && q.foundation == foundation {
                        q.state = PairState::Waiting;
                    }
                }
                if (use_candidate && self.role == IceRole::Controlling)
                    || (self.role == IceRole::Controlled && use_candidate_received)
                {
                    self.nominate(now, pair);
                } else if self.role == IceRole::Controlling
                    && self.nomination_at.is_none()
                    && !self.has_nominated()
                {
                    self.nomination_at = Some(now + self.cfg.nomination_delay);
                }
            }
        }
    }

    fn has_nominated(&self) -> bool {
        self.pairs.iter().any(|p| p.nominated || p.nominating)
    }

    fn maybe_nominate(&mut self, now: Instant) {
        if self.has_nominated() {
            return;
        }
        let Some(best) = self
            .pairs
            .iter()
            .enumerate()
            .filter(|(_, p)| p.state == PairState::Succeeded && !self.locals[p.local].dead)
            .max_by_key(|(_, p)| p.priority)
            .map(|(i, p)| (i, p.priority))
        else {
            return;
        };
        // より優先度の高い pair の検査が終わっているか、待ち時間を過ぎたら指名する。
        let pending_better = self.pairs.iter().any(|p| {
            p.priority > best.1
                && matches!(
                    p.state,
                    PairState::Waiting | PairState::InProgress | PairState::Frozen
                )
        });
        if pending_better && self.nomination_at.is_some_and(|t| now < t) {
            return;
        }
        self.nomination_at = None;
        let p = &mut self.pairs[best.0];
        p.nominating = true;
        self.enqueue_triggered(best.0);
        if self.next_check.is_some_and(|t| t > now) {
            // 次の Ta を待つ
        } else {
            self.send_next_check(now);
            self.next_check = Some(now + self.cfg.ta);
        }
    }

    fn nominate(&mut self, now: Instant, pidx: usize) {
        let p = &mut self.pairs[pidx];
        p.nominated = true;
        p.nominating = false;
        let (local, ridx) = (p.local, p.remote);
        let remote_addr = self.remotes[ridx].addr;
        if self
            .selected
            .as_ref()
            .is_some_and(|s| s.local == local && s.remote_addr == remote_addr && !s.consent_lost)
            && !self.restarting
        {
            return;
        }
        self.restarting = false;
        let Some(remote_creds) = self.remote_creds.clone().or_else(|| {
            // lite・相手の資格情報を受ける前の指名: consent は送らないので仮の値でよい
            self.cfg.lite.then(|| self.local_creds.clone())
        }) else {
            return;
        };
        self.selected = Some(Selected {
            local,
            remote: self.remotes[ridx].cand.clone(),
            remote_addr,
            remote_creds,
            last_consent: now,
            next_consent: now + self.consent_delay(),
            consent_lost: false,
        });
        self.prev_local_creds = None;
        if let Some(t) = self.locals[local].turn {
            let _ = self.turns[t].client.channel_bind(now, remote_addr);
            self.drain_turn(now, t);
        }
        self.events.push_back(IceEvent::SelectedPair {
            local: self.locals[local].cand.clone(),
            remote: self.remotes[ridx].cand.clone(),
        });
        self.update_state(now);
    }

    fn consent_delay(&self) -> Duration {
        let mut b = [0u8; 2];
        let r = if getrandom::fill(&mut b).is_ok() {
            u16::from_be_bytes(b) as f64 / 65535.0
        } else {
            0.5
        };
        let d = self.cfg.consent_interval.mul_f64(0.8 + 0.4 * r);
        d.max(Duration::from_secs(4).min(self.cfg.consent_interval))
    }

    fn consent_tick(&mut self, now: Instant) {
        let lite = self.cfg.lite;
        let Some(s) = self.selected.as_mut() else {
            return;
        };
        if s.consent_lost {
            return;
        }
        if now >= s.last_consent + self.cfg.consent_timeout {
            // 送信を止める（RFC 7675 5.1）。古い consent の transaction も捨てる。
            s.consent_lost = true;
            self.txns.retain(|_, t| t.kind != TxnKind::Consent);
            return;
        }
        if lite || now < s.next_consent {
            return;
        }
        s.next_consent = now + self.cfg.consent_interval;
        let (local, remote, creds) = (s.local, s.remote_addr, s.remote_creds.clone());
        let next = now + self.consent_delay();
        if let Some(s) = self.selected.as_mut() {
            s.next_consent = next;
        }
        let Ok(tid) = TransactionId::random() else {
            return;
        };
        let Ok(data) = self.build_check(tid, local, &creds, false) else {
            return;
        };
        // consent は 1 回だけ送る。応答は RTO の数倍まで待つ。
        let expires = now + self.cfg.check_rto * 8;
        self.txns.insert(
            tid,
            Txn {
                kind: TxnKind::Consent,
                local,
                remote,
                data: data.clone(),
                key: creds.pwd.into_bytes(),
                retrans: None,
                expires,
                cancelled: false,
            },
        );
        self.send_on(local, remote, data);
    }

    fn build_check(
        &self,
        tid: TransactionId,
        local: usize,
        remote: &Credentials,
        use_candidate: bool,
    ) -> Result<Bytes, StunError> {
        let lp = self.locals[local].local_pref;
        let mut b = MessageBuilder::new(Class::Request, Method::BINDING, tid)
            .attr(Attribute::Username(format!(
                "{}:{}",
                remote.ufrag, self.local_creds.ufrag
            )))
            .attr(Attribute::Priority(compute_priority(
                CandidateKind::PeerReflexive,
                lp,
                1,
            )));
        b = match self.role {
            IceRole::Controlling => {
                let b = b.attr(Attribute::IceControlling(self.tie_breaker));
                if use_candidate {
                    b.attr(Attribute::UseCandidate)
                } else {
                    b
                }
            }
            IceRole::Controlled => b.attr(Attribute::IceControlled(self.tie_breaker)),
        };
        if let Some(s) = &self.cfg.software {
            b = b.attr(Attribute::Software(s.clone()));
        }
        b.integrity(remote.pwd.as_bytes()).fingerprint().build()
    }

    fn has_check_work(&self) -> bool {
        if self.cfg.lite || self.remote_creds.is_none() || self.closed {
            return false;
        }
        if self
            .triggered
            .iter()
            .any(|&p| self.pairs[p].state != PairState::Succeeded || self.pairs[p].nominating)
        {
            return true;
        }
        // 選んだ後は通常の検査を止める（triggered だけ）。
        (self.selected.is_none() || self.restarting)
            && self
                .pairs
                .iter()
                .any(|p| matches!(p.state, PairState::Waiting | PairState::Frozen))
    }

    fn send_next_check(&mut self, now: Instant) {
        if self.cfg.lite || self.remote_creds.is_none() {
            return;
        }
        // 既に成功した pair は（指名のための検査でなければ）検査し直さない。
        while let Some(&p) = self.triggered.front() {
            let q = &self.pairs[p];
            if q.state == PairState::Succeeded && !q.nominating {
                self.triggered.pop_front();
            } else {
                break;
            }
        }
        let pidx = if let Some(p) = self.triggered.pop_front() {
            Some(p)
        } else if self.selected.is_none() || self.restarting {
            let waiting = self
                .pairs
                .iter()
                .enumerate()
                .filter(|(_, p)| p.state == PairState::Waiting)
                .max_by_key(|(_, p)| p.priority)
                .map(|(i, _)| i);
            waiting.or_else(|| {
                // Waiting が無ければ Frozen を 1 つ解く（RFC 8445 6.1.4.2）
                self.pairs
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| p.state == PairState::Frozen)
                    .max_by_key(|(_, p)| p.priority)
                    .map(|(i, _)| i)
            })
        } else {
            None
        };
        if let Some(p) = pidx {
            self.send_check(now, p);
        }
    }

    fn send_check(&mut self, now: Instant, pidx: usize) {
        let Some(creds) = self.remote_creds.clone() else {
            return;
        };
        let p = &self.pairs[pidx];
        if self.locals[p.local].dead {
            self.pairs[pidx].state = PairState::Failed;
            return;
        }
        let use_candidate = p.nominating && self.role == IceRole::Controlling;
        let (local, remote) = (p.local, self.remotes[p.remote].addr);
        let Ok(tid) = TransactionId::random() else {
            return;
        };
        let Ok(data) = self.build_check(tid, local, &creds, use_candidate) else {
            return;
        };
        let cfg = RtoConfig {
            initial_rto: self.cfg.check_rto,
            max_transmits: self.cfg.check_max_transmits,
            ..RtoConfig::default()
        };
        self.txns.insert(
            tid,
            Txn {
                kind: TxnKind::Check {
                    pair: pidx,
                    use_candidate,
                    role: self.role,
                },
                local,
                remote,
                data: data.clone(),
                key: creds.pwd.into_bytes(),
                retrans: Some(Retransmission::new(now, cfg, None)),
                expires: now + self.cfg.check_rto * 80,
                cancelled: false,
            },
        );
        let p = &mut self.pairs[pidx];
        p.state = PairState::InProgress;
        p.txn = Some(tid);
        self.send_on(local, remote, data);
    }

    fn enqueue_triggered(&mut self, pidx: usize) {
        if !self.triggered.contains(&pidx) {
            self.triggered.push_back(pidx);
        }
        self.kick();
    }

    /// 検査を始められるようにする（次の poll_timeout がすぐの時刻を返す）。
    fn kick(&mut self) {
        if self.next_check.is_none() {
            self.next_check = self.last_now;
        }
    }

    fn switch_role(&mut self, role: IceRole) {
        if self.role == role {
            return;
        }
        self.role = role;
        for i in 0..self.pairs.len() {
            let (l, r) = (self.pairs[i].local, self.pairs[i].remote);
            self.pairs[i].priority = pair_priority(
                role,
                self.locals[l].cand.priority,
                self.remotes[r].cand.priority,
            );
        }
        self.nomination_at = None;
        self.events.push_back(IceEvent::RoleChanged(role));
    }

    /// pair を作る（あれば番号を返す）。`force` なら上限を越えても作る（受けた検査による pair）。
    fn add_pair(
        &mut self,
        now: Instant,
        local: usize,
        remote: usize,
        force: bool,
    ) -> Option<usize> {
        let l = &self.locals[local];
        if l.dead || !matches!(l.cand.kind, CandidateKind::Host | CandidateKind::Relayed) {
            return None;
        }
        let laddr = l.cand.addr()?;
        let raddr = self.remotes[remote].addr;
        if !compatible(&laddr, &raddr) {
            return None;
        }
        if let Some(i) = self
            .pairs
            .iter()
            .position(|p| p.local == local && p.remote == remote)
        {
            return Some(i);
        }
        if self.pairs.len() >= self.cfg.max_pairs
            && !(force && self.pairs.len() < self.cfg.max_pairs * 2)
        {
            return None;
        }
        let priority = pair_priority(
            self.role,
            l.cand.priority,
            self.remotes[remote].cand.priority,
        );
        let foundation = (
            l.cand.foundation.clone(),
            self.remotes[remote].cand.foundation.clone(),
        );
        // 同じ foundation の pair が既にあれば Frozen、無ければ Waiting（RFC 8445 6.1.2.6）
        let state = if self.pairs.iter().any(|p| p.foundation == foundation) {
            PairState::Frozen
        } else {
            PairState::Waiting
        };
        if let Some(t) = l.turn {
            let _ = self.turns[t].client.create_permission(now, raddr.ip());
            self.drain_turn(now, t);
        }
        self.pairs.push(Pair {
            local,
            remote,
            priority,
            state,
            foundation,
            nominated: false,
            use_candidate_received: false,
            nominating: false,
            txn: None,
        });
        if self.checking_since.is_none() && self.remote_creds.is_some() {
            self.checking_since = Some(now);
        }
        self.kick();
        Some(self.pairs.len() - 1)
    }

    fn pair_new_local(&mut self, now: Instant, local: usize) {
        for r in 0..self.remotes.len() {
            self.add_pair(now, local, r, false);
        }
    }

    fn authorize(&mut self, local: usize, remote: SocketAddr) {
        if self.authorized.len() < self.cfg.max_pairs * 4 {
            self.authorized.insert((local, remote));
        }
    }

    fn send_on(&mut self, local: usize, remote: SocketAddr, data: Bytes) {
        let l = &self.locals[local];
        match l.turn {
            None => self.transmits.push_back(Transmit {
                local: l.base,
                remote,
                data,
            }),
            Some(t) => {
                if let Some(tx) = self.turns[t].client.send(remote, &data) {
                    self.transmits.push_back(tx);
                }
            }
        }
    }

    fn drain_turn(&mut self, now: Instant, t: usize) {
        while let Some(tx) = self.turns[t].client.poll_transmit() {
            self.transmits.push_back(tx);
        }
        while let Some(ev) = self.turns[t].client.poll_event() {
            match ev {
                TurnClientEvent::Allocated {
                    relayed, mapped, ..
                } => {
                    let slot = &self.turns[t];
                    let base_local = slot.base_local;
                    let server = slot.client.server();
                    if let Some(m) = mapped {
                        self.add_srflx(base_local, m, Some(server.ip()));
                    }
                    let base = self.locals[base_local].base;
                    let local_pref = self.locals[base_local].local_pref.saturating_sub(t as u16);
                    let cand = Candidate {
                        foundation: compute_foundation(
                            CandidateKind::Relayed,
                            base.ip(),
                            Some(server.ip()),
                            &Transport::Udp,
                        ),
                        component: 1,
                        transport: Transport::Udp,
                        priority: compute_priority(CandidateKind::Relayed, local_pref, 1),
                        address: CandidateAddress::Ip(relayed),
                        kind: CandidateKind::Relayed,
                        related: mapped,
                        tcptype: None,
                        extensions: Vec::new(),
                    };
                    let idx = self.push_local(cand, base, Some(t), local_pref);
                    self.turns[t].relay_local = Some(idx);
                    self.turns[t].done = true;
                    if !self.closed {
                        self.pair_new_local(now, idx);
                    }
                }
                TurnClientEvent::AllocationFailed { .. } => self.turns[t].done = true,
                TurnClientEvent::AllocationLost => {
                    if let Some(idx) = self.turns[t].relay_local {
                        self.locals[idx].dead = true;
                        for p in self.pairs.iter_mut().filter(|p| p.local == idx) {
                            p.state = PairState::Failed;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn set_state(&mut self, s: IceState) {
        if self.state != s {
            self.state = s;
            self.events.push_back(IceEvent::StateChanged(s));
        }
    }

    fn update_state(&mut self, now: Instant) {
        if self.closed {
            return self.set_state(IceState::Closed);
        }
        let new = if let Some(s) = &self.selected {
            if s.consent_lost {
                IceState::Failed
            } else if now >= s.last_consent + self.cfg.disconnected_timeout {
                IceState::Disconnected
            } else if self.remote_eoc
                && !self.has_check_work()
                && !self
                    .pairs
                    .iter()
                    .any(|p| matches!(p.state, PairState::InProgress))
            {
                IceState::Completed
            } else {
                IceState::Connected
            }
        } else if self.checking_since.is_none()
            && (self.remote_creds.is_none() || self.pairs.is_empty())
        {
            IceState::New
        } else {
            // trickle: 相手の end-of-candidates と自分の収集の完了の後で、全部失敗したら Failed（RFC 8838 8）。
            let all_failed = self.pairs.iter().all(|p| p.state == PairState::Failed)
                && self.triggered.is_empty();
            if self.remote_eoc
                && self.gathers.is_empty()
                && self.turns.iter().all(|t| t.done)
                && all_failed
                && !self.cfg.lite
            {
                IceState::Failed
            } else {
                IceState::Checking
            }
        };
        self.set_state(new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_priority_formula() {
        // RFC 8445 6.1.2.3
        let g = 2130706431u32;
        let d = 1694498815u32;
        let p = pair_priority(IceRole::Controlling, g, d);
        assert_eq!(p, (1u64 << 32) * d as u64 + 2 * g as u64 + 1);
        let q = pair_priority(IceRole::Controlled, d, g);
        assert_eq!(p, q);
    }

    #[test]
    fn credentials() {
        let c = Credentials::random().unwrap();
        c.validate().unwrap();
        assert!(!format!("{c:?}").contains(&c.pwd));
        assert!(
            Credentials {
                ufrag: "abc".into(),
                pwd: "x".repeat(22)
            }
            .validate()
            .is_err()
        );
        assert!(
            Credentials {
                ufrag: "abcd".into(),
                pwd: "x".repeat(21)
            }
            .validate()
            .is_err()
        );
        assert!(
            Credentials {
                ufrag: "ab:d".into(),
                pwd: "x".repeat(22)
            }
            .validate()
            .is_err()
        );
    }
}
