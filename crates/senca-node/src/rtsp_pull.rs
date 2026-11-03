//! RTSP pull の入力（仕様 6.6、REQ-006 のカメラ）。TCP interleaved で受ける（NAT・ファイアウォールを越えやすい）。
//! 再接続は呼び出し側（io.rs）の指数バックオフ。Basic 認証は options の `allow_basic=true` のときだけ（仕様 6.6）。
//! `rtsps://`（既定のポート 322）は TLS（senca-tls）を通す。サーバーの証明書は OS の信頼する認証局で検証する
//! （`SSL_CERT_FILE` も読む）。自己署名のカメラは options の `tls_spki_sha256`（leaf の公開鍵の SHA-256。
//! base64 か 16 進）で公開鍵を固定する。このときは認証局・期限・ホスト名を見ない。

use crate::app::App;
use crate::io::IoStatus;
use senca_control::model::Input;
use senca_core::{CodecConfig, TrackId};
use senca_rtp::rtcp::RtcpPacket;
use senca_rtp::{JitterBuffer, JitterOutput, RtpPacket, TrackReceiver, payload};
use senca_rtsp::auth::Credentials;
use senca_rtsp::{ClientConfig, ClientEvent, ClientSession, Range, TransportRequest};
use senca_sdp::{ParseOptions, SessionDescription};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// `tls_spki_sha256` の値（base64・base64url・16 進、`,` で複数）を読む。
pub fn parse_spki_pins(v: &str) -> Result<Vec<[u8; 32]>, String> {
    use base64::Engine;
    let mut pins = Vec::new();
    for p in v.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let raw = if p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit()) {
            (0..32).map(|i| u8::from_str_radix(&p[2 * i..2 * i + 2], 16).unwrap_or(0)).collect::<Vec<u8>>()
        } else {
            let t = p.trim_end_matches('=');
            base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(t)
                .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(t))
                .map_err(|_| "tls_spki_sha256 は SHA-256（32 バイト）の base64 か 16 進で書いてください".to_string())?
        };
        let pin: [u8; 32] = raw.try_into().map_err(|_| "tls_spki_sha256 は SHA-256（32 バイト）です".to_string())?;
        pins.push(pin);
    }
    if pins.is_empty() {
        return Err("tls_spki_sha256 が空です".into());
    }
    Ok(pins)
}

/// rtsps の TLS の設定（公開鍵の固定があればそれだけで照合、無ければ OS の認証局で検証）。
fn tls_config(i: &Input) -> Result<Arc<senca_tls::ClientConfig>, String> {
    match i.options.get("tls_spki_sha256") {
        Some(v) => Ok(Arc::new(senca_tls::ClientConfig::new(senca_tls::ServerVerifier::PinnedSpkiOnly(parse_spki_pins(v)?)))),
        None => crate::tls_io::client_config(&[]),
    }
}

struct Track {
    pt: u8,
    receiver: TrackReceiver,
    jitter: JitterBuffer,
    config: Option<CodecConfig>,
    sent_gen: Option<u32>,
}

