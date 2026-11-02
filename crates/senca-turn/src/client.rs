//! TURN クライアント（RFC 8656、UDP）。sans-io。
//!
//! 駆動のしかた:
//!
//! 1. [`TurnClient::allocate`] で Allocate を始める（最初は認証なしで送り、401 の REALM・NONCE で鍵を作って送り直す）。
//! 2. サーバーから受けたデータグラムは [`TurnClient::handle_input`] に渡す。TURN のものでなければ
//!    [`TurnInput::NotMine`] が返る（同じソケットで STUN Binding などを扱う呼び出し側に回す）。
//!    peer からの中継データは [`TurnInput::Data`] で返る。
//! 3. [`TurnClient::poll_transmit`] を空になるまで送り、[`TurnClient::poll_event`] でイベントを見る。
//! 4. [`TurnClient::poll_timeout`] の時刻に [`TurnClient::handle_timeout`] を呼ぶ（再送・Refresh・permission と
//!    channel の更新）。
//!
//! permission は [`TurnClient::create_permission`] で作り、以後 240 秒ごとに自動で更新する。
//! channel は [`TurnClient::channel_bind`] で作り、540 秒ごとに更新する。[`TurnClient::send`] は channel があれば
//! ChannelData、無ければ Send indication で送る。

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use bytes::Bytes;
use senca_stun::{
    AddressFamily, Attribute, Class, Message, MessageBuilder, Method, PasswordAlgorithm,
    Retransmission, RetransmitAction, RtoConfig, StunError, TransactionId, is_stun, long_term_key,
};

use crate::channel::{CHANNEL_MAX, CHANNEL_MIN, ChannelData, is_channel_data};
use crate::{PROTO_UDP, Transmit};

/// permission を更新する間隔（寿命 300 秒より前）。
const PERMISSION_REFRESH: Duration = Duration::from_secs(240);
/// channel を更新する間隔（寿命 600 秒より前）。
const CHANNEL_REFRESH: Duration = Duration::from_secs(540);
/// 438（stale nonce）・401 で送り直す回数の上限。
const MAX_AUTH_RETRIES: u8 = 3;
/// 同時に持つ permission・channel の上限。
const MAX_PEERS: usize = 1024;

/// クライアントの設定。
#[derive(Clone)]
pub struct TurnClientConfig {
    /// 使うローカルソケットのアドレス（[`Transmit::local`] になる）。
    pub local: SocketAddr,
    /// TURN サーバー。
    pub server: SocketAddr,
    pub username: String,
    pub password: String,
    pub software: Option<String>,
    /// 要求する寿命（秒）。
    pub lifetime: u32,
    /// relay アドレスの族（`None` ならサーバーの既定＝IPv4）。
    pub family: Option<AddressFamily>,
    pub rto: RtoConfig,
}

impl TurnClientConfig {
    pub fn new(local: SocketAddr, server: SocketAddr, username: &str, password: &str) -> Self {
        TurnClientConfig {
            local,
            server,
            username: username.to_string(),
            password: password.to_string(),
            software: None,
            lifetime: 600,
            family: None,
            rto: RtoConfig::default(),
        }
    }
}

impl fmt::Debug for TurnClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnClientConfig")
            .field("local", &self.local)
            .field("server", &self.server)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("lifetime", &self.lifetime)
            .field("family", &self.family)
            .finish()
    }
}

/// クライアントのイベント。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnClientEvent {
    /// allocation ができた。
    Allocated {
        relayed: SocketAddr,
        mapped: Option<SocketAddr>,
        lifetime: Duration,
    },
    /// allocation に失敗した（`code` が `None` なら応答なし）。
    AllocationFailed {
        code: Option<u16>,
        reason: String,
    },
    /// Refresh できた。
    Refreshed {
        lifetime: Duration,
    },
    /// allocation を失った（Refresh の失敗・期限切れ）。
    AllocationLost,
    /// 解放した（Refresh 0 の完了）。
    Closed,
    PermissionCreated {
        peer: IpAddr,
    },
    PermissionFailed {
        peer: IpAddr,
        code: Option<u16>,
    },
    ChannelBound {
        peer: SocketAddr,
        channel: u16,
    },
    ChannelBindFailed {
        peer: SocketAddr,
        code: Option<u16>,
    },
}

