//! 本物のピア接続（senca-webrtc）で部屋をつなぐ試験。ネットワークは記憶の中（時刻は仮想）。
//!
//! - B（sendrecv、映像 1 層・音声・#chat）、C（recvonly、#chat）、A（sendonly、サイマルキャスト 3 層・音声）の順に参加。
//!   A が来たとき B と C は枠が足りず `renegotiate` → offer し直す（DataChannel の m 行は同じ位置のまま）。
//! - 受け手が受けた映像のフレームに書いた (層, 番号) で、誰の・どの層が届いたかを確かめる。
//! - C が `layer` で A の層を r2 → r0 と選ぶと、キーフレームで切り替わる（壊れた GOP を配らない）。
//! - B の #chat の message は C に届き、C から B へも届く。

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::{Value, json};
use senca_core::media::{AccessUnit, Codec, MediaKind, TrackId};
use senca_core::time::Timebase;
use senca_rooms::host::PeerHost;
use senca_rooms::service::{Action, ConnKey, RoomService, ServiceConfig};
use senca_rooms::signaling::{ServerMessage, Slot};
use senca_rooms::token::{Audience, JoinClaims, TokenSigner};
use senca_rooms::Role;
use senca_webrtc::{
    ChannelOptions, DtlsCertificate, Event, OfferMediaSpec, OfferSpec, PeerConfig, PeerConnection,
};
use senca_sdp::Direction;

const UNIX_MS: i64 = 1_767_222_000_000;

