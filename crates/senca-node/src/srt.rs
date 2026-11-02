//! SRT（仕様 6.8）。受け口（listener）で publish と play を受け、pull（caller）入力と push（caller）出力も扱う。
//!
//! Stream ID の形:
//! - publish: `publish:{stream_key}`、または `#!::r={stream_key},m=publish`
//! - play:    `play:{stream_id}?exp={exp}&sig={sig}`（署名付きの再生 URL と同じ署名）、または `#!::r=...,m=request`
//!
//! パスフレーズ: テナントに `srt-passphrase` という名前の秘密値があれば、そのテナントの stream はそれを要求する
//! （片側だけ暗号の接続は拒否。平文へ黙って降格しない）。

use crate::app::App;
use crate::hub::HubEvent;
use crate::io::IoStatus;
use bytes::{Bytes, BytesMut};
use senca_control::model::{Input, Output};
use senca_srt::{AcceptDecision, AcceptParams, Config, ConnectionId, Event, Listener, ListenerConfig, Passphrase, RejectReason};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// publish の 1 接続から hub へのキュー（TS のメッセージ数）。あふれたら落とす（受け口全体を止めない）。
const PUBLISH_QUEUE: usize = 4096;

enum Mode {
    Publish(String),
    Play(String),
}

fn parse_mode(raw: &str) -> Option<Mode> {
    if let Some(k) = raw.strip_prefix("publish:") {
        return Some(Mode::Publish(k.to_string()));
    }
    if let Some(k) = raw.strip_prefix("play:") {
        return Some(Mode::Play(k.to_string()));
    }
    let sid = senca_srt::StreamId::parse(raw).ok()?;
    let r = sid.resource.clone()?;
    if sid.is_publish() {
        Some(Mode::Publish(r))
    } else {
        Some(Mode::Play(r))
    }
}

fn tenant_passphrase(app: &App, tenant: &str) -> Option<Passphrase> {
    let v = crate::io::secret_value(app, tenant, &Some("secret:srt-passphrase".into())).ok().flatten()?;
    Passphrase::new(v).ok()
}

fn decide(app: &App, req: &senca_srt::AcceptRequest) -> AcceptDecision {
    let Some(raw) = req.stream_id else { return AcceptDecision::Reject(RejectReason::X_FORBIDDEN) };
    let tenant = match parse_mode(raw) {
        Some(Mode::Publish(key)) => {
            let h = senca_control::secret::hash(&key);
            let core = app.control.lock().unwrap();
            core.state.streams.values().find(|s| s.key_hash == h).map(|s| s.tenant.clone())
        }
        Some(Mode::Play(q)) => {
            if app.subscribe_rtmp_play(&q).is_none() {
                None
            } else {
                let id = q.split('?').next().unwrap_or("");
                app.control.lock().unwrap().state.streams.get(id).map(|s| s.tenant.clone())
            }
        }
        None => None,
    };
    let Some(tenant) = tenant else { return AcceptDecision::Reject(RejectReason::X_FORBIDDEN) };
    AcceptDecision::Accept(AcceptParams { passphrase: tenant_passphrase(app, &tenant), ..Default::default() })
}

