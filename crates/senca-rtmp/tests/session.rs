//! ServerSession と ClientSession をメモリ上でつなぐ試験。

use bytes::Bytes;
use senca_core::{Codec, CodecConfig};
use senca_flv::script::ScriptData;
use senca_flv::{FlvIngest, IngestEvent, TagType, build};
use senca_rtmp::{
    ClientConfig, ClientEvent, ClientMode, ClientSession, CloseReason, Error, MediaType, Secret,
    ServerConfig, ServerEvent, ServerSession,
};

/// 出力がなくなるまで双方向に流す。
fn pump(c: &mut ClientSession, s: &mut ServerSession) -> (Vec<ClientEvent>, Vec<ServerEvent>) {
    let mut ce = Vec::new();
    let mut se = Vec::new();
    for _ in 0..100 {
        let a = c.take_output();
        let b = s.take_output();
        if a.is_empty() && b.is_empty() {
            break;
        }
        // 細切れにして入れる
        for part in a.chunks(97) {
            s.feed(part).unwrap();
        }
        for part in b.chunks(89) {
            c.feed(part).unwrap();
        }
        while let Some(e) = c.poll_event() {
            ce.push(e);
        }
        while let Some(e) = s.poll_event() {
            se.push(e);
        }
    }
    (ce, se)
}

fn publish_client(key: &str) -> ClientSession {
    ClientSession::new(ClientConfig::new(
        "live",
        "rtmp://127.0.0.1/live",
        ClientMode::Publish {
            stream_key: Secret::new(key),
            publish_type: "live".into(),
        },
    ))
}

const AVCC: &[u8] = &[1, 0x64, 0, 0x1F, 0xFF, 0xE1, 0, 0, 1, 0];

