//! QUIC v1 の受け口（`--quic ADDR`）: HTTP/3・WebTransport・MoQ（draft-22）を一つの UDP ポートで。
//!
//! - 一つの UDP socket と、sans-io の [`senca_quic::Endpoint`] を一つの tokio タスクで回す
//!   （受信・timer・送信。接続ごとのタスクは作らない）。
//! - ALPN `h3`: HTTP/3（RFC 9114）。TCP の HTTP と同じ経路（[`crate::http::route`]）で答える
//!   （`/api/v1`・署名付きの HLS と LL-HLS のブロッキング・リロード・`/healthz` など）。要求ごとに
//!   tokio のタスクで経路を動かし、応答を [`Msg::Reply`] で受けて書き出す。SSE（`/api/v1/events`）と
//!   部屋のシグナリング（WebSocket）は HTTP/3 では扱わない（501）。
//!   同じ接続の拡張 CONNECT（`:protocol = webtransport`、`:path = /moq`）は WebTransport の MoQ。
//! - ALPN `moqt-22`: native QUIC の MoQ。
//! - MoQ のセッションの中身（購読・送出・認可）は [`crate::moq`]。

use crate::app::App;
use crate::http::Reply;
use crate::moq::{Action, FeedEvent, Feeds, MoqPeer, PeerKey};
use bytes::{Bytes, BytesMut};
use senca_h3::{H3Config, H3Connection, H3Event};
use senca_http::{BodyFraming, HeaderMap, Method, RequestHead, Version};
use senca_moq::transport::{feed_quic, feed_webtransport, flush_quic, flush_webtransport};
use senca_moq::{Role, Session, SessionConfig, offers_protocol, publish_done};
use senca_quic::{ConnectionHandle, Endpoint, EndpointConfig, EndpointEvent, ServerConfig, StreamId};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// WebTransport の MoQ の :path（query に再生の署名）。
pub const WT_PATH: &str = "/moq";
/// 接続があるときの見回りの間隔（MoQ の配送期限など）。
const TICK: Duration = Duration::from_millis(50);
const IDLE_TICK: Duration = Duration::from_secs(1);
/// 一度に読む datagram の数（他の仕事を待たせない）。
const RECV_BURST: usize = 256;
/// 同時の接続の上限。
const MAX_CONNECTIONS: usize = 4096;

/// 駆動タスクへの知らせ。
pub enum Msg {
    /// HTTP/3 の要求への応答（経路のタスクから）。
    Reply { conn: ConnectionHandle, stream: StreamId, reply: Reply, head_only: bool },
    /// stream の変換の出力（[`crate::moq`] のタスクから）。
    Feed { stream: String, generation: u64, ev: FeedEvent },
    FeedEnded { stream: String, generation: u64 },
}

/// 受け口を開く（UDP を bind し、駆動タスクを始める）。
pub async fn start(app: Arc<App>, addr: SocketAddr) -> Result<(), String> {
    let (cert, key) = match (&app.cfg.tls_cert, &app.cfg.tls_key) {
        (Some(c), Some(k)) => (c.clone(), k.clone()),
        _ => return Err("--quic needs --tls-cert and --tls-key".into()),
    };
    let ck = senca_tls::CertifiedKey::from_pem_files(&cert, &key).map_err(|e| format!("quic: {}: {e:?}", cert.display()))?;
    let mut tls = senca_tls::ServerConfig::new(Arc::new(ck)).map_err(|e| format!("quic: tls: {e:?}"))?;
    tls.alpn_protocols = vec![senca_h3::ALPN.to_vec(), senca_moq::ALPN.to_vec()];
    let mut sc = ServerConfig::new(Arc::new(tls));
    sc.max_connections = MAX_CONNECTIONS;
    let ec = EndpointConfig::new().map_err(|e| format!("quic: rng: {e}"))?;
    let ep = Endpoint::new(ec, Some(Arc::new(sc)));
    let sock = UdpSocket::bind(addr).await.map_err(|e| format!("quic {addr}: {e}"))?;
    let (tx, rx) = mpsc::channel(4096);
    app.log(&format!("quic listening on {addr} (HTTP/3, WebTransport, MoQ {})", String::from_utf8_lossy(senca_moq::ALPN)));
    let d = Driver { app, ep, conns: HashMap::new(), feeds: Feeds::default(), tx };
    tokio::spawn(d.run(sock, rx));
    Ok(())
}

