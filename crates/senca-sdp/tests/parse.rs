//! 解析・出力の試験（RFC の例、実機の SDP、往復、上限、壊れた入力）。

mod samples;
use samples::*;
use senca_sdp::*;

fn roundtrip(s: &SessionDescription) {
    let text = s.to_string();
    let again = SessionDescription::parse(&text).unwrap();
    assert_eq!(&again, s);
    assert_eq!(again.to_string(), text);
}

#[test]
fn rfc8866_example() {
    let s = SessionDescription::parse(RFC8866).unwrap();
    assert_eq!(s.origin.username, "jdoe");
    assert_eq!(s.origin.session_id, 3724394400);
    assert_eq!(s.session_name, "Call to John Smith");
    assert_eq!(s.emails.len(), 1);
    assert_eq!(s.media.len(), 3);
    assert_eq!(s.effective_connection(0).unwrap().address, "198.51.100.1");
    assert_eq!(s.effective_connection(2).unwrap().address, "2001:db8::2");
    assert_eq!(s.media[2].rtpmap(99).unwrap().encoding, "h263-1998");
    assert_eq!(s.media[0].rtpmap(0).unwrap().encoding, "PCMU"); // 静的
    assert_eq!(s.effective_direction(0), Direction::SendRecv);
    assert_eq!(s.to_string(), RFC8866);
    roundtrip(&s);
}

#[test]
fn chrome_offer() {
    let s = SessionDescription::parse(CHROME_OFFER).unwrap();
    assert_eq!(s.to_string(), CHROME_OFFER);
    assert_eq!(
        s.bundle_groups(),
        vec![vec!["0".to_string(), "1".into(), "2".into()]]
    );
    assert!(s.has_flag("extmap-allow-mixed"));
    let a = &s.media[0];
    assert_eq!(a.mid(), Some("0"));
    assert_eq!(a.candidates().len(), 4);
    assert_eq!(a.ssrcs().len(), 2);
    assert_eq!(a.rtpmap(111).unwrap().channels, Some(2));
    assert_eq!(a.fmtp(111).unwrap().get("useinbandfec"), Some("1"));
    assert_eq!(s.ice_ufrag(0), Some("Oyef"));
    assert_eq!(s.setup(0), Some(Setup::ActPass));
    assert_eq!(s.fingerprint(0).unwrap().bytes.len(), 32);
    assert_eq!(s.ice_options(1), vec!["trickle"]);
    let v = &s.media[1];
    assert_eq!(s.effective_direction(1), Direction::SendOnly);
    assert_eq!(v.rtcp_fbs(96).len(), 5);
    assert_eq!(v.extmaps().len(), 7);
    assert_eq!(v.rids().len(), 3);
    assert_eq!(v.simulcast().unwrap().send.len(), 3);
    assert_eq!(v.msid().unwrap().track_id.as_deref(), Some("videotrack"));
    let d = &s.media[2];
    assert_eq!(d.sctp_port(), Some(5000));
    assert_eq!(d.max_message_size(), Some(262144));
    roundtrip(&s);
}

#[test]
fn ffmpeg_and_mediamtx() {
    let s = SessionDescription::parse(FFMPEG_SDP).unwrap();
    let f = s.media[0].fmtp(96).unwrap();
    assert_eq!(f.get("packetization-mode"), Some("1"));
    assert!(f.get("sprop-parameter-sets").unwrap().contains(','));
    assert_eq!(s.media[0].bandwidths[0].bandwidth, 200);
    assert_eq!(s.media[1].port, 45002);
    roundtrip(&s);

    let m = SessionDescription::parse(MEDIAMTX_SDP).unwrap();
    assert_eq!(m.control(1), Some("rtsp://127.0.0.1:45554/cam/trackID=1"));
    let a = m.media[1].rtpmap(97).unwrap();
    assert_eq!(
        (a.encoding.as_str(), a.clock_rate, a.channels),
        ("mpeg4-generic", 48000, Some(2))
    );
    assert_eq!(m.media[1].fmtp(97).unwrap().get("SizeLength"), Some("13"));
    roundtrip(&m);
}