fn vp8_frame(layer: u8, n: u32, size: usize, key: bool) -> Bytes {
    let mut v = vec![0u8; size.max(16)];
    if key {
        v[..10].copy_from_slice(&[0x10, 0, 0, 0x9d, 0x01, 0x2a, 0x80, 0x02, 0xe0, 0x01]);
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

fn video_au(n: u32, payload: Bytes, key: bool) -> AccessUnit {
    AccessUnit {
        track: TrackId(0),
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

fn opus_au(n: u32, who: u8) -> AccessUnit {
    AccessUnit {
        track: TrackId(0),
        codec: Codec::Opus,
        epoch: 0,
        sequence: n as u64,
        config_generation: 0,
        pts: n as i64 * 1584,
        dts: n as i64 * 1584,
        duration: 1584,
        timebase: Timebase::new(1, 48000),
        keyframe: true,
        discontinuity: false,
        capture_time_ns: None,
        payload: Bytes::from(vec![0xFC, who, (n & 0xff) as u8, 4, 5, 6, 7, 8]),
    }
}

/// 受けた映像のフレーム。
#[derive(Debug, Clone)]
struct Got {
    mid: String,
    layer: u8,
    n: u32,
    key: bool,
}

struct Client {
    name: &'static str,
    pc: PeerConnection,
    addr: SocketAddr,
    key: ConnKey,
    /// 送る m 行（先頭）と受ける m 行（後ろ）。
    spec: OfferSpec,
    sends_video: bool,
    connection_id: String,
    slots: BTreeMap<String, Slot>,
    recv_track_mid: HashMap<TrackId, String>,
    got_video: Vec<Got>,
    got_audio: Vec<(String, u8)>,
    got_data: Vec<String>,
    kf: HashMap<TrackId, bool>,
    renegotiations: u32,
    offering: bool,
    pending_reneg: Option<(u32, u32)>,
    answers: u32,
    msgs: Vec<ServerMessage>,
}

/// 届く先（`None` はサーバー）、送り元、中身。
type Packet = (Option<usize>, SocketAddr, Vec<u8>);

struct World {
    now: Instant,
    svc: RoomService,
    host: PeerHost,
    server: SocketAddr,
    clients: Vec<Client>,
    queue: BTreeMap<(Instant, u64), Packet>,
    seq: u64,
    delay: Duration,
    signer: TokenSigner,
    events: Vec<String>,
}

impl World {
    fn new() -> World {
        let signer = TokenSigner::ed25519("k1", &[1u8; 32]);
        let now = Instant::now();
        let svc = RoomService::new(
            ServiceConfig {
                node_id: "node-a".into(),
                token_verifier: Some(signer.verifier("senca", "senca-rooms")),
                ..Default::default()
            },
            now,
            UNIX_MS,
        );
        let server: SocketAddr = "10.0.1.1:6000".parse().unwrap();
        let base = PeerConfig {
            certificate: Some(DtlsCertificate::generate_self_signed("senca-rooms").unwrap()),
            ..Default::default()
        };
        World {
            now,
            svc,
            host: PeerHost::new(base, server),
            server,
            clients: Vec::new(),
            queue: BTreeMap::new(),
            seq: 0,
            delay: Duration::from_millis(10),
            signer,
            events: Vec::new(),
        }
    }

    fn join(&mut self, name: &'static str, role: Role, video: Option<&[&str]>, audio: bool) -> usize {
        let i = self.clients.len();
        let addr: SocketAddr = format!("10.0.0.{}:5000", i + 1).parse().unwrap();
        let mut pc = PeerConnection::new(PeerConfig { ice_lite: false, ..Default::default() }, self.now).unwrap();
        pc.add_host_address(self.now, addr).unwrap();
        pc.create_data_channel(self.now, ChannelOptions { label: "#chat".into(), ..Default::default() })
            .unwrap();
        let mut media = Vec::new();
        if let Some(rids) = video {
            media.push(OfferMediaSpec {
                kind: MediaKind::Video,
                direction: Direction::SendOnly,
                codecs: Some(vec![Codec::Vp8]),
                simulcast: rids.iter().map(|r| r.to_string()).collect(),
            });
        }
        if audio {
            media.push(OfferMediaSpec {
                kind: MediaKind::Audio,
                direction: Direction::SendOnly,
                codecs: Some(vec![Codec::Opus]),
                simulcast: Vec::new(),
            });
        }
        let key = self.svc.open(self.now, Default::default());
        let claims = JoinClaims {
            iss: "senca".into(),
            aud: Audience::One("senca-rooms".into()),
            exp: UNIX_MS / 1000 + 600,
            nbf: None,
            iat: Some(UNIX_MS / 1000),
            jti: None,
            sub: None,
            room: "e2e".into(),
            role: Role::Sendrecv,
            client_id: Some(name.into()),
            bundle_id: None,
            metadata: Some(json!({"name": name})),
            permissions: None,
        };
        let token = self.signer.sign(&claims).unwrap();
        let mut connect = json!({
            "type": "connect", "version": 1, "room_id": "e2e", "role": role, "token": token,
            "client_id": name, "data_channels": [{"label": "#chat"}],
        });
        if let Some(r) = video.filter(|r| !r.is_empty()) {
            connect["simulcast"] = json!({"rids": r});
        }
        self.clients.push(Client {
            name,
            pc,
            addr,
            key,
            spec: OfferSpec { media, datachannel: true },
            sends_video: video.is_some(),
            connection_id: String::new(),
            slots: BTreeMap::new(),
            recv_track_mid: HashMap::new(),
            got_video: Vec::new(),
            got_audio: Vec::new(),
            got_data: Vec::new(),
            kf: HashMap::new(),
            renegotiations: 0,
            offering: false,
            pending_reneg: None,
            answers: 0,
            msgs: Vec::new(),
        });
        self.svc.on_signaling_text(self.now, key, &connect.to_string());
        self.run(Duration::from_millis(1));
        i
    }

    fn client_of(&self, k: ConnKey) -> Option<usize> {
        self.clients.iter().position(|c| c.key == k)
    }

    fn signal(&mut self, i: usize, v: Value) {
        let k = self.clients[i].key;
        self.svc.on_signaling_text(self.now, k, &v.to_string());
    }

    /// 受信の枠を `audio`・`video` まで足して offer する。
    fn offer(&mut self, i: usize, want: Option<(u32, u32)>) {
        let c = &mut self.clients[i];
        if let Some((a, v)) = want {
            for (kind, n) in [(MediaKind::Audio, a), (MediaKind::Video, v)] {
                let have = c.spec.media.iter().filter(|m| m.kind == kind && m.direction == Direction::RecvOnly).count() as u32;
                for _ in have..n {
                    c.spec.media.push(OfferMediaSpec {
                        kind,
                        direction: Direction::RecvOnly,
                        codecs: Some(vec![if kind == MediaKind::Video { Codec::Vp8 } else { Codec::Opus }]),
                        simulcast: Vec::new(),
                    });
                }
            }
        }
        let sdp = c.pc.create_offer(self.now, &c.spec).unwrap();
        c.offering = true;
        self.signal(i, json!({"type": "offer", "sdp": sdp}));
    }

    fn on_server_message(&mut self, i: usize, m: ServerMessage) {
        self.clients[i].msgs.push(m.clone());
        match m {
            ServerMessage::Connected(c) => {
                self.clients[i].connection_id = c.connection_id.clone();
                let want = (c.recv_slots.audio, c.recv_slots.video);
                self.offer(i, Some(want));
            }
            ServerMessage::Answer(a) => {
                let now = self.now;
                let c = &mut self.clients[i];
                c.pc.apply_answer(now, &a.sdp).unwrap();
                c.offering = false;
                c.answers += 1;
                for s in a.slots {
                    c.slots.insert(s.mid.clone(), s);
                }
                if let Some(w) = c.pending_reneg.take() {
                    self.clients[i].renegotiations += 1;
                    self.offer(i, Some(w));
                }
            }
            ServerMessage::Slots(s) => {
                for s in s.slots {
                    self.clients[i].slots.insert(s.mid.clone(), s);
                }
            }
            ServerMessage::Renegotiate(r) => {
                if self.clients[i].offering {
                    self.clients[i].pending_reneg = Some((r.audio, r.video));
                } else {
                    self.clients[i].renegotiations += 1;
                    self.offer(i, Some((r.audio, r.video)));
                }
            }
            ServerMessage::Ping(_) => self.signal(i, json!({"type": "pong"})),
            ServerMessage::Error(e) => panic!("{} got error {e:?}", self.clients[i].name),
            _ => {}
        }
    }

    fn flush(&mut self) -> bool {
        let mut acts = Vec::new();
        let now = self.now;
        self.host.pump(now, &mut self.svc, |a| acts.push(a));
        let progressed = !acts.is_empty();
        for a in acts {
            match a {
                Action::Signal { conn, text, .. } => {
                    let m: ServerMessage = serde_json::from_str(&text).unwrap();
                    if let Some(i) = self.client_of(conn) {
                        self.on_server_message(i, m);
                    }
                }
                Action::Event(e) => self.events.push(e.event.type_name().to_string()),
                Action::CloseSignaling { .. } => {}
                other => panic!("unexpected action {other:?}"),
            }
        }
        while let Some(t) = self.host.poll_transmit() {
            let dst = self.clients.iter().position(|c| c.addr == t.remote);
            self.seq += 1;
            self.queue.insert((self.now + self.delay, self.seq), (dst, self.server, t.data.to_vec()));
        }
        for i in 0..self.clients.len() {
            while let Some(t) = self.clients[i].pc.poll_transmit() {
                if t.remote == self.server {
                    self.seq += 1;
                    let from = self.clients[i].addr;
                    self.queue.insert((self.now + self.delay, self.seq), (None, from, t.data.to_vec()));
                }
            }
            while let Some(e) = self.clients[i].pc.poll_event() {
                self.on_client_event(i, e);
            }
        }
        progressed
    }

    fn on_client_event(&mut self, i: usize, e: Event) {
        let c = &mut self.clients[i];
        match e {
            Event::Track(info) => {
                c.recv_track_mid.insert(info.track, info.mid.clone());
            }
            Event::AccessUnit { track, au, .. } => {
                let mid = c.recv_track_mid.get(&track).cloned().unwrap_or_default();
                if au.codec == Codec::Vp8 {
                    c.got_video.push(Got {
                        mid,
                        layer: au.payload[10],
                        n: u32::from_be_bytes([au.payload[11], au.payload[12], au.payload[13], au.payload[14]]),
                        key: au.keyframe,
                    });
                } else {
                    c.got_audio.push((mid, au.payload[1]));
                }
            }
            Event::KeyframeRequest { track } => {
                c.kf.insert(track, true);
            }
            Event::DataChannelMessage { data, .. } => c.got_data.push(String::from_utf8_lossy(&data).into_owned()),
            _ => {}
        }
    }

    fn run(&mut self, d: Duration) {
        let until = self.now + d;
        for _ in 0..1_000_000 {
            while self.flush() {}
            let mut next = self.queue.keys().next().map(|k| k.0);
            let mut take = |t: Option<Instant>| {
                if let Some(t) = t {
                    next = Some(next.map_or(t, |n: Instant| n.min(t)));
                }
            };
            take(self.host.poll_timeout());
            take(self.svc.poll_timeout());
            for c in &self.clients {
                take(c.pc.poll_timeout());
            }
            let Some(next) = next else { break };
            if next > until {
                break;
            }
            self.now = self.now.max(next);
            while let Some((&k, _)) = self.queue.iter().next() {
                if k.0 > self.now {
                    break;
                }
                let (dst, from, data) = self.queue.remove(&k).unwrap();
                match dst {
                    Some(i) => {
                        let now = self.now;
                        self.clients[i].pc.handle_datagram(now, from, &data);
                    }
                    None => {
                        self.host.handle_datagram(self.now, from, &data);
                    }
                }
            }
            self.host.handle_timeout(self.now);
            self.svc.handle_timeout(self.now);
            for c in &mut self.clients {
                if c.pc.poll_timeout().is_some_and(|t| t <= self.now) {
                    c.pc.handle_timeout(self.now);
                }
            }
        }
        self.now = until.max(self.now);
        while self.flush() {}
    }

    fn send_tracks(&self, i: usize, kind: MediaKind) -> Vec<(Option<String>, TrackId)> {
        self.clients[i]
            .pc
            .send_tracks()
            .into_iter()
            .filter(|t| t.kind == kind)
            .map(|t| (t.rid, t.track))
            .collect()
    }

    /// 映像 30 fps（層ごとの大きさ）と音声を `frames` フレーム送る。
    fn stream(&mut self, frames: u32, start: u32) {
        for n in start..start + frames {
            for i in 0..self.clients.len() {
                if !self.clients[i].sends_video || !self.clients[i].pc.is_connected() {
                    continue;
                }
                let tracks = self.send_tracks(i, MediaKind::Video);
                for (li, (rid, t)) in tracks.iter().enumerate() {
                    let layer = match rid.as_deref() {
                        Some("r0") => 0,
                        Some("r1") => 1,
                        Some("r2") => 2,
                        _ => 7,
                    };
                    let size = [500usize, 1500, 4000, 0, 0, 0, 0, 2000][layer as usize];
                    let key = n % 90 == 0 || self.clients[i].kf.remove(t).is_some();
                    let _ = li;
                    let now = self.now;
                    self.clients[i].pc.send_access_unit(now, *t, &video_au(n, vp8_frame(layer, n, size, key), key), None).unwrap();
                }
                for (_, t) in self.send_tracks(i, MediaKind::Audio) {
                    let now = self.now;
                    let who = i as u8;
                    self.clients[i].pc.send_access_unit(now, t, &opus_au(n, who), None).unwrap();
                }
            }
            self.run(Duration::from_millis(33));
        }
    }
}

fn mid_of(c: &Client, connection_id: &str, kind: &str) -> String {
    c.slots
        .values()
        .find(|s| s.connection_id.as_deref() == Some(connection_id) && serde_json::to_value(s.kind).unwrap() == kind)
        .unwrap_or_else(|| panic!("{}: no {kind} slot for {connection_id}: {:?}", c.name, c.slots))
        .mid
        .clone()
}

#[test]
fn three_participants_route_media_switch_layers_and_messages() {
    let mut w = World::new();
    let b = w.join("bob", Role::Sendrecv, Some(&[]), true);
    w.run(Duration::from_millis(300));
    let c = w.join("carol", Role::Recvonly, None, false);
    w.run(Duration::from_millis(300));
    let a = w.join("alice", Role::Sendonly, Some(&["r0", "r1", "r2"]), true);
    w.run(Duration::from_millis(1500));
    for x in [a, b, c] {
        assert!(w.clients[x].pc.is_connected(), "{} not connected", w.clients[x].name);
        assert!(!w.clients[x].offering, "{} still negotiating", w.clients[x].name);
    }
    // B と C は A が来たときに再交渉した
    assert!(w.clients[b].renegotiations >= 1);
    assert!(w.clients[c].renegotiations >= 1);
    assert_eq!(w.clients[a].renegotiations, 0);
    let (aid, bid) = (w.clients[a].connection_id.clone(), w.clients[b].connection_id.clone());
    w.stream(120, 0);

    // --- 配った先
    let c_a_video = mid_of(&w.clients[c], &aid, "video");
    let c_b_video = mid_of(&w.clients[c], &bid, "video");
    let b_a_video = mid_of(&w.clients[b], &aid, "video");
    let c_b_audio = mid_of(&w.clients[c], &bid, "audio");
    let count = |x: usize, mid: &str| w.clients[x].got_video.iter().filter(|g| g.mid == mid).count();
    assert!(count(c, &c_a_video) > 60, "carol got {} of alice's frames", count(c, &c_a_video));
    assert!(count(c, &c_b_video) > 60, "carol got {} of bob's frames", count(c, &c_b_video));
    assert!(count(b, &b_a_video) > 60, "bob got {} of alice's frames", count(b, &b_a_video));
    assert!(w.clients[a].got_video.is_empty(), "alice is sendonly");
    // B の映像は層 7（サイマルキャストでない）
    assert!(w.clients[c].got_video.iter().filter(|g| g.mid == c_b_video).all(|g| g.layer == 7));
    // 音声も届く（C は B の音声を B の枠で）
    assert!(w.clients[c].got_audio.iter().filter(|x| x.0 == c_b_audio).all(|x| x.1 == b as u8));
    assert!(w.clients[c].got_audio.iter().filter(|x| x.0 == c_b_audio).count() > 60);
    // 最初に配ったフレームはキーフレーム
    let first = w.clients[c].got_video.iter().find(|g| g.mid == c_a_video).unwrap();
    assert!(first.key, "{first:?}");

    // --- 層の切り替え（C が A の層を選ぶ）
    let a_track = w.clients[c].slots[&c_a_video].track_id.clone().unwrap();
    let before = w.clients[c].got_video.len();
    w.signal(c, json!({"type": "layer", "track_id": a_track, "rid": "r2"}));
    w.stream(60, 120);
    let after: Vec<Got> = w.clients[c].got_video[before..].iter().filter(|g| g.mid == c_a_video).cloned().collect();
    assert_eq!(after.last().unwrap().layer, 2, "{after:?}");
    let before = w.clients[c].got_video.len();
    w.signal(c, json!({"type": "layer", "track_id": a_track, "rid": "r0"}));
    w.stream(60, 180);
    let after2: Vec<Got> = w.clients[c].got_video[before..].iter().filter(|g| g.mid == c_a_video).cloned().collect();
    assert_eq!(after2.last().unwrap().layer, 0, "{after2:?}");
    // 層の変わり目は必ずキーフレーム、層の中で番号は増え続ける
    let seq: Vec<&Got> = w.clients[c].got_video.iter().filter(|g| g.mid == c_a_video).collect();
    let mut switches = 0;
    for p in seq.windows(2) {
        if p[0].layer != p[1].layer {
            switches += 1;
            assert!(p[1].key, "layer switch {:?} -> {:?} without keyframe", p[0], p[1]);
        }
        assert!(p[1].n >= p[0].n, "frames out of order {:?} -> {:?}", p[0], p[1]);
    }
    assert!(switches >= 2, "{switches} switches");
    let notes = w.clients[c]
        .msgs
        .iter()
        .filter(|m| matches!(m, ServerMessage::Notify(senca_rooms::signaling::Notify::LayerChanged { .. })))
        .count();
    assert!(notes >= 2);

    // --- DataChannel（#chat）
    for (from, text) in [(b, "hello from bob"), (c, "hello from carol")] {
        let id = w.clients[from].pc.data_channel_by_label("#chat").expect("chat open");
        let now = w.now;
        w.clients[from].pc.send_data_text(now, id, text).unwrap();
    }
    w.run(Duration::from_millis(500));
    assert!(w.clients[c].got_data.contains(&"hello from bob".to_string()), "{:?}", w.clients[c].got_data);
    assert!(w.clients[b].got_data.contains(&"hello from carol".to_string()), "{:?}", w.clients[b].got_data);
    assert!(w.clients[a].got_data.contains(&"hello from bob".to_string()));
    assert!(!w.clients[b].got_data.contains(&"hello from bob".to_string()), "not echoed");

    // --- 統計
    let st = w.svc.api_stats(w.now, "e2e").unwrap();
    let cs = st.participants.iter().find(|p| p.connection_id == w.clients[c].connection_id).unwrap();
    assert!(cs.forwarded_bytes > 100_000);
    assert!(cs.subscriptions.iter().any(|s| s.rid.as_deref() == Some("r0")));
    let as_ = st.participants.iter().find(|p| p.connection_id == aid).unwrap();
    let layers = &as_.published.iter().find(|t| t.kind == senca_rooms::Kind::Video).unwrap().layers;
    assert_eq!(layers.len(), 3);

    // --- A が抜けると、B と C に知らせ、配らなくなる
    w.signal(a, json!({"type": "disconnect"}));
    w.run(Duration::from_millis(200));
    let n_before = w.clients[c].got_video.iter().filter(|g| g.mid == c_a_video).count();
    w.stream(30, 240);
    let n_after = w.clients[c].got_video.iter().filter(|g| g.mid == c_a_video).count();
    assert_eq!(n_before, n_after);
    assert!(w.clients[c].slots[&c_a_video].track_id.is_none());
    for t in ["room.created", "connection.created", "track.published", "connection.destroyed", "track.unpublished"] {
        assert!(w.events.iter().any(|e| e == t), "no {t} event in {:?}", w.events);
    }
}
