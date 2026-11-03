//! MoQ（draft-ietf-moq-transport-22、`senca-moq`）でライブの stream を配る・受ける。
//!
//! - namespace は `senca/live/{stream_id}`。その下に hang の `catalog.json` と CMAF の track
//!   （`video`・`audio`、二本目からは `video2` など）を置く（`senca-moq/DRAFT.md` のメディアプロファイル v1）。
//! - media track の object 一つが CMAF fragment 一つ（moof+mdat、サンプル一つ）。映像は GOP ごと、
//!   音声はおよそ 1 秒ごとに新しい group（subgroup 0、Object ID は group の中で 0 から）。
//! - 素材は stream の hub（[`crate::hub`]）の購読。別のノードが持つ stream は中継の写し
//!   （[`App::source_hub`]）から作る。stream ごとに一つの変換（[`Feed`]）を全購読者で共有する。
//! - 購読の認可は再生の署名（HLS の署名付き URL と同じ `exp`・`sig`）。MoQ の AUTHORIZATION TOKEN
//!   （SETUP か要求）の値か、native QUIC の PATH・WebTransport の URL の query に `exp=..&sig=..` を置く。
//! - 送出（publish）: publish 鍵を AUTHORIZATION TOKEN に入れて PUBLISH_NAMESPACE `senca/live/{stream_id}`
//!   を送ると、ノードが `catalog.json` と CMAF の track を購読して stream の送り手になる。
//!   hang の `legacy`・`loc` 容器は受け付けない（容器の読み方が固定されていないため。`cmaf` だけ）。

use crate::app::App;
use crate::hub::{HubEvent, HubHandle};
use bytes::Bytes;
use senca_core::time::Timebase;
use senca_core::{AccessUnit, CodecConfig, MediaKind, TrackId};
use senca_moq::mapping::{CATALOG_TRACK, GroupMapper, MediaKind as MoqKind, Rendition, catalog_json, parse_catalog};
use senca_moq::params::{LocationFilter, Params, Token};
use senca_moq::{Location, Namespace, Object, RequestErrorCode, Session, SessionErrorCode, SessionEvent, SubscribeArgs, filter_end, filter_start};
use senca_quic::{ConnectionHandle, StreamId};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

/// namespace の前の二つ。
pub const NS_ROOT: [&str; 2] = ["senca", "live"];
/// track ごとに持っておく group の数（遅れて来た購読者・FETCH 用）。
const KEEP_GROUPS: usize = 2;
/// track ごとに持っておく量の上限。
const KEEP_BYTES: usize = 32 << 20;
/// 購読者がいなくなってから変換を止めるまで。
const FEED_IDLE: Duration = Duration::from_secs(10);
/// 音声の group の長さ（秒）。
const AUDIO_GROUP_SECS: i64 = 1;
/// 送出（publish）で受ける object の上限（一つの CMAF fragment）。
const MAX_INGEST_OBJECT: usize = 8 << 20;
/// 送出の catalog の track の数の上限。
const MAX_INGEST_TRACKS: usize = 8;

/// stream の namespace（`senca/live/{id}`）。
pub fn namespace_of(id: &str) -> Namespace {
    Namespace(vec![Bytes::from_static(b"senca"), Bytes::from_static(b"live"), Bytes::copy_from_slice(id.as_bytes())])
}

/// namespace から stream ID を読む（`senca/live/{id}` 以外は `None`）。
pub fn stream_of(ns: &Namespace) -> Option<String> {
    let [a, b, id] = ns.0.as_slice() else { return None };
    if a.as_ref() != NS_ROOT[0].as_bytes() || b.as_ref() != NS_ROOT[1].as_bytes() || id.is_empty() || id.len() > 128 {
        return None;
    }
    let id = std::str::from_utf8(id).ok()?;
    id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-').then(|| id.to_string())
}

/// RFC 6381 の codecs 文字列（catalog の `codec`）。
pub fn codec_string(c: &CodecConfig) -> String {
    use senca_core::Codec;
    match c.codec {
        Codec::H264 => match senca_h264::AvcDecoderConfigurationRecord::parse(&c.record) {
            Ok(r) => r.codecs_string(),
            Err(_) => {
                let v = c.video.unwrap_or_default();
                format!("avc1.{:02x}{:02x}{:02x}", v.profile, v.compat, v.level)
            }
        },
        Codec::H265 => senca_h265::HevcDecoderConfigurationRecord::parse(&c.record).map(|r| r.codecs_string()).unwrap_or_else(|_| "hvc1.1.6.L93.B0".into()),
        Codec::Av1 => senca_av1::Av1CodecConfigurationRecord::parse(&c.record).map(|r| r.codecs_string()).unwrap_or_else(|_| "av01.0.08M.08".into()),
        Codec::Vp8 => "vp8".into(),
        Codec::Vp9 => "vp09.00.10.08".into(),
        Codec::Aac => format!("mp4a.40.{}", c.audio.map_or(2, |a| if a.object_type == 0 { 2 } else { a.object_type })),
        Codec::Opus => "opus".into(),
        Codec::Pcma => "alaw".into(),
        Codec::Pcmu => "ulaw".into(),
        Codec::WebVtt => "wvtt".into(),
    }
}

// ------------------------------------------------------------------ hub → CMAF の object

/// 変換の出力。
#[derive(Debug, Clone)]
pub enum FeedEvent {
    /// catalog の新しい版（group 一つ・object 一つ）。
    Catalog { group: u64, json: Bytes },
    Object { track: Arc<str>, object: Object },
}

