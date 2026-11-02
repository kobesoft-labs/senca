//! MoQ の relay（draft-22 7 章）。sans-io。
//!
//! 複数の [`Session`] を持ち、namespace の公開・購読、track の購読、object の中継、fetch を扱う。
//! object は中身を読まずに転送し、[`TrackCache`] に不変に保存する（同じ ID で違う中身は拒否）。
//! 要求ごとに [`Authorizer`] で publish・subscribe の権限を確かめる。
//!
//! 使い方: 受信を各セッションへ渡し（[`crate::transport::feed_quic`] など）、[`Relay::process`] を
//! 呼び、各セッションを flush する。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::auth::{Action, AuthContext, Authorizer};
use crate::cache::{Insert, TrackCache};
use crate::error::{RequestErrorCode, publish_done};
use crate::params::{GroupOrder, LocationFilter, Params, TrackProperties};
use crate::session::{Delivery, Object, Session, SessionConfig, SessionEvent, SubscribeArgs, filter_end, filter_start};
use crate::{FullTrackName, Location, Namespace, RangeEnd};

/// relay の中のセッションの番号。
pub type SessionId = u64;

/// relay の設定と上限。
#[derive(Clone)]
pub struct RelayConfig {
    /// cache に置く時間の既定（MAX_CACHE_DURATION が短ければそちら）。
    pub cache_max_age: Duration,
    /// track ごとの cache の上限。
    pub cache_max_bytes: usize,
    /// 同時に扱う track の上限。
    pub max_tracks: usize,
    pub authorizer: Arc<dyn Authorizer>,
}

impl std::fmt::Debug for RelayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayConfig")
            .field("cache_max_age", &self.cache_max_age)
            .field("cache_max_bytes", &self.cache_max_bytes)
            .field("max_tracks", &self.max_tracks)
            .finish_non_exhaustive()
    }
}

impl RelayConfig {
    pub fn new(authorizer: Arc<dyn Authorizer>) -> Self {
        Self {
            cache_max_age: Duration::from_secs(30),
            cache_max_bytes: 64 << 20,
            max_tracks: 10_000,
            authorizer,
        }
    }
}

/// 統計。
#[derive(Debug, Clone, Default)]
pub struct RelayStats {
    pub objects_in: u64,
    pub objects_out: u64,
    pub duplicates: u64,
    pub cache_mismatches: u64,
    pub unauthorized: u64,
    pub fetches_from_cache: u64,
}

#[derive(Debug)]
struct Downstream {
    session: SessionId,
    req: u64,
    start: Location,
    end: Option<Location>,
}

#[derive(Debug)]
struct Track {
    upstream: Option<(SessionId, u64)>,
    upstream_ready: bool,
    properties: Bytes,
    subscribers: Vec<Downstream>,
    /// 上流の応答を待つ購読。
    pending: Vec<(SessionId, u64, Params)>,
    cache: TrackCache,
    ended: bool,
}

#[derive(Debug)]
struct Announce {
    prefix: Namespace,
    session: SessionId,
    req: u64,
}

#[derive(Debug)]
struct NsSub {
    prefix: Namespace,
    session: SessionId,
    req: u64,
}

/// 上流への fetch の中継。
#[derive(Debug)]
struct FetchRelay {
    down: (SessionId, u64),
    /// 下流へ FETCH_OK を送った。
    ok_sent: bool,
    /// 上流のデータ stream が終わった（FETCH_OK を送ってから閉じる）。
    complete: bool,
}

/// MoQ の relay。
pub struct Relay {
    cfg: RelayConfig,
    sessions: HashMap<SessionId, Session>,
    next_sid: SessionId,
    announces: Vec<Announce>,
    ns_subs: Vec<NsSub>,
    tracks: HashMap<FullTrackName, Track>,
    upstream: HashMap<(SessionId, u64), FullTrackName>,
    downstream: HashMap<(SessionId, u64), FullTrackName>,
    fetch_up: HashMap<(SessionId, u64), FetchRelay>,
    /// 上流へ中継した TRACK_STATUS → 下流の要求。
    status_up: HashMap<(SessionId, u64), (SessionId, u64)>,
    stats: RelayStats,
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relay")
            .field("sessions", &self.sessions.len())
            .field("tracks", &self.tracks.len())
            .finish_non_exhaustive()
    }
}

