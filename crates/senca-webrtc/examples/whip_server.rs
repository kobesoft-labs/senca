//! WHIP（RFC 9725）で受ける試験用サーバー。ブラウザーの送ったメディアをファイルに書く。
//!
//! `whip_server --http 0.0.0.0:50080 --udp 192.0.2.1:50100 --out DIR [--codec vp8|h264|any]`
//!
//! - `GET /` は試験ページ（`?simulcast=1&codec=vp8&secs=8`）
//! - `POST /whip`（application/sdp）→ 201、Location、answer
//! - `PATCH /whip/<id>`（application/trickle-ice-sdpfrag）→ 204 か 200（ICE restart）
//! - `DELETE /whip/<id>` → 閉じてファイルを仕上げる
//! - `GET /status` → JSON（トラック・書いた AU の数・DataChannel の echo の数）
//!
//! 受信トラックごとに `DIR/<session>_<mid>_<rid>.{ivf,h264,ogg}` を書く。DataChannel の文字列には
//! `echo:` を付けて返し、バイナリはそのまま返す。

#[path = "common/mod.rs"]
mod common;

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::*;
use senca_webrtc::*;

enum Writer {
    Ivf(IvfWriter),
    H264(std::io::BufWriter<std::fs::File>, Vec<Vec<u8>>),
    Ogg(OggOpusWriter),
}

struct TrackOut {
    writer: Option<Writer>,
    path: String,
    aus: u64,
    keyframes: u64,
    codec: Codec,
    rid: Option<String>,
    bytes: u64,
}

struct Session {
    id: u32,
    pc: PeerConnection,
    remote: Option<SocketAddr>,
    tracks: HashMap<TrackId, TrackOut>,
    echoes: u32,
    dc_messages: Vec<String>,
    closed: bool,
    connected_at: Option<Instant>,
    events: Vec<String>,
}

