//! 送りの順番（接続ごとに 1 つ）。
//!
//! # 方針
//!
//! - チャネル（stream）ごと・トラックごとに FIFO。トラックの中の順番は変えない（GOP を壊さない）。
//!   設定（Config）はトラックの FIFO に入れ、AU との前後を保つ。チャネルの出来事（不連続・終わり・
//!   セグメント）は、それより前に積んだ全トラックの物が出てから出す。
//! - 送る順: 出来事・設定・音声（全チャネルを巡回）→ 映像（キーフレームが先頭のトラック → 差分）→ データ。
//!   映像とデータはチャネル間を deficit round robin（購読の優先度で重み 1〜4）で分け合う。
//! - 上限: キューの合計バイト数と、種類ごとの待ち時間。超えたら低い優先度から捨てる:
//!   データ → 映像の古い GOP（後ろにキーフレームがあるもの）→ 映像の差分（以後次のキーフレームまで捨てる）
//!   → 映像のキーフレーム → 音声。どの段でも、積んだ量が重み当たり最も多いトラックから捨てる（公平）。
//!   映像はいつも「GOP の先頭からの連続」だけを送る（壊れた GOP を送らない。仕様 5.3）。
//! - 捨てた直後の AU には不連続の印をつける。
//! - まとめ: 小さな AU だけが積まれていて、最も古いものが `batch_window` より若ければ待つ（既定 5 ms）。
//! - 帯域の上限: チャネルごと・全体（トークンバケット）。設定と出来事は上限に数えない。

use crate::wire::ChannelEvent;
use senca_core::{AccessUnit, CodecConfig, MediaKind, TrackId};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// 優先度の段（小さいほど先）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    Audio = 0,
    VideoKey = 1,
    VideoDelta = 2,
    Data = 3,
}

impl Class {
    pub fn of(au: &AccessUnit) -> Class {
        match au.codec.kind() {
            MediaKind::Audio => Class::Audio,
            MediaKind::Video if au.keyframe => Class::VideoKey,
            MediaKind::Video => Class::VideoDelta,
            MediaKind::Subtitle | MediaKind::Data => Class::Data,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SchedConfig {
    /// 積める合計のバイト数（payload）。
    pub max_queue_bytes: usize,
    pub max_delay_audio: Duration,
    pub max_delay_video: Duration,
    pub max_delay_data: Duration,
    /// 小さな AU をまとめるために待つ時間（0 なら待たない。その時に積まれている分だけまとめる）。
    pub batch_window: Duration,
    /// これ以下の payload を「小さい」とみなす（まとめる対象）。
    pub batch_item_max: usize,
    /// まとめの枠 1 つの上限。
    pub batch_max_bytes: usize,
    /// DRR の 1 巡の量（重み 1 のとき）。
    pub quantum: usize,
    /// 全体の帯域の上限（bit/s）。
    pub total_rate_bps: Option<u64>,
    /// トークンバケットの深さ（上限の速さでこの時間に送れる量）。
    pub rate_burst: Duration,
}

impl Default for SchedConfig {
    fn default() -> Self {
        SchedConfig {
            max_queue_bytes: 8 << 20,
            max_delay_audio: Duration::from_millis(500),
            max_delay_video: Duration::from_millis(1500),
            max_delay_data: Duration::from_secs(3),
            batch_window: Duration::from_millis(5),
            batch_item_max: 1200,
            batch_max_bytes: 32 << 10,
            quantum: 16 << 10,
            total_rate_bps: None,
            rate_burst: Duration::from_millis(50),
        }
    }
}

/// 取り出したもの。
#[derive(Debug, Clone)]
pub enum Out {
    Event {
        channel: u32,
        event: ChannelEvent,
    },
    Config {
        channel: u32,
        track: TrackId,
        config: CodecConfig,
    },
    /// `gap`: この AU の前を捨てた（不連続の印をつける）。
    Au {
        channel: u32,
        au: AccessUnit,
        gap: bool,
    },
}

impl Out {
    pub fn channel(&self) -> u32 {
        match self {
            Out::Event { channel, .. } | Out::Config { channel, .. } | Out::Au { channel, .. } => {
                *channel
            }
        }
    }
}

/// 計測（配置の係が使う。ADR-0013）。
#[derive(Debug, Clone, Default)]
pub struct SchedStats {
    pub queued_bytes: usize,
    pub queued_items: usize,
    pub sent_aus: u64,
    pub sent_bytes: u64,
    /// 段ごとの捨てた AU の数（[`Class`] の順）。
    pub dropped: [u64; 4],
    pub dropped_bytes: u64,
    /// キューでの待ち時間（指数移動平均、µs）。
    pub queue_delay_avg_us: u64,
    /// 前回の読み出しからの最大の待ち時間（µs）。
    pub queue_delay_max_us: u64,
    /// 次のキーフレームを待っているトラックの数。
    pub tracks_waiting_key: usize,
}

/// チャネルごとの計測。
#[derive(Debug, Clone, Copy, Default)]
pub struct ChannelStats {
    pub queued_bytes: usize,
    pub sent_aus: u64,
    pub sent_bytes: u64,
    pub dropped: u64,
    pub dropped_bytes: u64,
}

#[derive(Debug)]
enum Entry {
    Au(AccessUnit),
    Config(CodecConfig),
}

#[derive(Debug)]
struct QItem {
    cseq: u64,
    enq: Instant,
    /// 前を捨てた。
    gap: bool,
    e: Entry,
}

impl QItem {
    fn size(&self) -> usize {
        match &self.e {
            Entry::Au(a) => a.payload.len(),
            Entry::Config(_) => 0,
        }
    }
    fn au(&self) -> Option<&AccessUnit> {
        match &self.e {
            Entry::Au(a) => Some(a),
            Entry::Config(_) => None,
        }
    }
    fn is_key(&self) -> bool {
        self.au().is_some_and(|a| a.keyframe)
    }
    /// 出来事・設定・音声は「急ぎ」（全チャネルを巡回して先に出す）。
    fn urgent(&self) -> bool {
        match &self.e {
            Entry::Config(_) => true,
            Entry::Au(a) => a.codec.kind() == MediaKind::Audio,
        }
    }
}

#[derive(Debug)]
struct TrackQ {
    track: TrackId,
    kind: Option<MediaKind>,
    q: VecDeque<QItem>,
    bytes: usize,
    /// 次のキーフレームまで映像を捨てる。
    need_key: bool,
    /// 次に積む AU に不連続の印をつける。
    gap_pending: bool,
}

impl TrackQ {
    fn is_video(&self) -> bool {
        self.kind == Some(MediaKind::Video)
    }
}

#[derive(Debug)]
struct Bucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn new(bps: u64, burst: Duration, now: Instant) -> Bucket {
        let rate = bps as f64 / 8.0;
        let burst = (rate * burst.as_secs_f64()).max(1500.0);
        Bucket {
            rate,
            burst,
            tokens: burst,
            last: now,
        }
    }
    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + self.rate * dt).min(self.burst);
        self.last = now;
    }
    fn ready(&self) -> bool {
        self.tokens > 0.0
    }
    fn ready_at(&self) -> Instant {
        if self.tokens > 0.0 {
            return self.last;
        }
        self.last + Duration::from_secs_f64((-self.tokens + 1.0) / self.rate)
    }
}