pub async fn serve(app: Arc<App>, addr: SocketAddr) -> Result<(), String> {
    let sock = Arc::new(UdpSocket::bind(addr).await.map_err(|e| format!("srt {addr}: {e}"))?);
    app.log(&format!("srt listening on {addr}"));
    let app2 = app.clone();
    let cfg = ListenerConfig { conn: Config { latency_ms: 200, ..Config::default() }, max_connections: 1024 };
    let mut l = Listener::new(cfg, move |req| decide(&app2, req)).map_err(|e| format!("{e:?}"))?;
    let t0 = Instant::now();
    let now_us = move || t0.elapsed().as_micros() as u64;
    let mut buf = vec![0u8; 2048];
    let mut publishers: HashMap<ConnectionId, mpsc::Sender<Bytes>> = HashMap::new();
    // play の接続へ送る TS（接続ごとのタスク → ここ）
    let (play_tx, mut play_rx) = mpsc::channel::<(ConnectionId, Bytes)>(8192);
    let mut players: HashMap<ConnectionId, tokio::task::JoinHandle<()>> = HashMap::new();
    loop {
        let now = now_us();
        let wait = l.poll_timeout().map(|t| t.saturating_sub(now)).unwrap_or(20_000).clamp(500, 20_000);
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                if let Ok((n, from)) = r {
                    l.handle_datagram(now_us(), from, &buf[..n]);
                }
            }
            Some((id, data)) = play_rx.recv() => {
                // 空のバイト列は「配信が終わったので閉じる」の合図
                if data.is_empty() { l.close(id, now_us()); } else { let _ = l.send(id, &data, now_us()); }
            }
            _ = tokio::time::sleep(Duration::from_micros(wait)) => {}
        }
        l.handle_timeout(now_us());
        while let Some((to, p)) = l.poll_transmit() {
            let _ = sock.send_to(&p, to).await;
        }
        while let Some(r) = l.poll_rejection() {
            app.log(&format!("srt rejected {} ({})", r.peer, r.reason));
        }
        while let Some((id, ev)) = l.poll_event() {
            match ev {
                Event::Connected(info) => match info.stream_id.as_deref().and_then(parse_mode) {
                    Some(Mode::Publish(key)) => {
                        let (tx, rx) = mpsc::channel(PUBLISH_QUEUE);
                        publishers.insert(id, tx);
                        let app = app.clone();
                        let peer = info.peer.to_string();
                        tokio::spawn(async move { publish_task(app, key, peer, rx).await });
                    }
                    Some(Mode::Play(q)) => {
                        if let Some(sub) = app.subscribe_rtmp_play(&q) {
                            let tx = play_tx.clone();
                            players.insert(id, tokio::spawn(play_task(sub, id, tx)));
                        } else {
                            l.close(id, now_us());
                        }
                    }
                    None => l.close(id, now_us()),
                },
                Event::Data { payload, .. } => {
                    if let Some(tx) = publishers.get(&id)
                        && tx.try_send(payload).is_err()
                    {
                        // 遅れている: 落とす（TS の連続性カウンターで不連続として扱われる）
                    }
                }
                Event::Closed { .. } => {
                    publishers.remove(&id);
                    if let Some(t) = players.remove(&id) {
                        t.abort();
                    }
                }
            }
        }
        if app.shutting_down() {
            return Ok(());
        }
    }
}

/// TS のバイト列を分離して hub へ。
async fn feed_ts(p: &crate::hub::Publisher, d: &mut senca_mpegts::TsDemuxer, data: &[u8]) -> Result<(), String> {
    d.push(data);
    while let Some(ev) = d.poll() {
        match ev {
            senca_mpegts::DemuxEvent::Config { track, mut config } => {
                crate::media::enrich(&mut config);
                p.config(track, config).await.map_err(|e| e.to_string())?;
            }
            senca_mpegts::DemuxEvent::AccessUnit(a) => p.access_unit(a).await.map_err(|e| e.to_string())?,
            _ => {}
        }
    }
    Ok(())
}

async fn publish_task(app: Arc<App>, key: String, peer: String, mut rx: mpsc::Receiver<Bytes>) {
    let p = match app.attach_publisher(&key, "srt", &peer).await {
        Ok(p) => p,
        Err(e) => {
            app.log(&format!("srt publish from {peer} rejected: {e:?}"));
            return;
        }
    };
    app.log(&format!("srt publish started stream={} peer={peer}", p.stream_id()));
    let mut d = senca_mpegts::TsDemuxer::new(Default::default());
    while let Some(b) = rx.recv().await {
        if let Err(e) = feed_ts(&p, &mut d, &b).await {
            app.log(&format!("srt publish {peer}: {e}"));
            break;
        }
    }
    d.flush();
    let _ = feed_ts(&p, &mut d, &[]).await;
    p.detach("srt closed").await;
}

/// hub の AU を TS にして 1316 バイトずつ送る。
pub struct TsSender {
    mux: Option<senca_mpegts::TsMuxer>,
    configs: Vec<(senca_core::TrackId, senca_core::CodecConfig)>,
    buf: BytesMut,
}

impl TsSender {
    pub fn new() -> Self {
        TsSender { mux: None, configs: Vec::new(), buf: BytesMut::new() }
    }

