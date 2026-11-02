//! HTTP/3・WebTransport の結合試験（メモリ上の QUIC の上で）。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::{Bytes, BytesMut};
use senca_h3::*;
use senca_quic::{ClientConfig, ConnectionHandle, Endpoint, EndpointConfig, ServerConfig, Side, StreamId};
use senca_tls::{CertifiedKey, RootStore, ServerVerifier};

const CA: &[u8] = include_bytes!("../../senca-tls/tests/data/ca.pem");
const CHAIN: &[u8] = include_bytes!("../../senca-tls/tests/data/server-ecdsa.pem");
const KEY: &[u8] = include_bytes!("../../senca-tls/tests/data/server-ecdsa.key");

struct Pair {
    now: Instant,
    client: Endpoint,
    server: Endpoint,
    ch: ConnectionHandle,
    sh: Option<ConnectionHandle>,
    ch3: H3Connection,
    sh3: H3Connection,
    client_h3: bool,
    caddr: SocketAddr,
    saddr: SocketAddr,
    cev: Vec<H3Event>,
    sev: Vec<H3Event>,
}

impl Pair {
    fn new() -> Self {
        let mut stls = senca_tls::ServerConfig::new(Arc::new(CertifiedKey::from_pem(CHAIN, KEY).unwrap())).unwrap();
        stls.alpn_protocols = vec![ALPN.to_vec()];
        let mut roots = RootStore::new();
        roots.add_pem(CA).unwrap();
        let mut ctls = senca_tls::ClientConfig::new(ServerVerifier::WebPki {
            roots: Arc::new(roots),
            spki_pins: vec![],
        });
        ctls.alpn_protocols = vec![ALPN.to_vec()];
        let now = Instant::now();
        let server = Endpoint::new(EndpointConfig::new().unwrap(), Some(Arc::new(ServerConfig::new(Arc::new(stls)))));
        let mut client = Endpoint::new(EndpointConfig::new().unwrap(), None);
        let saddr: SocketAddr = "10.0.0.1:443".parse().unwrap();
        let ch = client
            .connect(now, SystemTime::now(), ClientConfig::new(Arc::new(ctls)), saddr, "localhost")
            .unwrap();
        Self {
            now,
            client,
            server,
            ch,
            sh: None,
            ch3: H3Connection::new(Side::Client, H3Config::default()),
            sh3: H3Connection::new(Side::Server, H3Config::default()),
            client_h3: true,
            caddr: "10.0.0.2:5000".parse().unwrap(),
            saddr,
            cev: Vec::new(),
            sev: Vec::new(),
        }
    }

    fn step(&mut self) {
        self.now += Duration::from_millis(1);
        let now = self.now;
        if self.client_h3
            && let Some(c) = self.client.connection_mut(self.ch)
        {
            self.ch3.process(c, now);
        }
        if let Some(h) = self.sh
            && let Some(c) = self.server.connection_mut(h)
        {
            self.sh3.process(c, now);
        }
        while let Some(t) = self.client.poll_transmit(now) {
            let h = self.server.handle_datagram(now, SystemTime::now(), self.caddr, BytesMut::from(&t.contents[..]));
            if self.sh.is_none() {
                self.sh = h;
            }
        }
        while let Some(t) = self.server.poll_transmit(now) {
            self.client.handle_datagram(now, SystemTime::now(), self.saddr, BytesMut::from(&t.contents[..]));
        }
        self.client.handle_timeout(now);
        self.server.handle_timeout(now);
        if self.client_h3
            && let Some(c) = self.client.connection_mut(self.ch)
        {
            self.ch3.process(c, now);
        }
        if let Some(h) = self.sh
            && let Some(c) = self.server.connection_mut(h)
        {
            self.sh3.process(c, now);
        }
        while let Some(e) = self.ch3.poll_event() {
            self.cev.push(e);
        }
        while let Some(e) = self.sh3.poll_event() {
            self.sev.push(e);
        }
    }

    fn run(&mut self, n: usize, mut f: impl FnMut(&mut Self) -> bool) -> bool {
        for _ in 0..n {
            if f(self) {
                return true;
            }
            self.step();
        }
        f(self)
    }

