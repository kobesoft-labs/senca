//! 購読の集約（sans-io）。
//!
//! あるノードで同じ remote stream を見る購読者（RTMP play・WebRTC・HLS の packager など）が何人いても、
//! ノード間の上流の購読は 1 つにする。
//!
//! - 最初の購読者で上流を開き（[`AggAction::Open`]）、最後の購読者が去ってから `linger` 後に閉じる
//!   （[`AggAction::Close`]）。linger の間に来た購読者はすぐに直近の GOP を受け取れる。
//! - 後から来た購読者には、設定と直近の GOP（キーフレームから）を先に渡す（ノードの hub と同じ）。
//!   トラックごとに持つので、サイマルキャストの層ごとに正しい GOP になる。
//! - サイマルキャスト: 上流に頼むトラックは、購読者が選んだトラックの和だけ（[`AggAction::Update`]）。
//!
//! 配るのは統合側（[`Aggregator::on_upstream`] が配り先の [`LocalId`] を返す）。

use crate::event::{StreamEvent, TrackSel};
use senca_core::{AccessUnit, CodecConfig, MediaKind, TrackId};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

/// このノードの中の購読者の番号。
pub type LocalId = u64;

#[derive(Debug, Clone)]
pub struct AggregatorConfig {
    /// 最後の購読者が去ってから上流を閉じるまで。
    pub linger: Duration,
    /// GOP キャッシュの上限（AU 数・バイト。超えたら次のキーフレームまで持たない）。
    pub gop_max_aus: usize,
    pub gop_max_bytes: usize,
    /// 映像の無い stream で持つ音声の AU の数。
    pub audio_only_keep: usize,
}

impl Default for AggregatorConfig {
    fn default() -> Self {
        AggregatorConfig {
            linger: Duration::from_secs(5),
            gop_max_aus: 2000,
            gop_max_bytes: 64 << 20,
            audio_only_keep: 50,
        }
    }
}

/// 統合側がすること。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggAction {
    /// 上流を開く（所有するノードを調べて購読する）。
    Open { stream_id: String, tracks: TrackSel },
    /// 上流のトラック（層）を変える。
    Update { stream_id: String, tracks: TrackSel },
    /// 上流を閉じる。
    Close { stream_id: String },
}

#[derive(Debug, Default)]
struct Gop {
    order: u64,
    tracks: BTreeMap<TrackId, VecDeque<(u64, AccessUnit)>>,
    aus: usize,
    bytes: usize,
}

impl Gop {
    fn clear(&mut self) {
        self.tracks.clear();
        self.aus = 0;
        self.bytes = 0;
    }

    fn push(&mut self, au: &AccessUnit, has_video: bool, cfg: &AggregatorConfig) {
        self.order += 1;
        let video = au.codec.kind() == MediaKind::Video;
        let q = self.tracks.entry(au.track).or_default();
        if video {
            if au.keyframe {
                for (_, a) in q.drain(..) {
                    self.aus -= 1;
                    self.bytes -= a.payload.len();
                }
            } else if q.is_empty() {
                return; // キーフレームの前の差分は持たない
            }
        }
        q.push_back((self.order, au.clone()));
        self.aus += 1;
        self.bytes += au.payload.len();
        if !video {
            // 映像があれば、最も古い映像の GOP の始まりより前の音声は要らない
            let start = if has_video {
                self.tracks
                    .iter()
                    .filter(|(_, q)| {
                        q.front()
                            .is_some_and(|(_, a)| a.codec.kind() == MediaKind::Video)
                    })
                    .filter_map(|(_, q)| q.front().map(|x| x.0))
                    .min()
            } else {
                None
            };
            let q = self.tracks.get_mut(&au.track).expect("pushed");
            loop {
                let drop = match (start, q.front()) {
                    (_, None) => false,
                    (Some(s), Some(f)) => f.0 < s,
                    (None, Some(_)) => q.len() > cfg.audio_only_keep,
                };
                if !drop {
                    break;
                }
                let (_, a) = q.pop_front().expect("front");
                self.aus -= 1;
                self.bytes -= a.payload.len();
            }
        }
        if self.aus > cfg.gop_max_aus || self.bytes > cfg.gop_max_bytes {
            self.clear(); // 異常に長い GOP は持たない
        }
    }

