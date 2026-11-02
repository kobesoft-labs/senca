//! sans-io の segmenter。AU を受け取り、segment・partial segment の境界を決めて出来事を返す。
//!
//! 多重化（TS・fMP4）はしない。呼び出し側は [`SegmentEvent`] を順に受け取り、
//! `SegmentStart` で新しい入れ物を開き、`Au` をその segment（と part）に書き、`PartEnd`・`SegmentEnd`
//! で確定させる。playlist には確定したものだけが載る（仕様 6.9: 途中書込みを完成品として出さない）。
//!
//! 境界の規則:
//! - 基準トラックは映像（あれば）、なければ音声。
//! - 映像: target 以上経ってから来た keyframe で切る。音声だけ: target 以上経った AU で切る。
//! - 不連続（AU の `discontinuity`・`epoch` の変化・DTS の逆行や大きな飛び）では今の segment を閉じ、
//!   映像は次の keyframe まで捨てる（依存フレームだけを出さない。仕様 5.3）。次の segment に DISCONTINUITY を付ける。
//! - LL-HLS の part は任意の AU で切る（part target を超えない位置）。keyframe で始まる part は INDEPENDENT。
//!
//! 時間はすべてマイクロ秒。segment の長さは次の segment の最初の PTS との差（最後や不連続の前は推定）。

use crate::error::HlsError;
use crate::media::{Map, MediaPlaylist, MediaSegment, OpenSegment, Part, PlaylistType, PreloadHint, PreloadHintType, RenditionReport, ServerControl};
use senca_core::time::{Rescale, Timebase};
use senca_core::{AccessUnit, TrackId};
use std::collections::VecDeque;

/// playlist の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistKind {
    /// 窓（`window` 個）だけを載せる。
    Live,
    /// 追記のみ（PLAYLIST-TYPE:EVENT）。
    Event,
    /// 追記のみで、終わったら PLAYLIST-TYPE:VOD と ENDLIST。
    Vod,
}

/// 設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmenterConfig {
    pub video_track: Option<TrackId>,
    pub audio_track: Option<TrackId>,
    /// 目標の segment 長。
    pub target_duration_us: u64,
    /// LL-HLS の part target（`None` なら part を作らない）。
    pub part_target_us: Option<u64>,
    pub kind: PlaylistKind,
    /// Live の窓の segment 数。
    pub window: usize,
    /// Event・Vod で保持する segment 数の上限（超えたら古いものから落とし `truncated` を立てる）。
    pub max_segments: usize,
    /// DTS がこれ以上飛んだら不連続とみなす。
    pub max_gap_us: u64,
    /// 最初の media sequence number。
    pub first_sequence: u64,
    /// delta update（CAN-SKIP-UNTIL）を出す。
    pub delta_updates: bool,
}

impl SegmenterConfig {
    /// 2 秒 segment・窓 6 の live。
    pub fn live(video_track: Option<TrackId>, audio_track: Option<TrackId>) -> Self {
        SegmenterConfig {
            video_track,
            audio_track,
            target_duration_us: 2_000_000,
            part_target_us: None,
            kind: PlaylistKind::Live,
            window: 6,
            max_segments: 100_000,
            max_gap_us: 10_000_000,
            first_sequence: 0,
            delta_updates: false,
        }
    }
}

/// segmenter が返す出来事。順番: `SegmentStart` →（`PartStart` → `Au`* → `PartEnd`）* または `Au`* → `SegmentEnd`。
/// `SegmentRemoved` は窓から外れた segment（呼び出し側が猶予の後に消す）。
#[derive(Debug, Clone)]
pub enum SegmentEvent {
    SegmentStart {
        seq: u64,
        /// 直前に不連続がある（EXT-X-DISCONTINUITY）。
        discontinuity: bool,
        /// 何番目の不連続区間か（0 から）。TS の連続性カウンターや fMP4 の init の切り替えに使う。
        discontinuity_index: u64,
        start_us: i64,
        program_date_time_ms: Option<i64>,
    },
    PartStart { seq: u64, part: u32, independent: bool },
    /// AU をこの segment（LL-HLS ならこの part）に入れる。
    Au { seq: u64, part: Option<u32>, au: AccessUnit },
    PartEnd { seq: u64, part: u32, duration_us: u64 },
    SegmentEnd { seq: u64, duration_us: u64 },
    SegmentRemoved { seq: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PartInfo {
    duration_us: u64,
    independent: bool,
}

#[derive(Debug, Clone)]
struct SegmentInfo {
    seq: u64,
    duration_us: u64,
    discontinuity: bool,
    discontinuity_index: u64,
    pdt_ms: Option<i64>,
    parts: Vec<PartInfo>,
}

#[derive(Debug, Clone)]
struct Open {
    seq: u64,
    epoch: u64,
    /// 基準トラックの時間軸。以下の時刻はこの刻みで持つ（差を取ってから換算し、丸めの誤差をためない）。
    tb: Timebase,
    start: i64,
    /// PTS - DTS（segment 先頭の keyframe）。part の時刻は DTS をこれでずらして測る（単調にするため）。
    offset: i64,
    discontinuity: bool,
    discontinuity_index: u64,
    pdt_ms: Option<i64>,
    parts: Vec<PartInfo>,
    part_start: i64,
    part_independent: bool,
    /// 推定の終わり（最大の PTS + 1 フレーム）。
    end: i64,
}

impl Open {
    /// 刻みの差をマイクロ秒に（負は 0）。
    fn us(&self, a: i64, b: i64) -> u64 {
        a.saturating_sub(b).rescale(self.tb, Timebase::MICROS).unwrap_or(i64::MAX).max(0) as u64
    }
}

/// LL-HLS の blocking reload の判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockingDecision {
    /// 今の playlist で答えてよい。
    Ready,
    /// 求める segment / part がまだない（呼び出し側が待つ。待ち時間と待機数に上限を置くこと）。
    Wait,
    /// 遠すぎる未来や不正な組み合わせ（400）。
    BadRequest,
}

/// `_HLS_skip`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkipRequest {
    #[default]
    No,
    Yes,
    V2,
}