impl Relay {
    pub fn new(cfg: RelayConfig) -> Self {
        Self {
            cfg,
            sessions: HashMap::new(),
            next_sid: 1,
            announces: Vec::new(),
            ns_subs: Vec::new(),
            tracks: HashMap::new(),
            upstream: HashMap::new(),
            downstream: HashMap::new(),
            fetch_up: HashMap::new(),
            status_up: HashMap::new(),
            stats: RelayStats::default(),
        }
    }

    pub fn stats(&self) -> &RelayStats {
        &self.stats
    }

    /// 新しいセッション（server 側）を加える。
    pub fn add_session(&mut self, cfg: SessionConfig, now: Instant) -> SessionId {
        let id = self.next_sid;
        self.next_sid += 1;
        self.sessions.insert(id, Session::new(cfg, now));
        id
    }

    pub fn session_mut(&mut self, id: SessionId) -> Option<&mut Session> {
        self.sessions.get_mut(&id)
    }

    pub fn sessions(&self) -> impl Iterator<Item = SessionId> + '_ {
        self.sessions.keys().copied()
    }

    /// track の cache（試験・監視用）。
    pub fn cache(&self, name: &FullTrackName) -> Option<&TrackCache> {
        self.tracks.get(name).map(|t| &t.cache)
    }

    /// セッションを取り除く（接続が終わった）。
    pub fn remove_session(&mut self, sid: SessionId) {
        self.sessions.remove(&sid);
        self.drop_session_state(sid);
    }

    fn auth(&mut self, sid: SessionId, action: Action, ns: &Namespace, track: Option<&[u8]>, params: &Params) -> bool {
        let Some(s) = self.sessions.get(&sid) else { return false };
        let setup = s.peer_setup();
        let empty = Vec::new();
        let ctx = AuthContext {
            setup_tokens: setup.map_or(&empty, |p| &p.auth_tokens),
            request_tokens: &params.auth_tokens,
            path: setup.and_then(|p| p.path.as_deref()),
        };
        let ok = !params.unknown_token_alias && self.cfg.authorizer.authorize(&ctx, action, ns, track);
        if !ok {
            self.stats.unauthorized += 1;
        }
        ok
    }

    /// 全セッションのイベントを処理する。
    pub fn process(&mut self, now: Instant) {
        // イベントを集める（セッションをまたいで送るので、先に取り出す）
        loop {
            let mut evs: Vec<(SessionId, SessionEvent)> = Vec::new();
            for (sid, s) in self.sessions.iter_mut() {
                while let Some(e) = s.poll_event() {
                    evs.push((*sid, e));
                }
            }
            if evs.is_empty() {
                break;
            }
            for (sid, e) in evs {
                self.on_event(sid, e, now);
            }
        }
        // 閉じたセッションを片付ける
        let closed: Vec<SessionId> = self.sessions.iter().filter(|(_, s)| s.is_closed()).map(|(id, _)| *id).collect();
        for sid in closed {
            self.remove_session(sid);
        }
        for t in self.tracks.values_mut() {
            t.cache.evict(now);
        }
        // 購読者も上流も無い track の cache を捨てる（期限切れ）
        self.tracks
            .retain(|_, t| t.upstream.is_some() || !t.subscribers.is_empty() || !t.cache.is_empty());
    }

    fn s(&mut self, sid: SessionId) -> Option<&mut Session> {
        self.sessions.get_mut(&sid)
    }

    fn on_event(&mut self, sid: SessionId, e: SessionEvent, now: Instant) {
        match e {
            SessionEvent::Ready { .. } => {}
            SessionEvent::PublishNamespace { request_id, prefix, params } => {
                if !self.auth(sid, Action::Publish, &prefix, None, &params) {
                    if let Some(s) = self.s(sid) {
                        s.request_error(request_id, RequestErrorCode::UNAUTHORIZED, "unauthorized");
                    }
                    return;
                }
                if let Some(s) = self.s(sid) {
                    let _ = s.request_ok(request_id, Params::default(), Bytes::new());
                }
                self.announces.push(Announce {
                    prefix: prefix.clone(),
                    session: sid,
                    req: request_id,
                });
                self.notify_namespace(&prefix, false);
            }
            SessionEvent::SubscribeNamespace { request_id, prefix, params } => {
                if !self.auth(sid, Action::Discover, &prefix, None, &params) {
                    if let Some(s) = self.s(sid) {
                        s.request_error(request_id, RequestErrorCode::UNAUTHORIZED, "unauthorized");
                    }
                    return;
                }
                if let Some(s) = self.s(sid) {
                    let _ = s.request_ok(request_id, Params::default(), Bytes::new());
                }
                let matching: Vec<Namespace> = self
                    .announces
                    .iter()
                    .filter_map(|a| a.prefix.suffix_after(&prefix))
                    .collect();
                if let Some(s) = self.s(sid) {
                    for suffix in matching {
                        s.namespace(request_id, suffix, false);
                    }
                }
                self.ns_subs.push(NsSub {
                    prefix,
                    session: sid,
                    req: request_id,
                });
            }
            SessionEvent::RequestCancelled { request_id } => self.on_cancel(sid, request_id),
            SessionEvent::Subscribe {
                request_id,
                namespace,
                track,
                params,
            } => self.on_subscribe(sid, request_id, namespace, track, params, now),
            SessionEvent::Publish {
                request_id,
                namespace,
                track,
                params,
                properties,
                ..
            } => {
                if !self.auth(sid, Action::Publish, &namespace, Some(&track), &params) {
                    if let Some(s) = self.s(sid) {
                        s.request_error(request_id, RequestErrorCode::UNAUTHORIZED, "unauthorized");
                    }
                    return;
                }
                let name = FullTrackName::new(namespace, track);
                if !self.tracks.contains_key(&name) && self.tracks.len() >= self.cfg.max_tracks {
                    if let Some(s) = self.s(sid) {
                        s.request_error(request_id, RequestErrorCode::EXCESSIVE_LOAD, "too many tracks");
                    }
                    return;
                }
                if let Some(s) = self.s(sid) {
                    let _ = s.request_ok(request_id, Params::default(), Bytes::new());
                }
                let t = self.track_entry(&name);
                t.upstream = Some((sid, request_id));
                t.upstream_ready = true;
                t.ended = false;
                if let Ok(tp) = TrackProperties::parse(properties.clone())
                    && let Some(ms) = tp.max_cache_duration
                {
                    t.cache.limit_age(ms);
                }
                t.properties = properties;
                if let Some(l) = params.largest_object {
                    t.cache.note_largest(l);
                }
                self.upstream.insert((sid, request_id), name.clone());
                self.release_pending(&name, now);
            }
            SessionEvent::SubscribeOk {
                request_id,
                params,
                properties,
                ..
            } => {
                let Some(name) = self.upstream.get(&(sid, request_id)).cloned() else { return };
                let Some(t) = self.tracks.get_mut(&name) else { return };
                t.upstream_ready = true;
                if let Some(l) = params.largest_object {
                    t.cache.note_largest(l);
                }
                if let Ok(tp) = TrackProperties::parse(properties.clone())
                    && let Some(ms) = tp.max_cache_duration
                {
                    t.cache.limit_age(ms);
                }
                t.properties = properties;
                self.release_pending(&name, now);
            }
            SessionEvent::RequestError {
                request_id,
                code,
                reason,
                ..
            } => {
                if let Some((ds, dr)) = self.status_up.remove(&(sid, request_id)) {
                    if let Some(s) = self.s(ds) {
                        s.request_error(dr, RequestErrorCode(code), &reason);
                    }
                    return;
                }
                if let Some(f) = self.fetch_up.remove(&(sid, request_id)) {
                    let (ds, dr) = f.down;
                    if let Some(s) = self.s(ds) {
                        s.request_error(dr, RequestErrorCode(code), &reason);
                    }
                    return;
                }
                let Some(name) = self.upstream.remove(&(sid, request_id)) else { return };
                let Some(t) = self.tracks.get_mut(&name) else { return };
                t.upstream = None;
                t.upstream_ready = false;
                let pend = std::mem::take(&mut t.pending);
                for (ds, dr, _) in pend {
                    self.downstream.remove(&(ds, dr));
                    if let Some(s) = self.s(ds) {
                        s.request_error(dr, RequestErrorCode(code), &reason);
                    }
                }
            }
            SessionEvent::Object {
                request_id,
                object,
                delivery,
                received,
            } => {
                if let Some(f) = self.fetch_up.get(&(sid, request_id)) {
                    let (ds, dr) = f.down;
                    if let Some(s) = self.s(ds) {
                        s.fetch_object(dr, &object);
                    }
                    return;
                }
                self.on_object(sid, request_id, object, delivery, received, now);
            }
            SessionEvent::SubgroupEnd {
                request_id,
                group,
                subgroup,
            } => {
                let Some(name) = self.upstream.get(&(sid, request_id)).cloned() else { return };
                let subs: Vec<(SessionId, u64)> = self.tracks.get(&name).map_or(Vec::new(), |t| t.subscribers.iter().map(|d| (d.session, d.req)).collect());
                for (ds, dr) in subs {
                    if let Some(s) = self.s(ds) {
                        s.end_subgroup(dr, group, subgroup, now);
                    }
                }
            }
            SessionEvent::SubgroupReset {
                request_id,
                group,
                subgroup,
                code,
            } => {
                let Some(name) = self.upstream.get(&(sid, request_id)).cloned() else { return };
                let subs: Vec<(SessionId, u64)> = self.tracks.get(&name).map_or(Vec::new(), |t| t.subscribers.iter().map(|d| (d.session, d.req)).collect());
                for (ds, dr) in subs {
                    if let Some(s) = self.s(ds) {
                        s.reset_subgroup(dr, group, subgroup, code);
                    }
                }
            }
            SessionEvent::PublishDone { request_id, status, reason, .. } => {
                let Some(name) = self.upstream.remove(&(sid, request_id)) else { return };
                self.end_track(&name, status, &reason);
            }
            SessionEvent::Fetch {
                request_id,
                namespace,
                track,
                params,
            } => self.on_fetch(sid, request_id, namespace, track, params),
            SessionEvent::FetchOk {
                request_id,
                end_of_track,
                end,
                properties,
            } => {
                let Some(f) = self.fetch_up.get_mut(&(sid, request_id)) else { return };
                f.ok_sent = true;
                let (ds, dr) = f.down;
                let complete = f.complete;
                if complete {
                    self.fetch_up.remove(&(sid, request_id));
                }
                if let Some(s) = self.s(ds) {
                    let _ = s.fetch_ok(dr, end_of_track, end, properties);
                    if complete {
                        s.fetch_finish(dr);
                    }
                }
            }
            SessionEvent::FetchRangeEnd { request_id, kind, end } => {
                if let Some(f) = self.fetch_up.get(&(sid, request_id)) {
                    let (ds, dr) = f.down;
                    if let Some(s) = self.s(ds) {
                        s.fetch_range_end(dr, kind, end);
                    }
                }
            }
            SessionEvent::FetchComplete { request_id } => {
                // FETCH_OK より先にデータ stream が終わることがある: FETCH_OK を送ってから閉じる（9.12）
                let Some(f) = self.fetch_up.get_mut(&(sid, request_id)) else { return };
                if !f.ok_sent {
                    f.complete = true;
                    return;
                }
                let (ds, dr) = f.down;
                self.fetch_up.remove(&(sid, request_id));
                if let Some(s) = self.s(ds) {
                    s.fetch_finish(dr);
                }
            }
            SessionEvent::TrackStatus {
                request_id,
                namespace,
                track,
                params,
            } => {
                if !self.auth(sid, Action::Subscribe, &namespace, Some(&track), &params) {
                    if let Some(s) = self.s(sid) {
                        s.request_error(request_id, RequestErrorCode::UNAUTHORIZED, "unauthorized");
                    }
                    return;
                }
                let name = FullTrackName::new(namespace.clone(), track.clone());
                let info = self
                    .tracks
                    .get(&name)
                    .filter(|t| t.upstream_ready || !t.cache.is_empty())
                    .map(|t| (t.cache.largest(), t.properties.clone()));
                if let Some((largest, props)) = info {
                    if let Some(s) = self.s(sid) {
                        let _ = s.request_ok(
                            request_id,
                            Params {
                                largest_object: largest,
                                ..Default::default()
                            },
                            props,
                        );
                    }
                    return;
                }
                // 知らない track: 出版者へ中継する（9.13）
                let up = self
                    .announces
                    .iter()
                    .filter(|a| namespace.starts_with(&a.prefix) && a.session != sid)
                    .max_by_key(|a| a.prefix.0.len())
                    .map(|a| a.session);
                match up.and_then(|u| self.s(u).and_then(|s| s.track_status(namespace, track, Params::default()).ok()).map(|r| (u, r))) {
                    Some((u, r)) => {
                        self.status_up.insert((u, r), (sid, request_id));
                    }
                    None => {
                        if let Some(s) = self.s(sid) {
                            s.request_error(request_id, RequestErrorCode::DOES_NOT_EXIST, "no such track");
                        }
                    }
                }
            }
            SessionEvent::RequestUpdate { request_id, .. } => {
                // 購読者の優先度・転送の切り替えはセッションが反映する
                if let Some(s) = self.s(sid) {
                    let _ = s.request_ok(request_id, Params::default(), Bytes::new());
                }
            }
            SessionEvent::Closed { .. } => self.drop_session_state(sid),
            SessionEvent::RequestOk {
                request_id,
                params,
                properties,
            } => {
                if let Some((ds, dr)) = self.status_up.remove(&(sid, request_id))
                    && let Some(s) = self.s(ds)
                {
                    let p = Params {
                        largest_object: params.largest_object,
                        ..Default::default()
                    };
                    let _ = s.request_ok(dr, p, properties);
                }
            }
            SessionEvent::PublishStateNotify { .. }
            | SessionEvent::Namespace { .. }
            | SessionEvent::NamespaceDone { .. }
            | SessionEvent::GoAway { .. } => {}
        }
    }

    fn track_entry(&mut self, name: &FullTrackName) -> &mut Track {
        let (bytes, age) = (self.cfg.cache_max_bytes, self.cfg.cache_max_age);
        self.tracks.entry(name.clone()).or_insert_with(|| Track {
            upstream: None,
            upstream_ready: false,
            properties: Bytes::new(),
            subscribers: Vec::new(),
            pending: Vec::new(),
            cache: TrackCache::new(bytes, age),
            ended: false,
        })
    }

    fn notify_namespace(&mut self, ns: &Namespace, done: bool) {
        let targets: Vec<(SessionId, u64, Namespace)> = self
            .ns_subs
            .iter()
            .filter_map(|s| ns.suffix_after(&s.prefix).map(|suf| (s.session, s.req, suf)))
            .collect();
        for (sid, req, suf) in targets {
            if let Some(s) = self.s(sid) {
                s.namespace(req, suf, done);
            }
        }
    }

    fn on_subscribe(&mut self, sid: SessionId, req: u64, ns: Namespace, track: Bytes, params: Params, now: Instant) {
        if !self.auth(sid, Action::Subscribe, &ns, Some(&track), &params) {
            if let Some(s) = self.s(sid) {
                s.request_error(req, RequestErrorCode::UNAUTHORIZED, "unauthorized");
            }
            return;
        }
        let name = FullTrackName::new(ns.clone(), track.clone());
        if !self.tracks.contains_key(&name) && self.tracks.len() >= self.cfg.max_tracks {
            if let Some(s) = self.s(sid) {
                s.request_error(req, RequestErrorCode::EXCESSIVE_LOAD, "too many tracks");
            }
            return;
        }
        let has_upstream = self.tracks.get(&name).is_some_and(|t| t.upstream.is_some());
        self.downstream.insert((sid, req), name.clone());
        if has_upstream {
            let ready = self.tracks.get(&name).is_some_and(|t| t.upstream_ready);
            if ready {
                self.attach(&name, sid, req, &params, now);
            } else if let Some(t) = self.tracks.get_mut(&name) {
                t.pending.push((sid, req, params));
            }
            return;
        }
        // 上流を探す（要素単位の最長一致）
        let up = self
            .announces
            .iter()
            .filter(|a| ns.starts_with(&a.prefix) && a.session != sid)
            .max_by_key(|a| a.prefix.0.len())
            .map(|a| a.session);
        let Some(up) = up else {
            self.downstream.remove(&(sid, req));
            if let Some(s) = self.s(sid) {
                s.request_error(req, RequestErrorCode::DOES_NOT_EXIST, "no publisher for namespace");
            }
            return;
        };
        let up_params = Params {
            // 今の group の先頭から受ける（後から来た購読者へ cache から渡せるように）
            location_filter: Some(LocationFilter::RelativeStart(1)),
            subscriber_priority: Some(128),
            group_order: Some(GroupOrder::Ascending),
            forward: Some(true),
            ..Default::default()
        };
        let ureq = match self.s(up).map(|s| {
            s.subscribe(SubscribeArgs {
                namespace: ns,
                track,
                params: up_params,
            })
        }) {
            Some(Ok(r)) => r,
            _ => {
                self.downstream.remove(&(sid, req));
                if let Some(s) = self.s(sid) {
                    s.request_error(req, RequestErrorCode::INTERNAL_ERROR, "upstream unavailable");
                }
                return;
            }
        };
        self.upstream.insert((up, ureq), name.clone());
        let t = self.track_entry(&name);
        t.upstream = Some((up, ureq));
        t.upstream_ready = false;
        t.pending.push((sid, req, params));
    }

    fn release_pending(&mut self, name: &FullTrackName, now: Instant) {
        let Some(t) = self.tracks.get_mut(name) else { return };
        let pend = std::mem::take(&mut t.pending);
        for (sid, req, params) in pend {
            self.attach(name, sid, req, &params, now);
        }
    }

    /// 下流の購読を確立し、cache から追いつかせる。
    fn attach(&mut self, name: &FullTrackName, sid: SessionId, req: u64, params: &Params, now: Instant) {
        let Some(t) = self.tracks.get(name) else { return };
        let largest = t.cache.largest();
        let props = t.properties.clone();
        let start = filter_start(params.location_filter.as_ref(), largest);
        let end = filter_end(params.location_filter.as_ref());
        if let Some(e) = end
            && e < start
        {
            if let Some(s) = self.s(sid) {
                s.request_error(req, RequestErrorCode::INVALID_RANGE, "end before start");
            }
            self.downstream.remove(&(sid, req));
            return;
        }
        let backlog: Vec<(Object, Delivery, Instant)> = t
            .cache
            .range(start, end.unwrap_or(Location::new(u64::MAX, u64::MAX)))
            .map(|c| (c.object.clone(), c.delivery, c.received))
            .collect();
        let Some(s) = self.sessions.get_mut(&sid) else { return };
        if s.subscribe_ok(req, largest, props).is_err() {
            return;
        }
        // cache の分を先に送る（group の途中から始まる subgroup も stream を開いて送る）
        for (o, d, at) in backlog {
            if d == Delivery::Datagram {
                s.send_object_datagram(req, &o, at);
            } else {
                let _ = s.send_object(req, &o, at, now);
            }
            self.stats.objects_out += 1;
        }
        if let Some(t) = self.tracks.get_mut(name) {
            t.subscribers.push(Downstream {
                session: sid,
                req,
                start,
                end,
            });
        }
    }

    fn on_object(&mut self, sid: SessionId, req: u64, o: Object, delivery: Delivery, received: Instant, now: Instant) {
        let Some(name) = self.upstream.get(&(sid, req)).cloned() else { return };
        self.stats.objects_in += 1;
        let Some(t) = self.tracks.get_mut(&name) else { return };
        match t.cache.insert(&o, delivery, received) {
            Insert::New => {}
            Insert::Duplicate => {
                self.stats.duplicates += 1;
                return;
            }
            Insert::Mismatch => {
                // 同じ ID に違う中身: track を malformed として終える（12.1）
                self.stats.cache_mismatches += 1;
                self.malformed(&name);
                return;
            }
        }
        let targets: Vec<(SessionId, u64)> = t
            .subscribers
            .iter()
            .filter(|d| o.location() >= d.start && d.end.is_none_or(|e| o.location() <= e))
            .map(|d| (d.session, d.req))
            .collect();
        for (ds, dr) in targets {
            if let Some(s) = self.sessions.get_mut(&ds) {
                match delivery {
                    Delivery::Datagram => s.send_object_datagram(dr, &o, received),
                    _ => {
                        let _ = s.send_object(dr, &o, received, now);
                    }
                }
                self.stats.objects_out += 1;
            }
        }
    }

    fn malformed(&mut self, name: &FullTrackName) {
        let Some(t) = self.tracks.get_mut(name) else { return };
        let subs = std::mem::take(&mut t.subscribers);
        let up = t.upstream.take();
        t.upstream_ready = false;
        for d in subs {
            self.downstream.remove(&(d.session, d.req));
            if let Some(s) = self.s(d.session) {
                s.publish_done(d.req, publish_done::MALFORMED_TRACK, "malformed track");
            }
        }
        if let Some((us, ur)) = up {
            self.upstream.remove(&(us, ur));
            if let Some(s) = self.s(us) {
                s.cancel(ur);
            }
        }
    }

    fn end_track(&mut self, name: &FullTrackName, status: u64, reason: &str) {
        let Some(t) = self.tracks.get_mut(name) else { return };
        t.upstream = None;
        t.upstream_ready = false;
        t.ended = true;
        let subs = std::mem::take(&mut t.subscribers);
        let pend = std::mem::take(&mut t.pending);
        for d in subs {
            self.downstream.remove(&(d.session, d.req));
            if let Some(s) = self.s(d.session) {
                s.publish_done(d.req, status, reason);
            }
        }
        for (ds, dr, _) in pend {
            self.downstream.remove(&(ds, dr));
            if let Some(s) = self.s(ds) {
                s.request_error(dr, RequestErrorCode::DOES_NOT_EXIST, "track ended");
            }
        }
    }

    fn on_cancel(&mut self, sid: SessionId, req: u64) {
        // 下流の購読の解除
        if let Some(name) = self.downstream.remove(&(sid, req)) {
            if let Some(t) = self.tracks.get_mut(&name) {
                t.subscribers.retain(|d| !(d.session == sid && d.req == req));
                t.pending.retain(|(s, r, _)| !(*s == sid && *r == req));
                // 誰も見ていなければ上流の購読も解く（publisher 起点は除く）
                if t.subscribers.is_empty() && t.pending.is_empty()
                    && let Some((us, ur)) = t.upstream
                    && self.upstream.contains_key(&(us, ur))
                {
                    let publisher_initiated = self.announces.iter().all(|a| a.session != us)
                        || !self.sessions.contains_key(&us);
                    if !publisher_initiated {
                        t.upstream = None;
                        t.upstream_ready = false;
                        self.upstream.remove(&(us, ur));
                        if let Some(s) = self.sessions.get_mut(&us) {
                            s.cancel(ur);
                        }
                    }
                }
            }
            return;
        }
        // 上流の購読が取り消された（publisher が PUBLISH を取り下げた）
        if let Some(name) = self.upstream.remove(&(sid, req)) {
            self.end_track(&name, publish_done::TRACK_ENDED, "publisher cancelled");
            return;
        }
        // namespace の取り下げ
        if let Some(i) = self.announces.iter().position(|a| a.session == sid && a.req == req) {
            let a = self.announces.remove(i);
            self.notify_namespace(&a.prefix, true);
            return;
        }
        self.ns_subs.retain(|n| !(n.session == sid && n.req == req));
        // fetch の取り消し（下流）
        self.fetch_up.retain(|_, f| f.down != (sid, req));
    }

    fn on_fetch(&mut self, sid: SessionId, req: u64, ns: Namespace, track: Bytes, params: Params) {
        if !self.auth(sid, Action::Subscribe, &ns, Some(&track), &params) {
            if let Some(s) = self.s(sid) {
                s.request_error(req, RequestErrorCode::UNAUTHORIZED, "unauthorized");
            }
            return;
        }
        let name = FullTrackName::new(ns.clone(), track.clone());
        let order = params.group_order.unwrap_or(GroupOrder::Ascending);
        let has_upstream = self
            .announces
            .iter()
            .any(|a| ns.starts_with(&a.prefix) && a.session != sid)
            || self.tracks.get(&name).is_some_and(|t| t.upstream.is_some());
        // cache が要求の始まりを持っているときだけ cache から返す（持っていなければ上流へ）
        let cached = self.tracks.get(&name).filter(|t| {
            let Some(first) = t.cache.first() else { return false };
            let largest = t.cache.largest().unwrap_or_default();
            let start = match params.location_filter {
                Some(LocationFilter::AbsoluteStart(s)) | Some(LocationFilter::AbsoluteRange { start: s, .. }) => s,
                Some(LocationFilter::RelativeStart(n)) => Location::new(largest.group.saturating_add(1).saturating_sub(n), 0),
                _ => Location::new(0, 0),
            };
            start >= first || !has_upstream
        });
        if let Some(t) = cached {
            let largest = t.cache.largest().unwrap_or_default();
            let (start, end) = match params.location_filter {
                Some(LocationFilter::AbsoluteStart(s)) => (s, largest),
                Some(LocationFilter::AbsoluteRange {
                    start,
                    end_group,
                    end_object,
                }) => (start, Location::new(end_group, end_object.unwrap_or(u64::MAX)).min(largest)),
                Some(LocationFilter::RelativeStart(n)) => (Location::new(largest.group.saturating_add(1).saturating_sub(n), 0), largest),
                _ => (Location::new(0, 0), largest),
            };
            if start > largest {
                if let Some(s) = self.s(sid) {
                    s.request_error(req, RequestErrorCode::INVALID_RANGE, "start after largest object");
                }
                return;
            }
            let first = t.cache.first().unwrap_or_default();
            // FETCH は状態（Object Status）を運ばないので、状態だけの object は送らない（11.1.1）
            let mut objs: Vec<Object> = t
                .cache
                .range(start, end)
                .filter(|c| c.object.status == crate::ObjectStatus::Normal)
                .map(|c| c.object.clone())
                .collect();
            // End Location は範囲の中で実際に持っている最後の object（9.12）
            let end = objs.last().map_or(end, |o| o.location());
            // 最後の object まで返し、その後ろに End of Track があるなら End Of Track
            let eot = t.cache.end_of_track().is_some_and(|e| e > end) && end >= largest;
            if order == GroupOrder::Descending {
                // 群は降順、群の中は Object ID の昇順（3.2.1）
                objs.sort_by(|a, b| b.group.cmp(&a.group).then(a.id.cmp(&b.id)));
            }
            let props = t.properties.clone();
            self.stats.fetches_from_cache += 1;
            let Some(s) = self.sessions.get_mut(&sid) else { return };
            let _ = s.fetch_ok(req, eot, end, props);
            if order == GroupOrder::Ascending && start < first {
                // cache より前は分からない（End of Unknown Range、11.4.1.2）。範囲の外へは出さない
                let before = if first.object > 0 {
                    Location::new(first.group, first.object - 1)
                } else {
                    Location::new(first.group.saturating_sub(1), u64::MAX)
                };
                let before = before.min(end);
                if before >= start && first > start {
                    s.fetch_range_end(req, RangeEnd::Unknown, before);
                }
            }
            for o in &objs {
                s.fetch_object(req, o);
            }
            s.fetch_finish(req);
            return;
        }
        // cache に無い: 上流へ中継する
        let up = self
            .announces
            .iter()
            .filter(|a| ns.starts_with(&a.prefix) && a.session != sid)
            .max_by_key(|a| a.prefix.0.len())
            .map(|a| a.session)
            .or_else(|| self.tracks.get(&name).and_then(|t| t.upstream.map(|u| u.0)));
        let Some(up) = up else {
            if let Some(s) = self.s(sid) {
                s.request_error(req, RequestErrorCode::DOES_NOT_EXIST, "no such track");
            }
            return;
        };
        let up_params = Params {
            location_filter: params.location_filter,
            group_order: params.group_order,
            ..Default::default()
        };
        match self.s(up).map(|s| s.fetch(ns, track, up_params)) {
            Some(Ok(ur)) => {
                self.fetch_up.insert(
                    (up, ur),
                    FetchRelay {
                        down: (sid, req),
                        ok_sent: false,
                        complete: false,
                    },
                );
            }
            _ => {
                if let Some(s) = self.s(sid) {
                    s.request_error(req, RequestErrorCode::INTERNAL_ERROR, "upstream unavailable");
                }
            }
        }
    }

    fn drop_session_state(&mut self, sid: SessionId) {
        // 出版者がいなくなった
        let gone: Vec<Namespace> = self.announces.iter().filter(|a| a.session == sid).map(|a| a.prefix.clone()).collect();
        self.announces.retain(|a| a.session != sid);
        for ns in gone {
            self.notify_namespace(&ns, true);
        }
        self.ns_subs.retain(|n| n.session != sid);
        let ups: Vec<FullTrackName> = self
            .upstream
            .iter()
            .filter(|((s, _), _)| *s == sid)
            .map(|(_, n)| n.clone())
            .collect();
        self.upstream.retain(|(s, _), _| *s != sid);
        for name in ups {
            self.end_track(&name, publish_done::GOING_AWAY, "publisher went away");
        }
        // 購読者がいなくなった
        let downs: Vec<(SessionId, u64)> = self.downstream.keys().filter(|(s, _)| *s == sid).copied().collect();
        for (s, r) in downs {
            self.on_cancel(s, r);
        }
        self.fetch_up.retain(|(s, _), f| *s != sid && f.down.0 != sid);
        self.status_up.retain(|(s, _), d| *s != sid && d.0 != sid);
    }
}
