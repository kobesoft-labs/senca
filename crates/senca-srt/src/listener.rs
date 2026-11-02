//! 応答側（listener）。誘導（SYN cookie）と結論を処理し、相手ごとに `Connection` を作る。
//!
//! 1 つの UDP ソケットに届くものをすべて `handle_datagram` に渡す。宛先 socket ID が 0 の
//! ハンドシェイクはここで処理し、それ以外は該当する接続へ振り分ける。

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;

use bytes::Bytes;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::config::{Config, ConfigError, Event, RejectReason};
use crate::connection::{AcceptInput, Connection, SendError, Stats};
use crate::crypto::{CipherMode, KeyLen, Passphrase, random_bytes};
use crate::handshake::*;
use crate::packet::{ControlType, Packet, build_control, parse, peek_dest};
use crate::streamid::StreamId;

const MAX_OUT_QUEUE: usize = 1024;
const COOKIE_PERIOD_US: u64 = 60_000_000;

/// 接続の識別子（こちらの socket ID）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(pub u32);

/// 受け入れ判断に渡す情報。
#[derive(Debug)]
pub struct AcceptRequest<'a> {
    pub peer: SocketAddr,
    /// 受け取った Stream ID（なければ None）。
    pub stream_id: Option<&'a str>,
    /// 解釈した Stream ID（`#!::` の書式誤りなら None）。
    pub parsed: Option<&'a StreamId>,
    /// 相手の SRT 版（0x00MMmmpp）。
    pub peer_version: u32,
    /// 相手が暗号（KMREQ）を求めているか。
    pub encrypted: bool,
    /// 相手が申告した鍵長。
    pub key_len: Option<KeyLen>,
}

/// 受け入れるときの接続ごとの設定。
#[derive(Clone, Debug, Default)]
pub struct AcceptParams {
    /// この接続（テナント・資源）で使うパスフレーズ。None なら平文だけを受け付ける。
    pub passphrase: Option<Passphrase>,
    /// 受信遅延（ms）。None なら Listener の設定。
    pub latency_ms: Option<u16>,
    /// 暗号方式を限定する。None なら相手の申告（CTR か GCM）に従う。
    pub cipher: Option<CipherMode>,
}

/// 受け入れの判断。
#[derive(Clone, Debug)]
pub enum AcceptDecision {
    Accept(AcceptParams),
    Reject(RejectReason),
}

/// Listener の設定。
#[derive(Clone, Debug)]
pub struct ListenerConfig {
    /// 接続の既定値（`passphrase` と `stream_id` は使わない。判断ごとに決める）。
    pub conn: Config,
    /// 同時接続数の上限。
    pub max_connections: usize,
}

impl Default for ListenerConfig {
    fn default() -> Self {
        ListenerConfig { conn: Config::default(), max_connections: 256 }
    }
}

type AcceptFn = Box<dyn FnMut(&AcceptRequest<'_>) -> AcceptDecision + Send>;

/// SRT の listener。
pub struct Listener {
    cfg: ListenerConfig,
    cookie_key: [u8; 32],
    accept: AcceptFn,
    conns: HashMap<u32, Connection>,
    by_peer: HashMap<(SocketAddr, u32), u32>,
    order: VecDeque<u32>,
    out: VecDeque<(SocketAddr, Bytes)>,
    now: u64,
    rejections: VecDeque<Rejection>,
    /// 拒否した結論の数。
    pub rejected: u64,
}

/// 拒否した接続要求（監査・ログ用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub peer: SocketAddr,
    pub stream_id: Option<String>,
    pub reason: RejectReason,
}

const MAX_REJECTIONS: usize = 64;

impl Listener {
    /// 作る。`accept` は結論を受け取るたびに呼ばれ、Stream ID などから受け入れを決める。
    pub fn new(
        cfg: ListenerConfig,
        accept: impl FnMut(&AcceptRequest<'_>) -> AcceptDecision + Send + 'static,
    ) -> Result<Self, ConfigError> {
        let mut cookie_key = [0u8; 32];
        random_bytes(&mut cookie_key).map_err(|_| ConfigError::Rng)?;
        Ok(Listener {
            cfg,
            cookie_key,
            accept: Box::new(accept),
            conns: HashMap::new(),
            by_peer: HashMap::new(),
            order: VecDeque::new(),
            out: VecDeque::new(),
            now: 0,
            rejections: VecDeque::new(),
            rejected: 0,
        })
    }

    fn cookie(&self, peer: &SocketAddr, bucket: u64) -> u32 {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&self.cookie_key).expect("hmac accepts any key length");
        match peer.ip() {
            std::net::IpAddr::V4(a) => m.update(&a.octets()),
            std::net::IpAddr::V6(a) => m.update(&a.octets()),
        }
        m.update(&peer.port().to_be_bytes());
        m.update(&bucket.to_be_bytes());
        let t = m.finalize().into_bytes();
        u32::from_be_bytes([t[0], t[1], t[2], t[3]])
    }