/// [`TurnClient::handle_input`] の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnInput {
    /// TURN のものではない（呼び出し側で扱う）。
    NotMine,
    /// TURN の制御メッセージとして処理した。
    Consumed,
    /// peer からの中継データ。
    Data { peer: SocketAddr, data: Bytes },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Allocating,
    Allocated,
    Closing,
    Closed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Allocate,
    Refresh(u32),
    CreatePermission(Vec<IpAddr>),
    ChannelBind(SocketAddr, u16),
}

struct Pending {
    kind: Kind,
    data: Bytes,
    retrans: Retransmission,
    auth_retries: u8,
    authenticated: bool,
}

struct Auth {
    realm: String,
    nonce: String,
    key: Vec<u8>,
    algorithms: Option<Vec<PasswordAlgorithm>>,
    algorithm: Option<PasswordAlgorithm>,
}

#[derive(Debug, Clone, Copy)]
struct Perm {
    /// 有効になった（成功応答を受けた）か。
    installed: bool,
    pending: bool,
    refresh_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct Chan {
    number: u16,
    bound: bool,
    pending: bool,
    refresh_at: Option<Instant>,
}

/// TURN クライアント。
pub struct TurnClient {
    cfg: TurnClientConfig,
    state: State,
    auth: Option<Auth>,
    pending: HashMap<TransactionId, Pending>,
    relayed: Option<SocketAddr>,
    mapped: Option<SocketAddr>,
    refresh_at: Option<Instant>,
    expires_at: Option<Instant>,
    perms: HashMap<IpAddr, Perm>,
    chans: HashMap<SocketAddr, Chan>,
    by_number: HashMap<u16, SocketAddr>,
    next_channel: u16,
    transmits: VecDeque<Transmit>,
    events: VecDeque<TurnClientEvent>,
}

impl fmt::Debug for TurnClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnClient")
            .field("server", &self.cfg.server)
            .field("state", &self.state)
            .field("relayed", &self.relayed)
            .finish_non_exhaustive()
    }
}