#[derive(Debug)]
struct ChanQ {
    tracks: Vec<TrackQ>,
    events: VecDeque<(u64, ChannelEvent)>,
    next_cseq: u64,
    weight: usize,
    deficit: usize,
    visited: bool,
    in_urgent: bool,
    in_drr: bool,
    /// 輪の中の古い項目を見分ける番号。
    urgent_gen: u32,
    drr_gen: u32,
    throttled: bool,
    bucket: Option<Bucket>,
    bytes: usize,
    stats: ChannelStats,
}

impl ChanQ {
    fn min_event(&self) -> u64 {
        self.events.front().map_or(u64::MAX, |e| e.0)
    }
    fn event_ready(&self) -> bool {
        let Some(&(c, _)) = self.events.front() else {
            return false;
        };
        self.tracks
            .iter()
            .all(|t| t.q.front().is_none_or(|h| h.cseq > c))
    }
    /// 急ぎの先頭（出来事を除く）のうち最も古いもの。`rate_ok` が偽なら音声は出せない。
    fn urgent_head(&self, rate_ok: bool) -> Option<usize> {
        let lim = self.min_event();
        let mut best: Option<(u64, usize)> = None;
        for (i, t) in self.tracks.iter().enumerate() {
            if let Some(h) = t.q.front()
                && h.cseq < lim
                && h.urgent()
                && (rate_ok || matches!(h.e, Entry::Config(_)))
                && best.is_none_or(|b| h.cseq < b.0)
            {
                best = Some((h.cseq, i));
            }
        }
        best.map(|b| b.1)
    }
    /// 映像・データの先頭のうち、段（キーフレーム < 差分 < データ）が高く、古いもの。
    fn normal_head(&self) -> Option<usize> {
        let lim = self.min_event();
        let mut best: Option<(Class, u64, usize)> = None;
        for (i, t) in self.tracks.iter().enumerate() {
            if let Some(h) = t.q.front()
                && h.cseq < lim
                && let Some(a) = h.au()
                && !h.urgent()
            {
                let k = (Class::of(a), h.cseq, i);
                if best.is_none_or(|b| (k.0, k.1) < (b.0, b.1)) {
                    best = Some(k);
                }
            }
        }
        best.map(|b| b.2)
    }
    fn has_urgent(&self) -> bool {
        self.event_ready() || self.urgent_head(true).is_some()
    }
    fn has_normal(&self) -> bool {
        self.normal_head().is_some()
    }
    fn rate_ok(&self) -> bool {
        self.bucket.as_ref().is_none_or(|b| b.ready())
    }
}

/// 重み当たりの量が最も多いトラック。
fn pick_track(chans: &HashMap<u32, ChanQ>, f: &dyn Fn(&TrackQ) -> usize) -> Option<(u32, usize)> {
    let mut best: Option<(usize, u32, usize)> = None;
    for (&ch, c) in chans {
        for (i, t) in c.tracks.iter().enumerate() {
            let b = f(t);
            if b > 0 {
                let score = b / c.weight;
                if best.is_none_or(|x| score > x.0) {
                    best = Some((score, ch, i));
                }
            }
        }
    }
    best.map(|b| (b.1, b.2))
}

/// 接続 1 本分の送りの順番。
#[derive(Debug)]
pub struct Scheduler {
    cfg: SchedConfig,
    chans: HashMap<u32, ChanQ>,
    urgent: VecDeque<(u32, u32)>,
    drr: VecDeque<(u32, u32)>,
    throttled: Vec<(Instant, u32)>,
    total: Option<Bucket>,
    bytes: usize,
    items: usize,
    /// まとめの判断のための数。
    big_items: usize,
    small_items: usize,
    small_bytes: usize,
    oldest_small: Option<Instant>,
    last_expire: Option<Instant>,
    stats: SchedStats,
    /// キーフレームが要る（捨てたので次のキーフレームを待っている）トラック。統合側が送り手に頼む。
    key_wanted: Vec<(u32, TrackId)>,
}

