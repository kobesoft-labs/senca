//! 2 つ以上の PeerConnection を記憶の中のネットワークでつなぐ試験（時刻は仮想）。

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use senca_core::time::Timebase;
use senca_sdp::Direction;

use crate::sfu::{Room, RoomConfig, RoomOutput};
use crate::*;

/// 決まった乱数。
pub(crate) struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next() % 1_000_000) as f64 / 1_000_000.0 < p
    }
}

/// 片方向の回線。
#[derive(Clone)]
pub(crate) struct Link {
    pub delay: Duration,
    pub loss: f64,
    /// 容量（bit/s）。`None` なら無制限。
    pub rate: Option<f64>,
    /// 待ち行列の上限（時間）。超えたら捨てる。
    pub max_queue: Duration,
    busy_until: Option<Instant>,
}

impl Link {
    pub fn new(delay_ms: u64) -> Link {
        Link {
            delay: Duration::from_millis(delay_ms),
            loss: 0.0,
            rate: None,
            max_queue: Duration::from_millis(500),
            busy_until: None,
        }
    }
}

pub(crate) struct Sim {
    pub now: Instant,
    pub pcs: Vec<PeerConnection>,
    pub addrs: Vec<SocketAddr>,
    links: HashMap<(usize, usize), Link>,
    queue: BTreeMap<(Instant, u64), (usize, SocketAddr, Vec<u8>)>,
    seq: u64,
    rng: Rng,
    pub events: Vec<Vec<Event>>,
    pub delivered: u64,
    pub lost: u64,
    /// 全部の packet を落とす（期限つき）。
    pub blackout: Option<(usize, usize, Instant)>,
}

impl Sim {
    pub fn new() -> Sim {
        Sim {
            now: Instant::now(),
            pcs: Vec::new(),
            addrs: Vec::new(),
            links: HashMap::new(),
            queue: BTreeMap::new(),
            seq: 0,
            rng: Rng(0x9E37_79B9_7F4A_7C15),
            events: Vec::new(),
            delivered: 0,
            lost: 0,
            blackout: None,
        }
    }

    pub fn add(&mut self, cfg: PeerConfig, addr: &str) -> usize {
        let addr: SocketAddr = addr.parse().unwrap();
        let mut pc = PeerConnection::new(cfg, self.now).unwrap();
        pc.add_host_address(self.now, addr).unwrap();
        self.pcs.push(pc);
        self.addrs.push(addr);
        self.events.push(Vec::new());
        self.pcs.len() - 1
    }

    pub fn link(&mut self, a: usize, b: usize) -> &mut Link {
        self.links.entry((a, b)).or_insert_with(|| Link::new(10))
    }

    fn flush(&mut self) {
        for i in 0..self.pcs.len() {
            while let Some(t) = self.pcs[i].poll_transmit() {
                let Some(dst) = self.addrs.iter().position(|a| *a == t.remote) else {
                    continue;
                };
                let link = self.links.entry((i, dst)).or_insert_with(|| Link::new(10));
                if let Some((a, b, until)) = self.blackout
                    && a == i
                    && b == dst
                    && self.now < until
                {
                    self.lost += 1;
                    continue;
                }
                if link.loss > 0.0 && self.rng.chance(link.loss) {
                    self.lost += 1;
                    continue;
                }
                let mut at = self.now + link.delay;
                if let Some(rate) = link.rate {
                    let start = link.busy_until.map_or(self.now, |b| b.max(self.now));
                    if start.saturating_duration_since(self.now) > link.max_queue {
                        self.lost += 1;
                        continue;
                    }
                    let tx = Duration::from_secs_f64((t.data.len() + 28) as f64 * 8.0 / rate);
                    link.busy_until = Some(start + tx);
                    at = start + tx + link.delay;
                }
                self.seq += 1;
                self.queue
                    .insert((at, self.seq), (dst, self.addrs[i], t.data.to_vec()));
            }
            while let Some(e) = self.pcs[i].poll_event() {
                self.events[i].push(e);
            }
        }
    }

    /// `until` まで進める。
    pub fn run_until(&mut self, until: Instant) {
        let mut guard = 0u64;
        loop {
            self.flush();
            let mut next = self.queue.keys().next().map(|k| k.0);
            for pc in &self.pcs {
                if let Some(t) = pc.poll_timeout() {
                    next = Some(next.map_or(t, |n| n.min(t)));
                }
            }
            let Some(next) = next else { break };
            if next > until {
                break;
            }
            guard += 1;
            assert!(guard < 5_000_000, "simulation does not progress");
            if next > self.now {
                self.now = next;
            }
            while let Some((&k, _)) = self.queue.iter().next() {
                if k.0 > self.now {
                    break;
                }
                let (dst, from, data) = self.queue.remove(&k).unwrap();
                self.delivered += 1;
                let now = self.now;
                self.pcs[dst].handle_datagram(now, from, &data);
            }
            for i in 0..self.pcs.len() {
                if self.pcs[i].poll_timeout().is_some_and(|t| t <= self.now) {
                    let now = self.now;
                    self.pcs[i].handle_timeout(now);
                }
            }
        }
        self.now = until.max(self.now);
        self.flush();
    }

