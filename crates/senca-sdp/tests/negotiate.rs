//! offer/answer 交渉の試験。

mod samples;
use samples::*;
use senca_sdp::*;

fn fp() -> Fingerprint {
    Fingerprint::parse("sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99").unwrap()
}

fn server_opts() -> AnswerOptions {
    AnswerOptions {
        ice: Some(IceCredentials {
            ufrag: "srvU".into(),
            pwd: "serverpasswordserverpw".into(),
        }),
        ice_lite: true,
        fingerprint: Some(fp()),
        candidates: vec![Candidate::parse("1 1 UDP 2130706431 192.0.2.10 45100 typ host").unwrap()],
        end_of_candidates: true,
        session_id: 42,
        accept_datachannel: true,
        ..Default::default()
    }
}

#[test]
fn chrome_offer_whip_answer() {
    let offer = SessionDescription::parse(CHROME_OFFER).unwrap();
    let mut opts = server_opts();
    // WHIP: 受けるだけ
    opts.audio_direction = Direction::RecvOnly;
    opts.video_direction = Direction::RecvOnly;
    let ans = create_answer(&offer, &Capabilities::default_server(), &opts).unwrap();
    let text = ans.sdp.to_string();
    let sdp = SessionDescription::parse(&text).unwrap();
    assert_eq!(
        sdp.bundle_groups(),
        vec![vec!["0".to_string(), "1".into(), "2".into()]]
    );
    assert!(sdp.has_flag("ice-lite"));
    assert!(sdp.has_flag("extmap-allow-mixed"));

    // 音声: opus（111）・PCMU・PCMA、offer の並び。G722・CN・red・telephone-event は落とす
    let a = &ans.media[0];
    assert!(!a.rejected);
    let pts: Vec<u8> = a.codecs.iter().map(|c| c.payload_type).collect();
    assert_eq!(pts, vec![111, 0, 8]);
    assert_eq!(a.direction, Direction::RecvOnly);
    assert_eq!(a.codecs[0].rtcp_fb, vec!["transport-cc"]);
    assert_eq!(
        a.extmap_id("urn:ietf:params:rtp-hdrext:ssrc-audio-level"),
        Some(1)
    );
    assert_eq!(a.remote_ssrcs.len(), 2);
    assert!(a.rtcp_mux);
    assert_eq!(sdp.media[0].formats, vec!["111", "0", "8"]);
    assert_eq!(sdp.effective_direction(0), Direction::RecvOnly);
    assert_eq!(sdp.setup(0), Some(Setup::Passive));
    assert_eq!(sdp.ice_ufrag(0), Some("srvU"));
    assert_eq!(sdp.media[0].port, 45100);
    assert_eq!(sdp.effective_connection(0).unwrap().address, "192.0.2.10");
    assert!(sdp.media[0].has_flag("end-of-candidates"));

    // 映像: offer は sendonly → answer は recvonly
    let v = &ans.media[1];
    assert_eq!(v.direction, Direction::RecvOnly);
    let pts: Vec<(u8, Option<u8>)> = v
        .codecs
        .iter()
        .map(|c| (c.payload_type, c.rtx_payload_type))
        .collect();
    // VP8, H264 42001f(Baseline: 合わない) 落ちる, 106(CB pm1)、108(pm0) 落ちる、127(Main) 落ちる、39(High) は合う、AV1、VP9
    assert_eq!(
        pts,
        vec![
            (96, Some(97)),
            (106, Some(107)),
            (39, Some(40)),
            (45, Some(46)),
            (98, Some(99))
        ]
    );
    assert_eq!(v.codecs[0].rtcp_fb.len(), 5);
    assert!(v.codecs[0].has_feedback("NACK  PLI"));
    let h = &v.codecs[1];
    assert_eq!(h.fmtp_get("profile-level-id"), Some("42e01f"));
    assert_eq!(h.fmtp_get("packetization-mode"), Some("1"));
    let high = &v.codecs[2];
    // level-asymmetry-allowed が両方にあるので自分の level（1f）
    assert_eq!(high.fmtp_get("profile-level-id"), Some("64001f"));
    assert_eq!(high.remote_fmtp_get("profile-level-id"), Some("640034"));
    // ヘッダー拡張: toffset・video-orientation は能力にないので落ちる
    let ids: Vec<u8> = v.extmaps.iter().map(|(i, _)| *i).collect();
    assert_eq!(ids, vec![2, 3, 4, 10, 11]);
    assert!(v.rtcp_rsize);
    // simulcast を受ける
    assert_eq!(v.rids.len(), 3);
    let sm = &sdp.media[1];
    assert_eq!(sm.simulcast().unwrap().recv.len(), 3);
    assert!(sm.rids().iter().all(|r| r.direction == RidDirection::Recv));
    assert!(sm.rtcp_fbs(96).iter().any(|f| f.kind == "goog-remb"));
    assert_eq!(sm.fmtp(97).unwrap().get("apt"), Some("96"));

    // DataChannel
    let d = &ans.media[2];
    assert!(!d.rejected);
    assert_eq!(d.remote_sctp_port, Some(5000));
    assert_eq!(sdp.media[2].sctp_port(), Some(5000));

    // ブラウザー側から見て answer が offer に合っている
    let back = apply_answer(&offer, &sdp).unwrap();
    assert_eq!(back[0].direction, Direction::SendOnly);
    assert_eq!(back[1].codecs.len(), 5);
    assert_eq!(back[1].codecs[0].rtx_payload_type, Some(97));
}