impl TurnClient {
    pub fn new(cfg: TurnClientConfig) -> Self {
        TurnClient {
            cfg,
            state: State::Idle,
            auth: None,
            pending: HashMap::new(),
            relayed: None,
            mapped: None,
            refresh_at: None,
            expires_at: None,
            perms: HashMap::new(),
            chans: HashMap::new(),
            by_number: HashMap::new(),
            next_channel: CHANNEL_MIN,
            transmits: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    pub fn server(&self) -> SocketAddr {
        self.cfg.server
    }

    pub fn local(&self) -> SocketAddr {
        self.cfg.local
    }

    /// relay アドレス（allocation ができていれば）。
    pub fn relayed_address(&self) -> Option<SocketAddr> {
        if self.state == State::Allocated {
            self.relayed
        } else {
            None
        }
    }

    /// サーバーから見たクライアントのアドレス（XOR-MAPPED-ADDRESS）。
    pub fn mapped_address(&self) -> Option<SocketAddr> {
        self.mapped
    }

    pub fn is_allocated(&self) -> bool {
        self.state == State::Allocated
    }

    /// permission が有効か。
    pub fn has_permission(&self, ip: IpAddr) -> bool {
        self.perms.get(&ip).is_some_and(|p| p.installed)
    }

    /// peer に結び付いた channel 番号（bind 済みなら）。
    pub fn channel(&self, peer: SocketAddr) -> Option<u16> {
        self.chans.get(&peer).filter(|c| c.bound).map(|c| c.number)
    }

    /// Allocate を始める。
    pub fn allocate(&mut self, now: Instant) -> Result<(), StunError> {
        if self.state != State::Idle {
            return Ok(());
        }
        self.state = State::Allocating;
        self.request(now, Kind::Allocate, 0)
    }

    /// allocation を解放する（Refresh LIFETIME=0）。
    pub fn deallocate(&mut self, now: Instant) -> Result<(), StunError> {
        match self.state {
            State::Allocated => {
                self.state = State::Closing;
                self.request(now, Kind::Refresh(0), 0)
            }
            State::Allocating | State::Idle => {
                self.pending.clear();
                self.state = State::Closed;
                self.events.push_back(TurnClientEvent::Closed);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// peer の IP への permission を作る（既にあれば何もしない）。以後自動で更新する。
    pub fn create_permission(&mut self, now: Instant, peer: IpAddr) -> Result<(), StunError> {
        if self.perms.contains_key(&peer) {
            return Ok(());
        }
        if self.perms.len() >= MAX_PEERS {
            return Err(StunError::TooLarge);
        }
        self.perms.insert(
            peer,
            Perm {
                installed: false,
                pending: false,
                refresh_at: None,
            },
        );
        if self.state == State::Allocated {
            self.send_permission(now, peer)?;
        }
        Ok(())
    }

    fn send_permission(&mut self, now: Instant, peer: IpAddr) -> Result<(), StunError> {
        if let Some(p) = self.perms.get_mut(&peer) {
            p.pending = true;
            p.refresh_at = None;
        }
        self.request(now, Kind::CreatePermission(vec![peer]), 0)
    }

    /// peer に channel を結ぶ（permission も兼ねる）。既にあれば何もしない。
    pub fn channel_bind(&mut self, now: Instant, peer: SocketAddr) -> Result<(), StunError> {
        if self.chans.contains_key(&peer) {
            return Ok(());
        }
        if self.chans.len() >= MAX_PEERS || self.next_channel > CHANNEL_MAX {
            return Err(StunError::TooLarge);
        }
        let number = self.next_channel;
        self.next_channel += 1;
        self.chans.insert(
            peer,
            Chan {
                number,
                bound: false,
                pending: false,
                refresh_at: None,
            },
        );
        self.by_number.insert(number, peer);
        if self.state == State::Allocated {
            self.send_channel_bind(now, peer)?;
        }
        Ok(())
    }

    fn send_channel_bind(&mut self, now: Instant, peer: SocketAddr) -> Result<(), StunError> {
        let Some(c) = self.chans.get_mut(&peer) else {
            return Ok(());
        };
        c.pending = true;
        c.refresh_at = None;
        let n = c.number;
        self.request(now, Kind::ChannelBind(peer, n), 0)
    }

    /// peer へ送る。allocation が無ければ `None`。
    pub fn send(&self, peer: SocketAddr, data: &[u8]) -> Option<Transmit> {
        if self.state != State::Allocated {
            return None;
        }
        let bytes = if let Some(n) = self.channel(peer) {
            ChannelData::encode(n, data, false)
        } else {
            let tid = TransactionId::random().ok()?;
            MessageBuilder::new(Class::Indication, Method::SEND, tid)
                .attr(Attribute::XorPeerAddress(peer))
                .attr(Attribute::Data(Bytes::copy_from_slice(data)))
                .build()
                .ok()?
        };
        Some(Transmit {
            local: self.cfg.local,
            remote: self.cfg.server,
            data: bytes,
        })
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmits.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<TurnClientEvent> {
        self.events.pop_front()
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        let mut t: Option<Instant> = None;
        let mut upd = |x: Option<Instant>| {
            if let Some(x) = x {
                t = Some(t.map_or(x, |t| t.min(x)));
            }
        };
        for p in self.pending.values() {
            upd(p.retrans.poll_timeout());
        }
        if self.state == State::Allocated {
            upd(self.refresh_at);
            upd(self.expires_at);
            for p in self.perms.values() {
                upd(p.refresh_at);
            }
            for c in self.chans.values() {
                upd(c.refresh_at);
            }
        }
        t
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        // 再送
        let mut timed_out = Vec::new();
        for (tid, p) in self.pending.iter_mut() {
            match p.retrans.handle_timeout(now) {
                RetransmitAction::Wait => {}
                RetransmitAction::Retransmit => self.transmits.push_back(Transmit {
                    local: self.cfg.local,
                    remote: self.cfg.server,
                    data: p.data.clone(),
                }),
                RetransmitAction::TimedOut => timed_out.push(*tid),
            }
        }
        for tid in timed_out {
            if let Some(p) = self.pending.remove(&tid) {
                self.on_failure(now, p.kind, None, "timeout".into());
            }
        }
        if self.state != State::Allocated {
            return;
        }
        if self.expires_at.is_some_and(|e| now >= e) {
            self.lose();
            return;
        }
        if self.refresh_at.is_some_and(|r| now >= r) {
            self.refresh_at = None;
            let lt = self.cfg.lifetime;
            let _ = self.request(now, Kind::Refresh(lt), 0);
        }
        let due: Vec<IpAddr> = self
            .perms
            .iter()
            .filter(|(_, p)| p.refresh_at.is_some_and(|r| now >= r))
            .map(|(ip, _)| *ip)
            .collect();
        for ip in due {
            let _ = self.send_permission(now, ip);
        }
        let due: Vec<SocketAddr> = self
            .chans
            .iter()
            .filter(|(_, c)| c.refresh_at.is_some_and(|r| now >= r))
            .map(|(a, _)| *a)
            .collect();
        for a in due {
            let _ = self.send_channel_bind(now, a);
        }
    }

    /// サーバーからのデータグラムを処理する。
    pub fn handle_input(&mut self, now: Instant, from: SocketAddr, data: Bytes) -> TurnInput {
        if from != self.cfg.server {
            return TurnInput::NotMine;
        }
        if is_channel_data(&data) {
            let Ok(cd) = ChannelData::parse(data) else {
                return TurnInput::Consumed;
            };
            return match self.by_number.get(&cd.number) {
                Some(peer) if self.chans.get(peer).is_some_and(|c| c.bound) => TurnInput::Data {
                    peer: *peer,
                    data: cd.data,
                },
                _ => TurnInput::Consumed,
            };
        }
        if !is_stun(&data) {
            return TurnInput::NotMine;
        }
        let Ok(msg) = Message::parse(data) else {
            return TurnInput::NotMine;
        };
        match msg.class {
            Class::Indication if msg.method == Method::DATA => {
                let (Some(peer), Some(d)) = (msg.xor_peer_addresses().next(), msg.data()) else {
                    return TurnInput::Consumed;
                };
                if !self.has_permission(peer.ip()) {
                    return TurnInput::Consumed;
                }
                TurnInput::Data {
                    peer,
                    data: d.clone(),
                }
            }
            Class::Success | Class::Error => {
                let Some(p) = self.pending.remove(&msg.transaction_id) else {
                    return TurnInput::NotMine;
                };
                self.on_response(now, p, &msg);
                TurnInput::Consumed
            }
            _ => TurnInput::NotMine,
        }
    }

    fn build(&self, kind: &Kind, tid: TransactionId) -> Result<(Bytes, bool), StunError> {
        let method = match kind {
            Kind::Allocate => Method::ALLOCATE,
            Kind::Refresh(_) => Method::REFRESH,
            Kind::CreatePermission(_) => Method::CREATE_PERMISSION,
            Kind::ChannelBind(..) => Method::CHANNEL_BIND,
        };
        let mut b = MessageBuilder::new(Class::Request, method, tid);
        match kind {
            Kind::Allocate => {
                b = b
                    .attr(Attribute::RequestedTransport(PROTO_UDP))
                    .attr(Attribute::Lifetime(self.cfg.lifetime));
                if let Some(f) = self.cfg.family
                    && f != AddressFamily::V4
                {
                    b = b.attr(Attribute::RequestedAddressFamily(f));
                }
            }
            Kind::Refresh(lt) => b = b.attr(Attribute::Lifetime(*lt)),
            Kind::CreatePermission(ips) => {
                for ip in ips {
                    b = b.attr(Attribute::XorPeerAddress(SocketAddr::new(*ip, 0)));
                }
            }
            Kind::ChannelBind(peer, n) => {
                b = b
                    .attr(Attribute::ChannelNumber(*n))
                    .attr(Attribute::XorPeerAddress(*peer));
            }
        }
        if let Some(s) = &self.cfg.software {
            b = b.attr(Attribute::Software(s.clone()));
        }
        let authenticated = self.auth.is_some();
        if let Some(a) = &self.auth {
            b = b
                .attr(Attribute::Username(self.cfg.username.clone()))
                .attr(Attribute::Realm(a.realm.clone()))
                .attr(Attribute::Nonce(a.nonce.clone()));
            if let (Some(algs), Some(alg)) = (&a.algorithms, a.algorithm) {
                b = b
                    .attr(Attribute::PasswordAlgorithms(algs.clone()))
                    .attr(Attribute::PasswordAlgorithm(alg));
            }
            b = b.integrity(&a.key);
        }
        Ok((b.fingerprint().build()?, authenticated))
    }

    fn request(&mut self, now: Instant, kind: Kind, auth_retries: u8) -> Result<(), StunError> {
        let tid = TransactionId::random()?;
        let (data, authenticated) = self.build(&kind, tid)?;
        self.transmits.push_back(Transmit {
            local: self.cfg.local,
            remote: self.cfg.server,
            data: data.clone(),
        });
        let retrans = Retransmission::new(now, self.cfg.rto, None);
        self.pending.insert(
            tid,
            Pending {
                kind,
                data,
                retrans,
                auth_retries,
                authenticated,
            },
        );
        Ok(())
    }

    fn on_response(&mut self, now: Instant, p: Pending, msg: &Message) {
        // 認証付きで送った要求への応答は、MESSAGE-INTEGRITY があれば必ず検査する。成功応答には必須。
        if let Some(a) = &self.auth
            && p.authenticated
        {
            let verified = msg.verify_integrity(&a.key).is_ok();
            if msg.has_integrity() && !verified {
                // 偽の応答。捨てて再送を待つ。
                self.pending.insert(msg.transaction_id, p);
                return;
            }
            if msg.class == Class::Success && !verified {
                self.pending.insert(msg.transaction_id, p);
                return;
            }
        }
        if msg.class == Class::Error {
            let (code, reason) = msg
                .error_code()
                .map(|(c, r)| (Some(c), r.to_string()))
                .unwrap_or((None, String::new()));
            match code {
                Some(401) | Some(438) if p.auth_retries < MAX_AUTH_RETRIES => {
                    // 401 は初回（認証なし）だけ送り直す。認証付きの 401 は資格情報の誤り。
                    if code == Some(401)
                        && p.authenticated
                        && self
                            .auth
                            .as_ref()
                            .is_some_and(|a| Some(a.nonce.as_str()) == msg.nonce())
                    {
                        self.on_failure(now, p.kind, code, reason);
                        return;
                    }
                    if self.update_auth(msg) {
                        let _ = self.request(now, p.kind, p.auth_retries + 1);
                        return;
                    }
                    self.on_failure(now, p.kind, code, reason);
                }
                _ => self.on_failure(now, p.kind, code, reason),
            }
            return;
        }
        match p.kind {
            Kind::Allocate => {
                let Some(relayed) = msg.xor_relayed_address() else {
                    self.on_failure(now, Kind::Allocate, None, "no XOR-RELAYED-ADDRESS".into());
                    return;
                };
                let lifetime = Duration::from_secs(msg.lifetime().unwrap_or(600) as u64);
                self.relayed = Some(relayed);
                self.mapped = msg.mapped_address();
                self.state = State::Allocated;
                self.schedule_refresh(now, lifetime);
                self.events.push_back(TurnClientEvent::Allocated {
                    relayed,
                    mapped: self.mapped,
                    lifetime,
                });
                // allocation 前に頼まれた permission・channel を送る。
                let ips: Vec<IpAddr> = self.perms.keys().copied().collect();
                for ip in ips {
                    let _ = self.send_permission(now, ip);
                }
                let peers: Vec<SocketAddr> = self.chans.keys().copied().collect();
                for a in peers {
                    let _ = self.send_channel_bind(now, a);
                }
            }
            Kind::Refresh(0) => {
                self.state = State::Closed;
                self.clear();
                self.events.push_back(TurnClientEvent::Closed);
            }
            Kind::Refresh(_) => {
                let lifetime =
                    Duration::from_secs(msg.lifetime().unwrap_or(self.cfg.lifetime) as u64);
                self.schedule_refresh(now, lifetime);
                self.events
                    .push_back(TurnClientEvent::Refreshed { lifetime });
            }
            Kind::CreatePermission(ips) => {
                for ip in ips {
                    if let Some(perm) = self.perms.get_mut(&ip) {
                        let first = !perm.installed;
                        perm.installed = true;
                        perm.pending = false;
                        perm.refresh_at = Some(now + PERMISSION_REFRESH);
                        if first {
                            self.events
                                .push_back(TurnClientEvent::PermissionCreated { peer: ip });
                        }
                    }
                }
            }
            Kind::ChannelBind(peer, n) => {
                if let Some(c) = self.chans.get_mut(&peer) {
                    let first = !c.bound;
                    c.bound = true;
                    c.pending = false;
                    c.refresh_at = Some(now + CHANNEL_REFRESH);
                    // channel は permission も兼ねる
                    let perm = self.perms.entry(peer.ip()).or_insert(Perm {
                        installed: false,
                        pending: false,
                        refresh_at: None,
                    });
                    perm.installed = true;
                    if perm.refresh_at.is_none() && !perm.pending {
                        perm.refresh_at = Some(now + PERMISSION_REFRESH);
                    }
                    if first {
                        self.events
                            .push_back(TurnClientEvent::ChannelBound { peer, channel: n });
                    }
                }
            }
        }
    }

    fn schedule_refresh(&mut self, now: Instant, lifetime: Duration) {
        // 期限の 60 秒前（短い寿命なら半分）に更新する。
        let margin = if lifetime > Duration::from_secs(120) {
            Duration::from_secs(60)
        } else {
            lifetime / 2
        };
        self.refresh_at = Some(now + lifetime - margin);
        self.expires_at = Some(now + lifetime);
    }

    /// 401・438 から REALM・NONCE を取り、鍵を作る。使えなければ偽。
    fn update_auth(&mut self, msg: &Message) -> bool {
        let Some(nonce) = msg.nonce() else {
            return false;
        };
        let realm = match (msg.realm(), &self.auth) {
            (Some(r), _) => r.to_string(),
            (None, Some(a)) => a.realm.clone(),
            (None, None) => return false,
        };
        // PASSWORD-ALGORITHMS（RFC 8489）を提示されたら MD5 を優先し、無ければ SHA-256。
        let algorithms = msg.attributes.iter().find_map(|a| match a {
            Attribute::PasswordAlgorithms(v) => Some(v.clone()),
            _ => None,
        });
        let algorithm = algorithms.as_ref().and_then(|v| {
            if v.contains(&PasswordAlgorithm::Md5) {
                Some(PasswordAlgorithm::Md5)
            } else if v.contains(&PasswordAlgorithm::Sha256) {
                Some(PasswordAlgorithm::Sha256)
            } else {
                None
            }
        });
        if algorithms.is_some() && algorithm.is_none() {
            return false;
        }
        let key = long_term_key(
            &self.cfg.username,
            &realm,
            &self.cfg.password,
            algorithm.unwrap_or(PasswordAlgorithm::Md5),
        );
        self.auth = Some(Auth {
            realm,
            nonce: nonce.to_string(),
            key,
            algorithms,
            algorithm,
        });
        true
    }

    fn on_failure(&mut self, now: Instant, kind: Kind, code: Option<u16>, reason: String) {
        match kind {
            Kind::Allocate => {
                self.state = State::Failed;
                self.events
                    .push_back(TurnClientEvent::AllocationFailed { code, reason });
            }
            Kind::Refresh(0) => {
                self.state = State::Closed;
                self.clear();
                self.events.push_back(TurnClientEvent::Closed);
            }
            Kind::Refresh(_) => {
                // 応答が無いだけなら期限まで待つ。誤り応答なら失った。
                if code.is_some() {
                    self.lose();
                } else if self.state == State::Allocated {
                    // 期限の手前でもう一度だけ試す。
                    if let Some(e) = self.expires_at
                        && e > now + Duration::from_secs(5)
                    {
                        self.refresh_at = Some(now + Duration::from_secs(5).min(e - now));
                    }
                }
            }
            Kind::CreatePermission(ips) => {
                for ip in ips {
                    self.perms.remove(&ip);
                    self.events
                        .push_back(TurnClientEvent::PermissionFailed { peer: ip, code });
                }
            }
            Kind::ChannelBind(peer, n) => {
                self.chans.remove(&peer);
                self.by_number.remove(&n);
                self.events
                    .push_back(TurnClientEvent::ChannelBindFailed { peer, code });
            }
        }
    }

    fn lose(&mut self) {
        self.state = State::Failed;
        self.clear();
        self.events.push_back(TurnClientEvent::AllocationLost);
    }

    fn clear(&mut self) {
        self.pending.clear();
        self.perms.clear();
        self.chans.clear();
        self.by_number.clear();
        self.refresh_at = None;
        self.expires_at = None;
        self.relayed = None;
    }

    /// 応答を待っている要求があるか（試験用）。
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}