    fn cq(&mut self) -> &mut senca_quic::Connection {
        self.client.connection_mut(self.ch).unwrap()
    }

    fn connected(&mut self) {
        assert!(self.run(500, |p| p.ch3.peer_settings().is_some() && p.sh3.peer_settings().is_some()));
    }
}

/// 二つの H3 と QUIC を同時に借りる。
macro_rules! with_c {
    ($p:expr, |$h:ident, $q:ident| $body:expr) => {{
        let $q = $p.client.connection_mut($p.ch).unwrap();
        let $h = &mut $p.ch3;
        $body
    }};
}
macro_rules! with_s {
    ($p:expr, |$h:ident, $q:ident| $body:expr) => {{
        let $q = $p.server.connection_mut($p.sh.unwrap()).unwrap();
        let $h = &mut $p.sh3;
        $body
    }};
}

#[test]
fn request_response_with_body() {
    let mut p = Pair::new();
    p.connected();
    let settings = p.ch3.peer_settings().unwrap().clone();
    assert!(settings.enable_connect_protocol && settings.h3_datagram && settings.webtransport_max_sessions > 0);
    let body: Vec<u8> = (0..200_000).map(|i| (i % 253) as u8).collect();
    let req = Headers::new()
        .with(":method", "POST")
        .with(":scheme", "https")
        .with(":authority", "localhost")
        .with(":path", "/upload")
        .with("content-type", "application/octet-stream");
    let sid = with_c!(p, |h, q| h.send_request(q, req.clone(), false)).unwrap();
    let mut off = 0;
    let mut got = Vec::new();
    let mut responded = false;
    let mut resp_body = Vec::new();
    let ok = p.run(5000, |p| {
        if off < body.len() {
            let n = with_c!(p, |h, q| h.send_data(q, sid, Bytes::copy_from_slice(&body[off..]), true)).unwrap();
            off += n;
        }
        let evs = std::mem::take(&mut p.sev);
        for e in evs {
            match e {
                H3Event::Request { stream, headers } => {
                    assert_eq!(stream, sid);
                    assert_eq!(headers.get(":path"), Some(&b"/upload"[..]));
                }
                H3Event::Data { stream } => {
                    while let Some(b) = with_s!(p, |h, q| h.recv_body(q, stream, 10_000)) {
                        got.extend_from_slice(&b);
                    }
                }
                H3Event::Finished { stream } => {
                    assert_eq!(got, body);
                    let r = Headers::new().with(":status", "200").with("content-type", "text/plain");
                    with_s!(p, |h, q| h.send_response(q, stream, r, false)).unwrap();
                    with_s!(p, |h, q| h.send_data(q, stream, Bytes::from_static(b"ok:200000"), true)).unwrap();
                    responded = true;
                }
                _ => {}
            }
        }
        let evs = std::mem::take(&mut p.cev);
        for e in evs {
            match e {
                H3Event::Response { headers, .. } => assert_eq!(headers.status(), Some(200)),
                H3Event::Data { stream } => {
                    while let Some(b) = with_c!(p, |h, q| h.recv_body(q, stream, 100)) {
                        resp_body.extend_from_slice(&b);
                    }
                }
                H3Event::Finished { .. } => return true,
                _ => {}
            }
        }
        false
    });
    assert!(ok && responded);
    assert_eq!(resp_body, b"ok:200000");
}

