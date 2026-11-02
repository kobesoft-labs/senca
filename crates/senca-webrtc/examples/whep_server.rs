//! WHEP 風の再生の試験用サーバー。ファイルの映像をブラウザーへ送る（サーバーは sendonly）。
//!
//! `whep_server --http 0.0.0.0:50081 --udp 192.0.2.1:50200 --input FILE --codec vp8|h264|av1
//!   [--advertise 192.0.2.1:50300] [--offer server]`
//!
//! - 入力: VP8・AV1 は IVF、H.264 は AUD 付きの Annex B（ffmpeg の `-x264-params aud=1 -f h264`）、
//!   AV1 は `.obu`（Low Overhead Bitstream Format）でもよい。30 fps で繰り返し送る。
//! - `GET /` は試験ページ。`POST /whep`（ブラウザーの recvonly offer）→ 201 と answer。
//!   `--offer server` のときは `POST /whep/offer` で server の offer を返し、`PATCH /whep/<id>` の本文の answer を入れる。
//! - `--advertise` を付けると、その（中継の）アドレスを host 候補として出す（損失のある中継を挟む試験）。
//! - `GET /status` → JSON（送った AU、NACK に応えた再送の数、推定帯域）。

#[path = "common/mod.rs"]
mod common;

use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use common::*;
use senca_webrtc::*;

struct Frame {
    data: Bytes,
    key: bool,
}

