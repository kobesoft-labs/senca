//! TURN サーバー（RFC 8656、UDP の待ち受けと UDP の relay）。sans-io。
//!
//! 駆動のしかた（I/O 層）:
//!
//! 1. 待ち受けソケットで受けたら [`TurnServer::handle_client`]、relay ソケットで受けたら
//!    [`TurnServer::handle_peer`] を呼ぶ。
//! 2. [`TurnServer::poll_transmit`] が空になるまで取り出して、`Transmit::local` のソケットから送る。
//! 3. [`TurnServer::poll_event`] の `AllocationCreated` で relay ソケットを bind し、`AllocationClosed` で閉じる。
//!    bind できなければ [`TurnServer::relay_unavailable`] を呼ぶ（allocation を消す）。
//! 4. [`TurnServer::poll_timeout`] の時刻に [`TurnServer::handle_timeout`] を呼ぶ。
//! 5. 壁時計（UNIX 秒）を [`TurnServer::set_unix_time`] で時々渡す（REST 資格情報の期限の判定に使う。
//!    一度も渡さなければ REST 資格情報は使えない＝閉じた側に倒す）。
//!
//! 守ること:
//!
//! - 認証は長期資格情報（MD5 鍵）。nonce は待ち受けごとの秘密による HMAC で作り、状態を持たない。
//!   送信元アドレスに結び付け、期限（既定 600 秒）を過ぎたら 438。
//! - relay 先は [`PeerPolicy`] で検査し、拒否なら CreatePermission・ChannelBind は 403、
//!   Send・ChannelData は捨てる（仕様 9.4: open relay にしない）。
//! - 認証前の応答（401・438・Binding など）は送信元 IP ごとの token bucket で絞り、SOFTWARE を付けない
//!   （UDP の応答増幅を防ぐ）。壊れた入力・要求でない STUN には応えない。
//! - 未対応: TCP allocation（RFC 6062）、dual allocation（ADDITIONAL-ADDRESS-FAMILY は IPv4 だけ割り当てて
//!   ADDRESS-ERROR-CODE 440 を返す）、EVEN-PORT の R bit・RESERVATION-TOKEN（508）、DONT-FRAGMENT（420）、
//!   ICMP の中継、PASSWORD-ALGORITHMS（MD5 だけ）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use bytes::Bytes;
use hmac::{Hmac, Mac};
use senca_stun::attr::attr_type;
use senca_stun::{
    AddressFamily, Attribute, Class, Message, MessageBuilder, Method, PasswordAlgorithm, StunError,
    TransactionId, long_term_key,
};
use subtle::ConstantTimeEq;

use crate::channel::{ChannelData, is_valid_channel};
use crate::credentials::{rest_api_expiry, rest_api_password};
use crate::policy::PeerPolicy;
use crate::{PROTO_UDP, Transmit};

/// 固定の利用者。
#[derive(Clone)]
pub struct StaticUser {
    pub username: String,
    pub password: String,
    pub tenant: Option<String>,
}

impl fmt::Debug for StaticUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StaticUser")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("tenant", &self.tenant)
            .finish()
    }
}

/// REST API 方式の共有秘密（複数持てば鍵の入れ替えができる）。
#[derive(Clone)]
pub struct RestSecret {
    pub secret: Vec<u8>,
    /// この秘密で認証した利用者が属するテナント。
    pub tenant: Option<String>,
}

impl fmt::Debug for RestSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestSecret")
            .field("secret", &"<redacted>")
            .field("tenant", &self.tenant)
            .finish()
    }
}

/// relay に使う IP。`external` は応答で知らせるアドレス（1:1 NAT の外側）。無ければ `local`。
#[derive(Debug, Clone, Copy)]
pub struct RelayIp {
    pub local: IpAddr,
    pub external: Option<IpAddr>,
}

/// 上限。
#[derive(Debug, Clone, Copy)]
pub struct Quotas {
    /// サーバー全体の allocation 数（超えたら 508）。
    pub max_allocations: usize,
    /// 利用者ごと（超えたら 486）。
    pub per_user: usize,
    /// テナントごと（超えたら 486）。
    pub per_tenant: usize,
    /// allocation ごとの permission 数（超えたら 508）。
    pub permissions_per_allocation: usize,
    /// allocation ごとの channel 数（超えたら 508）。
    pub channels_per_allocation: usize,
}

impl Default for Quotas {
    fn default() -> Self {
        Quotas {
            max_allocations: 10_000,
            per_user: 64,
            per_tenant: 1_000,
            permissions_per_allocation: 64,
            channels_per_allocation: 64,
        }
    }
}

