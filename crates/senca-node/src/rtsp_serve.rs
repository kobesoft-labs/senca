//! RTSP サーバーの play 出力（仕様 6.6「サーバー play」）。監視システム・既存のプレイヤー向け。
//!
//! URL: `rtsp://host:port/live/{stream_id}?exp={exp}&sig={sig}`（署名付きの再生 URL と同じ署名）。
//! 転送は TCP interleaved（NAT・ファイアウォールを越えやすい）。UDP は 461 で断る（後続）。
//! AU はコピーせずに参照で受け、自前のパケット化（senca-rtp）で RTP にする。

use crate::app::App;
use crate::hub::HubEvent;
use senca_core::{CodecConfig, MediaKind, TrackId};
use senca_rtp::rtcp::{RtcpPacket, Sdes, SdesChunk};
use senca_rtp::{RtpSender, payload};
use senca_rtsp::headers::NptTime;
use senca_rtsp::{LowerTransport, Range, RtpInfoEntry, ServerConfig, ServerEvent, ServerSession};
use senca_sdp::{Direction, OfferCodec, OfferMedia, OfferMode, OfferOptions, create_offer};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Out {
    track: TrackId,
    sender: RtpSender,
    config: CodecConfig,
    channels: Option<(u8, u8)>,
}

fn rand_u64() -> u64 {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    u64::from_le_bytes(b)
}