    pub fn run(&mut self, d: Duration) {
        let u = self.now + d;
        self.run_until(u);
    }

    pub fn take_events(&mut self, i: usize) -> Vec<Event> {
        std::mem::take(&mut self.events[i])
    }
}

pub(crate) fn client_config() -> PeerConfig {
    PeerConfig {
        ice_lite: false,
        ..Default::default()
    }
}

/// VP8 のフレーム（先頭の 2 byte に層と番号、キーフレームは frame tag の P ビットが 0）。
pub(crate) fn vp8_frame(layer: u8, n: u32, size: usize, key: bool) -> Bytes {
    let mut v = vec![0u8; size.max(16)];
    if key {
        v[0] = 0x10;
        v[3] = 0x9d;
        v[4] = 0x01;
        v[5] = 0x2a;
        v[6] = 0x80;
        v[7] = 0x02;
        v[8] = 0xe0;
        v[9] = 0x01;
    } else {
        v[0] = 0x11;
    }
    v[10] = layer;
    v[11..15].copy_from_slice(&n.to_be_bytes());
    for (i, b) in v.iter_mut().enumerate().skip(15) {
        *b = (i as u32).wrapping_mul(31).wrapping_add(n) as u8;
    }
    Bytes::from(v)
}

pub(crate) fn frame_id(p: &[u8]) -> (u8, u32) {
    (p[10], u32::from_be_bytes([p[11], p[12], p[13], p[14]]))
}

pub(crate) fn video_au(n: u32, payload: Bytes, key: bool, src: u32) -> AccessUnit {
    AccessUnit {
        track: TrackId(src),
        codec: Codec::Vp8,
        epoch: 0,
        sequence: n as u64,
        config_generation: 0,
        pts: n as i64 * 3000,
        dts: n as i64 * 3000,
        duration: 3000,
        timebase: Timebase::new(1, 90000),
        keyframe: key,
        discontinuity: false,
        capture_time_ns: None,
        payload,
    }
}

pub(crate) fn opus_au(n: u32) -> AccessUnit {
    AccessUnit {
        track: TrackId(0),
        codec: Codec::Opus,
        epoch: 0,
        sequence: n as u64,
        config_generation: 0,
        pts: n as i64 * 960,
        dts: n as i64 * 960,
        duration: 960,
        timebase: Timebase::new(1, 48000),
        keyframe: true,
        discontinuity: false,
        capture_time_ns: None,
        payload: Bytes::from(vec![0xFC, (n & 0xff) as u8, 3, 4, 5]),
    }
}

fn spec(video_dir: Direction, audio: bool, simulcast: &[&str], dc: bool) -> OfferSpec {
    let mut media = vec![OfferMediaSpec {
        kind: MediaKind::Video,
        direction: video_dir,
        codecs: Some(vec![Codec::Vp8]),
        simulcast: simulcast.iter().map(|s| s.to_string()).collect(),
    }];
    if audio {
        media.push(OfferMediaSpec {
            kind: MediaKind::Audio,
            direction: video_dir,
            codecs: Some(vec![Codec::Opus]),
            simulcast: Vec::new(),
        });
    }
    OfferSpec {
        media,
        datachannel: dc,
    }
}

/// client（full ICE、offer）と server（ICE-lite、answer）をつなぐ。
fn connect_pair(sim: &mut Sim, client: usize, server: usize, s: &OfferSpec) {
    let now = sim.now;
    let offer = sim.pcs[client].create_offer(now, s).unwrap();
    let answer = sim.pcs[server].accept_offer(now, &offer).unwrap();
    sim.pcs[client].apply_answer(now, &answer).unwrap();
    wait_connected(sim, &[client, server]);
}

fn wait_connected(sim: &mut Sim, who: &[usize]) {
    for _ in 0..100 {
        sim.run(Duration::from_millis(50));
        if who.iter().all(|&i| sim.pcs[i].is_connected()) {
            return;
        }
    }
    panic!(
        "not connected: {:?} events {:?}",
        who.iter().map(|&i| &sim.pcs[i]).collect::<Vec<_>>(),
        who.iter()
            .map(|&i| sim.events[i]
                .iter()
                .filter(|e| !matches!(e, Event::AccessUnit { .. }))
                .cloned()
                .collect::<Vec<_>>())
            .collect::<Vec<_>>()
    );
}