/// 認証前の応答の速度制限（送信元 IP ごとの token bucket）。
#[derive(Debug, Clone, Copy)]
pub struct RateLimit {
    pub burst: u32,
    pub per_second: u32,
    /// 覚えておく IP の数の上限。
    pub max_entries: usize,
}

impl Default for RateLimit {
    fn default() -> Self {
        RateLimit {
            burst: 20,
            per_second: 10,
            max_entries: 10_000,
        }
    }
}

/// サーバーの設定。
#[derive(Debug, Clone)]
pub struct TurnServerConfig {
    pub realm: String,
    pub users: Vec<StaticUser>,
    pub rest_secrets: Vec<RestSecret>,
    pub relay_v4: Option<RelayIp>,
    pub relay_v6: Option<RelayIp>,
    /// relay ポートの範囲（両端を含む）。
    pub port_min: u16,
    pub port_max: u16,
    pub peer_policy: PeerPolicy,
    pub quotas: Quotas,
    pub default_lifetime: Duration,
    pub max_lifetime: Duration,
    pub permission_lifetime: Duration,
    pub channel_lifetime: Duration,
    /// channel が切れてから同じ番号・相手を別の組で使えない期間（RFC 8656 12: 5 分）。
    pub channel_cooldown: Duration,
    pub nonce_lifetime: Duration,
    /// 認証済みの応答にだけ付ける。
    pub software: Option<String>,
    pub unauth_rate: RateLimit,
}

impl Default for TurnServerConfig {
    fn default() -> Self {
        TurnServerConfig {
            realm: "senca".into(),
            users: Vec::new(),
            rest_secrets: Vec::new(),
            relay_v4: None,
            relay_v6: None,
            port_min: 49152,
            port_max: 65535,
            peer_policy: PeerPolicy::strict(),
            quotas: Quotas::default(),
            default_lifetime: Duration::from_secs(600),
            max_lifetime: Duration::from_secs(3600),
            permission_lifetime: Duration::from_secs(crate::PERMISSION_LIFETIME_SECS),
            channel_lifetime: Duration::from_secs(crate::CHANNEL_LIFETIME_SECS),
            channel_cooldown: Duration::from_secs(300),
            nonce_lifetime: Duration::from_secs(600),
            software: None,
            unauth_rate: RateLimit::default(),
        }
    }
}

/// サーバーのイベント（I/O 層が relay ソケットを開閉する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnServerEvent {
    /// `relay`（ローカルのアドレス）で UDP を bind すること。
    AllocationCreated {
        relay: SocketAddr,
        client: SocketAddr,
        local: SocketAddr,
        username: String,
    },
    /// `relay` を閉じること（期限切れ・Refresh 0）。
    AllocationClosed { relay: SocketAddr },
}

/// 送信キューの上限（I/O 層が取り出さないときに無制限に溜めない）。
const MAX_QUEUE: usize = 65_536;

#[derive(Debug, Clone, Copy)]
struct Channel {
    peer: SocketAddr,
    expires: Instant,
}

struct Allocation {
    client: SocketAddr,
    local: SocketAddr,
    relay_local: SocketAddr,
    username: String,
    tenant: Option<String>,
    expires: Instant,
    permissions: HashMap<IpAddr, Instant>,
    channels: HashMap<u16, Channel>,
    peer_channels: HashMap<SocketAddr, u16>,
    last: Option<(TransactionId, Bytes)>,
}

struct Auth {
    username: String,
    tenant: Option<String>,
    key: Vec<u8>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// 誤り応答の中身。
struct Fail {
    code: u16,
    reason: &'static str,
    extra: Vec<Attribute>,
}

fn fail(code: u16, reason: &'static str) -> Fail {
    Fail {
        code,
        reason,
        extra: Vec::new(),
    }
}

/// TURN サーバー。
pub struct TurnServer {
    cfg: TurnServerConfig,
    users: HashMap<String, (Vec<u8>, Option<String>)>,
    nonce_secret: [u8; 32],
    epoch: Option<Instant>,
    unix_time: Option<u64>,
    next_id: u64,
    allocs: HashMap<u64, Allocation>,
    by_tuple: HashMap<(SocketAddr, SocketAddr), u64>,
    by_relay: HashMap<SocketAddr, u64>,
    per_user: HashMap<String, usize>,
    per_tenant: HashMap<String, usize>,
    rate: HashMap<IpAddr, Bucket>,
    port_seed: u32,
    transmits: VecDeque<Transmit>,
    events: VecDeque<TurnServerEvent>,
}

impl fmt::Debug for TurnServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnServer")
            .field("realm", &self.cfg.realm)
            .field("allocations", &self.allocs.len())
            .finish_non_exhaustive()
    }
}