const EXPIRE_INTERVAL: Duration = Duration::from_millis(10);

impl Scheduler {
    pub fn new(cfg: SchedConfig, now: Instant) -> Scheduler {
        let total = cfg
            .total_rate_bps
            .map(|r| Bucket::new(r, cfg.rate_burst, now));
        Scheduler {
            cfg,
            chans: HashMap::new(),
            urgent: VecDeque::new(),
            drr: VecDeque::new(),
            throttled: Vec::new(),
            total,
            bytes: 0,
            items: 0,
            big_items: 0,
            small_items: 0,
            small_bytes: 0,
            oldest_small: None,
            last_expire: None,
            stats: SchedStats::default(),
            key_wanted: Vec::new(),
        }
    }

    pub fn config(&self) -> &SchedConfig {
        &self.cfg
    }

    /// チャネルを作る（`priority` 0〜255、高いほど帯域を多く分ける）。`rate_bps` はチャネルの帯域の上限。
    pub fn add_channel(&mut self, channel: u32, priority: u8, rate_bps: Option<u64>, now: Instant) {
        let burst = self.cfg.rate_burst;
        self.chans.entry(channel).or_insert_with(|| ChanQ {
            tracks: Vec::new(),
            events: VecDeque::new(),
            next_cseq: 0,
            weight: 1 + priority as usize / 64,
            deficit: 0,
            visited: false,
            in_urgent: false,
            in_drr: false,
            urgent_gen: 0,
            drr_gen: 0,
            throttled: false,
            bucket: rate_bps.map(|r| Bucket::new(r, burst, now)),
            bytes: 0,
            stats: ChannelStats::default(),
        });
    }

    pub fn set_channel_rate(&mut self, channel: u32, rate_bps: Option<u64>, now: Instant) {
        let burst = self.cfg.rate_burst;
        if let Some(c) = self.chans.get_mut(&channel) {
            c.bucket = rate_bps.map(|r| Bucket::new(r, burst, now));
        }
    }

    pub fn set_total_rate(&mut self, rate_bps: Option<u64>, now: Instant) {
        self.cfg.total_rate_bps = rate_bps;
        self.total = rate_bps.map(|r| Bucket::new(r, self.cfg.rate_burst, now));
    }

    /// チャネルを消す（積んだ物は捨てる。数えない）。
    pub fn remove_channel(&mut self, channel: u32) {
        if let Some(c) = self.chans.remove(&channel) {
            for t in c.tracks {
                for it in t.q {
                    self.forget(&it);
                }
            }
            self.big_items -= c.events.len();
            self.items -= c.events.len();
        }
        self.key_wanted.retain(|k| k.0 != channel);
    }

    pub fn has_channel(&self, channel: u32) -> bool {
        self.chans.contains_key(&channel)
    }

    fn track_mut(c: &mut ChanQ, track: TrackId) -> &mut TrackQ {
        let i = match c.tracks.iter().position(|t| t.track == track) {
            Some(i) => i,
            None => {
                c.tracks.push(TrackQ {
                    track,
                    kind: None,
                    q: VecDeque::new(),
                    bytes: 0,
                    need_key: false,
                    gap_pending: false,
                });
                c.tracks.len() - 1
            }
        };
        &mut c.tracks[i]
    }

    /// トラックを外す（積んだ物は捨てる。数えない）。
    pub fn remove_track(&mut self, channel: u32, track: TrackId) {
        let Some(c) = self.chans.get_mut(&channel) else {
            return;
        };
        let Some(i) = c.tracks.iter().position(|t| t.track == track) else {
            return;
        };
        let t = c.tracks.remove(i);
        c.bytes -= t.bytes;
        for it in t.q {
            self.forget(&it);
        }
    }

    /// 次のキーフレームから送る（層を足したとき・受け手が取りこぼしたとき）。
    pub fn require_key(&mut self, channel: u32, track: TrackId) {
        if let Some(c) = self.chans.get_mut(&channel) {
            let t = Self::track_mut(c, track);
            if t.kind != Some(MediaKind::Video) && t.kind.is_some() {
                return;
            }
            // 積んである差分は前の続きなので、そのままでよい。次に積む差分から捨てる
            t.need_key = true;
        }
    }

    /// 捨てたので次のキーフレームが欲しいトラック（統合側が送り手へ頼む）。
    pub fn take_key_wanted(&mut self) -> Vec<(u32, TrackId)> {
        std::mem::take(&mut self.key_wanted)
    }

    fn account_push(&mut self, it: &QItem) {
        let s = it.size();
        self.bytes += s;
        self.items += 1;
        if matches!(it.e, Entry::Config(_)) || s > self.cfg.batch_item_max {
            self.big_items += 1;
        } else {
            self.small_items += 1;
            self.small_bytes += s;
            if self.oldest_small.is_none() {
                self.oldest_small = Some(it.enq);
            }
        }
    }

    fn forget(&mut self, it: &QItem) {
        let s = it.size();
        self.bytes -= s;
        self.items -= 1;
        if matches!(it.e, Entry::Config(_)) || s > self.cfg.batch_item_max {
            self.big_items -= 1;
        } else {
            self.small_items -= 1;
            self.small_bytes -= s;
            if self.small_items == 0 {
                self.oldest_small = None;
            }
        }
    }