/// LL-HLS の delivery directive（`_HLS_msn`・`_HLS_part`・`_HLS_skip`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HlsQuery {
    pub msn: Option<u64>,
    pub part: Option<u32>,
    pub skip: SkipRequest,
}

impl HlsQuery {
    /// 復号済みの query の組から読む。重複・数値でない値・`_HLS_msn` なしの `_HLS_part` は `Err`。
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<HlsQuery, HlsError> {
        let mut q = HlsQuery::default();
        let mut seen = [false; 3];
        let num = |v: &str| crate::attr::parse_integer(v).ok_or(HlsError::Syntax("delivery directive"));
        for (k, v) in pairs {
            let idx = match k {
                "_HLS_msn" => {
                    q.msn = Some(num(v)?);
                    0
                }
                "_HLS_part" => {
                    q.part = Some(u32::try_from(num(v)?).map_err(|_| HlsError::Syntax("delivery directive"))?);
                    1
                }
                "_HLS_skip" => {
                    q.skip = match v {
                        "YES" => SkipRequest::Yes,
                        "v2" => SkipRequest::V2,
                        _ => return Err(HlsError::Syntax("delivery directive")),
                    };
                    2
                }
                _ => continue,
            };
            if seen[idx] {
                return Err(HlsError::Syntax("duplicate delivery directive"));
            }
            seen[idx] = true;
        }
        if q.part.is_some() && q.msn.is_none() {
            return Err(HlsError::Syntax("_HLS_part without _HLS_msn"));
        }
        Ok(q)
    }
}

/// segment・part・init の URI の付け方。
pub trait UriScheme {
    fn segment(&self, seq: u64) -> String;
    fn part(&self, seq: u64, part: u32) -> String;
    /// 不連続区間ごとの初期化 segment（fMP4）。TS では `None`。
    fn init(&self, _discontinuity_index: u64) -> Option<String> {
        None
    }
}

/// `{prefix}{seq}.{ext}`・`{prefix}{seq}.{part}.{ext}`・`init{n}.mp4` の単純な命名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleUris {
    pub prefix: String,
    pub ext: String,
    pub fmp4_init: bool,
}

impl UriScheme for SimpleUris {
    fn segment(&self, seq: u64) -> String {
        format!("{}{seq}.{}", self.prefix, self.ext)
    }
    fn part(&self, seq: u64, part: u32) -> String {
        format!("{}{seq}.{part}.{}", self.prefix, self.ext)
    }
    fn init(&self, d: u64) -> Option<String> {
        self.fmp4_init.then(|| format!("{}init{d}.mp4", self.prefix))
    }
}

/// segmenter 本体。
#[derive(Debug)]
pub struct Segmenter {
    cfg: SegmenterConfig,
    reference: TrackId,
    ref_is_video: bool,
    open: Option<Open>,
    segments: VecDeque<SegmentInfo>,
    next_seq: u64,
    /// 窓の先頭より前に落とした DISCONTINUITY の数。
    discontinuity_sequence: u64,
    /// これまでの不連続の数（次の区間の番号）。
    discontinuity_count: u64,
    pending_discontinuity: bool,
    last_dts_us: Option<i64>,
    last_dts: Option<i64>,
    last_epoch: Option<u64>,
    ref_tb: Option<Timebase>,
    /// 1 フレームの推定（基準トラックの刻み）。
    frame: i64,
    target_duration_s: u64,
    ended: bool,
    truncated: bool,
    dropped: u64,
}

fn to_us(v: i64, tb: Timebase) -> Option<i64> {
    v.rescale(tb, Timebase::MICROS)
}

