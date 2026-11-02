//! 試験の共通部品: メモリ上のネットワーク（遅延・損失・並べ替え・アドレス変更）と仮想時刻。
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::BytesMut;
use senca_quic::*;
use senca_tls::{CertifiedKey, RootStore, ServerVerifier};

pub const CA: &[u8] = include_bytes!("../../../senca-tls/tests/data/ca.pem");
pub const ECDSA_CHAIN: &[u8] = include_bytes!("../../../senca-tls/tests/data/server-ecdsa.pem");
pub const ECDSA_KEY: &[u8] = include_bytes!("../../../senca-tls/tests/data/server-ecdsa.key");
pub const RSA_CHAIN: &[u8] = include_bytes!("../../../senca-tls/tests/data/server-rsa.pem");
pub const RSA_KEY: &[u8] = include_bytes!("../../../senca-tls/tests/data/server-rsa.key");

pub fn tls_server(chain: &[u8], key: &[u8], alpn: &[&[u8]]) -> Arc<senca_tls::ServerConfig> {
    let mut c = senca_tls::ServerConfig::new(Arc::new(CertifiedKey::from_pem(chain, key).unwrap())).unwrap();
    c.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    Arc::new(c)
}

pub fn tls_client(alpn: &[&[u8]]) -> Arc<senca_tls::ClientConfig> {
    let mut r = RootStore::new();
    r.add_pem(CA).unwrap();
    let mut c = senca_tls::ClientConfig::new(ServerVerifier::WebPki {
        roots: Arc::new(r),
        spki_pins: Vec::new(),
    });
    c.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    Arc::new(c)
}

/// 簡単な決定的乱数（xorshift）。
pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next() % 1_000_000) as f64 / 1_000_000.0 < p
    }
}

struct InFlight {
    at: Instant,
    to_server: bool,
    from: SocketAddr,
    to: SocketAddr,
    data: Vec<u8>,
}

/// datagram の書き換え（`false` で捨てる）。
pub type Filter = Box<dyn FnMut(&mut Vec<u8>) -> bool>;

/// client と server の二つの endpoint をつなぐ仮想ネットワーク。
pub struct Net {
    pub now: Instant,
    pub client: Endpoint,
    pub server: Endpoint,
    pub ch: ConnectionHandle,
    pub sh: Option<ConnectionHandle>,
    pub client_addr: SocketAddr,
    pub server_addr: SocketAddr,
    pub delay: Duration,
    pub loss: f64,
    pub reorder: f64,
    pub rng: Rng,
    flight: Vec<InFlight>,
    /// server から client アドレスへ送ったバイト数・client から受けたバイト数（anti-amplification の確認）。
    pub s2c_bytes: u64,
    pub c2s_bytes: u64,
    /// 配送を止める（idle timeout の試験）。
    pub blackhole: bool,
    /// client→server の datagram を書き換える。
    pub c2s_filter: Option<Filter>,
    pub s2c_filter: Option<Filter>,
}

pub fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

impl Net {
    pub fn new(server_cfg: ServerConfig, client_cfg: ClientConfig) -> Self {
        let now = Instant::now();
        let server = Endpoint::new(EndpointConfig::new().unwrap(), Some(Arc::new(server_cfg)));
        let mut client = Endpoint::new(EndpointConfig::new().unwrap(), None);
        let server_addr = addr("10.0.0.1:4433");
        let client_addr = addr("10.0.0.2:50000");
        let ch = client
            .connect(now, SystemTime::now(), client_cfg, server_addr, "localhost")
            .unwrap();
        Self {
            now,
            client,
            server,
            ch,
            sh: None,
            client_addr,
            server_addr,
            delay: Duration::from_millis(10),
            loss: 0.0,
            reorder: 0.0,
            rng: Rng(0x9e37_79b9_7f4a_7c15),
            flight: Vec::new(),
            s2c_bytes: 0,
            c2s_bytes: 0,
            blackhole: false,
            c2s_filter: None,
            s2c_filter: None,
        }
    }

    pub fn default_pair(alpn: &[u8]) -> Self {
        let s = ServerConfig::new(tls_server(ECDSA_CHAIN, ECDSA_KEY, &[alpn]));
        let c = ClientConfig::new(tls_client(&[alpn]));
        Self::new(s, c)
    }

    pub fn c(&mut self) -> &mut Connection {
        self.client.connection_mut(self.ch).unwrap()
    }

    pub fn s(&mut self) -> &mut Connection {
        let h = self.sh.expect("server connection");
        self.server.connection_mut(h).unwrap()
    }

