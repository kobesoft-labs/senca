//! 同じプロセスの中の RTMP サーバー・SRT listener（`senca-rtmp`・`senca-srt` の sans-io 実装）へ送り、
//! 届いたフレームが送ったものと同じか、切れたらつなぎ直してキーフレームから送るかを確かめる。

use bytes::Bytes;
use senca_core::{AccessUnit, MediaKind};
use senca_flv::{FlvIngest, IngestEvent, TagType};
use senca_mpegts::{DemuxEvent, DemuxerConfig, TsDemuxer};
use senca_rtmp::{MediaType, ServerConfig, ServerEvent, ServerSession};
use senca_sdk::frame::{AudioCodec, AudioFormat, AudioFrame, VideoCodec, VideoFormat, VideoFrame};
use senca_sdk::{Event, Sender, SenderConfig, SenderState};
use senca_srt::{
    AcceptDecision, AcceptParams, Event as SrtEvent, Listener, ListenerConfig, Passphrase,
};
use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SPS: &[u8] = &[
    0x67, 0x42, 0xC0, 0x0D, 0xD9, 0x01, 0x41, 0xFB, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00,
    0x00, 0x03, 0x03, 0xC0, 0xF1, 0x42, 0xA4, 0x80,
];
const PPS: &[u8] = &[0x68, 0xCB, 0x83, 0xCB, 0x20];
const GOP: usize = 10;

/// i 番目の映像フレームのスライス NAL（中身で番号が分かる）。
fn slice(i: usize) -> Vec<u8> {
    let key = i.is_multiple_of(GOP);
    vec![
        if key { 0x65 } else { 0x41 },
        0x80 | (i / 100) as u8,
        0x80 | (i % 100) as u8,
        0x55,
        0x55,
        0x55,
    ]
}

fn annexb(i: usize) -> Vec<u8> {
    let mut v = Vec::new();
    let s = slice(i);
    let nals: Vec<&[u8]> = if i.is_multiple_of(GOP) {
        vec![SPS, PPS, &s]
    } else {
        vec![&s]
    };
    for n in nals {
        v.extend_from_slice(&[0, 0, 0, 1]);
        v.extend_from_slice(n);
    }
    v
}

fn adts(i: usize) -> Vec<u8> {
    let asc = senca_aac::AudioSpecificConfig::new(2, 48000, 2).unwrap();
    senca_aac::wrap_adts(&asc, &[0x21, (i % 200) as u8 + 1, 0x33]).unwrap()
}

/// 受け取った映像 AU のスライス NAL。
fn slices(au: &AccessUnit) -> Vec<Vec<u8>> {
    senca_h264::length_prefixed_nals(&au.payload, 4)
        .unwrap()
        .map(|n| n.unwrap())
        .filter(|n| matches!(n[0] & 0x1F, 1 | 5))
        .map(|n| n.to_vec())
        .collect()
}

/// 映像 `n` フレーム（30 fps の時刻、`pace` ごと）と音声を送る。
fn feed(s: &Sender, n: usize, pace: Duration) {
    for i in 0..n {
        let t = i as i64 * 33_333;
        let data = annexb(i);
        s.push_video(&VideoFrame {
            codec: VideoCodec::H264,
            format: VideoFormat::AnnexB,
            data: &data,
            pts_us: t,
            dts_us: t,
            keyframe: None,
        })
        .unwrap();
        let a = adts(i);
        s.push_audio(&AudioFrame {
            codec: AudioCodec::Aac,
            format: AudioFormat::Adts,
            data: &a,
            pts_us: t,
        })
        .unwrap();
        std::thread::sleep(pace);
    }
}

fn check_video(aus: &[AccessUnit]) -> Vec<usize> {
    let v: Vec<&AccessUnit> = aus
        .iter()
        .filter(|a| a.kind() == MediaKind::Video)
        .collect();
    assert!(!v.is_empty());
    assert!(v[0].keyframe, "first video AU must be a keyframe");
    v.iter()
        .map(|a| {
            let s = slices(a);
            assert_eq!(s.len(), 1);
            let i = ((s[0][1] & 0x7F) as usize) * 100 + (s[0][2] & 0x7F) as usize;
            assert_eq!(s[0], slice(i), "payload of frame {i}");
            assert_eq!(a.keyframe, i.is_multiple_of(GOP));
            i
        })
        .collect()
}

