//! 部屋のカスケード（ADR-0015）: 1 つの部屋の参加者が複数のノードにいる。
//!
//! - 部屋を置いたノード（グリッドの割り当て）が **home**。部屋の設定 `cascade`（またはノードの `--rooms-cascade`）が
//!   真なら、別のノードにつないだ参加者はそのノードの **edge** の部屋で受ける（redirect しない）。
//! - 在室とトラックは、ノード間の中継（ADR-0014）の App の枠（topic [`TOPIC`]）で、部屋ごとの **全量の同期**
//!   （`sync`）として送る。変わったらすぐ（少しまとめて）、変わらなくても [`SYNC_EVERY`] ごと。届かなくなって
//!   [`EXPIRE`] 経った相手の参加者は外す（ノードが落ちた・つながらない）。差分を送らないので、取りこぼし・順序の
//!   入れ替わり・つなぎ直しに強い。
//! - 星形: edge は自分の参加者を home へ送る。home は自分の参加者と、他の edge から知った参加者（`node` は元のノード）を
//!   各 edge へ送る。メディア（`@room/<部屋>/<トラック>` の購読）とアプリの message は、元のノードへ直接（ノードの組
//!   ごとの接続は全部の組にある）。
//! - 別のノードの参加者は部屋の核の `add_remote_participant`・`add_remote_track` で足す（参加者の上限はノードを
//!   またいで数える。同時の参加で少し超えることはある）。
//!
//! 部屋の核（[`RoomService`]）を引数で受ける sans-io の状態機械（送るものは [`Out`] で返す）。

use bytes::Bytes;
use senca_core::Codec;
use senca_rooms::Kind;
use senca_rooms::service::RoomService;
use senca_rooms::signaling::{ParticipantInfo, TrackInfo};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

/// App の枠の topic（規約の名前と版）。
pub const TOPIC: &str = "senca.rooms/1";
/// 本文の版。
pub const VERSION: u32 = 1;
/// 部屋のトラックの購読の stream id の頭（`@room/<部屋>/<トラック>`）。
pub const STREAM_PREFIX: &str = "@room/";
/// 変わらなくても同期を送る間隔。
pub const SYNC_EVERY: Duration = Duration::from_secs(2);
/// 同期がこれだけ届かない相手の参加者は外す。
pub const EXPIRE: Duration = Duration::from_secs(7);
/// 変わってから同期を送るまで（続けて変わるものをまとめる）。
const DEBOUNCE: Duration = Duration::from_millis(100);
/// 1 つの同期に載せる参加者の上限（部屋の上限と同じ）。
const MAX_PARTICIPANTS: usize = 10_000;
const MAX_TRACKS: usize = 16;
const MAX_ID: usize = 256;
/// 1 つの message の data の上限（REST・シグナリングと同じ 64 KiB）。
const MAX_DATA: usize = 64 * 1024;

pub fn stream_id(room: &str, track: &str) -> String {
    format!("{STREAM_PREFIX}{room}/{track}")
}