    /// 送るべき 1316 バイトの塊を返す。
    pub fn push(&mut self, ev: &HubEvent) -> Vec<Bytes> {
        match ev {
            HubEvent::Config(t, c) => {
                self.configs.retain(|(x, _)| x != t);
                self.configs.push((*t, c.clone()));
                if let Some(m) = self.mux.as_mut()
                    && m.update_config(*t, c).is_err()
                {
                    self.mux = None;
                }
            }
            HubEvent::Au(a) => {
                if self.mux.is_none() {
                    let mut m = senca_mpegts::TsMuxer::new(senca_mpegts::MuxerConfig { pcr_delay: 9000, ..Default::default() });
                    for (t, c) in &self.configs {
                        let _ = m.add_track(*t, c);
                    }
                    self.mux = Some(m);
                }
                let _ = self.mux.as_mut().unwrap().push(a, &mut self.buf);
            }
            HubEvent::Discontinuity | HubEvent::End => {}
        }
        let mut out = Vec::new();
        while self.buf.len() >= 1316 {
            out.push(self.buf.split_to(1316).freeze());
        }
        out
    }
}

async fn play_task(mut sub: crate::hub::Subscription, id: ConnectionId, tx: mpsc::Sender<(ConnectionId, Bytes)>) {
    let mut s = TsSender::new();
    for ev in sub.initial() {
        for c in s.push(&ev) {
            if tx.send((id, c)).await.is_err() {
                return;
            }
        }
    }
    loop {
        match sub.recv().await {
            Ok(HubEvent::End) => {
                let _ = tx.send((id, Bytes::new())).await;
                return;
            }
            Ok(ev) => {
                for c in s.push(&ev) {
                    if tx.send((id, c)).await.is_err() {
                        return;
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => sub.lagged(n),
            Err(_) => return,
        }
    }
}

/// `srt://host:port?streamid=..&passphrase=..&latency=..` のうち、host・port・streamid・latency を読む。
/// パスフレーズは URL ではなく credential_ref で渡す。
fn parse_srt_url(url: &str) -> Option<(String, u16, Option<String>, Option<u16>)> {
    let rest = url.strip_prefix("srt://")?;
    let (hp, q) = rest.split_once('?').unwrap_or((rest, ""));
    let hp = hp.trim_end_matches('/');
    let (h, p) = hp.rsplit_once(':')?;
    let mut sid = None;
    let mut lat = None;
    for kv in q.split('&') {
        if let Some(v) = kv.strip_prefix("streamid=") {
            sid = Some(v.to_string());
        } else if let Some(v) = kv.strip_prefix("latency=") {
            lat = v.parse::<u32>().ok().map(|us| (us / 1000).min(u16::MAX as u32) as u16);
        }
    }
    Some((h.trim_matches(['[', ']']).to_string(), p.parse().ok()?, sid, lat))
}

/// caller でつなぐ（pull・push）。`on_ready` が送受信を担う。
async fn caller(
    app: &Arc<App>,
    tenant: &str,
    url: &str,
    cred: &Option<String>,
    st: &Mutex<IoStatus>,
) -> Result<(senca_srt::Connection, UdpSocket, Instant), String> {
    caller_with(app, url, crate::io::secret_value(app, tenant, cred)?, st).await
}

async fn caller_with(app: &Arc<App>, url: &str, passphrase: Option<String>, st: &Mutex<IoStatus>) -> Result<(senca_srt::Connection, UdpSocket, Instant), String> {
    let (host, port, sid, lat) = parse_srt_url(url).ok_or("srt:// の URL の形が違います")?;
    let addr = crate::egress::resolve(&host, port, &app.cfg.egress_allow).await?;
    let pass = match passphrase {
        Some(p) => Some(Passphrase::new(p).map_err(|_| "パスフレーズは 10〜79 文字")?),
        None => None,
    };
    let cfg = Config { stream_id: sid, passphrase: pass, latency_ms: lat.unwrap_or(200), ..Config::default() };
    let bind: SocketAddr = if addr.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
    let sock = UdpSocket::bind(bind).await.map_err(|e| e.to_string())?;
    let t0 = Instant::now();
    st.lock().unwrap().state = "connecting".into();
    let c = senca_srt::Connection::connect(cfg, addr, 0).map_err(|e| format!("{e:?}"))?;
    Ok((c, sock, t0))
}

pub async fn pull(app: &Arc<App>, i: &Input, st: &Mutex<IoStatus>) -> Result<(), String> {
    let (mut c, sock, t0) = caller(app, &i.tenant, &i.url, &i.credential_ref, st).await?;
    let now_us = || t0.elapsed().as_micros() as u64;
    let mut buf = vec![0u8; 2048];
    let mut publisher: Option<crate::hub::Publisher> = None;
    let mut d = senca_mpegts::TsDemuxer::new(Default::default());
    loop {
        while let Some((to, p)) = c.poll_transmit() {
            let _ = sock.send_to(&p, to).await;
        }
        let now = now_us();
        let wait = c.poll_timeout().map(|t| t.saturating_sub(now)).unwrap_or(20_000).clamp(500, 20_000);
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                if let Ok((n, from)) = r { c.handle_datagram(now_us(), from, &buf[..n]); }
            }
            _ = tokio::time::sleep(Duration::from_micros(wait)) => {}
        }
        c.handle_timeout(now_us());
        while let Some(ev) = c.poll_event() {
            match ev {
                Event::Connected(_) => {
                    publisher = Some(crate::io::attach(app, &i.stream, "srt_pull", &i.url).await?);
                    st.lock().unwrap().connects += 1;
                    st.lock().unwrap().state = "active".into();
                }
                Event::Data { payload, .. } => {
                    st.lock().unwrap().bytes += payload.len() as u64;
                    if let Some(p) = publisher.as_ref() {
                        feed_ts(p, &mut d, &payload).await?;
                    }
                }
                Event::Closed { reason } => {
                    if let Some(p) = publisher.take() {
                        p.detach("srt pull closed").await;
                    }
                    return Err(format!("SRT が閉じました: {reason:?}"));
                }
            }
        }
    }
}

pub async fn push(app: &Arc<App>, o: &Output, st: &Mutex<IoStatus>) -> Result<(), String> {
    let url = o.url.clone().ok_or("url がありません")?;
    let pass = crate::io::secret_value(app, &o.tenant, &o.credential_ref)?;
    push_to(app, &o.stream, &url, pass, st).await
}

/// stream を外部の SRT（caller）へ送る（PaaS の受け口にも使う）。
pub async fn push_to(app: &Arc<App>, stream: &str, url: &str, passphrase: Option<String>, st: &Mutex<IoStatus>) -> Result<(), String> {
    let hub = loop {
        let h = app.hubs.lock().unwrap().get(stream).cloned();
        match h {
            Some(h) if h.is_live() => break h,
            _ => {
                st.lock().unwrap().state = "waiting_input".into();
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };
    let (mut c, sock, t0) = caller_with(app, url, passphrase, st).await?;
    let now_us = || t0.elapsed().as_micros() as u64;
    let mut buf = vec![0u8; 2048];
    let mut sub: Option<crate::hub::Subscription> = None;
    let mut ts = TsSender::new();
    loop {
        while let Some((to, p)) = c.poll_transmit() {
            st.lock().unwrap().bytes += p.len() as u64;
            let _ = sock.send_to(&p, to).await;
        }
        let now = now_us();
        let wait = c.poll_timeout().map(|t| t.saturating_sub(now)).unwrap_or(20_000).clamp(500, 20_000);
        let recv_ev = async {
            match sub.as_mut() {
                Some(s) => Some(s.recv().await),
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                if let Ok((n, from)) = r { c.handle_datagram(now_us(), from, &buf[..n]); }
            }
            ev = recv_ev => match ev {
                Some(Ok(HubEvent::End)) => { c.close(now_us()); }
                Some(Ok(e)) => for chunk in ts.push(&e) { let _ = c.send(&chunk, now_us()); },
                Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => { if let Some(s) = sub.as_mut() { s.lagged(n) } }
                Some(Err(_)) => { c.close(now_us()); }
                None => {}
            },
            _ = tokio::time::sleep(Duration::from_micros(wait)) => {}
        }
        c.handle_timeout(now_us());
        while let Some(ev) = c.poll_event() {
            match ev {
                Event::Connected(_) => {
                    let mut s = hub.subscribe();
                    for e in s.initial() {
                        for chunk in ts.push(&e) {
                            let _ = c.send(&chunk, now_us());
                        }
                    }
                    sub = Some(s);
                    st.lock().unwrap().connects += 1;
                    st.lock().unwrap().state = "active".into();
                }
                Event::Closed { reason } => return Err(format!("SRT が閉じました: {reason:?}")),
                Event::Data { .. } => {}
            }
        }
        if !hub.is_live() && sub.is_some() {
            c.close(now_us());
            return Ok(());
        }
    }
}
