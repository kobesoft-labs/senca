//! WebRTC（M8、仕様 6.3、ADR-0012）。WHIP（RFC 9725）で受け、WHEP で配る。
//!
//! - 1 つの UDP ソケット（`--webrtc-udp`）で全セッションを受け、STUN の USERNAME とアドレスで振り分ける。
//! - 駆動役は 1 つのタスクで、ピア接続（sans-io の senca-webrtc）をすべて持つ。hub とのやり取りはセッションごとの
//!   中継タスクがキュー越しに行う（駆動役は await で止まらない）。
//! - WHIP: `POST /whip/{stream_id}`（`Authorization: Bearer {stream_key}`、`application/sdp`）→ 201 と answer。
//!   `PATCH`（trickle・ICE restart）、`DELETE`。受けた AU は stream の送り手として hub へ。
//! - WHEP: `POST /whep/{stream_id}?exp&sig`（署名付きの再生 URL と同じ署名）→ 201 と answer。hub の AU を送る。
//!   AAC は WebRTC で運べないので音声は出さない（変換計画でも理由付きで拒否。ADR-0004）。

use crate::app::App;
use crate::hub::HubEvent;
use crate::http::Reply;
use bytes::Bytes;
use senca_core::{AccessUnit, Codec, CodecConfig, MediaKind, TrackId};
use senca_webrtc::{DtlsCertificate, Event, PeerConfig, PeerConnection};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

/// WHIP の答え: (セッション ID, answer の SDP, 受信の流れ)
type WhipReply = oneshot::Sender<Result<(u64, String, mpsc::Receiver<Up>), String>>;
/// WHEP の答え: (セッション ID, answer の SDP)
type WhepReply = oneshot::Sender<Result<(u64, String), String>>;

enum Cmd {
    Whip { offer: String, reply: WhipReply },
    Whep { offer: String, kinds: Vec<(MediaKind, Codec)>, reply: WhepReply },
    Patch { id: u64, body: String, reply: oneshot::Sender<Result<Option<String>, String>> },
    Delete { id: u64 },
}

/// 受信（WHIP）→ hub
pub enum Up {
    Track(TrackId, MediaKind),
    Au(AccessUnit, Option<CodecConfig>),
    Closed,
}

/// hub → 送信（WHEP）
pub enum Down {
    Au(AccessUnit, Option<CodecConfig>),
}

struct Session {
    pc: PeerConnection,
    remote: Option<SocketAddr>,
    up: Option<mpsc::Sender<Up>>,
    /// WHEP: 種類 → 送信トラック
    send: HashMap<MediaKind, TrackId>,
    /// WHEP: 送るトラックの最新の設定（H.264 のキーフレームに SPS/PPS を付けるため毎回渡す）
    send_cfg: HashMap<MediaKind, CodecConfig>,
    /// WHEP: つながる前に届いた AU（直近の GOP。つながったら送る）
    pending: Vec<AccessUnit>,
    /// WHIP: 映像の受信トラックと、最後にキーフレームを受けた時刻（HLS を区切るため定期的に求める）
    last_key: HashMap<TrackId, Instant>,
    connected: bool,
    closed_at: Option<Instant>,
}

/// WHIP の送り手にキーフレームを求める間隔（HLS のセグメントの目標長に合わせる）。
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);

pub struct Rtc {
    tx: mpsc::Sender<Cmd>,
    down_tx: mpsc::Sender<(u64, Down)>,
}

pub async fn start(app: Arc<App>, udp: SocketAddr) -> Result<Arc<Rtc>, String> {
    let sock = UdpSocket::bind(udp).await.map_err(|e| format!("webrtc udp {udp}: {e}"))?;
    let cert = DtlsCertificate::generate_self_signed("senca").map_err(|e| format!("{e:?}"))?;
    let (tx, rx) = mpsc::channel(256);
    let (down_tx, down_rx) = mpsc::channel(8192);
    // 公開する候補のアドレス（0.0.0.0 で待つときは、ノードの公開 URL のホストに置き換えるべきだが、ここでは待ち受けのアドレス）
    tokio::spawn(drive(app.clone(), sock, cert, rx, down_rx, udp));
    app.log(&format!("webrtc listening on udp {udp}"));
    Ok(Arc::new(Rtc { tx, down_tx }))
}

