//! fuzz の種を作る。`senca-fuzz-seedgen <corpus_dir>` で `<corpus_dir>/<target>/` に書く。
//!
//! 種の形は fuzz/fuzz_targets/*.rs の入力の形に合わせる（先頭の選択バイト、「長さ 2 バイト・データグラム」の並びなど）。
//! 自前の client と server を繋いで、client 側が送ったバイト列を記録する。

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use ed25519_dalek::{Signer, SigningKey};
use senca_core::{Codec, CodecConfig};

struct Out {
    root: PathBuf,
    count: usize,
}

impl Out {
    fn put(&mut self, target: &str, name: &str, data: &[u8]) {
        let dir = self.root.join(target);
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join(format!("seedgen-{name}")), data).expect("write");
        self.count += 1;
    }
}

/// 「長さ 2 バイト・データグラム」の並びにする。
fn frame_datagrams(sel: Option<u8>, dgs: &[Vec<u8>]) -> Vec<u8> {
    let mut v = Vec::new();
    if let Some(s) = sel {
        v.push(s);
    }
    for d in dgs {
        let n = d.len().min(0xffff);
        v.extend_from_slice(&(n as u16).to_be_bytes());
        v.extend_from_slice(&d[..n]);
    }
    v
}

// ---------------------------------------------------------------- RTMP

fn rtmp(out: &mut Out) {
    use senca_rtmp::{
        ClientConfig, ClientMode, ClientSession, MediaType, Secret, ServerConfig, ServerEvent, ServerSession,
    };
    const AVCC: &[u8] = &[1, 0x64, 0, 0x1F, 0xFF, 0xE1, 0, 4, 0x67, 0x64, 0, 0x1f, 1, 0, 4, 0x68, 0xee, 0x3c, 0x80];

    for (name, mode) in [
        (
            "publish",
            ClientMode::Publish {
                stream_key: Secret::new("live/key?token=abc"),
                publish_type: "live".into(),
            },
        ),
        (
            "play",
            ClientMode::Play {
                stream_key: Secret::new("live/key"),
                start: -2.0,
            },
        ),
    ] {
        let publish = matches!(mode, ClientMode::Publish { .. });
        let mut c = ClientSession::new(ClientConfig::new("live", "rtmp://127.0.0.1/live", mode));
        let mut s = ServerSession::new(ServerConfig::default());
        let mut sent = Vec::new();
        let mut media_sent = false;
        for _ in 0..200 {
            let a = c.take_output();
            sent.extend_from_slice(&a);
            let b = s.take_output();
            if a.is_empty() && b.is_empty() {
                if publish && !media_sent && c.active_stream().is_some() {
                    media_sent = true;
                    let meta = senca_flv::script::ScriptData {
                        name: "onMetaData".into(),
                        values: vec![senca_amf0::Value::EcmaArray(vec![
                            ("width".into(), senca_amf0::Value::Number(320.0)),
                            ("videocodecid".into(), senca_amf0::Value::Number(7.0)),
                        ])],
                    }
                    .encode()
                    .unwrap();
                    c.send_metadata(0, &meta).unwrap();
                    let seq = senca_flv::build::video_sequence_start(Codec::H264, Bytes::from_static(AVCC)).unwrap();
                    c.send_media(MediaType::Video, 0, &seq.parts()).unwrap();
                    let asc = CodecConfig {
                        codec: Codec::Aac,
                        generation: 1,
                        record: Bytes::from_static(&[0x12, 0x10]),
                        video: None,
                        audio: None,
                    };
                    if let Some(a) = senca_flv::build::audio_sequence_start(&asc).unwrap() {
                        c.send_media(MediaType::Audio, 0, &a.parts()).unwrap();
                    }
                    let mut frame = vec![0, 0, 0x10, 0x00, 0x65];
                    frame.extend(std::iter::repeat_n(0xAB, 0x0fff));
                    for ts in [0u32, 33, 0x00FF_FFFF, 0x0100_0010] {
                        let f = senca_flv::build::video_frame(Codec::H264, ts == 0, 0, Bytes::from(frame.clone()))
                            .unwrap();
                        c.send_media(MediaType::Video, ts, &f.parts()).unwrap();
                    }
                    let af = senca_flv::build::audio_frame(Codec::Aac, Some(&asc), Bytes::from_static(&[0x21; 64]))
                        .unwrap();
                    c.send_media(MediaType::Audio, 23, &af.parts()).unwrap();
                    c.ping(1).unwrap();
                    c.close().unwrap();
                    continue;
                }
                break;
            }
            for part in a.chunks(97) {
                if s.feed(part).is_err() {
                    break;
                }
            }
            let _ = c.feed(&b);
            while let Some(e) = s.poll_event() {
                let _ = match e {
                    ServerEvent::Connect(_) => s.accept_connect(),
                    ServerEvent::Publish { stream_id, .. } => s.accept_publish(stream_id),
                    ServerEvent::Play { stream_id, .. } => s.accept_play(stream_id),
                    _ => Ok(()),
                };
            }
            while c.poll_event().is_some() {}
        }
        // 先頭のバイトは区切りの大きさ（rtmp_server.rs）。0 なら 1 バイトずつなので大きめにする
        let mut v = vec![200u8];
        v.extend_from_slice(&sent);
        out.put("rtmp_server", name, &v);
        // AMF0 の種: connect の command を抜き出すのは手間なので、典型的な値を直接符号化する
    }
    let connect = [
        senca_amf0::Value::String("connect".into()),
        senca_amf0::Value::Number(1.0),
        senca_amf0::Value::Object(vec![
            ("app".into(), senca_amf0::Value::String("live".into())),
            ("tcUrl".into(), senca_amf0::Value::String("rtmp://127.0.0.1/live".into())),
            ("fpad".into(), senca_amf0::Value::Boolean(false)),
            ("capabilities".into(), senca_amf0::Value::Number(15.0)),
            ("fourCcList".into(), senca_amf0::Value::StrictArray(vec![senca_amf0::Value::String("av01".into())])),
        ]),
        senca_amf0::Value::Null,
    ];
    out.put("amf0", "connect", &senca_amf0::encode_all(connect.iter()).unwrap());
}