    fn collect(&mut self) -> bool {
        let mut any = false;
        let mut guard = 0;
        while let Some(t) = self.client.poll_transmit(self.now) {
            guard += 1;
            assert!(guard < 100_000, "client keeps transmitting: {:?}", self.client.connection(self.ch));
            any = true;
            let mut d = t.contents;
            if let Some(f) = self.c2s_filter.as_mut()
                && !f(&mut d)
            {
                continue;
            }
            self.c2s_bytes += d.len() as u64;
            self.enqueue(true, self.client_addr, t.destination, d);
        }
        let mut guard = 0;
        while let Some(t) = self.server.poll_transmit(self.now) {
            guard += 1;
            assert!(guard < 100_000, "server keeps transmitting");
            any = true;
            let mut d = t.contents;
            if let Some(f) = self.s2c_filter.as_mut()
                && !f(&mut d)
            {
                continue;
            }
            self.s2c_bytes += d.len() as u64;
            self.enqueue(false, self.server_addr, t.destination, d);
        }
        any
    }

    fn enqueue(&mut self, to_server: bool, from: SocketAddr, to: SocketAddr, data: Vec<u8>) {
        if self.blackhole || self.rng.chance(self.loss) {
            return;
        }
        let mut at = self.now + self.delay;
        if self.rng.chance(self.reorder) {
            at += Duration::from_millis(1 + self.rng.next() % 20);
        }
        self.flight.push(InFlight {
            at,
            to_server,
            from,
            to,
            data,
        });
    }

    fn deliver_due(&mut self) {
        let now = self.now;
        let mut due: Vec<InFlight> = Vec::new();
        let mut i = 0;
        while i < self.flight.len() {
            if self.flight[i].at <= now {
                due.push(self.flight.swap_remove(i));
            } else {
                i += 1;
            }
        }
        due.sort_by_key(|f| f.at);
        for f in due {
            if f.to_server {
                if f.to != self.server_addr {
                    continue;
                }
                let h = self.server.handle_datagram(now, SystemTime::now(), f.from, BytesMut::from(&f.data[..]));
                if self.sh.is_none() {
                    self.sh = h;
                }
            } else {
                // client のアドレスが変わったら古いアドレス宛ては届かない
                if f.to != self.client_addr {
                    continue;
                }
                self.client.handle_datagram(now, SystemTime::now(), f.from, BytesMut::from(&f.data[..]));
            }
        }
    }

    /// 一歩進める（送信を集め、次の出来事まで時刻を進め、配送・timer を処理する）。
    pub fn step(&mut self) {
        self.collect();
        let mut next: Option<Instant> = self.flight.iter().map(|f| f.at).min();
        for t in [self.client.poll_timeout(), self.server.poll_timeout()].into_iter().flatten() {
            next = Some(next.map_or(t, |n| n.min(t)));
        }
        // 一度に進める時間は 5ms まで（アプリケーションが応じる機会を与える）
        let next = next.unwrap_or(self.now + Duration::from_millis(5)).min(self.now + Duration::from_millis(5));
        if next > self.now {
            self.now = next;
        }
        self.deliver_due();
        self.client.handle_timeout(self.now);
        self.server.handle_timeout(self.now);
    }

    /// 条件が満たされるまで進める（仮想時刻で `limit` まで）。
    pub fn run_until(&mut self, limit: Duration, mut cond: impl FnMut(&mut Self) -> bool) -> bool {
        let end = self.now + limit;
        while self.now < end {
            if cond(self) {
                return true;
            }
            self.step();
        }
        cond(self)
    }

    pub fn flight_info(&self) -> Vec<(std::time::Duration, bool, usize)> {
        self.flight.iter().map(|f| (f.at.saturating_duration_since(self.now), f.to_server, f.data.len())).collect()
    }

    pub fn handshake(&mut self) {
        assert!(
            self.run_until(Duration::from_secs(10), |n| {
                n.sh.is_some()
                    && n.client.connection(n.ch).is_some_and(|c| c.is_handshake_confirmed())
                    && n.sh.and_then(|h| n.server.connection(h)).is_some_and(|c| c.is_handshake_confirmed())
            }),
            "handshake did not complete"
        );
    }

    pub fn drain_events(&mut self, client: bool) -> Vec<Event> {
        let mut v = Vec::new();
        let c = if client { self.c() } else { self.s() };
        while let Some(e) = c.poll_event() {
            v.push(e);
        }
        v
    }
}

/// stream を最後まで読む（読めた分を `out` に足す）。FIN まで読んだら `true`。
pub fn read_all(c: &mut Connection, id: StreamId, out: &mut Vec<u8>) -> bool {
    loop {
        match c.stream_recv(id, 64 * 1024) {
            Ok(ch) => {
                out.extend_from_slice(&ch.data);
                if ch.fin {
                    return true;
                }
                if ch.data.is_empty() {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
}
