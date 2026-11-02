//! 小さな SFU の核（sans-io）。
//!
//! [`Room`] は publisher のトラック（simulcast なら層ごと）と subscriber の送信トラックを結び、AU を配る。
//! I/O は持たず、[`PeerConnection`](crate::PeerConnection) の出来事を入れて [`RoomOutput`] を取り出す。
//!
//! - **層の選択**: subscriber ごとの推定帯域（[`Room::set_bandwidth`]）× 余裕率を、その subscriber の映像の購読の数で
//!   分け、測った層の bitrate がそこに収まる最も高い層を選ぶ。下げるのはすぐ、上げるのは `upgrade_hold` の間
//!   続けて収まったときだけ（揺れを防ぐ）。[`Room::set_preferred_layer`] で固定もできる。
//! - **切り替えはキーフレームで**: 新しい層（別の送り手でも同じ）にキーフレームを求め、届いたら切り替える。
//!   それまでは今の層を配り続ける。送信トラック側（[`PeerConnection::send_access_unit`](crate::PeerConnection::send_access_unit)）が
//!   SSRC・番号・timestamp を続けるので、受け手には 1 本の流れに見える。
//! - **キーフレーム要求の集約**: 多数の subscriber の要求を、送り手の層ごとに `keyframe_min_interval` に 1 回へまとめる
//!   （[`KeyframeRequestLimiter`]、仕様 6.2）。
//!
//! ```text
//! // publisher の PeerConnection の Event::Track → room.add_publisher_track(..)
//! // publisher の Event::AccessUnit → room.on_access_unit(now, publisher, track, &au, config)
//! // subscriber の PeerConnection の Event::BandwidthEstimate → room.set_bandwidth(now, sub, bps)
//! // subscriber の Event::KeyframeRequest → room.on_keyframe_request(now, sub, track)
//! while let Some(o) = room.poll_output() {
//!     match o {
//!         RoomOutput::Forward { subscriber, track, au, config } => pcs[subscriber].send_access_unit(now, track, &au, config.as_ref()),
//!         RoomOutput::KeyframeRequest { publisher, track } => pcs[publisher].request_keyframe(now, track),
//!         RoomOutput::LayerChanged { .. } => {}
//!     }
//! }
//! ```

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use senca_core::media::{AccessUnit, Codec, CodecConfig, MediaKind, TrackId};
use senca_rtp::keyframe::KeyframeRequestLimiter;
use senca_rtp::rtcp::RtcpPacket;

/// 参加者の番号（呼び出し側が決める）。
pub type PeerId = u64;

/// 設定。
#[derive(Debug, Clone)]
pub struct RoomConfig {
    /// 送り手の層ごとのキーフレーム要求の最短の間隔。
    pub keyframe_min_interval: Duration,
    /// bitrate を測る窓。
    pub bitrate_window: Duration,
    /// 推定帯域のうち映像に使う割合。
    pub headroom: f64,
    /// 音声の分として推定帯域から引く値（bit/s）。
    pub audio_reserve_bps: u64,
    /// 上の層へ上げる前に、収まる状態が続くべき時間。
    pub upgrade_hold: Duration,
    /// 切り替えの最短の間隔（下げるとき）。
    pub min_switch_interval: Duration,
    /// 下げる前に、収まらない状態が続くべき時間（推定が必要量の `severe` 倍を下回ればすぐ下げる）。
    pub downgrade_hold: Duration,
    pub severe: f64,
    /// キーフレームを待つ最長の時間（過ぎたら求め直す）。
    pub keyframe_wait: Duration,
    pub max_tracks: usize,
    pub max_subscriptions: usize,
    /// 出力の待ち行列の上限。
    pub max_outputs: usize,
}

impl Default for RoomConfig {
    fn default() -> Self {
        RoomConfig {
            keyframe_min_interval: Duration::from_millis(500),
            bitrate_window: Duration::from_secs(2),
            headroom: 0.9,
            audio_reserve_bps: 64_000,
            upgrade_hold: Duration::from_secs(2),
            min_switch_interval: Duration::from_millis(500),
            downgrade_hold: Duration::from_millis(1000),
            severe: 0.6,
            keyframe_wait: Duration::from_secs(1),
            max_tracks: 4096,
            max_subscriptions: 65536,
            max_outputs: 65536,
        }
    }
}