/// セッション ID（推測できない 64 bit。DELETE・PATCH はこの ID を知っている者だけができる）。
fn session_id(fallback: &mut u64) -> u64 {
    let mut b = [0u8; 8];
    match getrandom::fill(&mut b) {
        Ok(()) => u64::from_le_bytes(b) >> 1 | 1,
        Err(_) => {
            *fallback += 1;
            *fallback
        }
    }
}

fn epoch_ns() -> Option<i64> {
    SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_nanos() as i64)
}

async fn drive(app: Arc<App>, sock: UdpSocket, cert: DtlsCertificate, mut rx: mpsc::Receiver<Cmd>, mut down_rx: mpsc::Receiver<(u64, Down)>, local: SocketAddr) {
    let mut sessions: HashMap<u64, Session> = HashMap::new();
    let mut next_id = 1u64;
    let mut buf = vec![0u8; 65536];
    loop {
        let now = Instant::now();
        let wait = sessions.values().filter_map(|s| s.pc.poll_timeout()).map(|t| t.saturating_duration_since(now)).min().unwrap_or(Duration::from_millis(50)).clamp(Duration::from_micros(200), Duration::from_millis(50));
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                if let Ok((n, from)) = r {
                    let data = &buf[..n];
                    let id = sessions.iter().find(|(_, s)| s.remote == Some(from)).map(|(k, _)| *k).or_else(|| {
                        let u = senca_webrtc::stun_username_ufrag(data)?;
                        sessions.iter().find(|(_, s)| s.pc.local_ufrag() == u).map(|(k, _)| *k)
                    });
                    if let Some(s) = id.and_then(|i| sessions.get_mut(&i)) {
                        s.pc.handle_datagram(Instant::now(), from, data);
                    }
                }
            }
            c = rx.recv() => {
                let Some(c) = c else { return };
                match c {
                    Cmd::Whip { offer, reply } => {
                        let r = new_pc(&cert, local, now).and_then(|mut pc| {
                            let ans = pc.accept_offer(now, &offer).map_err(|e| format!("{e:?}"))?;
                            Ok((pc, ans))
                        });
                        match r {
                            Ok((pc, ans)) => {
                                let (utx, urx) = mpsc::channel(4096);
                                let id = session_id(&mut next_id);
                                sessions.insert(id, Session { pc, remote: None, up: Some(utx), send: HashMap::new(), send_cfg: HashMap::new(), pending: Vec::new(), last_key: HashMap::new(), connected: false, closed_at: None });
                                let _ = reply.send(Ok((id, ans, urx)));
                            }
                            Err(e) => { let _ = reply.send(Err(e)); }
                        }
                    }
                    Cmd::Whep { offer, kinds, reply } => {
                        let r = new_pc_for(&cert, local, now, &kinds).and_then(|mut pc| {
                            let ans = pc.accept_offer(now, &offer).map_err(|e| format!("{e:?}"))?;
                            Ok((pc, ans))
                        });
                        match r {
                            Ok((pc, ans)) => {
                                let id = session_id(&mut next_id);
                                let mut send = HashMap::new();
                                for t in pc.send_tracks() {
                                    send.entry(t.kind).or_insert(t.track);
                                }
                                sessions.insert(id, Session { pc, remote: None, up: None, send, send_cfg: HashMap::new(), pending: Vec::new(), last_key: HashMap::new(), connected: false, closed_at: None });
                                let _ = reply.send(Ok((id, ans)));
                            }
                            Err(e) => { let _ = reply.send(Err(e)); }
                        }
                    }
                    Cmd::Patch { id, body, reply } => {
                        let r = match sessions.get_mut(&id) {
                            Some(s) => s.pc.handle_trickle_patch(now, &body).map_err(|e| format!("{e:?}")),
                            None => Err("not found".into()),
                        };
                        let _ = reply.send(r);
                    }
                    Cmd::Delete { id } => {
                        if let Some(s) = sessions.get_mut(&id) {
                            s.pc.close(now);
                            s.closed_at = Some(now);
                        }
                    }
                }
            }
            d = down_rx.recv() => {
                if let Some((id, Down::Au(au, cfg))) = d
                    && let Some(s) = sessions.get_mut(&id)
                {
                    let k = au.codec.kind();
                    if let Some(c) = cfg {
                        s.send_cfg.insert(k, c);
                    }
                    if !s.connected {
                        // つながるまで直近の GOP をためる（映像のキーフレームで捨て直す）
                        if k == MediaKind::Video && au.keyframe {
                            s.pending.retain(|a| a.codec.kind() != MediaKind::Video);
                        }
                        if s.pending.len() < 2000 {
                            s.pending.push(au);
                        }
                    } else if let Some(t) = s.send.get(&k).copied() {
                        let _ = s.pc.send_access_unit(Instant::now(), t, &au, s.send_cfg.get(&k));
                    }
                }
            }
            _ = tokio::time::sleep(wait) => {}
        }
        let now = Instant::now();
        let mut dead = Vec::new();
        for (id, s) in sessions.iter_mut() {
            if s.pc.poll_timeout().is_some_and(|t| t <= now) {
                s.pc.handle_timeout(now);
            }
            while let Some(t) = s.pc.poll_transmit() {
                let _ = sock.send_to(&t.data, t.remote).await;
            }
            if let Some((_, r)) = s.pc.selected_path() {
                s.remote = Some(r);
            }
            while let Some(e) = s.pc.poll_event() {
                match e {
                    Event::Connected => {
                        s.connected = true;
                        for au in std::mem::take(&mut s.pending) {
                            let k = au.codec.kind();
                            if let Some(t) = s.send.get(&k).copied() {
                                let _ = s.pc.send_access_unit(now, t, &au, s.send_cfg.get(&k));
                            }
                        }
                    }
                    Event::KeyframeRequest { .. } => {}
                    Event::Track(ti) => {
                        if let Some(u) = &s.up {
                            let _ = u.try_send(Up::Track(ti.track, ti.kind));
                        }
                    }
                    Event::AccessUnit { track, au, config } => {
                        // 欠落の後は送り手にキーフレームを求める（PLI。senca-rtp の制限で間隔はまとめられる）
                        if au.codec.kind() == MediaKind::Video {
                            if au.keyframe {
                                s.last_key.insert(track, now);
                            } else {
                                s.last_key.entry(track).or_insert(now);
                            }
                            if au.discontinuity {
                                let _ = s.pc.request_keyframe(now, track);
                            }
                        }
                        if let Some(u) = &s.up {
                            let _ = u.try_send(Up::Au(au, config));
                        }
                    }
                    Event::Disconnected { reason } => {
                        app.log(&format!("webrtc session {id}: disconnected ({reason:?})"));
                        s.closed_at.get_or_insert(now);
                    }
                    _ => {}
                }
            }
            // HLS のセグメント（既定 2 秒）を区切れるよう、送り手にキーフレームを求める
            if s.up.is_some() {
                let due: Vec<TrackId> = s.last_key.iter().filter(|(_, t)| now.duration_since(**t) >= KEYFRAME_INTERVAL).map(|(k, _)| *k).collect();
                for k in due {
                    let _ = s.pc.request_keyframe(now, k);
                    s.last_key.insert(k, now);
                }
            }
            if s.closed_at.is_some_and(|t| now.duration_since(t) > Duration::from_secs(2)) {
                dead.push(*id);
            }
        }
        for id in dead {
            if let Some(s) = sessions.remove(&id)
                && let Some(u) = s.up
            {
                let _ = u.try_send(Up::Closed);
            }
        }
    }
}