enum Conn {
    /// ハンドシェイク中（ALPN がまだ分からない）。
    Pending,
    Native(Box<MoqPeer>),
    H3(Box<H3Peer>),
    Closed,
}

struct Driver {
    app: Arc<App>,
    ep: Endpoint,
    conns: HashMap<ConnectionHandle, Conn>,
    feeds: Feeds,
    tx: mpsc::Sender<Msg>,
}

fn peer_mut(conns: &mut HashMap<ConnectionHandle, Conn>, k: PeerKey) -> Option<&mut MoqPeer> {
    match (conns.get_mut(&k.conn)?, k.wt) {
        (Conn::Native(p), None) => Some(p),
        (Conn::H3(h), Some(s)) => h.wt.get_mut(&s),
        _ => None,
    }
}

impl Driver {
    async fn run(mut self, sock: UdpSocket, mut rx: mpsc::Receiver<Msg>) {
        let mut buf = vec![0u8; 65536];
        let mut last_gc = Instant::now();
        loop {
            let now = Instant::now();
            let tick = if self.ep.is_empty() { IDLE_TICK } else { TICK };
            let deadline = self.ep.poll_timeout().map_or(now + tick, |t| t.min(now + tick));
            tokio::select! {
                r = sock.recv_from(&mut buf) => {
                    if let Ok((n, from)) = r {
                        self.ep.handle_datagram(Instant::now(), SystemTime::now(), from, BytesMut::from(&buf[..n]));
                    }
                    for _ in 0..RECV_BURST {
                        match sock.try_recv_from(&mut buf) {
                            Ok((n, from)) => {
                                self.ep.handle_datagram(Instant::now(), SystemTime::now(), from, BytesMut::from(&buf[..n]));
                            }
                            Err(_) => break,
                        }
                    }
                }
                m = rx.recv() => {
                    // 送り手（self.tx）を持っているので閉じない
                    let Some(m) = m else { return };
                    self.on_msg(m);
                    for _ in 0..1024 {
                        match rx.try_recv() {
                            Ok(m) => self.on_msg(m),
                            Err(_) => break,
                        }
                    }
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
            }
            let now = Instant::now();
            self.ep.handle_timeout(now);
            self.drive(now);
            while let Some(t) = self.ep.poll_transmit(Instant::now()) {
                match sock.try_send_to(&t.contents, t.destination) {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        let _ = sock.send_to(&t.contents, t.destination).await;
                    }
                    Err(_) => {}
                }
            }
            if now.duration_since(last_gc) >= Duration::from_secs(1) {
                last_gc = now;
                self.feeds.gc(now);
            }
        }
    }

    fn on_msg(&mut self, m: Msg) {
        let now = Instant::now();
        match m {
            Msg::Reply { conn, stream, reply, head_only } => {
                let Some(q) = self.ep.connection_mut(conn) else { return };
                if let Some(Conn::H3(h)) = self.conns.get_mut(&conn) {
                    h.write_reply(q, stream, reply, head_only);
                }
            }
            Msg::Feed { stream, generation, ev } => {
                for (k, a) in self.feeds.on_event(&stream, generation, ev) {
                    let Some(p) = peer_mut(&mut self.conns, k) else { continue };
                    match a {
                        Action::Send { req, object } => {
                            let _ = p.session.send_object(req, &object, now, now);
                        }
                        Action::EndGroup { req, group } => p.session.end_subgroup(req, group, 0, now),
                    }
                }
            }
            Msg::FeedEnded { stream, generation } => {
                for (k, req) in self.feeds.on_ended(&stream, generation) {
                    if let Some(p) = peer_mut(&mut self.conns, k) {
                        p.session.publish_done(req, publish_done::TRACK_ENDED, "stream ended");
                    }
                }
            }
        }
    }

