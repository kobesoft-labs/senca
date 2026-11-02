//! 試験用の RTP 受信（UDP、ブロッキング I/O）。SDP ファイル（FFmpeg の `-sdp_file`）の最初の m 行を depacketize してファイルに書く。
//!
//! `rtp_recv <port> <sdp_file> <out_prefix> <idle_secs>`
//!
//! SDP ファイルが現れるまで受けたパケットは溜めておく。RTCP BYE か、最後のパケットから `idle_secs` 秒で終わる。
//! 終わりに `TRACK 0 <codec> <file> <AU 数> <discontinuity 数>` を出す。

mod common;

use common::{Res, Sink, media_format};
use senca_core::media::{CodecConfig, TrackId};
use senca_rtp::rtcp::RtcpPacket;
use senca_rtp::{JitterBuffer, JitterOutput, RtpPacket, TrackReceiver, payload};
use senca_sdp::SessionDescription;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: rtp_recv <port> <sdp_file> <out_prefix> <idle_secs>");
        std::process::exit(2);
    }
    let port: u16 = args[1].parse()?;
    let idle = Duration::from_secs(args[4].parse()?);
    let rtp = UdpSocket::bind(("127.0.0.1", port))?;
    let rtcp = UdpSocket::bind(("127.0.0.1", port + 1))?;
    rtp.set_read_timeout(Some(Duration::from_millis(5)))?;
    rtcp.set_nonblocking(true)?;
    println!("LISTENING {port}");
    let mut pending: Vec<(Vec<u8>, Instant)> = Vec::new();
    let mut state: Option<(u8, TrackReceiver, JitterBuffer, Sink, String)> = None;
    let mut config: Option<CodecConfig> = None;
    let (mut aus, mut disc, mut errors) = (0u64, 0u64, 0u64);
    let mut last = None::<Instant>;
    let started = Instant::now();
    let mut buf = [0u8; 65536];
    let mut bye = false;
    loop {
        let now = Instant::now();
        if state.is_none()
            && let Ok(text) = std::fs::read_to_string(&args[2])
            && let Ok(sd) = SessionDescription::parse(&text)
            && let Some((pt, fmt)) = sd.media.first().and_then(media_format)
        {
            let dep = payload::depacketizer(&fmt)?;
            let (sink, path) = Sink::create(dep.codec(), &args[3], dep.clock_rate())?;
            state = Some((
                pt,
                TrackReceiver::new(TrackId(0), dep),
                JitterBuffer::new(Duration::from_millis(200), 4096),
                sink,
                path,
            ));
        }
        if let Ok((n, _)) = rtp.recv_from(&mut buf) {
            pending.push((buf[..n].to_vec(), now));
            last = Some(now);
        }
        while let Ok((n, _)) = rtcp.recv_from(&mut buf) {
            if let Ok(pkts) = senca_rtp::parse_reduced(&bytes::Bytes::copy_from_slice(&buf[..n])) {
                for p in pkts {
                    match p {
                        RtcpPacket::Bye(_) => bye = true,
                        RtcpPacket::SenderReport(sr) => {
                            if let Some((_, r, _, _, _)) = state.as_mut() {
                                r.on_sender_report(&sr, now);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        let flush_all = bye || last.is_some_and(|l| now.duration_since(l) > idle);
        if let Some((pt, r, jb, sink, _)) = state.as_mut() {
            for (d, at) in pending.drain(..) {
                let Ok(p) = RtpPacket::parse(bytes::Bytes::from(d)) else {
                    continue;
                };
                if p.header.payload_type != *pt {
                    continue;
                }
                r.observe(&p, at);
                jb.push(p, at);
            }
            let t = if flush_all {
                now + Duration::from_secs(10)
            } else {
                now
            };
            while let Some(o) = jb.pop(t) {
                match o {
                    JitterOutput::Packet(p) => {
                        if r.push(&p).is_err() {
                            errors += 1;
                        }
                    }
                    JitterOutput::Lost { .. } => r.mark_loss(),
                    JitterOutput::Reset => r.reset(),
                }
            }
            while let Some((au, cfg)) = r.pop() {
                if let Some(c) = cfg {
                    config = Some(c);
                }
                aus += 1;
                disc += au.discontinuity as u64;
                sink.write(&au, config.as_ref())?;
            }
        }
        if flush_all && state.is_some() {
            break;
        }
        if (last.is_none() && started.elapsed() > Duration::from_secs(30))
            || started.elapsed() > Duration::from_secs(120)
        {
            return Err("timeout".into());
        }
    }
    let (_, r, jb, mut sink, path) = state.ok_or("no SDP")?;
    sink.finish()?;
    let (lost, late, dup) = jb.counters();
    println!(
        "TRACK 0 {} {path} {aus} {disc} lost={lost} late={late} dup={dup} errors={errors}",
        r.codec()
    );
    Ok(())
}