#[test]
fn rejects_and_bundle_pruning() {
    let offer = SessionDescription::parse(CHROME_OFFER).unwrap();
    let mut opts = server_opts();
    opts.accept_datachannel = false;
    let caps = Capabilities {
        codecs: vec![CodecCap::opus(111)],
        header_extensions: vec![],
        rtx: false,
    };
    let ans = create_answer(&offer, &caps, &opts).unwrap();
    assert!(!ans.media[0].rejected);
    assert!(ans.media[1].rejected);
    assert!(ans.media[2].rejected);
    assert_eq!(ans.bundle, vec![vec!["0".to_string()]]);
    let sdp = SessionDescription::parse(&ans.sdp.to_string()).unwrap();
    assert_eq!(sdp.media[1].port, 0);
    assert_eq!(sdp.media[1].mid(), Some("1"));
    assert_eq!(sdp.media[1].formats, offer.media[1].formats);
    assert!(sdp.media[0].extmaps().is_empty());
    let back = apply_answer(&offer, &sdp).unwrap();
    assert!(back[1].rejected && back[2].rejected);
    // 何も受け付けられない
    let none = Capabilities {
        codecs: vec![],
        header_extensions: vec![],
        rtx: false,
    };
    assert!(matches!(
        create_answer(&offer, &none, &opts),
        Err(Error::Negotiation(_))
    ));
}

#[test]
fn rfc4317_offer_answer() {
    let offer = SessionDescription::parse(RFC4317_OFFER).unwrap();
    // RTP/AVP の古い offer: rtcp-mux なし → Require なら拒否、Negotiate なら受ける
    assert_eq!(
        create_answer(
            &offer,
            &Capabilities::default_server(),
            &AnswerOptions::default()
        )
        .unwrap_err(),
        Error::Negotiation("rtcp-mux がない")
    );
    let opts = AnswerOptions {
        rtcp_mux: RtcpMuxPolicy::Negotiate,
        ..Default::default()
    };
    let ans = create_answer(&offer, &Capabilities::default_server(), &opts).unwrap();
    assert_eq!(
        ans.media[0]
            .codecs
            .iter()
            .map(|c| c.payload_type)
            .collect::<Vec<_>>(),
        vec![0, 8]
    );
    assert!(ans.media[1].rejected);
    assert!(!ans.sdp.media[0].has_flag("rtcp-mux"));
    assert!(ans.sdp.media[0].attribute("fingerprint").is_none());
    // RFC 4317 の Bob の answer を Alice の側で検査する
    let bob = SessionDescription::parse(RFC4317_ANSWER).unwrap();
    let r = apply_answer(&offer, &bob).unwrap();
    assert_eq!(r[0].codecs[0].encoding, "PCMU");
    assert_eq!(r[1].codecs[0].encoding, "MPV");
    assert_eq!(r[0].direction, Direction::SendRecv);
}

fn h264_offer(fmtp: &str, dir: &str) -> SessionDescription {
    SessionDescription::parse(&format!(
        "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 100\r\nc=IN IP4 0.0.0.0\r\na=mid:v\r\na={dir}\r\na=rtcp-mux\r\na=setup:active\r\na=fingerprint:sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99\r\na=rtpmap:100 H264/90000\r\na=fmtp:100 {fmtp}\r\n"
    ))
    .unwrap()
}