impl Segmenter {
    pub fn new(cfg: SegmenterConfig) -> Result<Self, HlsError> {
        let (reference, ref_is_video) = match (cfg.video_track, cfg.audio_track) {
            (Some(v), _) => (v, true),
            (None, Some(a)) => (a, false),
            (None, None) => return Err(HlsError::InvalidValue("segmenter needs a video or audio track")),
        };
        if cfg.target_duration_us < 100_000 || cfg.target_duration_us > 60_000_000 {
            return Err(HlsError::InvalidValue("target duration"));
        }
        if let Some(p) = cfg.part_target_us
            && (p < 10_000 || p >= cfg.target_duration_us)
        {
            return Err(HlsError::InvalidValue("part target"));
        }
        if cfg.window < 3 || cfg.max_segments < cfg.window {
            return Err(HlsError::InvalidValue("window"));
        }
        Ok(Segmenter {
            reference,
            ref_is_video,
            open: None,
            segments: VecDeque::new(),
            next_seq: cfg.first_sequence,
            discontinuity_sequence: 0,
            discontinuity_count: 0,
            pending_discontinuity: false,
            last_dts_us: None,
            last_dts: None,
            last_epoch: None,
            ref_tb: None,
            frame: 0,
            target_duration_s: cfg.target_duration_us.div_ceil(1_000_000),
            ended: false,
            truncated: false,
            dropped: 0,
            cfg,
        })
    }

    pub fn config(&self) -> &SegmenterConfig {
        &self.cfg
    }

    /// 捨てた AU の数（keyframe 待ち・世代違い・時刻の換算不能）。
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Event・Vod で上限を超えて古い segment を落とした。
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn is_ended(&self) -> bool {
        self.ended
    }

    /// 現在の EXT-X-TARGETDURATION（長い GOP が来ると増える。減らない）。
    pub fn target_duration_s(&self) -> u64 {
        self.target_duration_s
    }

    /// AU を 1 つ入れる。出来事は `out` に足す。
    pub fn push(&mut self, au: AccessUnit, out: &mut Vec<SegmentEvent>) -> Result<(), HlsError> {
        if self.ended {
            return Err(HlsError::InvalidValue("segmenter already finished"));
        }
        if au.track != self.reference {
            match &self.open {
                Some(o) if o.epoch == au.epoch => {
                    let part = self.cfg.part_target_us.map(|_| o.parts.len() as u32);
                    out.push(SegmentEvent::Au { seq: o.seq, part, au });
                }
                _ => self.dropped += 1,
            }
            return Ok(());
        }
        let tb = au.timebase;
        let (Some(pts_us), Some(dts_us)) = (to_us(au.pts, tb), to_us(au.dts, tb)) else {
            self.dropped += 1;
            return Err(HlsError::InvalidValue("timestamp out of range"));
        };
        let (pts, dts) = (au.pts, au.dts);

        let disc = au.discontinuity
            || self.last_epoch.is_some_and(|e| e != au.epoch)
            || self.ref_tb.is_some_and(|t| t != tb)
            || self.last_dts_us.is_some_and(|d| dts_us < d || dts_us.saturating_sub(d) as u64 > self.cfg.max_gap_us);
        if au.duration > 0 && au.duration < i64::MAX as u64 {
            self.frame = au.duration as i64;
        } else if !disc
            && let Some(d) = self.last_dts
            && dts > d
        {
            self.frame = dts.saturating_sub(d);
        }
        self.last_dts_us = Some(dts_us);
        self.last_dts = Some(dts);
        self.last_epoch = Some(au.epoch);
        self.ref_tb = Some(tb);

        if disc {
            if let Some(o) = self.open.take() {
                let end = o.end;
                self.close(o, end, out);
            }
            self.pending_discontinuity = true;
        }

        let independent = au.keyframe || !self.ref_is_video;
        match &mut self.open {
            None => {
                if !independent {
                    self.dropped += 1;
                    return Ok(());
                }
                self.start(&au, pts_us, out);
            }
            Some(o) => {
                let elapsed = o.us(pts, o.start);
                if independent && pts >= o.start && elapsed >= self.cfg.target_duration_us {
                    let o = self.open.take().expect("open segment");
                    self.close(o, pts, out);
                    self.start(&au, pts_us, out);
                } else if let Some(pt) = self.cfg.part_target_us {
                    let t = dts.saturating_add(o.offset);
                    // segment 先頭からの位置の差で測る（part の合計が segment の長さと必ず一致する）
                    let ps = o.us(o.part_start, o.start);
                    let in_part = o.us(t, o.start).saturating_sub(ps);
                    let with_this = o.us(t.saturating_add(self.frame), o.start).saturating_sub(ps);
                    if in_part > 0 && with_this > pt {
                        let part = o.parts.len() as u32;
                        o.parts.push(PartInfo { duration_us: in_part, independent: o.part_independent });
                        out.push(SegmentEvent::PartEnd { seq: o.seq, part, duration_us: in_part });
                        o.part_start = t;
                        o.part_independent = au.keyframe || !self.ref_is_video;
                        out.push(SegmentEvent::PartStart { seq: o.seq, part: part + 1, independent: o.part_independent });
                    }
                }
            }
        }
        let o = self.open.as_mut().expect("open segment");
        o.end = o.end.max(pts.saturating_add(self.frame));
        let part = self.cfg.part_target_us.map(|_| o.parts.len() as u32);
        out.push(SegmentEvent::Au { seq: o.seq, part, au });
        Ok(())
    }