struct MuxTrack {
    name: Arc<str>,
    config: CodecConfig,
    timescale: u32,
    init: Bytes,
    writer: senca_fmp4::FragmentWriter,
    queue: senca_fmp4::SampleQueue,
    mapper: GroupMapper,
    video: bool,
    group_start: Option<i64>,
    /// 取りこぼし・不連続の後、映像は次のキーフレームまで落とす。
    need_key: bool,
    priority: u8,
}

/// hub の出来事を CMAF の object に変える（stream ごと）。
#[derive(Default)]
pub struct CmafMux {
    tracks: BTreeMap<TrackId, MuxTrack>,
    dirty: bool,
    next_catalog_group: u64,
}

impl CmafMux {
    pub fn new() -> Self {
        Self::default()
    }

    fn catalog(&mut self, out: &mut Vec<FeedEvent>) {
        let renditions: Vec<Rendition> = self
            .tracks
            .values()
            .map(|t| {
                let codec = codec_string(&t.config);
                let kind = if t.video {
                    let v = t.config.video.unwrap_or_default();
                    MoqKind::Video { codec, width: v.width, height: v.height, framerate: None }
                } else {
                    let a = t.config.audio.unwrap_or_default();
                    MoqKind::Audio { codec, sample_rate: a.sample_rate, channels: u32::from(a.channels) }
                };
                let description = (!t.config.record.is_empty()).then(|| t.config.record.clone());
                Rendition { track: t.name.to_string(), kind, init: t.init.clone(), bitrate: None, description }
            })
            .collect();
        let group = self.next_catalog_group;
        self.next_catalog_group += 1;
        out.push(FeedEvent::Catalog { group, json: Bytes::from(catalog_json(&renditions)) });
    }

    fn set_config(&mut self, t: TrackId, c: &CodecConfig) {
        let kind = c.codec.kind();
        if !matches!(kind, MediaKind::Video | MediaKind::Audio) {
            return;
        }
        if self.tracks.get(&t).is_some_and(|x| x.config == *c) {
            return;
        }
        let spec = senca_fmp4::TrackSpec::new(1, c.clone());
        let mut init = Vec::new();
        if senca_fmp4::write_init_segment(std::slice::from_ref(&spec), &mut init).is_err() {
            return;
        }
        let video = kind == MediaKind::Video;
        let (name, mapper) = match self.tracks.remove(&t) {
            // 同じ track の設定が変わった: 名前と group の番号はそのまま
            Some(old) => (old.name, old.mapper),
            None => {
                let base = if video { "video" } else { "audio" };
                let n = self.tracks.values().filter(|x| x.video == video).count();
                let name: Arc<str> = if n == 0 { base.into() } else { format!("{base}{}", n + 1).into() };
                (name, GroupMapper::new())
            }
        };
        self.tracks.insert(
            t,
            MuxTrack {
                name,
                config: c.clone(),
                timescale: spec.timescale,
                init: Bytes::from(init),
                writer: senca_fmp4::FragmentWriter::new(),
                queue: senca_fmp4::SampleQueue::new(spec.timescale),
                mapper,
                video,
                group_start: None,
                need_key: video,
                // 音声を先に（MoQ は小さいほど優先）
                priority: if video { 128 } else { 64 },
            },
        );
        self.dirty = true;
    }

    fn emit(tr: &mut MuxTrack, out: &mut Vec<FeedEvent>) {
        for s in tr.queue.take_ready() {
            if s.dts < 0 {
                continue;
            }
            let boundary = if tr.video {
                s.keyframe
            } else {
                tr.group_start.is_none_or(|g| s.dts - g >= AUDIO_GROUP_SECS * i64::from(tr.timescale) || s.dts < g)
            };
            if boundary {
                tr.group_start = Some(s.dts);
            }
            let mut v = Vec::with_capacity(s.data.len() + 128);
            if tr.writer.write(&[senca_fmp4::TrackRun { track_id: 1, samples: std::slice::from_ref(&s) }], &mut v).is_err() {
                continue;
            }
            if let Some(o) = tr.mapper.push(Bytes::from(v), boundary, tr.priority) {
                out.push(FeedEvent::Object { track: tr.name.clone(), object: o });
            }
        }
    }

    /// 送り手が切れた・取りこぼした: 残りを出し、映像は次のキーフレームから。
    pub fn break_continuity(&mut self, out: &mut Vec<FeedEvent>) {
        for tr in self.tracks.values_mut() {
            tr.queue.flush();
            Self::emit(tr, out);
            tr.queue = senca_fmp4::SampleQueue::new(tr.timescale);
            tr.need_key = tr.video;
            tr.group_start = None;
        }
    }

    pub fn on_event(&mut self, ev: &HubEvent, out: &mut Vec<FeedEvent>) {
        match ev {
            HubEvent::Config(t, c) => self.set_config(*t, c),
            HubEvent::Au(au) => self.on_au(au, out),
            HubEvent::Discontinuity | HubEvent::End => self.break_continuity(out),
            HubEvent::Segment { .. } => {}
        }
    }

    fn on_au(&mut self, au: &AccessUnit, out: &mut Vec<FeedEvent>) {
        if self.dirty {
            self.dirty = false;
            self.catalog(out);
        }
        let Some(tr) = self.tracks.get_mut(&au.track) else { return };
        if tr.need_key {
            if !au.keyframe {
                return;
            }
            tr.need_key = false;
        }
        if tr.queue.push(au).is_err() {
            // 時刻が戻った: 残りを出してから数え直す
            tr.queue.flush();
            Self::emit(tr, out);
            tr.queue = senca_fmp4::SampleQueue::new(tr.timescale);
            tr.group_start = None;
            if tr.video && !au.keyframe {
                tr.need_key = true;
                return;
            }
            let _ = tr.queue.push(au);
        }
        Self::emit(tr, out);
    }
}

