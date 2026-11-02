//! MoQ の relay の server（native QUIC と WebTransport を一つの UDP ポートで）。sans-io。
//!
//! QUIC の ALPN が `moqt-22` なら native QUIC のセッション、`h3` なら HTTP/3 の上の WebTransport
//! （`wt_path` への拡張 CONNECT）を受け付ける。WebTransport では `wt-available-protocols` に
//! `moqt-22` を求め、応答に `wt-protocol` を返す（draft-22 6.2.1、版の交渉）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Instant, SystemTime};

use bytes::BytesMut;
use senca_h3::{H3Config, H3Connection, H3Event, Headers};
use senca_quic::{ConnectionHandle, Endpoint, EndpointEvent, Side, StreamId, Transmit};

use crate::relay::{Relay, SessionId};
use crate::session::{Role, SessionConfig};
use crate::transport::{feed_quic, feed_webtransport, flush_quic, flush_webtransport};
use crate::{ALPN, WT_PROTOCOL};

/// WebTransport の要求の `wt-available-protocols` に `moqt-22` が入っているか（structured field の文字列の並び）。
pub fn offers_protocol(h: &Headers, proto: &str) -> Option<bool> {
    let v = h.get_str("wt-available-protocols")?;
    Some(
        v.split(',')
            .map(|s| s.trim().split(';').next().unwrap_or("").trim().trim_matches('"'))
            .any(|s| s == proto),
    )
}

enum ConnState {
    Native(SessionId),
    H3 {
        h3: Box<H3Connection>,
        sessions: HashMap<StreamId, SessionId>,
    },
    /// ALPN がまだ分からない・使えない。
    Unknown,
}

/// relay の server。
pub struct MoqServer {
    pub endpoint: Endpoint,
    pub relay: Relay,
    conns: HashMap<ConnectionHandle, ConnState>,
    /// WebTransport を受け付ける :path。
    pub wt_path: String,
    /// `wt-available-protocols` が無い WebTransport の要求も受け付ける（ブラウザーの `protocols` 未対応のとき）。
    pub allow_missing_wt_protocol: bool,
}

impl std::fmt::Debug for MoqServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MoqServer").field("connections", &self.conns.len()).finish_non_exhaustive()
    }
}

impl MoqServer {
    pub fn new(endpoint: Endpoint, relay: Relay, wt_path: &str) -> Self {
        Self {
            endpoint,
            relay,
            conns: HashMap::new(),
            wt_path: wt_path.to_string(),
            allow_missing_wt_protocol: false,
        }
    }

    pub fn handle_datagram(&mut self, now: Instant, wall: SystemTime, from: SocketAddr, data: BytesMut) {
        self.endpoint.handle_datagram(now, wall, from, data);
        self.drive(now);
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        self.endpoint.handle_timeout(now);
        self.drive(now);
    }

    pub fn poll_transmit(&mut self, now: Instant) -> Option<Transmit> {
        self.endpoint.poll_transmit(now)
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        self.endpoint.poll_timeout()
    }

    /// 接続・セッションを進める。
    pub fn drive(&mut self, now: Instant) {
        let handles: Vec<ConnectionHandle> = self.endpoint.connections().collect();
        // 受信を各セッションへ
        for h in &handles {
            let Some(conn) = self.endpoint.connection_mut(*h) else { continue };
            let st = self.conns.entry(*h).or_insert(ConnState::Unknown);
            if matches!(st, ConnState::Unknown) && conn.is_established() {
                *st = match conn.alpn() {
                    Some(a) if a == ALPN => {
                        let id = self.relay.add_session(SessionConfig::new(Role::Server), now);
                        ConnState::Native(id)
                    }
                    Some(a) if a == senca_h3::ALPN => ConnState::H3 {
                        h3: Box::new(H3Connection::new(Side::Server, H3Config::default())),
                        sessions: HashMap::new(),
                    },
                    _ => {
                        conn.close(now, 0x0, b"unsupported ALPN");
                        ConnState::Unknown
                    }
                };
            }
            match st {
                ConnState::Native(id) => {
                    if let Some(s) = self.relay.session_mut(*id) {
                        feed_quic(s, conn, now);
                    }
                }
                ConnState::H3 { h3, sessions } => {
                    h3.process(conn, now);
                    let mut evs = Vec::new();
                    while let Some(e) = h3.poll_event() {
                        evs.push(e);
                    }
                    for e in &evs {
                        if let H3Event::WebTransportRequest { session, headers } = e {
                            let path = headers.get_str(":path").unwrap_or("");
                            let path_only = path.split('?').next().unwrap_or("");
                            let offered = offers_protocol(headers, WT_PROTOCOL);
                            let version_ok = match offered {
                                Some(true) => true,
                                Some(false) => false,
                                None => self.allow_missing_wt_protocol,
                            };
                            if path_only != self.wt_path {
                                let _ = h3.wt_reject(conn, *session, 404);
                            } else if !version_ok {
                                // 未知の版・版の指定なし（推測で接続しない）
                                let _ = h3.wt_reject(conn, *session, 400);
                            } else {
                                let extra = if offered == Some(true) {
                                    Headers::new().with("wt-protocol", format!("\"{WT_PROTOCOL}\""))
                                } else {
                                    Headers::new()
                                };
                                if h3.wt_accept(conn, *session, extra).is_ok() {
                                    let mut cfg = SessionConfig::new(Role::Server);
                                    cfg.native_quic = false;
                                    let id = self.relay.add_session(cfg, now);
                                    sessions.insert(*session, id);
                                }
                            }
                        }
                    }
                    for (wt, id) in sessions.iter() {
                        if let Some(s) = self.relay.session_mut(*id) {
                            feed_webtransport(s, h3, conn, *wt, &evs, now);
                        }
                    }
                    // 閉じた WebTransport の session
                    for e in &evs {
                        if let H3Event::WebTransportClosed { session, .. } = e
                            && let Some(id) = sessions.remove(session)
                        {
                            self.relay.remove_session(id);
                        }
                    }
                }
                ConnState::Unknown => {}
            }
        }
        self.relay.process(now);
        // 送信
        for h in &handles {
            let Some(conn) = self.endpoint.connection_mut(*h) else { continue };
            match self.conns.get_mut(h) {
                Some(ConnState::Native(id)) => {
                    if let Some(s) = self.relay.session_mut(*id) {
                        flush_quic(s, conn, now);
                    }
                }
                Some(ConnState::H3 { h3, sessions }) => {
                    for (wt, id) in sessions.iter() {
                        if let Some(s) = self.relay.session_mut(*id) {
                            flush_webtransport(s, h3, conn, *wt, now);
                        }
                    }
                    h3.process(conn, now);
                }
                _ => {}
            }
        }
        // 終わった接続
        while let Some(e) = self.endpoint.poll_event() {
            if let EndpointEvent::Drained(h) = e
                && let Some(st) = self.conns.remove(&h)
            {
                match st {
                    ConnState::Native(id) => self.relay.remove_session(id),
                    ConnState::H3 { sessions, .. } => {
                        for id in sessions.values() {
                            self.relay.remove_session(*id);
                        }
                    }
                    ConnState::Unknown => {}
                }
            }
        }
    }
}
