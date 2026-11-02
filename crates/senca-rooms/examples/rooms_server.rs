//! 部屋の試験用サーバー（std のスレッドだけ）。ブラウザーが WebSocket のシグナリング v1 で部屋に入る。
//!
//! `rooms_server --http 127.0.0.1:50080 --udp 192.0.2.1:50100 [--spotlight N]`
//!
//! - `GET /`（`?room=demo&name=alice&simulcast=1&peers=2&secs=40`）は試験ページ（3 枚開いて互いの映像を復号し、
//!   DataChannel `#chat` で ping/pong を往復させる）
//! - `GET /token?room=..&name=..` は参加トークン（この起動で作った Ed25519 の鍵で署名、試験用）
//! - `GET /signaling`（Upgrade: websocket、subprotocol `senca.rooms.v1`）はシグナリング
//! - `GET /status` は部屋の統計（REST の `GET /rooms/{id}/stats` と同じ形）
//!
//! WebSocket は senca-ws、HTTP の head は senca-http、ピア接続は senca-rooms の PeerHost（1 つの UDP ソケット）。

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use senca_rooms::host::PeerHost;
use senca_rooms::model::{RoomSettings, SpotlightSettings, Unfocused};
use senca_rooms::service::{Action, ConnKey, ConnMeta, RoomService, ServiceConfig};
use senca_rooms::signaling::SUBPROTOCOL;
use senca_rooms::token::{Audience, JoinClaims, TokenSigner};
use senca_rooms::Role;
use senca_webrtc::{DtlsCertificate, PeerConfig};
use senca_ws::{AcceptOptions, Connection, Limits, Message, Role as WsRole};

enum Msg {
    Open { reply: mpsc::Sender<ConnKey>, out: mpsc::Sender<WsOut>, meta: ConnMeta },
    Text(ConnKey, String),
    Closed(ConnKey),
    Status(String, mpsc::Sender<String>),
}