#[test]
fn h264_fmtp_matching() {
    let caps = Capabilities::default_server();
    let opts = server_opts();
    // packetization-mode 0 は能力（mode 1）と合わない
    let r = create_answer(
        &h264_offer("packetization-mode=0;profile-level-id=42e01f", "sendrecv"),
        &caps,
        &opts,
    );
    assert!(r.is_err());
    // Main は合わない
    assert!(
        create_answer(
            &h264_offer("packetization-mode=1;profile-level-id=4d001f", "sendrecv"),
            &caps,
            &opts
        )
        .is_err()
    );
    // 4d401f（Main + constraint_set0 → Constrained Baseline 扱い）は合う
    let a = create_answer(
        &h264_offer("packetization-mode=1;profile-level-id=4d801f", "sendrecv"),
        &caps,
        &opts,
    )
    .unwrap();
    assert_eq!(
        a.media[0].codecs[0].fmtp_get("profile-level-id"),
        Some("42e01f")
    );
    // level-asymmetry-allowed なし → 低い方の level（0b）、answer から level-asymmetry-allowed を外す
    let a = create_answer(
        &h264_offer("packetization-mode=1;profile-level-id=42e00b", "sendonly"),
        &caps,
        &opts,
    )
    .unwrap();
    let c = &a.media[0].codecs[0];
    assert_eq!(c.fmtp_get("profile-level-id"), Some("42e00b"));
    assert_eq!(c.fmtp_get("level-asymmetry-allowed"), None);
    assert_eq!(a.media[0].direction, Direction::RecvOnly);
    // offer が setup:active → answer は passive
    assert_eq!(a.sdp.setup(0), Some(Setup::Passive));
    // profile-level-id なし → 42e01f とみなす
    let a = create_answer(
        &h264_offer("packetization-mode=1", "recvonly"),
        &caps,
        &opts,
    )
    .unwrap();
    assert_eq!(a.media[0].direction, Direction::SendOnly);
}

#[test]
fn direction_policy_and_send_source() {
    let caps = Capabilities::default_server();
    let mut opts = server_opts();
    opts.video_direction = Direction::RecvOnly;
    // offer が recvonly、こちらも recvonly → inactive
    let a = create_answer(
        &h264_offer("packetization-mode=1", "recvonly"),
        &caps,
        &opts,
    )
    .unwrap();
    assert_eq!(a.media[0].direction, Direction::Inactive);
    // mid ごとの上書きと送信元
    opts.direction_overrides = vec![("v".into(), Direction::SendOnly)];
    opts.send_sources = vec![SendSource {
        mid: None,
        kind: MediaKind::Video,
        ssrc: 1111,
        rtx_ssrc: Some(2222),
        cname: "senca".into(),
        msid: Some(Msid::parse("s t").unwrap()),
    }];
    let a = create_answer(
        &h264_offer("packetization-mode=1", "recvonly"),
        &caps,
        &opts,
    )
    .unwrap();
    assert_eq!(a.media[0].direction, Direction::SendOnly);
    let m = &a.sdp.media[0];
    assert_eq!(m.ssrc_groups()[0].ssrcs, vec![1111, 2222]);
    assert_eq!(m.ssrcs().len(), 2);
    assert_eq!(m.msid().unwrap().stream_id, "s");
    // holdconn は交渉できない
    let mut off = h264_offer("packetization-mode=1", "recvonly");
    off.media[0].attributes.retain(|a| a.name != "setup");
    off.media[0].push_attr("setup", "holdconn");
    assert!(create_answer(&off, &caps, &opts).is_err());
}

#[test]
fn duplicate_mid_and_bad_bundle() {
    let caps = Capabilities::default_server();
    let mut off = h264_offer("packetization-mode=1", "sendrecv");
    off.media.push(off.media[0].clone());
    assert_eq!(
        create_answer(&off, &caps, &server_opts()).unwrap_err(),
        Error::Negotiation("mid が重複")
    );
    let mut off = h264_offer("packetization-mode=1", "sendrecv");
    off.push_attr("group", "BUNDLE v x");
    assert!(create_answer(&off, &caps, &server_opts()).is_err());
}