/// 出力。
#[derive(Debug, Clone)]
pub enum RoomOutput {
    /// subscriber の送信トラックへ送る AU（`au.track` は Room の中の送り元の番号）。
    Forward {
        subscriber: PeerId,
        track: TrackId,
        au: AccessUnit,
        config: Option<CodecConfig>,
    },
    /// publisher の受信トラックにキーフレームを求める（[`PeerConnection::request_keyframe`](crate::PeerConnection::request_keyframe)）。
    KeyframeRequest { publisher: PeerId, track: TrackId },
    /// subscriber に送りたい速さ（今より 1 つ上の層に要る帯域。上が無ければ 0）。
    /// [`PeerConnection::set_desired_bitrate`](crate::PeerConnection::set_desired_bitrate) に渡すと、
    /// padding で帯域を探り、上の層へ上げられるようになる。
    DesiredBitrate { subscriber: PeerId, bps: u64 },
    /// subscriber の層が変わった（`rid` は新しい層）。
    LayerChanged {
        subscriber: PeerId,
        track: TrackId,
        rid: Option<String>,
    },
}

#[derive(Debug, Default)]
struct Meter {
    samples: VecDeque<(Instant, usize)>,
    bytes: usize,
}

impl Meter {
    fn add(&mut self, now: Instant, n: usize, window: Duration) {
        self.samples.push_back((now, n));
        self.bytes += n;
        while let Some(&(t, b)) = self.samples.front() {
            if now.saturating_duration_since(t) > window || self.samples.len() > 4096 {
                self.samples.pop_front();
                self.bytes -= b;
            } else {
                break;
            }
        }
    }

    fn bps(&self, now: Instant, window: Duration) -> Option<u64> {
        let first = self.samples.front()?.0;
        let span = now.saturating_duration_since(first);
        if span < window / 4 {
            return None;
        }
        Some((self.bytes as f64 * 8.0 / span.max(window / 2).as_secs_f64()) as u64)
    }
}

#[derive(Debug)]
struct Source {
    publisher: PeerId,
    track: TrackId,
    kind: MediaKind,
    codec: Codec,
    group: String,
    rid: Option<String>,
    order: usize,
    config: Option<CodecConfig>,
    meter: Meter,
}

#[derive(Debug)]
struct Subscription {
    subscriber: PeerId,
    track: TrackId,
    publisher: PeerId,
    group: String,
    current: Option<u32>,
    pending: Option<u32>,
    pending_since: Option<Instant>,
    preferred: Option<String>,
    last_switch: Option<Instant>,
    upgrade_since: Option<Instant>,
    downgrade_since: Option<Instant>,
    /// 切り替えで送り手（別の publisher）が変わる。
    kind: MediaKind,
}

/// SFU の部屋。
pub struct Room {
    cfg: RoomConfig,
    sources: HashMap<u32, Source>,
    by_pub: HashMap<(PeerId, TrackId), u32>,
    subs: Vec<Subscription>,
    bandwidth: HashMap<PeerId, u64>,
    desired: HashMap<PeerId, u64>,
    limiter: KeyframeRequestLimiter,
    next_id: u32,
    out: VecDeque<RoomOutput>,
    dropped: u64,
}

impl std::fmt::Debug for Room {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Room")
            .field("sources", &self.sources.len())
            .field("subscriptions", &self.subs.len())
            .finish_non_exhaustive()
    }
}

impl Room {
    pub fn new(cfg: RoomConfig) -> Room {
        Room {
            limiter: KeyframeRequestLimiter::new(
                0,
                cfg.keyframe_min_interval,
                false,
                cfg.max_tracks,
            ),
            cfg,
            sources: HashMap::new(),
            by_pub: HashMap::new(),
            subs: Vec::new(),
            bandwidth: HashMap::new(),
            desired: HashMap::new(),
            next_id: 1,
            out: VecDeque::new(),
            dropped: 0,
        }
    }