    /// 設定を積む（捨てない）。
    pub fn push_config(&mut self, channel: u32, track: TrackId, config: CodecConfig, now: Instant) {
        let Some(c) = self.chans.get_mut(&channel) else {
            return;
        };
        let cseq = c.next_cseq;
        c.next_cseq += 1;
        let kind = config.codec.kind();
        let t = Self::track_mut(c, track);
        if t.kind.is_none() {
            t.kind = Some(kind);
            if kind == MediaKind::Video {
                t.need_key = true; // 最初はキーフレームから
            }
        }
        let it = QItem {
            cseq,
            enq: now,
            gap: false,
            e: Entry::Config(config),
        };
        self.account_push(&it);
        let c = self.chans.get_mut(&channel).expect("exists");
        Self::track_mut(c, track).q.push_back(it);
        self.mark(channel);
    }

    /// 出来事を積む（捨てない）。
    pub fn push_event(&mut self, channel: u32, event: ChannelEvent, _now: Instant) {
        let Some(c) = self.chans.get_mut(&channel) else {
            return;
        };
        let cseq = c.next_cseq;
        c.next_cseq += 1;
        c.events.push_back((cseq, event));
        self.items += 1;
        self.big_items += 1;
        self.mark(channel);
    }

    /// AU を積む。積まずに捨てたら false。
    pub fn push_au(&mut self, channel: u32, au: AccessUnit, now: Instant) -> bool {
        self.expire(now, false);
        let Some(c) = self.chans.get_mut(&channel) else {
            return false;
        };
        let kind = au.codec.kind();
        let size = au.payload.len();
        let cseq = c.next_cseq;
        let t = Self::track_mut(c, au.track);
        if t.kind.is_none() {
            t.kind = Some(kind);
            if kind == MediaKind::Video {
                t.need_key = true;
            }
        }
        if kind == MediaKind::Video {
            if au.keyframe {
                t.need_key = false;
            } else if t.need_key {
                t.gap_pending = true;
                c.stats.dropped += 1;
                c.stats.dropped_bytes += size as u64;
                self.stats.dropped[Class::VideoDelta as usize] += 1;
                self.stats.dropped_bytes += size as u64;
                return false;
            }
        }
        let gap = std::mem::take(&mut t.gap_pending);
        let track = au.track;
        let it = QItem {
            cseq,
            enq: now,
            gap,
            e: Entry::Au(au),
        };
        self.account_push(&it);
        let c = self.chans.get_mut(&channel).expect("exists");
        c.next_cseq += 1;
        c.bytes += size;
        let t = Self::track_mut(c, track);
        t.bytes += size;
        t.q.push_back(it);
        self.mark(channel);
        let mut guard = 0;
        while self.bytes > self.cfg.max_queue_bytes && guard < 10_000 {
            guard += 1;
            if !self.shed_one() {
                break;
            }
        }
        true
    }

    /// チャネルを巡回の輪に入れる（遅延して確かめるので、余分に入っていてもよい）。
    fn mark(&mut self, channel: u32) {
        let Some(c) = self.chans.get_mut(&channel) else {
            return;
        };
        if c.throttled {
            return;
        }
        if !c.in_urgent && c.has_urgent() {
            c.in_urgent = true;
            c.urgent_gen = c.urgent_gen.wrapping_add(1);
            self.urgent.push_back((channel, c.urgent_gen));
        }
        if !c.in_drr && c.has_normal() {
            c.in_drr = true;
            c.drr_gen = c.drr_gen.wrapping_add(1);
            self.drr.push_back((channel, c.drr_gen));
        }
    }

    pub fn queued_bytes(&self) -> usize {
        self.bytes
    }

    pub fn queued_items(&self) -> usize {
        self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items == 0
    }

    pub fn channel_stats(&self, channel: u32) -> Option<ChannelStats> {
        self.chans.get(&channel).map(|c| ChannelStats {
            queued_bytes: c.bytes,
            ..c.stats
        })
    }

    /// 計測を読む（最大の待ち時間はここで 0 に戻す）。
    pub fn take_stats(&mut self) -> SchedStats {
        let mut s = self.stats.clone();
        s.queued_bytes = self.bytes;
        s.queued_items = self.items;
        s.tracks_waiting_key = self
            .chans
            .values()
            .flat_map(|c| c.tracks.iter())
            .filter(|t| t.need_key)
            .count();
        self.stats.queue_delay_max_us = 0;
        s
    }

    pub fn stats(&self) -> SchedStats {
        let mut s = self.stats.clone();
        s.queued_bytes = self.bytes;
        s.queued_items = self.items;
        s.tracks_waiting_key = self
            .chans
            .values()
            .flat_map(|c| c.tracks.iter())
            .filter(|t| t.need_key)
            .count();
        s
    }

    /// 小さな AU をまとめるために、いつまで待つか（待たないなら None）。
    pub fn hold_until(&self, now: Instant) -> Option<Instant> {
        if self.cfg.batch_window.is_zero()
            || self.big_items > 0
            || self.small_items == 0
            || self.small_bytes >= self.cfg.batch_max_bytes
        {
            return None;
        }
        let d = self.oldest_small? + self.cfg.batch_window;
        (d > now).then_some(d)
    }

