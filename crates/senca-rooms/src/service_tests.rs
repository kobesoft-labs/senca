//! 状態機械の試験（ピア接続は偽の出来事で代える。本物のピア接続での試験は tests/e2e.rs）。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::{Value, json};
use senca_core::media::{AccessUnit, Codec, MediaKind, TrackId};
use senca_core::time::Timebase;
use senca_webrtc::{ChannelInfo, Event as PcEvent, Reliability, SendTrackInfo, TrackInfo as PcTrackInfo};

use crate::api::{CreateRoom, IssueToken, PostMessage, UpdateParticipant, UpdateRoom};
use crate::model::{Kind, Role, RoomSettings, SpotlightSettings, Unfocused};
use crate::service::{Action, AuthWebhookMode, ConnKey, RelayAction, RoomService, ServiceConfig, SignalPath};
use crate::signaling::{ErrorCode, Notify, ParticipantInfo, ServerMessage, SlotState, TrackInfo};
use crate::token::{Audience, JoinClaims, TokenSigner};

struct H {
    svc: RoomService,
    now: Instant,
    signer: TokenSigner,
    /// 接続ごとに受けたシグナリングの message。
    inbox: BTreeMap<ConnKey, Vec<(SignalPath, ServerMessage)>>,
    other: Vec<Action>,
    closed: Vec<(ConnKey, u16)>,
    next_track: u32,
}

const UNIX_MS: i64 = 1_767_222_000_000;

impl H {
    fn new(f: impl FnOnce(&mut ServiceConfig)) -> H {
        let signer = TokenSigner::ed25519("k1", &[7u8; 32]);
        let mut cfg = ServiceConfig {
            node_id: "node-a".into(),
            token_verifier: Some(signer.verifier("senca", "senca-rooms")),
            token_signer: Some(signer.clone()),
            ..Default::default()
        };
        f(&mut cfg);
        let now = Instant::now();
        H {
            svc: RoomService::new(cfg, now, UNIX_MS),
            now,
            signer,
            inbox: BTreeMap::new(),
            other: Vec::new(),
            closed: Vec::new(),
            next_track: 1000,
        }
    }

    fn token(&self, room: &str, role: Role, client_id: Option<&str>, extra: Value) -> String {
        let mut c = serde_json::to_value(JoinClaims {
            iss: "senca".into(),
            aud: Audience::One("senca-rooms".into()),
            exp: UNIX_MS / 1000 + 3600,
            nbf: None,
            iat: Some(UNIX_MS / 1000),
            jti: None,
            sub: None,
            room: room.into(),
            role,
            client_id: client_id.map(str::to_string),
            bundle_id: None,
            metadata: None,
            permissions: None,
        })
        .unwrap();
        if let (Some(m), Some(e)) = (c.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                m.insert(k.clone(), v.clone());
            }
        }
        self.signer.sign_value(&c).unwrap()
    }

    fn drain(&mut self) {
        while let Some(a) = self.svc.poll_action() {
            match a {
                Action::Signal { conn, path, text } => {
                    let m: ServerMessage = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
                    self.inbox.entry(conn).or_default().push((path, m));
                }
                Action::CloseSignaling { conn, code, .. } => self.closed.push((conn, code)),
                other => self.other.push(other),
            }
        }
    }

    fn take(&mut self, k: ConnKey) -> Vec<ServerMessage> {
        self.drain();
        self.inbox.remove(&k).unwrap_or_default().into_iter().map(|x| x.1).collect()
    }

    fn take_other(&mut self) -> Vec<Action> {
        self.drain();
        std::mem::take(&mut self.other)
    }

    fn send(&mut self, k: ConnKey, v: Value) {
        self.svc.on_signaling_text(self.now, k, &v.to_string());
        self.drain();
    }

    /// 参加する（`connected` まで）。
    fn join(&mut self, room: &str, role: Role, client: &str, extra: Value) -> (ConnKey, Value) {
        let k = self.svc.open(self.now, Default::default());
        // bundle_id は発行者がトークンに入れる（利用者が名乗った値は信じない）
        let claims_extra = extra.get("bundle_id").map(|b| json!({ "bundle_id": b })).unwrap_or(json!({}));
        let tok = self.token(room, Role::Sendrecv, Some(client), claims_extra);
        let mut c = json!({"type":"connect","version":1,"room_id":room,"role":role,"token":tok,"client_id":client});
        if let (Some(m), Some(e)) = (c.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                m.insert(k.clone(), v.clone());
            }
        }
        self.send(k, c);
        let msgs = self.take(k);
        let connected = msgs
            .iter()
            .find_map(|m| match m {
                ServerMessage::Connected(c) => Some(serde_json::to_value(c).unwrap()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("not connected: {msgs:?}"));
        (k, connected)
    }

    /// offer を出し、偽の answer で答える。`lines` は (kind, 方向, rids)、mid は順番。
    fn offer(&mut self, k: ConnKey, lines: &[(&str, &str, &[&str])]) -> Vec<ServerMessage> {
        self.send(k, json!({"type":"offer","sdp": sdp(lines)}));
        let acts = self.take_other();
        assert!(
            acts.iter().any(|a| matches!(a, Action::AcceptOffer { conn, .. } if *conn == k)),
            "no AcceptOffer: {acts:?} {:?}",
            self.inbox.get(&k)
        );
        let mut tracks = Vec::new();
        for (i, (kind, dir, _)) in lines.iter().enumerate() {
            if *dir == "recvonly" || *dir == "sendrecv" {
                self.next_track += 1;
                tracks.push(SendTrackInfo {
                    track: TrackId(self.next_track),
                    mid: i.to_string(),
                    rid: None,
                    kind: if *kind == "video" { MediaKind::Video } else { MediaKind::Audio },
                    ssrc: self.next_track,
                    rtx_ssrc: None,
                    codecs: vec![],
                });
            }
        }
        self.svc.on_offer_result(self.now, k, Ok("v=0\r\n".into()), &tracks);
        self.take(k)
    }

    /// 送り手の受信トラックが始まった（PeerConnection の Event::Track）。
    fn track_event(&mut self, k: ConnKey, mid: &str, kind: MediaKind, rid: Option<&str>, track: u32) {
        self.svc.on_peer_event(
            self.now,
            k,
            PcEvent::Track(PcTrackInfo {
                track: TrackId(track),
                mid: mid.into(),
                kind,
                codec: if kind == MediaKind::Video { Codec::Vp8 } else { Codec::Opus },
                rid: rid.map(str::to_string),
                ssrc: track,
                payload_type: 96,
            }),
        );
        self.drain();
    }

    fn au(&mut self, k: ConnKey, track: u32, kind: MediaKind, size: usize, key: bool) {
        let a = AccessUnit {
            track: TrackId(track),
            codec: if kind == MediaKind::Video { Codec::Vp8 } else { Codec::Opus },
            epoch: 0,
            sequence: 0,
            config_generation: 0,
            pts: 0,
            dts: 0,
            duration: if kind == MediaKind::Video { 3000 } else { 960 },
            timebase: if kind == MediaKind::Video { Timebase::new(1, 90000) } else { Timebase::new(1, 48000) },
            keyframe: key,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from(vec![0u8; size]),
        };
        self.svc.on_peer_event(self.now, k, PcEvent::AccessUnit { track: TrackId(track), au: a, config: None });
        self.drain();
    }

    fn advance(&mut self, d: Duration) {
        self.now += d;
        self.svc.handle_timeout(self.now);
        self.drain();
    }

    fn connected(&mut self, k: ConnKey) {
        self.svc.on_peer_event(self.now, k, PcEvent::Connected);
        self.drain();
    }
}