    /// 全接続を進める（受信の取り込み・要求の処理・送信の書き出し）。
    fn drive(&mut self, now: Instant) {
        let handles: Vec<ConnectionHandle> = self.ep.connections().collect();
        for h in handles {
            let Some(q) = self.ep.connection_mut(h) else { continue };
            let st = self.conns.entry(h).or_insert(Conn::Pending);
            if matches!(st, Conn::Pending) && q.is_established() {
                let peer = q.remote_address().to_string();
                *st = match q.alpn() {
                    Some(a) if a == senca_moq::ALPN => {
                        Conn::Native(Box::new(MoqPeer::new(Session::new(SessionConfig::new(Role::Server), now), None, peer)))
                    }
                    Some(a) if a == senca_h3::ALPN => Conn::H3(Box::new(H3Peer::new(peer))),
                    _ => {
                        q.close(now, 0, b"unsupported ALPN");
                        Conn::Closed
                    }
                };
            }
            match st {
                Conn::Native(p) => {
                    feed_quic(&mut p.session, q, now);
                    p.handle_events(PeerKey { conn: h, wt: None }, &self.app, &mut self.feeds, &self.tx, now);
                    flush_quic(&mut p.session, q, now);
                }
                Conn::H3(hp) => hp.drive(h, q, &self.app, &mut self.feeds, &self.tx, now),
                Conn::Pending | Conn::Closed => {}
            }
        }
        while let Some(e) = self.ep.poll_event() {
            if let EndpointEvent::Drained(h) = e {
                match self.conns.remove(&h) {
                    Some(Conn::Native(_)) => self.feeds.remove_peer(PeerKey { conn: h, wt: None }),
                    Some(Conn::H3(hp)) => {
                        for s in hp.wt.keys() {
                            self.feeds.remove_peer(PeerKey { conn: h, wt: Some(*s) });
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// 受けている途中の HTTP/3 の要求。
struct H3Req {
    head: RequestHead,
    body: BytesMut,
    limit: u64,
}

/// HTTP/3 の接続（要求と WebTransport の MoQ）。
struct H3Peer {
    h3: H3Connection,
    peer: String,
    reqs: HashMap<StreamId, H3Req>,
    /// 書ききれていない応答の本文。
    pending: Vec<(StreamId, Bytes)>,
    wt: HashMap<StreamId, MoqPeer>,
}

impl H3Peer {
    fn new(peer: String) -> Self {
        Self { h3: H3Connection::new(senca_quic::Side::Server, H3Config::default()), peer, reqs: HashMap::new(), pending: Vec::new(), wt: HashMap::new() }
    }

    #[allow(clippy::too_many_arguments)]
    fn drive(&mut self, h: ConnectionHandle, q: &mut senca_quic::Connection, app: &Arc<App>, feeds: &mut Feeds, tx: &mpsc::Sender<Msg>, now: Instant) {
        self.h3.process(q, now);
        let mut evs = Vec::new();
        while let Some(e) = self.h3.poll_event() {
            evs.push(e);
        }
        for e in &evs {
            match e {
                H3Event::Request { stream, headers } => match request_head(headers) {
                    Ok(head) => {
                        let limit = crate::http::body_limit(head.path());
                        self.reqs.insert(*stream, H3Req { head, body: BytesMut::new(), limit });
                    }
                    Err(status) => self.write_reply(q, *stream, Reply::status(status), false),
                },
                H3Event::Data { stream } => {
                    while let Some(b) = self.h3.recv_body(q, *stream, 64 * 1024) {
                        let Some(r) = self.reqs.get_mut(stream) else { continue };
                        if r.body.len() as u64 + b.len() as u64 > r.limit {
                            self.reqs.remove(stream);
                            self.write_reply(q, *stream, Reply::status(413), false);
                            self.h3.reset_stream(q, *stream, senca_h3::H3ErrorCode::REQUEST_REJECTED);
                            break;
                        }
                        r.body.extend_from_slice(&b);
                    }
                }
                H3Event::Finished { stream } => {
                    if let Some(r) = self.reqs.remove(stream) {
                        self.dispatch(h, *stream, r, app, tx, q);
                    }
                }
                H3Event::StreamReset { stream, .. } => {
                    self.reqs.remove(stream);
                }
                H3Event::WebTransportRequest { session, headers } => self.on_wt_request(q, *session, headers, now),
                _ => {}
            }
        }
        for (wt, p) in self.wt.iter_mut() {
            feed_webtransport(&mut p.session, &mut self.h3, q, *wt, &evs, now);
            p.handle_events(PeerKey { conn: h, wt: Some(*wt) }, app, feeds, tx, now);
        }
        for e in &evs {
            if let H3Event::WebTransportClosed { session, .. } = e
                && self.wt.remove(session).is_some()
            {
                feeds.remove_peer(PeerKey { conn: h, wt: Some(*session) });
            }
        }
        for (wt, p) in self.wt.iter_mut() {
            flush_webtransport(&mut p.session, &mut self.h3, q, *wt, now);
        }
        let pend = std::mem::take(&mut self.pending);
        for (s, d) in pend {
            if let Ok(n) = self.h3.send_data(q, s, d.clone(), true)
                && n < d.len()
            {
                self.pending.push((s, d.slice(n..)));
            }
        }
        self.h3.process(q, now);
    }

    fn on_wt_request(&mut self, q: &mut senca_quic::Connection, session: StreamId, headers: &senca_h3::Headers, now: Instant) {
        let path = headers.get_str(":path").unwrap_or("");
        let (p, query) = match path.split_once('?') {
            Some((a, b)) => (a, Some(b)),
            None => (path, None),
        };
        if p != WT_PATH {
            let _ = self.h3.wt_reject(q, session, 404);
            return;
        }
        // 版は wt-available-protocols で決める（無い・違うものは推測しない）
        if offers_protocol(headers, senca_moq::WT_PROTOCOL) != Some(true) {
            let _ = self.h3.wt_reject(q, session, 400);
            return;
        }
        if query.is_some_and(|q| q.len() > 2048) {
            let _ = self.h3.wt_reject(q, session, 414);
            return;
        }
        let extra = senca_h3::Headers::new().with("wt-protocol", format!("\"{}\"", senca_moq::WT_PROTOCOL));
        if self.h3.wt_accept(q, session, extra).is_ok() {
            let mut cfg = SessionConfig::new(Role::Server);
            cfg.native_quic = false;
            self.wt.insert(session, MoqPeer::new(Session::new(cfg, now), query.map(str::to_string), self.peer.clone()));
        }
    }

    fn dispatch(&mut self, h: ConnectionHandle, stream: StreamId, mut r: H3Req, app: &Arc<App>, tx: &mpsc::Sender<Msg>, q: &mut senca_quic::Connection) {
        let path = r.head.path();
        // 接続を握り続けるもの（SSE・WebSocket）は HTTP/1.1 で
        if path == "/api/v1/events" || path == "/signaling" {
            self.write_reply(q, stream, Reply::status(501), false);
            return;
        }
        r.head.body = BodyFraming::Length(r.body.len() as u64);
        let head_only = r.head.method == Method::Head;
        let (app, tx, peer) = (app.clone(), tx.clone(), self.peer.clone());
        let body = r.body.freeze();
        let head = r.head;
        tokio::spawn(async move {
            let reply = crate::http::route(&app, &head, body, &peer).await;
            let _ = tx.send(Msg::Reply { conn: h, stream, reply, head_only }).await;
        });
    }

    fn write_reply(&mut self, q: &mut senca_quic::Connection, stream: StreamId, r: Reply, head_only: bool) {
        let mut hd = senca_h3::Headers::new().with(":status", r.status.to_string());
        let mut has_len = false;
        for (k, v) in &r.headers {
            let k = k.to_ascii_lowercase();
            // HTTP/3 では接続の header を使わない（RFC 9114 4.2）
            if matches!(k.as_str(), "connection" | "keep-alive" | "transfer-encoding" | "upgrade" | "proxy-connection") {
                continue;
            }
            has_len |= k == "content-length";
            hd = hd.with(k, v);
        }
        let bodyless = r.status == 204 || r.status == 304 || (100..200).contains(&r.status);
        if !has_len && !bodyless {
            hd = hd.with("content-length", r.body.len().to_string());
        }
        let no_body = head_only || bodyless || r.body.is_empty();
        if self.h3.send_response(q, stream, hd, no_body).is_err() {
            // header に使えない値があった
            let hd = senca_h3::Headers::new().with(":status", "500").with("content-length", "0");
            let _ = self.h3.send_response(q, stream, hd, true);
            return;
        }
        if !no_body {
            match self.h3.send_data(q, stream, r.body.clone(), true) {
                Ok(n) if n < r.body.len() => self.pending.push((stream, r.body.slice(n..))),
                _ => {}
            }
        }
    }
}

/// HTTP/3 の header から、TCP の経路が読む形（[`RequestHead`]）を作る。
fn request_head(hd: &senca_h3::Headers) -> Result<RequestHead, u16> {
    let method = hd.get_str(":method").ok_or(400u16)?;
    let path = hd.get_str(":path").ok_or(400u16)?;
    if !path.starts_with('/') || path.len() > 8192 || !path.bytes().all(|c| c.is_ascii_graphic()) {
        return Err(400);
    }
    let method = Method::parse(method.as_bytes()).map_err(|_| 400u16)?;
    if !matches!(method, Method::Get | Method::Head | Method::Post | Method::Put | Method::Patch | Method::Delete | Method::Options) {
        return Err(501);
    }
    let mut headers = HeaderMap::new();
    if let Some(a) = hd.get(":authority") {
        headers.append("host", Bytes::copy_from_slice(a)).map_err(|_| 400u16)?;
    }
    for f in hd.iter() {
        if f.name.first() == Some(&b':') {
            continue;
        }
        let name = std::str::from_utf8(&f.name).map_err(|_| 400u16)?;
        headers.append(name, Bytes::copy_from_slice(&f.value)).map_err(|_| 400u16)?;
    }
    Ok(RequestHead {
        method,
        target: path.to_string(),
        version: Version::Http11,
        headers,
        body: BodyFraming::None,
        keep_alive: true,
        expect_continue: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_head_from_h3() {
        let hd = senca_h3::Headers::new()
            .with(":method", "GET")
            .with(":scheme", "https")
            .with(":authority", "example.com:4433")
            .with(":path", "/s/1/abc/live/x/index.m3u8?_HLS_msn=3")
            .with("authorization", "Bearer t");
        let h = request_head(&hd).unwrap();
        assert_eq!(h.method, Method::Get);
        assert_eq!(h.path(), "/s/1/abc/live/x/index.m3u8");
        assert_eq!(h.query(), Some("_HLS_msn=3"));
        assert_eq!(h.headers.get_str("host"), Some("example.com:4433"));
        assert_eq!(h.headers.get_str("authorization"), Some("Bearer t"));
        assert_eq!(request_head(&senca_h3::Headers::new().with(":method", "GET").with(":path", "x")).err(), Some(400));
        assert_eq!(request_head(&senca_h3::Headers::new().with(":method", "TRACE").with(":path", "/")).err(), Some(501));
        assert_eq!(request_head(&senca_h3::Headers::new().with(":method", "GET").with(":path", "/a b")).err(), Some(400));
    }
}