    /// 次に呼ぶべき時刻（まとめの待ち・帯域の上限の回復）。
    pub fn next_wakeup(&self, now: Instant) -> Option<Instant> {
        if self.items == 0 {
            return None;
        }
        let mut w = self.hold_until(now);
        for &(t, _) in &self.throttled {
            w = Some(w.map_or(t, |x| x.min(t)));
        }
        if let Some(b) = &self.total
            && !b.ready()
        {
            let t = b.ready_at();
            w = Some(w.map_or(t, |x| x.min(t)));
        }
        w
    }

    /// 次に送る物。帯域の上限・まとめの待ちで出せなければ None（[`next_wakeup`](Self::next_wakeup) に呼び直す）。
    pub fn pop(&mut self, now: Instant) -> Option<Out> {
        if self.items == 0 {
            return None;
        }
        self.expire(now, false);
        if let Some(b) = self.total.as_mut() {
            b.refill(now);
            if !b.ready() {
                return None;
            }
        }
        if !self.throttled.is_empty() {
            let mut back = Vec::new();
            self.throttled.retain(|&(t, ch)| {
                if t <= now {
                    back.push(ch);
                    false
                } else {
                    true
                }
            });
            for ch in back {
                if let Some(c) = self.chans.get_mut(&ch) {
                    c.throttled = false;
                    if let Some(b) = c.bucket.as_mut() {
                        b.refill(now);
                    }
                }
                self.mark(ch);
            }
        }
        let out = self.pop_urgent(now).or_else(|| self.pop_drr(now))?;
        if let Some(b) = self.total.as_mut()
            && let Out::Au { au, .. } = &out
        {
            b.tokens -= au.payload.len() as f64;
        }
        Some(out)
    }

    fn throttle(&mut self, ch: u32) {
        let Some(c) = self.chans.get_mut(&ch) else {
            return;
        };
        if c.throttled {
            return;
        }
        let at = c
            .bucket
            .as_ref()
            .map(|b| b.ready_at())
            .unwrap_or_else(Instant::now);
        c.throttled = true;
        c.in_urgent = false;
        c.in_drr = false;
        self.throttled.push((at, ch));
    }

    fn pop_urgent(&mut self, now: Instant) -> Option<Out> {
        let n = self.urgent.len();
        for _ in 0..n {
            let (ch, g) = self.urgent.pop_front()?;
            let Some(c) = self.chans.get_mut(&ch) else {
                continue;
            };
            if c.throttled || !c.in_urgent || c.urgent_gen != g {
                continue;
            }
            if let Some(b) = c.bucket.as_mut() {
                b.refill(now);
            }
            if c.event_ready() {
                let (_, event) = c.events.pop_front().expect("ready");
                self.items -= 1;
                self.big_items -= 1;
                c.in_urgent = false;
                self.mark(ch);
                return Some(Out::Event { channel: ch, event });
            }
            let rate_ok = c.rate_ok();
            match c.urgent_head(rate_ok) {
                Some(i) => {
                    c.in_urgent = false;
                    let out = self.take(ch, i, now);
                    self.mark(ch);
                    return Some(out);
                }
                None => {
                    c.in_urgent = false;
                    if c.urgent_head(true).is_some() {
                        // 帯域の上限で止まっている
                        self.throttle(ch);
                    }
                }
            }
        }
        None
    }

    fn pop_drr(&mut self, now: Instant) -> Option<Out> {
        loop {
            let &(ch, g) = self.drr.front()?;
            let quantum = self.cfg.quantum;
            let Some(c) = self.chans.get_mut(&ch) else {
                self.drr.pop_front();
                continue;
            };
            if c.throttled || !c.in_drr || c.drr_gen != g {
                self.drr.pop_front();
                continue;
            }
            let Some(i) = c.normal_head() else {
                c.in_drr = false;
                c.deficit = 0;
                c.visited = false;
                self.drr.pop_front();
                continue;
            };
            if let Some(b) = c.bucket.as_mut() {
                b.refill(now);
            }
            if !c.rate_ok() {
                c.deficit = 0;
                c.visited = false;
                self.drr.pop_front();
                self.throttle(ch);
                continue;
            }
            if !c.visited {
                c.visited = true;
                c.deficit += quantum * c.weight;
            }
            let size = c.tracks[i].q.front().map_or(0, |h| h.size());
            if size <= c.deficit {
                c.deficit -= size;
                let out = self.take(ch, i, now);
                // 続けて同じチャネル（deficit が残る限り）。空なら輪から外す
                let c = self.chans.get_mut(&ch).expect("exists");
                if !c.has_normal() {
                    c.in_drr = false;
                    c.deficit = 0;
                    c.visited = false;
                    self.drr.pop_front();
                }
                self.mark(ch);
                return Some(out);
            }
            c.visited = false;
            self.drr.rotate_left(1);
        }
    }

    /// トラック i の先頭を取り出す。
    fn take(&mut self, ch: u32, i: usize, now: Instant) -> Out {
        let c = self.chans.get_mut(&ch).expect("exists");
        let t = &mut c.tracks[i];
        let it = t.q.pop_front().expect("head");
        let s = it.size();
        t.bytes -= s;
        c.bytes -= s;
        if let Some(b) = c.bucket.as_mut()
            && s > 0
        {
            b.tokens -= s as f64;
        }
        let track = t.track;
        if s > 0 {
            c.stats.sent_aus += 1;
            c.stats.sent_bytes += s as u64;
        }
        self.forget(&it);
        // oldest_small は進めない（実際より古いと、まとめの待ちが短くなるだけで安全）
        let delay = now.saturating_duration_since(it.enq).as_micros() as u64;
        match it.e {
            Entry::Config(config) => Out::Config {
                channel: ch,
                track,
                config,
            },
            Entry::Au(au) => {
                self.stats.sent_aus += 1;
                self.stats.sent_bytes += s as u64;
                let avg = self.stats.queue_delay_avg_us;
                self.stats.queue_delay_avg_us = if avg == 0 {
                    delay
                } else {
                    avg - avg / 16 + delay / 16
                };
                self.stats.queue_delay_max_us = self.stats.queue_delay_max_us.max(delay);
                Out::Au {
                    channel: ch,
                    au,
                    gap: it.gap,
                }
            }
        }
    }