    fn push_out(&mut self, to: SocketAddr, b: Bytes) {
        if self.out.len() < MAX_OUT_QUEUE {
            self.out.push_back((to, b));
        }
    }

    /// 受け取ったデータグラムを処理する。
    pub fn handle_datagram(&mut self, now: u64, from: SocketAddr, buf: &[u8]) {
        self.now = self.now.max(now);
        let now = self.now;
        let Some(dest) = peek_dest(buf) else { return };
        if dest != 0 {
            if let Some(c) = self.conns.get_mut(&dest) {
                c.handle_datagram(now, from, buf);
            }
            return;
        }
        let Ok(Packet::Control(h, cif)) = parse(buf) else { return };
        if h.ctype != ControlType::Handshake {
            return;
        }
        let Ok(hs) = Handshake::parse(cif) else { return };
        match hs.req_type {
            REQ_INDUCTION => self.on_induction(now, from, h.timestamp, hs),
            REQ_CONCLUSION => self.on_conclusion(now, from, h.timestamp, hs),
            _ => {}
        }
    }

    fn on_induction(&mut self, now: u64, from: SocketAddr, _ts: u32, mut hs: Handshake) {
        let caller_id = hs.socket_id;
        hs.version = HS_VERSION_SRT1;
        hs.ext_field = SRT_MAGIC;
        hs.enc_field = 0;
        hs.cookie = self.cookie(&from, now / COOKIE_PERIOD_US);
        hs.ext.clear();
        let pkt = build_control(ControlType::Handshake, 0, 0, now as u32, caller_id, &hs.to_bytes());
        self.push_out(from, pkt);
    }

    fn reject(&mut self, now: u64, from: SocketAddr, mut hs: Handshake, reason: RejectReason) {
        self.rejected += 1;
        if self.rejections.len() >= MAX_REJECTIONS {
            self.rejections.pop_front();
        }
        let stream_id = hs.ext.iter().find_map(|e| if let HsExt::Sid(s) = e { Some(s.clone()) } else { None });
        self.rejections.push_back(Rejection { peer: from, stream_id, reason });
        let caller_id = hs.socket_id;
        hs.req_type = reason.to_req_type();
        hs.ext.clear();
        hs.ext_field = 0;
        hs.enc_field = 0;
        let pkt = build_control(ControlType::Handshake, 0, 0, now as u32, caller_id, &hs.to_bytes());
        self.push_out(from, pkt);
    }

    fn on_conclusion(&mut self, now: u64, from: SocketAddr, ts: u32, hs: Handshake) {
        let bucket = now / COOKIE_PERIOD_US;
        let ok = hs.cookie == self.cookie(&from, bucket)
            || (bucket > 0 && hs.cookie == self.cookie(&from, bucket - 1));
        if !ok {
            return; // 偽装された送信元の可能性。何も返さない。
        }
        if let Some(id) = self.by_peer.get(&(from, hs.socket_id)) {
            if let Some(c) = self.conns.get_mut(id) {
                c.resend_hs_response();
            }
            return;
        }
        if hs.version != HS_VERSION_SRT1 || hs.ext_field & HS_EXT_HSREQ == 0 {
            self.reject(now, from, hs, RejectReason::VERSION);
            return;
        }
        if self.conns.len() >= self.cfg.max_connections {
            self.reject(now, from, hs, RejectReason::BACKLOG);
            return;
        }
        let sid = hs.ext.iter().find_map(|e| if let HsExt::Sid(s) = e { Some(s.clone()) } else { None });
        let parsed = sid.as_deref().map(StreamId::parse).and_then(|r| r.ok());
        let peer_version = hs
            .ext
            .iter()
            .find_map(|e| if let HsExt::HsReq(s) = e { Some(s.version) } else { None })
            .unwrap_or(0);
        let encrypted = hs.ext.iter().any(|e| matches!(e, HsExt::KmReq(_)));
        let key_len = match hs.enc_field {
            2 => Some(KeyLen::Aes128),
            3 => Some(KeyLen::Aes192),
            4 => Some(KeyLen::Aes256),
            _ => None,
        };
        let req = AcceptRequest {
            peer: from,
            stream_id: sid.as_deref(),
            parsed: parsed.as_ref(),
            peer_version,
            encrypted,
            key_len,
        };
        let params = match (self.accept)(&req) {
            AcceptDecision::Accept(p) => p,
            AcceptDecision::Reject(r) => {
                self.reject(now, from, hs, r);
                return;
            }
        };
        let mut cfg = self.cfg.conn.clone();
        cfg.passphrase = params.passphrase;
        cfg.stream_id = None;
        if let Some(l) = params.latency_ms {
            cfg.latency_ms = l;
        }
        let mut local_id = 0;
        for _ in 0..16 {
            let mut b = [0u8; 4];
            if random_bytes(&mut b).is_err() {
                break;
            }
            let id = u32::from_be_bytes(b) & 0x3FFF_FFFF;
            if id != 0 && !self.conns.contains_key(&id) {
                local_id = id;
                break;
            }
        }
        if local_id == 0 {
            self.reject(now, from, hs, RejectReason::RESOURCE);
            return;
        }
        let input = AcceptInput {
            cfg,
            local_id,
            peer: from,
            hs: &hs,
            hs_ts: ts,
            now,
            stream_id: sid,
            require_cipher: params.cipher,
        };
        match Connection::accept(input) {
            Ok(c) => {
                self.by_peer.insert((from, hs.socket_id), local_id);
                self.conns.insert(local_id, c);
                self.order.push_back(local_id);
            }
            Err(r) => self.reject(now, from, hs, r),
        }
    }