enum WsOut {
    Text(String),
    Close(u16, String),
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let http = arg(&args, "--http").unwrap_or_else(|| "127.0.0.1:50080".into());
    let udp: SocketAddr = arg(&args, "--udp").unwrap_or_else(|| "127.0.0.1:50100".into()).parse()?;
    let spotlight: Option<u8> = arg(&args, "--spotlight").and_then(|s| s.parse().ok());
    let signer = TokenSigner::generate_ed25519("demo")?;
    let mut settings = RoomSettings::default();
    if let Some(n) = spotlight {
        settings.spotlight = Some(SpotlightSettings { number: n, unfocused: Unfocused::Lowest, focus_rid: None });
    }
    let now = Instant::now();
    let mut svc = RoomService::new(
        ServiceConfig {
            node_id: "rooms-example".into(),
            token_verifier: Some(signer.verifier("senca", "senca-rooms")),
            default_settings: settings,
            ..Default::default()
        },
        now,
        unix_ms(),
    );
    let sock = UdpSocket::bind(udp)?;
    let base = PeerConfig {
        certificate: Some(DtlsCertificate::generate_self_signed("senca-rooms")?),
        unix_time_ns_at_start: Some(unix_ms() * 1_000_000),
        ..Default::default()
    };
    let mut host = PeerHost::new(base, udp);
    let (tx, rx) = mpsc::channel::<Msg>();
    let listener = TcpListener::bind(&http)?;
    let signer2 = signer.clone();
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            let tx = tx.clone();
            let signer = signer2.clone();
            std::thread::spawn(move || {
                let _ = handle_tcp(s, tx, signer);
            });
        }
    });
    eprintln!("rooms_server http={http} udp={udp}");
    let mut outs: HashMap<ConnKey, mpsc::Sender<WsOut>> = HashMap::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let now = Instant::now();
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Open { reply, out, meta } => {
                    let k = svc.open(now, meta);
                    outs.insert(k, out);
                    let _ = reply.send(k);
                }
                Msg::Text(k, t) => svc.on_signaling_text(now, k, &t),
                Msg::Closed(k) => {
                    outs.remove(&k);
                    svc.on_signaling_closed(now, k);
                }
                Msg::Status(room, reply) => {
                    let body = match svc.api_stats(now, &room) {
                        Ok(s) => serde_json::to_string(&s).unwrap_or_default(),
                        Err(e) => e.to_problem().to_string(),
                    };
                    let _ = reply.send(body);
                }
            }
        }
        let mut wait = Duration::from_millis(5);
        for t in [host.poll_timeout(), svc.poll_timeout()].into_iter().flatten() {
            wait = wait.min(t.saturating_duration_since(now));
        }
        sock.set_read_timeout(Some(wait.max(Duration::from_micros(200))))?;
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            host.handle_datagram(Instant::now(), from, &buf[..n]);
            sock.set_nonblocking(true)?;
            for _ in 0..256 {
                match sock.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        host.handle_datagram(Instant::now(), from, &buf[..n]);
                    }
                    Err(_) => break,
                }
            }
            sock.set_nonblocking(false)?;
        }
        let now = Instant::now();
        host.handle_timeout(now);
        if svc.poll_timeout().is_some_and(|t| t <= now) {
            svc.handle_timeout(now);
        }
        let mut acts = Vec::new();
        host.pump(now, &mut svc, |a| acts.push(a));
        for a in acts {
            match a {
                Action::Signal { conn, text, .. } => {
                    if let Some(o) = outs.get(&conn) {
                        let _ = o.send(WsOut::Text(text));
                    }
                }
                Action::CloseSignaling { conn, code, reason } => {
                    if let Some(o) = outs.remove(&conn) {
                        let _ = o.send(WsOut::Close(code, reason));
                    }
                }
                Action::Event(e) => eprintln!("event {} {}", e.event.type_name(), serde_json::to_string(&e.event).unwrap_or_default()),
                _ => {}
            }
        }
        while let Some(t) = host.poll_transmit() {
            let _ = sock.send_to(&t.data, t.remote);
        }
    }
}