fn whep_offer() -> SessionDescription {
    let caps = Capabilities::default_server();
    let video = OfferMedia {
        kind: MediaKind::Video,
        mid: "0".into(),
        direction: Direction::SendOnly,
        codecs: vec![{
            let mut c = OfferCodec::from(&caps.codecs[0]);
            c.rtx_payload_type = Some(103);
            c
        }],
        extmaps: vec![(4, "urn:ietf:params:rtp-hdrext:sdes:mid".into())],
        send: Some(SendSource {
            mid: Some("0".into()),
            kind: MediaKind::Video,
            ssrc: 10,
            rtx_ssrc: Some(11),
            cname: "c".into(),
            msid: None,
        }),
        control: None,
        datachannel: false,
    };
    let audio = OfferMedia {
        kind: MediaKind::Audio,
        mid: "1".into(),
        direction: Direction::SendOnly,
        codecs: vec![OfferCodec::from(&caps.codecs[6])],
        extmaps: vec![],
        send: None,
        control: None,
        datachannel: false,
    };
    let opts = OfferOptions {
        ice: Some(IceCredentials {
            ufrag: "abcd".into(),
            pwd: "0123456789012345678901".into(),
        }),
        fingerprint: Some(fp()),
        ice_lite: true,
        ..Default::default()
    };
    create_offer(&[video, audio], &opts).unwrap()
}

#[test]
fn whep_offer_and_browser_answer() {
    let offer = whep_offer();
    let text = offer.to_string();
    let offer = SessionDescription::parse(&text).unwrap();
    assert_eq!(
        offer.bundle_groups(),
        vec![vec!["0".to_string(), "1".into()]]
    );
    assert_eq!(offer.media[0].protocol, "UDP/TLS/RTP/SAVPF");
    assert_eq!(offer.setup(0), Some(Setup::ActPass));
    assert_eq!(offer.media[0].formats, vec!["102", "103"]);
    assert_eq!(offer.media[0].ssrcs().len(), 2);
    // ブラウザー役: 同じ能力で recvonly の answer を作る
    let mut b = server_opts();
    b.ice_lite = false;
    b.setup = Setup::Active;
    b.audio_direction = Direction::RecvOnly;
    b.video_direction = Direction::RecvOnly;
    let ans = create_answer(&offer, &Capabilities::default_server(), &b).unwrap();
    let ans_sdp = SessionDescription::parse(&ans.sdp.to_string()).unwrap();
    assert_eq!(ans_sdp.setup(0), Some(Setup::Active));
    let r = apply_answer(&offer, &ans_sdp).unwrap();
    assert_eq!(r[0].direction, Direction::SendOnly);
    assert_eq!(r[0].codecs[0].rtx_payload_type, Some(103));
    assert_eq!(
        r[0].extmaps,
        vec![(4, "urn:ietf:params:rtp-hdrext:sdes:mid".to_string())]
    );
    assert_eq!(r[1].codecs[0].encoding, "opus");

    // 不正な answer
    let mut bad = ans_sdp.clone();
    bad.media[0].formats.push("120".into());
    assert!(apply_answer(&offer, &bad).is_err());
    let mut bad = ans_sdp.clone();
    bad.media[0].push_attr("extmap", "9 urn:x");
    assert!(apply_answer(&offer, &bad).is_err());
    let mut bad = ans_sdp.clone();
    bad.media[0].attributes.retain(|a| a.name != "recvonly");
    bad.media[0].push_flag("sendonly");
    assert!(apply_answer(&offer, &bad).is_err());
    let mut bad = ans_sdp.clone();
    bad.media.pop();
    assert!(apply_answer(&offer, &bad).is_err());
    let mut bad = ans_sdp.clone();
    bad.attributes.push(Attribute::new("group", "BUNDLE 0 9"));
    assert!(apply_answer(&offer, &bad).is_err());
}