struct Session {
    id: u32,
    pc: PeerConnection,
    remote: Option<SocketAddr>,
    track: Option<TrackId>,
    started: Option<Instant>,
    sent: u64,
    keyframe_requests: u32,
    bwe: u64,
    closed: bool,
    events: Vec<String>,
    pending_offer: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let http = arg(&args, "--http").unwrap_or_else(|| "127.0.0.1:50081".into());
    let udp: SocketAddr = arg(&args, "--udp")
        .unwrap_or_else(|| "127.0.0.1:50200".into())
        .parse()?;
    let advertise: SocketAddr = arg(&args, "--advertise")
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(udp);
    let input = arg(&args, "--input").ok_or("--input")?;
    let codec = match arg(&args, "--codec").as_deref() {
        Some("h264") => Codec::H264,
        Some("av1") => Codec::Av1,
        _ => Codec::Vp8,
    };
    // 入力を読む
    let mut config = None;
    let frames: Vec<Frame> = match codec {
        Codec::H264 => {
            let d = std::fs::read(&input)?;
            let mut sps = None;
            let mut pps = None;
            let mut v = Vec::new();
            for (au, key, s, p) in h264_aus(&d) {
                if s.is_some() {
                    sps = s;
                }
                if p.is_some() {
                    pps = p;
                }
                v.push(Frame { data: au, key });
            }
            if let (Some(s), Some(p)) = (sps, pps) {
                config = Some(senca_h264::codec_config(&s, &p, 0)?);
            }
            v
        }
        Codec::Av1 if input.ends_with(".obu") => {
            let d = std::fs::read(&input)?;
            av1_temporal_units(&d)
                .into_iter()
                .map(|(data, key)| Frame { data, key })
                .collect()
        }
        _ => {
            let (_, fr) = read_ivf(&input)?;
            fr.into_iter()
                .map(|(_, data)| {
                    let key = match codec {
                        Codec::Vp8 => data.first().is_some_and(|b| b & 1 == 0),
                        // AV1 の IVF: 順序ヘッダー（OBU type 1）を含む TU をキーフレームとみなす
                        _ => data.len() > 2 && av1_has_seq_header(&data),
                    };
                    let data = if codec == Codec::Av1 {
                        senca_av1::strip_temporal_delimiters(&data).unwrap_or(data)
                    } else {
                        data
                    };
                    Frame { data, key }
                })
                .collect()
        }
    };
    if frames.is_empty() || !frames[0].key {
        return Err("input must start with a keyframe".into());
    }
    eprintln!(
        "whep_server http={http} udp={udp} advertise={advertise} frames={} keyframes={}",
        frames.len(),
        frames.iter().filter(|f| f.key).count()
    );
    let server_offer = arg(&args, "--offer").as_deref() == Some("server");
    let sock = UdpSocket::bind(udp)?;
    let (tx, rx) = mpsc::channel::<HttpMsg>();
    serve_http(&http, tx)?;
    let cert = DtlsCertificate::generate_self_signed("senca-whep")?;
    let mut sessions: Vec<Session> = Vec::new();
    let mut next_id = 1u32;
    let mut buf = vec![0u8; 65536];
    let frame_dur = Duration::from_micros(33_333);
    loop {
        let now = Instant::now();
        while let Ok((req, resp)) = rx.try_recv() {
            let r = handle_http(
                &req,
                &mut sessions,
                &mut next_id,
                &cert,
                advertise,
                codec,
                server_offer,
                now,
            );
            let _ = resp.send(r);
        }
        // メディア: 接続した session ごとに 30 fps
        for s in sessions.iter_mut().filter(|s| !s.closed) {
            let (Some(track), Some(start)) = (s.track, s.started) else {
                continue;
            };
            let due = (now.saturating_duration_since(start).as_micros() / frame_dur.as_micros())
                as u64
                + 1;
            while s.sent < due {
                let n = s.sent;
                let f = &frames[(n % frames.len() as u64) as usize];
                let au = AccessUnit {
                    track: TrackId(1),
                    codec,
                    epoch: 0,
                    sequence: n,
                    config_generation: 0,
                    pts: n as i64 * 3000,
                    dts: n as i64 * 3000,
                    duration: 3000,
                    timebase: Timebase::new(1, 90000),
                    keyframe: f.key,
                    discontinuity: false,
                    capture_time_ns: None,
                    payload: f.data.clone(),
                };
                if let Err(e) = s.pc.send_access_unit(now, track, &au, config.as_ref()) {
                    s.events.push(format!("send error {e}"));
                }
                s.sent += 1;
            }
        }
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
                sessions[i]
                    .pc
                    .handle_datagram_on(now, advertise, from, data);
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
                match e {
                    Event::Connected => {
                        s.events.push("connected".into());
                        s.track =
                            s.pc.send_tracks()
                                .iter()
                                .find(|t| t.kind == MediaKind::Video)
                                .map(|t| t.track);
                        s.started = Some(now);
                    }
                    Event::KeyframeRequest { .. } => s.keyframe_requests += 1,
                    Event::BandwidthEstimate { bps } => s.bwe = bps,
                    Event::Disconnected { reason } => {
                        s.events.push(format!("disconnected {reason:?}"));
                        s.closed = true;
                    }
                    Event::AccessUnit { .. } => {}
                    other => s
                        .events
                        .push(format!("{other:?}").chars().take(80).collect()),
                }
            }
        }
    }
}

fn av1_has_seq_header(d: &[u8]) -> bool {
    av1_temporal_units(d).iter().any(|(_, k)| *k)
        || (d.first().is_some_and(|h| (h >> 3) & 0xf == 1))
}