#[test]
fn lenient_camera() {
    assert_eq!(
        SessionDescription::parse(OLD_CAMERA_SDP),
        Err(Error::Syntax {
            line: 2,
            what: "s= が空"
        })
    );
    let s = SessionDescription::parse_with(OLD_CAMERA_SDP, &ParseOptions::lenient()).unwrap();
    assert_eq!(s.attribute("control"), Some("*"));
    assert_eq!(s.control(0), Some("track1"));
    assert_eq!(s.times.len(), 1);
    // 出力は厳格な解析でも読める形になる（o=・t= を補う）。s= は空のまま
    let text = s.to_string();
    assert!(SessionDescription::parse_with(&text, &ParseOptions::lenient()).is_ok());
}

#[test]
fn order_and_syntax_errors() {
    let bad = [
        ("o=- 0 0 IN IP4 1.1.1.1\r\nv=0\r\n", "v 先頭でない"),
        ("v=1\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\nt=0 0\r\n", "v=1"),
        (
            "v=0\r\ns=-\r\no=- 0 0 IN IP4 1.1.1.1\r\nt=0 0\r\n",
            "o と s の順",
        ),
        (
            "v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\ns=-\r\nt=0 0\r\n",
            "s 重複",
        ),
        (
            "v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\nt=0 0\r\nc=IN IP4 1.1.1.1\r\n",
            "c が t の後",
        ),
        (
            "v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\nr=1 2 3\r\nt=0 0\r\n",
            "r が t の前",
        ),
        (
            "v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\nt=0 0\r\nx=1\r\n",
            "知らない種類",
        ),
        (
            "v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\nt=0 0\r\nm=audio 1 RTP/AVP 0\r\na=x\r\nc=IN IP4 1.1.1.1\r\n",
            "media の順",
        ),
        (
            "v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\nt=0 0\r\nm=audio 1 RTP/AVP\r\n",
            "format なし",
        ),
        (
            "v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\nt=0 0\r\nm=audio 70000 RTP/AVP 0\r\n",
            "port 範囲",
        ),
        (
            "v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\nt=0 0\r\nm=audio 1 RTP/AVP 0\r\nt=0 0\r\n",
            "media に t",
        ),
        (
            "v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\n\r\nt=0 0\r\n",
            "途中の空行",
        ),
        (
            "v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\nt=0 0\r\na=a\rb\r\n",
            "途中の CR",
        ),
        (
            "v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\nt=0 0\r\na=\r\n",
            "空の属性名",
        ),
        (
            "v=0\r\no=- x 0 IN IP4 1.1.1.1\r\ns=-\r\nt=0 0\r\n",
            "sess-id",
        ),
        ("v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\n", "t なし"),
        ("v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\nt=0 0\r\n", "s なし"),
        ("v=0\r\ns=-\r\nt=0 0\r\n", "o なし"),
        ("V=0\r\n", "大文字"),
        ("v =0\r\n", "空白"),
        ("", "空"),
    ];
    for (text, what) in bad {
        assert!(SessionDescription::parse(text).is_err(), "{what}");
    }
    // 繰り返しの t= r= は許す
    let ok = "v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\nt=1 2\r\nr=7d 1h 0 25h\r\nt=3 4\r\nz=1 -1h\r\nk=prompt\r\nm=audio 1/2 RTP/AVP 0\r\ni=x\r\nc=IN IP4 224.2.1.1/127\r\nb=AS:64\r\nk=prompt\r\na=recvonly\r\n";
    let s = SessionDescription::parse(ok).unwrap();
    assert_eq!(s.times.len(), 2);
    assert_eq!(s.times[0].repeats.len(), 1);
    assert_eq!(s.media[0].port_count, Some(2));
    assert_eq!(
        s.effective_connection(0).unwrap().base_address(),
        "224.2.1.1"
    );
    assert_eq!(s.to_string(), ok);
    // LF だけ・末尾の改行なしも読める
    assert!(SessionDescription::parse("v=0\no=- 0 0 IN IP4 1.1.1.1\ns=-\nt=0 0").is_ok());
}