#[test]
fn rtsp_describe_offer() {
    let video = OfferMedia {
        kind: MediaKind::Video,
        mid: "v".into(),
        direction: Direction::SendOnly,
        codecs: vec![OfferCodec {
            payload_type: 96,
            encoding: "H264".into(),
            clock_rate: 90000,
            channels: None,
            fmtp: vec![
                ("packetization-mode".into(), "1".into()),
                (
                    "sprop-parameter-sets".into(),
                    "Z0IAH5WoFAFuQA==,aM48gA==".into(),
                ),
            ],
            rtcp_fb: vec![],
            rtx_payload_type: None,
        }],
        extmaps: vec![],
        send: None,
        control: None,
        datachannel: false,
    };
    let audio = OfferMedia {
        kind: MediaKind::Audio,
        mid: "a".into(),
        direction: Direction::SendOnly,
        codecs: vec![OfferCodec {
            payload_type: 97,
            encoding: "MPEG4-GENERIC".into(),
            clock_rate: 48000,
            channels: Some(2),
            fmtp: vec![
                ("streamtype".into(), "5".into()),
                ("mode".into(), "AAC-hbr".into()),
                ("config".into(), "1190".into()),
            ],
            rtcp_fb: vec![],
            rtx_payload_type: None,
        }],
        extmaps: vec![],
        send: None,
        control: Some("audio".into()),
        datachannel: false,
    };
    let opts = OfferOptions {
        mode: OfferMode::Rtsp,
        session_name: "senca".into(),
        rtsp_control: Some("*".into()),
        rtsp_range: Some("npt=now-".into()),
        ..Default::default()
    };
    let sdp = create_offer(&[video.clone(), audio], &opts).unwrap();
    let text = sdp.to_string();
    let s = SessionDescription::parse(&text).unwrap();
    assert_eq!(s.connection.as_ref().unwrap().address, "0.0.0.0");
    assert_eq!(s.attribute("control"), Some("*"));
    assert_eq!(s.attribute("range"), Some("npt=now-"));
    assert_eq!(s.media[0].protocol, "RTP/AVP");
    assert_eq!(s.media[0].port, 0);
    assert_eq!(s.control(0), Some("trackID=0"));
    assert_eq!(s.control(1), Some("audio"));
    assert!(s.media[0].mid().is_none());
    assert!(s.media[0].direction().is_none());
    assert_eq!(
        s.media[0].fmtp(96).unwrap().get("sprop-parameter-sets"),
        Some("Z0IAH5WoFAFuQA==,aM48gA==")
    );
    assert_eq!(s.media[1].rtpmap(97).unwrap().channels, Some(2));
    assert!(text.contains("a=fmtp:97 streamtype=5;mode=AAC-hbr;config=1190\r\n"));

    // 改行の注入や不正な値は拒否する
    let mut bad = video.clone();
    bad.codecs[0].fmtp.push(("x".into(), "1\r\na=evil".into()));
    assert!(create_offer(&[bad], &opts).is_err());
    let mut bad = video.clone();
    bad.control = Some("a b".into());
    assert!(create_offer(&[bad], &opts).is_err());
    let mut bad = video.clone();
    bad.codecs[0].payload_type = 200;
    assert!(create_offer(&[bad], &opts).is_err());
    assert!(create_offer(&[video.clone(), video], &opts).is_err()); // mid 重複
    let bad_opts = OfferOptions {
        session_name: "x\r\nm=evil".into(),
        ..opts.clone()
    };
    assert!(create_offer(&[], &bad_opts).is_err());
}

#[test]
fn datachannel_offer() {
    let dc = OfferMedia {
        kind: MediaKind::Application,
        mid: "d".into(),
        direction: Direction::SendRecv,
        codecs: vec![],
        extmaps: vec![],
        send: None,
        control: None,
        datachannel: true,
    };
    let opts = OfferOptions {
        ice: Some(IceCredentials {
            ufrag: "abcd".into(),
            pwd: "0123456789012345678901".into(),
        }),
        fingerprint: Some(fp()),
        ..Default::default()
    };
    let s = SessionDescription::parse(&create_offer(&[dc], &opts).unwrap().to_string()).unwrap();
    assert_eq!(s.media[0].protocol, "UDP/DTLS/SCTP");
    assert_eq!(s.media[0].formats, vec!["webrtc-datachannel"]);
    let ans = create_answer(&s, &Capabilities::default_server(), &server_opts()).unwrap();
    let back = apply_answer(
        &s,
        &SessionDescription::parse(&ans.sdp.to_string()).unwrap(),
    )
    .unwrap();
    assert!(!back[0].rejected);
    assert_eq!(back[0].remote_sctp_port, Some(5000));
}