pub async fn run(app: &Arc<App>, i: &Input, st: &Mutex<IoStatus>) -> Result<(), String> {
    let mut cfg = ClientConfig::new(i.url.clone());
    cfg.allow_basic = i.options.get("allow_basic").is_some_and(|v| v == "true");
    if let Some(v) = crate::io::secret_value(app, &i.tenant, &i.credential_ref)? {
        // 秘密値は "user:password"
        let (u, p) = v.split_once(':').ok_or("RTSP の資格情報は user:password の形で保存してください")?;
        cfg.credentials = Some(Credentials::new(u.to_string(), p.to_string()));
    }
    let mut c = ClientSession::new(cfg).map_err(|e| format!("{e:?}"))?;
    let hp = c.url().host_port();
    let (host, port) = hp.rsplit_once(':').map(|(h, p)| (h.trim_matches(['[', ']']).to_string(), p.parse().unwrap_or(554))).unwrap_or((hp.clone(), 554));
    st.lock().unwrap().state = "connecting".into();
    let tls = c.url().scheme == senca_rtsp::url::Scheme::Rtsps;
    let tls_cfg = if tls { Some(tls_config(i)?) } else { None };
    let addr = crate::egress::resolve(&host, port, &app.cfg.egress_allow).await?;
    let tcp = tokio::time::timeout(Duration::from_secs(10), tokio::net::TcpStream::connect(addr))
        .await
        .map_err(|_| "connect timeout")?
        .map_err(|e| e.to_string())?;
    let _ = tcp.set_nodelay(true);
    let mut sock: Box<dyn crate::io::IoStream> = match tls_cfg {
        Some(cfg) => Box::new(crate::tls_io::connect(tcp, cfg, &host).await.map_err(|e| format!("rtsps: TLS: {e}"))?),
        None => Box::new(tcp),
    };
    let t0 = Instant::now();
    c.start(t0).map_err(|e| format!("{e:?}"))?;
    c.describe(t0).map_err(|e| format!("{e:?}"))?;
    let mut tracks: Vec<Track> = Vec::new();
    let mut setups = 0usize;
    let mut publisher: Option<crate::hub::Publisher> = None;
    let mut buf = vec![0u8; 64 * 1024];
    let mut last_rr = Instant::now();
    loop {
        let out = c.take_output();
        if !out.is_empty() {
            sock.write_all(&out).await.map_err(|e| e.to_string())?;
        }
        let now = Instant::now();
        let wait = c.poll_timeout().map(|t| t.saturating_duration_since(now)).unwrap_or(Duration::from_millis(200)).min(Duration::from_millis(200));
        match tokio::time::timeout(wait.max(Duration::from_millis(5)), sock.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                st.lock().unwrap().bytes += n as u64;
                c.feed(Instant::now(), &buf[..n]).map_err(|e| format!("{e:?}"))?;
            }
            Ok(Err(e)) => return Err(e.to_string()),
            Err(_) => {}
        }
        let now = Instant::now();
        if c.poll_timeout().is_some_and(|t| t <= now) {
            c.handle_timeout(now).map_err(|e| format!("{e:?}"))?;
        }
        while let Some(ev) = c.poll_event() {
            match ev {
                ClientEvent::Described { sdp, .. } => {
                    let text = std::str::from_utf8(&sdp).map_err(|_| "SDP が UTF-8 ではありません")?;
                    let sd = SessionDescription::parse_with(text, &ParseOptions::lenient()).map_err(|e| format!("sdp: {e:?}"))?;
                    for m in sd.media.iter() {
                        let Some((pt, fmt)) = media_format(m) else { continue };
                        let Ok(dep) = payload::depacketizer(&fmt) else { continue };
                        let idx = tracks.len();
                        let control = c.resolve_control(m.control().unwrap_or(""));
                        let ch = (idx * 2) as u8;
                        tracks.push(Track {
                            pt,
                            receiver: TrackReceiver::new(TrackId(idx as u32), dep),
                            jitter: JitterBuffer::new(Duration::from_millis(200), 2048),
                            config: None,
                            sent_gen: None,
                        });
                        c.setup(now, &control, TransportRequest::Interleaved { rtp_channel: ch, rtcp_channel: ch + 1 }).map_err(|e| format!("{e:?}"))?;
                        if tracks.len() >= 4 {
                            break;
                        }
                    }
                    if tracks.is_empty() {
                        return Err("使えるトラックがありません".into());
                    }
                }
                ClientEvent::SetupDone { .. } => {
                    setups += 1;
                    if setups == tracks.len() {
                        c.play(now, Some(Range::npt_from(0.0))).map_err(|e| format!("{e:?}"))?;
                    }
                }
                ClientEvent::Playing { .. } => {
                    publisher = Some(crate::io::attach(app, &i.stream, "rtsp_pull", &format!("{host}:{port}")).await?);
                    st.lock().unwrap().connects += 1;
                    st.lock().unwrap().state = "active".into();
                }
                ClientEvent::Interleaved { channel, data } => {
                    let Some(t) = tracks.get_mut(channel as usize / 2) else { continue };
                    if channel % 2 == 0 {
                        let Ok(pkt) = RtpPacket::parse(data) else { continue };
                        if pkt.header.payload_type != t.pt {
                            continue;
                        }
                        t.receiver.observe(&pkt, now);
                        t.jitter.push(pkt, now);
                    } else if let Ok(pkts) = senca_rtp::parse_reduced(&data) {
                        for p in pkts {
                            if let RtcpPacket::SenderReport(sr) = p {
                                t.receiver.on_sender_report(&sr, now);
                            }
                        }
                    }
                }
                ClientEvent::Failed { method, status, .. } => return Err(format!("RTSP {method:?} に失敗: {status}")),
                ClientEvent::Reconnect { .. } => return Err("別のホストへのリダイレクトは受け付けません".into()),
                ClientEvent::TornDown => break,
                _ => {}
            }
        }
        // 受け取った RTP を AU にして hub へ
        for t in tracks.iter_mut() {
            while let Some(o) = t.jitter.pop(now) {
                match o {
                    JitterOutput::Packet(p) => {
                        let _ = t.receiver.push(&p);
                    }
                    JitterOutput::Lost { .. } => t.receiver.mark_loss(),
                    JitterOutput::Reset => t.receiver.reset(),
                }
            }
            while let Some((au, cfg)) = t.receiver.pop() {
                if let Some(mut c) = cfg {
                    crate::media::enrich(&mut c);
                    t.config = Some(c);
                }
                let Some(p) = publisher.as_ref() else { continue };
                let Some(c) = t.config.as_ref() else { continue };
                if t.sent_gen != Some(c.generation) {
                    p.config(au.track, c.clone()).await.map_err(|e| e.to_string())?;
                    t.sent_gen = Some(c.generation);
                }
                p.access_unit(au).await.map_err(|e| e.to_string())?;
            }
        }
        if now.duration_since(last_rr) >= Duration::from_secs(1) {
            last_rr = now;
            for (k, t) in tracks.iter_mut().enumerate() {
                let rb = t.receiver.stats_mut().report_block(0, now);
                let rr = RtcpPacket::ReceiverReport(senca_rtp::rtcp::ReceiverReport { ssrc: 0x5E4C_A001, reports: vec![rb], extension: bytes::Bytes::new() });
                if let Ok(b) = senca_rtp::build_compound(&[rr]) {
                    let _ = c.send_interleaved((k * 2 + 1) as u8, &b);
                }
            }
        }
    }
    if let Some(p) = publisher.take() {
        p.detach("rtsp ended").await;
    }
    Err("相手が切りました".into())
}

fn media_format(m: &senca_sdp::MediaDescription) -> Option<(u8, senca_rtp::PayloadFormat)> {
    for pt in m.payload_types() {
        if let Some(rm) = m.rtpmap(pt) {
            let fmtp = m.fmtp(pt).map(|f| f.params).unwrap_or_default();
            return Some((pt, senca_rtp::PayloadFormat { encoding: rm.encoding, clock_rate: rm.clock_rate, channels: rm.channels, fmtp }));
        }
    }
    None
}