#[test]
fn limits() {
    let head = "v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\ns=-\r\nt=0 0\r\n";
    // 大きさ
    let mut big = head.to_string();
    while big.len() <= MAX_SDP_BYTES {
        big.push_str(
            "a=x-pad:0123456789012345678901234567890123456789012345678901234567890123456789\r\n",
        );
    }
    assert_eq!(
        SessionDescription::parse(&big),
        Err(Error::TooLarge {
            limit: MAX_SDP_BYTES
        })
    );
    assert!(SessionDescription::parse_bytes(big.as_bytes()).is_err());
    // 属性の数
    let mut many = head.to_string();
    for _ in 0..1100 {
        many.push_str("a=x\r\n");
    }
    assert_eq!(
        SessionDescription::parse(&many),
        Err(Error::Limit("属性の数"))
    );
    // media の数
    let mut media = head.to_string();
    for _ in 0..70 {
        media.push_str("m=audio 1 RTP/AVP 0\r\n");
    }
    assert_eq!(
        SessionDescription::parse(&media),
        Err(Error::Limit("media の数"))
    );
    // 行の長さ
    let long = format!("{head}a=x:{}\r\n", "y".repeat(20000));
    assert_eq!(
        SessionDescription::parse(&long),
        Err(Error::Limit("行の長さ"))
    );
    // 行数（短い行を大量に）
    let mut lines = head.to_string();
    for _ in 0..60 {
        lines.push_str("m=audio 1 RTP/AVP 0\r\n");
        for _ in 0..200 {
            lines.push_str("a=x\r\n");
        }
    }
    assert_eq!(SessionDescription::parse(&lines), Err(Error::Limit("行数")));
    // UTF-8 でない
    assert_eq!(
        SessionDescription::parse_bytes(b"v=0\r\ns=\xff\r\n"),
        Err(Error::NotUtf8)
    );
}

/// 決定的な擬似乱数（xorshift64）。
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

#[test]
fn fuzz_mutations_never_panic() {
    let seeds = [
        RFC8866,
        RFC4317_OFFER,
        CHROME_OFFER,
        FFMPEG_SDP,
        MEDIAMTX_SDP,
        OLD_CAMERA_SDP,
    ];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let caps = Capabilities::default_server();
    let opts = AnswerOptions {
        rtcp_mux: RtcpMuxPolicy::Negotiate,
        accept_datachannel: true,
        ..Default::default()
    };
    let interesting = b" :=/;,\r\n0123456789abcmvtsoa*~-";
    let mut ok = 0;
    for i in 0..20000 {
        let seed = seeds[i % seeds.len()].as_bytes();
        let mut b = seed.to_vec();
        for _ in 0..1 + rng.below(4) {
            let pos = rng.below(b.len());
            match rng.below(5) {
                0 => b[pos] = rng.next() as u8,
                1 => b[pos] = interesting[rng.below(interesting.len())],
                2 => {
                    b.remove(pos);
                }
                3 => b.insert(pos, interesting[rng.below(interesting.len())]),
                _ => b.truncate(pos),
            }
            if b.is_empty() {
                break;
            }
        }
        for opts_p in [ParseOptions::default(), ParseOptions::lenient()] {
            if let Ok(s) = SessionDescription::parse_bytes_with(&b, &opts_p) {
                ok += 1;
                // 出力し直したものは同じ値に戻る
                let text = s.to_string();
                let again = SessionDescription::parse_with(&text, &opts_p).unwrap();
                assert_eq!(again, s);
                // 型付きの取り出しと交渉もパニックしない
                for (idx, m) in s.media.iter().enumerate() {
                    let _ = (
                        m.rtpmaps(),
                        m.extmaps(),
                        m.candidates(),
                        m.rids(),
                        m.simulcast(),
                    );
                    let _ = (s.fingerprint(idx), s.setup(idx), s.effective_direction(idx));
                    for pt in m.payload_types() {
                        let _ = (m.fmtp(pt), m.rtcp_fbs(pt));
                    }
                }
                if let Ok(a) = create_answer(&s, &caps, &opts) {
                    let parsed = SessionDescription::parse(&a.sdp.to_string()).unwrap();
                    let _ = apply_answer(&s, &parsed);
                }
            }
        }
    }
    assert!(ok > 1000, "変異のうち解析できたものが少なすぎる: {ok}");
}