#[test]
fn webtransport_session_streams_datagrams_and_close() {
    let mut p = Pair::new();
    p.connected();
    let origin = Headers::new().with("origin", "https://localhost");
    let session = with_c!(p, |h, q| h.wt_connect(q, "localhost", "/wt", origin.clone())).unwrap();
    // server: 受け付ける
    assert!(p.run(200, |p| p.sev.iter().any(|e| matches!(e, H3Event::WebTransportRequest { .. }))));
    let req = p.sev.drain(..).find_map(|e| match e {
        H3Event::WebTransportRequest { session, headers } => Some((session, headers)),
        _ => None,
    });
    let (ss, headers) = req.unwrap();
    assert_eq!(ss, session);
    assert_eq!(headers.get(":path"), Some(&b"/wt"[..]));
    assert_eq!(headers.get("origin"), Some(&b"https://localhost"[..]));
    with_s!(p, |h, q| h.wt_accept(q, ss, Headers::new())).unwrap();
    assert!(p.run(200, |p| p.cev.iter().any(|e| matches!(e, H3Event::WebTransportEstablished { .. }))));
    p.cev.clear();

    // client が bidi を開いて送る → server が echo する
    let bi = with_c!(p, |h, q| h.wt_open(q, session, WtStreamKind::Bi)).unwrap();
    with_c!(p, |h, q| h.wt_send(q, bi, Bytes::from_static(b"ping over wt"))).unwrap();
    with_c!(p, |h, q| h.wt_finish(q, bi)).unwrap();
    // server が uni を開いて送る
    let uni = with_s!(p, |h, q| h.wt_open(q, ss, WtStreamKind::Uni)).unwrap();
    with_s!(p, |h, q| h.wt_send(q, uni, Bytes::from_static(b"server uni"))).unwrap();
    with_s!(p, |h, q| h.wt_finish(q, uni)).unwrap();
    // datagram（両方向）
    with_c!(p, |h, q| h.wt_send_datagram(q, session, b"dg-c2s")).unwrap();
    with_s!(p, |h, q| h.wt_send_datagram(q, ss, b"dg-s2c")).unwrap();

    let mut server_got = Vec::new();
    let mut echoed = false;
    let mut client_bi = Vec::new();
    let mut client_uni = Vec::new();
    let mut dg_c = None;
    let mut dg_s = None;
    let ok = p.run(1000, |p| {
        let evs = std::mem::take(&mut p.sev);
        for e in evs {
            match e {
                H3Event::WebTransportStream { session: s, stream, kind } => {
                    assert_eq!((s, kind), (ss, WtStreamKind::Bi));
                    assert_eq!(stream, bi);
                }
                H3Event::WebTransportReadable { stream, .. } => loop {
                    let Ok(c) = with_s!(p, |h, q| h.wt_recv(q, stream, 1000)) else { break };
                    server_got.extend_from_slice(&c.data);
                    if c.fin && !echoed {
                        echoed = true;
                        with_s!(p, |h, q| h.wt_send(q, stream, Bytes::from(server_got.clone()))).unwrap();
                        with_s!(p, |h, q| h.wt_finish(q, stream)).unwrap();
                    }
                    if c.data.is_empty() || c.fin {
                        break;
                    }
                },
                H3Event::WebTransportDatagram { data, .. } => dg_s = Some(data),
                _ => {}
            }
        }
        let evs = std::mem::take(&mut p.cev);
        for e in evs {
            match e {
                H3Event::WebTransportStream { stream, kind, .. } => {
                    assert_eq!((stream, kind), (uni, WtStreamKind::Uni));
                }
                H3Event::WebTransportReadable { stream, .. } => loop {
                    let c = with_c!(p, |h, q| h.wt_recv(q, stream, 1000));
                    let Ok(c) = c else { break };
                    if stream == bi {
                        client_bi.extend_from_slice(&c.data);
                    } else {
                        client_uni.extend_from_slice(&c.data);
                    }
                    if c.data.is_empty() || c.fin {
                        break;
                    }
                },
                H3Event::WebTransportDatagram { data, .. } => dg_c = Some(data),
                _ => {}
            }
        }
        client_bi == b"ping over wt" && client_uni == b"server uni" && dg_c.is_some() && dg_s.is_some()
    });
    assert!(ok, "bi={client_bi:?} uni={client_uni:?} dg_c={dg_c:?} dg_s={dg_s:?} server_got={server_got:?} echoed={echoed}");
    assert_eq!(dg_c.unwrap(), &b"dg-s2c"[..]);
    assert_eq!(dg_s.unwrap(), &b"dg-c2s"[..]);

    // session を閉じる（client から、コードと理由つき）
    with_c!(p, |h, q| h.wt_close(q, session, 42, "done"));
    assert!(p.run(200, |p| p.sev.iter().any(|e| matches!(
        e,
        H3Event::WebTransportClosed { code: 42, reason, .. } if reason == "done"
    ))));
    assert!(p.cev.iter().any(|e| matches!(e, H3Event::WebTransportClosed { code: 42, .. })));
    assert!(p.ch3.wt_sessions().is_empty() && p.sh3.wt_sessions().is_empty());
}