    fn initial(&self, sel: &TrackSel, out: &mut Vec<StreamEvent>) {
        let mut all: Vec<&(u64, AccessUnit)> = self
            .tracks
            .iter()
            .filter(|(t, _)| sel.contains(**t))
            .flat_map(|(_, q)| q.iter())
            .collect();
        all.sort_by_key(|x| x.0);
        out.extend(all.into_iter().map(|(_, a)| StreamEvent::Au(a.clone())));
    }
}

#[derive(Debug)]
struct Agg {
    subs: BTreeMap<LocalId, TrackSel>,
    upstream: TrackSel,
    linger_until: Option<Instant>,
    configs: BTreeMap<TrackId, CodecConfig>,
    gop: Gop,
}

impl Agg {
    fn union(&self) -> TrackSel {
        let mut it = self.subs.values();
        let Some(first) = it.next() else {
            return self.upstream.clone();
        };
        it.fold(first.clone(), |a, b| a.union(b))
    }
    fn has_video(&self) -> bool {
        self.configs
            .values()
            .any(|c| c.codec.kind() == MediaKind::Video)
    }
    fn initial(&self, sel: &TrackSel) -> Vec<StreamEvent> {
        let mut v: Vec<StreamEvent> = self
            .configs
            .iter()
            .filter(|(t, _)| sel.contains(**t))
            .map(|(t, c)| StreamEvent::Config(*t, c.clone()))
            .collect();
        self.gop.initial(sel, &mut v);
        v
    }
}

/// 購読の集約の表。
#[derive(Debug)]
pub struct Aggregator {
    cfg: AggregatorConfig,
    streams: HashMap<String, Agg>,
    owner: HashMap<LocalId, String>,
    next_id: LocalId,
    actions: VecDeque<AggAction>,
}

/// 集約の様子。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AggStats {
    pub upstreams: usize,
    pub local_subscribers: usize,
    pub lingering: usize,
}

impl Aggregator {
    pub fn new(cfg: AggregatorConfig) -> Aggregator {
        Aggregator {
            cfg,
            streams: HashMap::new(),
            owner: HashMap::new(),
            next_id: 1,
            actions: VecDeque::new(),
        }
    }

    pub fn stats(&self) -> AggStats {
        AggStats {
            upstreams: self.streams.len(),
            local_subscribers: self.owner.len(),
            lingering: self
                .streams
                .values()
                .filter(|a| a.linger_until.is_some())
                .count(),
        }
    }

    pub fn poll_action(&mut self) -> Option<AggAction> {
        self.actions.pop_front()
    }

    /// 上流に頼んでいるトラック。
    /// 上流の今の設定（取りこぼした購読者へ渡し直す）。
    pub fn configs_of(&self, stream_id: &str) -> Vec<(TrackId, CodecConfig)> {
        self.streams
            .get(stream_id)
            .map(|a| a.configs.iter().map(|(t, c)| (*t, c.clone())).collect())
            .unwrap_or_default()
    }

    pub fn upstream_tracks(&self, stream_id: &str) -> Option<&TrackSel> {
        self.streams.get(stream_id).map(|a| &a.upstream)
    }