    /// publisher の受信トラックを足す。同じ `group`（publisher の m 行の mid など）の映像は simulcast の層として扱い、
    /// 足した順（低い層から）を既定の並びにする。Room の中の番号を返す。
    pub fn add_publisher_track(
        &mut self,
        publisher: PeerId,
        track: TrackId,
        kind: MediaKind,
        codec: Codec,
        group: &str,
        rid: Option<&str>,
    ) -> Option<TrackId> {
        if let Some(&id) = self.by_pub.get(&(publisher, track)) {
            if let Some(s) = self.sources.get_mut(&id) {
                s.codec = codec;
            }
            return Some(TrackId(id));
        }
        if self.sources.len() >= self.cfg.max_tracks {
            return None;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let order = self
            .sources
            .values()
            .filter(|s| s.publisher == publisher && s.group == group)
            .count();
        self.sources.insert(
            id,
            Source {
                publisher,
                track,
                kind,
                codec,
                group: group.to_string(),
                rid: rid.map(str::to_string),
                order,
                config: None,
                meter: Meter::default(),
            },
        );
        self.by_pub.insert((publisher, track), id);
        Some(TrackId(id))
    }

    /// publisher のトラックを外す。
    pub fn remove_publisher_track(&mut self, publisher: PeerId, track: TrackId) {
        if let Some(id) = self.by_pub.remove(&(publisher, track)) {
            self.sources.remove(&id);
            self.limiter.remove(id);
            for s in self.subs.iter_mut() {
                if s.current == Some(id) {
                    s.current = None;
                }
                if s.pending == Some(id) {
                    s.pending = None;
                }
            }
        }
    }

    /// 参加者を外す（publisher のトラックも subscriber の購読も）。
    pub fn remove_peer(&mut self, peer: PeerId) {
        let tracks: Vec<TrackId> = self
            .by_pub
            .keys()
            .filter(|(p, _)| *p == peer)
            .map(|(_, t)| *t)
            .collect();
        for t in tracks {
            self.remove_publisher_track(peer, t);
        }
        self.subs.retain(|s| s.subscriber != peer);
        self.bandwidth.remove(&peer);
        self.desired.remove(&peer);
    }

    /// subscriber の送信トラック `track` に、publisher の `group` を配る。既に購読していれば送り手を替える
    /// （新しい送り手のキーフレームで切り替える）。
    pub fn subscribe(
        &mut self,
        now: Instant,
        subscriber: PeerId,
        track: TrackId,
        publisher: PeerId,
        group: &str,
    ) -> bool {
        let kind = self
            .sources
            .values()
            .find(|s| s.publisher == publisher && s.group == group)
            .map(|s| s.kind)
            .unwrap_or(MediaKind::Video);
        match self
            .subs
            .iter_mut()
            .position(|s| s.subscriber == subscriber && s.track == track)
        {
            Some(i) => {
                let s = &mut self.subs[i];
                s.publisher = publisher;
                s.group = group.to_string();
                s.kind = kind;
                s.pending = None;
                s.upgrade_since = None;
            }
            None => {
                if self.subs.len() >= self.cfg.max_subscriptions {
                    return false;
                }
                self.subs.push(Subscription {
                    subscriber,
                    track,
                    publisher,
                    group: group.to_string(),
                    current: None,
                    pending: None,
                    pending_since: None,
                    preferred: None,
                    last_switch: None,
                    upgrade_since: None,
                    downgrade_since: None,
                    kind,
                });
            }
        }
        let i = self
            .subs
            .iter()
            .position(|s| s.subscriber == subscriber && s.track == track)
            .unwrap_or(0);
        self.select(now, i, true);
        true
    }

    pub fn unsubscribe(&mut self, subscriber: PeerId, track: TrackId) {
        self.subs
            .retain(|s| !(s.subscriber == subscriber && s.track == track));
    }

    /// subscriber の推定帯域（[`Event::BandwidthEstimate`](crate::Event::BandwidthEstimate)）。
    pub fn set_bandwidth(&mut self, now: Instant, subscriber: PeerId, bps: u64) {
        self.bandwidth.insert(subscriber, bps);
        for i in 0..self.subs.len() {
            if self.subs[i].subscriber == subscriber {
                self.select(now, i, false);
            }
        }
    }

    /// 層を固定する（`None` で自動に戻す）。
    pub fn set_preferred_layer(
        &mut self,
        now: Instant,
        subscriber: PeerId,
        track: TrackId,
        rid: Option<&str>,
    ) {
        if let Some(i) = self
            .subs
            .iter()
            .position(|s| s.subscriber == subscriber && s.track == track)
        {
            self.subs[i].preferred = rid.map(str::to_string);
            self.subs[i].upgrade_since = None;
            self.select(now, i, true);
        }
    }

    /// 今配っている層の rid。
    pub fn current_layer(&self, subscriber: PeerId, track: TrackId) -> Option<Option<String>> {
        let s = self
            .subs
            .iter()
            .find(|s| s.subscriber == subscriber && s.track == track)?;
        let id = s.current?;
        Some(self.sources.get(&id)?.rid.clone())
    }

    /// 層の測った bitrate（rid, bit/s）。低い順。
    pub fn layer_bitrates(
        &self,
        now: Instant,
        publisher: PeerId,
        group: &str,
    ) -> Vec<(Option<String>, Option<u64>)> {
        let mut v: Vec<(usize, Option<String>, Option<u64>)> = self
            .sources
            .values()
            .filter(|s| s.publisher == publisher && s.group == group)
            .map(|s| {
                (
                    s.order,
                    s.rid.clone(),
                    s.meter.bps(now, self.cfg.bitrate_window),
                )
            })
            .collect();
        v.sort_by_key(|x| x.0);
        v.into_iter().map(|(_, r, b)| (r, b)).collect()
    }

    /// 層の並び（低い順）: 測った bitrate、分からなければ足した順。
    fn layers(&self, now: Instant, publisher: PeerId, group: &str) -> Vec<(u32, Option<u64>)> {
        let mut v: Vec<(u32, usize, Option<u64>)> = self
            .sources
            .iter()
            .filter(|(_, s)| s.publisher == publisher && s.group == group)
            .map(|(id, s)| (*id, s.order, s.meter.bps(now, self.cfg.bitrate_window)))
            .collect();
        let all_measured = v.iter().all(|x| x.2.is_some());
        if all_measured {
            v.sort_by_key(|x| (x.2, x.1));
        } else {
            v.sort_by_key(|x| x.1);
        }
        v.into_iter().map(|(id, _, b)| (id, b)).collect()
    }

    fn target(&self, now: Instant, i: usize) -> Option<u32> {
        let s = &self.subs[i];
        let layers = self.layers(now, s.publisher, &s.group);
        if layers.is_empty() {
            return None;
        }
        if let Some(p) = &s.preferred
            && let Some((id, _)) = layers
                .iter()
                .find(|(id, _)| self.sources[id].rid.as_deref() == Some(p))
        {
            return Some(*id);
        }
        if s.kind != MediaKind::Video || layers.len() == 1 {
            return Some(layers[layers.len() - 1].0);
        }
        let Some(&bw) = self.bandwidth.get(&s.subscriber) else {
            // 推定がまだ無ければ一番低い層から始める
            return Some(layers[0].0);
        };
        let nvideo = self
            .subs
            .iter()
            .filter(|x| x.subscriber == s.subscriber && x.kind == MediaKind::Video)
            .count()
            .max(1);
        let budget = ((bw.saturating_sub(self.cfg.audio_reserve_bps)) as f64 * self.cfg.headroom)
            / nvideo as f64;
        let mut pick = layers[0].0;
        for (id, b) in &layers {
            match b {
                Some(b) if (*b as f64) <= budget => pick = *id,
                Some(_) => break,
                None => break,
            }
        }
        Some(pick)
    }

    /// subscriber が 1 つ上の層へ上がるのに要る帯域（上が無い・固定なら 0）。
    fn desired_for(&self, now: Instant, subscriber: PeerId) -> u64 {
        let video: Vec<&Subscription> = self
            .subs
            .iter()
            .filter(|x| x.subscriber == subscriber && x.kind == MediaKind::Video)
            .collect();
        let nvideo = video.len().max(1) as f64;
        let mut need = 0f64;
        for s in &video {
            if s.preferred.is_some() {
                continue;
            }
            let layers = self.layers(now, s.publisher, &s.group);
            let cur = s.pending.or(s.current);
            let pos = cur.and_then(|c| layers.iter().position(|(x, _)| *x == c));
            let next = match pos {
                Some(p) => layers.get(p + 1),
                None => layers.first(),
            };
            if let Some((_, Some(b))) = next {
                need = need.max(*b as f64);
            }
        }
        if need == 0.0 {
            return 0;
        }
        (need * nvideo / self.cfg.headroom * 1.1) as u64 + self.cfg.audio_reserve_bps
    }

    fn update_desired(&mut self, now: Instant, subscriber: PeerId) {
        let d = self.desired_for(now, subscriber);
        let old = self.desired.get(&subscriber).copied();
        let changed = old.is_none_or(|o| {
            (o == 0) != (d == 0) || (o as f64 - d as f64).abs() > 0.1 * o.max(1) as f64
        });
        if changed {
            self.desired.insert(subscriber, d);
            self.push(RoomOutput::DesiredBitrate { subscriber, bps: d });
        }
    }

    /// 今の層に要る帯域に、推定が大きく足りない。
    fn severe_shortage(&self, now: Instant, i: usize, cur: u32) -> bool {
        let s = &self.subs[i];
        let Some(&bw) = self.bandwidth.get(&s.subscriber) else {
            return false;
        };
        let Some(rate) = self
            .sources
            .get(&cur)
            .and_then(|c| c.meter.bps(now, self.cfg.bitrate_window))
        else {
            return false;
        };
        let nvideo = self
            .subs
            .iter()
            .filter(|x| x.subscriber == s.subscriber && x.kind == MediaKind::Video)
            .count()
            .max(1) as f64;
        let budget =
            (bw.saturating_sub(self.cfg.audio_reserve_bps)) as f64 * self.cfg.headroom / nvideo;
        budget < rate as f64 * self.cfg.severe
    }

    fn rank(&self, now: Instant, i: usize, id: u32) -> usize {
        let s = &self.subs[i];
        self.layers(now, s.publisher, &s.group)
            .iter()
            .position(|(x, _)| *x == id)
            .unwrap_or(0)
    }

    /// 層を選び直す。
    fn select(&mut self, now: Instant, i: usize, force: bool) {
        let Some(target) = self.target(now, i) else {
            return;
        };
        let s = &self.subs[i];
        let same_group = s
            .current
            .and_then(|c| self.sources.get(&c))
            .is_some_and(|c| c.publisher == s.publisher && c.group == s.group);
        let subscriber = s.subscriber;
        if s.current == Some(target) {
            self.subs[i].pending = None;
            self.subs[i].upgrade_since = None;
            self.subs[i].downgrade_since = None;
            self.update_desired(now, subscriber);
            return;
        }
        if s.pending == Some(target) {
            return;
        }
        if same_group && !force {
            let cur = s.current.unwrap_or(target);
            let up = self.rank(now, i, target) > self.rank(now, i, cur);
            let s = &mut self.subs[i];
            if up {
                // 上げるのは収まる状態が続いてから
                let since = *s.upgrade_since.get_or_insert(now);
                if now.saturating_duration_since(since) < self.cfg.upgrade_hold {
                    return;
                }
            } else {
                if s.last_switch.is_some_and(|t| {
                    now.saturating_duration_since(t) < self.cfg.min_switch_interval
                }) {
                    return;
                }
                // 少し足りないだけなら様子を見る（推定の揺れで行き来しない）
                let severe = self.severe_shortage(now, i, cur);
                let s = &mut self.subs[i];
                let since = *s.downgrade_since.get_or_insert(now);
                if !severe && now.saturating_duration_since(since) < self.cfg.downgrade_hold {
                    return;
                }
            }
        }
        let s = &mut self.subs[i];
        s.pending = Some(target);
        s.pending_since = Some(now);
        s.upgrade_since = None;
        s.downgrade_since = None;
        self.request_keyframe(now, target);
    }

    fn request_keyframe(&mut self, now: Instant, id: u32) {
        if let Some(RtcpPacket::Pli(p)) = self.limiter.request(id, now) {
            self.emit_keyframe(p.media_ssrc);
        }
    }

    fn emit_keyframe(&mut self, id: u32) {
        if let Some(s) = self.sources.get(&id) {
            let (publisher, track) = (s.publisher, s.track);
            self.push(RoomOutput::KeyframeRequest { publisher, track });
        }
    }

    fn push(&mut self, o: RoomOutput) {
        if self.out.len() >= self.cfg.max_outputs {
            self.dropped += 1;
            return;
        }
        self.out.push_back(o);
    }

    /// publisher の AU を入れる。
    pub fn on_access_unit(
        &mut self,
        now: Instant,
        publisher: PeerId,
        track: TrackId,
        au: &AccessUnit,
        config: Option<&CodecConfig>,
    ) {
        let Some(&id) = self.by_pub.get(&(publisher, track)) else {
            return;
        };
        let window = self.cfg.bitrate_window;
        let cfg = {
            let Some(src) = self.sources.get_mut(&id) else {
                return;
            };
            src.meter.add(now, au.payload.len(), window);
            if let Some(c) = config {
                src.config = Some(c.clone());
            }
            src.config.clone()
        };
        for i in 0..self.subs.len() {
            let s = &self.subs[i];
            let video = s.kind == MediaKind::Video;
            if s.pending == Some(id) && (au.keyframe || !video) {
                let rid = self.sources.get(&id).and_then(|x| x.rid.clone());
                let s = &mut self.subs[i];
                s.current = Some(id);
                s.pending = None;
                s.last_switch = Some(now);
                let (subscriber, t) = (s.subscriber, s.track);
                self.push(RoomOutput::LayerChanged {
                    subscriber,
                    track: t,
                    rid,
                });
                self.update_desired(now, subscriber);
            } else if s.pending.is_some()
                && s.pending_since
                    .is_some_and(|t| now.saturating_duration_since(t) >= self.cfg.keyframe_wait)
            {
                // キーフレームが来ない: 求め直す
                let p = s.pending.unwrap_or(id);
                self.subs[i].pending_since = Some(now);
                self.request_keyframe(now, p);
            }
            let s = &self.subs[i];
            if s.current == Some(id) {
                let mut a = au.clone();
                a.track = TrackId(id);
                let (subscriber, t) = (s.subscriber, s.track);
                self.push(RoomOutput::Forward {
                    subscriber,
                    track: t,
                    au: a,
                    config: cfg.clone(),
                });
            }
        }
        // 層の bitrate が変わるので、ときどき選び直す（キーフレームごと）
        if au.keyframe {
            for i in 0..self.subs.len() {
                let s = &self.subs[i];
                if s.publisher == publisher
                    && self.sources.get(&id).is_some_and(|x| x.group == s.group)
                {
                    self.select(now, i, false);
                }
            }
        }
    }

    /// subscriber の送信トラックにキーフレームが要る（[`Event::KeyframeRequest`](crate::Event::KeyframeRequest)）。
    pub fn on_keyframe_request(&mut self, now: Instant, subscriber: PeerId, track: TrackId) {
        let Some(s) = self
            .subs
            .iter()
            .find(|s| s.subscriber == subscriber && s.track == track)
        else {
            return;
        };
        if let Some(id) = s.pending.or(s.current) {
            self.request_keyframe(now, id);
        } else {
            let i = self
                .subs
                .iter()
                .position(|s| s.subscriber == subscriber && s.track == track)
                .unwrap_or(0);
            self.select(now, i, true);
        }
    }

    pub fn poll_output(&mut self) -> Option<RoomOutput> {
        self.out.pop_front()
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        self.limiter.poll_timeout()
    }

    /// 保留したキーフレーム要求を出す。
    pub fn handle_timeout(&mut self, now: Instant) {
        for p in self.limiter.poll(now) {
            if let RtcpPacket::Pli(p) = p {
                self.emit_keyframe(p.media_ssrc);
            }
        }
    }

    /// まとめて抑えたキーフレーム要求の数。
    pub fn suppressed_keyframe_requests(&self) -> u64 {
        self.limiter.suppressed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use senca_core::time::Timebase;

    fn au(track: u32, n: u64, size: usize, key: bool) -> AccessUnit {
        AccessUnit {
            track: TrackId(track),
            codec: Codec::Vp8,
            epoch: 0,
            sequence: n,
            config_generation: 0,
            pts: n as i64 * 3000,
            dts: n as i64 * 3000,
            duration: 3000,
            timebase: Timebase::new(1, 90000),
            keyframe: key,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from(vec![0u8; size]),
        }
    }

    struct Sim {
        room: Room,
        t0: Instant,
        frame: u64,
        /// 次の AU をキーフレームにする層（publisher の track）。
        kf_pending: Vec<(PeerId, TrackId)>,
        forwarded: Vec<(PeerId, TrackId, u32, bool)>,
        layer_changes: Vec<(PeerId, Option<String>)>,
        kf_requests: usize,
    }

    impl Sim {
        fn new() -> Sim {
            let mut room = Room::new(RoomConfig::default());
            let t0 = Instant::now();
            // publisher 1: 3 層（150k・500k・1.5M at 30 fps）
            for (t, rid) in [(1u32, "q"), (2, "h"), (3, "f")] {
                room.add_publisher_track(
                    1,
                    TrackId(t),
                    MediaKind::Video,
                    Codec::Vp8,
                    "0",
                    Some(rid),
                );
            }
            Sim {
                room,
                t0,
                frame: 0,
                kf_pending: Vec::new(),
                forwarded: Vec::new(),
                layer_changes: Vec::new(),
                kf_requests: 0,
            }
        }

        fn now(&self) -> Instant {
            self.t0 + Duration::from_millis(self.frame * 33)
        }

        fn step(&mut self, frames: u64) {
            for _ in 0..frames {
                let now = self.now();
                self.room.handle_timeout(now);
                for (t, size) in [(1u32, 625usize), (2, 2083), (3, 6250)] {
                    let key = self.frame.is_multiple_of(300)
                        || self.kf_pending.iter().any(|(_, x)| x.0 == t);
                    self.kf_pending.retain(|(_, x)| x.0 != t || !key);
                    let a = au(t, self.frame, if key { size * 3 } else { size }, key);
                    self.room.on_access_unit(now, 1, TrackId(t), &a, None);
                }
                while let Some(o) = self.room.poll_output() {
                    match o {
                        RoomOutput::Forward {
                            subscriber,
                            track,
                            au,
                            ..
                        } => self
                            .forwarded
                            .push((subscriber, track, au.track.0, au.keyframe)),
                        RoomOutput::KeyframeRequest { publisher, track } => {
                            self.kf_requests += 1;
                            self.kf_pending.push((publisher, track));
                        }
                        RoomOutput::LayerChanged {
                            subscriber, rid, ..
                        } => self.layer_changes.push((subscriber, rid)),
                        RoomOutput::DesiredBitrate { .. } => {}
                    }
                }
                self.frame += 1;
            }
        }
    }

    #[test]
    fn layer_selection_follows_bandwidth_at_keyframes() {
        let mut s = Sim::new();
        s.step(60); // bitrate を測る
        let now = s.now();
        assert!(s.room.subscribe(now, 10, TrackId(100), 1, "0"));
        s.room.set_bandwidth(now, 10, 3_000_000);
        s.step(30);
        assert_eq!(
            s.room.current_layer(10, TrackId(100)),
            Some(Some("f".into()))
        );
        // 最初に配った AU はキーフレーム
        let first = s.forwarded.iter().find(|f| f.0 == 10).unwrap();
        assert!(first.3);
        // 帯域が下がるとすぐ中の層へ
        let now = s.now();
        s.room.set_bandwidth(now, 10, 800_000);
        s.step(30);
        assert_eq!(
            s.room.current_layer(10, TrackId(100)),
            Some(Some("h".into()))
        );
        // 層の切り替わりの直後の AU はキーフレーム
        let mut prev_src = None;
        for f in s.forwarded.iter().filter(|f| f.0 == 10) {
            if prev_src.is_some() && prev_src != Some(f.2) {
                assert!(f.3, "switched on a non-keyframe");
            }
            prev_src = Some(f.2);
        }
        // さらに下がると一番下へ
        let now = s.now();
        s.room.set_bandwidth(now, 10, 200_000);
        s.step(30);
        assert_eq!(
            s.room.current_layer(10, TrackId(100)),
            Some(Some("q".into()))
        );
        // 戻っても上げるのは upgrade_hold の後
        let now = s.now();
        s.room.set_bandwidth(now, 10, 3_000_000);
        s.step(10);
        assert_eq!(
            s.room.current_layer(10, TrackId(100)),
            Some(Some("q".into()))
        );
        for _ in 0..12 {
            let now = s.now();
            s.room.set_bandwidth(now, 10, 3_000_000);
            s.step(10);
        }
        assert_eq!(
            s.room.current_layer(10, TrackId(100)),
            Some(Some("f".into()))
        );
        let names: Vec<_> = s
            .layer_changes
            .iter()
            .map(|c| c.1.clone().unwrap())
            .collect();
        assert_eq!(names, vec!["f", "h", "q", "f"]);
    }

    #[test]
    fn keyframe_requests_are_aggregated() {
        let mut s = Sim::new();
        s.step(60);
        let now = s.now();
        for sub in 0..100u64 {
            s.room.subscribe(now, 1000 + sub, TrackId(1), 1, "0");
            s.room.set_bandwidth(now, 1000 + sub, 3_000_000);
        }
        s.step(30);
        let before = s.kf_requests;
        // 100 人が 1 秒の間に何度も求めても、層ごとに 500 ms に 1 回
        for k in 0..30 {
            let now = s.now();
            for sub in 0..100u64 {
                s.room.on_keyframe_request(now, 1000 + sub, TrackId(1));
            }
            let _ = k;
            s.step(1);
        }
        let n = s.kf_requests - before;
        assert!(n <= 3, "{n} keyframe requests for 3000 subscriber requests");
        assert!(s.room.suppressed_keyframe_requests() > 2900);
    }

    #[test]
    fn publisher_switch_waits_for_keyframe() {
        let mut s = Sim::new();
        s.room
            .add_publisher_track(2, TrackId(7), MediaKind::Video, Codec::Vp8, "cam", None);
        s.step(30);
        let now = s.now();
        s.room.subscribe(now, 20, TrackId(5), 1, "0");
        s.room.set_preferred_layer(now, 20, TrackId(5), Some("h"));
        s.step(30);
        assert_eq!(s.room.current_layer(20, TrackId(5)), Some(Some("h".into())));
        // 送り手 2 へ切り替える: 2 のキーフレームまでは 1 を配り続ける
        let now = s.now();
        s.room.subscribe(now, 20, TrackId(5), 2, "cam");
        let mut sent2 = 0;
        for f in 0..30u64 {
            let now = s.now();
            let key = f == 10;
            s.room
                .on_access_unit(now, 2, TrackId(7), &au(7, f, 1000, key), None);
            s.step(1);
            let got: Vec<_> = s.forwarded.iter().filter(|x| x.0 == 20).cloned().collect();
            let last = got.last().unwrap();
            if f < 10 {
                assert_ne!(Some(last.2), s.room.by_pub.get(&(2, TrackId(7))).copied());
            } else {
                sent2 += 1;
            }
        }
        assert!(sent2 > 0);
        assert_eq!(s.room.current_layer(20, TrackId(5)), Some(None));
        // 外すと配らない
        s.room.remove_peer(2);
        let n = s.forwarded.len();
        s.step(5);
        assert!(s.forwarded[n..].iter().all(|x| x.0 != 20));
    }
}