#[test]
fn publish_then_play_end_to_end() {
    // --- publish ---
    let mut c = publish_client("secret-key");
    let mut s = ServerSession::new(ServerConfig::default());
    let (_, se) = pump(&mut c, &mut s);
    let ServerEvent::Connect(info) = &se[0] else {
        panic!("{se:?}")
    };
    assert_eq!(info.app, "live");
    assert_eq!(
        info.tc_url.as_ref().unwrap().expose(),
        "rtmp://127.0.0.1/live"
    );
    s.accept_connect().unwrap();
    let (ce, se) = pump(&mut c, &mut s);
    assert!(ce.contains(&ClientEvent::Connected));
    let ServerEvent::Publish {
        stream_id,
        stream_key,
        publish_type,
    } = &se[0]
    else {
        panic!("{se:?}")
    };
    assert_eq!(stream_key.expose(), "secret-key");
    assert_eq!(publish_type, "live");
    let sid = *stream_id;
    s.accept_publish(sid).unwrap();
    let (ce, _) = pump(&mut c, &mut s);
    assert!(ce.contains(&ClientEvent::PublishStarted { stream_id: sid }));

    // metadata + 映像（大きいフレームで複数 chunk・extended timestamp）
    let meta = ScriptData {
        name: "onMetaData".into(),
        values: vec![senca_amf0::Value::EcmaArray(vec![(
            "width".into(),
            senca_amf0::Value::Number(320.0),
        )])],
    }
    .encode()
    .unwrap();
    c.send_metadata(0, &meta).unwrap();
    let seq = build::video_sequence_start(Codec::H264, Bytes::from_static(AVCC)).unwrap();
    c.send_media(MediaType::Video, 0, &seq.parts()).unwrap();
    let mut frame = vec![0, 0, 0x9C, 0x3C];
    frame.extend(std::iter::repeat_n(0xAB, 0x9C3C));
    let f = build::video_frame(Codec::H264, true, 40, Bytes::from(frame.clone())).unwrap();
    let timestamps = [0u32, 33, 0x00FF_FFFF, 0x0100_0010, 0x0100_0020];
    for ts in timestamps {
        c.send_media(MediaType::Video, ts, &f.parts()).unwrap();
    }
    let asc = build::audio_sequence_start(&CodecConfig {
        codec: Codec::Aac,
        generation: 1,
        record: Bytes::from_static(&[0x12, 0x10]),
        video: None,
        audio: None,
    })
    .unwrap()
    .unwrap();
    c.send_media(MediaType::Audio, 0, &asc.parts()).unwrap();

    let (_, se) = pump(&mut c, &mut s);
    let ServerEvent::Metadata { payload, .. } = &se[0] else {
        panic!("{se:?}")
    };
    assert_eq!(
        ScriptData::parse(payload)
            .unwrap()
            .metadata()
            .unwrap()
            .get_f64("width"),
        Some(320.0)
    );
    let mut ing = FlvIngest::default();
    let mut out = Vec::new();
    let mut media_ts = Vec::new();
    for e in &se[1..] {
        let ServerEvent::Media { stream_id, message } = e else {
            panic!("{e:?}")
        };
        assert_eq!(*stream_id, sid);
        let tt = match message.media_type {
            MediaType::Audio => TagType::Audio,
            MediaType::Video => TagType::Video,
        };
        media_ts.push(message.timestamp);
        ing.push(tt, message.timestamp, &message.payload, &mut out)
            .unwrap();
    }
    assert_eq!(&media_ts[1..6], &timestamps);
    let aus: Vec<_> = out
        .iter()
        .filter_map(|e| match e {
            IngestEvent::AccessUnit(a) => Some(a),
            _ => None,
        })
        .collect();
    assert_eq!(aus.len(), 5);
    assert_eq!(&aus[0].payload[..], &frame[..]);
    assert_eq!(aus[3].pts - aus[3].dts, 40);

    // 終了
    c.close().unwrap();
    let (_, se) = pump(&mut c, &mut s);
    assert_eq!(
        se,
        vec![ServerEvent::StreamClosed {
            stream_id: sid,
            reason: CloseReason::FcUnpublish
        }]
    );

    // --- play ---
    let mut c = ClientSession::new(ClientConfig::new(
        "live",
        "rtmp://127.0.0.1/live",
        ClientMode::Play {
            stream_key: Secret::new("secret-key"),
            start: -2.0,
        },
    ));
    let mut s = ServerSession::new(ServerConfig::default());
    pump(&mut c, &mut s);
    s.accept_connect().unwrap();
    let (_, se) = pump(&mut c, &mut s);
    let ServerEvent::Play {
        stream_id,
        stream_key,
        ..
    } = &se[0]
    else {
        panic!("{se:?}")
    };
    assert_eq!(stream_key.expose(), "secret-key");
    let sid = *stream_id;
    // 受理前は送れない
    assert!(matches!(
        s.send_media(sid, MediaType::Video, 0, &[&[1]]),
        Err(Error::InvalidState(_))
    ));
    s.accept_play(sid).unwrap();
    s.send_data(sid, 0, &meta).unwrap();
    s.send_media(sid, MediaType::Video, 0, &seq.parts())
        .unwrap();
    s.send_media(sid, MediaType::Video, 100, &f.parts())
        .unwrap();
    s.end_play(sid).unwrap();
    let (ce, _) = pump(&mut c, &mut s);
    assert!(ce.contains(&ClientEvent::PlayStarted { stream_id: sid }));
    assert!(ce.iter().any(|e| matches!(e, ClientEvent::Metadata { .. })));
    let media: Vec<_> = ce
        .iter()
        .filter_map(|e| match e {
            ClientEvent::Media { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(media.len(), 2);
    assert_eq!(media[1].timestamp, 100);
    assert_eq!(media[1].payload, f.to_bytes());
    assert!(ce.contains(&ClientEvent::StreamEnded { stream_id: sid }));
}

#[test]
fn rejections() {
    let mut c = publish_client("k");
    let mut s = ServerSession::new(ServerConfig::default());
    pump(&mut c, &mut s);
    s.reject_connect("no").unwrap();
    let (ce, _) = pump(&mut c, &mut s);
    assert!(
        matches!(&ce[0], ClientEvent::Rejected { command, code: Some(code), .. }
        if command == "connect" && code == "NetConnection.Connect.Rejected")
    );

    let mut c = publish_client("bad");
    let mut s = ServerSession::new(ServerConfig::default());
    pump(&mut c, &mut s);
    s.accept_connect().unwrap();
    let (_, se) = pump(&mut c, &mut s);
    let ServerEvent::Publish { stream_id, .. } = se[0] else {
        panic!()
    };
    s.reject_publish(stream_id, "NetStream.Publish.BadName", "denied")
        .unwrap();
    let (ce, _) = pump(&mut c, &mut s);
    assert!(ce.iter().any(|e| matches!(e, ClientEvent::Rejected { code: Some(c), .. } if c == "NetStream.Publish.BadName")));
    assert!(c.send_media(MediaType::Audio, 0, &[&[0xAF, 1]]).is_err());
}

#[test]
fn ping_and_acknowledgement() {
    let mut c = publish_client("k");
    let mut s = ServerSession::new(ServerConfig {
        window_ack_size: 1000,
        ..ServerConfig::default()
    });
    pump(&mut c, &mut s);
    s.accept_connect().unwrap();
    let (_, se) = pump(&mut c, &mut s);
    let ServerEvent::Publish { stream_id, .. } = se[0] else {
        panic!()
    };
    s.accept_publish(stream_id).unwrap();
    pump(&mut c, &mut s);
    s.ping(1234).unwrap();
    let (_, se) = pump(&mut c, &mut s);
    assert!(se.contains(&ServerEvent::PingResponse { timestamp: 1234 }));
    // client は window 1000 ごとに Ack を返すはず。ここではエラーにならないことを確かめる。
    for i in 0..50 {
        c.send_media(MediaType::Audio, i * 23, &[&[0xAF, 1], &[0u8; 300]])
            .unwrap();
    }
    let (_, se) = pump(&mut c, &mut s);
    assert_eq!(se.len(), 50);
}

#[test]
fn garbage_after_handshake_is_rejected_without_panic() {
    let mut x: u64 = 0x1234_5678_9ABC_DEF1;
    for round in 0..500 {
        let mut c = publish_client("k");
        let mut s = ServerSession::new(ServerConfig::default());
        // ハンドシェイクだけ通す
        let c01 = c.take_output();
        s.feed(&c01).unwrap();
        c.feed(&s.take_output()).unwrap();
        let out = c.take_output(); // C2 + set chunk size + connect
        let mut data = out.to_vec();
        // 最初の 1536 バイト（C2）より後ろを壊す
        for _ in 0..(1 + round % 8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = data.len();
            let i = 1536 + (x as usize) % (len - 1536);
            data[i] = (x >> 32) as u8;
        }
        for _ in 0..(round % 50) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            data.push(x as u8);
        }
        let r = s.feed(&data);
        if r.is_err() {
            assert_eq!(s.feed(&[0]), Err(Error::Closed));
        }
        while s.poll_event().is_some() {}
    }
}

#[test]
fn oversized_message_is_rejected() {
    let mut c = publish_client("k");
    let mut s = ServerSession::new(ServerConfig {
        chunk_limits: senca_rtmp::chunk::ChunkLimits {
            max_message_size: 1000,
            ..Default::default()
        },
        ..ServerConfig::default()
    });
    s.feed(&c.take_output()).unwrap();
    c.feed(&s.take_output()).unwrap();
    let mut d = c.take_output()[..1536].to_vec();
    // fmt 0, csid 3, 長さ 1001
    d.extend_from_slice(&[0x03, 0, 0, 0, 0x00, 0x03, 0xE9, 0x14, 0, 0, 0, 0]);
    assert_eq!(s.feed(&d), Err(Error::LimitExceeded("message size")));
}
