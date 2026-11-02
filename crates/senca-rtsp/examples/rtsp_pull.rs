//! 試験用の RTSP pull（ブロッキング I/O）。ClientSession → senca-sdp → senca-rtp の depacketizer → ファイル。
//!
//! `rtsp_pull <url> (udp|tcp) <out_prefix> <seconds> [--user U --pass P] [--allow-basic]`
//!
//! トラックごとに `<out_prefix>.<n>.<拡張子>` を書く（H.264/H.265: Annex B、VP8/VP9/AV1: IVF、音声: AU ごとの md5）。
//! 終わりに `TRACK <n> <codec> <file> <AU 数> <discontinuity 数>` を標準出力に出す。

mod common;

use common::{Res, Sink, media_format};
use senca_core::media::{CodecConfig, TrackId};
use senca_rtp::rtcp::{ReceiverReport, RtcpPacket};
use senca_rtp::{JitterBuffer, JitterOutput, RtpPacket, TrackReceiver, payload};
use senca_rtsp::auth::Credentials;
use senca_rtsp::{ClientConfig, ClientEvent, ClientSession, Range, TransportRequest};
use senca_sdp::{ParseOptions, SessionDescription};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

struct Track {
    pt: u8,
    receiver: TrackReceiver,
    jitter: JitterBuffer,
    sink: Sink,
    path: String,
    config: Option<CodecConfig>,
    aus: u64,
    discontinuities: u64,
    rtp: Option<UdpSocket>,
    rtcp: Option<UdpSocket>,
    server_rtcp: Option<SocketAddr>,
    ssrc: Option<u32>,
}

fn bind_pair() -> Res<(UdpSocket, UdpSocket)> {
    for _ in 0..50 {
        let a = UdpSocket::bind("0.0.0.0:0")?;
        let p = a.local_addr()?.port();
        let (rtp, rtcp_port) = if p % 2 == 0 {
            (a, p + 1)
        } else {
            match UdpSocket::bind(("0.0.0.0", p + 1)) {
                Ok(b) => (b, p + 2),
                Err(_) => continue,
            }
        };
        if let Ok(rtcp) = UdpSocket::bind(("0.0.0.0", rtcp_port)) {
            rtp.set_nonblocking(true)?;
            rtcp.set_nonblocking(true)?;
            return Ok((rtp, rtcp));
        }
    }
    Err("UDP のポートを確保できない".into())
}

impl Track {
    fn on_rtp(&mut self, data: &[u8], now: Instant) {
        let Ok(pkt) = RtpPacket::parse(bytes::Bytes::copy_from_slice(data)) else {
            return;
        };
        if pkt.header.payload_type != self.pt {
            return;
        }
        self.ssrc = Some(pkt.header.ssrc);
        self.receiver.observe(&pkt, now);
        self.jitter.push(pkt, now);
        self.drain(now);
    }

    fn drain(&mut self, now: Instant) {
        while let Some(o) = self.jitter.pop(now) {
            match o {
                JitterOutput::Packet(p) => {
                    let _ = self.receiver.push(&p);
                }
                JitterOutput::Lost { .. } => self.receiver.mark_loss(),
                JitterOutput::Reset => self.receiver.reset(),
            }
        }
        while let Some((au, cfg)) = self.receiver.pop() {
            if let Some(c) = cfg {
                self.config = Some(c);
            }
            if au.discontinuity {
                self.discontinuities += 1;
            }
            self.aus += 1;
            if let Err(e) = self.sink.write(&au, self.config.as_ref()) {
                eprintln!("write: {e}");
            }
        }
    }

    fn on_rtcp(&mut self, data: &[u8], now: Instant) {
        let b = bytes::Bytes::copy_from_slice(data);
        if let Ok(pkts) = senca_rtp::parse_reduced(&b) {
            for p in pkts {
                if let RtcpPacket::SenderReport(sr) = p {
                    self.receiver.on_sender_report(&sr, now);
                }
            }
        }
    }