    /// 入力の終わり。開いている segment を（推定の長さで）閉じる。
    pub fn finish(&mut self, out: &mut Vec<SegmentEvent>) {
        if let Some(o) = self.open.take() {
            let end = o.end;
            self.close(o, end, out);
        }
        self.ended = true;
    }

    fn start(&mut self, au: &AccessUnit, pts_us: i64, out: &mut Vec<SegmentEvent>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let emitted_before = seq > self.cfg.first_sequence;
        let discontinuity = self.pending_discontinuity && emitted_before;
        if discontinuity {
            self.discontinuity_count += 1;
        }
        self.pending_discontinuity = false;
        let pdt_ms = au.capture_time_ns.map(|ns| ns.div_euclid(1_000_000));
        out.push(SegmentEvent::SegmentStart {
            seq,
            discontinuity,
            discontinuity_index: self.discontinuity_count,
            start_us: pts_us,
            program_date_time_ms: pdt_ms,
        });
        if self.cfg.part_target_us.is_some() {
            out.push(SegmentEvent::PartStart { seq, part: 0, independent: true });
        }
        self.open = Some(Open {
            seq,
            epoch: au.epoch,
            tb: au.timebase,
            start: au.pts,
            offset: au.pts.saturating_sub(au.dts),
            discontinuity,
            discontinuity_index: self.discontinuity_count,
            pdt_ms,
            parts: Vec::new(),
            part_start: au.pts,
            part_independent: true,
            end: au.pts,
        });
    }

    /// `end` は基準トラックの刻み。
    fn close(&mut self, mut o: Open, end: i64, out: &mut Vec<SegmentEvent>) {
        let duration_us = o.us(end, o.start);
        if self.cfg.part_target_us.is_some() {
            let d = duration_us.saturating_sub(o.us(o.part_start, o.start));
            let part = o.parts.len() as u32;
            o.parts.push(PartInfo { duration_us: d, independent: o.part_independent });
            out.push(SegmentEvent::PartEnd { seq: o.seq, part, duration_us: d });
        }
        out.push(SegmentEvent::SegmentEnd { seq: o.seq, duration_us });
        self.target_duration_s = self.target_duration_s.max(duration_us.saturating_add(500_000) / 1_000_000);
        self.segments.push_back(SegmentInfo {
            seq: o.seq,
            duration_us,
            discontinuity: o.discontinuity,
            discontinuity_index: o.discontinuity_index,
            pdt_ms: o.pdt_ms,
            parts: o.parts,
        });
        let cap = match self.cfg.kind {
            PlaylistKind::Live => self.cfg.window,
            PlaylistKind::Event | PlaylistKind::Vod => self.cfg.max_segments,
        };
        while self.segments.len() > cap {
            let s = self.segments.pop_front().expect("non-empty");
            if self.cfg.kind != PlaylistKind::Live {
                self.truncated = true;
            }
            if s.discontinuity {
                self.discontinuity_sequence += 1;
            }
            out.push(SegmentEvent::SegmentRemoved { seq: s.seq });
        }
        // 古い segment の part は playlist に載せないので捨てる（3 target duration より前）
        let keep_us = 3 * self.target_duration_s * 1_000_000;
        let mut acc = 0u64;
        for s in self.segments.iter_mut().rev() {
            if acc >= keep_us {
                s.parts = Vec::new();
            }
            acc = acc.saturating_add(s.duration_us);
        }
    }

    /// 窓の最初の media sequence number。
    pub fn media_sequence(&self) -> u64 {
        self.segments.front().map_or(self.open.as_ref().map_or(self.next_seq, |o| o.seq), |s| s.seq)
    }

    /// 最後に完成した segment の番号。
    pub fn last_complete(&self) -> Option<u64> {
        self.segments.back().map(|s| s.seq)
    }

    /// LL-HLS の blocking reload 要求に今答えてよいか。
    pub fn blocking_decision(&self, q: &HlsQuery) -> BlockingDecision {
        let Some(msn) = q.msn else {
            return if q.part.is_some() { BlockingDecision::BadRequest } else { BlockingDecision::Ready };
        };
        let current = self.open.as_ref().map_or(self.next_seq, |o| o.seq);
        if msn > current.saturating_add(2) {
            return BlockingDecision::BadRequest;
        }
        if self.ended || msn < self.media_sequence() {
            return BlockingDecision::Ready;
        }
        let part = match (q.part, self.cfg.part_target_us) {
            (Some(p), Some(_)) => p,
            _ => {
                return if self.last_complete().is_some_and(|l| msn <= l) { BlockingDecision::Ready } else { BlockingDecision::Wait };
            }
        };
        // part 付き: 完成した segment なら part の数を超えた要求は次の segment の part 0 とみなす
        if let Some(s) = self.segments.iter().find(|s| s.seq == msn) {
            if (part as usize) < s.parts.len() || s.parts.is_empty() {
                return BlockingDecision::Ready;
            }
            return self.blocking_decision(&HlsQuery { msn: Some(msn + 1), part: Some(0), skip: q.skip });
        }
        match &self.open {
            Some(o) if o.seq == msn && (part as usize) < o.parts.len() => BlockingDecision::Ready,
            _ => BlockingDecision::Wait,
        }
    }