fn new_pc(cert: &DtlsCertificate, local: SocketAddr, now: Instant) -> Result<PeerConnection, String> {
    let cfg = PeerConfig { certificate: Some(cert.clone()), unix_time_ns_at_start: epoch_ns(), ..Default::default() };
    let mut pc = PeerConnection::new(cfg, now).map_err(|e| format!("{e:?}"))?;
    pc.add_host_address(now, local).map_err(|e| format!("{e:?}"))?;
    Ok(pc)
}

/// WHEP: 配る stream のコーデックだけを申し出る。
fn new_pc_for(cert: &DtlsCertificate, local: SocketAddr, now: Instant, kinds: &[(MediaKind, Codec)]) -> Result<PeerConnection, String> {
    let mut cfg = PeerConfig { certificate: Some(cert.clone()), unix_time_ns_at_start: epoch_ns(), ..Default::default() };
    let want: Vec<String> = kinds.iter().map(|(_, c)| c.as_str().to_string()).collect();
    cfg.capabilities.codecs.retain(|c| {
        let e = c.encoding.to_ascii_lowercase();
        let e = if e == "h264" { "h264".to_string() } else { e };
        want.contains(&e) || e == "rtx"
    });
    let mut pc = PeerConnection::new(cfg, now).map_err(|e| format!("{e:?}"))?;
    pc.add_host_address(now, local).map_err(|e| format!("{e:?}"))?;
    Ok(pc)
}