    fn count_drop(&mut self, ch: u32, it: &QItem) {
        let Some(a) = it.au() else { return };
        let s = a.payload.len() as u64;
        self.stats.dropped[Class::of(a) as usize] += 1;
        self.stats.dropped_bytes += s;
        if let Some(c) = self.chans.get_mut(&ch) {
            c.stats.dropped += 1;
            c.stats.dropped_bytes += s;
        }
    }

    /// トラックの AU のうち `pred` に合うものを捨てる（設定は残す）。捨てた後ろの物に不連続の印をつける。
    fn drop_where(
        &mut self,
        ch: u32,
        ti: usize,
        mut pred: impl FnMut(usize, &QItem) -> bool,
    ) -> usize {
        let Some(c) = self.chans.get_mut(&ch) else {
            return 0;
        };
        let t = &mut c.tracks[ti];
        let old = std::mem::take(&mut t.q);
        let mut dropped = Vec::new();
        let mut gap = false;
        for (i, it) in old.into_iter().enumerate() {
            if it.au().is_some() && pred(i, &it) {
                gap = true;
                t.bytes -= it.size();
                c.bytes -= it.size();
                dropped.push(it);
            } else {
                let mut it = it;
                if gap && it.au().is_some() {
                    it.gap = true;
                    gap = false;
                }
                t.q.push_back(it);
            }
        }
        if gap {
            t.gap_pending = true;
        }
        let n = dropped.len();
        for it in dropped {
            self.count_drop(ch, &it);
            self.forget(&it);
        }
        n
    }

    /// 映像のトラックを詰める: 古い GOP（後ろにキーフレームがある）を捨てる。無ければ差分を全部捨てて
    /// 次のキーフレームを待つ。`allow_key` ならキーフレームだけの状態からも捨てる。
    fn shed_video(&mut self, ch: u32, ti: usize, allow_key: bool) -> bool {
        let Some(c) = self.chans.get(&ch) else {
            return false;
        };
        let t = &c.tracks[ti];
        let last_key = t.q.iter().rposition(|it| it.is_key());
        let first_au = t.q.iter().position(|it| it.au().is_some());
        if let (Some(k), Some(f)) = (last_key, first_au)
            && f < k
        {
            return self.drop_where(ch, ti, |i, _| i < k) > 0;
        }
        let has_delta = t.q.iter().any(|it| it.au().is_some_and(|a| !a.keyframe));
        let n = if has_delta {
            self.drop_where(ch, ti, |_, it| !it.is_key())
        } else if allow_key {
            self.drop_where(ch, ti, |_, _| true)
        } else {
            return false;
        };
        let c = self.chans.get_mut(&ch).expect("exists");
        let t = &mut c.tracks[ti];
        if !t.need_key {
            t.need_key = true;
            self.key_wanted.push((ch, t.track));
        }
        n > 0
    }

    /// 上限を超えたので 1 段捨てる。捨てる物が無ければ false。
    fn shed_one(&mut self) -> bool {
        // 1. データ
        if let Some((ch, ti)) = pick_track(&self.chans, &|t| {
            if matches!(t.kind, Some(MediaKind::Data | MediaKind::Subtitle)) {
                t.bytes
            } else {
                0
            }
        }) {
            let mut first = true;
            return self.drop_where(ch, ti, |_, _| std::mem::replace(&mut first, false)) > 0;
        }
        // 2. 映像の古い GOP・差分
        let delta_bytes = |t: &TrackQ| {
            if !t.is_video() {
                return 0;
            }
            let has_delta = t.q.iter().any(|it| it.au().is_some_and(|a| !a.keyframe));
            let keys = t.q.iter().filter(|it| it.is_key()).count();
            if has_delta || keys > 1 { t.bytes } else { 0 }
        };
        if let Some((ch, ti)) = pick_track(&self.chans, &delta_bytes) {
            return self.shed_video(ch, ti, false);
        }
        // 3. 映像のキーフレーム
        if let Some((ch, ti)) = pick_track(&self.chans, &|t| if t.is_video() { t.bytes } else { 0 })
        {
            return self.shed_video(ch, ti, true);
        }
        // 4. 音声（古いものから）
        if let Some((ch, ti)) = pick_track(&self.chans, &|t| {
            if t.kind == Some(MediaKind::Audio) {
                t.bytes
            } else {
                0
            }
        }) {
            let mut first = true;
            return self.drop_where(ch, ti, |_, _| std::mem::replace(&mut first, false)) > 0;
        }
        false
    }