/// `@room/<部屋>/<トラック>` を分ける（部屋の ID に `/` は入らない）。
pub fn parse_stream_id(s: &str) -> Option<(&str, &str)> {
    let (room, track) = s.strip_prefix(STREAM_PREFIX)?.split_once('/')?;
    (!room.is_empty() && !track.is_empty() && !track.contains('/')).then_some((room, track))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case", deny_unknown_fields)]
enum Body {
    /// 送り手の知る参加者の全量（edge → home は自分の参加者、home → edge は宛先の edge 以外の全員）。
    Sync { room: String, home: bool, participants: Vec<ParticipantInfo> },
    /// アプリの message（`binary` なら `data` は base64）。
    Message { room: String, label: String, from: String, to: Vec<String>, data: String, binary: bool },
    /// この部屋から抜ける（送り手の参加者を外してよい）。
    Leave { room: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    v: u32,
    body: Body,
}

/// 部屋の中でのこのノードの役目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    Home,
    /// `home` は今の home のノード（決まっていなければ None）。
    Edge { home: Option<String> },
}

/// 送るもの（宛先のノードと App の payload）。
#[derive(Debug, Clone, PartialEq)]
pub struct Out {
    pub node: String,
    pub payload: Bytes,
}

#[derive(Debug, Default)]
struct Source {
    last: Option<Instant>,
    parts: BTreeMap<String, ParticipantInfo>,
}

#[derive(Debug)]
struct CRoom {
    role: Role,
    /// 相手のノードごとに、その相手から知った参加者。
    sources: BTreeMap<String, Source>,
    dirty_at: Option<Instant>,
    next_sync: Instant,
}

/// カスケードの状態（部屋ごと）。
#[derive(Debug)]
pub struct Cascade {
    me: String,
    rooms: HashMap<String, CRoom>,
}

fn encode(body: Body) -> Bytes {
    Bytes::from(serde_json::to_vec(&Envelope { v: VERSION, body }).unwrap_or_default())
}

fn same_identity(a: &ParticipantInfo, b: &ParticipantInfo) -> bool {
    a.client_id == b.client_id && a.bundle_id == b.bundle_id && a.role == b.role && a.metadata == b.metadata && a.node == b.node
}

fn valid_participant(p: &ParticipantInfo) -> bool {
    let ok_id = |s: &str| !s.is_empty() && s.len() <= MAX_ID;
    ok_id(&p.connection_id)
        && ok_id(&p.bundle_id)
        && p.node.as_deref().is_some_and(ok_id)
        && p.client_id.as_deref().is_none_or(|c| c.len() <= MAX_ID)
        && p.tracks.len() <= MAX_TRACKS
        && p.tracks.iter().all(|t| ok_id(&t.track_id) && t.connection_id == p.connection_id && !t.track_id.contains('/'))
}

fn codec_of(svc: &RoomService, room: &str, kind: Kind) -> Codec {
    match kind {
        Kind::Audio => Codec::Opus,
        Kind::Video => svc.api_get_room(room).map(|v| v.settings.video_codec.codec()).unwrap_or(Codec::Vp8),
    }
}

impl Cascade {
    pub fn new(me: &str) -> Cascade {
        Cascade { me: me.to_string(), rooms: HashMap::new() }
    }

    /// 部屋の役目を決める（無ければ足す）。変わったら、今の役目で受けない相手から知った参加者を外す。
    pub fn set_role(&mut self, now: Instant, svc: &mut RoomService, room: &str, role: Role) {
        let r = self.rooms.entry(room.to_string()).or_insert_with(|| CRoom {
            role: role.clone(),
            sources: BTreeMap::new(),
            dirty_at: Some(now),
            next_sync: now,
        });
        if r.role == role {
            return;
        }
        r.role = role;
        r.dirty_at = Some(now);
        let keep = match &r.role {
            Role::Home => None,
            Role::Edge { home } => home.clone(),
        };
        // 役目が変わった: 前の役目で知った参加者を一度外す（新しい相手の同期ですぐ戻る）
        let drop: Vec<String> = r.sources.keys().filter(|k| keep.as_ref() != Some(*k) || matches!(r.role, Role::Home)).cloned().collect();
        for s in drop {
            self.apply(now, svc, room, &s, Vec::new());
            if let Some(r) = self.rooms.get_mut(room) {
                r.sources.remove(&s);
            }
        }
    }

    /// 部屋のカスケードをやめる（部屋を消した・edge を畳んだ）。相手へ `leave` を送る。
    pub fn remove(&mut self, room: &str) -> Vec<Out> {
        let Some(r) = self.rooms.remove(room) else { return Vec::new() };
        let to: Vec<String> = match r.role {
            Role::Home => r.sources.into_keys().collect(),
            Role::Edge { home } => home.into_iter().collect(),
        };
        to.into_iter().map(|node| Out { node, payload: encode(Body::Leave { room: room.to_string() }) }).collect()
    }

    /// この部屋の参加者・トラックが変わった（このノードの出来事）。
    pub fn mark_dirty(&mut self, now: Instant, room: &str) {
        if let Some(r) = self.rooms.get_mut(room)
            && r.dirty_at.is_none()
        {
            r.dirty_at = Some(now);
        }
    }

    /// 別のノードのトラックのサイマルキャストの層（低い順）。
    pub fn track_rids(&self, room: &str, track: &str) -> Option<Vec<String>> {
        let r = self.rooms.get(room)?;
        r.sources.values().flat_map(|s| s.parts.values()).flat_map(|p| p.tracks.iter()).find(|t| t.track_id == track).map(|t| t.rids.clone())
    }

    /// アプリの message を `node` へ。
    #[allow(clippy::too_many_arguments)]
    pub fn message(&self, node: &str, room: &str, label: &str, from: &str, to: &[String], data: &[u8], binary: bool) -> Option<Out> {
        use base64::Engine;
        if data.len() > MAX_DATA {
            return None;
        }
        let data = if binary { base64::engine::general_purpose::STANDARD.encode(data) } else { String::from_utf8_lossy(data).into_owned() };
        let body = Body::Message { room: room.into(), label: label.into(), from: from.into(), to: to.to_vec(), data, binary };
        Some(Out { node: node.to_string(), payload: encode(body) })
    }

    /// 相手のノード `from` から届いた payload。
    pub fn on_payload(&mut self, now: Instant, svc: &mut RoomService, from: &str, payload: &[u8]) -> Result<(), String> {
        let env: Envelope = serde_json::from_slice(payload).map_err(|e| format!("cascade: bad message from {from}: {e}"))?;
        if env.v != VERSION {
            return Err(format!("cascade: unsupported version {} from {from}", env.v));
        }
        match env.body {
            Body::Sync { room, home, participants } => {
                if participants.len() > MAX_PARTICIPANTS {
                    return Err(format!("cascade: too many participants from {from}"));
                }
                let Some(r) = self.rooms.get(&room) else { return Ok(()) };
                let accept = match &r.role {
                    Role::Home => !home,
                    Role::Edge { home: Some(h) } => home && h == from,
                    Role::Edge { home: None } => false,
                };
                if !accept {
                    return Ok(());
                }
                let list = participants.into_iter().filter(valid_participant).collect();
                let new_source = !r.sources.contains_key(from);
                let changed = self.apply(now, svc, &room, from, list);
                if let Some(r) = self.rooms.get_mut(&room) {
                    if let Some(s) = r.sources.get_mut(from) {
                        s.last = Some(now);
                    }
                    // home は変化を他の edge へ、新しい edge には今の全量をすぐ渡す
                    if matches!(r.role, Role::Home) && (changed || new_source) && r.dirty_at.is_none() {
                        r.dirty_at = Some(now);
                    }
                }
            }
            Body::Message { room, label, from: sender, to, data, binary } => {
                use base64::Engine;
                if data.len() > MAX_DATA * 2 || to.len() > MAX_PARTICIPANTS || label.len() > 64 || sender.len() > MAX_ID {
                    return Err(format!("cascade: message too large from {from}"));
                }
                let data = if binary {
                    Bytes::from(base64::engine::general_purpose::STANDARD.decode(data.as_bytes()).map_err(|_| "cascade: bad base64".to_string())?)
                } else {
                    Bytes::from(data.into_bytes())
                };
                if data.len() > MAX_DATA {
                    return Err(format!("cascade: message too large from {from}"));
                }
                if self.rooms.contains_key(&room) {
                    svc.on_relay_message(now, &room, &label, &sender, &to, data, binary);
                }
            }
            Body::Leave { room } => {
                if self.rooms.get(&room).is_some_and(|r| r.sources.contains_key(from)) {
                    self.apply(now, svc, &room, from, Vec::new());
                    if let Some(r) = self.rooms.get_mut(&room) {
                        r.sources.remove(from);
                        if matches!(r.role, Role::Home) {
                            r.dirty_at.get_or_insert(now);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// `source` から知った参加者を `list` に合わせる。変わったら true。
    fn apply(&mut self, now: Instant, svc: &mut RoomService, room: &str, source: &str, list: Vec<ParticipantInfo>) -> bool {
        let Some(r) = self.rooms.get_mut(room) else { return false };
        // 他の相手から知った人・手元の人は足さない（役目の移り目に二重にしない）
        let others: BTreeSet<String> =
            r.sources.iter().filter(|(k, _)| k.as_str() != source).flat_map(|(_, s)| s.parts.keys().cloned()).collect();
        let locals: BTreeSet<String> =
            svc.local_participants(room).unwrap_or_default().into_iter().map(|p| p.connection_id).collect();
        let want: BTreeMap<String, ParticipantInfo> = list
            .into_iter()
            .filter(|p| p.node.as_deref() != Some(self.me.as_str()))
            .filter(|p| !others.contains(&p.connection_id) && !locals.contains(&p.connection_id))
            .map(|p| (p.connection_id.clone(), p))
            .collect();
        let src = r.sources.entry(source.to_string()).or_default();
        let old = std::mem::take(&mut src.parts);
        let mut changed = false;
        for (cid, p) in &old {
            if want.get(cid).is_none_or(|n| !same_identity(p, n)) {
                svc.remove_remote_participant(now, room, cid);
                changed = true;
            }
        }
        let mut kept = BTreeMap::new();
        for (cid, p) in want {
            let prev = old.get(&cid).filter(|o| same_identity(o, &p));
            if prev.is_none() {
                if !svc.add_remote_participant(now, room, p.clone()) {
                    continue;
                }
                changed = true;
            }
            let prev_tracks: &[TrackInfo] = prev.map(|o| o.tracks.as_slice()).unwrap_or(&[]);
            let mut stored = p.clone();
            stored.tracks.clear();
            for t in prev_tracks {
                if p.tracks.contains(t) {
                    stored.tracks.push(t.clone());
                } else {
                    svc.remove_remote_track(now, room, &t.track_id);
                    changed = true;
                }
            }
            for t in &p.tracks {
                if prev_tracks.contains(t) {
                    continue;
                }
                let codec = codec_of(svc, room, t.kind);
                if svc.add_remote_track(now, room, t.clone(), codec) {
                    stored.tracks.push(t.clone());
                    changed = true;
                }
            }
            kept.insert(cid, stored);
        }
        if let Some(src) = self.rooms.get_mut(room).and_then(|r| r.sources.get_mut(source)) {
            src.parts = kept;
        }
        changed
    }

    /// 期限切れの相手を外し、送る同期を返す。
    pub fn poll(&mut self, now: Instant, svc: &mut RoomService) -> Vec<Out> {
        let mut out = Vec::new();
        let ids: Vec<String> = self.rooms.keys().cloned().collect();
        for id in ids {
            let expired: Vec<String> = self.rooms[&id]
                .sources
                .iter()
                .filter(|(_, s)| s.last.is_some_and(|t| now.saturating_duration_since(t) >= EXPIRE))
                .map(|(k, _)| k.clone())
                .collect();
            for s in expired {
                self.apply(now, svc, &id, &s, Vec::new());
                if let Some(r) = self.rooms.get_mut(&id) {
                    r.sources.remove(&s);
                    if matches!(r.role, Role::Home) {
                        r.dirty_at.get_or_insert(now);
                    }
                }
            }
            let Some(r) = self.rooms.get_mut(&id) else { continue };
            let due = r.dirty_at.is_some_and(|d| now >= d + DEBOUNCE) || now >= r.next_sync;
            if !due {
                continue;
            }
            r.dirty_at = None;
            r.next_sync = now + SYNC_EVERY;
            let locals = svc.local_participants(&id).unwrap_or_default();
            match &r.role {
                Role::Home => {
                    for dest in r.sources.keys() {
                        let mut ps = locals.clone();
                        for (k, s) in &r.sources {
                            if k != dest {
                                ps.extend(s.parts.values().cloned());
                            }
                        }
                        out.push(Out { node: dest.clone(), payload: encode(Body::Sync { room: id.clone(), home: true, participants: ps }) });
                    }
                }
                Role::Edge { home: Some(h) } => {
                    out.push(Out { node: h.clone(), payload: encode(Body::Sync { room: id.clone(), home: false, participants: locals }) });
                }
                Role::Edge { home: None } => {}
            }
        }
        out
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        self.rooms
            .values()
            .flat_map(|r| {
                [Some(r.next_sync), r.dirty_at.map(|d| d + DEBOUNCE)]
                    .into_iter()
                    .flatten()
                    .chain(r.sources.values().filter_map(|s| s.last.map(|t| t + EXPIRE)))
            })
            .min()
    }

    /// 状態（GET /nodes の補足など）。
    pub fn status(&self) -> serde_json::Value {
        let rooms: Vec<serde_json::Value> = self
            .rooms
            .iter()
            .map(|(id, r)| {
                let (role, home) = match &r.role {
                    Role::Home => ("home", None),
                    Role::Edge { home } => ("edge", home.clone()),
                };
                let peers: BTreeMap<&String, usize> = r.sources.iter().map(|(k, s)| (k, s.parts.len())).collect();
                serde_json::json!({ "room": id, "role": role, "home": home, "peers": peers })
            })
            .collect();
        serde_json::json!({ "rooms": rooms })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use senca_rooms::service::{Action, ServiceConfig};

    fn svc(node: &str, now: Instant) -> RoomService {
        let mut s = RoomService::new(ServiceConfig { node_id: node.into(), allow_anonymous: true, ..Default::default() }, now, 0);
        s.api_create_room(now, senca_rooms::api::CreateRoom { id: Some("r".into()), settings: None, metadata: serde_json::Value::Null })
            .unwrap();
        while s.poll_action().is_some() {}
        s
    }

    fn part(cid: &str, node: &str, tracks: &[&str]) -> ParticipantInfo {
        ParticipantInfo {
            connection_id: cid.into(),
            client_id: Some(format!("c-{cid}")),
            bundle_id: cid.into(),
            role: senca_rooms::Role::Sendrecv,
            metadata: serde_json::Value::Null,
            tracks: tracks
                .iter()
                .map(|t| TrackInfo { track_id: format!("{cid}.{t}"), connection_id: cid.into(), kind: Kind::Video, rids: vec![] })
                .collect(),
            node: Some(node.into()),
        }
    }

    fn sync(home: bool, ps: Vec<ParticipantInfo>) -> Vec<u8> {
        encode(Body::Sync { room: "r".into(), home, participants: ps }).to_vec()
    }

    fn remote_ids(s: &RoomService) -> Vec<String> {
        let mut v: Vec<String> = s.remote_participants("r").unwrap().into_iter().map(|p| p.connection_id).collect();
        v.sort();
        v
    }

    #[test]
    fn stream_ids() {
        assert_eq!(stream_id("a", "b.0"), "@room/a/b.0");
        assert_eq!(parse_stream_id("@room/a/b.0"), Some(("a", "b.0")));
        assert_eq!(parse_stream_id("@room/a"), None);
        assert_eq!(parse_stream_id("@room//x"), None);
        assert_eq!(parse_stream_id("@room/a/b/c"), None);
        assert_eq!(parse_stream_id("live/a"), None);
    }

    #[test]
    fn home_learns_edges_and_rebroadcasts() {
        let now = Instant::now();
        let mut h = svc("H", now);
        let mut c = Cascade::new("H");
        c.set_role(now, &mut h, "r", Role::Home);
        // edge E1 の参加者 a（映像 1 本）
        c.on_payload(now, &mut h, "E1", &sync(false, vec![part("a", "E1", &["v"])])).unwrap();
        assert_eq!(remote_ids(&h), vec!["a"]);
        assert_eq!(h.remote_participants("r").unwrap()[0].tracks.len(), 1);
        // home のふりをした同期・自分自身の参加者は受けない
        c.on_payload(now, &mut h, "E2", &sync(true, vec![part("x", "E2", &[])])).unwrap();
        c.on_payload(now, &mut h, "E2", &sync(false, vec![part("me", "H", &[])])).unwrap();
        assert_eq!(remote_ids(&h), vec!["a"]);
        c.on_payload(now, &mut h, "E2", &sync(false, vec![part("b", "E2", &[])])).unwrap();
        assert_eq!(remote_ids(&h), vec!["a", "b"]);
        // 各 edge へ、宛先以外の全員を送る
        let out = c.poll(now + DEBOUNCE, &mut h);
        let get = |n: &str| -> Vec<String> {
            let o = out.iter().find(|o| o.node == n).unwrap();
            let e: Envelope = serde_json::from_slice(&o.payload).unwrap();
            let Body::Sync { participants, home, .. } = e.body else { panic!() };
            assert!(home);
            participants.into_iter().map(|p| p.connection_id).collect()
        };
        assert_eq!(get("E1"), vec!["b"]);
        assert_eq!(get("E2"), vec!["a"]);
        // トラックを外す・足す
        c.on_payload(now, &mut h, "E1", &sync(false, vec![part("a", "E1", &["w"])])).unwrap();
        let ts: Vec<String> = h.remote_participants("r").unwrap().into_iter().find(|p| p.connection_id == "a").unwrap().tracks.into_iter().map(|t| t.track_id).collect();
        assert_eq!(ts, vec!["a.w"]);
        assert_eq!(c.track_rids("r", "a.w"), Some(vec![]));
        // E2 からの同期が途絶えたら、その参加者を外す（E1 は同期を続ける）
        let later = now + EXPIRE - Duration::from_millis(1);
        c.on_payload(later, &mut h, "E1", &sync(false, vec![part("a", "E1", &["w"])])).unwrap();
        c.poll(now + EXPIRE, &mut h);
        assert_eq!(remote_ids(&h), vec!["a"]);
        // leave
        c.on_payload(later, &mut h, "E1", &encode(Body::Leave { room: "r".into() })).unwrap();
        assert!(remote_ids(&h).is_empty());
    }

    #[test]
    fn edge_follows_home_only_and_switches() {
        let now = Instant::now();
        let mut e = svc("E", now);
        let mut c = Cascade::new("E");
        c.set_role(now, &mut e, "r", Role::Edge { home: Some("H".into()) });
        // 最初の poll で home へ同期（自分の参加者。まだ空）
        let out = c.poll(now, &mut e);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].node, "H");
        // home 以外・edge の印の同期は受けない
        c.on_payload(now, &mut e, "X", &sync(true, vec![part("x", "X", &[])])).unwrap();
        c.on_payload(now, &mut e, "H", &sync(false, vec![part("x", "H", &[])])).unwrap();
        assert!(remote_ids(&e).is_empty());
        c.on_payload(now, &mut e, "H", &sync(true, vec![part("h1", "H", &["v"]), part("o1", "O", &[])])).unwrap();
        assert_eq!(remote_ids(&e), vec!["h1", "o1"]);
        // 同じ同期をもう一度: 何も変えない（部屋の核への出し入れが無い）
        while e.poll_action().is_some() {}
        c.on_payload(now, &mut e, "H", &sync(true, vec![part("h1", "H", &["v"]), part("o1", "O", &[])])).unwrap();
        assert!(!std::iter::from_fn(|| e.poll_action()).any(|a| matches!(a, Action::Relay(_))));
        // home が変わった: 前の home から知った参加者を外し、新しい home へ同期する
        c.set_role(now, &mut e, "r", Role::Edge { home: Some("H2".into()) });
        assert!(remote_ids(&e).is_empty());
        let out = c.poll(now + DEBOUNCE, &mut e);
        assert_eq!(out.iter().map(|o| o.node.as_str()).collect::<Vec<_>>(), vec!["H2"]);
        // home になった
        c.on_payload(now, &mut e, "H2", &sync(true, vec![part("o1", "O", &[])])).unwrap();
        c.set_role(now, &mut e, "r", Role::Home);
        assert!(remote_ids(&e).is_empty());
        assert_eq!(c.remove("r"), Vec::new());
    }

    #[test]
    fn messages_and_bounds() {
        let now = Instant::now();
        let mut e = svc("E", now);
        let mut c = Cascade::new("E");
        c.set_role(now, &mut e, "r", Role::Edge { home: Some("H".into()) });
        let o = c.message("H", "r", "#chat", "a", &[], &[0, 1, 2, 255], true).unwrap();
        let env: Envelope = serde_json::from_slice(&o.payload).unwrap();
        assert!(matches!(env.body, Body::Message { binary: true, ref data, .. } if data == "AAEC/w=="));
        assert!(c.on_payload(now, &mut e, "H", &o.payload).is_ok());
        assert!(c.message("H", "r", "#chat", "a", &[], &vec![0u8; MAX_DATA + 1], false).is_none());
        // 版・形の違うものは誤り
        assert!(c.on_payload(now, &mut e, "H", br#"{"v":2,"body":{"t":"leave","room":"r"}}"#).is_err());
        assert!(c.on_payload(now, &mut e, "H", br#"{"v":1,"body":{"t":"bogus"}}"#).is_err());
        assert!(c.on_payload(now, &mut e, "H", b"not json").is_err());
        // 形の悪い参加者（ノードが無い・トラックの持ち主が違う）は捨てる
        let mut bad = part("z", "H", &["v"]);
        bad.tracks[0].connection_id = "other".into();
        let mut nonode = part("y", "H", &[]);
        nonode.node = None;
        c.on_payload(now, &mut e, "H", &sync(true, vec![bad, nonode, part("ok", "H", &[])])).unwrap();
        assert_eq!(remote_ids(&e), vec!["ok"]);
    }
}