#[test]
fn webtransport_reject_and_goaway() {
    let mut p = Pair::new();
    p.connected();
    let s = with_c!(p, |h, q| h.wt_connect(q, "localhost", "/nope", Headers::new())).unwrap();
    assert!(p.run(200, |p| p.sev.iter().any(|e| matches!(e, H3Event::WebTransportRequest { .. }))));
    with_s!(p, |h, q| h.wt_reject(q, s, 404)).unwrap();
    assert!(p.run(200, |p| p.cev.iter().any(|e| matches!(e, H3Event::WebTransportRejected { status: 404, .. }))));
    with_s!(p, |h, q| h.goaway(q));
    assert!(p.run(200, |p| p.cev.iter().any(|e| matches!(e, H3Event::GoAway { id: 4 }))));
    let req = Headers::new()
        .with(":method", "GET")
        .with(":scheme", "https")
        .with(":authority", "localhost")
        .with(":path", "/");
    assert!(with_c!(p, |h, q| h.send_request(q, req, true)).is_err());
}

#[test]
fn malformed_request_is_reset() {
    let mut p = Pair::new();
    p.connected();
    // 疑似ヘッダーのない HEADERS を直接 QUIC に書く
    let q = p.cq();
    let id = q.open_stream(senca_quic::Dir::Bi).unwrap();
    let block = qpack::Encoder.encode(&Headers::new().with("x", "y"));
    let mut f = vec![0x01];
    senca_quic::varint::write(&mut f, block.len() as u64);
    f.extend_from_slice(&block);
    q.stream_send(id, &f).unwrap();
    q.stream_finish(id).unwrap();
    let mut reset = false;
    p.run(200, |p| {
        reset = matches!(
            p.cq().stream_recv(id, 100),
            Err(senca_quic::StreamError::Reset(c)) if c == H3ErrorCode::MESSAGE_ERROR.0
        );
        reset
    });
    assert!(reset);
    assert!(p.sev.iter().all(|e| !matches!(e, H3Event::Request { .. })));
}

#[test]
fn missing_settings_closes_connection() {
    let mut p = Pair::new();
    p.client_h3 = false;
    // client 側の H3 を動かさず、control stream に SETTINGS 以外を書く
    assert!(p.run(100, |p| p.sh.is_some() && p.cq().is_established()));
    let q = p.cq();
    let id = q.open_stream(senca_quic::Dir::Uni).unwrap();
    q.stream_send(id, &[0x00, 0x07, 0x01, 0x00]).unwrap(); // control + GOAWAY
    let mut closed = false;
    for _ in 0..200 {
        p.now += Duration::from_millis(1);
        let now = p.now;
        if let Some(h) = p.sh
            && let Some(c) = p.server.connection_mut(h)
        {
            p.sh3.process(c, now);
        }
        while let Some(t) = p.client.poll_transmit(now) {
            p.server.handle_datagram(now, SystemTime::now(), p.caddr, BytesMut::from(&t.contents[..]));
        }
        while let Some(t) = p.server.poll_transmit(now) {
            p.client.handle_datagram(now, SystemTime::now(), p.saddr, BytesMut::from(&t.contents[..]));
        }
        if let Some(c) = p.client.connection_mut(p.ch) {
            while let Some(e) = c.poll_event() {
                if let senca_quic::Event::ConnectionLost(senca_quic::ConnectionError::ApplicationClosed { code, .. }) = e {
                    assert_eq!(code, H3ErrorCode::MISSING_SETTINGS.0);
                    closed = true;
                }
            }
        }
        if closed {
            break;
        }
    }
    assert!(closed);
}

#[test]
fn fuzz_parsers_do_not_panic() {
    let mut x: u64 = 0x1234_5678_9abc_def1;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _ in 0..20_000 {
        let len = (rnd() % 64) as usize;
        let data: Vec<u8> = (0..len).map(|_| rnd() as u8).collect();
        let mut d = qpack::Decoder::new(4096, 4, 4096);
        let _ = d.on_encoder_stream(&data);
        let _ = d.decode(0, &data);
        let mut buf = Vec::new();
        let _ = qpack::Decoder::on_decoder_stream(&mut buf, &data);
    }
    let _ = StreamId(0);
}
