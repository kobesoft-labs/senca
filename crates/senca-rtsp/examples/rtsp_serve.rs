//! 試験用の RTSP サーバー（1 接続、ブロッキング I/O）。MPEG-TS の AU を senca-rtp の packetizer で送る。
//!
//! `rtsp_serve <port> <input.ts> [--user U --pass P]`
//!
//! DESCRIBE には senca-sdp の RTSP 用 offer を返す。SETUP は TCP interleaved と UDP unicast の両方を受ける。
//! PLAY のあと AU を実時間で送り、終わったら各トラックに SR と BYE を送る。待ち受けを始めたら `LISTENING <port>` を出す。

mod common;

use common::{Res, SourceTrack, au_time_ns, read_ts};
use senca_rtp::rtcp::{Bye, RtcpPacket, Sdes, SdesChunk};
use senca_rtp::{RtpSender, payload};
use senca_rtsp::headers::NptTime;
use senca_rtsp::{
    Algorithm, LowerTransport, Range, RtpInfoEntry, ServerAuth, ServerConfig, ServerEvent,
    ServerSession,
};
use senca_sdp::Direction;
use senca_sdp::{MediaKind, OfferCodec, OfferMedia, OfferMode, OfferOptions, create_offer};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Out {
    sender: RtpSender,
    config: Option<senca_core::media::CodecConfig>,
    first_pts: i64,
    timebase: senca_core::Timebase,
    /// UDP: (RTP, RTCP) のソケットと宛先。TCP: interleaved のチャンネル。
    udp: Option<(UdpSocket, UdpSocket, SocketAddr, SocketAddr)>,
    channels: Option<(u8, u8)>,
    setup: bool,
}

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: rtsp_serve <port> <input.ts> [--user U --pass P]");
        std::process::exit(2);
    }
    let port: u16 = args[1].parse()?;
    let sources: Vec<SourceTrack> = read_ts(&args[2])?;
    let mut user = None;
    let mut pass = None;
    let mut i = 3;
    while i + 1 < args.len() {
        match args[i].as_str() {
            "--user" => user = Some(args[i + 1].clone()),
            "--pass" => pass = Some(args[i + 1].clone()),
            _ => return Err("unknown option".into()),
        }
        i += 2;
    }
    let seed = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64;
    let mut outs = Vec::new();
    let mut media = Vec::new();
    for (n, t) in sources.iter().enumerate() {
        let (pz, fmt) = payload::packetizer(t.codec, t.config.as_ref(), 1400)?;
        let pt = payload::static_payload_type(t.codec).unwrap_or(96 + n as u8);
        let first = t.aus.first().ok_or("空のトラック")?;
        let r = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(n as u64 * 1442695040888963407);
        outs.push(Out {
            sender: RtpSender::new(pz, pt, (r >> 32) as u32, r as u16, (r >> 16) as u32),
            config: t.config.clone(),
            first_pts: first.pts,
            timebase: first.timebase,
            udp: None,
            channels: None,
            setup: false,
        });
        media.push(OfferMedia {
            kind: if t.codec.kind() == senca_core::MediaKind::Video {
                MediaKind::Video
            } else {
                MediaKind::Audio
            },
            mid: n.to_string(),
            direction: Direction::SendOnly,
            codecs: vec![OfferCodec {
                payload_type: pt,
                encoding: fmt.encoding.clone(),
                clock_rate: fmt.clock_rate,
                channels: fmt.channels,
                fmtp: fmt.fmtp.clone(),
                rtcp_fb: Vec::new(),
                rtx_payload_type: None,
            }],
            extmaps: Vec::new(),
            send: None,
            control: Some(format!("trackID={n}")),
            datachannel: false,
        });
    }
    let sdp = create_offer(
        &media,
        &OfferOptions {
            mode: OfferMode::Rtsp,
            session_id: seed >> 1,
            session_name: "SENCA".into(),
            origin_address: "127.0.0.1".into(),
            rtsp_range: Some("npt=0-".into()),
            rtsp_control: Some("*".into()),
            ..Default::default()
        },
    )?
    .to_string();

    // 送る順（DTS 順）に並べる
    let mut schedule: Vec<(i64, usize, usize)> = Vec::new();
    let t0 = sources
        .iter()
        .filter_map(|t| t.aus.first().map(au_time_ns))
        .min()
        .unwrap_or(0);
    for (n, t) in sources.iter().enumerate() {
        for (k, au) in t.aus.iter().enumerate() {
            schedule.push((au_time_ns(au) - t0, n, k));
        }
    }
    schedule.sort();

    let listener = TcpListener::bind(("127.0.0.1", port))?;
    println!("LISTENING {port}");
    std::io::stdout().flush()?;
    let (mut sock, peer) = listener.accept()?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(Duration::from_millis(1)))?;
    let mut cfg = ServerConfig::new(format!("{:016X}", seed ^ 0x5E4C_A5E5));
    if let (Some(u), Some(p)) = (user, pass) {
        cfg.auth = Some(ServerAuth {
            realm: "SENCA".into(),
            nonce: format!("{:032x}", seed as u128 * 0x9E37_79B9_7F4A_7C15),
            algorithm: Algorithm::Md5,
            allow_basic: false,
            password_of: Box::new(move |name| (name == u).then(|| p.clone())),
        });
    }
    let mut s = ServerSession::new(cfg);
    let mut buf = vec![0u8; 64 * 1024];
    let mut play_start: Option<Instant> = None;
    let mut next = 0usize;
    let mut finished_at: Option<Instant> = None;
    let mut bye_sent = false;
    let mut last_sr = Instant::now();
    'outer: loop {
        let now = Instant::now();
        match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => s.feed(now, &buf[..n])?,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.into()),
        }
        if s.poll_timeout().is_some_and(|t| t <= now) {
            s.handle_timeout(now);
        }
        while let Some(ev) = s.poll_event() {
            match ev {
                ServerEvent::Describe { id, .. } => {
                    s.accept_describe(id, bytes::Bytes::from(sdp.clone()), None)?
                }
                ServerEvent::Setup {
                    id,
                    uri,
                    transports,
                    ..
                } => {
                    let n = uri
                        .rsplit("trackID=")
                        .next()
                        .and_then(|v| v.parse::<usize>().ok())
                        .filter(|n| *n < outs.len());
                    let (Some(n), Some(mut t)) = (n, transports.first().cloned()) else {
                        s.reject(id, 404)?;
                        continue;
                    };
                    if t.lower == LowerTransport::Udp {
                        let Some((crtp, crtcp)) = t.client_port else {
                            s.reject(id, 461)?;
                            continue;
                        };
                        let a = UdpSocket::bind("127.0.0.1:0")?;
                        let b = UdpSocket::bind("127.0.0.1:0")?;
                        b.set_nonblocking(true)?;
                        t.server_port = Some((a.local_addr()?.port(), b.local_addr()?.port()));
                        let o = &mut outs[n];
                        o.udp = Some((
                            a,
                            b,
                            SocketAddr::new(peer.ip(), crtp),
                            SocketAddr::new(peer.ip(), crtcp),
                        ));
                    }
                    t.ssrc = Some(outs[n].sender.ssrc());
                    let t = s.accept_setup(id, t)?;
                    outs[n].channels = t.interleaved;
                    outs[n].setup = true;
                }
                ServerEvent::Play { id, uri, .. } => {
                    let base = uri.trim_end_matches('/').to_string();
                    let info: Vec<RtpInfoEntry> = outs
                        .iter()
                        .enumerate()
                        .filter(|(_, o)| o.setup)
                        .map(|(n, o)| RtpInfoEntry {
                            url: format!("{base}/trackID={n}"),
                            seq: Some(o.sender.next_sequence()),
                            rtptime: o.sender.rtp_timestamp(o.first_pts, o.timebase),
                        })
                        .collect();
                    let range = Range::Npt {
                        start: NptTime::Seconds(0.0),
                        end: None,
                    };
                    s.accept_play(id, Some(range), &info)?;
                    play_start = Some(now);
                    eprintln!("PLAY");
                }
                ServerEvent::Teardown { .. } | ServerEvent::TimedOut => {
                    let out = s.take_output();
                    let _ = sock.write_all(&out);
                    break 'outer;
                }
                ServerEvent::Pause { id, .. } => s.accept(id)?,
                _ => {}
            }
        }
        if let Some(ps) = play_start {
            let elapsed = now.duration_since(ps).as_nanos() as i64;
            while next < schedule.len() && schedule[next].0 <= elapsed {
                let (_, n, k) = schedule[next];
                next += 1;
                let o = &mut outs[n];
                if !o.setup {
                    continue;
                }
                let au = &sources[n].aus[k];
                for pkt in o.sender.send(au, o.config.as_ref())? {
                    let b = pkt.to_bytes()?;
                    if let Some((rtp, _, dst, _)) = &o.udp {
                        rtp.send_to(&b, dst)?;
                    } else if let Some((ch, _)) = o.channels {
                        s.send_interleaved(ch, &b)?;
                    }
                }
            }
            let finished = next >= schedule.len();
            if finished && finished_at.is_none() {
                finished_at = Some(now);
                eprintln!("SENT ALL");
            }
            // BYE は最後の RTP から少し後に送る（UDP では RTP と RTCP が別のソケットで、追い越さないように）
            let send_bye = !bye_sent
                && finished_at.is_some_and(|f| now.duration_since(f) >= Duration::from_millis(300));
            if now.duration_since(last_sr) >= Duration::from_millis(500) || send_bye {
                last_sr = now;
                let unix_ns = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as i64;
                let ntp = senca_rtp::clock::unix_ns_to_ntp(unix_ns);
                for o in outs.iter().filter(|o| o.setup) {
                    let media_ns = now.duration_since(ps).as_nanos() as i64;
                    let rtp_now = o
                        .sender
                        .rtp_timestamp(o.first_pts, o.timebase)
                        .unwrap_or(0)
                        .wrapping_add(
                            (media_ns as i128 * o.sender.clock_rate() as i128 / 1_000_000_000)
                                as u32,
                        );
                    let mut pkts = vec![
                        RtcpPacket::SenderReport(o.sender.sender_report(ntp, rtp_now)),
                        RtcpPacket::Sdes(Sdes {
                            chunks: vec![SdesChunk::cname(o.sender.ssrc(), "senca")],
                        }),
                    ];
                    if send_bye {
                        pkts.push(RtcpPacket::Bye(Bye {
                            ssrcs: vec![o.sender.ssrc()],
                            reason: None,
                        }));
                    }
                    let b = senca_rtp::build_compound(&pkts)?;
                    if let Some((_, rtcp, _, dst)) = &o.udp {
                        rtcp.send_to(&b, dst)?;
                    } else if let Some((_, ch)) = o.channels {
                        s.send_interleaved(ch, &b)?;
                    }
                }
                bye_sent |= send_bye;
            }
            if finished_at.is_some_and(|f| now.duration_since(f) > Duration::from_secs(10)) {
                break;
            }
        }
        // UDP の RTCP（受信報告）は読み捨てる
        for o in &outs {
            if let Some((_, rtcp, _, _)) = &o.udp {
                let mut d = [0u8; 1500];
                while rtcp.recv_from(&mut d).is_ok() {
                    s.touch(now);
                }
            }
        }
        let out = s.take_output();
        if !out.is_empty() {
            match sock.write_all(&out) {
                Ok(()) => {}
                Err(_) => break,
            }
        }
        if s.is_closed() {
            break;
        }
        if play_start.is_none() {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    println!("DONE");
    Ok(())
}