impl TurnServer {
    /// 作る。nonce の秘密を OS の乱数源から取る（取れなければ `Err`）。
    pub fn new(cfg: TurnServerConfig) -> Result<TurnServer, StunError> {
        let mut nonce_secret = [0u8; 32];
        getrandom::fill(&mut nonce_secret).map_err(|_| StunError::Random)?;
        let mut seed = [0u8; 4];
        getrandom::fill(&mut seed).map_err(|_| StunError::Random)?;
        let users = cfg
            .users
            .iter()
            .map(|u| {
                let key =
                    long_term_key(&u.username, &cfg.realm, &u.password, PasswordAlgorithm::Md5);
                (u.username.clone(), (key, u.tenant.clone()))
            })
            .collect();
        Ok(TurnServer {
            cfg,
            users,
            nonce_secret,
            epoch: None,
            unix_time: None,
            next_id: 1,
            allocs: HashMap::new(),
            by_tuple: HashMap::new(),
            by_relay: HashMap::new(),
            per_user: HashMap::new(),
            per_tenant: HashMap::new(),
            rate: HashMap::new(),
            port_seed: u32::from_be_bytes(seed),
            transmits: VecDeque::new(),
            events: VecDeque::new(),
        })
    }

    /// 壁時計（UNIX 秒）。REST 資格情報の期限の判定に使う。
    pub fn set_unix_time(&mut self, secs: u64) {
        self.unix_time = Some(secs);
    }