    fn receiver_report(&mut self, now: Instant) -> Option<Vec<u8>> {
        let ssrc = self.ssrc?;
        let rb = self.receiver.stats_mut().report_block(ssrc, now);
        let rr = RtcpPacket::ReceiverReport(ReceiverReport {
            ssrc: 0x5E4C_A001,
            reports: vec![rb],
            extension: bytes::Bytes::new(),
        });
        let sdes = RtcpPacket::Sdes(senca_rtp::rtcp::Sdes {
            chunks: vec![senca_rtp::rtcp::SdesChunk::cname(0x5E4C_A001, "senca-pull")],
        });
        senca_rtp::build_compound(&[rr, sdes])
            .ok()
            .map(|b| b.to_vec())
    }
}

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!(
            "usage: rtsp_pull <url> (udp|tcp) <out_prefix> <seconds> [--user U --pass P] [--allow-basic]"
        );
        std::process::exit(2);
    }
    let udp = match args[2].as_str() {
        "udp" => true,
        "tcp" => false,
        _ => return Err("transport は udp か tcp".into()),
    };
    let prefix = args[3].clone();
    let seconds: u64 = args[4].parse()?;
    let mut cfg = ClientConfig::new(args[1].clone());
    let mut user = None;
    let mut pass = None;
    let mut i = 5;
    while i < args.len() {
        match args[i].as_str() {
            "--user" => user = args.get(i + 1).cloned(),
            "--pass" => pass = args.get(i + 1).cloned(),
            "--allow-basic" => {
                cfg.allow_basic = true;
                i += 1;
                continue;
            }
            _ => return Err("unknown option".into()),
        }
        i += 2;
    }
    if let (Some(u), Some(p)) = (user, pass) {
        cfg.credentials = Some(Credentials::new(u, p));
    }
    let mut c = ClientSession::new(cfg)?;
    let mut sock = TcpStream::connect(c.url().host_port())?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(Duration::from_millis(2)))?;
    let peer_ip = sock.peer_addr()?.ip();
    let start = Instant::now();
    c.start(start)?;
    c.describe(start)?;
    let mut tracks: Vec<Track> = Vec::new();
    let mut setups_done = 0usize;
    let mut playing_since: Option<Instant> = None;
    let mut last_rr = Instant::now();
    let mut buf = vec![0u8; 64 * 1024];
    let mut torn = false;
    loop {
        let now = Instant::now();
        let out = c.take_output();
        if !out.is_empty() {
            sock.write_all(&out)?;
        }
        if torn {
            break;
        }
        match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => c.feed(now, &buf[..n])?,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.into()),
        }
        if c.poll_timeout().is_some_and(|t| t <= now) {
            c.handle_timeout(now)?;
        }
        while let Some(ev) = c.poll_event() {
            match ev {
                ClientEvent::Described { sdp, .. } => {
                    let text = std::str::from_utf8(&sdp)?;
                    let sd = SessionDescription::parse_with(text, &ParseOptions::lenient())?;
                    for (n, m) in sd.media.iter().enumerate() {
                        let Some((pt, fmt)) = media_format(m) else {
                            continue;
                        };
                        let Ok(dep) = payload::depacketizer(&fmt) else {
                            eprintln!("skip track {n}: {}", fmt.encoding);
                            continue;
                        };
                        let codec = dep.codec();
                        let rate = dep.clock_rate();
                        let idx = tracks.len();
                        let (sink, path) = Sink::create(codec, &format!("{prefix}.{idx}"), rate)?;
                        let control = c.resolve_control(m.control().unwrap_or(""));
                        let (req, rtp, rtcp) = if udp {
                            let (a, b) = bind_pair()?;
                            let r = TransportRequest::Udp {
                                rtp_port: a.local_addr()?.port(),
                                rtcp_port: b.local_addr()?.port(),
                            };
                            (r, Some(a), Some(b))
                        } else {
                            let ch = (idx * 2) as u8;
                            (
                                TransportRequest::Interleaved {
                                    rtp_channel: ch,
                                    rtcp_channel: ch + 1,
                                },
                                None,
                                None,
                            )
                        };
                        tracks.push(Track {
                            pt,
                            receiver: TrackReceiver::new(TrackId(idx as u32), dep),
                            jitter: JitterBuffer::new(Duration::from_millis(300), 2048),
                            sink,
                            path,
                            config: None,
                            aus: 0,
                            discontinuities: 0,
                            rtp,
                            rtcp,
                            server_rtcp: None,
                            ssrc: None,
                        });
                        c.setup(now, &control, req)?;
                    }
                    if tracks.is_empty() {
                        return Err("使えるトラックがない".into());
                    }
                }
                ClientEvent::SetupDone {
                    track, transport, ..
                } => {
                    if let (Some(t), Some((_, rtcp))) =
                        (tracks.get_mut(track), transport.server_port)
                    {
                        t.server_rtcp = Some(SocketAddr::new(peer_ip, rtcp));
                    }
                    setups_done += 1;
                    if setups_done == tracks.len() {
                        c.play(now, Some(Range::npt_from(0.0)))?;
                    }
                }
                ClientEvent::Playing { .. } => {
                    playing_since = Some(now);
                    eprintln!("PLAYING");
                }
                ClientEvent::Interleaved { channel, data } => {
                    if let Some(t) = tracks.get_mut(channel as usize / 2) {
                        if channel % 2 == 0 {
                            t.on_rtp(&data, now);
                        } else {
                            t.on_rtcp(&data, now);
                        }
                    }
                }
                ClientEvent::TornDown => torn = true,
                ClientEvent::Failed { method, status, .. } => {
                    return Err(format!("{method:?} failed: {status}").into());
                }
                ClientEvent::Reconnect { url } => {
                    return Err(
                        format!("redirect to another host is not handled here: {url}").into(),
                    );
                }
                _ => {}
            }
        }
        let mut dgram = [0u8; 2048];
        for t in tracks.iter_mut() {
            while let Some(s) = &t.rtp {
                match s.recv_from(&mut dgram) {
                    Ok((n, _)) => {
                        let d = dgram[..n].to_vec();
                        t.on_rtp(&d, now);
                    }
                    Err(_) => break,
                }
            }
            while let Some(s) = &t.rtcp {
                match s.recv_from(&mut dgram) {
                    Ok((n, _)) => {
                        let d = dgram[..n].to_vec();
                        t.on_rtcp(&d, now);
                    }
                    Err(_) => break,
                }
            }
            t.drain(now);
        }
        if now.duration_since(last_rr) >= Duration::from_secs(1) {
            last_rr = now;
            for (i, t) in tracks.iter_mut().enumerate() {
                if let Some(rr) = t.receiver_report(now) {
                    match (&t.rtcp, t.server_rtcp) {
                        (Some(s), Some(addr)) => {
                            let _ = s.send_to(&rr, addr);
                        }
                        _ if !udp => {
                            let _ = c.send_interleaved((i * 2 + 1) as u8, &rr);
                        }
                        _ => {}
                    }
                }
            }
        }
        if playing_since.is_some_and(|p| now.duration_since(p) >= Duration::from_secs(seconds)) {
            c.teardown(now)?;
            // TEARDOWN の応答を待つ（来なければ 2 秒で終える）
            let deadline = now + Duration::from_secs(2);
            loop {
                let out = c.take_output();
                if !out.is_empty() {
                    sock.write_all(&out)?;
                }
                match sock.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = c.feed(Instant::now(), &buf[..n]);
                    }
                    Err(_) => {}
                }
                let mut done = false;
                while let Some(ev) = c.poll_event() {
                    if ev == ClientEvent::TornDown {
                        done = true;
                    }
                }
                if done || Instant::now() > deadline {
                    break;
                }
            }
            break;
        }
        if start.elapsed() > Duration::from_secs(seconds + 30) {
            return Err("timeout".into());
        }
    }
    let end = Instant::now() + Duration::from_secs(10);
    for (i, t) in tracks.iter_mut().enumerate() {
        t.drain(end);
        t.sink.finish()?;
        let (lost, late, dup) = t.jitter.counters();
        println!(
            "TRACK {i} {} {} {} {} lost={lost} late={late} dup={dup}",
            t.receiver.codec(),
            t.path,
            t.aus,
            t.discontinuities
        );
    }
    Ok(())
}