fn finish(t: &mut TrackOut) {
    if let Some(w) = t.writer.take() {
        let _ = match w {
            Writer::Ivf(w) => w.finish().map(|_| ()),
            Writer::H264(mut f, _) => std::io::Write::flush(&mut f),
            Writer::Ogg(w) => w.finish().map(|_| ()),
        };
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let http = arg(&args, "--http").unwrap_or_else(|| "127.0.0.1:50080".into());
    let udp: SocketAddr = arg(&args, "--udp")
        .unwrap_or_else(|| "127.0.0.1:50100".into())
        .parse()?;
    let out = arg(&args, "--out").unwrap_or_else(|| ".".into());
    let codec = arg(&args, "--codec").unwrap_or_else(|| "any".into());
    std::fs::create_dir_all(&out)?;
    let sock = UdpSocket::bind(udp)?;
    let (tx, rx) = mpsc::channel::<HttpMsg>();
    serve_http(&http, tx)?;
    eprintln!("whip_server http={http} udp={udp}");
    let cert = DtlsCertificate::generate_self_signed("senca-whip")?;
    let mut sessions: Vec<Session> = Vec::new();
    let mut next_id = 1u32;
    let mut buf = vec![0u8; 65536];
    loop {
        let now = Instant::now();
        // HTTP
        while let Ok((req, resp)) = rx.try_recv() {
            let r = handle_http(
                &req,
                &mut sessions,
                &mut next_id,
                &cert,
                udp,
                &codec,
                &out,
                now,
            );
            let _ = resp.send(r);
        }
        // UDP
        let mut wait = Duration::from_millis(5);
        for s in &sessions {
            if let Some(t) = s.pc.poll_timeout() {
                wait = wait.min(t.saturating_duration_since(now));
            }
        }
        sock.set_read_timeout(Some(wait.max(Duration::from_micros(200))))?;
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            let now = Instant::now();
            let data = &buf[..n];
            let idx = sessions
                .iter()
                .position(|s| s.remote == Some(from))
                .or_else(|| {
                    let u = stun_username_ufrag(data)?;
                    sessions.iter().position(|s| s.pc.local_ufrag() == u)
                });
            if let Some(i) = idx {
                let s = &mut sessions[i];
                s.pc.handle_datagram(now, from, data);
            }
        }
        let now = Instant::now();
        for s in sessions.iter_mut() {
            if s.pc.poll_timeout().is_some_and(|t| t <= now) {
                s.pc.handle_timeout(now);
            }
            while let Some(t) = s.pc.poll_transmit() {
                let _ = sock.send_to(&t.data, t.remote);
            }
            if let Some((_, r)) = s.pc.selected_path() {
                s.remote = Some(r);
            }
            while let Some(e) = s.pc.poll_event() {
                on_event(s, e, &out, now);
            }
        }
        sessions.retain(|s| !(s.closed && s.pc.poll_timeout().is_none()));
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_http(
    req: &Request,
    sessions: &mut Vec<Session>,
    next_id: &mut u32,
    cert: &DtlsCertificate,
    udp: SocketAddr,
    codec: &str,
    _out: &str,
    now: Instant,
) -> Response {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/whip.html") => {
            Response::new(200, "text/html; charset=utf-8", PAGE)
        }
        ("GET", "/status") => {
            let mut v = Vec::new();
            for s in sessions.iter() {
                let tracks: Vec<String> = s
                    .tracks
                    .values()
                    .map(|t| {
                        format!(
                            r#"{{"path":"{}","codec":"{}","rid":{},"aus":{},"keyframes":{},"bytes":{}}}"#,
                            t.path,
                            t.codec,
                            t.rid.as_ref().map_or("null".into(), |r| format!("\"{r}\"")),
                            t.aus,
                            t.keyframes,
                            t.bytes
                        )
                    })
                    .collect();
                let (rtx, nack) = s.pc.retransmission_counters();
                v.push(format!(
                    r#"{{"id":{},"connected":{},"closed":{},"echoes":{},"dc":{:?},"tracks":[{}],"nack_requested":{},"retransmitted":{},"dropped":"{:?}","events":{:?}}}"#,
                    s.id,
                    s.pc.is_connected(),
                    s.closed,
                    s.echoes,
                    s.dc_messages,
                    tracks.join(","),
                    nack,
                    rtx,
                    s.pc.dropped_counters(),
                    s.events
                ));
            }
            Response::new(200, "application/json", format!("[{}]", v.join(",")))
        }
        ("POST", "/whip") => {
            if req.headers.get("content-type").map(|c| c.as_str()) != Some("application/sdp") {
                return Response::empty(415);
            }
            let offer = String::from_utf8_lossy(&req.body).to_string();
            let mut cfg = PeerConfig {
                certificate: Some(cert.clone()),
                unix_time_ns_at_start: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_nanos() as i64),
                ..Default::default()
            };
            if codec != "any" {
                let keep = |e: &str| -> bool {
                    let e = e.to_ascii_lowercase();
                    e == codec || e == "opus" || e == "pcmu" || e == "pcma"
                };
                cfg.capabilities.codecs.retain(|c| keep(&c.encoding));
            }
            let mut pc = match PeerConnection::new(cfg, now) {
                Ok(p) => p,
                Err(e) => return Response::new(500, "text/plain", e.to_string()),
            };
            if let Err(e) = pc.add_host_address(now, udp) {
                return Response::new(500, "text/plain", e.to_string());
            }
            match pc.accept_offer(now, &offer) {
                Ok(answer) => {
                    let id = *next_id;
                    *next_id += 1;
                    sessions.push(Session {
                        id,
                        pc,
                        remote: None,
                        tracks: HashMap::new(),
                        echoes: 0,
                        dc_messages: Vec::new(),
                        closed: false,
                        connected_at: None,
                        events: Vec::new(),
                    });
                    Response::new(201, "application/sdp", answer)
                        .header("Location", &format!("/whip/{id}"))
                        .header("ETag", &format!("\"s{id}\""))
                }
                Err(e) => Response::new(400, "text/plain", e.to_string()),
            }
        }
        (m, p) if p.starts_with("/whip/") => {
            let id: u32 = p[6..].parse().unwrap_or(0);
            let Some(s) = sessions.iter_mut().find(|s| s.id == id) else {
                return Response::empty(404);
            };
            match m {
                "PATCH" => {
                    if req.headers.get("content-type").map(|c| c.as_str())
                        != Some("application/trickle-ice-sdpfrag")
                    {
                        return Response::empty(415);
                    }
                    let body = String::from_utf8_lossy(&req.body).to_string();
                    s.events.push(format!("patch {} bytes", body.len()));
                    match s.pc.handle_trickle_patch(now, &body) {
                        Ok(None) => Response::empty(204),
                        Ok(Some(frag)) => {
                            Response::new(200, "application/trickle-ice-sdpfrag", frag)
                        }
                        Err(e) => Response::new(409, "text/plain", e.to_string()),
                    }
                }
                "DELETE" => {
                    s.pc.close(now);
                    s.closed = true;
                    for t in s.tracks.values_mut() {
                        finish(t);
                    }
                    Response::empty(200)
                }
                _ => Response::empty(405),
            }
        }
        _ => Response::empty(404),
    }
}