fn received_aus(events: &[Event]) -> Vec<(TrackId, AccessUnit)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::AccessUnit { track, au, .. } => Some((*track, au.clone())),
            _ => None,
        })
        .collect()
}

fn track_of(sim: &Sim, pc: usize, kind: MediaKind, rid: Option<&str>) -> TrackId {
    sim.pcs[pc]
        .send_tracks()
        .into_iter()
        .find(|t| t.kind == kind && t.rid.as_deref() == rid)
        .unwrap_or_else(|| panic!("no {kind:?} send track {rid:?}"))
        .track
}

/// 映像 30 fps と音声 50 fps を `secs` 秒送る。キーフレームは 2 秒ごと。
fn stream_both(sim: &mut Sim, from: usize, frames: u32, size: usize) {
    let v = track_of(sim, from, MediaKind::Video, None);
    let a = track_of(sim, from, MediaKind::Audio, None);
    let mut audio_n = 0u32;
    for n in 0..frames {
        let key = n % 60 == 0;
        let now = sim.now;
        sim.pcs[from]
            .send_access_unit(
                now,
                v,
                &video_au(n, vp8_frame(0, n, size, key), key, 1),
                None,
            )
            .unwrap();
        // 33 ms の間に音声 20 ms ごと
        let t0 = sim.now;
        while Duration::from_millis(audio_n as u64 * 20) < Duration::from_millis(n as u64 * 33 + 33)
        {
            let now = sim.now;
            sim.pcs[from]
                .send_access_unit(now, a, &opus_au(audio_n), None)
                .unwrap();
            audio_n += 1;
            sim.run(Duration::from_millis(1));
        }
        let el = sim.now - t0;
        sim.run(Duration::from_millis(33).saturating_sub(el));
    }
}

