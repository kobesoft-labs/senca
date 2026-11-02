//! 試験用の RTP 送信（UDP）。入力（MPEG-TS か IVF）の 1 トラックを senca-rtp の packetizer で送り、SDP を書く。
//!
//! - `rtp_send sdp <input> <track> <port> <sdp_out>`: SDP（senca-sdp で組み立てる）だけを書く。
//! - `rtp_send send <input> <track> <port>`: 127.0.0.1:<port> へ実時間で送り、最後に RTCP の SR と BYE を送る。

mod common;

use common::{Res, SourceTrack, au_time_ns, read_ivf, read_ts};
use senca_rtp::rtcp::{Bye, RtcpPacket};
use senca_rtp::{RtpSender, payload};
use senca_sdp::{
    Connection, Direction, MediaKind, OfferCodec, OfferMedia, OfferMode, OfferOptions, create_offer,
};
use std::net::UdpSocket;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn load(input: &str, track: usize) -> Res<SourceTrack> {
    if input.ends_with(".ivf") {
        return read_ivf(input);
    }
    read_ts(input)?
        .into_iter()
        .nth(track)
        .ok_or_else(|| "no such track".into())
}

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!("usage: rtp_send (sdp|send) <input> <track> <port> [sdp_out]");
        std::process::exit(2);
    }
    let src = load(&args[2], args[3].parse()?)?;
    let port: u16 = args[4].parse()?;
    let (pz, fmt) = payload::packetizer(src.codec, src.config.as_ref(), 1200)?;
    let pt = payload::static_payload_type(src.codec).unwrap_or(96);
    if args[1] == "sdp" {
        let media = OfferMedia {
            kind: if src.codec.kind() == senca_core::MediaKind::Video {
                MediaKind::Video
            } else {
                MediaKind::Audio
            },
            mid: "0".into(),
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
            control: None,
            datachannel: false,
        };
        let mut sd = create_offer(
            &[media],
            &OfferOptions {
                mode: OfferMode::Rtsp,
                session_name: "SENCA".into(),
                origin_address: "127.0.0.1".into(),
                ..Default::default()
            },
        )?;
        // RTSP 用の offer を素の RTP 用にする（宛先とポート、control を外す）
        sd.connection = Some(Connection::ip4("127.0.0.1"));
        sd.attributes
            .retain(|a| a.name != "control" && a.name != "range");
        for m in &mut sd.media {
            m.port = port;
            m.attributes.retain(|a| a.name != "control");
        }
        std::fs::write(args.get(5).ok_or("sdp_out")?, sd.to_string())?;
        return Ok(());
    }
    let sock = UdpSocket::bind("127.0.0.1:0")?;
    let seed = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64;
    let mut tx = RtpSender::new(
        pz,
        pt,
        (seed >> 7) as u32 | 1,
        (seed >> 3) as u16,
        (seed >> 11) as u32,
    );
    let t0 = src.aus.first().map(au_time_ns).unwrap_or(0);
    let start = Instant::now();
    let mut packets = 0u64;
    for au in &src.aus {
        let due = Duration::from_nanos((au_time_ns(au) - t0).max(0) as u64);
        let el = start.elapsed();
        if due > el {
            std::thread::sleep(due - el);
        }
        for p in tx.send(au, src.config.as_ref())? {
            sock.send_to(&p.to_bytes()?, ("127.0.0.1", port))?;
            packets += 1;
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    let ntp = senca_rtp::clock::unix_ns_to_ntp(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as i64,
    );
    let last = src
        .aus
        .last()
        .map(|a| tx.rtp_timestamp(a.pts, a.timebase).unwrap_or(0))
        .unwrap_or(0);
    let rtcp = senca_rtp::build_compound(&[
        RtcpPacket::SenderReport(tx.sender_report(ntp, last)),
        RtcpPacket::Bye(Bye {
            ssrcs: vec![tx.ssrc()],
            reason: None,
        }),
    ])?;
    for _ in 0..3 {
        sock.send_to(&rtcp, ("127.0.0.1", port + 1))?;
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("SENT {} AUs {packets} packets", src.aus.len());
    Ok(())
}