fn on_event(s: &mut Session, e: Event, out: &str, now: Instant) {
    match e {
        Event::Connected => {
            s.connected_at = Some(now);
            s.events.push("connected".into());
        }
        Event::Track(info) => {
            s.events.push(format!(
                "track {} {} {:?} {:?}",
                info.mid, info.codec, info.rid, info.kind
            ));
            let rid = info.rid.clone().unwrap_or_else(|| "main".into());
            let (ext, w) = match info.codec {
                Codec::Vp8 => ("ivf", None),
                Codec::Vp9 => ("ivf", None),
                Codec::Av1 => ("ivf", None),
                Codec::H264 => ("h264", None::<Writer>),
                Codec::Opus => ("ogg", None),
                _ => ("bin", None),
            };
            let path = format!("{out}/{}_{}_{}.{ext}", s.id, info.mid, rid);
            let writer = w.or_else(|| match info.codec {
                Codec::Vp8 => IvfWriter::create(&path, b"VP80", 0, 0)
                    .ok()
                    .map(Writer::Ivf),
                Codec::Vp9 => IvfWriter::create(&path, b"VP90", 0, 0)
                    .ok()
                    .map(Writer::Ivf),
                Codec::Av1 => IvfWriter::create(&path, b"AV01", 0, 0)
                    .ok()
                    .map(Writer::Ivf),
                Codec::H264 => std::fs::File::create(&path)
                    .ok()
                    .map(|f| Writer::H264(std::io::BufWriter::new(f), Vec::new())),
                Codec::Opus => OggOpusWriter::create(&path).ok().map(Writer::Ogg),
                _ => None,
            });
            if let Some(old) = s.tracks.get_mut(&info.track) {
                finish(old);
            }
            s.tracks.insert(
                info.track,
                TrackOut {
                    writer,
                    path,
                    aus: 0,
                    keyframes: 0,
                    codec: info.codec,
                    rid: info.rid,
                    bytes: 0,
                },
            );
        }
        Event::AccessUnit { track, au, config } => {
            let Some(t) = s.tracks.get_mut(&track) else {
                return;
            };
            t.aus += 1;
            t.bytes += au.payload.len() as u64;
            if au.keyframe {
                t.keyframes += 1;
            }
            match t.writer.as_mut() {
                Some(Writer::Ivf(w)) => {
                    let mut data = Vec::new();
                    if au.codec == Codec::Av1 {
                        // IVF の AV1 は temporal unit（Temporal Delimiter から）
                        data.extend_from_slice(&[0x12, 0x00]);
                    }
                    data.extend_from_slice(&au.payload);
                    let _ = w.write(au.pts, &data);
                }
                Some(Writer::H264(f, params)) => {
                    if let Some(c) = &config
                        && let Ok(r) = senca_h264::AvcDecoderConfigurationRecord::parse(&c.record)
                    {
                        *params = r
                            .sps
                            .iter()
                            .chain(r.pps.iter())
                            .map(|n| n.to_vec())
                            .collect();
                    }
                    let mut v = Vec::new();
                    let ps: &[Vec<u8>] = if au.keyframe { params } else { &[] };
                    avcc_to_annexb(&au.payload, ps, &mut v);
                    let _ = std::io::Write::write_all(f, &v);
                }
                Some(Writer::Ogg(w)) => {
                    let samples = if au.duration > 0 { au.duration } else { 960 };
                    let _ = w.write(samples, &au.payload);
                }
                None => {}
            }
        }
        Event::DataChannelOpen(info) => {
            s.events.push(format!("dc open {} {}", info.id, info.label))
        }
        Event::DataChannelMessage { id, data, binary } => {
            if binary {
                s.dc_messages.push(format!("binary {}", data.len()));
                let _ = s.pc.send_data_binary(now, id, data);
            } else {
                let t = String::from_utf8_lossy(&data).to_string();
                s.dc_messages.push(t.clone());
                let _ = s.pc.send_data_text(now, id, &format!("echo:{t}"));
            }
            s.echoes += 1;
        }
        Event::Disconnected { reason } => {
            s.events.push(format!("disconnected {reason:?}"));
            for t in s.tracks.values_mut() {
                finish(t);
            }
            s.closed = true;
        }
        Event::KeyframeRequest { .. } | Event::BandwidthEstimate { .. } => {}
        other => s
            .events
            .push(format!("{other:?}").chars().take(80).collect()),
    }
}

