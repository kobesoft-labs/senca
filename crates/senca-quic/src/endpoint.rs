//! endpoint: datagram を connection ID で接続へ振り分け、新しい接続・Retry・
//! Version Negotiation・stateless reset を扱う。

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use bytes::BytesMut;
use subtle::ConstantTimeEq;

use crate::cid::{ConnectionId, ResetToken};
use crate::config::{ClientConfig, EndpointConfig, ServerConfig};
use crate::conn::{ConnParams, Connection, Side, ToEndpoint, Transmit};
use crate::crypto::{HmacKey, Keys, TAG_LEN};
use crate::error::{ConnectionError, TransportErrorCode};
use crate::frame;
use crate::packet::{self, Header, LongType};
use crate::{VERSION_1, varint};

/// endpoint の中の接続の番号。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionHandle(pub u64);

/// endpoint のイベント。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointEvent {
    /// server が新しい接続を受け付けた。
    Accepted(ConnectionHandle),
    /// 接続が完全に終わり、取り除いた。
    Drained(ConnectionHandle),
}

/// stateless な送信の待ち行列の上限。
const MAX_STATELESS: usize = 256;

/// QUIC の endpoint（client・server 兼用）。
pub struct Endpoint {
    config: EndpointConfig,
    key: HmacKey,
    server: Option<Arc<ServerConfig>>,
    conns: HashMap<ConnectionHandle, Connection>,
    order: Vec<ConnectionHandle>,
    cursor: usize,
    by_cid: HashMap<ConnectionId, ConnectionHandle>,
    by_initial: HashMap<ConnectionId, ConnectionHandle>,
    reset_tokens: HashMap<ResetToken, ConnectionHandle>,
    next_handle: u64,
    stateless: VecDeque<Transmit>,
    events: VecDeque<EndpointEvent>,
    epoch: Option<Instant>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("connections", &self.conns.len())
            .field("server", &self.server.is_some())
            .finish_non_exhaustive()
    }
}