    pub fn subscribers(&self, stream_id: &str) -> impl Iterator<Item = LocalId> + '_ {
        self.streams
            .get(stream_id)
            .into_iter()
            .flat_map(|a| a.subs.keys().copied())
    }

    /// 購読者を足す。返り値は番号と、先に渡す物（設定と直近の GOP）。
    pub fn add_subscriber(
        &mut self,
        stream_id: &str,
        sel: TrackSel,
        _now: Instant,
    ) -> (LocalId, Vec<StreamEvent>) {
        let id = self.next_id;
        self.next_id += 1;
        self.owner.insert(id, stream_id.to_string());
        match self.streams.get_mut(stream_id) {
            None => {
                let mut subs = BTreeMap::new();
                subs.insert(id, sel.clone());
                self.streams.insert(
                    stream_id.to_string(),
                    Agg {
                        subs,
                        upstream: sel.clone(),
                        linger_until: None,
                        configs: BTreeMap::new(),
                        gop: Gop::default(),
                    },
                );
                self.actions.push_back(AggAction::Open {
                    stream_id: stream_id.to_string(),
                    tracks: sel,
                });
                (id, Vec::new())
            }
            Some(a) => {
                a.linger_until = None;
                a.subs.insert(id, sel.clone());
                let initial = a.initial(&sel);
                self.update_union(stream_id);
                (id, initial)
            }
        }
    }

    fn update_union(&mut self, stream_id: &str) {
        let Some(a) = self.streams.get_mut(stream_id) else {
            return;
        };
        if a.subs.is_empty() {
            return; // linger の間は今の層のまま
        }
        let u = a.union();
        if u != a.upstream {
            a.upstream = u.clone();
            self.actions.push_back(AggAction::Update {
                stream_id: stream_id.to_string(),
                tracks: u,
            });
        }
    }

    /// 購読者の層を変える。返り値は新しく足したトラックの先に渡す物（キャッシュにあれば）。
    pub fn set_subscriber_tracks(&mut self, id: LocalId, sel: TrackSel) -> Vec<StreamEvent> {
        let Some(sid) = self.owner.get(&id).cloned() else {
            return Vec::new();
        };
        let Some(a) = self.streams.get_mut(&sid) else {
            return Vec::new();
        };
        let Some(prev) = a.subs.insert(id, sel.clone()) else {
            return Vec::new();
        };
        let added: Vec<TrackId> = a
            .configs
            .keys()
            .copied()
            .filter(|t| sel.contains(*t) && !prev.contains(*t))
            .collect();
        let initial = if added.is_empty() {
            Vec::new()
        } else {
            a.initial(&TrackSel::only(added))
        };
        self.update_union(&sid);
        initial
    }

    /// 購読者を外す。最後の 1 人なら linger を始める。
    pub fn remove_subscriber(&mut self, id: LocalId, now: Instant) {
        let Some(sid) = self.owner.remove(&id) else {
            return;
        };
        let Some(a) = self.streams.get_mut(&sid) else {
            return;
        };
        a.subs.remove(&id);
        if a.subs.is_empty() {
            if self.cfg.linger.is_zero() {
                self.streams.remove(&sid);
                self.actions.push_back(AggAction::Close { stream_id: sid });
            } else {
                a.linger_until = Some(now + self.cfg.linger);
            }
        } else {
            self.update_union(&sid);
        }
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        self.streams.values().filter_map(|a| a.linger_until).min()
    }

    /// linger の切れた上流を閉じる。
    pub fn handle_timeout(&mut self, now: Instant) {
        let done: Vec<String> = self
            .streams
            .iter()
            .filter(|(_, a)| a.linger_until.is_some_and(|t| t <= now))
            .map(|(k, _)| k.clone())
            .collect();
        for sid in done {
            self.streams.remove(&sid);
            self.actions.push_back(AggAction::Close { stream_id: sid });
        }
    }

    /// 上流から届いた物を記録し、配り先を `out` に入れる（`out` は先に空にする）。
    /// `Lost` は配らない（統合側が別のノードから購読し直し、[`upstream_reset`](Self::upstream_reset) を呼ぶ）。
    pub fn on_upstream(&mut self, stream_id: &str, ev: &StreamEvent, out: &mut Vec<LocalId>) {
        out.clear();
        let cfg = &self.cfg;
        let Some(a) = self.streams.get_mut(stream_id) else {
            return;
        };
        match ev {
            StreamEvent::Config(t, c) => {
                a.configs.insert(*t, c.clone());
                out.extend(
                    a.subs
                        .iter()
                        .filter(|(_, s)| s.contains(*t))
                        .map(|(id, _)| *id),
                );
            }
            StreamEvent::Au(au) => {
                let hv = a.has_video();
                a.gop.push(au, hv, cfg);
                out.extend(
                    a.subs
                        .iter()
                        .filter(|(_, s)| s.contains(au.track))
                        .map(|(id, _)| *id),
                );
            }
            StreamEvent::Discontinuity | StreamEvent::End => {
                a.gop.clear();
                out.extend(a.subs.keys().copied());
            }
            StreamEvent::Segment { .. } => out.extend(a.subs.keys().copied()),
            StreamEvent::Lost(_) => {}
        }
    }

    /// 上流を別のノードへ張り替えた（または失った）。キャッシュを捨て、不連続を配る相手を `out` に入れる。
    pub fn upstream_reset(&mut self, stream_id: &str, out: &mut Vec<LocalId>) {
        out.clear();
        if let Some(a) = self.streams.get_mut(stream_id) {
            a.gop.clear();
            out.extend(a.subs.keys().copied());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use senca_core::{Codec, Timebase};

    fn au(track: u32, codec: Codec, seq: u64, key: bool) -> AccessUnit {
        AccessUnit {
            track: TrackId(track),
            codec,
            epoch: 0,
            sequence: seq,
            config_generation: 1,
            pts: seq as i64,
            dts: seq as i64,
            duration: 0,
            timebase: Timebase::MILLIS,
            keyframe: key,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from_static(b"x"),
        }
    }

    fn cfg(codec: Codec) -> CodecConfig {
        CodecConfig {
            codec,
            generation: 1,
            record: Bytes::new(),
            video: None,
            audio: None,
        }
    }

    fn actions(a: &mut Aggregator) -> Vec<AggAction> {
        let mut v = Vec::new();
        while let Some(x) = a.poll_action() {
            v.push(x);
        }
        v
    }

    #[test]
    fn one_upstream_for_many_subscribers_with_linger() {
        let t0 = Instant::now();
        let mut g = Aggregator::new(AggregatorConfig::default());
        let (a, init) = g.add_subscriber("s", TrackSel::All, t0);
        assert!(init.is_empty());
        let (b, _) = g.add_subscriber("s", TrackSel::All, t0);
        let (_c, _) = g.add_subscriber("s", TrackSel::All, t0);
        assert_eq!(
            actions(&mut g),
            vec![AggAction::Open {
                stream_id: "s".into(),
                tracks: TrackSel::All
            }]
        );
        let mut out = Vec::new();
        g.on_upstream("s", &StreamEvent::Au(au(1, Codec::Aac, 0, true)), &mut out);
        assert_eq!(out.len(), 3);
        g.remove_subscriber(a, t0);
        g.remove_subscriber(b, t0);
        let ids: Vec<_> = g.subscribers("s").collect();
        g.remove_subscriber(ids[0], t0);
        assert!(actions(&mut g).is_empty());
        assert_eq!(g.poll_timeout(), Some(t0 + Duration::from_secs(5)));
        // linger の間に戻ってきた
        let (d, _) = g.add_subscriber("s", TrackSel::All, t0 + Duration::from_secs(1));
        g.handle_timeout(t0 + Duration::from_secs(6));
        assert!(actions(&mut g).is_empty());
        g.remove_subscriber(d, t0 + Duration::from_secs(7));
        g.handle_timeout(t0 + Duration::from_secs(12));
        assert_eq!(
            actions(&mut g),
            vec![AggAction::Close {
                stream_id: "s".into()
            }]
        );
        assert_eq!(g.stats(), AggStats::default());
    }

    #[test]
    fn late_joiner_gets_gop_per_layer() {
        let t0 = Instant::now();
        let mut g = Aggregator::new(AggregatorConfig::default());
        let (_a, _) = g.add_subscriber("s", TrackSel::All, t0);
        let mut out = Vec::new();
        for (t, c) in [(0, Codec::H264), (1, Codec::H264), (2, Codec::Aac)] {
            g.on_upstream("s", &StreamEvent::Config(TrackId(t), cfg(c)), &mut out);
        }
        // 層 0 は seq 3 がキー、層 1 は seq 5 がキー
        for i in 0..8u64 {
            g.on_upstream(
                "s",
                &StreamEvent::Au(au(0, Codec::H264, i, i == 0 || i == 3)),
                &mut out,
            );
            g.on_upstream(
                "s",
                &StreamEvent::Au(au(1, Codec::H264, i, i == 5)),
                &mut out,
            );
            g.on_upstream("s", &StreamEvent::Au(au(2, Codec::Aac, i, true)), &mut out);
        }
        let (_b, init) = g.add_subscriber("s", TrackSel::only([TrackId(0), TrackId(2)]), t0);
        let mut v0 = Vec::new();
        let mut aud = Vec::new();
        assert!(matches!(init[0], StreamEvent::Config(TrackId(0), _)));
        assert!(matches!(init[1], StreamEvent::Config(TrackId(2), _)));
        for e in &init[2..] {
            let StreamEvent::Au(a) = e else { panic!() };
            match a.track.0 {
                0 => v0.push(a.sequence),
                2 => aud.push(a.sequence),
                _ => panic!("選んでいない層"),
            }
        }
        assert_eq!(v0, [3, 4, 5, 6, 7]);
        // 音声は最も古い映像の GOP（層 1 の seq 5 は後なので層 0 の seq 3）から
        assert_eq!(aud, [3, 4, 5, 6, 7]);
        let (_c, init) = g.add_subscriber("s", TrackSel::only([TrackId(1)]), t0);
        let seqs: Vec<u64> = init
            .iter()
            .filter_map(|e| {
                if let StreamEvent::Au(a) = e {
                    Some(a.sequence)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(seqs, [5, 6, 7]);
    }

    #[test]
    fn simulcast_layer_union() {
        let t0 = Instant::now();
        let mut g = Aggregator::new(AggregatorConfig::default());
        let (lo, _) = g.add_subscriber("s", TrackSel::only([TrackId(0), TrackId(9)]), t0);
        assert_eq!(
            actions(&mut g),
            vec![AggAction::Open {
                stream_id: "s".into(),
                tracks: TrackSel::only([TrackId(0), TrackId(9)])
            }]
        );
        let (hi, _) = g.add_subscriber("s", TrackSel::only([TrackId(2), TrackId(9)]), t0);
        assert_eq!(
            actions(&mut g),
            vec![AggAction::Update {
                stream_id: "s".into(),
                tracks: TrackSel::only([TrackId(0), TrackId(2), TrackId(9)])
            }]
        );
        // 同じ層の 3 人目では何もしない
        let (_lo2, _) = g.add_subscriber("s", TrackSel::only([TrackId(0), TrackId(9)]), t0);
        assert!(actions(&mut g).is_empty());
        let mut out = Vec::new();
        g.on_upstream("s", &StreamEvent::Au(au(2, Codec::H264, 0, true)), &mut out);
        assert_eq!(out, vec![hi]);
        g.remove_subscriber(hi, t0);
        assert_eq!(
            actions(&mut g),
            vec![AggAction::Update {
                stream_id: "s".into(),
                tracks: TrackSel::only([TrackId(0), TrackId(9)])
            }]
        );
        // 層の切り替え: キャッシュにあればすぐ渡す
        g.on_upstream(
            "s",
            &StreamEvent::Config(TrackId(0), cfg(Codec::H264)),
            &mut out,
        );
        g.on_upstream(
            "s",
            &StreamEvent::Config(TrackId(2), cfg(Codec::H264)),
            &mut out,
        );
        let init = g.set_subscriber_tracks(lo, TrackSel::only([TrackId(2), TrackId(9)]));
        assert!(matches!(
            init.first(),
            Some(StreamEvent::Config(TrackId(2), _))
        ));
        assert_eq!(
            actions(&mut g),
            vec![AggAction::Update {
                stream_id: "s".into(),
                tracks: TrackSel::only([TrackId(0), TrackId(2), TrackId(9)])
            }]
        );
    }

    #[test]
    fn reset_clears_cache() {
        let t0 = Instant::now();
        let mut g = Aggregator::new(AggregatorConfig::default());
        let (a, _) = g.add_subscriber("s", TrackSel::All, t0);
        let mut out = Vec::new();
        g.on_upstream(
            "s",
            &StreamEvent::Config(TrackId(0), cfg(Codec::H264)),
            &mut out,
        );
        g.on_upstream("s", &StreamEvent::Au(au(0, Codec::H264, 0, true)), &mut out);
        g.upstream_reset("s", &mut out);
        assert_eq!(out, vec![a]);
        let (_b, init) = g.add_subscriber("s", TrackSel::All, t0);
        assert_eq!(init.len(), 1); // 設定だけ
    }
}