fn sdp(lines: &[(&str, &str, &[&str])]) -> String {
    let mids: Vec<String> = (0..lines.len()).map(|i| i.to_string()).collect();
    let mut s = format!(
        "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE {}\r\n",
        mids.join(" ")
    );
    for (i, (kind, dir, rids)) in lines.iter().enumerate() {
        let (pt, map) = if *kind == "video" { (96, "VP8/90000") } else { (111, "opus/48000/2") };
        s.push_str(&format!(
            "m={kind} 9 UDP/TLS/RTP/SAVPF {pt}\r\nc=IN IP4 0.0.0.0\r\na=mid:{i}\r\na={dir}\r\na=rtcp-mux\r\na=rtpmap:{pt} {map}\r\na=ice-ufrag:abcd\r\na=ice-pwd:abcdefghijklmnopqrstuv\r\na=setup:actpass\r\n"
        ));
        for r in rids.iter() {
            s.push_str(&format!("a=rid:{r} send\r\n"));
        }
        if !rids.is_empty() {
            s.push_str(&format!("a=simulcast:send {}\r\n", rids.join(";")));
        }
    }
    s
}

fn errors(msgs: &[ServerMessage]) -> Vec<(ErrorCode, bool)> {
    msgs.iter()
        .filter_map(|m| match m {
            ServerMessage::Error(e) => Some((e.code, e.fatal)),
            _ => None,
        })
        .collect()
}

fn slots_of(msgs: &[ServerMessage]) -> Vec<crate::signaling::Slot> {
    let mut v = Vec::new();
    for m in msgs {
        match m {
            ServerMessage::Answer(a) => v.extend(a.slots.clone()),
            ServerMessage::Slots(s) => v.extend(s.slots.clone()),
            _ => {}
        }
    }
    v
}

const AV_SEND: &[(&str, &str, &[&str])] = &[("video", "sendonly", &[]), ("audio", "sendonly", &[])];

// ------------------------------------------------------------------ 認証・参加

#[test]
fn connect_requires_valid_token_and_matching_claims() {
    let mut h = H::new(|_| {});
    // トークンなし
    let k = h.svc.open(h.now, Default::default());
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv"}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::AuthFailed, true)]);
    assert_eq!(h.closed.last(), Some(&(k, 4401)));
    // 別の部屋のトークン
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("other", Role::Sendrecv, None, json!({}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv","token":t}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::Forbidden, true)]);
    // 役割の上限を超える
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("r", Role::Recvonly, None, json!({}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendonly","token":t}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::Forbidden, true)]);
    // client_id の食い違い
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("r", Role::Sendrecv, Some("alice"), json!({}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv","token":t,"client_id":"bob"}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::Forbidden, true)]);
    // 期限切れ
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("r", Role::Sendrecv, None, json!({"exp": UNIX_MS / 1000 - 100}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv","token":t}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::AuthFailed, true)]);
    // 最初が connect でない・壊れた JSON・版
    let k = h.svc.open(h.now, Default::default());
    h.send(k, json!({"type":"offer","sdp":"v=0"}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::InvalidState, true)]);
    let k = h.svc.open(h.now, Default::default());
    h.svc.on_signaling_text(h.now, k, "{");
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::BadMessage, true)]);
    let k = h.svc.open(h.now, Default::default());
    h.send(k, json!({"type":"connect","version":2,"room_id":"r","role":"sendrecv"}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::UnsupportedVersion, true)]);
    // 正しい参加
    let (k, c) = h.join("r", Role::Sendrecv, "alice", json!({}));
    assert_eq!(c["room_id"], "r");
    assert_eq!(c["client_id"], "alice");
    assert_eq!(c["version"], 1);
    assert_eq!(c["recv_slots"], json!({"audio":0,"video":0}));
    assert!(h.take_other().iter().any(|a| matches!(a, Action::CreatePeer { conn, .. } if *conn == k)));
    // 参加の後の不正な message は誤りだが切断しない
    h.svc.on_signaling_text(h.now, k, "{");
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::BadMessage, false)]);
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv"}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::InvalidState, false)]);
    // 接続の期限: connect を送らない
    let k = h.svc.open(h.now, Default::default());
    h.advance(Duration::from_secs(11));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::Timeout, true)]);
}

#[test]
fn auth_webhook_allow_override_deny_and_timeout() {
    let mut h = H::new(|c| {
        c.auth_webhook = AuthWebhookMode::Only;
        c.token_verifier = None;
    });
    let k = h.svc.open(h.now, Default::default());
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv","client_id":"c","metadata":{"secret":"pw"}}));
    let acts = h.take_other();
    let Some(Action::AuthWebhook { request_id, body, .. }) = acts.into_iter().find(|a| matches!(a, Action::AuthWebhook { .. })) else {
        panic!()
    };
    let req: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(req["room_id"], "r");
    assert_eq!(req["metadata"]["secret"], "pw");
    assert_eq!(req["type"], "connection.auth");
    // 応答を待つ間の message は誤り
    h.send(k, json!({"type":"pong"}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::InvalidState, false)]);
    // 古い request_id は無視
    h.svc.on_auth_webhook_result(h.now, k, "old", Some(200), br#"{"allowed":true}"#);
    assert!(h.take(k).is_empty());
    h.svc.on_auth_webhook_result(
        h.now,
        k,
        &request_id,
        Some(200),
        br#"{"allowed":true,"role":"recvonly","metadata":{"name":"Carol"}}"#,
    );
    let msgs = h.take(k);
    let Some(ServerMessage::Connected(c)) = msgs.first() else { panic!("{msgs:?}") };
    assert_eq!(c.role, Role::Recvonly);
    assert_eq!(c.metadata, json!({"name":"Carol"}));
    // 拒否
    let k2 = h.svc.open(h.now, Default::default());
    h.send(k2, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv"}));
    let rid = h
        .take_other()
        .into_iter()
        .find_map(|a| match a {
            Action::AuthWebhook { request_id, .. } => Some(request_id),
            _ => None,
        })
        .unwrap();
    h.svc.on_auth_webhook_result(h.now, k2, &rid, Some(200), br#"{"allowed":false,"reason":"banned"}"#);
    let m = h.take(k2);
    assert!(matches!(&m[0], ServerMessage::Error(e) if e.code == ErrorCode::AuthFailed && e.message == "banned"));
    // 時間切れ
    let k3 = h.svc.open(h.now, Default::default());
    h.send(k3, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv"}));
    h.advance(Duration::from_secs(6));
    assert_eq!(errors(&h.take(k3)), vec![(ErrorCode::AuthFailed, true)]);
}

#[test]
fn auth_webhook_skipped_falls_back_to_the_token() {
    let mut h = H::new(|c| c.auth_webhook = AuthWebhookMode::AfterToken);
    let rid_of = |acts: Vec<Action>| {
        acts.into_iter()
            .find_map(|a| match a {
                Action::AuthWebhook { request_id, .. } => Some(request_id),
                _ => None,
            })
            .unwrap()
    };
    // トークンだけで決める: トークンに無い client_id は名乗れない（Webhook を通したときとは違う）
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("r", Role::Sendrecv, None, json!({}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv","token":t,"client_id":"mallory"}));
    let rid = rid_of(h.take_other());
    h.svc.on_auth_webhook_skipped(h.now, k, "old");
    assert!(h.take(k).is_empty());
    h.svc.on_auth_webhook_skipped(h.now, k, &rid);
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::Forbidden, true)]);
    // トークンの client_id で入る
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("r", Role::Sendrecv, Some("alice"), json!({}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv","token":t}));
    let rid = rid_of(h.take_other());
    h.svc.on_auth_webhook_skipped(h.now, k, &rid);
    let msgs = h.take(k);
    let Some(ServerMessage::Connected(c)) = msgs.first() else { panic!("{msgs:?}") };
    assert_eq!(c.client_id.as_deref(), Some("alice"));
}

#[test]
fn room_limits_redirect_e2ee_and_codec() {
    let mut h = H::new(|c| {
        c.default_settings.max_participants = 2;
    });
    h.join("r", Role::Sendrecv, "a", json!({}));
    h.join("r", Role::Sendrecv, "b", json!({}));
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("r", Role::Sendrecv, None, json!({}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv","token":t}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::RoomFull, true)]);
    // 部屋のつなぎ直し
    assert!(h.svc.set_room_redirect("r", Some("wss://node-b/signaling".into())));
    let k = h.svc.open(h.now, Default::default());
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv","token":t}));
    let m = h.take(k);
    assert!(matches!(&m[0], ServerMessage::Redirect(r) if r.location == "wss://node-b/signaling"), "{m:?}");
    assert!(matches!(&m[1], ServerMessage::Disconnect(d) if d.reason == crate::signaling::DisconnectReason::Redirected));
    // E2EE の部屋
    h.svc
        .api_create_room(h.now, CreateRoom {
            id: Some("sec".into()),
            settings: Some(RoomSettings { e2ee: true, ..Default::default() }),
            metadata: Value::Null,
        })
        .unwrap();
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("sec", Role::Sendrecv, None, json!({}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"sec","role":"sendrecv","token":t}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::E2eeMismatch, true)]);
    let (_, c) = h.join("sec", Role::Sendrecv, "e", json!({"e2ee": true}));
    assert_eq!(c["e2ee"], true);
    let e = h.svc.api_plan_composite("sec").unwrap_err();
    assert_eq!(e.status, 422);
    assert!(e.title.contains("e2ee"));
    assert!(!h.svc.api_get_room("sec").unwrap().compositing.allowed);
    // コーデック
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("sec", Role::Sendrecv, None, json!({}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"sec","role":"sendrecv","token":t,"e2ee":true,"video":{"codec":"h264"}}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::UnsupportedCodec, true)]);
    // 自動で作らない設定
    let mut h = H::new(|c| c.auto_create_rooms = false);
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("nope", Role::Sendrecv, None, json!({}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"nope","role":"sendrecv","token":t}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::RoomNotFound, true)]);
}