const PAGE: &str = r#"<!doctype html>
<meta charset="utf-8"><title>WHIP test</title>
<video id="v" autoplay muted playsinline></video>
<script>
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
window.__log = [];
const log = (m) => window.__log.push(String(m));
async function run() {
  const q = new URLSearchParams(location.search);
  const simulcast = q.get('simulcast') === '1';
  const codec = (q.get('codec') || 'vp8').toLowerCase();
  const secs = Number(q.get('secs') || 8);
  const stream = await navigator.mediaDevices.getUserMedia({
    video: { width: 1280, height: 720, frameRate: 30 }, audio: true });
  document.getElementById('v').srcObject = stream;
  const pc = new RTCPeerConnection({ bundlePolicy: 'max-bundle' });
  const enc = simulcast ? [
    { rid: 'q', scaleResolutionDownBy: 4, maxBitrate: 200000 },
    { rid: 'h', scaleResolutionDownBy: 2, maxBitrate: 600000 },
    { rid: 'f', maxBitrate: 1500000 }] : undefined;
  const vt = pc.addTransceiver(stream.getVideoTracks()[0], { direction: 'sendonly', streams: [stream], sendEncodings: enc });
  const caps = RTCRtpSender.getCapabilities('video').codecs;
  const pref = caps.filter((c) => c.mimeType.toLowerCase() === 'video/' + codec
      && (codec !== 'h264' || /packetization-mode=1/.test(c.sdpFmtpLine || '')));
  if (pref.length) vt.setCodecPreferences(pref.concat(caps.filter((c) => !pref.includes(c))));
  pc.addTransceiver(stream.getAudioTracks()[0], { direction: 'sendonly', streams: [stream] });
  const dc = pc.createDataChannel('chat');
  const echoes = [];
  dc.binaryType = 'arraybuffer';
  dc.onmessage = (e) => echoes.push(typeof e.data === 'string' ? e.data : 'binary:' + e.data.byteLength);
  const cands = [];
  let gathered = false;
  pc.onicecandidate = (e) => { if (e.candidate) cands.push(e.candidate.candidate); else gathered = true; };
  await pc.setLocalDescription(await pc.createOffer());
  const r = await fetch('/whip', { method: 'POST', headers: { 'Content-Type': 'application/sdp' }, body: pc.localDescription.sdp });
  if (r.status !== 201) throw new Error('POST ' + r.status + ' ' + await r.text());
  const loc = r.headers.get('Location');
  await pc.setRemoteDescription({ type: 'answer', sdp: await r.text() });
  // trickle（RFC 9725 4.3.1）
  for (let i = 0; i < 40 && !gathered; i++) await sleep(50);
  const ufrag = pc.localDescription.sdp.match(/a=ice-ufrag:(\S+)/)[1];
  const pwd = pc.localDescription.sdp.match(/a=ice-pwd:(\S+)/)[1];
  const mid = pc.getTransceivers()[0].mid;
  let frag = 'a=ice-ufrag:' + ufrag + '\r\na=ice-pwd:' + pwd + '\r\nm=video 9 UDP/TLS/RTP/SAVPF 0\r\na=mid:' + mid + '\r\n';
  for (const c of cands) frag += 'a=' + c + '\r\n';
  frag += 'a=end-of-candidates\r\n';
  const pr = await fetch(loc, { method: 'PATCH', headers: { 'Content-Type': 'application/trickle-ice-sdpfrag', 'If-Match': '*' }, body: frag });
  const patchStatus = pr.status;
  for (let i = 0; i < 100 && pc.connectionState !== 'connected'; i++) await sleep(100);
  if (pc.connectionState !== 'connected') throw new Error('not connected: ' + pc.connectionState);
  for (let i = 0; i < 50 && dc.readyState !== 'open'; i++) await sleep(100);
  const dcOpen = dc.readyState === 'open';
  if (dcOpen) {
    dc.send('hello');
    dc.send('こんにちは');
    dc.send('');
    dc.send(new Uint8Array(70000));
  }
  await sleep(secs * 1000);
  const stats = await pc.getStats();
  const out = [];
  let bwe = 0;
  stats.forEach((s) => {
    if (s.type === 'outbound-rtp') out.push({ kind: s.kind, rid: s.rid || null, packetsSent: s.packetsSent,
      framesEncoded: s.framesEncoded, frameWidth: s.frameWidth, frameHeight: s.frameHeight, nackCount: s.nackCount,
      retransmittedPacketsSent: s.retransmittedPacketsSent, qualityLimitationReason: s.qualityLimitationReason,
      targetBitrate: s.targetBitrate, encoderImplementation: s.encoderImplementation, active: s.active });
    if (s.type === 'candidate-pair' && s.nominated && s.availableOutgoingBitrate) bwe = s.availableOutgoingBitrate;
    if (s.type === 'codec' && s.mimeType.startsWith('video')) out.push({ codec: s.mimeType, fmtp: s.sdpFmtpLine || '' });
  });
  await sleep(500);
  const d = await fetch(loc, { method: 'DELETE' });
  pc.close();
  return { ok: true, patchStatus, deleteStatus: d.status, dcOpen, echoes, outbound: out, availableOutgoingBitrate: bwe };
}
run().then((r) => { window.__result = r; }, (e) => { window.__result = { ok: false, error: String(e), log: window.__log }; });
</script>
"#;