    /// 生きている allocation の数。
    pub fn allocation_count(&self) -> usize {
        self.allocs.len()
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmits.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<TurnServerEvent> {
        self.events.pop_front()
    }

    /// 次に `handle_timeout` を呼ぶべき時刻。
    pub fn poll_timeout(&self) -> Option<Instant> {
        let mut t: Option<Instant> = None;
        let mut upd = |x: Instant| t = Some(t.map_or(x, |t| t.min(x)));
        for a in self.allocs.values() {
            upd(a.expires);
            for p in a.permissions.values() {
                upd(*p);
            }
            for c in a.channels.values() {
                upd(c.expires);
                upd(c.expires + self.cfg.channel_cooldown);
            }
        }
        t
    }

    /// 期限切れの allocation・permission・channel を片付ける。
    pub fn handle_timeout(&mut self, now: Instant) {
        let expired: Vec<u64> = self
            .allocs
            .iter()
            .filter(|(_, a)| a.expires <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.remove_allocation(id, true);
        }
        let cooldown = self.cfg.channel_cooldown;
        for a in self.allocs.values_mut() {
            a.permissions.retain(|_, e| *e > now);
            let mut gone = Vec::new();
            a.channels.retain(|_, c| {
                let keep = c.expires + cooldown > now;
                if !keep {
                    gone.push(c.peer);
                }
                keep
            });
            for p in gone {
                a.peer_channels.remove(&p);
            }
        }
        self.rate
            .retain(|_, b| now.saturating_duration_since(b.last) < Duration::from_secs(60));
    }

    /// I/O 層が relay ソケットを bind できなかった。allocation を消す（イベントは出さない）。
    pub fn relay_unavailable(&mut self, _now: Instant, relay: SocketAddr) {
        if let Some(id) = self.by_relay.get(&relay).copied() {
            self.remove_allocation(id, false);
        }
    }

    fn remove_allocation(&mut self, id: u64, emit: bool) {
        let Some(a) = self.allocs.remove(&id) else {
            return;
        };
        self.by_tuple.remove(&(a.client, a.local));
        self.by_relay.remove(&a.relay_local);
        dec(&mut self.per_user, &a.username);
        if let Some(t) = &a.tenant {
            dec(&mut self.per_tenant, t);
        }
        if emit {
            self.events.push_back(TurnServerEvent::AllocationClosed {
                relay: a.relay_local,
            });
        }
    }

    fn push(&mut self, local: SocketAddr, remote: SocketAddr, data: Bytes) {
        if self.transmits.len() < MAX_QUEUE {
            self.transmits.push_back(Transmit {
                local,
                remote,
                data,
            });
        }
    }

    // ---- 入力 ----

    /// 待ち受けソケット `local` が `from` から受けたデータグラム。
    pub fn handle_client(
        &mut self,
        now: Instant,
        local: SocketAddr,
        from: SocketAddr,
        data: Bytes,
    ) {
        let Some(&b0) = data.first() else { return };
        if (0x40..=0x4F).contains(&b0) {
            self.on_channel_data(now, local, from, data);
            return;
        }
        if b0 & 0xC0 != 0 {
            return;
        }
        let Ok(msg) = Message::parse(data) else {
            return;
        };
        match msg.class {
            Class::Request => self.on_request(now, local, from, msg),
            Class::Indication if msg.method == Method::SEND => self.on_send(now, local, from, msg),
            _ => {}
        }
    }

    /// relay ソケット `relay`（ローカルのアドレス）が相手 `from` から受けたデータグラム。
    pub fn handle_peer(&mut self, now: Instant, relay: SocketAddr, from: SocketAddr, data: Bytes) {
        let Some(&id) = self.by_relay.get(&relay) else {
            return;
        };
        let Some(a) = self.allocs.get(&id) else {
            return;
        };
        if a.expires <= now || !a.permissions.get(&from.ip()).is_some_and(|e| *e > now) {
            return;
        }
        let (client, local) = (a.client, a.local);
        let channel = a
            .peer_channels
            .get(&from)
            .copied()
            .filter(|n| a.channels.get(n).is_some_and(|c| c.expires > now));
        let out = match channel {
            Some(n) => {
                if data.len() > 0xFFFF {
                    return;
                }
                ChannelData::encode(n, &data, false)
            }
            None => {
                let Ok(tid) = TransactionId::random() else {
                    return;
                };
                match MessageBuilder::new(Class::Indication, Method::DATA, tid)
                    .attr(Attribute::XorPeerAddress(from))
                    .attr(Attribute::Data(data))
                    .build()
                {
                    Ok(b) => b,
                    Err(_) => return,
                }
            }
        };
        self.push(local, client, out);
    }

    fn on_channel_data(&mut self, now: Instant, local: SocketAddr, from: SocketAddr, data: Bytes) {
        let Ok(cd) = ChannelData::parse(data) else {
            return;
        };
        let Some(&id) = self.by_tuple.get(&(from, local)) else {
            return;
        };
        let Some(a) = self.allocs.get(&id) else {
            return;
        };
        if a.expires <= now {
            return;
        }
        let Some(c) = a.channels.get(&cd.number).filter(|c| c.expires > now) else {
            return;
        };
        let peer = c.peer;
        if !a.permissions.get(&peer.ip()).is_some_and(|e| *e > now)
            || !self.cfg.peer_policy.is_allowed(peer.ip())
        {
            return;
        }
        let relay = a.relay_local;
        self.push(relay, peer, cd.data);
    }

    fn on_send(&mut self, now: Instant, local: SocketAddr, from: SocketAddr, msg: Message) {
        let Some(&id) = self.by_tuple.get(&(from, local)) else {
            return;
        };
        let Some(a) = self.allocs.get(&id) else {
            return;
        };
        if a.expires <= now || msg.get(attr_type::DONT_FRAGMENT).is_some() {
            return;
        }
        let (Some(peer), Some(data)) = (msg.xor_peer_addresses().next(), msg.data()) else {
            return;
        };
        if !a.permissions.get(&peer.ip()).is_some_and(|e| *e > now)
            || !self.cfg.peer_policy.is_allowed(peer.ip())
        {
            return;
        }
        let relay = a.relay_local;
        self.push(relay, peer, data.clone());
    }

    fn on_request(&mut self, now: Instant, local: SocketAddr, from: SocketAddr, msg: Message) {
        if msg.method == Method::BINDING {
            self.on_binding(now, local, from, &msg);
            return;
        }
        let known = [
            Method::ALLOCATE,
            Method::REFRESH,
            Method::CREATE_PERMISSION,
            Method::CHANNEL_BIND,
        ];
        if !known.contains(&msg.method) {
            let b = MessageBuilder::new(Class::Error, msg.method, msg.transaction_id).attr(
                Attribute::ErrorCode {
                    code: 400,
                    reason: "Bad Request".into(),
                },
            );
            self.send_unauth(now, local, from, &msg, b);
            return;
        }
        // 再送された要求には前の応答をそのまま返す。
        let existing = self.by_tuple.get(&(from, local)).copied();
        if let Some(id) = existing
            && let Some(a) = self.allocs.get(&id)
            && let Some((tid, bytes)) = &a.last
            && *tid == msg.transaction_id
        {
            let bytes = bytes.clone();
            self.push(local, from, bytes);
            return;
        }
        let auth = match self.authenticate(now, from, &msg) {
            Ok(a) => a,
            Err((code, reason)) => {
                let b = self.challenge(now, from, &msg, code, reason);
                self.send_unauth(now, local, from, &msg, b);
                return;
            }
        };
        let result = if let Some(id) = existing
            && self
                .allocs
                .get(&id)
                .is_some_and(|a| a.username != auth.username)
        {
            Err(fail(441, "Wrong Credentials"))
        } else if let Some(f) = self.unknown_attributes(&msg) {
            Err(f)
        } else {
            match msg.method {
                Method::ALLOCATE => self.allocate(now, local, from, &msg, &auth),
                Method::REFRESH => self.refresh(now, local, from, &msg),
                Method::CREATE_PERMISSION => self.create_permission(now, local, from, &msg),
                _ => self.channel_bind(now, local, from, &msg),
            }
        };
        let b = match result {
            Ok(attrs) => {
                MessageBuilder::new(Class::Success, msg.method, msg.transaction_id).attrs(attrs)
            }
            Err(f) => MessageBuilder::new(Class::Error, msg.method, msg.transaction_id)
                .attr(Attribute::ErrorCode {
                    code: f.code,
                    reason: f.reason.into(),
                })
                .attrs(f.extra),
        };
        let b = match &self.cfg.software {
            Some(s) => b.attr(Attribute::Software(s.clone())),
            None => b,
        };
        let mut b = b.integrity(&auth.key);
        if msg.has_fingerprint() {
            b = b.fingerprint();
        }
        let Ok(bytes) = b.build() else { return };
        if let Some(id) = self.by_tuple.get(&(from, local)).copied()
            && let Some(a) = self.allocs.get_mut(&id)
        {
            a.last = Some((msg.transaction_id, bytes.clone()));
        }
        self.push(local, from, bytes);
    }

    fn on_binding(&mut self, now: Instant, local: SocketAddr, from: SocketAddr, msg: &Message) {
        let b = if msg.unknown_comprehension_required().is_empty() {
            senca_stun::binding_success(msg, from)
        } else {
            senca_stun::error_response(msg, 420, "Unknown Attribute")
        };
        self.send_unauth(now, local, from, msg, b);
    }

    /// 認証前の応答を速度制限つきで送る。
    fn send_unauth(
        &mut self,
        now: Instant,
        local: SocketAddr,
        from: SocketAddr,
        req: &Message,
        b: MessageBuilder,
    ) {
        if !self.rate_allow(now, from.ip()) {
            return;
        }
        let b = if req.has_fingerprint() {
            b.fingerprint()
        } else {
            b
        };
        if let Ok(bytes) = b.build() {
            self.push(local, from, bytes);
        }
    }

    fn rate_allow(&mut self, now: Instant, ip: IpAddr) -> bool {
        let rl = self.cfg.unauth_rate;
        if !self.rate.contains_key(&ip) && self.rate.len() >= rl.max_entries {
            self.rate
                .retain(|_, b| now.saturating_duration_since(b.last) < Duration::from_secs(10));
            if self.rate.len() >= rl.max_entries {
                return false;
            }
        }
        let b = self.rate.entry(ip).or_insert(Bucket {
            tokens: rl.burst as f64,
            last: now,
        });
        let dt = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + dt * rl.per_second as f64).min(rl.burst as f64);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    // ---- 認証 ----

    fn secs(&mut self, now: Instant) -> u32 {
        let epoch = *self.epoch.get_or_insert(now);
        now.saturating_duration_since(epoch).as_secs() as u32
    }

    fn nonce_mac(&self, t: u32, from: SocketAddr) -> [u8; 20] {
        let mut m = <Hmac<sha1::Sha1> as Mac>::new_from_slice(&self.nonce_secret)
            .expect("HMAC accepts any key length");
        m.update(&t.to_be_bytes());
        match from.ip() {
            IpAddr::V4(a) => m.update(&a.octets()),
            IpAddr::V6(a) => m.update(&a.octets()),
        }
        m.update(&from.port().to_be_bytes());
        m.finalize().into_bytes().into()
    }

    fn make_nonce(&mut self, now: Instant, from: SocketAddr) -> String {
        let t = self.secs(now);
        let mac = self.nonce_mac(t, from);
        let mut s = format!("{t:08x}");
        for b in &mac[..12] {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    fn nonce_valid(&mut self, now: Instant, from: SocketAddr, nonce: &str) -> bool {
        if nonce.len() != 32 || !nonce.is_ascii() {
            return false;
        }
        let Ok(t) = u32::from_str_radix(&nonce[..8], 16) else {
            return false;
        };
        let mut got = [0u8; 12];
        for (i, g) in got.iter_mut().enumerate() {
            let Ok(v) = u8::from_str_radix(&nonce[8 + 2 * i..10 + 2 * i], 16) else {
                return false;
            };
            *g = v;
        }
        let mac = self.nonce_mac(t, from);
        if !bool::from(mac[..12].ct_eq(&got)) {
            return false;
        }
        let now_s = self.secs(now);
        now_s >= t && ((now_s - t) as u64) < self.cfg.nonce_lifetime.as_secs()
    }

    /// 401・438 の応答（REALM・NONCE 付き）、または 400。
    fn challenge(
        &mut self,
        now: Instant,
        from: SocketAddr,
        req: &Message,
        code: u16,
        reason: &str,
    ) -> MessageBuilder {
        let mut b = MessageBuilder::new(Class::Error, req.method, req.transaction_id).attr(
            Attribute::ErrorCode {
                code,
                reason: reason.into(),
            },
        );
        if code == 401 || code == 438 {
            let nonce = self.make_nonce(now, from);
            b = b
                .attr(Attribute::Realm(self.cfg.realm.clone()))
                .attr(Attribute::Nonce(nonce));
        }
        b
    }

    fn authenticate(
        &mut self,
        now: Instant,
        from: SocketAddr,
        msg: &Message,
    ) -> Result<Auth, (u16, &'static str)> {
        if !msg.has_integrity() {
            return Err((401, "Unauthorized"));
        }
        let (Some(username), Some(realm), Some(nonce)) = (msg.username(), msg.realm(), msg.nonce())
        else {
            return Err((400, "Bad Request"));
        };
        if realm != self.cfg.realm {
            return Err((401, "Unauthorized"));
        }
        if !self.nonce_valid(now, from, nonce) {
            return Err((438, "Stale Nonce"));
        }
        if let Some((key, tenant)) = self.users.get(username) {
            if msg.verify_integrity(key).is_ok() {
                return Ok(Auth {
                    username: username.to_string(),
                    tenant: tenant.clone(),
                    key: key.clone(),
                });
            }
            return Err((401, "Unauthorized"));
        }
        let Some(exp) = rest_api_expiry(username) else {
            return Err((401, "Unauthorized"));
        };
        match self.unix_time {
            Some(t) if t <= exp => {}
            _ => return Err((401, "Unauthorized")),
        }
        for s in &self.cfg.rest_secrets {
            let password = rest_api_password(&s.secret, username);
            let key = long_term_key(username, realm, &password, PasswordAlgorithm::Md5);
            if msg.verify_integrity(&key).is_ok() {
                return Ok(Auth {
                    username: username.to_string(),
                    tenant: s.tenant.clone(),
                    key,
                });
            }
        }
        Err((401, "Unauthorized"))
    }

    fn unknown_attributes(&self, msg: &Message) -> Option<Fail> {
        let mut unknown = msg.unknown_comprehension_required().to_vec();
        if msg.method == Method::ALLOCATE && msg.get(attr_type::DONT_FRAGMENT).is_some() {
            unknown.push(attr_type::DONT_FRAGMENT);
        }
        if unknown.is_empty() {
            return None;
        }
        Some(Fail {
            code: 420,
            reason: "Unknown Attribute",
            extra: vec![Attribute::UnknownAttributes(unknown)],
        })
    }

    // ---- 各メソッド ----

    fn lifetime(&self, requested: Option<u32>) -> Duration {
        match requested {
            Some(r) => {
                let t = Duration::from_secs(r as u64).min(self.cfg.max_lifetime);
                t.max(self.cfg.default_lifetime)
            }
            None => self.cfg.default_lifetime,
        }
    }

    fn allocate(
        &mut self,
        now: Instant,
        local: SocketAddr,
        from: SocketAddr,
        msg: &Message,
        auth: &Auth,
    ) -> Result<Vec<Attribute>, Fail> {
        if self.by_tuple.contains_key(&(from, local)) {
            return Err(fail(437, "Allocation Mismatch"));
        }
        match msg.requested_transport() {
            None => return Err(fail(400, "Bad Request")),
            Some(PROTO_UDP) => {}
            Some(_) => return Err(fail(442, "Unsupported Transport Protocol")),
        }
        let raf = msg.attributes.iter().find_map(|a| match a {
            Attribute::RequestedAddressFamily(f) => Some(*f),
            _ => None,
        });
        let aaf = msg.attributes.iter().find_map(|a| match a {
            Attribute::AdditionalAddressFamily(f) => Some(*f),
            _ => None,
        });
        let even = msg.attributes.iter().find_map(|a| match a {
            Attribute::EvenPort(r) => Some(*r),
            _ => None,
        });
        if msg.get(attr_type::RESERVATION_TOKEN).is_some() {
            if even.is_some() || raf.is_some() || aaf.is_some() {
                return Err(fail(400, "Bad Request"));
            }
            return Err(fail(508, "Insufficient Capacity"));
        }
        if raf.is_some() && aaf.is_some() {
            return Err(fail(400, "Bad Request"));
        }
        let family = raf.unwrap_or(AddressFamily::V4);
        let relay_ip = match family {
            AddressFamily::V4 => self.cfg.relay_v4,
            AddressFamily::V6 => self.cfg.relay_v6,
        };
        let Some(relay_ip) = relay_ip else {
            return Err(fail(440, "Address Family not Supported"));
        };
        if even == Some(true) {
            if aaf.is_some() {
                return Err(fail(400, "Bad Request"));
            }
            return Err(fail(508, "Insufficient Capacity"));
        }
        let mut extra = Vec::new();
        match aaf {
            Some(AddressFamily::V4) => return Err(fail(400, "Bad Request")),
            Some(AddressFamily::V6) => extra.push(Attribute::AddressErrorCode {
                family: AddressFamily::V6,
                code: 440,
                reason: "Address Family not Supported".into(),
            }),
            None => {}
        }
        let q = self.cfg.quotas;
        if self.per_user.get(&auth.username).copied().unwrap_or(0) >= q.per_user {
            return Err(fail(486, "Allocation Quota Reached"));
        }
        if let Some(t) = &auth.tenant
            && self.per_tenant.get(t).copied().unwrap_or(0) >= q.per_tenant
        {
            return Err(fail(486, "Allocation Quota Reached"));
        }
        if self.allocs.len() >= q.max_allocations {
            return Err(fail(508, "Insufficient Capacity"));
        }
        let Some(port) = self.pick_port(relay_ip.local, even == Some(false)) else {
            return Err(fail(508, "Insufficient Capacity"));
        };
        let lifetime = self.lifetime(msg.lifetime());
        let relay_local = SocketAddr::new(relay_ip.local, port);
        let relay_public = SocketAddr::new(relay_ip.external.unwrap_or(relay_ip.local), port);
        let id = self.next_id;
        self.next_id += 1;
        self.allocs.insert(
            id,
            Allocation {
                client: from,
                local,
                relay_local,
                username: auth.username.clone(),
                tenant: auth.tenant.clone(),
                expires: now + lifetime,
                permissions: HashMap::new(),
                channels: HashMap::new(),
                peer_channels: HashMap::new(),
                last: None,
            },
        );
        self.by_tuple.insert((from, local), id);
        self.by_relay.insert(relay_local, id);
        *self.per_user.entry(auth.username.clone()).or_insert(0) += 1;
        if let Some(t) = &auth.tenant {
            *self.per_tenant.entry(t.clone()).or_insert(0) += 1;
        }
        self.events.push_back(TurnServerEvent::AllocationCreated {
            relay: relay_local,
            client: from,
            local,
            username: auth.username.clone(),
        });
        let mut attrs = vec![
            Attribute::XorRelayedAddress(relay_public),
            Attribute::Lifetime(lifetime.as_secs() as u32),
            Attribute::XorMappedAddress(from),
        ];
        attrs.extend(extra);
        Ok(attrs)
    }

    /// 範囲の中から使っていないポートを選ぶ（開始位置は乱数、RFC 8656 の推奨）。
    fn pick_port(&mut self, ip: IpAddr, even: bool) -> Option<u16> {
        let (lo, hi) = (self.cfg.port_min.max(1024), self.cfg.port_max);
        if lo > hi {
            return None;
        }
        let n = (hi - lo) as u32 + 1;
        self.port_seed = self
            .port_seed
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        let start = (self.port_seed >> 8) % n;
        for i in 0..n {
            let port = lo as u32 + (start + i) % n;
            let port = port as u16;
            if even && !port.is_multiple_of(2) {
                continue;
            }
            if !self.by_relay.contains_key(&SocketAddr::new(ip, port)) {
                return Some(port);
            }
        }
        None
    }

    fn alloc_id(&self, now: Instant, local: SocketAddr, from: SocketAddr) -> Result<u64, Fail> {
        match self.by_tuple.get(&(from, local)) {
            Some(id) if self.allocs.get(id).is_some_and(|a| a.expires > now) => Ok(*id),
            _ => Err(fail(437, "Allocation Mismatch")),
        }
    }

    fn refresh(
        &mut self,
        now: Instant,
        local: SocketAddr,
        from: SocketAddr,
        msg: &Message,
    ) -> Result<Vec<Attribute>, Fail> {
        let id = self.alloc_id(now, local, from)?;
        if let Some(Attribute::RequestedAddressFamily(f)) =
            msg.get(attr_type::REQUESTED_ADDRESS_FAMILY)
            && self
                .allocs
                .get(&id)
                .is_some_and(|a| AddressFamily::of(&a.relay_local) != *f)
        {
            return Err(fail(443, "Peer Address Family Mismatch"));
        }
        if msg.lifetime() == Some(0) {
            self.remove_allocation(id, true);
            return Ok(vec![Attribute::Lifetime(0)]);
        }
        let lifetime = self.lifetime(msg.lifetime());
        if let Some(a) = self.allocs.get_mut(&id) {
            a.expires = now + lifetime;
        }
        Ok(vec![Attribute::Lifetime(lifetime.as_secs() as u32)])
    }

    fn check_peer(&self, a: &Allocation, peer: SocketAddr) -> Result<(), Fail> {
        if AddressFamily::of(&peer) != AddressFamily::of(&a.relay_local) {
            return Err(fail(443, "Peer Address Family Mismatch"));
        }
        if !self.cfg.peer_policy.is_allowed(peer.ip()) {
            return Err(fail(403, "Forbidden"));
        }
        Ok(())
    }

    fn create_permission(
        &mut self,
        now: Instant,
        local: SocketAddr,
        from: SocketAddr,
        msg: &Message,
    ) -> Result<Vec<Attribute>, Fail> {
        let id = self.alloc_id(now, local, from)?;
        let peers: Vec<SocketAddr> = msg.xor_peer_addresses().collect();
        if peers.is_empty() {
            return Err(fail(400, "Bad Request"));
        }
        let a = &self.allocs[&id];
        let mut new: HashSet<IpAddr> = HashSet::new();
        for p in &peers {
            self.check_peer(a, *p)?;
            if !a.permissions.get(&p.ip()).is_some_and(|e| *e > now) {
                new.insert(p.ip());
            }
        }
        let live = a.permissions.values().filter(|e| **e > now).count();
        if live + new.len() > self.cfg.quotas.permissions_per_allocation {
            return Err(fail(508, "Insufficient Capacity"));
        }
        let exp = now + self.cfg.permission_lifetime;
        let a = self.allocs.get_mut(&id).expect("checked above");
        a.permissions.retain(|_, e| *e > now);
        for p in peers {
            a.permissions.insert(p.ip(), exp);
        }
        Ok(Vec::new())
    }

    fn channel_bind(
        &mut self,
        now: Instant,
        local: SocketAddr,
        from: SocketAddr,
        msg: &Message,
    ) -> Result<Vec<Attribute>, Fail> {
        let id = self.alloc_id(now, local, from)?;
        let (Some(number), Some(peer)) = (msg.channel_number(), msg.xor_peer_addresses().next())
        else {
            return Err(fail(400, "Bad Request"));
        };
        if !is_valid_channel(number) {
            return Err(fail(400, "Bad Request"));
        }
        let cooldown = self.cfg.channel_cooldown;
        let a = &self.allocs[&id];
        self.check_peer(a, peer)?;
        // 期限切れでも cooldown 中なら別の組には使えない。
        if let Some(c) = a.channels.get(&number)
            && c.expires + cooldown > now
            && c.peer != peer
        {
            return Err(fail(400, "Bad Request"));
        }
        if let Some(n) = a.peer_channels.get(&peer)
            && *n != number
            && a.channels
                .get(n)
                .is_some_and(|c| c.expires + cooldown > now)
        {
            return Err(fail(400, "Bad Request"));
        }
        let live = a
            .channels
            .values()
            .filter(|c| c.expires + cooldown > now)
            .count();
        if !a.channels.contains_key(&number) && live >= self.cfg.quotas.channels_per_allocation {
            return Err(fail(508, "Insufficient Capacity"));
        }
        let perm_new = !a.permissions.get(&peer.ip()).is_some_and(|e| *e > now);
        if perm_new
            && a.permissions.values().filter(|e| **e > now).count()
                >= self.cfg.quotas.permissions_per_allocation
        {
            return Err(fail(508, "Insufficient Capacity"));
        }
        let ch_exp = now + self.cfg.channel_lifetime;
        let perm_exp = now + self.cfg.permission_lifetime;
        let a = self.allocs.get_mut(&id).expect("checked above");
        a.channels.insert(
            number,
            Channel {
                peer,
                expires: ch_exp,
            },
        );
        a.peer_channels.insert(peer, number);
        let e = a.permissions.entry(peer.ip()).or_insert(perm_exp);
        *e = (*e).max(perm_exp);
        Ok(Vec::new())
    }
}

fn dec(map: &mut HashMap<String, usize>, k: &str) {
    if let Some(v) = map.get_mut(k) {
        *v = v.saturating_sub(1);
        if *v == 0 {
            map.remove(k);
        }
    }
}