#[test]
fn connect_media_both_ways() {
    let mut sim = Sim::new();
    let c = sim.add(client_config(), "10.0.0.1:5000");
    let s = sim.add(PeerConfig::default(), "10.0.0.2:6000");
    connect_pair(&mut sim, c, s, &spec(Direction::SendRecv, true, &[], false));
    let ev = sim.take_events(s);
    assert!(ev.iter().any(|e| matches!(e, Event::Connected)));
    assert_eq!(sim.pcs[s].send_tracks().len(), 2);
    stream_both(&mut sim, c, 90, 3000);
    stream_both(&mut sim, s, 90, 3000);
    sim.run(Duration::from_millis(500));
    for (rx, name) in [(s, "server"), (c, "client")] {
        let ev = sim.take_events(rx);
        let tracks: Vec<TrackInfo> = ev
            .iter()
            .filter_map(|e| match e {
                Event::Track(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(tracks.len(), 2, "{name}: {tracks:?}");
        let vt = tracks.iter().find(|t| t.kind == MediaKind::Video).unwrap();
        assert_eq!(vt.codec, Codec::Vp8);
        let aus = received_aus(&ev);
        let video: Vec<_> = aus.iter().filter(|(t, _)| *t == vt.track).collect();
        let audio: Vec<_> = aus.iter().filter(|(t, _)| *t != vt.track).collect();
        assert_eq!(video.len(), 90, "{name} video frames");
        assert!(audio.len() >= 140, "{name} audio {}", audio.len());
        for (i, (_, au)) in video.iter().enumerate() {
            assert_eq!(
                au.payload,
                vp8_frame(0, i as u32, 3000, i % 60 == 0),
                "{name} frame {i}"
            );
            assert_eq!(au.keyframe, i % 60 == 0);
            assert_eq!(au.timebase, Timebase::new(1, 90000));
        }
        // 時刻は単調で、間隔は名目の 75〜125 %（SR による lip sync の合わせは少しずつ）
        for (list, step) in [(&video, 3000i64), (&audio, 960)] {
            let d: Vec<i64> = list.windows(2).map(|w| w[1].1.pts - w[0].1.pts).collect();
            assert!(
                d.iter().all(|x| *x >= step * 3 / 4 && *x <= step * 5 / 4),
                "{name} {d:?}"
            );
            let odd = d.iter().filter(|x| **x != step).count();
            assert!(odd <= 20, "{name} {d:?}");
        }
    }
}

#[test]
fn loss_recovered_by_nack_and_rtx() {
    let mut sim = Sim::new();
    let c = sim.add(client_config(), "10.0.0.1:5000");
    let s = sim.add(PeerConfig::default(), "10.0.0.2:6000");
    connect_pair(&mut sim, c, s, &spec(Direction::SendOnly, true, &[], false));
    sim.take_events(s);
    sim.link(c, s).loss = 0.08;
    sim.link(s, c).loss = 0.05;
    stream_both(&mut sim, c, 150, 4000);
    sim.link(c, s).loss = 0.0;
    sim.run(Duration::from_secs(1));
    let ev = sim.take_events(s);
    let aus = received_aus(&ev);
    let video: Vec<_> = aus.iter().filter(|(_, a)| a.codec == Codec::Vp8).collect();
    let (rtx, nacked) = sim.pcs[c].retransmission_counters();
    let (_, nack_req) = sim.pcs[s].retransmission_counters();
    assert!(rtx > 20, "retransmitted {rtx}");
    assert!(nack_req > 20, "nack requested {nack_req}");
    let _ = nacked;
    eprintln!(
        "rtx {rtx} nack_req {nack_req} video {} dropped {:?} rtt {:?} lost {} delivered {}",
        video.len(),
        sim.pcs[s].dropped_counters(),
        sim.pcs[s].rtt(),
        sim.lost,
        sim.delivered
    );
    eprintln!("{:?}", sim.pcs[s].debug_inbound());
    // 失われたフレームはほとんど無い（8% の損失を再送で埋める）
    assert!(video.len() >= 147, "only {} of 150 frames", video.len());
    for (_, au) in &video {
        let (_, n) = frame_id(&au.payload);
        assert_eq!(
            au.payload,
            vp8_frame(0, n, 4000, n % 60 == 0),
            "frame {n} corrupted"
        );
    }
}

#[test]
fn burst_loss_requests_keyframe() {
    let mut sim = Sim::new();
    // 再送の履歴を小さくして、長い途絶は NACK で戻せないようにする
    let c = sim.add(
        PeerConfig {
            retransmit_history: 16,
            ..client_config()
        },
        "10.0.0.1:5000",
    );
    let s = sim.add(PeerConfig::default(), "10.0.0.2:6000");
    connect_pair(&mut sim, c, s, &spec(Direction::SendOnly, true, &[], false));
    let v = track_of(&sim, c, MediaKind::Video, None);
    sim.take_events(c);
    let mut key_next = true;
    let mut got_request = false;
    for n in 0..120u32 {
        if n == 30 {
            // 400 ms 何も届かない（NACK でも戻せない）
            sim.blackout = Some((c, s, sim.now + Duration::from_millis(400)));
        }
        let now = sim.now;
        sim.pcs[c]
            .send_access_unit(
                now,
                v,
                &video_au(n, vp8_frame(0, n, 2000, key_next), key_next, 1),
                None,
            )
            .unwrap();
        key_next = false;
        sim.run(Duration::from_millis(33));
        for e in sim.take_events(c) {
            if let Event::KeyframeRequest { track } = e {
                assert_eq!(track, v);
                if n > 30 {
                    got_request = true;
                    key_next = true;
                }
            }
        }
    }
    assert!(
        got_request,
        "server did not ask for a keyframe after a burst loss"
    );
    let ev = sim.take_events(s);
    let aus = received_aus(&ev);
    // 抜けの後はキーフレームから出る
    let mut prev = None;
    for (_, au) in &aus {
        let (_, n) = frame_id(&au.payload);
        if let Some(p) = prev
            && n != p + 1
        {
            assert!(au.keyframe, "resumed at frame {n} without a keyframe");
            assert!(au.discontinuity || n > p + 1);
        }
        prev = Some(n);
    }
    assert!(aus.len() > 80);
}

#[test]
fn rejects_offers_without_bundle_or_rtcp_mux() {
    let mut sim = Sim::new();
    let c = sim.add(client_config(), "10.0.0.1:5000");
    let s = sim.add(PeerConfig::default(), "10.0.0.2:6000");
    let now = sim.now;
    let offer = sim.pcs[c]
        .create_offer(now, &spec(Direction::SendOnly, true, &[], false))
        .unwrap();
    let no_bundle: String = offer
        .lines()
        .filter(|l| !l.starts_with("a=group:BUNDLE"))
        .map(|l| format!("{l}\r\n"))
        .collect();
    assert!(matches!(
        sim.pcs[s].accept_offer(now, &no_bundle),
        Err(WebRtcError::Negotiation(_))
    ));
    let no_mux: String = offer
        .lines()
        .filter(|l| *l != "a=rtcp-mux")
        .map(|l| format!("{l}\r\n"))
        .collect();
    assert!(sim.pcs[s].accept_offer(now, &no_mux).is_err());
    let no_fp: String = offer
        .lines()
        .filter(|l| !l.starts_with("a=fingerprint"))
        .map(|l| format!("{l}\r\n"))
        .collect();
    assert!(sim.pcs[s].accept_offer(now, &no_fp).is_err());
    // 正しい offer は通る
    assert!(sim.pcs[s].accept_offer(now, &offer).is_ok());
}

#[test]
fn fingerprint_mismatch_never_delivers_media() {
    let mut sim = Sim::new();
    let c = sim.add(client_config(), "10.0.0.1:5000");
    let s = sim.add(PeerConfig::default(), "10.0.0.2:6000");
    let now = sim.now;
    let offer = sim.pcs[c]
        .create_offer(now, &spec(Direction::SendOnly, true, &[], false))
        .unwrap();
    // 別の証明書の fingerprint に差し替える（中間者）
    let other = DtlsCertificate::generate_self_signed("mitm").unwrap();
    let bad: String = offer
        .lines()
        .map(|l| {
            if l.starts_with("a=fingerprint:") {
                format!("a=fingerprint:{}\r\n", other.sdp_fingerprint())
            } else {
                format!("{l}\r\n")
            }
        })
        .collect();
    let answer = sim.pcs[s].accept_offer(now, &bad).unwrap();
    sim.pcs[c].apply_answer(now, &answer).unwrap();
    sim.run(Duration::from_secs(3));
    assert!(!sim.pcs[s].is_connected());
    let ev = sim.take_events(s);
    assert!(
        ev.iter().any(|e| matches!(
            e,
            Event::Disconnected {
                reason: DisconnectReason::FingerprintMismatch
            }
        )),
        "{ev:?}"
    );
    assert!(
        !ev.iter()
            .any(|e| matches!(e, Event::Connected | Event::AccessUnit { .. }))
    );
    assert!(
        sim.pcs[c]
            .send_access_unit(sim.now, TrackId(1), &opus_au(0), None)
            .is_err()
    );
}

#[test]
fn ice_restart_via_sdpfrag_keeps_media_flowing() {
    let mut sim = Sim::new();
    let c = sim.add(client_config(), "10.0.0.1:5000");
    let s = sim.add(PeerConfig::default(), "10.0.0.2:6000");
    connect_pair(&mut sim, c, s, &spec(Direction::SendOnly, true, &[], false));
    let before = sim.pcs[s].local_ufrag().to_string();
    stream_both(&mut sim, c, 30, 1000);
    let now = sim.now;
    sim.pcs[c].restart_ice(now).unwrap();
    let frag = sim.pcs[c].local_sdpfrag();
    let resp = sim.pcs[s]
        .handle_trickle_patch(now, &frag)
        .unwrap()
        .expect("restart response");
    assert_ne!(sim.pcs[s].local_ufrag(), before);
    let parsed = SdpFrag::parse(&resp).unwrap();
    assert_eq!(parsed.credentials().unwrap().0, sim.pcs[s].local_ufrag());
    assert!(parsed.all_candidates().count() >= 1);
    assert_eq!(sim.pcs[c].handle_trickle_patch(now, &resp).unwrap(), None);
    // trickle だけ（同じ ufrag）は 204 相当
    let again = sim.pcs[c].local_sdpfrag();
    assert_eq!(sim.pcs[s].handle_trickle_patch(now, &again).unwrap(), None);
    // 違う ufrag で pwd なし → 409 相当
    assert!(
        sim.pcs[s]
            .handle_trickle_patch(
                now,
                "a=ice-ufrag:zzzz\r\nm=audio 9 UDP/TLS/RTP/SAVPF 0\r\na=mid:0\r\n"
            )
            .is_err()
    );
    sim.take_events(s);
    stream_both(&mut sim, c, 60, 1000);
    sim.run(Duration::from_millis(300));
    let ev = sim.take_events(s);
    let video = received_aus(&ev)
        .into_iter()
        .filter(|(_, a)| a.codec == Codec::Vp8)
        .count();
    assert!(video >= 58, "{video}");
    assert!(sim.pcs[s].is_connected() && sim.pcs[c].is_connected());
    assert!(matches!(
        sim.pcs[s].ice_state(),
        IceState::Connected | IceState::Completed
    ));
}

/// simulcast（3 層）で送り、Room で別の視聴者へ。視聴者への回線は 800 kbit/s。
#[test]
fn simulcast_sfu_with_bandwidth_estimation() {
    let mut sim = Sim::new();
    let publ = sim.add(client_config(), "10.0.0.1:5000");
    let s1 = sim.add(PeerConfig::default(), "10.0.0.2:6000");
    // 推定の始まりを回線より上にする（最初は f を選び、詰まって下げる）
    let s2 = sim.add(
        PeerConfig {
            bwe: BweConfig {
                start_bps: 3_000_000,
                ..Default::default()
            },
            ..Default::default()
        },
        "10.0.0.2:6001",
    );
    let view = sim.add(client_config(), "10.0.0.3:7000");
    connect_pair(
        &mut sim,
        publ,
        s1,
        &spec(Direction::SendOnly, false, &["q", "h", "f"], false),
    );
    // 視聴: server の offer（sendonly）に視聴者が answer（WHEP の server offer の形）
    let now = sim.now;
    let offer = sim.pcs[s2]
        .create_offer(now, &spec(Direction::SendOnly, false, &[], false))
        .unwrap();
    let answer = sim.pcs[view].accept_offer(now, &offer).unwrap();
    sim.pcs[s2].apply_answer(now, &answer).unwrap();
    wait_connected(&mut sim, &[s2, view]);
    {
        let l = sim.link(s2, view);
        l.rate = Some(800_000.0);
        l.delay = Duration::from_millis(20);
        l.max_queue = Duration::from_millis(300);
    }
    let layers: Vec<(TrackId, usize)> = ["q", "h", "f"]
        .iter()
        .zip([600usize, 2000, 6000])
        .map(|(r, sz)| (track_of(&sim, publ, MediaKind::Video, Some(r)), sz))
        .collect();
    let sub_track = track_of(&sim, s2, MediaKind::Video, None);
    let mut room = Room::new(RoomConfig::default());
    let mut subscribed = false;
    let mut kf: Vec<bool> = vec![true; 3];
    let mut forwarded_layers = Vec::new();
    let mut estimates = Vec::new();
    for n in 0..(30 * 25) as u32 {
        let now = sim.now;
        for (li, (t, size)) in layers.iter().enumerate() {
            let key = kf[li] || n % 300 == 0;
            kf[li] = false;
            let au = video_au(n, vp8_frame(li as u8, n, *size, key), key, li as u32 + 1);
            sim.pcs[publ].send_access_unit(now, *t, &au, None).unwrap();
        }
        sim.run(Duration::from_millis(33));
        // publisher 側の server: 受信トラックを Room へ
        for e in sim.take_events(s1) {
            match e {
                Event::Track(info) => {
                    room.add_publisher_track(
                        1,
                        info.track,
                        info.kind,
                        info.codec,
                        &info.mid,
                        info.rid.as_deref(),
                    );
                }
                Event::AccessUnit { track, au, config } => {
                    room.on_access_unit(sim.now, 1, track, &au, config.as_ref());
                }
                _ => {}
            }
        }
        if !subscribed && room.layer_bitrates(sim.now, 1, "0").len() == 3 {
            room.subscribe(sim.now, 2, sub_track, 1, "0");
            subscribed = true;
        }
        for e in sim.take_events(s2) {
            match e {
                Event::BandwidthEstimate { bps } => {
                    estimates.push((sim.now, bps));
                    room.set_bandwidth(sim.now, 2, bps);
                }
                Event::KeyframeRequest { track } => room.on_keyframe_request(sim.now, 2, track),
                _ => {}
            }
        }
        for e in sim.take_events(publ) {
            if let Event::KeyframeRequest { track } = e
                && let Some(i) = layers.iter().position(|l| l.0 == track)
            {
                kf[i] = true;
            }
        }
        if std::env::var("SIMDBG").is_ok() && n % 30 == 0 {
            eprintln!(
                "n {n} send {} est {} dbg {}",
                sim.pcs[s2].send_bitrate(),
                sim.pcs[s2].bandwidth_estimate(),
                sim.pcs[s2].debug_bwe()
            );
        }
        room.handle_timeout(sim.now);
        while let Some(o) = room.poll_output() {
            let now = sim.now;
            match o {
                RoomOutput::Forward {
                    track, au, config, ..
                } => {
                    let _ = sim.pcs[s2].send_access_unit(now, track, &au, config.as_ref());
                }
                RoomOutput::KeyframeRequest { track, .. } => {
                    sim.pcs[s1].request_keyframe(now, track).unwrap();
                }
                RoomOutput::LayerChanged { rid, .. } => forwarded_layers.push((now, rid)),
                RoomOutput::DesiredBitrate { bps, .. } => sim.pcs[s2].set_desired_bitrate(bps),
            }
        }
    }
    sim.run(Duration::from_millis(500));
    if std::env::var("SIMDBG").is_ok() {
        let t0 = estimates[0].0;
        for (t, b) in &estimates {
            eprintln!("{:6.2} {b}", (*t - t0).as_secs_f64());
        }
        eprintln!("layers {:?}", room.layer_bitrates(sim.now, 1, "0"));
        eprintln!("send {} lost {}", sim.pcs[s2].send_bitrate(), sim.lost);
    }
    // 推定は 800 kbit/s の回線に収まる
    let last = estimates.last().unwrap().1;
    let tail: Vec<u64> = estimates
        .iter()
        .filter(|(t, _)| *t > sim.now - Duration::from_secs(8))
        .map(|x| x.1)
        .collect();
    let avg = tail.iter().sum::<u64>() / tail.len().max(1) as u64;
    assert!(
        (350_000..=950_000).contains(&avg),
        "estimate avg {avg} last {last} {:?}",
        &estimates[estimates.len().saturating_sub(20)..]
    );
    // 落ち着いた層は h（500 kbit/s）
    assert_eq!(
        room.current_layer(2, sub_track),
        Some(Some("h".into())),
        "{forwarded_layers:?}"
    );
    // 視聴者の受けたもの: 層が変わるところは必ずキーフレーム、層の中は抜けない
    let ev = sim.take_events(view);
    let aus = received_aus(&ev);
    assert!(aus.len() > 400, "viewer got {}", aus.len());
    let mut prev: Option<(u8, u32)> = None;
    let mut switches = 0;
    for (_, au) in &aus {
        let id = frame_id(&au.payload);
        if let Some(p) = prev {
            if p.0 != id.0 {
                switches += 1;
                assert!(au.keyframe, "layer switch {p:?} -> {id:?} without keyframe");
            }
            // 同じ撮影時刻のフレームで層が替わることはある
            assert!(
                id.1 > p.1 || (id.0 != p.0 && id.1 == p.1),
                "frames out of order {p:?} -> {id:?}"
            );
        }
        prev = Some(id);
    }
    assert!(switches >= 1);
    // 視聴者の受信トラックは 1 本（層が変わっても SSRC は同じ）
    let tracks = ev.iter().filter(|e| matches!(e, Event::Track(_))).count();
    assert_eq!(tracks, 1);
    // 時刻は単調
    for w in aus.windows(2) {
        assert!(w[1].1.pts > w[0].1.pts);
    }
}

/// 受けた packet の parser を壊れた入力で叩く（接続前・接続後）。
#[test]
fn fuzz_datagrams_and_sdp() {
    let mut sim = Sim::new();
    let c = sim.add(client_config(), "10.0.0.1:5000");
    let s = sim.add(PeerConfig::default(), "10.0.0.2:6000");
    let now = sim.now;
    let offer = sim.pcs[c]
        .create_offer(now, &spec(Direction::SendRecv, true, &["a", "b"], true))
        .unwrap();
    let mut rng = Rng(77);
    // SDP の変異
    for _ in 0..400 {
        let mut b = offer.clone().into_bytes();
        for _ in 0..(1 + rng.next() % 4) {
            let k = rng.next() as usize % b.len();
            b[k] = (rng.next() % 128) as u8;
        }
        if let Ok(t) = String::from_utf8(b) {
            let mut pc = PeerConnection::new(PeerConfig::default(), now).unwrap();
            pc.add_host_address(now, "10.0.0.9:1".parse().unwrap())
                .unwrap();
            let _ = pc.accept_offer(now, &t);
            let _ = pc.handle_trickle_patch(now, &t);
        }
    }
    let answer = sim.pcs[s].accept_offer(now, &offer).unwrap();
    sim.pcs[c].apply_answer(now, &answer).unwrap();
    let from: SocketAddr = "10.0.0.1:5000".parse().unwrap();
    let fuzz = |pc: &mut PeerConnection, rng: &mut Rng, now: Instant| {
        for _ in 0..2000 {
            let len = (rng.next() % 300) as usize;
            let mut v: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            if let Some(b) = v.first_mut() {
                // STUN・DTLS・RTP の範囲を多めに
                *b = match rng.next() % 4 {
                    0 => (*b) & 3,
                    1 => 20 + (*b % 44),
                    2 => 128 + (*b % 64),
                    _ => *b,
                };
            }
            pc.handle_datagram(now, from, &v);
            while pc.poll_transmit().is_some() {}
            while pc.poll_event().is_some() {}
        }
    };
    fuzz(&mut sim.pcs[s], &mut rng, now);
    wait_connected(&mut sim, &[c, s]);
    let now = sim.now;
    fuzz(&mut sim.pcs[s], &mut rng, now);
    // 復号の後の parser（RTP・RTCP）を平文で叩く
    for _ in 0..3000 {
        let len = (rng.next() % 200) as usize + 2;
        let mut v: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        v[0] = 0x80 | (v[0] & 0x3f);
        if rng.next().is_multiple_of(2) {
            v[1] = 200 + (v[1] % 8);
            sim.pcs[s].inject_plain_rtcp(now, &v);
        } else {
            v[1] &= 0x7f;
            sim.pcs[s].inject_plain_rtp(now, &v);
        }
    }
    sim.pcs[s].handle_timeout(now + Duration::from_secs(1));
    // それでもつながったまま
    assert!(sim.pcs[s].is_connected());
}

#[test]
fn datachannel_messages_both_ways() {
    let mut sim = Sim::new();
    let c = sim.add(client_config(), "10.0.0.1:5000");
    let s = sim.add(PeerConfig::default(), "10.0.0.2:6000");
    let now = sim.now;
    // つながる前に開く
    assert_eq!(
        sim.pcs[c]
            .create_data_channel(
                now,
                ChannelOptions {
                    label: "chat".into(),
                    ..Default::default()
                }
            )
            .unwrap(),
        None
    );
    connect_pair(&mut sim, c, s, &spec(Direction::SendOnly, false, &[], true));
    sim.run(Duration::from_secs(2));
    let ev = sim.take_events(s);
    let open = ev
        .iter()
        .find_map(|e| match e {
            Event::DataChannelOpen(i) => Some(i.clone()),
            _ => None,
        })
        .expect("server saw no DataChannelOpen");
    assert_eq!(open.label, "chat");
    let id = open.id;
    let cev = sim.take_events(c);
    assert!(
        cev.iter()
            .any(|e| matches!(e, Event::DataChannelOpen(i) if i.id == id))
    );
    let now = sim.now;
    sim.pcs[c].send_data_text(now, id, "hello").unwrap();
    sim.pcs[c].send_data_text(now, id, "").unwrap();
    sim.pcs[c]
        .send_data_binary(now, id, Bytes::from(vec![7u8; 100_000]))
        .unwrap();
    sim.run(Duration::from_secs(1));
    let msgs: Vec<(Bytes, bool)> = sim
        .take_events(s)
        .into_iter()
        .filter_map(|e| match e {
            Event::DataChannelMessage { data, binary, .. } => Some((data, binary)),
            _ => None,
        })
        .collect();
    assert_eq!(msgs.len(), 3);
    assert_eq!(&msgs[0].0[..], b"hello");
    assert!(!msgs[0].1);
    assert!(msgs[1].0.is_empty());
    assert_eq!(msgs[2].0.len(), 100_000);
    assert!(msgs[2].1);
    // server から字幕を送る（R6-4）
    let now = sim.now;
    let cap = sim.pcs[s]
        .create_data_channel(
            now,
            ChannelOptions {
                label: "captions".into(),
                ordered: true,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(cap % 2, 1, "server (DTLS server) uses odd ids");
    sim.run(Duration::from_millis(300));
    let now = sim.now;
    sim.pcs[s]
        .send_data_text(now, cap, r#"{"text":"こんにちは","start":1.0}"#)
        .unwrap();
    sim.run(Duration::from_millis(300));
    let cev = sim.take_events(c);
    assert!(
        cev.iter()
            .any(|e| matches!(e, Event::DataChannelOpen(i) if i.label == "captions"))
    );
    assert!(cev.iter().any(|e| matches!(e, Event::DataChannelMessage { data, .. } if std::str::from_utf8(data).unwrap().contains("こんにちは"))));
    // 閉じる
    let now = sim.now;
    sim.pcs[c].close_data_channel(now, id).unwrap();
    sim.run(Duration::from_secs(1));
    assert!(
        sim.take_events(s)
            .iter()
            .any(|e| matches!(e, Event::DataChannelClosed { id: x } if *x == id))
    );
}

#[test]
fn close_notifies_peer() {
    let mut sim = Sim::new();
    let c = sim.add(client_config(), "10.0.0.1:5000");
    let s = sim.add(PeerConfig::default(), "10.0.0.2:6000");
    connect_pair(&mut sim, c, s, &spec(Direction::SendOnly, true, &[], false));
    let now = sim.now;
    sim.pcs[c].close(now);
    sim.run(Duration::from_secs(1));
    let ev = sim.take_events(s);
    assert!(
        ev.iter().any(|e| matches!(
            e,
            Event::Disconnected {
                reason: DisconnectReason::DtlsClosed
            }
        )),
        "{ev:?}"
    );
}