/// hub を購読して object を作り、`tx` へ送る（stream ごとのタスク）。
async fn feed_task(hub: HubHandle, id: String, generation: u64, tx: mpsc::Sender<crate::quic::Msg>) {
    let mut sub = hub.subscribe();
    let mut mux = CmafMux::new();
    let mut out = Vec::new();
    for ev in sub.initial() {
        mux.on_event(&ev, &mut out);
    }
    loop {
        for ev in out.drain(..) {
            if tx.send(crate::quic::Msg::Feed { stream: id.clone(), generation, ev }).await.is_err() {
                return;
            }
        }
        match sub.recv().await {
            Ok(ev) => mux.on_event(&ev, &mut out),
            Err(broadcast::error::RecvError::Lagged(n)) => {
                sub.lagged(n);
                mux.break_continuity(&mut out);
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    let _ = tx.send(crate::quic::Msg::FeedEnded { stream: id, generation }).await;
}

// ------------------------------------------------------------------ 購読者への配り

/// MoQ のセッションの場所（接続と、WebTransport なら session の stream）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PeerKey {
    pub conn: ConnectionHandle,
    pub wt: Option<StreamId>,
}

/// セッションへの書き込み。
#[derive(Debug)]
pub enum Action {
    Send { req: u64, object: Object },
    EndGroup { req: u64, group: u64 },
}

#[derive(Default)]
struct TrackBuf {
    groups: VecDeque<(u64, Vec<Object>)>,
    bytes: usize,
}

impl TrackBuf {
    fn largest(&self) -> Option<Location> {
        self.groups.back().and_then(|(_, v)| v.last()).map(|o| o.location())
    }

    fn push(&mut self, o: &Object) {
        match self.groups.back_mut() {
            Some((g, v)) if *g == o.group => v.push(o.clone()),
            _ => self.groups.push_back((o.group, vec![o.clone()])),
        }
        self.bytes += o.payload.len();
        while self.groups.len() > KEEP_GROUPS || (self.bytes > KEEP_BYTES && self.groups.len() > 1) {
            if let Some((_, v)) = self.groups.pop_front() {
                self.bytes -= v.iter().map(|o| o.payload.len()).sum::<usize>();
            }
        }
    }

    fn range(&self, start: Location, end: Location) -> impl Iterator<Item = &Object> {
        self.groups.iter().flat_map(|(_, v)| v.iter()).filter(move |o| o.location() >= start && o.location() <= end)
    }
}

struct SubRef {
    peer: PeerKey,
    req: u64,
    track: Arc<str>,
    start: Location,
    end: Option<Location>,
    /// 視聴者の印（同じセッションの購読で共有する。最後の購読が消えると数が減る）。
    viewer: Arc<crate::viewers::ViewerGuard>,
}

/// 一つの stream の変換と購読者。
pub struct Feed {
    generation: u64,
    catalog: Option<Object>,
    tracks: HashMap<Arc<str>, TrackBuf>,
    subs: Vec<SubRef>,
    task: tokio::task::JoinHandle<()>,
    idle_since: Option<Instant>,
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Feed {
    fn largest(&self, track: &str) -> Option<Location> {
        if track == CATALOG_TRACK {
            return self.catalog.as_ref().map(|o| o.location());
        }
        self.tracks.get(track).and_then(|t| t.largest())
    }

    fn knows(&self, track: &str) -> bool {
        track == CATALOG_TRACK || self.tracks.contains_key(track)
    }

    /// cache にある object（範囲つき）。
    fn objects(&self, track: &str, start: Location, end: Location) -> Vec<Object> {
        if track == CATALOG_TRACK {
            return self.catalog.iter().filter(|o| o.location() >= start && o.location() <= end).cloned().collect();
        }
        self.tracks.get(track).map(|t| t.range(start, end).cloned().collect()).unwrap_or_default()
    }

    /// 完結した group（今の group より前）。
    fn current_group(&self, track: &str) -> Option<u64> {
        self.largest(track).map(|l| l.group)
    }
}

/// すべての stream の変換。
#[derive(Default)]
pub struct Feeds {
    feeds: HashMap<String, Feed>,
    next_generation: u64,
}

impl Feeds {
    /// stream の変換を用意する（無ければ hub の購読を始める）。stream が配信中でなければ `None`。
    fn ensure(&mut self, app: &Arc<App>, tx: &mpsc::Sender<crate::quic::Msg>, id: &str) -> Option<&mut Feed> {
        if !self.feeds.contains_key(id) {
            let hub = app.source_hub(id)?;
            self.next_generation += 1;
            let generation = self.next_generation;
            let task = tokio::spawn(feed_task(hub, id.to_string(), generation, tx.clone()));
            self.feeds.insert(
                id.to_string(),
                Feed { generation, catalog: None, tracks: HashMap::new(), subs: Vec::new(), task, idle_since: None },
            );
        }
        self.feeds.get_mut(id)
    }

    /// 変換の出力を取り込み、購読者への書き込みを返す。
    pub fn on_event(&mut self, id: &str, generation: u64, ev: FeedEvent) -> Vec<(PeerKey, Action)> {
        let mut acts = Vec::new();
        let Some(f) = self.feeds.get_mut(id).filter(|f| f.generation == generation) else { return acts };
        match ev {
            FeedEvent::Catalog { group, json } => {
                let o = Object {
                    group,
                    subgroup: Some(0),
                    id: 0,
                    priority: 0,
                    status: senca_moq::ObjectStatus::Normal,
                    properties: Bytes::new(),
                    payload: json,
                };
                for s in f.subs.iter().filter(|s| &*s.track == CATALOG_TRACK) {
                    acts.push((s.peer, Action::Send { req: s.req, object: o.clone() }));
                    acts.push((s.peer, Action::EndGroup { req: s.req, group }));
                }
                f.catalog = Some(o);
            }
            FeedEvent::Object { track, object } => {
                let prev = f.tracks.get(&track).and_then(|t| t.largest()).map(|l| l.group);
                f.tracks.entry(track.clone()).or_default().push(&object);
                for s in f.subs.iter().filter(|s| s.track == track) {
                    if let Some(g) = prev
                        && g != object.group
                        && Location::new(g, u64::MAX) >= s.start
                    {
                        acts.push((s.peer, Action::EndGroup { req: s.req, group: g }));
                    }
                    if object.location() >= s.start && s.end.is_none_or(|e| object.location() <= e) {
                        acts.push((s.peer, Action::Send { req: s.req, object: object.clone() }));
                    }
                }
            }
        }
        acts
    }

    /// 変換が終わった（hub が無くなった）。購読者には PUBLISH_DONE を送る。
    pub fn on_ended(&mut self, id: &str, generation: u64) -> Vec<(PeerKey, u64)> {
        if !self.feeds.get(id).is_some_and(|f| f.generation == generation) {
            return Vec::new();
        }
        let f = self.feeds.remove(id);
        f.map(|f| f.subs.iter().map(|s| (s.peer, s.req)).collect()).unwrap_or_default()
    }

    fn unsubscribe(&mut self, peer: PeerKey, req: Option<u64>) {
        let now = Instant::now();
        for f in self.feeds.values_mut() {
            f.subs.retain(|s| !(s.peer == peer && req.is_none_or(|r| r == s.req)));
            if f.subs.is_empty() && f.idle_since.is_none() {
                f.idle_since = Some(now);
            }
        }
    }

    /// セッション・接続が終わった。
    pub fn remove_peer(&mut self, peer: PeerKey) {
        self.unsubscribe(peer, None);
    }

    /// 誰も見ていない変換を止める。
    pub fn gc(&mut self, now: Instant) {
        self.feeds.retain(|_, f| !f.subs.is_empty() || f.idle_since.is_none_or(|t| now.duration_since(t) < FEED_IDLE));
    }
}

// ------------------------------------------------------------------ 認可

/// `exp=..&sig=..`（前に `?` や URL があってもよい）を読む。
fn exp_sig(s: &str) -> Option<(i64, String)> {
    let q = s.rsplit_once('?').map_or(s, |(_, q)| q);
    if q.len() > 1024 {
        return None;
    }
    let kv = senca_http::pct::parse_query(q).ok()?;
    let exp = kv.iter().find(|(k, _)| k == "exp")?.1.parse::<i64>().ok()?;
    let sig = kv.iter().find(|(k, _)| k == "sig")?.1.clone();
    Some((exp, sig))
}

fn token_strs(t: &[Token]) -> impl Iterator<Item = &str> {
    t.iter().filter_map(|t| std::str::from_utf8(&t.value).ok())
}

/// 購読者のセッション（ノードは出版者）。送出（ノードが購読者）も同じセッションで受ける。
pub struct MoqPeer {
    pub session: Session,
    /// WebTransport の URL の query（native QUIC は SETUP の PATH を使う）。
    pub url_query: Option<String>,
    pub peer_addr: String,
    /// 購読者の要求 → stream。
    subs: HashMap<u64, String>,
    ingest: Option<Ingest>,
}

impl MoqPeer {
    pub fn new(session: Session, url_query: Option<String>, peer_addr: String) -> Self {
        Self { session, url_query, peer_addr, subs: HashMap::new(), ingest: None }
    }

    /// 再生の署名で stream `id` の購読を許すか。
    fn may_play(&self, app: &App, id: &str, params: &Params) -> bool {
        if params.unknown_token_alias {
            return false;
        }
        let setup = self.session.peer_setup();
        let mut cands: Vec<&str> = token_strs(&params.auth_tokens).collect();
        if let Some(s) = setup {
            cands.extend(token_strs(&s.auth_tokens));
            if let Some(p) = s.path.as_deref() {
                cands.push(p);
            }
        }
        if let Some(q) = self.url_query.as_deref() {
            cands.push(q);
        }
        let now = crate::util::now_ms() / 1000;
        cands.into_iter().filter_map(exp_sig).any(|(exp, sig)| now <= exp && app.verify_signature("live", id, exp, &sig))
    }

    /// publish 鍵で stream `id` への送出を許すか（鍵は AUTHORIZATION TOKEN の値）。
    fn publish_stream(&self, app: &App, id: &str, params: &Params) -> Option<String> {
        if params.unknown_token_alias {
            return None;
        }
        let setup = self.session.peer_setup();
        let keys = token_strs(&params.auth_tokens).chain(setup.into_iter().flat_map(|s| token_strs(&s.auth_tokens)));
        for k in keys {
            if !k.starts_with(crate::api::streams::KEY_PREFIX) || k.len() > 128 {
                continue;
            }
            let h = senca_control::secret::hash(k);
            let ok = app.control.lock().unwrap().state.streams.get(id).is_some_and(|s| {
                use subtle::ConstantTimeEq;
                s.key_hash.len() == h.len() && bool::from(s.key_hash.as_bytes().ct_eq(h.as_bytes()))
            });
            if ok {
                return Some(k.to_string());
            }
        }
        None
    }

    /// セッションのイベントを処理する。
    pub fn handle_events(&mut self, key: PeerKey, app: &Arc<App>, feeds: &mut Feeds, tx: &mpsc::Sender<crate::quic::Msg>, now: Instant) {
        while let Some(e) = self.session.poll_event() {
            match e {
                SessionEvent::Subscribe { request_id, namespace, track, params } => {
                    self.on_subscribe(key, app, feeds, tx, request_id, &namespace, &track, &params, now)
                }
                SessionEvent::Fetch { request_id, namespace, track, params } => self.on_fetch(app, feeds, tx, request_id, &namespace, &track, &params),
                SessionEvent::TrackStatus { request_id, namespace, track, params } => {
                    let Some(id) = stream_of(&namespace) else {
                        self.session.request_error(request_id, RequestErrorCode::DOES_NOT_EXIST, "no such namespace");
                        continue;
                    };
                    if !self.may_play(app, &id, &params) {
                        self.session.request_error(request_id, RequestErrorCode::UNAUTHORIZED, "unauthorized");
                        continue;
                    }
                    let name = String::from_utf8_lossy(&track).to_string();
                    match feeds.ensure(app, tx, &id).filter(|f| f.knows(&name)) {
                        Some(f) => {
                            let p = Params { largest_object: f.largest(&name), ..Default::default() };
                            let _ = self.session.request_ok(request_id, p, Bytes::new());
                        }
                        None => self.session.request_error(request_id, RequestErrorCode::DOES_NOT_EXIST, "no such track"),
                    }
                }
                SessionEvent::SubscribeNamespace { request_id, prefix, params } => self.on_subscribe_namespace(app, request_id, &prefix, &params),
                SessionEvent::RequestCancelled { request_id } => {
                    if self.subs.remove(&request_id).is_some() {
                        feeds.unsubscribe(key, Some(request_id));
                    } else if let Some(ing) = self.ingest.as_mut() {
                        ing.on_cancel(request_id);
                    }
                }
                SessionEvent::RequestUpdate { request_id, .. } => {
                    let _ = self.session.request_ok(request_id, Params::default(), Bytes::new());
                }
                SessionEvent::PublishNamespace { request_id, prefix, params } => self.on_publish_namespace(app, request_id, &prefix, &params),
                SessionEvent::Publish { request_id, namespace, track, .. } => {
                    // 出版者起点の track は受けない（ノードから catalog を読んで購読する）
                    let _ = (namespace, track);
                    self.session.request_error(request_id, RequestErrorCode::NOT_SUPPORTED, "use PUBLISH_NAMESPACE; the node subscribes to catalog.json");
                }
                SessionEvent::SubscribeOk { .. } => {}
                SessionEvent::RequestError { request_id, code, reason, .. } => {
                    if let Some(ing) = self.ingest.as_mut()
                        && ing.is_catalog(request_id)
                    {
                        app.log(&format!("moq publish {}: catalog subscribe failed: {code:#x} {reason}", self.peer_addr));
                        self.session.close(SessionErrorCode::INTERNAL_ERROR, "catalog.json is required");
                    }
                }
                SessionEvent::Object { request_id, object, .. } => {
                    if let Some(ing) = self.ingest.as_mut() {
                        match ing.on_object(request_id, &object, &mut self.session) {
                            Ok(()) => {}
                            Err(why) => {
                                app.log(&format!("moq publish {}: {why}", self.peer_addr));
                                self.session.close(SessionErrorCode::INTERNAL_ERROR, &why);
                            }
                        }
                    }
                }
                SessionEvent::PublishDone { request_id, .. } => {
                    if let Some(ing) = self.ingest.as_mut()
                        && ing.is_catalog(request_id)
                    {
                        self.ingest = None;
                    }
                }
                SessionEvent::Closed { error } => {
                    if let Some(e) = error.filter(|e| e.code != SessionErrorCode::NO_ERROR) {
                        app.log(&format!("moq session {} closed: {e}", self.peer_addr));
                    }
                    self.ingest = None;
                    feeds.remove_peer(key);
                    self.subs.clear();
                }
                _ => {}
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_subscribe(
        &mut self,
        key: PeerKey,
        app: &Arc<App>,
        feeds: &mut Feeds,
        tx: &mpsc::Sender<crate::quic::Msg>,
        req: u64,
        ns: &Namespace,
        track: &[u8],
        params: &Params,
        now: Instant,
    ) {
        let Some(id) = stream_of(ns) else {
            self.session.request_error(req, RequestErrorCode::DOES_NOT_EXIST, "no such namespace");
            return;
        };
        if !self.may_play(app, &id, params) {
            self.session.request_error(req, RequestErrorCode::UNAUTHORIZED, "unauthorized");
            return;
        }
        let Ok(name) = std::str::from_utf8(track) else {
            self.session.request_error(req, RequestErrorCode::DOES_NOT_EXIST, "no such track");
            return;
        };
        let plausible = name == CATALOG_TRACK
            || ((name.starts_with("video") || name.starts_with("audio")) && name.len() <= 8 && name[5..].bytes().all(|c| c.is_ascii_digit()));
        if !plausible {
            self.session.request_error(req, RequestErrorCode::DOES_NOT_EXIST, "no such track");
            return;
        }
        let Some(f) = feeds.ensure(app, tx, &id) else {
            self.session.request_error(req, RequestErrorCode::DOES_NOT_EXIST, "stream is not live");
            return;
        };
        // catalog が分かっていれば、無い track は断る（分かる前は待つ）
        if f.catalog.is_some() && !f.knows(name) {
            self.session.request_error(req, RequestErrorCode::DOES_NOT_EXIST, "no such track");
            return;
        }
        // 視聴者の数（limits.max_viewers）: このセッションの 2 本目以降の購読は同じ 1 人
        let viewer = match f.subs.iter().find(|s| s.peer == key).map(|s| s.viewer.clone()) {
            Some(v) => v,
            None => match app.moq_viewer(&id) {
                Some(g) => Arc::new(g),
                None => {
                    self.session.request_error(req, RequestErrorCode::EXCESSIVE_LOAD, "too many viewers");
                    return;
                }
            },
        };
        let largest = f.largest(name);
        if self.session.subscribe_ok(req, largest, Bytes::new()).is_err() {
            return;
        }
        // 始まり: FILL_PARAMETERS（draft-22 3.5 の current-group join）があれば、購読は Location Filter
        // どおりに始め、その前の object（今の group の頭）を fill の fetch stream で渡す。fill が無く、指定が無い・
        // 次の object からなら、今の group の先頭から（復号できるところから渡す）。
        let start = match (&params.fill, params.location_filter) {
            (Some(_), lf) => filter_start(lf.as_ref(), largest),
            (None, None | Some(LocationFilter::None) | Some(LocationFilter::NextObject)) => Location::new(f.current_group(name).unwrap_or(0), 0),
            (None, Some(lf)) => filter_start(Some(&lf), largest),
        };
        let end = filter_end(params.location_filter.as_ref());
        if let (Some(fp), Some(l)) = (params.fill.as_deref(), largest) {
            let lf = fp.location_filter.or(params.location_filter);
            let fstart = match lf {
                None | Some(LocationFilter::None) => Some(Location::new(0, 0)),
                Some(LocationFilter::NextObject) => None,
                Some(LocationFilter::RelativeStart(n)) => Some(Location::new((l.group + 1).saturating_sub(n), 0)),
                Some(LocationFilter::AbsoluteStart(s)) | Some(LocationFilter::AbsoluteRange { start: s, .. }) => Some(s),
            };
            let fend = filter_end(lf.as_ref()).map_or(l, |e| e.min(l));
            let head = fstart.map(|s| f.objects(name, s, fend)).unwrap_or_default();
            // 空の範囲には stream を開かない
            if !head.is_empty() {
                for o in &head {
                    self.session.fill_object(req, o);
                }
                self.session.fill_finish(req);
            }
        }
        let backlog = f.objects(name, start, end.unwrap_or(Location::new(u64::MAX, u64::MAX)));
        let cur = f.current_group(name);
        let mut last = None;
        for o in &backlog {
            if let Some(g) = last
                && g != o.group
            {
                self.session.end_subgroup(req, g, 0, now);
            }
            last = Some(o.group);
            let _ = self.session.send_object(req, o, now, now);
        }
        if let Some(g) = last
            && (name == CATALOG_TRACK || Some(g) != cur)
        {
            self.session.end_subgroup(req, g, 0, now);
        }
        let track: Arc<str> = name.into();
        f.idle_since = None;
        f.subs.push(SubRef { peer: key, req, track, start, end, viewer });
        self.subs.insert(req, id);
    }

    #[allow(clippy::too_many_arguments)]
    fn on_fetch(&mut self, app: &Arc<App>, feeds: &mut Feeds, tx: &mpsc::Sender<crate::quic::Msg>, req: u64, ns: &Namespace, track: &[u8], params: &Params) {
        let Some(id) = stream_of(ns) else {
            self.session.request_error(req, RequestErrorCode::DOES_NOT_EXIST, "no such namespace");
            return;
        };
        if !self.may_play(app, &id, params) {
            self.session.request_error(req, RequestErrorCode::UNAUTHORIZED, "unauthorized");
            return;
        }
        let name = String::from_utf8_lossy(track).to_string();
        let Some(f) = feeds.ensure(app, tx, &id) else {
            self.session.request_error(req, RequestErrorCode::DOES_NOT_EXIST, "stream is not live");
            return;
        };
        let largest = f.largest(&name);
        let start = match params.location_filter {
            Some(LocationFilter::AbsoluteStart(s)) | Some(LocationFilter::AbsoluteRange { start: s, .. }) => s,
            Some(LocationFilter::RelativeStart(n)) => Location::new(largest.map_or(0, |l| l.group + 1).saturating_sub(n), 0),
            _ => Location::new(largest.map_or(0, |l| l.group), 0),
        };
        let end = filter_end(params.location_filter.as_ref()).unwrap_or(Location::new(u64::MAX, u64::MAX));
        let list = f.objects(&name, start, end);
        let Some(last) = list.last().map(|o| o.location()) else {
            self.session.request_error(req, RequestErrorCode::INVALID_RANGE, "not in the live window");
            return;
        };
        if self.session.fetch_ok(req, false, last, Bytes::new()).is_err() {
            return;
        }
        for o in &list {
            self.session.fetch_object(req, o);
        }
        self.session.fetch_finish(req);
    }

    fn on_subscribe_namespace(&mut self, app: &Arc<App>, req: u64, prefix: &Namespace, params: &Params) {
        // 署名が通る stream だけを知らせる（他の stream の有無は明かさない）
        let ids: Vec<String> = app.control.lock().unwrap().state.streams.keys().cloned().collect();
        let mut found = Vec::new();
        for id in ids {
            let ns = namespace_of(&id);
            if let Some(suffix) = ns.suffix_after(prefix)
                && self.may_play(app, &id, params)
            {
                found.push(suffix);
            }
        }
        if found.is_empty() {
            self.session.request_error(req, RequestErrorCode::UNAUTHORIZED, "unauthorized");
            return;
        }
        if self.session.request_ok(req, Params::default(), Bytes::new()).is_err() {
            return;
        }
        for s in found {
            self.session.namespace(req, s, false);
        }
    }

    fn on_publish_namespace(&mut self, app: &Arc<App>, req: u64, prefix: &Namespace, params: &Params) {
        let Some(id) = stream_of(prefix) else {
            self.session.request_error(req, RequestErrorCode::UNAUTHORIZED, "publish to senca/live/{stream_id}");
            return;
        };
        if self.ingest.is_some() {
            self.session.request_error(req, RequestErrorCode::EXCESSIVE_LOAD, "one publication per session");
            return;
        }
        let Some(key) = self.publish_stream(app, &id, params) else {
            self.session.request_error(req, RequestErrorCode::UNAUTHORIZED, "unauthorized");
            return;
        };
        if self.session.request_ok(req, Params::default(), Bytes::new()).is_err() {
            return;
        }
        let catalog_req = match self.session.subscribe(SubscribeArgs {
            namespace: prefix.clone(),
            track: Bytes::from_static(CATALOG_TRACK.as_bytes()),
            params: Params { location_filter: Some(LocationFilter::RelativeStart(1)), ..Default::default() },
        }) {
            Ok(r) => r,
            Err(e) => {
                self.session.close(SessionErrorCode::INTERNAL_ERROR, &e.to_string());
                return;
            }
        };
        let (itx, irx) = mpsc::channel(1024);
        let app2 = app.clone();
        let peer = self.peer_addr.clone();
        tokio::spawn(ingest_task(app2, key, peer, irx));
        app.log(&format!("moq publish {} -> stream {id}", self.peer_addr));
        self.ingest = Some(Ingest { namespace: prefix.clone(), catalog_req, tracks: HashMap::new(), tx: itx, seen_catalog: None, next_track: 1 });
    }
}

// ------------------------------------------------------------------ 送出（MoQ → hub）

enum IngestMsg {
    Config(TrackId, CodecConfig),
    Au(AccessUnit),
}

struct IngestTrack {
    track: TrackId,
    config: CodecConfig,
    timescale: u32,
    reader: senca_fmp4::Mp4Reader,
    sequence: u64,
    /// hub が詰まって落とした後は、次のキーフレームまで落とし、不連続の印を付ける。
    dropping: bool,
}

struct Ingest {
    namespace: Namespace,
    catalog_req: u64,
    /// 購読の要求 → track。
    tracks: HashMap<u64, IngestTrack>,
    tx: mpsc::Sender<IngestMsg>,
    seen_catalog: Option<Bytes>,
    next_track: u32,
}

impl Ingest {
    fn is_catalog(&self, req: u64) -> bool {
        req == self.catalog_req
    }

    fn on_cancel(&mut self, req: u64) {
        self.tracks.remove(&req);
    }

    fn on_object(&mut self, req: u64, o: &Object, session: &mut Session) -> Result<(), String> {
        if o.payload.len() > MAX_INGEST_OBJECT {
            return Err("object too large".into());
        }
        if req == self.catalog_req {
            if self.seen_catalog.as_ref() == Some(&o.payload) || o.payload.is_empty() {
                return Ok(());
            }
            return self.on_catalog(o.payload.clone(), session);
        }
        let Some(t) = self.tracks.get_mut(&req) else { return Ok(()) };
        if o.payload.is_empty() {
            return Ok(());
        }
        let samples = t.reader.read_fragments(&o.payload).map_err(|e| format!("CMAF fragment: {e:?}"))?;
        for s in samples {
            let video = t.config.codec.kind() == MediaKind::Video;
            if t.dropping && video && !s.keyframe {
                continue;
            }
            let discontinuity = std::mem::take(&mut t.dropping);
            t.sequence += 1;
            let au = AccessUnit {
                track: t.track,
                codec: t.config.codec,
                epoch: 0,
                sequence: t.sequence,
                config_generation: t.config.generation,
                pts: s.dts + i64::from(s.cts_offset),
                dts: s.dts,
                duration: u64::from(s.duration),
                timebase: Timebase::new(1, t.timescale),
                keyframe: s.keyframe,
                discontinuity,
                capture_time_ns: None,
                payload: s.data,
            };
            match self.tx.try_send(IngestMsg::Au(au)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => t.dropping = true,
                Err(mpsc::error::TrySendError::Closed(_)) => return Err("publish rejected by the stream".into()),
            }
        }
        Ok(())
    }

    fn on_catalog(&mut self, json: Bytes, session: &mut Session) -> Result<(), String> {
        if self.seen_catalog.is_some() {
            // 途中の catalog の変更（track の増減）には追わない
            return Ok(());
        }
        let entries = parse_catalog(&json)?;
        if entries.is_empty() || entries.len() > MAX_INGEST_TRACKS {
            return Err("catalog has no usable tracks".into());
        }
        self.seen_catalog = Some(json);
        for e in entries {
            if e.container != "cmaf" {
                return Err(format!("track {}: container {} is not supported (cmaf only)", e.track, e.container));
            }
            let init = e.init.ok_or_else(|| format!("track {}: cmaf without init", e.track))?;
            let seg = senca_fmp4::parse_init(&init).map_err(|err| format!("track {}: init: {err:?}", e.track))?;
            let Some(mt) = seg.tracks.first() else { return Err(format!("track {}: init has no track", e.track)) };
            let Some(mut config) = mt.config.clone() else { return Err(format!("track {}: unknown sample entry", e.track)) };
            crate::media::enrich(&mut config);
            let timescale = mt.timescale;
            let track = TrackId(self.next_track);
            self.next_track += 1;
            if self.tx.try_send(IngestMsg::Config(track, config.clone())).is_err() {
                return Err("publish rejected by the stream".into());
            }
            let req = session
                .subscribe(SubscribeArgs {
                    namespace: self.namespace.clone(),
                    track: Bytes::from(e.track.clone()),
                    params: Params { location_filter: Some(LocationFilter::RelativeStart(1)), ..Default::default() },
                })
                .map_err(|err| err.to_string())?;
            self.tracks.insert(req, IngestTrack { track, config, timescale, reader: senca_fmp4::Mp4Reader::new(seg), sequence: 0, dropping: false });
        }
        Ok(())
    }
}

/// 送出の送り手（hub の Publisher）。セッションが終わる（送信側が閉じる）と切る。
async fn ingest_task(app: Arc<App>, key: String, peer: String, mut rx: mpsc::Receiver<IngestMsg>) {
    let publisher = match app.attach_publisher(&key, "moq", &peer).await {
        Ok(p) => p,
        Err(e) => {
            app.log(&format!("moq publish {peer} rejected: {e:?}"));
            return;
        }
    };
    app.log(&format!("moq publish {peer} attached to stream {}", publisher.stream_id()));
    while let Some(m) = rx.recv().await {
        let r = match m {
            IngestMsg::Config(t, c) => publisher.config(t, c).await,
            IngestMsg::Au(au) => publisher.access_unit(au).await,
        };
        if let Err(e) = r {
            app.log(&format!("moq publish {peer}: {e}"));
            break;
        }
    }
    publisher.detach("moq session closed").await;
}

/// 再生の URL（WebTransport、query に署名）。`--quic` が無ければ `None`。
pub fn playback_url(app: &App, exp: i64, sig: &str) -> Option<String> {
    let q = app.cfg.quic_listen?;
    // 公開の HTTP の URL の host を使う（無ければ待ち受けの番地）
    let host = app
        .cfg
        .public
        .http
        .as_deref()
        .and_then(|u| u.split_once("://").map(|(_, r)| r))
        .map(|r| r.split(['/', '?']).next().unwrap_or(r))
        .map(|hp| match hp.rsplit_once(':') {
            Some((h, p)) if p.bytes().all(|c| c.is_ascii_digit()) => h.to_string(),
            _ => hp.to_string(),
        })
        .unwrap_or_else(|| q.ip().to_string());
    Some(format!("https://{host}:{}{}?exp={exp}&sig={sig}", q.port(), crate::quic::WT_PATH))
}

#[cfg(test)]
mod tests {
    use super::*;
    use senca_core::Codec;
    use senca_core::media::VideoParams;

    #[test]
    fn namespace_round_trip_and_rejects_others() {
        assert_eq!(stream_of(&namespace_of("str_01ABC")).as_deref(), Some("str_01ABC"));
        assert_eq!(stream_of(&Namespace::from_path("senca/live")), None);
        assert_eq!(stream_of(&Namespace::from_path("senca/vod/x")), None);
        assert_eq!(stream_of(&Namespace::from_path("senca/live/x/y")), None);
        assert_eq!(stream_of(&Namespace::from_path("senca/live/a.b")), None);
    }

    #[test]
    fn exp_sig_from_token_path_and_query() {
        assert_eq!(exp_sig("exp=10&sig=abc"), Some((10, "abc".into())));
        assert_eq!(exp_sig("/moq?exp=10&sig=abc"), Some((10, "abc".into())));
        assert_eq!(exp_sig("?sig=abc&exp=11"), Some((11, "abc".into())));
        assert_eq!(exp_sig("exp=x&sig=abc"), None);
        assert_eq!(exp_sig("sig=abc"), None);
    }

    fn h264_config() -> CodecConfig {
        // 最小の avcC（SPS・PPS は空でよい: 試験は形だけ）
        CodecConfig {
            codec: Codec::H264,
            generation: 0,
            record: Bytes::from_static(&[1, 0x64, 0, 0x1f, 0xff, 0xe0, 0]),
            video: Some(VideoParams { width: 640, height: 360, profile: 0x64, compat: 0, level: 0x1f }),
            audio: None,
        }
    }

    fn au(dts: i64, key: bool) -> AccessUnit {
        AccessUnit {
            track: TrackId(0),
            codec: Codec::H264,
            epoch: 0,
            sequence: 0,
            config_generation: 0,
            pts: dts,
            dts,
            duration: 0,
            timebase: Timebase::MILLIS,
            keyframe: key,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from_static(&[0, 0, 0, 1, 0x65]),
        }
    }

    #[test]
    fn cmaf_groups_follow_gops() {
        let mut m = CmafMux::new();
        let mut out = Vec::new();
        m.on_event(&HubEvent::Config(TrackId(0), h264_config()), &mut out);
        // キーフレームの前の AU は落とす
        m.on_event(&HubEvent::Au(au(0, false)), &mut out);
        for i in 1..=7 {
            m.on_event(&HubEvent::Au(au(i * 33, i == 1 || i == 4)), &mut out);
        }
        let FeedEvent::Catalog { group: 0, json } = &out[0] else { panic!("catalog first: {out:?}") };
        let cat = parse_catalog(json).unwrap();
        assert_eq!(cat[0].track, "video");
        assert_eq!(cat[0].container, "cmaf");
        let objs: Vec<(u64, u64)> = out
            .iter()
            .filter_map(|e| match e {
                FeedEvent::Object { object, .. } => Some((object.group, object.id)),
                _ => None,
            })
            .collect();
        // AU 1〜6 が出る（7 は次の AU を待つ）。group 0 = 1,2,3、group 1 = 4,5,6
        assert_eq!(objs, vec![(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2)]);
        // 不連続の後は次のキーフレームまで落とし、group の番号は続ける
        out.clear();
        m.on_event(&HubEvent::Discontinuity, &mut out);
        assert_eq!(out.len(), 1, "残りの AU 7 を出す");
        out.clear();
        m.on_event(&HubEvent::Au(au(0, false)), &mut out);
        m.on_event(&HubEvent::Au(au(33, true)), &mut out);
        m.on_event(&HubEvent::Au(au(66, false)), &mut out);
        let FeedEvent::Object { object, .. } = &out[0] else { panic!() };
        assert_eq!((object.group, object.id), (2, 0));
        // fragment はそのまま読める
        let FeedEvent::Object { object, .. } = &out[0] else { panic!() };
        assert_eq!(&object.payload[4..8], b"moof");
    }
}