#[allow(clippy::too_many_arguments)]
fn handle_http(
    req: &Request,
    sessions: &mut Vec<Session>,
    next_id: &mut u32,
    cert: &DtlsCertificate,
    advertise: SocketAddr,
    codec: Codec,
    server_offer: bool,
    now: Instant,
) -> Response {
    let new_pc = || -> Result<PeerConnection, String> {
        let mut cfg = PeerConfig {
            certificate: Some(cert.clone()),
            unix_time_ns_at_start: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| d.as_nanos() as i64),
            datachannel: false,
            ..Default::default()
        };
        let enc = match codec {
            Codec::H264 => "h264",
            Codec::Av1 => "av1",
            _ => "vp8",
        };
        cfg.capabilities.codecs.retain(|c| {
            c.encoding.eq_ignore_ascii_case(enc) || c.encoding.eq_ignore_ascii_case("opus")
        });
        let mut pc = PeerConnection::new(cfg, now).map_err(|e| e.to_string())?;
        pc.add_host_address(now, advertise)
            .map_err(|e| e.to_string())?;
        Ok(pc)
    };
    let new_session = |pc: PeerConnection, id: u32, pending: bool| Session {
        id,
        pc,
        remote: None,
        track: None,
        started: None,
        sent: 0,
        keyframe_requests: 0,
        bwe: 0,
        closed: false,
        events: Vec::new(),
        pending_offer: pending,
    };
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => Response::new(200, "text/html; charset=utf-8", PAGE),
        ("GET", "/status") => {
            let v: Vec<String> = sessions
                .iter()
                .map(|s| {
                    let (rtx, _) = s.pc.retransmission_counters();
                    format!(
                        r#"{{"id":{},"connected":{},"sent":{},"retransmitted":{},"keyframe_requests":{},"bwe":{},"rtt_ms":{},"events":{:?}}}"#,
                        s.id,
                        s.pc.is_connected(),
                        s.sent,
                        rtx,
                        s.keyframe_requests,
                        s.bwe,
                        s.pc.rtt().as_millis(),
                        s.events
                    )
                })
                .collect();
            Response::new(200, "application/json", format!("[{}]", v.join(",")))
        }
        ("POST", "/whep") if !server_offer => {
            let offer = String::from_utf8_lossy(&req.body).to_string();
            let mut pc = match new_pc() {
                Ok(p) => p,
                Err(e) => return Response::new(500, "text/plain", e),
            };
            match pc.accept_offer(now, &offer) {
                Ok(answer) => {
                    let id = *next_id;
                    *next_id += 1;
                    sessions.push(new_session(pc, id, false));
                    Response::new(201, "application/sdp", answer)
                        .header("Location", &format!("/whep/{id}"))
                }
                Err(e) => Response::new(400, "text/plain", e.to_string()),
            }
        }
        ("POST", "/whep/offer") => {
            let mut pc = match new_pc() {
                Ok(p) => p,
                Err(e) => return Response::new(500, "text/plain", e),
            };
            let spec = OfferSpec {
                media: vec![OfferMediaSpec {
                    kind: MediaKind::Video,
                    direction: Direction::SendOnly,
                    codecs: Some(vec![codec]),
                    simulcast: Vec::new(),
                }],
                datachannel: false,
            };
            match pc.create_offer(now, &spec) {
                Ok(offer) => {
                    let id = *next_id;
                    *next_id += 1;
                    sessions.push(new_session(pc, id, true));
                    Response::new(201, "application/sdp", offer)
                        .header("Location", &format!("/whep/{id}"))
                }
                Err(e) => Response::new(500, "text/plain", e.to_string()),
            }
        }
        (m, p) if p.starts_with("/whep/") => {
            let id: u32 = p[6..].parse().unwrap_or(0);
            let Some(s) = sessions.iter_mut().find(|s| s.id == id) else {
                return Response::empty(404);
            };
            match m {
                "PATCH" if s.pending_offer => {
                    let answer = String::from_utf8_lossy(&req.body).to_string();
                    s.pending_offer = false;
                    match s.pc.apply_answer(now, &answer) {
                        Ok(()) => Response::empty(204),
                        Err(e) => Response::new(400, "text/plain", e.to_string()),
                    }
                }
                "PATCH" => match s
                    .pc
                    .handle_trickle_patch(now, &String::from_utf8_lossy(&req.body))
                {
                    Ok(None) => Response::empty(204),
                    Ok(Some(f)) => Response::new(200, "application/trickle-ice-sdpfrag", f),
                    Err(e) => Response::new(409, "text/plain", e.to_string()),
                },
                "DELETE" => {
                    s.pc.close(now);
                    s.closed = true;
                    Response::empty(200)
                }
                _ => Response::empty(405),
            }
        }
        _ => Response::empty(404),
    }
}