    /// 待ち時間の上限を超えた物を捨てる。
    fn expire(&mut self, now: Instant, force: bool) {
        if self.items == 0 {
            return;
        }
        if !force
            && self
                .last_expire
                .is_some_and(|l| now.saturating_duration_since(l) < EXPIRE_INTERVAL)
        {
            return;
        }
        self.last_expire = Some(now);
        let (la, lv, ld) = (
            self.cfg.max_delay_audio,
            self.cfg.max_delay_video,
            self.cfg.max_delay_data,
        );
        let mut todo: Vec<(u32, usize, MediaKind)> = Vec::new();
        for (&ch, c) in &self.chans {
            for (i, t) in c.tracks.iter().enumerate() {
                let Some(kind) = t.kind else { continue };
                let lim = match kind {
                    MediaKind::Audio => la,
                    MediaKind::Video => lv,
                    _ => ld,
                };
                if t.q
                    .iter()
                    .find(|it| it.au().is_some())
                    .is_some_and(|h| now.saturating_duration_since(h.enq) > lim)
                {
                    todo.push((ch, i, kind));
                }
            }
        }
        for (ch, ti, kind) in todo {
            let lim = match kind {
                MediaKind::Audio => la,
                MediaKind::Video => lv,
                _ => ld,
            };
            if kind == MediaKind::Video {
                // 古い GOP を捨て、それでも古ければ全部捨てて次のキーフレームを待つ
                for _ in 0..3 {
                    let Some(c) = self.chans.get(&ch) else { break };
                    let t = &c.tracks[ti];
                    if !t
                        .q
                        .iter()
                        .find(|it| it.au().is_some())
                        .is_some_and(|h| now.saturating_duration_since(h.enq) > lim)
                    {
                        break;
                    }
                    if !self.shed_video(ch, ti, true) {
                        break;
                    }
                }
            } else {
                self.drop_where(ch, ti, |_, it| now.saturating_duration_since(it.enq) > lim);
            }
        }
    }

    /// 待ち時間の上限を今すぐ確かめる（試験用）。
    pub fn expire_now(&mut self, now: Instant) {
        self.expire(now, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use senca_core::{Codec, Timebase};

    fn au(track: u32, codec: Codec, seq: u64, key: bool, len: usize) -> AccessUnit {
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
            payload: Bytes::from(vec![0u8; len]),
        }
    }

    fn cfg() -> SchedConfig {
        SchedConfig {
            batch_window: Duration::ZERO,
            ..Default::default()
        }
    }

    fn drain(s: &mut Scheduler, now: Instant) -> Vec<Out> {
        let mut v = Vec::new();
        while let Some(o) = s.pop(now) {
            v.push(o);
        }
        v
    }

    #[test]
    fn audio_first_and_track_order_kept() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(cfg(), t0);
        s.add_channel(1, 128, None, t0);
        for i in 0..10 {
            s.push_au(1, au(0, Codec::H264, i, i == 0, 5000), t0);
            s.push_au(1, au(1, Codec::Aac, i, true, 300), t0);
        }
        let out = drain(&mut s, t0);
        assert_eq!(out.len(), 20);
        // 音声が全部先
        for o in &out[..10] {
            let Out::Au { au, .. } = o else { panic!() };
            assert_eq!(au.codec, Codec::Aac);
        }
        let seqs: Vec<u64> = out[10..]
            .iter()
            .map(|o| {
                if let Out::Au { au, .. } = o {
                    au.sequence
                } else {
                    99
                }
            })
            .collect();
        assert_eq!(seqs, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn video_starts_at_keyframe() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(cfg(), t0);
        s.add_channel(1, 0, None, t0);
        assert!(!s.push_au(1, au(0, Codec::H264, 0, false, 100), t0));
        assert!(s.push_au(1, au(0, Codec::H264, 1, true, 100), t0));
        assert!(s.push_au(1, au(0, Codec::H264, 2, false, 100), t0));
        let out = drain(&mut s, t0);
        assert_eq!(out.len(), 2);
        let Out::Au { gap, .. } = &out[0] else {
            panic!()
        };
        assert!(*gap);
    }

    #[test]
    fn config_and_events_keep_order() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(cfg(), t0);
        s.add_channel(1, 0, None, t0);
        let c = CodecConfig {
            codec: Codec::H264,
            generation: 1,
            record: Bytes::new(),
            video: None,
            audio: None,
        };
        s.push_config(1, TrackId(0), c.clone(), t0);
        s.push_au(1, au(0, Codec::H264, 0, true, 5000), t0);
        s.push_au(1, au(0, Codec::H264, 1, false, 5000), t0);
        s.push_event(1, ChannelEvent::Discontinuity, t0);
        s.push_config(1, TrackId(0), CodecConfig { generation: 2, ..c }, t0);
        s.push_au(1, au(0, Codec::H264, 2, true, 5000), t0);
        s.push_au(1, au(1, Codec::Aac, 0, true, 10), t0);
        let out = drain(&mut s, t0);
        let names: Vec<String> = out
            .iter()
            .map(|o| match o {
                Out::Event { .. } => "E".to_string(),
                Out::Config { config, .. } => format!("C{}", config.generation),
                Out::Au { au, .. } => format!(
                    "{}{}",
                    if au.codec == Codec::Aac { "A" } else { "V" },
                    au.sequence
                ),
            })
            .collect();
        assert_eq!(names, ["C1", "V0", "V1", "E", "C2", "A0", "V2"]);
    }