fn respond(s: &mut TcpStream, status: u16, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    write!(
        s,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    s.write_all(body)
}

fn query(target: &str) -> HashMap<String, String> {
    target
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn handle_tcp(mut s: TcpStream, tx: mpsc::Sender<Msg>, signer: TokenSigner) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let end = loop {
        let n = s.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        if buf.len() > 32 * 1024 {
            return respond(&mut s, 400, "text/plain", b"too large");
        }
    };
    let mut p = senca_http::RequestParser::new(senca_http::Limits::DEFAULT);
    let head = match p.feed(&buf[..end]).and_then(|_| p.poll()) {
        Ok(senca_http::Event::Head(h)) => h,
        _ => return respond(&mut s, 400, "text/plain", b"bad request"),
    };
    let leftover = buf[end..].to_vec();
    let q = query(&head.target);
    match head.path() {
        "/" => respond(&mut s, 200, "text/html; charset=utf-8", PAGE.as_bytes()),
        "/token" => {
            let room = q.get("room").cloned().unwrap_or_else(|| "demo".into());
            let name = q.get("name").cloned().unwrap_or_else(|| "anon".into());
            let iat = unix_ms() / 1000;
            let claims = JoinClaims {
                iss: "senca".into(),
                aud: Audience::One("senca-rooms".into()),
                exp: iat + 600,
                nbf: None,
                iat: Some(iat),
                jti: None,
                sub: None,
                room,
                role: Role::Sendrecv,
                client_id: Some(name.clone()),
                metadata: Some(serde_json::json!({ "name": name })),
                permissions: None,
            };
            let t = signer.sign(&claims).map_err(|e| std::io::Error::other(e.to_string()))?;
            respond(&mut s, 200, "application/json", serde_json::json!({ "token": t }).to_string().as_bytes())
        }
        "/status" => {
            let (rtx, rrx) = mpsc::channel();
            let room = q.get("room").cloned().unwrap_or_else(|| "demo".into());
            let _ = tx.send(Msg::Status(room, rtx));
            let body = rrx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
            respond(&mut s, 200, "application/json", body.as_bytes())
        }
        "/signaling" => {
            let opts = AcceptOptions {
                subprotocols: vec![SUBPROTOCOL.into()],
                require_subprotocol: true,
                ..Default::default()
            };
            let acc = match senca_ws::handshake::accept(&head, &opts) {
                Ok(a) => a,
                Err(e) => return s.write_all(&e.response_bytes()),
            };
            s.write_all(&acc.response_bytes())?;
            websocket(s, tx, leftover, acc.origin)
        }
        _ => respond(&mut s, 404, "text/plain", b"not found"),
    }
}

fn websocket(mut s: TcpStream, tx: mpsc::Sender<Msg>, leftover: Vec<u8>, origin: Option<String>) -> std::io::Result<()> {
    let (otx, orx) = mpsc::channel();
    let (ktx, krx) = mpsc::channel();
    let meta = ConnMeta {
        remote_addr: s.peer_addr().ok().map(|a| a.to_string()),
        user_agent: None,
        origin,
    };
    tx.send(Msg::Open { reply: ktx, out: otx, meta }).map_err(|_| std::io::Error::other("closed"))?;
    let key = krx.recv().map_err(|_| std::io::Error::other("closed"))?;
    let mut ws = Connection::new(WsRole::Server, Limits { max_message_size: 256 * 1024, max_frame_size: 256 * 1024, ..Default::default() });
    ws.feed(&leftover);
    s.set_read_timeout(Some(Duration::from_millis(10)))?;
    let mut tmp = vec![0u8; 16384];
    loop {
        match s.read(&mut tmp) {
            Ok(0) => {
                ws.feed_eof();
            }
            Ok(n) => ws.feed(&tmp[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => ws.feed_eof(),
        }
        loop {
            match ws.poll() {
                Ok(Some(Message::Text(t))) => {
                    let _ = tx.send(Msg::Text(key, t));
                }
                Ok(Some(Message::Close(_))) | Err(_) => break,
                Ok(Some(_)) => {}
                Ok(None) => break,
            }
        }
        while let Ok(o) = orx.try_recv() {
            match o {
                WsOut::Text(t) => {
                    let _ = ws.send_text(&t);
                }
                WsOut::Close(code, reason) => {
                    let _ = ws.close(Some(code), &reason);
                }
            }
        }
        while let Some(b) = ws.poll_transmit() {
            if s.write_all(&b).is_err() {
                ws.feed_eof();
                break;
            }
        }
        if ws.is_closed() {
            let _ = tx.send(Msg::Closed(key));
            // 残りの送りを出してから閉じる
            while let Ok(WsOut::Text(t)) = orx.try_recv() {
                let _ = t;
            }
            let _ = s.shutdown(std::net::Shutdown::Both);
            return Ok(());
        }
    }
}

const PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>senca rooms</title></head>
<body><pre id="log"></pre><div id="videos"></div>
<script>
const q = new URLSearchParams(location.search);
const room = q.get('room') || 'demo';
const name = q.get('name') || 'anon';
const simulcast = q.get('simulcast') === '1';
const peers = Number(q.get('peers') || '2');
const secs = Number(q.get('secs') || '45');
const switchLayers = q.get('switch') === '1';
const R = { ok: false, name, decoded: {}, widths: {}, pongsFrom: [], pingsFrom: [], renegotiations: 0, layerChanges: [], errors: [], events: [] };
const log = (...a) => { const s = a.map(x => typeof x === 'string' ? x : JSON.stringify(x)).join(' '); document.getElementById('log').textContent += s + '\n'; console.log(name + ': ' + s); };
let ws, pc, chat, connId, offering = false, pendingReneg = null;
const slots = {};
const recv = { audio: 0, video: 0 };
const send = (m) => ws.send(JSON.stringify(m));
const gathered = () => new Promise((res) => {
  if (pc.iceGatheringState === 'complete') return res();
  const t = setTimeout(res, 2000);
  pc.addEventListener('icegatheringstatechange', () => { if (pc.iceGatheringState === 'complete') { clearTimeout(t); res(); } });
});
async function offer(want) {
  for (const kind of ['audio', 'video']) {
    while (want && recv[kind] < want[kind]) {
      const tr = pc.addTransceiver(kind, { direction: 'recvonly' });
      if (kind === 'video') {
        const vp8 = RTCRtpReceiver.getCapabilities('video').codecs.filter(c => /vp8|rtx/i.test(c.mimeType));
        try { tr.setCodecPreferences(vp8); } catch (e) {}
      }
      recv[kind]++;
    }
  }
  offering = true;
  await pc.setLocalDescription(await pc.createOffer());
  await gathered();
  send({ type: 'offer', sdp: pc.localDescription.sdp });
}
async function handle(m) {
  switch (m.type) {
    case 'connected':
      connId = m.connection_id; log('connected', connId, 'slots', m.recv_slots);
      await offer(m.recv_slots); break;
    case 'answer':
      await pc.setRemoteDescription({ type: 'answer', sdp: m.sdp });
      for (const s of m.slots) slots[s.mid] = s;
      offering = false;
      if (pendingReneg) { const w = pendingReneg; pendingReneg = null; R.renegotiations++; await offer(w); }
      break;
    case 'slots': for (const s of m.slots) slots[s.mid] = s; break;
    case 'renegotiate':
      if (offering) pendingReneg = m; else { R.renegotiations++; await offer(m); }
      break;
    case 'ping': send({ type: 'pong' }); break;
    case 'notify':
      R.events.push(m.event);
      if (m.event === 'layer.changed') R.layerChanges.push(m.rid);
      break;
    case 'error': R.errors.push(m); log('error', m); break;
  }
}
async function stats() {
  const out = {};
  const st = await pc.getStats();
  st.forEach((r) => {
    if (r.type === 'inbound-rtp' && r.kind === 'video') {
      const s = slots[r.mid];
      if (s && s.client_id) out[s.client_id] = { decoded: r.framesDecoded || 0, width: r.frameWidth || 0, mid: r.mid, track: s.track_id };
    }
  });
  return out;
}
const sleep = (ms) => new Promise(r => setTimeout(r, ms));
async function main() {
  const tok = await (await fetch(`/token?room=${room}&name=${name}`)).json();
  // Chrome は解像度で simulcast の層の数を絞る（640x480 では 2 層）。3 層には 1280x720 が要る
  const size = simulcast ? { width: 1280, height: 720 } : { width: 640, height: 480 };
  const stream = await navigator.mediaDevices.getUserMedia({ video: { ...size, frameRate: 30 }, audio: true });
  pc = new RTCPeerConnection({ bundlePolicy: 'max-bundle' });
  chat = pc.createDataChannel('#chat');
  chat.onmessage = (e) => {
    const [kind, from, to] = String(e.data).split(':');
    if (kind === 'ping' && from !== name) { if (!R.pingsFrom.includes(from)) R.pingsFrom.push(from); chat.send(`pong:${from}:${name}`); }
    if (kind === 'pong' && from === name && !R.pongsFrom.includes(to)) R.pongsFrom.push(to);
  };
  pc.ontrack = (e) => {
    if (e.track.kind !== 'video') return;
    const v = document.createElement('video'); v.autoplay = true; v.muted = true; v.playsInline = true;
    v.srcObject = new MediaStream([e.track]); document.getElementById('videos').appendChild(v);
  };
  const vt = stream.getVideoTracks()[0];
  const enc = simulcast ? [{ rid: 'r0', scaleResolutionDownBy: 4, maxBitrate: 150000 }, { rid: 'r1', scaleResolutionDownBy: 2, maxBitrate: 500000 }, { rid: 'r2', maxBitrate: 1500000 }] : undefined;
  const vtr = pc.addTransceiver(vt, { direction: 'sendonly', sendEncodings: enc });
  const vp8 = RTCRtpSender.getCapabilities('video').codecs.filter(c => /vp8|rtx/i.test(c.mimeType));
  try { vtr.setCodecPreferences(vp8); } catch (e) {}
  pc.addTransceiver(stream.getAudioTracks()[0], { direction: 'sendonly' });
  ws = new WebSocket(`ws://${location.host}/signaling`, 'senca.rooms.v1');
  let chain = Promise.resolve();
  ws.onmessage = (e) => { const m = JSON.parse(e.data); chain = chain.then(() => handle(m)).catch((err) => { R.errors.push(String(err)); log('handler', String(err)); }); };
  ws.onclose = (e) => log('ws closed', e.code, e.reason);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  const c = { type: 'connect', version: 1, room_id: room, role: 'sendrecv', token: tok.token, client_id: name, data_channels: [{ label: '#chat' }], video: { codec: 'vp8' }, sdk: { name: 'interop-page', version: '1' } };
  if (simulcast) c.simulcast = { rids: ['r0', 'r1', 'r2'] };
  send(c);
  const deadline = Date.now() + secs * 1000;
  let doneAt = 0, switched = 0;
  while (Date.now() < deadline) {
    await sleep(500);
    if (chat.readyState === 'open') chat.send(`ping:${name}:${Date.now()}`);
    const s = await stats();
    for (const [k, v] of Object.entries(s)) { R.decoded[k] = v.decoded; R.widths[k] = v.width; }
    const good = Object.values(s).filter(v => v.decoded > 0).length;
    if (good >= peers && R.pongsFrom.length >= peers && !doneAt) { doneAt = Date.now(); log('all decoded', s); }
    // 層の切り替え: サイマルキャストの送り手の層を r2 → r0 と選び、幅が変わることを見る
    if (switchLayers && doneAt && switched === 0) {
      const sc = Object.entries(s).find(([k]) => k === (q.get('target') || 'alice'));
      if (sc) { send({ type: 'layer', track_id: sc[1].track, rid: 'r2' }); switched = 1; R.switchAt = Date.now(); }
    } else if (switched === 1 && Date.now() - R.switchAt > 8000) {
      const sc = Object.entries(s).find(([k]) => k === (q.get('target') || 'alice'));
      R.widthHigh = sc ? sc[1].width : 0;
      if (sc) send({ type: 'layer', track_id: sc[1].track, rid: 'r0' });
      switched = 2; R.switchAt = Date.now();
    } else if (switched === 2 && Date.now() - R.switchAt > 5000) {
      const sc = Object.entries(s).find(([k]) => k === (q.get('target') || 'alice'));
      R.widthLow = sc ? sc[1].width : 0; switched = 3;
    }
    if (doneAt && Date.now() - doneAt > 3000 && (!switchLayers || switched === 3)) break;
  }
  R.ok = Object.values(R.decoded).filter(d => d > 0).length >= peers && R.pongsFrom.length >= peers;
  R.connectionId = connId; R.slots = slots;
  R.outbound = []; R.availableOutgoingBitrate = 0;
  (await pc.getStats()).forEach((r) => {
    if (r.type === 'outbound-rtp' && r.kind === 'video') R.outbound.push({ rid: r.rid, width: r.frameWidth, sent: r.framesSent, limit: r.qualityLimitationReason, active: r.active });
    if (r.type === 'candidate-pair' && r.nominated && r.availableOutgoingBitrate) R.availableOutgoingBitrate = r.availableOutgoingBitrate;
  });
  // 切断はしない（他のページがまだこのページの映像を見ているかもしれない）。ブラウザーを閉じると切れる
  window.__result = R;
}
main().catch((e) => { R.error = String(e); window.__result = R; });
</script></body></html>
"#;