    /// 他の rendition の playlist に載せる RENDITION-REPORT（URI は呼び出し側が渡す）。
    pub fn rendition_report(&self, uri: impl Into<String>) -> RenditionReport {
        let (msn, part) = match &self.open {
            Some(o) if !o.parts.is_empty() => (Some(o.seq), Some(o.parts.len() as u64 - 1)),
            _ => match self.segments.back() {
                Some(s) => (Some(s.seq), s.parts.len().checked_sub(1).map(|p| p as u64)),
                None => (None, None),
            },
        };
        RenditionReport { uri: uri.into(), last_msn: msn, last_part: if self.cfg.part_target_us.is_some() { part } else { None } }
    }

    /// 今の状態から media playlist を作る。`skip` は delta update の要求（`delta_updates` が無効なら無視）。
    pub fn playlist(&self, uris: &dyn UriScheme, skip: SkipRequest) -> MediaPlaylist {
        let mut pl = MediaPlaylist::new(self.target_duration_s);
        pl.media_sequence = self.media_sequence();
        pl.discontinuity_sequence = self.discontinuity_sequence;
        pl.independent_segments = true;
        pl.playlist_type = match self.cfg.kind {
            PlaylistKind::Live => None,
            PlaylistKind::Event => Some(PlaylistType::Event),
            PlaylistKind::Vod if self.ended && !self.truncated => Some(PlaylistType::Vod),
            PlaylistKind::Vod => Some(PlaylistType::Event),
        };
        if self.truncated {
            pl.playlist_type = None;
        }
        let skip_until = (self.cfg.delta_updates && self.cfg.kind == PlaylistKind::Live)
            .then_some(6 * self.target_duration_s * 1_000_000);
        if let Some(pt) = self.cfg.part_target_us {
            pl.part_target_us = Some(pt);
            pl.server_control = Some(ServerControl {
                can_block_reload: true,
                can_skip_until_us: skip_until,
                can_skip_dateranges: false,
                hold_back_us: None,
                part_hold_back_us: Some(3 * pt),
            });
        } else if skip_until.is_some() {
            pl.server_control = Some(ServerControl { can_skip_until_us: skip_until, ..Default::default() });
        }

        // delta update: 末尾から CAN-SKIP-UNTIL より古い segment を省く
        let mut skipped = 0usize;
        if skip != SkipRequest::No
            && let Some(until) = skip_until
        {
            let mut acc = 0u64;
            let mut keep = 0usize;
            for s in self.segments.iter().rev() {
                acc = acc.saturating_add(s.duration_us);
                keep += 1;
                if acc >= until {
                    break;
                }
            }
            skipped = self.segments.len() - keep;
            if skipped > 0 {
                pl.skipped_segments = Some(skipped as u64);
            }
        }

        let mut last_init: Option<String> = None;
        for (i, s) in self.segments.iter().enumerate() {
            let init = uris.init(s.discontinuity_index);
            let map = if init != last_init || (i == skipped && init.is_some()) {
                init.clone().map(|uri| Map { uri, byte_range: None })
            } else {
                None
            };
            last_init = init;
            if i < skipped {
                continue;
            }
            let mut seg = MediaSegment::new(uris.segment(s.seq), s.duration_us);
            seg.discontinuity = s.discontinuity;
            seg.program_date_time_ms = s.pdt_ms;
            seg.map = map;
            seg.parts = s
                .parts
                .iter()
                .enumerate()
                .map(|(k, p)| Part { uri: uris.part(s.seq, k as u32), duration_us: p.duration_us, independent: p.independent, byte_range: None, gap: false })
                .collect();
            pl.segments.push(seg);
        }
        if self.cfg.part_target_us.is_some()
            && let Some(o) = &self.open
            && !o.parts.is_empty()
        {
            let init = uris.init(o.discontinuity_index);
            let map = if init != last_init || (pl.segments.is_empty() && init.is_some()) { init.map(|uri| Map { uri, byte_range: None }) } else { None };
            pl.open_segment = Some(OpenSegment {
                discontinuity: o.discontinuity,
                program_date_time_ms: o.pdt_ms,
                map,
                parts: o
                    .parts
                    .iter()
                    .enumerate()
                    .map(|(k, p)| Part { uri: uris.part(o.seq, k as u32), duration_us: p.duration_us, independent: p.independent, byte_range: None, gap: false })
                    .collect(),
            });
        }
        if self.cfg.part_target_us.is_some() && !self.ended {
            let (seq, part) = match &self.open {
                Some(o) => (o.seq, o.parts.len() as u32),
                None => (self.next_seq, 0),
            };
            pl.preload_hints.push(PreloadHint { hint_type: PreloadHintType::Part, uri: uris.part(seq, part), byte_range_start: None, byte_range_length: None });
        }
        pl.end_list = self.ended;
        pl.version = Some(pl.required_version());
        pl
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use senca_core::Codec;

    const V: TrackId = TrackId(0);
    const A: TrackId = TrackId(1);

    fn vau(n: i64, key: bool) -> AccessUnit {
        // 30 fps、90 kHz、B フレームなし
        AccessUnit {
            track: V,
            codec: Codec::H264,
            epoch: 0,
            sequence: n as u64,
            config_generation: 0,
            pts: n * 3000,
            dts: n * 3000,
            duration: 3000,
            timebase: Timebase::MPEG,
            keyframe: key,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from_static(b"v"),
        }
    }

    fn aau(n: i64) -> AccessUnit {
        AccessUnit {
            track: A,
            codec: Codec::Aac,
            epoch: 0,
            sequence: n as u64,
            config_generation: 0,
            pts: n * 1024,
            dts: n * 1024,
            duration: 1024,
            timebase: Timebase::new(1, 48_000),
            keyframe: true,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from_static(b"a"),
        }
    }

    fn uris() -> SimpleUris {
        SimpleUris { prefix: "s".into(), ext: "ts".into(), fmp4_init: false }
    }

    fn ends(ev: &[SegmentEvent]) -> Vec<(u64, u64)> {
        ev.iter().filter_map(|e| if let SegmentEvent::SegmentEnd { seq, duration_us } = e { Some((*seq, *duration_us)) } else { None }).collect()
    }

    #[test]
    fn video_cuts_at_keyframes() {
        let mut s = Segmenter::new(SegmenterConfig::live(Some(V), Some(A))).unwrap();
        let mut ev = Vec::new();
        // GOP 1 秒（30 フレーム）、10 秒
        for n in 0..300 {
            s.push(vau(n, n % 30 == 0), &mut ev).unwrap();
        }
        s.finish(&mut ev);
        let e = ends(&ev);
        assert_eq!(e.len(), 5);
        assert!(e.iter().all(|&(_, d)| d == 2_000_000), "{e:?}");
        let removed = ev.iter().filter(|e| matches!(e, SegmentEvent::SegmentRemoved { .. })).count();
        assert_eq!(removed, 0);
        let pl = s.playlist(&uris(), SkipRequest::No);
        pl.validate().unwrap();
        assert!(pl.end_list);
        let text = pl.to_m3u8().unwrap();
        assert_eq!(MediaPlaylist::parse(text.as_bytes()).unwrap(), pl);
    }

    #[test]
    fn audio_waits_for_first_video_keyframe_and_window_slides() {
        let mut cfg = SegmenterConfig::live(Some(V), Some(A));
        cfg.window = 3;
        let mut s = Segmenter::new(cfg).unwrap();
        let mut ev = Vec::new();
        s.push(aau(0), &mut ev).unwrap();
        s.push(vau(0, false), &mut ev).unwrap();
        assert!(ev.is_empty());
        assert_eq!(s.dropped(), 2);
        for n in 1..400 {
            s.push(vau(n, n % 60 == 1), &mut ev).unwrap();
        }
        let removed: Vec<u64> = ev.iter().filter_map(|e| if let SegmentEvent::SegmentRemoved { seq } = e { Some(*seq) } else { None }).collect();
        assert_eq!(removed, vec![0, 1, 2]);
        let pl = s.playlist(&uris(), SkipRequest::No);
        assert_eq!(pl.media_sequence, 3);
        assert_eq!(pl.segments.len(), 3);
        assert!(!pl.end_list);
    }

    #[test]
    fn audio_only_cuts_by_duration() {
        let mut s = Segmenter::new(SegmenterConfig::live(None, Some(A))).unwrap();
        let mut ev = Vec::new();
        for n in 0..1000 {
            s.push(aau(n), &mut ev).unwrap();
        }
        let e = ends(&ev);
        // 1024/48000 = 21.333 ms。2 秒を超えた最初のフレームで切る
        assert!(e.iter().all(|&(_, d)| (2_000_000..2_021_334).contains(&d)), "{e:?}");
        s.playlist(&uris(), SkipRequest::No).validate().unwrap();
    }

    #[test]
    fn gap_mid_gop_drops_until_keyframe() {
        let mut s = Segmenter::new(SegmenterConfig::live(Some(V), None)).unwrap();
        let mut ev = Vec::new();
        for n in 0..75 {
            s.push(vau(n, n % 30 == 0), &mut ev).unwrap();
        }
        // n=75..79 欠落。80 は GOP の途中
        let mut g = vau(80, false);
        g.discontinuity = true;
        s.push(g, &mut ev).unwrap();
        for n in 81..90 {
            s.push(vau(n, false), &mut ev).unwrap();
        }
        assert_eq!(s.dropped(), 10);
        let starts_before = ev.iter().filter(|e| matches!(e, SegmentEvent::SegmentStart { .. })).count();
        assert_eq!(starts_before, 2);
        for n in 90..200 {
            s.push(vau(n, n % 30 == 0), &mut ev).unwrap();
        }
        let disc: Vec<_> = ev
            .iter()
            .filter_map(|e| if let SegmentEvent::SegmentStart { seq, discontinuity, discontinuity_index, .. } = e { Some((*seq, *discontinuity, *discontinuity_index)) } else { None })
            .collect();
        assert_eq!(disc[2], (2, true, 1));
        // 欠落の前の segment は推定の終わり（最後の PTS + 1 フレーム）で閉じる
        assert_eq!(ends(&ev)[1], (1, 500_000));
        // 依存フレームが segment の先頭に来ない
        let mut expect_key = false;
        for e in &ev {
            match e {
                SegmentEvent::SegmentStart { .. } => expect_key = true,
                SegmentEvent::Au { au, .. } if expect_key => {
                    assert!(au.keyframe);
                    expect_key = false;
                }
                _ => {}
            }
        }
        let pl = s.playlist(&uris(), SkipRequest::No);
        assert!(pl.segments[2].discontinuity);
    }

    #[test]
    fn epoch_change_and_backwards_dts_are_discontinuities() {
        let mut s = Segmenter::new(SegmenterConfig::live(Some(V), Some(A))).unwrap();
        let mut ev = Vec::new();
        for n in 0..90 {
            s.push(vau(n, n % 30 == 0), &mut ev).unwrap();
        }
        let mut a = aau(0);
        a.epoch = 1;
        s.push(a, &mut ev).unwrap(); // 映像より先に来た新しい世代の音声は捨てる
        assert_eq!(s.dropped(), 1);
        for n in 0..90 {
            let mut v = vau(n, n % 30 == 0);
            v.epoch = 1;
            s.push(v, &mut ev).unwrap();
        }
        for n in 0..90 {
            let mut v = vau(n, n % 30 == 0); // 時刻が戻る（epoch は同じ）
            v.epoch = 1;
            s.push(v, &mut ev).unwrap();
        }
        let n_disc = ev.iter().filter(|e| matches!(e, SegmentEvent::SegmentStart { discontinuity: true, .. })).count();
        assert_eq!(n_disc, 2);
    }

    #[test]
    fn ll_hls_parts() {
        let mut cfg = SegmenterConfig::live(Some(V), Some(A));
        cfg.part_target_us = Some(500_000);
        cfg.delta_updates = true;
        cfg.window = 20;
        let mut s = Segmenter::new(cfg).unwrap();
        let mut ev = Vec::new();
        for n in 0..630 {
            s.push(vau(n, n % 60 == 0), &mut ev).unwrap();
            if n % 2 == 0 {
                s.push(aau(n * 3000 * 48_000 / 90_000 / 1024), &mut ev).unwrap();
            }
        }
        // part の長さの合計 = segment の長さ、part は part target 以下
        let mut sum = 0u64;
        let mut cur_part = None;
        for e in &ev {
            match e {
                SegmentEvent::PartStart { part, independent, .. } => {
                    cur_part = Some(*part);
                    if *part == 0 {
                        assert!(*independent);
                    }
                }
                SegmentEvent::PartEnd { duration_us, part, .. } => {
                    assert_eq!(Some(*part), cur_part);
                    assert!(*duration_us <= 500_000, "{duration_us}");
                    sum += duration_us;
                }
                SegmentEvent::Au { part, .. } => assert_eq!(*part, cur_part),
                SegmentEvent::SegmentEnd { duration_us, .. } => {
                    assert_eq!(sum, *duration_us);
                    sum = 0;
                }
                _ => {}
            }
        }
        let pl = s.playlist(&uris(), SkipRequest::No);
        pl.validate().unwrap();
        assert_eq!(pl.server_control.unwrap().part_hold_back_us, Some(1_500_000));
        let os = pl.open_segment.as_ref().unwrap();
        assert_eq!(os.parts.len(), 1);
        // 古い segment には part を載せない
        assert!(pl.segments[0].parts.is_empty());
        assert!(!pl.segments.last().unwrap().parts.is_empty());
        assert_eq!(pl.preload_hints.len(), 1);
        let text = pl.to_m3u8().unwrap();
        assert_eq!(MediaPlaylist::parse(text.as_bytes()).unwrap(), pl);

        // delta update
        let d = s.playlist(&uris(), SkipRequest::Yes);
        assert!(d.skipped_segments.unwrap() > 0);
        assert_eq!(d.skipped_segments.unwrap() as usize + d.segments.len(), pl.segments.len());
        assert!(d.to_m3u8().unwrap().contains("#EXT-X-SKIP:SKIPPED-SEGMENTS="));

        // blocking reload
        let last = s.last_complete().unwrap();
        let open_seq = last + 1;
        let q = |msn: u64, part: Option<u32>| HlsQuery { msn: Some(msn), part, skip: SkipRequest::No };
        assert_eq!(s.blocking_decision(&q(last, None)), BlockingDecision::Ready);
        assert_eq!(s.blocking_decision(&q(open_seq, None)), BlockingDecision::Wait);
        assert_eq!(s.blocking_decision(&q(open_seq, Some(0))), BlockingDecision::Ready);
        assert_eq!(s.blocking_decision(&q(open_seq, Some(50))), BlockingDecision::Wait);
        assert_eq!(s.blocking_decision(&q(last, Some(50))), BlockingDecision::Ready); // 次の segment の part 0 とみなす
        assert_eq!(s.blocking_decision(&q(open_seq + 2, None)), BlockingDecision::Wait);
        assert_eq!(s.blocking_decision(&q(open_seq + 3, None)), BlockingDecision::BadRequest);
        assert_eq!(s.blocking_decision(&HlsQuery { msn: None, part: Some(1), skip: SkipRequest::No }), BlockingDecision::BadRequest);
        let r = s.rendition_report("../a.m3u8");
        assert_eq!(r.last_msn, Some(open_seq));
    }

    #[test]
    fn query_parse() {
        let q = HlsQuery::from_pairs([("_HLS_msn", "12"), ("_HLS_part", "3"), ("x", "y"), ("_HLS_skip", "YES")]).unwrap();
        assert_eq!(q, HlsQuery { msn: Some(12), part: Some(3), skip: SkipRequest::Yes });
        assert!(HlsQuery::from_pairs([("_HLS_part", "3")]).is_err());
        assert!(HlsQuery::from_pairs([("_HLS_msn", "1"), ("_HLS_msn", "2")]).is_err());
        assert!(HlsQuery::from_pairs([("_HLS_msn", "-1")]).is_err());
        assert!(HlsQuery::from_pairs([("_HLS_skip", "yes")]).is_err());
    }

    #[test]
    fn vod_and_fmp4_map() {
        let mut cfg = SegmenterConfig::live(Some(V), None);
        cfg.kind = PlaylistKind::Vod;
        let mut s = Segmenter::new(cfg).unwrap();
        let mut ev = Vec::new();
        for n in 0..200 {
            let mut v = vau(n, n % 30 == 0);
            if n == 100 {
                v.discontinuity = true;
            }
            s.push(v, &mut ev).unwrap();
        }
        let u = SimpleUris { prefix: "".into(), ext: "m4s".into(), fmp4_init: true };
        assert_eq!(s.playlist(&u, SkipRequest::No).playlist_type, Some(PlaylistType::Event));
        s.finish(&mut ev);
        let pl = s.playlist(&u, SkipRequest::No);
        pl.validate().unwrap();
        assert_eq!(pl.playlist_type, Some(PlaylistType::Vod));
        let maps: Vec<_> = pl.segments.iter().filter_map(|s| s.map.as_ref().map(|m| m.uri.clone())).collect();
        assert_eq!(maps, vec!["init0.mp4", "init1.mp4"]);
        let text = pl.to_m3u8().unwrap();
        assert!(text.contains("#EXT-X-VERSION:6"));
        assert_eq!(MediaPlaylist::parse(text.as_bytes()).unwrap(), pl);
    }

    #[test]
    fn random_streams_keep_invariants() {
        let mut x = 0x1234_5678_9abc_def0u64;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for round in 0..200 {
            let mut cfg = SegmenterConfig::live(Some(V), Some(A));
            if round % 2 == 0 {
                cfg.part_target_us = Some(300_000);
            }
            let mut s = Segmenter::new(cfg).unwrap();
            let mut ev = Vec::new();
            let mut t = 0i64;
            let mut epoch = 0;
            for _ in 0..500 {
                let r = rnd();
                t += match r % 50 {
                    0 => -(((r >> 8) % 100_000) as i64),
                    1 => ((r >> 8) % 2_000_000_000) as i64,
                    _ => ((r >> 8) % 6000) as i64,
                };
                if r % 97 == 0 {
                    epoch += 1;
                }
                let mut au = if r % 3 == 0 { aau(0) } else { vau(0, r % 23 == 0) };
                au.pts = t + ((r >> 20) % 9000) as i64;
                au.dts = t;
                au.timebase = Timebase::MPEG;
                au.duration = if r % 5 == 0 { 0 } else { 3000 };
                au.epoch = epoch;
                au.discontinuity = r % 89 == 0;
                if r % 211 == 0 {
                    au.pts = i64::MAX;
                }
                let _ = s.push(au, &mut ev);
            }
            s.finish(&mut ev);
            let mut open: Option<u64> = None;
            let mut last_seq = None;
            let mut need_key = false;
            for e in &ev {
                match e {
                    SegmentEvent::SegmentStart { seq, .. } => {
                        assert!(open.is_none());
                        assert!(last_seq.is_none_or(|l| *seq == l + 1));
                        open = Some(*seq);
                        last_seq = Some(*seq);
                        need_key = true;
                    }
                    SegmentEvent::Au { seq, au, .. } => {
                        assert_eq!(Some(*seq), open);
                        if need_key && au.track == V {
                            assert!(au.keyframe);
                            need_key = false;
                        }
                    }
                    SegmentEvent::SegmentEnd { seq, .. } => {
                        assert_eq!(Some(*seq), open);
                        open = None;
                    }
                    _ => {}
                }
            }
            let pl = s.playlist(&uris(), SkipRequest::No);
            let text = pl.to_m3u8().unwrap();
            assert_eq!(MediaPlaylist::parse(text.as_bytes()).unwrap(), pl);
        }
    }
}