fn sdp_reply(status: u16, sdp: String, location: String) -> Reply {
    Reply { status, headers: vec![("Content-Type".into(), "application/sdp".into()), ("Location".into(), location), ("Access-Control-Allow-Origin".into(), "*".into()), ("Access-Control-Expose-Headers".into(), "Location, ETag".into())], body: Bytes::from(sdp) }
}

fn plain(status: u16, msg: &str) -> Reply {
    Reply { status, headers: vec![("Content-Type".into(), "text/plain; charset=utf-8".into()), ("Access-Control-Allow-Origin".into(), "*".into())], body: Bytes::from(msg.to_string()) }
}

/// `/whip/...`・`/whep/...` の HTTP。
pub async fn http(app: &Arc<App>, rtc: &Rtc, h: &senca_http::RequestHead, body: Bytes, peer: &str) -> Reply {
    let path = h.path();
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let method = h.method.as_str();
    if method == "OPTIONS" {
        return Reply { status: 204, headers: vec![
            ("Access-Control-Allow-Origin".into(), "*".into()),
            ("Access-Control-Allow-Methods".into(), "POST, PATCH, DELETE, OPTIONS".into()),
            ("Access-Control-Allow-Headers".into(), "Authorization, Content-Type, If-Match".into()),
            ("Access-Control-Expose-Headers".into(), "Location, ETag".into()),
        ], body: Bytes::new() };
    }
    let ct = h.headers.get_str("content-type").unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match (method, segs.as_slice()) {
        ("POST", ["whip", stream]) => {
            if ct != "application/sdp" {
                return plain(415, "Content-Type は application/sdp");
            }
            let key = senca_http::auth::bearer_token(&h.headers).ok().flatten().unwrap_or("").to_string();
            // 鍵が stream のものか（publish 鍵で受ける）
            let ok = {
                let core = app.control.lock().unwrap();
                core.state.streams.get(*stream).is_some_and(|s| senca_control::secret::verify(&key, &s.key_hash))
            };
            if !ok {
                return plain(401, "unauthorized");
            }
            let offer = String::from_utf8_lossy(&body).to_string();
            let (reply, rx) = oneshot::channel();
            if rtc.tx.send(Cmd::Whip { offer, reply }).await.is_err() {
                return plain(503, "webrtc unavailable");
            }
            match rx.await {
                Ok(Ok((id, ans, urx))) => {
                    let app2 = app.clone();
                    let peer = peer.to_string();
                    tokio::spawn(async move { forward_up(app2, key, peer, urx).await });
                    sdp_reply(201, ans, format!("/whip/{stream}/{id}"))
                }
                Ok(Err(e)) => plain(400, &e),
                Err(_) => plain(503, "webrtc unavailable"),
            }
        }
        ("POST", ["whep", stream]) => {
            if ct != "application/sdp" {
                return plain(415, "Content-Type は application/sdp");
            }
            let q = h.query().unwrap_or("");
            let Some(mut sub) = app.subscribe_rtmp_play(&format!("{stream}?{q}")) else { return plain(403, "forbidden") };
            let init = sub.initial();
            let kinds: Vec<(MediaKind, Codec)> = init
                .iter()
                .filter_map(|e| if let HubEvent::Config(_, c) = e { Some((c.codec.kind(), c.codec)) } else { None })
                .filter(|(_, c)| matches!(c, Codec::H264 | Codec::Vp8 | Codec::Vp9 | Codec::Av1 | Codec::Opus | Codec::Pcma | Codec::Pcmu))
                .collect();
            if kinds.is_empty() {
                return plain(422, "この stream のコーデックは WebRTC で運べません");
            }
            let offer = String::from_utf8_lossy(&body).to_string();
            let (reply, rx) = oneshot::channel();
            if rtc.tx.send(Cmd::Whep { offer, kinds, reply }).await.is_err() {
                return plain(503, "webrtc unavailable");
            }
            match rx.await {
                Ok(Ok((id, ans))) => {
                    let dtx = rtc.down_tx.clone();
                    tokio::spawn(async move { forward_down(sub, init, id, dtx).await });
                    sdp_reply(201, ans, format!("/whep/{stream}/{id}"))
                }
                Ok(Err(e)) => plain(400, &e),
                Err(_) => plain(503, "webrtc unavailable"),
            }
        }
        ("PATCH", [_, _, id]) => {
            if ct != "application/trickle-ice-sdpfrag" {
                return plain(415, "Content-Type は application/trickle-ice-sdpfrag");
            }
            let Ok(id) = id.parse::<u64>() else { return plain(404, "not found") };
            let (reply, rx) = oneshot::channel();
            let _ = rtc.tx.send(Cmd::Patch { id, body: String::from_utf8_lossy(&body).to_string(), reply }).await;
            match rx.await {
                Ok(Ok(None)) => plain(204, ""),
                Ok(Ok(Some(frag))) => Reply { status: 200, headers: vec![("Content-Type".into(), "application/trickle-ice-sdpfrag".into())], body: Bytes::from(frag) },
                Ok(Err(e)) if e == "not found" => plain(404, "not found"),
                Ok(Err(e)) => plain(409, &e),
                Err(_) => plain(503, "webrtc unavailable"),
            }
        }
        ("DELETE", [_, _, id]) => {
            let Ok(id) = id.parse::<u64>() else { return plain(404, "not found") };
            let _ = rtc.tx.send(Cmd::Delete { id }).await;
            plain(200, "")
        }
        _ => plain(405, "method not allowed"),
    }
}