    /// 時刻を進める。閉じて出力を出し切った接続はここで片付ける。
    pub fn handle_timeout(&mut self, now: u64) {
        self.now = self.now.max(now);
        for c in self.conns.values_mut() {
            if c.poll_timeout().is_some_and(|t| t <= self.now) {
                c.handle_timeout(self.now);
            }
        }
        self.cleanup();
    }

    fn cleanup(&mut self) {
        let dead: Vec<u32> = self.conns.iter().filter(|(_, c)| c.is_drained()).map(|(id, _)| *id).collect();
        for id in dead {
            if let Some(c) = self.conns.remove(&id) {
                self.by_peer.retain(|_, v| *v != id);
                self.order.retain(|x| *x != id);
                drop(c);
            }
        }
    }

    /// 送るべきデータグラムを 1 つ取り出す。
    pub fn poll_transmit(&mut self) -> Option<(SocketAddr, Bytes)> {
        if let Some(p) = self.out.pop_front() {
            return Some(p);
        }
        for _ in 0..self.order.len() {
            let id = self.order.pop_front()?;
            self.order.push_back(id);
            if let Some(c) = self.conns.get_mut(&id)
                && let Some(p) = c.poll_transmit()
            {
                return Some(p);
            }
        }
        None
    }

    /// 出来事を 1 つ取り出す。
    pub fn poll_event(&mut self) -> Option<(ConnectionId, Event)> {
        for _ in 0..self.order.len() {
            let id = self.order.pop_front()?;
            self.order.push_back(id);
            if let Some(c) = self.conns.get_mut(&id)
                && let Some(e) = c.poll_event()
            {
                return Some((ConnectionId(id), e));
            }
        }
        None
    }

    /// 拒否した接続要求を 1 つ取り出す（直近 64 件まで保持）。
    pub fn poll_rejection(&mut self) -> Option<Rejection> {
        self.rejections.pop_front()
    }

    /// 次に `handle_timeout` を呼ぶべき時刻。
    pub fn poll_timeout(&self) -> Option<u64> {
        self.conns.values().filter_map(|c| c.poll_timeout()).min()
    }

    pub fn connection(&self, id: ConnectionId) -> Option<&Connection> {
        self.conns.get(&id.0)
    }

    pub fn connection_mut(&mut self, id: ConnectionId) -> Option<&mut Connection> {
        self.conns.get_mut(&id.0)
    }

    pub fn connections(&self) -> impl Iterator<Item = (ConnectionId, &Connection)> {
        self.conns.iter().map(|(id, c)| (ConnectionId(*id), c))
    }

    pub fn len(&self) -> usize {
        self.conns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.conns.is_empty()
    }

    pub fn send(&mut self, id: ConnectionId, data: &[u8], now: u64) -> Result<(), SendError> {
        self.conns.get_mut(&id.0).ok_or(SendError::NotConnected)?.send(data, now)
    }

    pub fn close(&mut self, id: ConnectionId, now: u64) {
        if let Some(c) = self.conns.get_mut(&id.0) {
            c.close(now);
        }
    }

    pub fn stats(&self, id: ConnectionId) -> Option<Stats> {
        self.conns.get(&id.0).map(|c| c.stats())
    }
}