// ---------------------------------------------------------------- STUN

fn stun(out: &mut Out) {
    use senca_stun::{Attribute, MessageBuilder};
    use senca_stun::message::{Class, Method, TransactionId};
    let tid = TransactionId([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    let key = senca_stun::short_term_key("password");
    let addr: SocketAddr = "192.0.2.1:3478".parse().unwrap();
    let addr6: SocketAddr = "[2001:db8::1]:3478".parse().unwrap();
    let msgs = [
        (
            "binding-ice",
            MessageBuilder::new(Class::Request, Method::BINDING, tid)
                .attr(Attribute::Username("abcd:efgh".into()))
                .attr(Attribute::Priority(0x6e7f_1eff))
                .attr(Attribute::IceControlling(0x1122_3344_5566_7788))
                .attr(Attribute::UseCandidate)
                .integrity(&key)
                .fingerprint()
                .build(),
        ),
        (
            "binding-success",
            MessageBuilder::new(Class::Success, Method::BINDING, tid)
                .attr(Attribute::XorMappedAddress(addr))
                .attr(Attribute::XorMappedAddress(addr6))
                .attr(Attribute::Software("senca".into()))
                .fingerprint()
                .build(),
        ),
        (
            "allocate",
            MessageBuilder::new(Class::Request, Method::ALLOCATE, tid)
                .attr(Attribute::RequestedTransport(17))
                .attr(Attribute::Lifetime(600))
                .attr(Attribute::Realm("example.org".into()))
                .attr(Attribute::Nonce("obMatJos2AAACf//499k954d6OL34oL9FSTvy64sA".into()))
                .attr(Attribute::Username("user".into()))
                .integrity_sha256(&key)
                .build(),
        ),
        (
            "error",
            MessageBuilder::new(Class::Error, Method::ALLOCATE, tid)
                .attr(Attribute::ErrorCode { code: 401, reason: "Unauthorized".into() })
                .attr(Attribute::UnknownAttributes(vec![0x0001, 0x7fff]))
                .attr(Attribute::Unknown { typ: 0x8888, value: Bytes::from_static(b"xyz") })
                .build(),
        ),
    ];
    for (name, m) in msgs {
        if let Ok(b) = m {
            out.put("stun", name, &b);
        }
    }
}

// ---------------------------------------------------------------- SRT

fn srt(out: &mut Out) {
    use senca_srt::{AcceptDecision, AcceptParams, Config, Connection, Listener, ListenerConfig, Passphrase};
    for (name, pass) in [("plain", false), ("aes", true)] {
        let la: SocketAddr = "192.0.2.1:9000".parse().unwrap();
        let ca: SocketAddr = "192.0.2.10:40000".parse().unwrap();
        let mut l = Listener::new(ListenerConfig::default(), move |_r| {
            let mut p = AcceptParams::default();
            if pass {
                p.passphrase = Passphrase::new("fuzz-passphrase-0123").ok();
            }
            AcceptDecision::Accept(p)
        })
        .unwrap();
        let mut cfg = Config::default();
        cfg.stream_id = Some("#!::r=live/cam1,m=publish".into());
        if pass {
            cfg.passphrase = Passphrase::new("fuzz-passphrase-0123").ok();
        }
        let mut now: u64 = 1_000_000_000;
        let mut c = Connection::connect(cfg, la, now).unwrap();
        let mut sent: Vec<Vec<u8>> = Vec::new();
        let mut payload_sent = 0;
        for _ in 0..400 {
            now += 10_000;
            let mut moved = false;
            while let Some((_, d)) = c.poll_transmit() {
                sent.push(d.to_vec());
                l.handle_datagram(now, ca, &d);
                moved = true;
            }
            while let Some((_, d)) = l.poll_transmit() {
                c.handle_datagram(now, la, &d);
                moved = true;
            }
            while c.poll_event().is_some() {}
            while l.poll_event().is_some() {}
            if c.is_connected() && payload_sent < 20 {
                let _ = c.send(&[0x47; 1316], now);
                payload_sent += 1;
                moved = true;
            }
            if payload_sent >= 20 && !moved {
                break;
            }
            c.handle_timeout(now);
            l.handle_timeout(now);
        }
        c.close(now);
        while let Some((_, d)) = c.poll_transmit() {
            sent.push(d.to_vec());
        }
        let sel = if pass { 1 } else { 0 };
        out.put("srt_listener", name, &frame_datagrams(Some(sel), &sent));
        for (i, d) in sent.iter().take(8).enumerate() {
            out.put("srt_packet", &format!("{name}-{i}"), d);
        }
    }
    out.put("srt_packet", "streamid", b"#!::r=live/cam1,m=publish,u=admin,t=stream");
}

// ---------------------------------------------------------------- DTLS

fn dtls(out: &mut Out) {
    use senca_dtls::{DtlsCertificate, DtlsConfig, DtlsEndpoint};
    let cc = DtlsCertificate::generate_self_signed("client").unwrap();
    let sc = DtlsCertificate::generate_self_signed("server").unwrap();
    let mut now = Instant::now();
    let mut c = DtlsEndpoint::new(DtlsConfig::client(cc), now).unwrap();
    let mut scfg = DtlsConfig::server(sc);
    scfg.require_peer_certificate = false;
    let mut s = DtlsEndpoint::new(scfg, now).unwrap();
    let mut sent = Vec::new();
    for _ in 0..50 {
        now += Duration::from_millis(5);
        let mut moved = false;
        while let Some(d) = c.poll_transmit() {
            let _ = s.handle_datagram(&d, now);
            sent.push(d);
            moved = true;
        }
        while let Some(d) = s.poll_transmit() {
            let _ = c.handle_datagram(&d, now);
            moved = true;
        }
        while c.poll_event().is_some() {}
        while s.poll_event().is_some() {}
        if c.is_connected() && !moved {
            break;
        }
    }
    if c.is_connected() {
        let _ = c.send(b"datachannel-or-app-data");
        c.close();
        while let Some(d) = c.poll_transmit() {
            sent.push(d);
        }
    }
    out.put("dtls_server", "handshake", &frame_datagrams(Some(0), &sent));
    if let Some(first) = sent.first() {
        out.put("dtls_server", "client-hello", &frame_datagrams(Some(0), std::slice::from_ref(first)));
    }
}

// ---------------------------------------------------------------- TLS

fn tls(out: &mut Out, tls_data: &Path) {
    use senca_tls::{CertifiedKey, ClientConfig, Connection, RootStore, ServerConfig, ServerVerifier, TlsSession};
    let rd = |f: &str| fs::read(tls_data.join(f)).unwrap_or_else(|e| panic!("{f}: {e}"));
    let mut roots = RootStore::new();
    roots.add_pem(&rd("ca.pem")).unwrap();
    let roots = Arc::new(roots);
    for (sel, chain, key, alpn) in [
        (0xF0u8, "server-ecdsa.pem", "server-ecdsa.key", true),
        (0xF1u8, "server-rsa.pem", "server-rsa.key", false),
        (0xF2u8, "server-ed25519.pem", "server-ed25519.key", false),
    ] {
        let ck = Arc::new(CertifiedKey::from_pem(&rd(chain), &rd(key)).unwrap());
        let mut scfg = ServerConfig::new(ck).unwrap();
        scfg.alpn_protocols = vec![b"http/1.1".to_vec(), b"h3".to_vec()];
        let mut ccfg = ClientConfig::new(ServerVerifier::WebPki { roots: roots.clone(), spki_pins: Vec::new() });
        if alpn {
            ccfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        }
        let mut c = Connection::client(Arc::new(ccfg), "localhost", SystemTime::now()).unwrap();
        let mut s = Connection::server(Arc::new(scfg), SystemTime::now()).unwrap();
        let mut sent = Vec::new();
        let mut wrote = false;
        for _ in 0..64 {
            let mut moved = false;
            if c.wants_write() {
                let d = c.take_tls();
                let _ = s.read_tls(&d);
                sent.extend_from_slice(&d);
                moved = true;
            }
            if s.wants_write() {
                let d = s.take_tls();
                let _ = c.read_tls(&d);
                moved = true;
            }
            while c.take_plaintext().is_some() {}
            while s.take_plaintext().is_some() {}
            if !c.is_handshaking() && !wrote {
                let _ = c.write_plaintext(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
                c.send_close_notify();
                wrote = true;
                moved = true;
            }
            if !moved {
                break;
            }
        }
        let mut v = vec![sel];
        v.extend_from_slice(&sent);
        out.put("tls_server", chain, &v);
    }
}

// ---------------------------------------------------------------- JWT

fn jwt(out: &mut Out) {
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let mk = |h: &str, c: &str| {
        let h = URL_SAFE_NO_PAD.encode(h);
        let p = URL_SAFE_NO_PAD.encode(c);
        let sig = sk.sign(format!("{h}.{p}").as_bytes());
        format!("{h}.{p}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
    };
    let good_h = r#"{"alg":"EdDSA","kid":"k1","typ":"JWT"}"#;
    let good_c = r#"{"iss":"iss1","aud":["senca","x"],"exp":1900000000,"nbf":1700000000,"iat":1700000000,"sub":"u1","tenant":"t1","scope":"stream:read stream:write admin"}"#;
    let mut v = vec![0u8];
    v.extend_from_slice(mk(good_h, good_c).as_bytes());
    out.put("jwt", "token-valid", &v);
    let mut v = vec![0u8];
    v.extend_from_slice(mk(r#"{"alg":"none"}"#, good_c).as_bytes());
    out.put("jwt", "token-alg-none", &v);
    let mut v = vec![2u8];
    v.extend_from_slice(good_h.as_bytes());
    v.push(0);
    v.extend_from_slice(good_c.as_bytes());
    out.put("jwt", "signed-claims", &v);
    let x = URL_SAFE_NO_PAD.encode(sk.verifying_key().to_bytes());
    let jwks = format!(
        r#"{{"keys":[{{"kty":"OKP","crv":"Ed25519","kid":"k1","x":"{x}","alg":"EdDSA","use":"sig"}},{{"kty":"EC","crv":"P-256","kid":"e1","x":"{x}","y":"{x}","alg":"ES256"}},{{"kty":"RSA","kid":"r1","n":"0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw","e":"AQAB","alg":"RS256"}}]}}"#
    );
    let mut v = vec![1u8];
    v.extend_from_slice(jwks.as_bytes());
    out.put("jwt", "jwks", &v);
}

// ---------------------------------------------------------------- Raft

fn raft(out: &mut Out) {
    use senca_raft::codec::encode_message;
    use senca_raft::{Message, MsgBody};
    use senca_raft::{ConfState, Entry, EntryKind, SnapshotMeta};
    let e = |i: u64, k: EntryKind, d: &'static [u8]| Entry { term: 3, index: i, kind: k, data: Bytes::from_static(d) };
    let bodies = vec![
        MsgBody::PreVote { last_index: 10, last_term: 2 },
        MsgBody::PreVoteResp { reject: false },
        MsgBody::Vote { last_index: 10, last_term: 2, transfer: true },
        MsgBody::VoteResp { reject: true },
        MsgBody::Append {
            prev_index: 9,
            prev_term: 2,
            entries: vec![e(10, EntryKind::Normal, b"set a=1"), e(11, EntryKind::ConfChange, b"")],
            commit: 9,
        },
        MsgBody::AppendResp { reject: true, index: 9, reject_hint: 5, hint_term: 1 },
        MsgBody::Heartbeat { commit: 9, read_seq: 4 },
        MsgBody::HeartbeatResp { read_seq: 4 },
        MsgBody::Snapshot {
            meta: SnapshotMeta { index: 100, term: 3, conf_state: ConfState::new([1, 2, 3]) },
            total_len: 1000,
            offset: 0,
            data: Bytes::from_static(&[0x55; 64]),
            done: false,
        },
        MsgBody::SnapshotResp { snap_index: 100, snap_term: 3, next_offset: 64, reject: false },
        MsgBody::TimeoutNow,
    ];
    let mut stream = Vec::new();
    for (i, body) in bodies.into_iter().enumerate() {
        let m = Message { from: 1, to: 2, term: 3, body };
        let enc = encode_message(&m);
        // decode_message_body は長さの後ろ。FrameDecoder の種には長さ付きを並べる
        out.put("raft", &format!("body-{i}"), &enc[4..]);
        stream.extend_from_slice(&enc);
    }
    out.put("raft", "stream", &stream);
}

// ---------------------------------------------------------------- 画像

fn images(out: &mut Out) {
    use senca_image::{Plane, jpeg, png};
    let p = Plane::from_fn(33, 17, |x, y| ((x * 7) ^ (y * 13)) as u8).unwrap();
    let rgb: Vec<u8> = (0..(19 * 11 * 3)).map(|i| (i * 37 % 251) as u8).collect();
    let rgba: Vec<u8> = (0..(8 * 8 * 4)).map(|i| (i * 11 % 256) as u8).collect();
    if let Ok(b) = png::encode_png_gray(&p, 6) {
        out.put("png", "gray", &b);
    }
    if let Ok(b) = png::encode_png(19, 11, png::PngColor::Rgb, &rgb, 1) {
        out.put("png", "rgb", &b);
    }
    if let Ok(b) = png::encode_png(8, 8, png::PngColor::Rgba, &rgba, 9) {
        out.put("png", "rgba", &b);
    }
    if let Ok(b) = jpeg::encode_jpeg_gray(&p, 80) {
        out.put("jpeg", "gray", &b);
    }
    let y = Plane::from_fn(16, 16, |x, y| (x * 16 + y) as u8).unwrap();
    let cb = Plane::from_fn(16, 16, |x, _| (x * 8) as u8).unwrap();
    let cr = Plane::from_fn(16, 16, |_, y| (y * 8) as u8).unwrap();
    if let Ok(b) = jpeg::encode_yuv444(&y, &cb, &cr, 50) {
        out.put("jpeg", "yuv444", &b);
    }
}

// ---------------------------------------------------------------- 録画の索引

fn record(out: &mut Out) {
    use senca_record::{Entry, SCHEMA, SegmentRef, encode};
    let mut v = Vec::new();
    v.extend(encode(&Entry::Begin {
        schema: SCHEMA,
        recording: "rec1".into(),
        stream: "s1".into(),
        container: "fmp4".into(),
        started_at_ms: 1_700_000_000_000,
        not_before_ms: Some(1_700_000_000_000),
    }));
    v.extend(encode(&Entry::Init { path: "init.mp4".into(), sha256: "00".repeat(32) }));
    for i in 0..3u64 {
        v.extend(encode(&Entry::Segment(SegmentRef {
            path: format!("seg/{i}.m4s"),
            epoch: 1,
            sequence: i,
            duration_ms: 2000,
            bytes: 12345,
            sha256: "ab".repeat(32),
            wall_ms: 1_700_000_000_000 + i as i64 * 2000,
            discontinuity: i == 2,
        })));
    }
    v.extend(encode(&Entry::Gap { at_ms: 1_700_000_006_000, reason: "source lost".into(), dropped: 3 }));
    v.extend(encode(&Entry::End { ended_at_ms: 1_700_000_010_000, state: "complete".into(), not_after_ms: None }));
    out.put("record", "index", &v);
    // 最後の行が書きかけのもの
    v.extend_from_slice(br#"{"t":"segment","path":"x"#);
    out.put("record", "truncated", &v);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("usage: senca-fuzz-seedgen <corpus_dir> <senca-tls/tests/data>"));
    let tls_data = PathBuf::from(args.next().expect("usage: senca-fuzz-seedgen <corpus_dir> <senca-tls/tests/data>"));
    let mut out = Out { root, count: 0 };
    rtmp(&mut out);
    stun(&mut out);
    srt(&mut out);
    dtls(&mut out);
    tls(&mut out, &tls_data);
    jwt(&mut out);
    raft(&mut out);
    images(&mut out);
    record(&mut out);
    println!("seedgen: {} files", out.count);
}