/// WHIP の受信を hub の送り手として流す。
async fn forward_up(app: Arc<App>, key: String, peer: String, mut rx: mpsc::Receiver<Up>) {
    let p = match app.attach_publisher(&key, "webrtc", &peer).await {
        Ok(p) => p,
        Err(e) => {
            app.log(&format!("whip publish rejected: {e:?}"));
            return;
        }
    };
    // トラック番号は種類で決める（映像 0・音声 1。サイマルキャストは最初の層だけ）
    let mut map: HashMap<TrackId, TrackId> = HashMap::new();
    let mut gens: HashMap<TrackId, u32> = HashMap::new();
    // 時刻を付け直す: RTCP SR で壁時計に合わせ直されると時刻が飛ぶので、送り手の時間軸（µs）で滑らかにつなぐ。
    // 飛び（1 秒超の前進・後退）は、直前の時刻＋直前の間隔へ寄せる。
    let mut tl: HashMap<TrackId, (i64, i64, i64)> = HashMap::new(); // track → (offset_us, last_us, last_step_us)
    // 映像と音声の相対の位置を保つため、最初の基準は共通にする
    let mut base: Option<i64> = None;
    while let Some(m) = rx.recv().await {
        match m {
            Up::Track(t, k) => {
                let ours = if k == MediaKind::Video { TrackId(0) } else { TrackId(1) };
                if !map.values().any(|v| *v == ours) {
                    map.insert(t, ours);
                }
            }
            Up::Au(mut au, cfg) => {
                let Some(&ours) = map.get(&au.track) else { continue };
                au.track = ours;
                let need_cfg = cfg.is_some() || !gens.contains_key(&ours);
                if need_cfg {
                    let mut c = cfg.unwrap_or_else(|| CodecConfig { codec: au.codec, generation: 1, record: Bytes::new(), video: None, audio: None });
                    crate::media::enrich(&mut c);
                    let g = gens.get(&ours).map(|g| g + 1).unwrap_or(1);
                    c.generation = g;
                    gens.insert(ours, g);
                    if p.config(ours, c).await.is_err() {
                        break;
                    }
                }
                au.config_generation = gens[&ours];
                {
                    use senca_core::{Rescale, Timebase};
                    let raw = au.dts.rescale(au.timebase, Timebase::MICROS).unwrap_or(0);
                    let cts = au.pts.rescale(au.timebase, Timebase::MICROS).unwrap_or(raw) - raw;
                    let b = *base.get_or_insert(raw);
                    let e = tl.entry(ours).or_insert((-b, -1, 33_000));
                    let mut v = raw + e.0;
                    if e.1 >= 0 && (v - e.1 > 1_000_000 || v <= e.1 - 200_000) {
                        let target = e.1 + e.2.clamp(1_000, 100_000);
                        e.0 += target - v;
                        v = target;
                    }
                    if e.1 >= 0 && v > e.1 {
                        e.2 = v - e.1;
                    }
                    e.1 = e.1.max(v);
                    au.timebase = Timebase::MICROS;
                    au.dts = v;
                    au.pts = v + cts;
                    au.discontinuity = false;
                }
                if p.access_unit(au).await.is_err() {
                    break;
                }
            }
            Up::Closed => break,
        }
    }
    p.detach("whip closed").await;
}