const PAGE: &str = r#"<!doctype html>
<meta charset="utf-8"><title>WHEP test</title>
<video id="v" autoplay muted playsinline></video>
<script>
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function run() {
  const q = new URLSearchParams(location.search);
  const secs = Number(q.get('secs') || 10);
  const serverOffer = q.get('offer') === 'server';
  const pc = new RTCPeerConnection({ bundlePolicy: 'max-bundle' });
  const video = document.getElementById('v');
  pc.ontrack = (e) => { video.srcObject = e.streams[0] || new MediaStream([e.track]); };
  let loc;
  if (serverOffer) {
    const r = await fetch('/whep/offer', { method: 'POST' });
    if (r.status !== 201) throw new Error('offer ' + r.status + ' ' + await r.text());
    loc = r.headers.get('Location');
    await pc.setRemoteDescription({ type: 'offer', sdp: await r.text() });
    await pc.setLocalDescription(await pc.createAnswer());
    for (let i = 0; i < 40 && pc.iceGatheringState !== 'complete'; i++) await sleep(50);
    const p = await fetch(loc, { method: 'PATCH', headers: { 'Content-Type': 'application/sdp' }, body: pc.localDescription.sdp });
    if (p.status !== 204) throw new Error('answer ' + p.status + ' ' + await p.text());
  } else {
    pc.addTransceiver('video', { direction: 'recvonly' });
    await pc.setLocalDescription(await pc.createOffer());
    const r = await fetch('/whep', { method: 'POST', headers: { 'Content-Type': 'application/sdp' }, body: pc.localDescription.sdp });
    if (r.status !== 201) throw new Error('POST ' + r.status + ' ' + await r.text());
    loc = r.headers.get('Location');
    await pc.setRemoteDescription({ type: 'answer', sdp: await r.text() });
  }
  for (let i = 0; i < 100 && pc.connectionState !== 'connected'; i++) await sleep(100);
  if (pc.connectionState !== 'connected') throw new Error('not connected: ' + pc.connectionState);
  await sleep(secs * 1000);
  let inb = null; let codec = null;
  const stats = await pc.getStats();
  stats.forEach((s) => {
    if (s.type === 'inbound-rtp' && s.kind === 'video') inb = s;
  });
  stats.forEach((s) => { if (inb && s.id === inb.codecId) codec = s.mimeType; });
  const pq = video.getVideoPlaybackQuality ? video.getVideoPlaybackQuality() : {};
  const res = { ok: true, codec, framesDecoded: inb.framesDecoded, framesReceived: inb.framesReceived,
    framesDropped: inb.framesDropped, keyFramesDecoded: inb.keyFramesDecoded, packetsReceived: inb.packetsReceived,
    packetsLost: inb.packetsLost, nackCount: inb.nackCount, pliCount: inb.pliCount, firCount: inb.firCount,
    retransmittedPacketsReceived: inb.retransmittedPacketsReceived, freezeCount: inb.freezeCount,
    totalFreezesDuration: inb.totalFreezesDuration, decoderImplementation: inb.decoderImplementation,
    frameWidth: inb.frameWidth, frameHeight: inb.frameHeight, jitterBufferDelay: inb.jitterBufferDelay,
    jitterBufferEmittedCount: inb.jitterBufferEmittedCount,
    videoWidth: video.videoWidth, corruptedVideoFrames: pq.corruptedVideoFrames || 0,
    totalVideoFrames: pq.totalVideoFrames || 0 };
  await fetch(loc, { method: 'DELETE' });
  pc.close();
  return res;
}
run().then((r) => { window.__result = r; }, (e) => { window.__result = { ok: false, error: String(e) }; });
</script>
"#;