    /// 詰まったとき: 音声は残り、映像は GOP の先頭からの連続だけが残る。
    #[test]
    fn congestion_drops_lowest_priority_and_keeps_gops_intact() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(
            SchedConfig {
                max_queue_bytes: 200_000,
                ..cfg()
            },
            t0,
        );
        s.add_channel(1, 0, None, t0);
        // 送らずに積み続ける（10 秒分）
        for i in 0..300u64 {
            let seq = i;
            let key = i.is_multiple_of(30);
            s.push_au(
                1,
                au(0, Codec::H264, seq, key, if key { 40_000 } else { 8_000 }),
                t0,
            );
            if i.is_multiple_of(3) {
                s.push_au(1, au(1, Codec::Aac, i / 3, true, 300), t0);
            }
            assert!(s.queued_bytes() <= 200_000);
        }
        let st = s.stats();
        assert!(st.dropped[Class::VideoDelta as usize] > 0);
        assert_eq!(st.dropped[Class::Audio as usize], 0, "音声は捨てない");
        let out = drain(&mut s, t0);
        let audio = out
            .iter()
            .filter(|o| matches!(o, Out::Au { au, .. } if au.codec == Codec::Aac))
            .count();
        assert_eq!(audio, 100);
        // 映像: キーフレームから連続
        let mut expect_next: Option<u64> = None;
        for o in &out {
            let Out::Au { au, .. } = o else { continue };
            if au.codec != Codec::H264 {
                continue;
            }
            if au.keyframe {
                expect_next = Some(au.sequence + 1);
            } else {
                assert_eq!(Some(au.sequence), expect_next, "壊れた GOP");
                expect_next = Some(au.sequence + 1);
            }
        }
    }

    #[test]
    fn superseded_gop_dropped_first() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(
            SchedConfig {
                max_queue_bytes: 100_000,
                ..cfg()
            },
            t0,
        );
        s.add_channel(1, 0, None, t0);
        for i in 0..10 {
            s.push_au(1, au(0, Codec::H264, i, i == 0, 9_000), t0);
        }
        // 新しいキーフレームで溢れる: 古い GOP を丸ごと捨てる
        s.push_au(1, au(0, Codec::H264, 10, true, 30_000), t0);
        let out = drain(&mut s, t0);
        assert_eq!(out.len(), 1);
        let Out::Au { au, gap, .. } = &out[0] else {
            panic!()
        };
        assert!(au.keyframe && au.sequence == 10 && *gap);
    }

    #[test]
    fn delay_bound_expires_video_until_key() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(cfg(), t0);
        s.add_channel(1, 0, None, t0);
        for i in 0..5 {
            s.push_au(1, au(0, Codec::H264, i, i == 0, 100), t0);
        }
        let t1 = t0 + Duration::from_secs(2);
        s.expire_now(t1);
        assert!(s.is_empty());
        assert!(!s.push_au(1, au(0, Codec::H264, 5, false, 100), t1));
        assert!(s.push_au(1, au(0, Codec::H264, 6, true, 100), t1));
        assert_eq!(s.take_key_wanted(), vec![(1, TrackId(0))]);
    }

    #[test]
    fn drr_fairness_by_weight() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(
            SchedConfig {
                max_queue_bytes: 1 << 30,
                ..cfg()
            },
            t0,
        );
        s.add_channel(1, 0, None, t0); // 重み 1
        s.add_channel(2, 0, None, t0); // 重み 1
        s.add_channel(3, 255, None, t0); // 重み 4
        for ch in 1..=3 {
            for i in 0..400 {
                s.push_au(ch, au(0, Codec::H264, i, i == 0, 4000), t0);
            }
        }
        // 半分ほど送ったところでの割合
        let mut sent = [0usize; 4];
        for _ in 0..600 {
            let Some(Out::Au { channel, au, .. }) = s.pop(t0) else {
                panic!()
            };
            sent[channel as usize] += au.payload.len();
        }
        let r12 = sent[1] as f64 / sent[2] as f64;
        let r31 = sent[3] as f64 / sent[1] as f64;
        assert!((0.9..1.1).contains(&r12), "同じ重みは等分 {sent:?}");
        assert!((3.5..4.5).contains(&r31), "重み 4 は 4 倍 {sent:?}");
    }

    #[test]
    fn channel_rate_cap() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(
            SchedConfig {
                max_queue_bytes: 1 << 30,
                max_delay_video: Duration::from_secs(100),
                ..cfg()
            },
            t0,
        );
        s.add_channel(1, 0, Some(800_000), t0); // 100 kB/s
        s.add_channel(2, 0, None, t0);
        for i in 0..1000 {
            s.push_au(1, au(0, Codec::H264, i, i == 0, 1000), t0);
            s.push_au(2, au(0, Codec::H264, i, i == 0, 1000), t0);
        }
        let mut sent = [0usize; 3];
        let mut now = t0;
        for _ in 0..1000 {
            now += Duration::from_millis(1);
            while let Some(o) = s.pop(now) {
                if let Out::Au { channel, au, .. } = o {
                    sent[channel as usize] += au.payload.len();
                }
            }
        }
        // 1 秒で 100 kB（＋バケットの深さ 5 kB）
        assert!((95_000..=110_000).contains(&sent[1]), "{sent:?}");
        assert_eq!(sent[2], 1_000_000);
    }

    #[test]
    fn batching_holds_small_items() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(SchedConfig::default(), t0);
        s.add_channel(1, 0, None, t0);
        s.push_au(1, au(1, Codec::Aac, 0, true, 300), t0);
        assert_eq!(s.hold_until(t0), Some(t0 + Duration::from_millis(5)));
        assert_eq!(s.next_wakeup(t0), Some(t0 + Duration::from_millis(5)));
        assert_eq!(s.hold_until(t0 + Duration::from_millis(5)), None);
        // 大きな物があれば待たない
        s.push_au(1, au(0, Codec::H264, 0, true, 5000), t0);
        assert_eq!(s.hold_until(t0), None);
    }
}