/// hub の AU を WHEP のセッションへ。
async fn forward_down(mut sub: crate::hub::Subscription, init: Vec<HubEvent>, id: u64, tx: mpsc::Sender<(u64, Down)>) {
    let mut cfgs: HashMap<TrackId, CodecConfig> = HashMap::new();
    let mut sent_cfg: HashMap<TrackId, u32> = HashMap::new();
    let mut handle = |e: HubEvent, cfgs: &mut HashMap<TrackId, CodecConfig>| -> Option<(u64, Down)> {
        match e {
            HubEvent::Config(t, c) => {
                cfgs.insert(t, c);
                None
            }
            HubEvent::Au(a) => {
                let c = cfgs.get(&a.track)?;
                let cfg = (sent_cfg.get(&a.track) != Some(&c.generation)).then(|| c.clone());
                if cfg.is_some() {
                    sent_cfg.insert(a.track, c.generation);
                }
                Some((id, Down::Au(a, cfg)))
            }
            _ => None,
        }
    };
    for e in init {
        if let Some(m) = handle(e, &mut cfgs)
            && tx.send(m).await.is_err()
        {
            return;
        }
    }
    loop {
        match sub.recv().await {
            Ok(HubEvent::End) | Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            Ok(e) => {
                if let Some(m) = handle(e, &mut cfgs)
                    && tx.send(m).await.is_err()
                {
                    return;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => sub.lagged(n),
        }
    }
}