// ------------------------------------------------------------------ 枠・再交渉

#[test]
fn slots_are_assigned_stably_and_renegotiation_requested() {
    let mut h = H::new(|_| {});
    let (a, _) = h.join("r", Role::Sendonly, "a", json!({}));
    let ans = h.offer(a, AV_SEND);
    let ServerMessage::Answer(an) = &ans[0] else { panic!("{ans:?}") };
    assert_eq!(an.published.len(), 2);
    let a_conn = h.svc.connection_id(a).unwrap().to_string();
    assert_eq!(an.published[0].track_id, format!("{a_conn}.0"));
    // 受け手は今いる送り手の数だけ枠を求められる
    let (b, c) = h.join("r", Role::Recvonly, "b", json!({}));
    assert_eq!(c["recv_slots"], json!({"audio":1,"video":1}));
    assert_eq!(c["participants"][0]["tracks"].as_array().unwrap().len(), 2);
    let m = h.offer(b, &[("video", "recvonly", &[]), ("audio", "recvonly", &[])]);
    let s = slots_of(&m);
    assert_eq!(s.len(), 2);
    assert!(s.iter().all(|x| x.state == SlotState::Active && x.connection_id.as_deref() == Some(a_conn.as_str())));
    assert_eq!(s[0].client_id.as_deref(), Some("a"));
    // 2 人目の送り手 → 受け手に renegotiate
    let (c2, _) = h.join("r", Role::Sendonly, "c", json!({}));
    let bm = h.take(b);
    assert!(bm.iter().any(|m| matches!(m, ServerMessage::Notify(Notify::ConnectionCreated { .. }))));
    h.offer(c2, AV_SEND);
    let bm = h.take(b);
    assert_eq!(bm.iter().filter(|m| matches!(m, ServerMessage::Notify(Notify::TrackAdded { .. }))).count(), 2);
    let r: Vec<_> = bm.iter().filter_map(|m| match m { ServerMessage::Renegotiate(r) => Some(*r), _ => None }).collect();
    assert_eq!(r.len(), 1, "{bm:?}");
    assert_eq!((r[0].audio, r[0].video), (2, 2));
    // 枠を足した offer: 前の割り当ては同じ mid のまま
    let m = h.offer(b, &[("video", "recvonly", &[]), ("audio", "recvonly", &[]), ("video", "recvonly", &[]), ("audio", "recvonly", &[])]);
    let s = slots_of(&m);
    assert_eq!(s.len(), 4);
    assert_eq!(s[0].connection_id.as_deref(), Some(a_conn.as_str()));
    assert_eq!(s[1].connection_id.as_deref(), Some(a_conn.as_str()));
    let c_conn = h.svc.connection_id(c2).unwrap().to_string();
    assert_eq!(s[2].connection_id.as_deref(), Some(c_conn.as_str()));
    assert_eq!(s[3].connection_id.as_deref(), Some(c_conn.as_str()));
    // A が抜けると枠は空き、slots と connection.destroyed
    h.send(a, json!({"type":"disconnect"}));
    let bm = h.take(b);
    assert!(bm.iter().any(|m| matches!(m, ServerMessage::Notify(Notify::ConnectionDestroyed { reason, .. }) if *reason == crate::signaling::DisconnectReason::Normal)));
    assert_eq!(bm.iter().filter(|m| matches!(m, ServerMessage::Notify(Notify::TrackRemoved { .. }))).count(), 2);
    let s = slots_of(&bm);
    assert_eq!(s.len(), 2);
    assert!(s.iter().all(|x| x.state == SlotState::Empty));
    // 次の送り手は空いた枠に入り、renegotiate は要らない
    let (d, _) = h.join("r", Role::Sendonly, "d", json!({}));
    h.offer(d, AV_SEND);
    let bm = h.take(b);
    assert!(!bm.iter().any(|m| matches!(m, ServerMessage::Renegotiate(_))));
    let s = slots_of(&bm);
    assert_eq!(s.iter().map(|x| x.mid.as_str()).collect::<Vec<_>>(), vec!["0", "1"]);
    // 2 つ目の offer を処理中の offer は誤り
    h.svc.on_signaling_text(h.now, b, &json!({"type":"offer","sdp": sdp(&[("video","recvonly",&[])])}).to_string());
    h.svc.on_signaling_text(h.now, b, &json!({"type":"offer","sdp": sdp(&[("video","recvonly",&[])])}).to_string());
    assert_eq!(errors(&h.take(b)), vec![(ErrorCode::InvalidState, false)]);
}