pub async fn serve(app: Arc<App>, mut sock: tokio::net::TcpStream, peer: String) {
    let mut s = ServerSession::new(ServerConfig::new(format!("{:016X}", rand_u64())));
    let mut buf = vec![0u8; 64 * 1024];
    let mut outs: Vec<Out> = Vec::new();
    let mut sub: Option<crate::hub::Subscription> = None;
    let mut playing = false;
    let mut last_sr = Instant::now();
    let mut pending: Vec<HubEvent> = Vec::new();
    loop {
        let out = s.take_output();
        if !out.is_empty() && sock.write_all(&out).await.is_err() {
            return;
        }
        if s.is_closed() {
            return;
        }
        let ev = async {
            match sub.as_mut() {
                Some(x) if playing => Some(x.recv().await),
                _ => std::future::pending().await,
            }
        };
        tokio::select! {
            r = sock.read(&mut buf) => {
                let n = match r { Ok(0) | Err(_) => return, Ok(n) => n };
                if s.feed(Instant::now(), &buf[..n]).is_err() { return; }
            }
            e = ev => match e {
                Some(Ok(HubEvent::End)) | Some(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return,
                Some(Ok(x)) => pending.push(x),
                Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => { if let Some(x) = sub.as_mut() { x.lagged(n) } }
                None => {}
            },
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        let now = Instant::now();
        if s.poll_timeout().is_some_and(|t| t <= now) {
            s.handle_timeout(now);
        }
        while let Some(ev) = s.poll_event() {
            match ev {
                ServerEvent::Describe { id, uri, .. } => {
                    // /live/{id}?exp&sig
                    let tail = uri.split("/live/").nth(1).unwrap_or("").to_string();
                    let Some(mut x) = app.subscribe_rtmp_play(&tail) else {
                        let _ = s.reject(id, 404);
                        continue;
                    };
                    let init = x.initial();
                    let configs: BTreeMap<TrackId, CodecConfig> =
                        init.iter().filter_map(|e| if let HubEvent::Config(t, c) = e { Some((*t, c.clone())) } else { None }).collect();
                    outs.clear();
                    let mut media = Vec::new();
                    for (n, (t, c)) in configs.iter().enumerate() {
                        let Ok((pz, fmt)) = payload::packetizer(c.codec, Some(c), 1400) else { continue };
                        let pt = payload::static_payload_type(c.codec).unwrap_or(96 + n as u8);
                        let r = rand_u64();
                        outs.push(Out { track: *t, sender: RtpSender::new(pz, pt, (r >> 32) as u32, r as u16, (r >> 16) as u32), config: c.clone(), channels: None });
                        media.push(OfferMedia {
                            kind: if c.codec.kind() == MediaKind::Video { senca_sdp::MediaKind::Video } else { senca_sdp::MediaKind::Audio },
                            mid: (outs.len() - 1).to_string(),
                            direction: Direction::SendOnly,
                            codecs: vec![OfferCodec { payload_type: pt, encoding: fmt.encoding.clone(), clock_rate: fmt.clock_rate, channels: fmt.channels, fmtp: fmt.fmtp.clone(), rtcp_fb: Vec::new(), rtx_payload_type: None }],
                            extmaps: Vec::new(),
                            send: None,
                            control: Some(format!("trackID={}", outs.len() - 1)),
                            datachannel: false,
                        });
                    }
                    if outs.is_empty() {
                        let _ = s.reject(id, 415);
                        continue;
                    }
                    let opts = OfferOptions { mode: OfferMode::Rtsp, session_id: rand_u64() >> 1, session_name: "SENCA".into(), origin_address: "0.0.0.0".into(), rtsp_range: Some("npt=now-".into()), rtsp_control: Some("*".into()), ..Default::default() };
                    match create_offer(&media, &opts) {
                        Ok(sdp) => { let _ = s.accept_describe(id, bytes::Bytes::from(sdp.to_string()), None); }
                        Err(_) => { let _ = s.reject(id, 500); continue; }
                    }
                    pending = init.into_iter().filter(|e| matches!(e, HubEvent::Au(_))).collect();
                    sub = Some(x);
                }
                ServerEvent::Setup { id, uri, transports, .. } => {
                    let n = uri.rsplit("trackID=").next().and_then(|v| v.split(['?', '/']).next()).and_then(|v| v.parse::<usize>().ok()).filter(|n| *n < outs.len());
                    let t = transports.into_iter().find(|t| t.lower == LowerTransport::Tcp);
                    match (n, t) {
                        (Some(n), Some(mut t)) => {
                            t.ssrc = Some(outs[n].sender.ssrc());
                            match s.accept_setup(id, t) {
                                Ok(t) => outs[n].channels = t.interleaved,
                                Err(_) => { let _ = s.reject(id, 461); }
                            }
                        }
                        (None, _) => { let _ = s.reject(id, 404); }
                        (_, None) => { let _ = s.reject(id, 461); }
                    }
                }
                ServerEvent::Play { id, uri, .. } => {
                    let base = uri.split('?').next().unwrap_or("").trim_end_matches('/').to_string();
                    let first: BTreeMap<TrackId, (i64, senca_core::Timebase)> = pending.iter().filter_map(|e| if let HubEvent::Au(a) = e { Some((a.track, (a.pts, a.timebase))) } else { None }).rev().collect();
                    let info: Vec<RtpInfoEntry> = outs.iter().enumerate().filter(|(_, o)| o.channels.is_some()).map(|(n, o)| RtpInfoEntry {
                        url: format!("{base}/trackID={n}"),
                        seq: Some(o.sender.next_sequence()),
                        rtptime: first.get(&o.track).and_then(|(p, tb)| o.sender.rtp_timestamp(*p, *tb)),
                    }).collect();
                    let _ = s.accept_play(id, Some(Range::Npt { start: NptTime::Seconds(0.0), end: None }), &info);
                    playing = true;
                    app.log(&format!("rtsp play started peer={peer}"));
                }
                ServerEvent::Teardown { .. } | ServerEvent::TimedOut => {
                    let out = s.take_output();
                    let _ = sock.write_all(&out).await;
                    return;
                }
                ServerEvent::Pause { id, .. } => { let _ = s.accept(id); }
                _ => {}
            }
        }
        if playing {
            for e in pending.drain(..) {
                match e {
                    HubEvent::Au(a) => {
                        if let Some(o) = outs.iter_mut().find(|o| o.track == a.track)
                            && let Some((ch, _)) = o.channels
                            && let Ok(pkts) = o.sender.send(&a, Some(&o.config))
                        {
                            for p in pkts {
                                if let Ok(b) = p.to_bytes() {
                                    let _ = s.send_interleaved(ch, &b);
                                }
                            }
                        }
                    }
                    HubEvent::Config(t, c) => {
                        if let Some(o) = outs.iter_mut().find(|o| o.track == t) {
                            o.config = c;
                        }
                    }
                    _ => {}
                }
            }
            if last_sr.elapsed() >= Duration::from_secs(1) {
                last_sr = Instant::now();
                let unix_ns = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0);
                let ntp = senca_rtp::clock::unix_ns_to_ntp(unix_ns);
                for o in outs.iter().filter(|o| o.channels.is_some()) {
                    let pk = vec![RtcpPacket::SenderReport(o.sender.sender_report(ntp, 0)), RtcpPacket::Sdes(Sdes { chunks: vec![SdesChunk::cname(o.sender.ssrc(), "senca")] })];
                    if let (Ok(b), Some((_, ch))) = (senca_rtp::build_compound(&pk), o.channels) {
                        let _ = s.send_interleaved(ch, &b);
                    }
                }
            }
        }
    }
}