#[test]
fn rtmp_publish_reconnects_and_resumes_at_keyframe() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let mut per_conn: Vec<Vec<AccessUnit>> = Vec::new();
        for conn in 0..2 {
            let (mut sock, _) = l.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut s = ServerSession::new(ServerConfig::default());
            let mut ingest = FlvIngest::default();
            let mut aus = Vec::new();
            let mut ev_out = Vec::new();
            let mut buf = vec![0u8; 65536];
            'conn: loop {
                let n = match sock.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                s.feed(&buf[..n]).unwrap();
                while let Some(ev) = s.poll_event() {
                    match ev {
                        ServerEvent::Connect(_) => s.accept_connect().unwrap(),
                        ServerEvent::Publish {
                            stream_id,
                            stream_key,
                            ..
                        } => {
                            assert_eq!(stream_key.expose(), "key1");
                            s.accept_publish(stream_id).unwrap();
                        }
                        ServerEvent::Media { message, .. } => {
                            let tt = if message.media_type == MediaType::Video {
                                TagType::Video
                            } else {
                                TagType::Audio
                            };
                            ingest
                                .push(tt, message.timestamp, &message.payload, &mut ev_out)
                                .unwrap();
                            for e in ev_out.drain(..) {
                                if let IngestEvent::AccessUnit(a) = e {
                                    aus.push(a);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                sock.write_all(&s.take_output()).unwrap();
                // 1 本目は映像 15 フレームで切る
                if conn == 0 && aus.iter().filter(|a| a.kind() == MediaKind::Video).count() >= 15 {
                    break 'conn;
                }
            }
            per_conn.push(aus);
        }
        per_conn
    });

    let mut cfg = SenderConfig::from_uri(&format!("rtmp://127.0.0.1:{port}/live/key1")).unwrap();
    cfg.reconnect_initial = Duration::from_millis(50);
    let s = Sender::new(cfg).unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let e2 = events.clone();
    s.set_event_handler(move |e| e2.lock().unwrap().push(e.clone()))
        .unwrap();
    s.start().unwrap();
    feed(&s, 90, Duration::from_millis(10));
    s.stop();
    let st = s.stats();
    let per_conn = server.join().unwrap();
    assert_eq!(per_conn.len(), 2);
    let a = check_video(&per_conn[0]);
    let b = check_video(&per_conn[1]);
    assert_eq!(a[..15], (0..15).collect::<Vec<_>>()[..]);
    // 2 本目はキーフレームから、最後まで欠けずに
    assert_eq!(b.last(), Some(&89));
    assert!(b.windows(2).all(|w| w[1] == w[0] + 1), "{b:?}");
    // 音声も届く（ADTS を外した raw）
    assert!(
        per_conn[1]
            .iter()
            .any(|a| a.kind() == MediaKind::Audio && a.payload.len() == 3)
    );
    assert!(st.reconnects >= 1, "{st:?}");
    assert!(st.bytes_sent > 0);
    let ev = events.lock().unwrap();
    assert!(ev.iter().any(|e| matches!(
        e,
        Event::StateChanged {
            state: SenderState::Reconnecting,
            ..
        }
    )));
    assert_eq!(
        ev.last(),
        Some(&Event::StateChanged {
            state: SenderState::Stopped,
            reason: None
        })
    );
}

/// SRT listener を回す。`passphrase` が違えば拒否される。
fn srt_server(
    passphrase: &str,
    run_for: Duration,
) -> (u16, std::thread::JoinHandle<Vec<AccessUnit>>) {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = sock.local_addr().unwrap().port();
    let pp = Passphrase::new(passphrase).unwrap();
    let h = std::thread::spawn(move || {
        let start = Instant::now();
        let now = || start.elapsed().as_micros() as u64 + 1;
        let mut l = Listener::new(ListenerConfig::default(), move |req| {
            if req.stream_id == Some("publish:sk_test") {
                AcceptDecision::Accept(AcceptParams {
                    passphrase: Some(pp.clone()),
                    ..Default::default()
                })
            } else {
                AcceptDecision::Reject(senca_srt::RejectReason::X_FORBIDDEN)
            }
        })
        .unwrap();
        let mut demux = TsDemuxer::new(DemuxerConfig::default());
        let mut aus = Vec::new();
        let mut buf = vec![0u8; 2048];
        let mut closed = false;
        while start.elapsed() < run_for && !closed {
            let wait = l
                .poll_timeout()
                .map_or(5_000, |t| t.saturating_sub(now()))
                .clamp(200, 5_000);
            sock.set_read_timeout(Some(Duration::from_micros(wait)))
                .unwrap();
            if let Ok((n, from)) = sock.recv_from(&mut buf) {
                l.handle_datagram(now(), from, &buf[..n]);
            }
            l.handle_timeout(now());
            while let Some((to, p)) = l.poll_transmit() {
                let _ = sock.send_to(&p, to);
            }
            while let Some((_, ev)) = l.poll_event() {
                match ev {
                    SrtEvent::Data { payload, .. } => {
                        demux.push(&payload);
                        while let Some(e) = demux.poll() {
                            if let DemuxEvent::AccessUnit(a) = e {
                                aus.push(a);
                            }
                        }
                    }
                    SrtEvent::Closed { .. } => closed = true,
                    SrtEvent::Connected(_) => {}
                }
            }
        }
        demux.flush();
        while let Some(e) = demux.poll() {
            if let DemuxEvent::AccessUnit(a) = e {
                aus.push(a);
            }
        }
        aus
    });
    (port, h)
}

#[test]
fn srt_publish_encrypted_delivers_all_frames() {
    let (port, server) = srt_server("0123456789abcdef", Duration::from_secs(20));
    let uri = format!(
        "senca://ingest?v=1&proto=srt&host=127.0.0.1&port={port}&streamid=publish:sk_test&passphrase=0123456789abcdef&latency=120&pbkeylen=32"
    );
    let s = Sender::new(SenderConfig::from_uri(&uri).unwrap()).unwrap();
    s.start().unwrap();
    let t = Instant::now();
    while s.state() != SenderState::Connected {
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "not connected: {:?}",
            s.state()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    feed(&s, 60, Duration::from_millis(5));
    // RTT が測れるまで少し待つ
    std::thread::sleep(Duration::from_millis(300));
    let st = s.stats();
    s.stop();
    let aus = server.join().unwrap();
    let v = check_video(&aus);
    assert_eq!(v, (0..60).collect::<Vec<_>>());
    let audio: Vec<&AccessUnit> = aus
        .iter()
        .filter(|a| a.kind() == MediaKind::Audio)
        .collect();
    assert_eq!(audio.len(), 60);
    assert_eq!(audio[5].payload, Bytes::from_static(&[0x21, 6, 0x33]));
    assert!(st.rtt_us.is_some(), "{st:?}");
    assert_eq!(st.video_frames_dropped, 0);
}

#[test]
fn srt_wrong_passphrase_fails_without_retry() {
    let (port, server) = srt_server("another-passphrase", Duration::from_secs(3));
    let uri =
        format!("srt://127.0.0.1:{port}?streamid=publish:sk_test&passphrase=0123456789abcdef");
    let s = Sender::new(SenderConfig::from_uri(&uri).unwrap()).unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let e2 = events.clone();
    s.set_event_handler(move |e| e2.lock().unwrap().push(e.clone()))
        .unwrap();
    s.start().unwrap();
    let t = Instant::now();
    while s.state() != SenderState::Failed && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    s.stop();
    assert_eq!(s.state(), SenderState::Failed);
    let ev = events.lock().unwrap();
    let reason = ev.iter().find_map(|e| match e {
        Event::StateChanged {
            state: SenderState::Failed,
            reason,
        } => reason.clone(),
        _ => None,
    });
    assert!(
        reason
            .as_deref()
            .is_some_and(|r| r.contains("rejected") && !r.contains("0123456789")),
        "{reason:?}"
    );
    let _ = server.join();
}