#[test]
fn roles_permissions_bundle_and_multistream() {
    let mut h = H::new(|_| {});
    // recvonly が送る m 行を出す（最初の offer なので切断）
    let (r, _) = h.join("r", Role::Recvonly, "r", json!({}));
    h.send(r, json!({"type":"offer","sdp": sdp(AV_SEND)}));
    assert_eq!(errors(&h.take(r)), vec![(ErrorCode::RoleViolation, true)]);
    // 許可は音声だけ
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("r", Role::Sendrecv, None, json!({"permissions": {"publish": ["audio"]}}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv","token":t}));
    h.take(k);
    h.send(k, json!({"type":"offer","sdp": sdp(AV_SEND)}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::Forbidden, true)]);
    // 壊れた SDP
    let (x, _) = h.join("r", Role::Sendrecv, "x", json!({}));
    h.send(x, json!({"type":"offer","sdp":"garbage"}));
    assert_eq!(errors(&h.take(x)), vec![(ErrorCode::InvalidSdp, true)]);
    // bundle_id が同じなら受けない
    let (cam, _) = h.join("b", Role::Sendrecv, "u1", json!({"bundle_id": "u1"}));
    h.offer(cam, AV_SEND);
    let (screen, c) = h.join("b", Role::Sendrecv, "u1s", json!({"bundle_id": "u1"}));
    assert_eq!(c["recv_slots"], json!({"audio":0,"video":0}));
    let (other, c) = h.join("b", Role::Recvonly, "o", json!({}));
    assert_eq!(c["recv_slots"], json!({"audio":1,"video":1}));
    let _ = (screen, other);
    // multistream=false は種類ごとに 1 本
    h.offer(screen, &[("video", "sendonly", &[])]);
    let (single, c) = h.join("b", Role::Recvonly, "s", json!({"multistream": false}));
    assert_eq!(c["recv_slots"], json!({"audio":1,"video":1}));
    let (multi, c) = h.join("b", Role::Recvonly, "m", json!({}));
    assert_eq!(c["recv_slots"], json!({"audio":1,"video":2}));
    let _ = (single, multi);
}

#[test]
fn forwarding_filters_room_and_connection() {
    let mut h = H::new(|_| {});
    h.svc
        .api_create_room(h.now, CreateRoom {
            id: Some("class".into()),
            settings: Some(RoomSettings {
                forwarding_filter: Some(serde_json::from_value(json!({
                    "action":"block",
                    "rules":[[{"field":"kind","operator":"is_in","values":["video"]},
                              {"field":"client_id","operator":"is_not_in","values":["teacher"]}]]
                })).unwrap()),
                ..Default::default()
            }),
            metadata: Value::Null,
        })
        .unwrap();
    let (t, _) = h.join("class", Role::Sendonly, "teacher", json!({}));
    h.offer(t, AV_SEND);
    let (s1, _) = h.join("class", Role::Sendonly, "student1", json!({}));
    h.offer(s1, AV_SEND);
    // 先生の映像と 2 人の音声
    let (v, c) = h.join("class", Role::Recvonly, "viewer", json!({}));
    assert_eq!(c["recv_slots"], json!({"audio":2,"video":1}));
    // 購読者のフィルター: 音声を受けない
    let (v2, c) = h.join(
        "class",
        Role::Recvonly,
        "viewer2",
        json!({"forwarding_filter":{"action":"block","rules":[[{"field":"kind","operator":"is_in","values":["audio"]}]]}}),
    );
    assert_eq!(c["recv_slots"], json!({"audio":0,"video":1}));
    let _ = (v, v2);
    // 部屋のフィルターを外す（If-Match）→ 枠が足りなくなり renegotiate
    let m = h.offer(v, &[("video", "recvonly", &[]), ("audio", "recvonly", &[]), ("audio", "recvonly", &[])]);
    assert_eq!(slots_of(&m).iter().filter(|s| s.state == SlotState::Active).count(), 3);
    let gen_ = h.svc.api_get_room("class").unwrap().generation;
    h.svc
        .api_update_room(h.now, "class", Some(gen_), UpdateRoom { settings: Some(RoomSettings::default()), metadata: None })
        .unwrap();
    let m = h.take(v);
    assert!(m.iter().any(|x| matches!(x, ServerMessage::Renegotiate(r) if r.video == 2 && r.audio == 2)), "{m:?}");
}

// ------------------------------------------------------------------ メディア・層・スポットライト

/// 送り手 `k` の simulcast の映像（mid 0、層 r0〜r2 = track 1〜3）と音声（mid 1、track 4）を始める。
fn start_media(h: &mut H, k: ConnKey, base: u32) {
    for (i, r) in ["r0", "r1", "r2"].iter().enumerate() {
        h.track_event(k, "0", MediaKind::Video, Some(r), base + i as u32);
    }
    h.track_event(k, "1", MediaKind::Audio, None, base + 3);
}

fn forwards(acts: &[Action], to: ConnKey) -> Vec<(TrackId, bool, usize)> {
    acts.iter()
        .filter_map(|a| match a {
            Action::Forward { conn, track, au, .. } if *conn == to => Some((*track, au.keyframe, au.payload.len())),
            _ => None,
        })
        .collect()
}

#[test]
fn layer_selection_preferred_and_keyframe_switch() {
    let mut h = H::new(|_| {});
    let (a, _) = h.join("r", Role::Sendonly, "a", json!({"simulcast":{"rids":["r0","r1","r2"]}}));
    h.offer(a, &[("video", "sendonly", &["r0", "r1", "r2"]), ("audio", "sendonly", &[])]);
    start_media(&mut h, a, 1);
    let (s, _) = h.join("r", Role::Recvonly, "s", json!({"simulcast":{"rids":["r0"],"default_rid":"r0"}}));
    let m = h.offer(s, &[("video", "recvonly", &[]), ("audio", "recvonly", &[])]);
    let vt = slots_of(&m)[0].track_id.clone().unwrap();
    h.connected(a);
    h.connected(s);
    // 既定の層 r0 にキーフレームを求める
    let acts = h.take_other();
    assert!(acts.iter().any(|x| matches!(x, Action::RequestKeyframe { conn, track } if *conn == a && *track == TrackId(1))), "{acts:?}");
    // 層ごとの大きさ（r0 < r1 < r2）で AU を流す。キーフレームは r0 だけ
    for n in 0..10 {
        h.au(a, 1, MediaKind::Video, 100, n == 0);
        h.au(a, 2, MediaKind::Video, 400, false);
        h.au(a, 3, MediaKind::Video, 1200, false);
        h.au(a, 4, MediaKind::Audio, 60, true);
        h.now += Duration::from_millis(33);
    }
    let acts = h.take_other();
    let f = forwards(&acts, s);
    let sizes: Vec<usize> = f.iter().map(|x| x.2).collect();
    assert!(sizes.iter().filter(|x| **x == 100).count() == 10, "{sizes:?}");
    assert!(sizes.iter().all(|x| *x == 100 || *x == 60));
    assert!(f[0].1, "first video frame is a keyframe");
    let msgs = h.take(s);
    assert!(msgs.iter().any(|m| matches!(m, ServerMessage::Notify(Notify::LayerChanged { rid, .. }) if rid.as_deref() == Some("r0"))));
    // r2 を選ぶ: r2 のキーフレームまでは r0 を配り続ける
    h.send(s, json!({"type":"layer","track_id":vt,"rid":"r2"}));
    let acts = h.take_other();
    assert!(acts.iter().any(|x| matches!(x, Action::RequestKeyframe { track, .. } if *track == TrackId(3))));
    for n in 0..6 {
        h.au(a, 1, MediaKind::Video, 100, false);
        h.au(a, 2, MediaKind::Video, 400, false);
        h.au(a, 3, MediaKind::Video, 1200, n == 3);
        h.now += Duration::from_millis(33);
    }
    let f = forwards(&h.take_other(), s);
    let sizes: Vec<usize> = f.iter().map(|x| x.2).collect();
    // 同じ時刻の r0 の後に r2 のキーフレームが来て切り替わる
    assert_eq!(sizes, vec![100, 100, 100, 100, 1200, 1200, 1200], "switch at the keyframe");
    assert!(f[4].1);
    // 統計
    let st = h.svc.api_stats(h.now, "r").unwrap();
    let ss = st.participants.iter().find(|p| p.connection_id == h.svc.connection_id(s).unwrap()).unwrap();
    assert_eq!(ss.subscriptions[0].rid.as_deref(), Some("r2"));
    assert!(ss.forwarded_bytes > 0);
    let ps = st.participants.iter().find(|p| p.connection_id == h.svc.connection_id(a).unwrap()).unwrap();
    assert_eq!(ps.published[0].layers.len(), 3);
    // 不明な層・不明なトラック
    h.send(s, json!({"type":"layer","track_id":vt,"rid":"zz"}));
    h.send(s, json!({"type":"layer","track_id":"nope","rid":null}));
    assert_eq!(errors(&h.take(s)), vec![(ErrorCode::UnknownTrack, false), (ErrorCode::UnknownTrack, false)]);
    // 送り手のキーフレーム要求（受け手の PLI）は今の層へ
    let st_track = TrackId(h.next_track - 1);
    h.svc.on_peer_event(h.now + Duration::from_secs(1), s, PcEvent::KeyframeRequest { track: st_track });
    let acts = h.take_other();
    assert!(acts.iter().any(|x| matches!(x, Action::RequestKeyframe { conn, track } if *conn == a && *track == TrackId(3))), "{acts:?}");
}

#[test]
fn spotlight_follows_active_speaker_with_hysteresis() {
    let mut h = H::new(|c| {
        c.default_settings.spotlight = Some(SpotlightSettings { number: 1, unfocused: Unfocused::Off, focus_rid: None });
    });
    let (a, _) = h.join("r", Role::Sendonly, "a", json!({}));
    h.offer(a, &[("video", "sendonly", &["r0", "r1", "r2"]), ("audio", "sendonly", &[])]);
    start_media(&mut h, a, 1);
    let (b, _) = h.join("r", Role::Sendonly, "b", json!({}));
    h.offer(b, &[("video", "sendonly", &["r0", "r1", "r2"]), ("audio", "sendonly", &[])]);
    start_media(&mut h, b, 11);
    let (s, c) = h.join("r", Role::Recvonly, "s", json!({}));
    assert_eq!(c["spotlight"]["number"], 1);
    let m = h.offer(s, &[("video", "recvonly", &[]), ("audio", "recvonly", &[]), ("video", "recvonly", &[]), ("audio", "recvonly", &[])]);
    let _ = m;
    let a_id = h.svc.connection_id(a).unwrap().to_string();
    let b_id = h.svc.connection_id(b).unwrap().to_string();
    h.advance(Duration::from_millis(250));
    let msgs = h.take(s);
    assert!(msgs.iter().any(|m| matches!(m, ServerMessage::Notify(Notify::SpotlightChanged { focused }) if *focused == vec![a_id.clone()])), "{msgs:?}");
    let s_id = h.svc.connection_id(s).unwrap().to_string();
    let video_state = |h: &H, who: &str| {
        h.svc
            .api_get_participant("r", &s_id)
            .unwrap()
            .subscriptions
            .into_iter()
            .find(|x| x.kind == Kind::Video && x.connection_id.as_deref() == Some(who))
            .map(|x| x.state)
    };
    assert_eq!(video_state(&h, &a_id), Some(SlotState::Active));
    assert_eq!(video_state(&h, &b_id), Some(SlotState::Paused));
    // b が 1 秒だけ話す: min_hold（2 秒）の前なので入れ替えない... a は参加の順の注目なので 2 秒たてば入れ替わる
    for _ in 0..150 {
        h.au(b, 14, MediaKind::Audio, 80, true);
        h.au(a, 4, MediaKind::Audio, 5, true);
        h.advance(Duration::from_millis(20));
    }
    let msgs = h.take(s);
    let changes: Vec<Vec<String>> = msgs
        .iter()
        .filter_map(|m| match m {
            ServerMessage::Notify(Notify::SpotlightChanged { focused }) => Some(focused.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(changes, vec![vec![b_id.clone()]], "one change to b");
    assert_eq!(video_state(&h, &a_id), Some(SlotState::Paused));
    assert_eq!(video_state(&h, &b_id), Some(SlotState::Active));
    assert_eq!(slots_of(&msgs).len(), 2, "both video slots changed");
    let evs: Vec<_> = h.take_other().into_iter().filter(|a| matches!(a, Action::Event(e) if e.event.type_name() == "spotlight.changed")).collect();
    assert!(!evs.is_empty());
    // 短く交互に話しても揺れない
    let mut n = 0;
    for i in 0..30 {
        let (talk, quiet, t1, t2) = if i % 2 == 0 { (a, b, 4, 14) } else { (b, a, 14, 4) };
        for _ in 0..10 {
            h.au(talk, t1, MediaKind::Audio, 80, true);
            h.au(quiet, t2, MediaKind::Audio, 5, true);
            h.advance(Duration::from_millis(20));
        }
        n += h.take(s).iter().filter(|m| matches!(m, ServerMessage::Notify(Notify::SpotlightChanged { .. }))).count();
    }
    assert!(n <= 3, "{n} spotlight changes in 6 s of alternating speech");
}

#[test]
fn spotlight_lowest_layer_for_unfocused() {
    let mut h = H::new(|c| {
        c.default_settings.spotlight = Some(SpotlightSettings { number: 1, unfocused: Unfocused::Lowest, focus_rid: Some("r2".into()) });
    });
    let (a, _) = h.join("r", Role::Sendonly, "a", json!({}));
    h.offer(a, &[("video", "sendonly", &["r0", "r1", "r2"])]);
    start_media(&mut h, a, 1);
    let (b, _) = h.join("r", Role::Sendonly, "b", json!({}));
    h.offer(b, &[("video", "sendonly", &["r0", "r1", "r2"])]);
    start_media(&mut h, b, 11);
    let (s, _) = h.join("r", Role::Recvonly, "s", json!({}));
    h.offer(s, &[("video", "recvonly", &[]), ("video", "recvonly", &[])]);
    h.advance(Duration::from_millis(250));
    let acts = h.take_other();
    // a（注目）は r2、b は r0 を求める
    assert!(acts.iter().any(|x| matches!(x, Action::RequestKeyframe { conn, track } if *conn == a && *track == TrackId(3))), "{acts:?}");
    assert!(acts.iter().any(|x| matches!(x, Action::RequestKeyframe { conn, track } if *conn == b && *track == TrackId(11))), "{acts:?}");
}

// ------------------------------------------------------------------ DataChannel・シグナリングの切り替え

fn dc_open(h: &mut H, k: ConnKey, id: u16, label: &str) {
    h.svc.on_peer_event(
        h.now,
        k,
        PcEvent::DataChannelOpen(ChannelInfo {
            id,
            label: label.into(),
            protocol: String::new(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: false,
            open: true,
        }),
    );
    h.drain();
}

#[test]
fn datachannel_messages_routed_by_label_and_direction() {
    let mut h = H::new(|_| {});
    let dc = |dir: &str| json!({"data_channels":[{"label":"#chat","direction":dir},{"label":"#other"}]});
    let (a, c) = h.join("r", Role::Sendrecv, "a", dc("sendrecv"));
    assert_eq!(c["data_channels"].as_array().unwrap().len(), 2);
    let (b, _) = h.join("r", Role::Sendrecv, "b", dc("sendrecv"));
    let (r, _) = h.join("r", Role::Sendrecv, "r", dc("recvonly"));
    let (n, _) = h.join("r", Role::Sendrecv, "n", json!({}));
    for k in [a, b, r] {
        dc_open(&mut h, k, 1, "#chat");
    }
    // 宣言していないラベルは閉じる
    dc_open(&mut h, n, 1, "#chat");
    dc_open(&mut h, a, 3, "#undeclared");
    let acts = h.take_other();
    assert!(acts.iter().any(|x| matches!(x, Action::CloseDataChannel { conn, id: 1 } if *conn == n)));
    assert!(acts.iter().any(|x| matches!(x, Action::CloseDataChannel { conn, id: 3 } if *conn == a)));
    h.svc.on_peer_event(h.now, a, PcEvent::DataChannelMessage { id: 1, data: Bytes::from_static(b"hello"), binary: false });
    let acts = h.take_other();
    let to: Vec<ConnKey> = acts
        .iter()
        .filter_map(|x| match x {
            Action::SendData { conn, label, data, binary: false } if label == "#chat" && &data[..] == b"hello" => Some(*conn),
            _ => None,
        })
        .collect();
    assert_eq!(to, vec![b, r]);
    // recvonly の参加者からは配らない
    h.svc.on_peer_event(h.now, r, PcEvent::DataChannelMessage { id: 1, data: Bytes::from_static(b"x"), binary: true });
    assert!(!h.take_other().iter().any(|x| matches!(x, Action::SendData { .. })));
    // シグナリングの message は宛先を選べる
    h.drain();
    h.inbox.clear();
    let b_id = h.svc.connection_id(b).unwrap().to_string();
    h.send(a, json!({"type":"message","label":"#chat","to":[b_id],"data":"dm"}));
    let bm = h.take(b);
    assert!(matches!(&bm[0], ServerMessage::Message(m) if m.data.as_deref() == Some("dm") && m.from.as_deref() == h.svc.connection_id(a)));
    assert!(h.take(r).is_empty());
    h.send(a, json!({"type":"message","label":"#chat","data_base64":"AAEC"}));
    assert!(matches!(&h.take(r)[0], ServerMessage::Message(m) if m.data_base64.as_deref() == Some("AAEC")));
    assert!(h.take(a).is_empty(), "not echoed to the sender");
    // 許可されていないラベル
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("r", Role::Sendrecv, None, json!({"permissions": {"data": ["#chat"]}}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv","token":t,"data_channels":[{"label":"#chat"},{"label":"#admin"}]}));
    let m = h.take(k);
    let ServerMessage::Connected(c) = &m[0] else { panic!() };
    assert_eq!(c.data_channels.iter().map(|d| d.label.as_str()).collect::<Vec<_>>(), vec!["#chat"]);
    h.send(k, json!({"type":"message","label":"#admin","data":"x"}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::UnknownLabel, false)]);
    // API から: DataChannel のラベルへ・push
    let r_id = h.svc.connection_id(r).unwrap().to_string();
    let res = h
        .svc
        .api_post_message(h.now, "r", PostMessage { to: vec![r_id.clone()], label: Some("#chat".into()), data: json!("from api") })
        .unwrap();
    assert_eq!(res.delivered, 1);
    assert!(h.take_other().iter().any(|x| matches!(x, Action::SendData { conn, .. } if *conn == r)));
    let res = h.svc.api_post_message(h.now, "r", PostMessage { to: vec![], label: None, data: json!({"k": 1}) }).unwrap();
    assert_eq!(res.delivered, 5);
    assert!(h.take(a).iter().any(|m| matches!(m, ServerMessage::Push(p) if p.data == json!({"k":1}))));
    assert_eq!(h.svc.api_post_message(h.now, "r", PostMessage { to: vec!["zz".into()], label: None, data: json!(1) }).unwrap_err().status, 404);
}

#[test]
fn signaling_switches_to_datachannel() {
    let mut h = H::new(|_| {});
    let (k, c) = h.join(
        "r",
        Role::Sendrecv,
        "a",
        json!({"signaling":{"data_channel":true,"close_websocket":true},"data_channels":[{"label":"signaling"}]}),
    );
    assert_eq!(c["signaling_data_channel"], true);
    h.connected(k);
    dc_open(&mut h, k, 5, "signaling");
    h.drain();
    let v = h.inbox.remove(&k).unwrap();
    assert!(matches!(v.last(), Some((SignalPath::WebSocket, ServerMessage::Switched(_)))));
    // 以後は DataChannel で。WebSocket が閉じても切断しない
    h.svc.on_signaling_closed(h.now, k);
    h.advance(Duration::from_secs(10));
    h.drain();
    let v = h.inbox.remove(&k).unwrap();
    assert!(matches!(v.last(), Some((SignalPath::DataChannel, ServerMessage::Ping(_)))), "{v:?}");
    // DataChannel で届いた pong
    h.svc.on_peer_event(h.now, k, PcEvent::DataChannelMessage { id: 5, data: Bytes::from_static(br#"{"type":"pong"}"#), binary: false });
    h.advance(Duration::from_secs(9));
    assert!(h.svc.connection_id(k).is_some());
    // DataChannel も閉じれば切断
    h.svc.on_peer_event(h.now, k, PcEvent::DataChannelClosed { id: 5 });
    assert!(h.svc.connection_id(k).is_none());
    // close_websocket なしでは WebSocket が閉じたら切断
    let (k2, _) = h.join("r", Role::Sendrecv, "b", json!({"signaling":{"data_channel":true},"data_channels":[{"label":"signaling"}]}));
    dc_open(&mut h, k2, 5, "signaling");
    h.svc.on_signaling_closed(h.now, k2);
    assert!(h.svc.connection_id(k2).is_none());
}

#[test]
fn ping_timeout_and_peer_timeout() {
    let mut h = H::new(|_| {});
    let (k, _) = h.join("r", Role::Sendrecv, "a", json!({}));
    h.connected(k);
    h.advance(Duration::from_secs(10));
    assert!(matches!(h.take(k).last(), Some(ServerMessage::Ping(_))));
    h.send(k, json!({"type":"pong","stats":{"x":1}}));
    h.advance(Duration::from_secs(10));
    h.take(k);
    h.advance(Duration::from_secs(10));
    let m = h.take(k);
    assert!(m.iter().any(|x| matches!(x, ServerMessage::Disconnect(d) if d.reason == crate::signaling::DisconnectReason::PingTimeout)), "{m:?}");
    assert!(h.take_other().iter().any(|a| matches!(a, Action::ClosePeer { conn } if *conn == k)));
    // つながらないピア接続
    let (k2, _) = h.join("r", Role::Sendrecv, "b", json!({}));
    for _ in 0..4 {
        h.advance(Duration::from_secs(8));
        h.send(k2, json!({"type":"pong"}));
    }
    let m = h.take(k2);
    assert!(m.iter().any(|x| matches!(x, ServerMessage::Disconnect(d) if d.reason == crate::signaling::DisconnectReason::PeerFailed)));
    // 頻度の制限
    let (k3, _) = h.join("r", Role::Sendrecv, "c", json!({}));
    for _ in 0..60 {
        h.svc.on_signaling_text(h.now, k3, r#"{"type":"pong"}"#);
    }
    assert_eq!(errors(&h.take(k3)), vec![(ErrorCode::RateLimited, false)]);
}

// ------------------------------------------------------------------ REST・イベント

#[test]
fn rest_resources_and_events() {
    let mut h = H::new(|_| {});
    let r = h.svc.api_create_room(h.now, CreateRoom { id: Some("daily".into()), settings: None, metadata: json!({"topic":"x"}) }).unwrap();
    assert_eq!(r.etag, "\"1\"");
    assert_eq!(h.svc.api_create_room(h.now, CreateRoom { id: Some("daily".into()), ..Default::default() }).unwrap_err().status, 409);
    assert_eq!(h.svc.api_create_room(h.now, CreateRoom { id: Some("a/b".into()), ..Default::default() }).unwrap_err().status, 400);
    let bad = RoomSettings { max_participants: 0, ..Default::default() };
    let e = h.svc.api_create_room(h.now, CreateRoom { id: None, settings: Some(bad), metadata: Value::Null }).unwrap_err();
    assert_eq!(e.to_problem()["errors"][0]["field"], "max_participants");
    for i in 0..3 {
        h.svc.api_create_room(h.now, CreateRoom { id: Some(format!("room{i}")), ..Default::default() }).unwrap();
    }
    let p1 = h.svc.api_list_rooms(2, None).unwrap();
    assert_eq!(p1.items.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["daily", "room0"]);
    let p2 = h.svc.api_list_rooms(2, p1.next_cursor.as_deref()).unwrap();
    assert_eq!(p2.items.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["room1", "room2"]);
    assert!(h.svc.api_list_rooms(0, None).is_err());
    // 更新は If-Match
    let up = UpdateRoom { settings: None, metadata: Some(json!({"topic":"y"})) };
    assert_eq!(h.svc.api_update_room(h.now, "daily", None, up.clone()).unwrap_err().status, 428);
    assert_eq!(h.svc.api_update_room(h.now, "daily", Some(9), up.clone()).unwrap_err().status, 412);
    let r = h.svc.api_update_room(h.now, "daily", Some(1), up).unwrap();
    assert_eq!((r.generation, r.metadata["topic"].as_str()), (2, Some("y")));
    // 参加者
    let (a, _) = h.join("daily", Role::Sendrecv, "alice", json!({}));
    let (b, _) = h.join("daily", Role::Sendrecv, "bob", json!({}));
    let a_id = h.svc.connection_id(a).unwrap().to_string();
    let list = h.svc.api_list_participants("daily", 100, None).unwrap();
    assert_eq!(list.items.len(), 2);
    let p = h.svc.api_get_participant("daily", &a_id).unwrap();
    assert_eq!(p.info.client_id.as_deref(), Some("alice"));
    assert_eq!(p.signaling, "websocket");
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["connection_id"], a_id.as_str());
    h.svc.api_update_participant(h.now, "daily", &a_id, UpdateParticipant { metadata: json!({"hand": true}) }).unwrap();
    assert!(h.take(b).iter().any(|m| matches!(m, ServerMessage::Notify(Notify::ConnectionUpdated { metadata, .. }) if metadata["hand"] == true)));
    // 部屋の e2ee は参加者がいる間は変えられない
    let gen_ = h.svc.api_get_room("daily").unwrap().generation;
    let e2ee = UpdateRoom { settings: Some(RoomSettings { e2ee: true, ..Default::default() }), metadata: None };
    assert_eq!(h.svc.api_update_room(h.now, "daily", Some(gen_), e2ee).unwrap_err().status, 409);
    // 切断
    h.svc.api_disconnect_participant(h.now, "daily", &a_id).unwrap();
    assert!(h.take(a).iter().any(|m| matches!(m, ServerMessage::Disconnect(d) if d.reason == crate::signaling::DisconnectReason::Kicked)));
    assert_eq!(h.svc.api_get_participant("daily", &a_id).unwrap_err().status, 404);
    // トークンの発行
    let t = h.svc.api_issue_token(h.now, "daily", IssueToken { role: Role::Recvonly, client_id: Some("carol".into()), bundle_id: None, metadata: None, permissions: None, ttl_secs: 600 }).unwrap();
    let claims = h.signer.verifier("senca", "senca-rooms").verify(&t.token, UNIX_MS / 1000).unwrap();
    assert_eq!((claims.room.as_str(), claims.role), ("daily", Role::Recvonly));
    assert_eq!(t.expires_at, UNIX_MS / 1000 + 600);
    // 部屋を消す: 参加者は room_deleted
    h.svc.api_delete_room(h.now, "daily").unwrap();
    assert!(h.take(b).iter().any(|m| matches!(m, ServerMessage::Disconnect(d) if d.reason == crate::signaling::DisconnectReason::RoomDeleted)));
    assert_eq!(h.svc.api_get_room("daily").unwrap_err().status, 404);
    // イベントの並び
    let types: Vec<&'static str> = h
        .take_other()
        .iter()
        .filter_map(|a| match a {
            Action::Event(e) => Some(e.event.type_name()),
            _ => None,
        })
        .collect();
    assert_eq!(
        types.iter().filter(|t| **t == "connection.created").count(),
        2,
        "{types:?}"
    );
    assert!(types.contains(&"room.updated"));
    assert!(types.contains(&"connection.updated"));
    assert_eq!(types.last(), Some(&"room.destroyed"));
    let ids: std::collections::BTreeSet<String> = h.other.iter().filter_map(|a| match a { Action::Event(e) => Some(e.id.clone()), _ => None }).collect();
    let _ = ids;
    // 自動で作った部屋は空になってから消える
    let (x, _) = h.join("auto", Role::Sendrecv, "x", json!({}));
    h.send(x, json!({"type":"disconnect"}));
    assert!(h.svc.api_get_room("auto").is_ok());
    h.advance(Duration::from_secs(61));
    assert!(h.svc.api_get_room("auto").is_err());
}

// ------------------------------------------------------------------ カスケード

#[test]
fn cascade_remote_tracks_and_relay() {
    let mut h = H::new(|_| {});
    let (s, _) = h.join("r", Role::Sendrecv, "local", json!({"data_channels":[{"label":"#chat"}]}));
    h.offer(s, AV_SEND);
    h.track_event(s, "0", MediaKind::Video, None, 1);
    h.track_event(s, "1", MediaKind::Audio, None, 2);
    // 別のノードの参加者とサイマルキャストのトラック
    assert!(h.svc.add_remote_participant(h.now, "r", ParticipantInfo {
        connection_id: "remote1".into(),
        client_id: Some("far".into()),
        bundle_id: "remote1".into(),
        role: Role::Sendrecv,
        metadata: json!({}),
        tracks: vec![],
        node: Some("node-b".into()),
    }));
    assert!(h.svc.add_remote_track(h.now, "r", TrackInfo {
        track_id: "remote1.0".into(),
        connection_id: "remote1".into(),
        kind: Kind::Video,
        rids: vec!["r0".into(), "r1".into()],
    }, Codec::Vp8));
    let m = h.take(s);
    assert!(m.iter().any(|x| matches!(x, ServerMessage::Notify(Notify::ConnectionCreated { participant }) if participant.node.as_deref() == Some("node-b"))));
    assert!(m.iter().any(|x| matches!(x, ServerMessage::Renegotiate(r) if r.video == 1)), "{m:?}");
    let m = h.offer(s, &[("video", "sendonly", &[]), ("audio", "sendonly", &[]), ("video", "recvonly", &[])]);
    let sl = slots_of(&m);
    assert_eq!(sl[0].track_id.as_deref(), Some("remote1.0"));
    assert_eq!(sl[0].client_id.as_deref(), Some("far"));
    // 自動の層なので全部の層を求める
    let acts = h.take_other();
    assert!(acts.iter().any(|a| matches!(a, Action::Relay(RelayAction::Subscribe { node, track_id, rids, .. }) if node == "node-b" && track_id == "remote1.0" && rids.len() == 2)), "{acts:?}");
    assert!(acts.iter().any(|a| matches!(a, Action::Relay(RelayAction::KeyframeRequest { node, .. }) if node == "node-b")));
    h.connected(s);
    // 届いた AU は購読者へ
    let au = AccessUnit {
        track: TrackId(0),
        codec: Codec::Vp8,
        epoch: 0,
        sequence: 0,
        config_generation: 0,
        pts: 0,
        dts: 0,
        duration: 3000,
        timebase: Timebase::new(1, 90000),
        keyframe: true,
        discontinuity: false,
        capture_time_ns: None,
        payload: Bytes::from(vec![1u8; 500]),
    };
    h.svc.on_remote_access_unit(h.now, "r", "remote1.0", Some("r0"), &au, None);
    assert_eq!(forwards(&h.take_other(), s).len(), 1);
    // このノードのトラックを別のノードが要る
    let local_video = format!("{}.0", h.svc.connection_id(s).unwrap());
    h.svc.on_relay_subscribe(h.now, "node-b", "r", &local_video, Some(vec![]));
    assert!(h.take_other().iter().any(|a| matches!(a, Action::RequestKeyframe { conn, track } if *conn == s && *track == TrackId(1))));
    h.au(s, 1, MediaKind::Video, 300, true);
    assert!(h.take_other().iter().any(|a| matches!(a, Action::Relay(RelayAction::Forward { node, track_id, .. }) if node == "node-b" && *track_id == local_video)));
    h.svc.on_relay_keyframe_request(h.now, "r", &local_video, None);
    assert!(h.take_other().iter().any(|a| matches!(a, Action::RequestKeyframe { conn, .. } if *conn == s)));
    h.svc.on_relay_subscribe(h.now, "node-b", "r", &local_video, None);
    h.au(s, 1, MediaKind::Video, 300, false);
    assert!(!h.take_other().iter().any(|a| matches!(a, Action::Relay(RelayAction::Forward { .. }))));
    // message は別のノードへ中継し、別のノードからのものは配る
    dc_open(&mut h, s, 1, "#chat");
    h.svc.on_peer_event(h.now, s, PcEvent::DataChannelMessage { id: 1, data: Bytes::from_static(b"hi"), binary: false });
    assert!(h.take_other().iter().any(|a| matches!(a, Action::Relay(RelayAction::Message { node, label, .. }) if node == "node-b" && label == "#chat")));
    h.svc.on_relay_message(h.now, "r", "#chat", "remote1", &[], Bytes::from_static(b"yo"), false);
    assert!(h.take_other().iter().any(|a| matches!(a, Action::SendData { conn, data, .. } if *conn == s && &data[..] == b"yo")));
    // 部屋の数に数える
    assert_eq!(h.svc.api_get_room("r").unwrap().participants, 2);
    // 外すと購読をやめ、枠は空く
    h.svc.remove_remote_participant(h.now, "r", "remote1");
    let acts = h.take_other();
    assert!(acts.iter().any(|a| matches!(a, Action::Relay(RelayAction::Unsubscribe { track_id, .. }) if track_id == "remote1.0")), "{acts:?}");
    let m = h.take(s);
    assert!(slots_of(&m).iter().any(|x| x.state == SlotState::Empty));
}

/// 見直しで見つかった穴の回帰試験: 名乗った client_id・bundle_id、宣言していないラベル、受け手の権限。
#[test]
fn review_regressions_identity_and_labels() {
    let mut h = H::new(|_| {});
    // トークンに client_id が無いのに名乗る → 拒否（転送フィルターをすり抜けられない）
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("r", Role::Sendrecv, None, json!({}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv","token":t,"client_id":"teacher"}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::Forbidden, true)]);
    // bundle_id も同じ
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("r", Role::Sendrecv, None, json!({}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv","token":t,"bundle_id":"teachers"}));
    assert_eq!(errors(&h.take(k)), vec![(ErrorCode::Forbidden, true)]);
    // トークンが bundle_id を決めていれば、それを使う
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("r", Role::Sendrecv, Some("s1"), json!({"bundle_id": "b1"}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"r","role":"sendrecv","token":t}));
    assert!(h.take(k).iter().any(|m| matches!(m, ServerMessage::Connected(_))));
    assert_eq!(h.svc.api_get_participant("r", h.svc.connection_id(k).unwrap()).unwrap().info.bundle_id, "b1");

    // シグナリングの message: 受け手の権限と宣言を確かめる・送り手は送る向きで宣言したラベルだけ
    let (a, _) = h.join("m", Role::Sendrecv, "a", json!({"data_channels":[{"label":"#admin","direction":"sendrecv"},{"label":"#ro","direction":"recvonly"}]}));
    let kb = h.svc.open(h.now, Default::default());
    let t = h.token("m", Role::Sendrecv, Some("b"), json!({"permissions": {"data": ["#chat"]}}));
    h.send(kb, json!({"type":"connect","version":1,"room_id":"m","role":"sendrecv","token":t,"data_channels":[{"label":"#chat"}]}));
    h.take(kb);
    let (c, _) = h.join("m", Role::Sendrecv, "c", json!({"data_channels":[{"label":"#admin","direction":"sendrecv"}]}));
    h.inbox.clear();
    h.send(a, json!({"type":"message","label":"#admin","data":"secret"}));
    assert!(h.take(kb).is_empty(), "#admin を許されていない参加者には届かない");
    assert!(h.take(c).iter().any(|m| matches!(m, ServerMessage::Message(x) if x.data.as_deref() == Some("secret"))));
    // recvonly で宣言したラベルでは送れない
    h.send(a, json!({"type":"message","label":"#ro","data":"x"}));
    assert_eq!(errors(&h.take(a)), vec![(ErrorCode::UnknownLabel, false)]);
    assert!(h.take(c).is_empty());
}

/// 部屋が別のノードへ移る（グリッド）: 今の参加者は redirect を受けて切れ、新しい参加も案内される。
#[test]
fn redirect_room_moves_members_and_new_joins() {
    let mut h = H::new(|_| {});
    let (a, _) = h.join("mv", Role::Sendrecv, "a", json!({}));
    let (b, _) = h.join("mv", Role::Sendrecv, "b", json!({}));
    h.inbox.clear();
    assert_eq!(h.svc.redirect_room(h.now, "mv", "wss://n2.example/signaling".into()), 2);
    for k in [a, b] {
        assert!(h.take(k).iter().any(|m| matches!(m, ServerMessage::Redirect(r) if r.location == "wss://n2.example/signaling")));
    }
    // 新しい参加も案内される
    let k = h.svc.open(h.now, Default::default());
    let t = h.token("mv", Role::Sendrecv, Some("c"), json!({}));
    h.send(k, json!({"type":"connect","version":1,"room_id":"mv","role":"sendrecv","token":t}));
    assert!(h.take(k).iter().any(|m| matches!(m, ServerMessage::Redirect(_))));
    assert_eq!(h.svc.redirect_room(h.now, "nope", "x".into()), 0);
}

/// 書き出し（メディアパイプライン）: サイマルキャストの一つの層だけを、途中から購読しても設定付きで受ける。
#[test]
fn relay_subscribe_to_one_simulcast_layer() {
    let mut h = H::new(|_| {});
    let (a, _) = h.join("r", Role::Sendonly, "a", json!({"simulcast":{"rids":["r0","r1","r2"]}}));
    h.offer(a, &[("video", "sendonly", &["r0", "r1", "r2"]), ("audio", "sendonly", &[])]);
    start_media(&mut h, a, 1);
    h.connected(a);
    let tracks = h.svc.api_tracks("r").unwrap();
    let v = tracks.iter().find(|t| t.kind == Kind::Video).unwrap().clone();
    assert_eq!(v.rids, vec!["r0", "r1", "r2"], "{tracks:?}");
    h.take_other();
    h.svc.on_relay_subscribe(h.now, "bridge", "r", &v.track_id, Some(vec!["r2".into()]));
    for n in 0..5 {
        h.au(a, 1, MediaKind::Video, 100, n == 0);
        h.au(a, 3, MediaKind::Video, 1200, n == 0);
        h.now += Duration::from_millis(33);
    }
    let acts = h.take_other();
    let fw: Vec<(Option<String>, usize)> = acts
        .iter()
        .filter_map(|x| match x {
            Action::Relay(RelayAction::Forward { node, rid, au, .. }) if node == "bridge" => Some((rid.clone(), au.payload.len())),
            _ => None,
        })
        .collect();
    assert_eq!(fw.len(), 5, "{fw:?}");
    assert!(fw.iter().all(|(r, n)| r.as_deref() == Some("r2") && *n == 1200), "{fw:?}");
}