/// 接続を始められない理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    Rng,
    Connection(ConnectionError),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rng => write!(f, "乱数源が使えない"),
            Self::Connection(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl Endpoint {
    /// `server` を渡すと接続を受け付ける。
    pub fn new(mut config: EndpointConfig, server: Option<Arc<ServerConfig>>) -> Self {
        config.cid_len = config.cid_len.clamp(4, 20);
        Self {
            key: HmacKey::new(config.secret_key),
            config,
            server,
            conns: HashMap::new(),
            order: Vec::new(),
            cursor: 0,
            by_cid: HashMap::new(),
            by_initial: HashMap::new(),
            reset_tokens: HashMap::new(),
            next_handle: 0,
            stateless: VecDeque::new(),
            events: VecDeque::new(),
            epoch: None,
        }
    }

    pub fn set_server_config(&mut self, server: Option<Arc<ServerConfig>>) {
        self.server = server;
    }

    pub fn connection(&self, h: ConnectionHandle) -> Option<&Connection> {
        self.conns.get(&h)
    }

    pub fn connection_mut(&mut self, h: ConnectionHandle) -> Option<&mut Connection> {
        self.conns.get_mut(&h)
    }

    pub fn connections(&self) -> impl Iterator<Item = ConnectionHandle> + '_ {
        self.order.iter().copied()
    }

    pub fn len(&self) -> usize {
        self.conns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.conns.is_empty()
    }

    pub fn poll_event(&mut self) -> Option<EndpointEvent> {
        self.events.pop_front()
    }

    fn epoch_ms(&mut self, now: Instant) -> u64 {
        let e = *self.epoch.get_or_insert(now);
        now.saturating_duration_since(e).as_millis() as u64
    }

    fn new_handle(&mut self) -> ConnectionHandle {
        let h = ConnectionHandle(self.next_handle);
        self.next_handle += 1;
        h
    }

    fn new_cid(&self) -> Result<ConnectionId, ConnectError> {
        for _ in 0..8 {
            let c = ConnectionId::random(self.config.cid_len).map_err(|_| ConnectError::Rng)?;
            if !self.by_cid.contains_key(&c) {
                return Ok(c);
            }
        }
        Err(ConnectError::Rng)
    }

    fn insert(&mut self, h: ConnectionHandle, c: Connection) {
        for cid in c.local_cids() {
            self.by_cid.insert(cid, h);
        }
        self.conns.insert(h, c);
        self.order.push(h);
        self.sync(h);
    }

    /// 接続から endpoint への通知を取り込む。
    fn sync(&mut self, h: ConnectionHandle) {
        let Some(c) = self.conns.get_mut(&h) else { return };
        while let Some(e) = c.poll_endpoint() {
            match e {
                ToEndpoint::NewCid(cid) => {
                    self.by_cid.insert(cid, h);
                }
                ToEndpoint::RetireCid(cid) => {
                    self.by_cid.remove(&cid);
                }
                ToEndpoint::ResetToken(t) => {
                    self.reset_tokens.insert(t, h);
                }
            }
        }
    }

    /// client として接続を始める。`wall` は証明書の検証に使う現在時刻。
    pub fn connect(
        &mut self,
        now: Instant,
        wall: SystemTime,
        config: ClientConfig,
        remote: SocketAddr,
        server_name: &str,
    ) -> Result<ConnectionHandle, ConnectError> {
        let local_cid = self.new_cid()?;
        let p = ConnParams {
            side: Side::Client,
            config: config.transport.clone(),
            remote,
            local_cid,
            cid_key: self.key.clone(),
            cid_len: self.config.cid_len,
            now,
        };
        let c = Connection::new_client(p, config.tls.clone(), server_name, wall).map_err(ConnectError::Connection)?;
        let h = self.new_handle();
        self.insert(h, c);
        Ok(h)
    }

    /// 受け取った UDP datagram を処理する。届け先の接続を返す（stateless に処理したら `None`）。
    pub fn handle_datagram(&mut self, now: Instant, wall: SystemTime, remote: SocketAddr, data: BytesMut) -> Option<ConnectionHandle> {
        let partial = packet::parse(&data, self.config.cid_len).ok()?;
        let dcid = partial.dcid_bytes();
        // 既知の接続
        if let Some(cid) = ConnectionId::new(dcid)
            && let Some(&h) = self.by_cid.get(&cid).or_else(|| self.by_initial.get(&cid))
            && let Some(c) = self.conns.get_mut(&h)
        {
            c.handle_datagram(now, remote, data);
            self.sync(h);
            return Some(h);
        }
        match partial.header {
            Header::Short { .. } => {
                self.maybe_stateless_reset(now, remote, &data);
                None
            }
            Header::Unsupported { dcid, scid } => {
                // 未知の版: client の Initial らしい大きさなら Version Negotiation を返す（RFC 9000 6.1）
                if self.server.is_some() && data.len() >= 1200 {
                    let r = random_u8();
                    self.push_stateless(Transmit {
                        destination: remote,
                        contents: packet::version_negotiation(scid, dcid, r),
                    });
                }
                None
            }
            Header::Long {
                ty: LongType::Initial,
                dcid,
                scid,
                token,
            } => {
                let token = token.to_vec();
                self.accept(now, wall, remote, dcid, scid, &token, data)
            }
            _ => None,
        }
    }

    fn push_stateless(&mut self, t: Transmit) {
        if self.stateless.len() < MAX_STATELESS {
            self.stateless.push_back(t);
        }
    }

    fn maybe_stateless_reset(&mut self, now: Instant, remote: SocketAddr, data: &[u8]) {
        // 相手からの stateless reset か
        if data.len() >= 21 {
            let tail: [u8; 16] = data[data.len() - 16..].try_into().unwrap_or([0; 16]);
            if let Some(&h) = self.reset_tokens.get(&tail)
                && let Some(c) = self.conns.get_mut(&h)
            {
                // 照合は接続側で定数時間で行う
                c.handle_datagram(now, remote, BytesMut::from(data));
                return;
            }
        }
        if !self.config.stateless_reset || self.server.is_none() {
            return;
        }
        // 未知の connection ID への stateless reset（RFC 9000 10.3）。受けたものより短く、ループしないように。
        if data.len() <= 43 {
            return;
        }
        let Some(cid) = ConnectionId::new(&data[1..1 + self.config.cid_len.min(data.len() - 1)]) else { return };
        let token = self.key.reset_token(&cid);
        let len = (data.len() - 1).min(43);
        let mut v = vec![0u8; len - 16];
        if getrandom::fill(&mut v).is_err() {
            return;
        }
        v[0] = 0x40 | (v[0] & 0x3f);
        v.extend_from_slice(&token);
        self.push_stateless(Transmit {
            destination: remote,
            contents: v,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn accept(
        &mut self,
        now: Instant,
        wall: SystemTime,
        remote: SocketAddr,
        dcid: ConnectionId,
        scid: ConnectionId,
        token: &[u8],
        data: BytesMut,
    ) -> Option<ConnectionHandle> {
        let server = self.server.clone()?;
        // 小さすぎる Initial は捨てる（RFC 9000 14.1）。DCID は 8 バイト以上（RFC 9000 7.2）。
        if data.len() < 1200 || dcid.len() < 8 {
            return None;
        }
        if self.conns.len() >= server.max_connections {
            self.stateless_close(remote, &dcid, &scid, TransportErrorCode::CONNECTION_REFUSED);
            return None;
        }
        // アドレスの検証（Retry）
        let mut odcid = dcid;
        let mut retry_scid = None;
        if server.retry {
            if token.is_empty() {
                let new_scid = self.new_cid().ok()?;
                let t = self.make_retry_token(now, remote, &dcid, &new_scid);
                let r = random_u8();
                self.push_stateless(Transmit {
                    destination: remote,
                    contents: packet::retry(&scid, &new_scid, &dcid, &t, r),
                });
                return None;
            }
            match self.check_retry_token(now, remote, &dcid, token, server.retry_token_lifetime.as_millis() as u64) {
                Some(o) => {
                    odcid = o;
                    retry_scid = Some(dcid);
                }
                None => {
                    self.stateless_close(remote, &dcid, &scid, TransportErrorCode::INVALID_TOKEN);
                    return None;
                }
            }
        }
        let local_cid = self.new_cid().ok()?;
        let p = ConnParams {
            side: Side::Server,
            config: server.transport.clone(),
            remote,
            local_cid,
            cid_key: self.key.clone(),
            cid_len: self.config.cid_len,
            now,
        };
        let c = Connection::new_server(p, server.tls.clone(), wall, dcid, scid, odcid, retry_scid).ok()?;
        let h = self.new_handle();
        self.insert(h, c);
        self.by_initial.insert(dcid, h);
        self.events.push_back(EndpointEvent::Accepted(h));
        if let Some(c) = self.conns.get_mut(&h) {
            c.handle_datagram(now, remote, data);
        }
        self.sync(h);
        Some(h)
    }

    fn token_mac(&self, issued: u64, remote: SocketAddr, odcid: &ConnectionId, retry_scid: &ConnectionId) -> [u8; 16] {
        let addr = match remote {
            SocketAddr::V4(a) => {
                let mut v = a.ip().octets().to_vec();
                v.extend_from_slice(&a.port().to_be_bytes());
                v
            }
            SocketAddr::V6(a) => {
                let mut v = a.ip().octets().to_vec();
                v.extend_from_slice(&a.port().to_be_bytes());
                v
            }
        };
        let h = self.key.sign(
            b"senca quic retry",
            &[&issued.to_be_bytes(), odcid.as_slice(), retry_scid.as_slice(), &addr],
        );
        let mut m = [0u8; 16];
        m.copy_from_slice(&h[..16]);
        m
    }

    /// Retry トークン: 種別(1) 発行時刻ms(8) odcid長(1) odcid MAC(16)。
    fn make_retry_token(&mut self, now: Instant, remote: SocketAddr, odcid: &ConnectionId, retry_scid: &ConnectionId) -> Vec<u8> {
        let issued = self.epoch_ms(now);
        let mut t = vec![0x01];
        t.extend_from_slice(&issued.to_be_bytes());
        t.push(odcid.len() as u8);
        t.extend_from_slice(odcid.as_slice());
        t.extend_from_slice(&self.token_mac(issued, remote, odcid, retry_scid));
        t
    }

    fn check_retry_token(&mut self, now: Instant, remote: SocketAddr, dcid: &ConnectionId, token: &[u8], lifetime_ms: u64) -> Option<ConnectionId> {
        if token.len() < 1 + 8 + 1 + 16 || token[0] != 0x01 {
            return None;
        }
        let issued = u64::from_be_bytes(token[1..9].try_into().ok()?);
        let ol = token[9] as usize;
        if token.len() != 10 + ol + 16 {
            return None;
        }
        let odcid = ConnectionId::new(&token[10..10 + ol])?;
        let mac = &token[10 + ol..];
        let expected = self.token_mac(issued, remote, &odcid, dcid);
        if !bool::from(expected.ct_eq(mac)) {
            return None;
        }
        let now_ms = self.epoch_ms(now);
        if issued > now_ms || now_ms - issued > lifetime_ms {
            return None;
        }
        Some(odcid)
    }

    /// 接続を作らずに Initial の CONNECTION_CLOSE を返す（拒否・不正なトークン）。
    fn stateless_close(&mut self, remote: SocketAddr, dcid: &ConnectionId, scid: &ConnectionId, code: TransportErrorCode) {
        let Ok(keys) = Keys::initial(dcid, false) else { return };
        let Ok(my) = self.new_cid() else { return };
        let mut buf = vec![0xc0 | (LongType::Initial.bits() << 4)]; // pn 長 1
        buf.extend_from_slice(&VERSION_1.to_be_bytes());
        buf.push(scid.len() as u8);
        buf.extend_from_slice(scid.as_slice());
        buf.push(my.len() as u8);
        buf.extend_from_slice(my.as_slice());
        buf.push(0); // token 長
        let mut payload = Vec::new();
        frame::write_close(&mut payload, false, code.0, 0, &[]);
        while payload.len() < 4 {
            payload.push(0);
        }
        let len = 1 + payload.len() + TAG_LEN;
        varint::write_with_len(&mut buf, len as u64, 2);
        let pn_offset = buf.len();
        buf.push(0); // pn = 0
        let hdr_len = buf.len();
        buf.extend_from_slice(&payload);
        let tag = {
            let (h, p) = buf.split_at_mut(hdr_len);
            match keys.local.packet.seal(0, h, p) {
                Ok(t) => t,
                Err(_) => return,
            }
        };
        buf.extend_from_slice(&tag);
        packet::protect(&mut buf, pn_offset, 1, &keys.local.header);
        self.push_stateless(Transmit {
            destination: remote,
            contents: buf,
        });
    }

    /// 送る datagram を一つ返す。
    pub fn poll_transmit(&mut self, now: Instant) -> Option<Transmit> {
        if let Some(t) = self.stateless.pop_front() {
            return Some(t);
        }
        let n = self.order.len();
        for i in 0..n {
            let idx = (self.cursor + i) % n;
            let h = self.order[idx];
            let t = self.conns.get_mut(&h).and_then(|c| c.poll_transmit(now));
            self.sync(h);
            if let Some(t) = t {
                self.cursor = (idx + 1) % n.max(1);
                return Some(t);
            }
        }
        None
    }

    /// 次に [`Self::handle_timeout`] を呼ぶ時刻。
    pub fn poll_timeout(&self) -> Option<Instant> {
        self.conns.values().filter_map(|c| c.poll_timeout()).min()
    }

    /// timer を処理し、終わった接続を取り除く。
    pub fn handle_timeout(&mut self, now: Instant) {
        let due: Vec<ConnectionHandle> = self
            .conns
            .iter()
            .filter(|(_, c)| c.poll_timeout().is_some_and(|t| t <= now))
            .map(|(h, _)| *h)
            .collect();
        for h in due {
            if let Some(c) = self.conns.get_mut(&h) {
                c.handle_timeout(now);
            }
            self.sync(h);
        }
        self.reap();
    }

    /// drained の接続を取り除く。
    pub fn reap(&mut self) {
        let dead: Vec<ConnectionHandle> = self.conns.iter().filter(|(_, c)| c.is_drained()).map(|(h, _)| *h).collect();
        for h in dead {
            self.conns.remove(&h);
            self.order.retain(|x| *x != h);
            self.by_cid.retain(|_, v| *v != h);
            self.by_initial.retain(|_, v| *v != h);
            self.reset_tokens.retain(|_, v| *v != h);
            self.events.push_back(EndpointEvent::Drained(h));
        }
        if self.cursor >= self.order.len() {
            self.cursor = 0;
        }
    }
}

fn random_u8() -> u8 {
    let mut b = [0u8; 1];
    let _ = getrandom::fill(&mut b);
    b[0]
}
