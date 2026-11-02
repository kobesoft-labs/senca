//! 断片化しない MP4（moov を先頭に置く）。録画の書き出し用。
//!
//! サンプルの中身は持たない。[`Mp4Writer::add_sample`] でメタデータだけを入れ、
//! [`Mp4Writer::finish`] が返すヘッダー（ftyp + moov + mdat の箱ヘッダー）の後ろに、
//! 入れた順にサンプルの中身を並べればファイルになる（大きな録画をメモリーに載せずに書ける）。

use crate::bmff::{begin_full_box, end_box, ftyp, put_u32, put_u64};
use crate::error::Mp4Error;
use crate::fragment::Sample;
use crate::init::{MOVIE_TIMESCALE, TrackSpec, TrakTiming, mvhd, trak, validate_tracks};
use senca_core::MediaKind;
use senca_core::time::{Rescale, Timebase};

/// サンプルの数の上限（全トラック合計）。
const MAX_SAMPLES: usize = 1 << 28;

/// サンプルのメタデータ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleInfo {
    /// DTS（トラックの timescale）。
    pub dts: i64,
    pub duration: u32,
    pub cts_offset: i32,
    pub keyframe: bool,
    pub size: u32,
}

#[derive(Debug, Clone)]
struct TrackState {
    spec: TrackSpec,
    samples: Vec<SampleInfo>,
    /// チャンク: (先頭サンプル番号, サンプル数, mdat 内の位置)。
    chunks: Vec<(u32, u32, u64)>,
}

/// 断片化しない MP4 の書き手。
#[derive(Debug, Clone)]
pub struct Mp4Writer {
    tracks: Vec<TrackState>,
    last_track: Option<usize>,
    mdat_size: u64,
    total_samples: usize,
}

impl Mp4Writer {
    pub fn new(tracks: Vec<TrackSpec>) -> Result<Self, Mp4Error> {
        validate_tracks(&tracks)?;
        Ok(Mp4Writer {
            tracks: tracks.into_iter().map(|spec| TrackState { spec, samples: Vec::new(), chunks: Vec::new() }).collect(),
            last_track: None,
            mdat_size: 0,
            total_samples: 0,
        })
    }

    /// サンプルを 1 つ足す（中身はヘッダーの後ろにこの順で置く）。DTS はトラック内で単調非減少。
    pub fn add_sample(&mut self, track_id: u32, s: SampleInfo) -> Result<(), Mp4Error> {
        let idx = self.tracks.iter().position(|t| t.spec.track_id == track_id).ok_or(Mp4Error::UnknownTrack(track_id))?;
        if self.total_samples >= MAX_SAMPLES {
            return Err(Mp4Error::TooLarge("sample count"));
        }
        let t = &mut self.tracks[idx];
        if let Some(last) = t.samples.last()
            && s.dts < last.dts
        {
            return Err(Mp4Error::Timestamp("decode time went backwards"));
        }
        let n = t.samples.len() as u32;
        if self.last_track == Some(idx) {
            if let Some(c) = t.chunks.last_mut() {
                c.1 += 1;
            }
        } else {
            t.chunks.push((n, 1, self.mdat_size));
        }
        t.samples.push(s);
        self.last_track = Some(idx);
        self.mdat_size += s.size as u64;
        self.total_samples += 1;
        Ok(())
    }

    /// [`Sample`] から足す（中身の長さだけを使う）。
    pub fn add(&mut self, track_id: u32, s: &Sample) -> Result<(), Mp4Error> {
        let size = u32::try_from(s.data.len()).map_err(|_| Mp4Error::TooLarge("sample"))?;
        self.add_sample(track_id, SampleInfo { dts: s.dts, duration: s.duration, cts_offset: s.cts_offset, keyframe: s.keyframe, size })
    }

    /// mdat の中身の長さ。
    pub fn mdat_payload_size(&self) -> u64 {
        self.mdat_size
    }

    /// ftyp + moov + mdat の箱ヘッダーを返す。後ろに中身を足す順は [`add_sample`](Self::add_sample) と同じ。
    pub fn finish(&self) -> Result<Vec<u8>, Mp4Error> {
        // 時刻のそろえ: 全トラックで最も早い表示時刻を 0 にする。
        let mut global_start: Option<(i64, u32)> = None; // (pts, timescale)
        for t in &self.tracks {
            if let Some(p) = min_pts(&t.samples) {
                let earlier = match global_start {
                    None => true,
                    Some((g, gts)) => (p as i128) * (gts as i128) < (g as i128) * (t.spec.timescale as i128),
                };
                if earlier {
                    global_start = Some((p, t.spec.timescale));
                }
            }
        }
        let mdat_hdr: u64 = if self.mdat_size + 8 > u32::MAX as u64 { 16 } else { 8 };
        // stco と co64 のどちらにするかで moov の長さが変わるので、まず stco で長さを見積もる
        let mut co64 = false;
        let mut moov = self.moov(global_start, 0, co64)?;
        let head = 32 + moov.len() as u64 + mdat_hdr;
        if head + self.mdat_size > u32::MAX as u64 {
            co64 = true;
            moov = self.moov(global_start, 0, co64)?;
        }
        let mut ft = Vec::new();
        let mut compat: Vec<&[u8; 4]> = vec![b"isom", b"iso2", b"mp41"];
        if self.tracks.iter().any(|t| t.samples.iter().any(|s| s.cts_offset < 0)) {
            compat.push(b"iso4");
        }
        ftyp(&mut ft, b"isom", 0x200, &compat);
        let base = ft.len() as u64 + moov.len() as u64 + mdat_hdr;
        let moov2 = self.moov(global_start, base, co64)?;
        debug_assert_eq!(moov.len(), moov2.len());
        let mut out = ft;
        out.extend_from_slice(&moov2);
        if mdat_hdr == 16 {
            put_u32(&mut out, 1);
            out.extend_from_slice(b"mdat");
            put_u64(&mut out, self.mdat_size + 16);
        } else {
            put_u32(&mut out, (self.mdat_size + 8) as u32);
            out.extend_from_slice(b"mdat");
        }
        Ok(out)
    }

    fn moov(&self, global_start: Option<(i64, u32)>, base: u64, co64: bool) -> Result<Vec<u8>, Mp4Error> {
        let movie = Timebase::new(1, MOVIE_TIMESCALE);
        let mut buf = Vec::with_capacity(4096 + self.total_samples * 12);
        let moov = crate::bmff::begin_box(&mut buf, b"moov");
        let mut movie_duration = 0u64;
        let mut traks = Vec::new();
        for t in &self.tracks {
            let ts = Timebase::new(1, t.spec.timescale);
            let media_duration: u64 = t.samples.iter().map(|s| s.duration as u64).sum();
            let first_dts = t.samples.first().map(|s| s.dts).unwrap_or(0);
            let mut edits: Vec<(u64, i64)> = Vec::new();
            let mut track_duration = to_movie(media_duration, ts, movie)?;
            if let (Some(mp), Some((g, gts))) = (min_pts(&t.samples), global_start) {
                // 空の編集（このトラックが遅れて始まる分）
                let g_in_track = g.rescale(Timebase::new(1, gts), ts).ok_or(Mp4Error::Timestamp("overflow"))?;
                let empty = to_movie((mp - g_in_track).max(0) as u64, ts, movie)?;
                let media_time = (mp - first_dts).max(0);
                // 表示される範囲: 最も早い PTS から最も遅い PTS + 長さまで
                let end = t.samples.iter().map(|s| s.dts + s.cts_offset as i64 + s.duration as i64).max().unwrap_or(mp);
                let shown = (end - mp).max(0) as u64;
                if empty > 0 {
                    edits.push((empty, -1));
                }
                if empty > 0 || media_time != 0 {
                    edits.push((to_movie(shown, ts, movie)?, media_time));
                }
                track_duration = empty + to_movie(shown, ts, movie)?;
            }
            movie_duration = movie_duration.max(track_duration);
            traks.push((t, track_duration, media_duration, edits));
        }
        let next = self.tracks.iter().map(|t| t.spec.track_id).max().unwrap_or(0) + 1;
        mvhd(&mut buf, movie_duration, next);
        for (t, track_duration, media_duration, edits) in &traks {
            let timing = TrakTiming { track_duration: *track_duration, media_duration: *media_duration, edits };
            trak(&mut buf, &t.spec, &timing, &mut |b| write_tables(b, t, base, co64))?;
        }
        crate::bmff::end_box(&mut buf, moov);
        Ok(buf)
    }
}

fn to_movie(v: u64, from: Timebase, movie: Timebase) -> Result<u64, Mp4Error> {
    let v = i64::try_from(v).map_err(|_| Mp4Error::Timestamp("overflow"))?;
    Ok(v.rescale(from, movie).ok_or(Mp4Error::Timestamp("overflow"))?.max(0) as u64)
}

fn min_pts(s: &[SampleInfo]) -> Option<i64> {
    s.iter().map(|s| s.dts + s.cts_offset as i64).min()
}

fn write_tables(buf: &mut Vec<u8>, t: &TrackState, base: u64, co64: bool) {
    let s = &t.samples;
    // stts（同じ長さを詰める）
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for x in s {
        match runs.last_mut() {
            Some((n, d)) if *d == x.duration => *n += 1,
            _ => runs.push((1, x.duration)),
        }
    }
    let p = begin_full_box(buf, b"stts", 0, 0);
    put_u32(buf, runs.len() as u32);
    for (n, d) in runs {
        put_u32(buf, n);
        put_u32(buf, d);
    }
    end_box(buf, p);
    // ctts（必要なときだけ。負があれば version 1）
    if s.iter().any(|x| x.cts_offset != 0) {
        let v1 = s.iter().any(|x| x.cts_offset < 0);
        let mut runs: Vec<(u32, i32)> = Vec::new();
        for x in s {
            match runs.last_mut() {
                Some((n, c)) if *c == x.cts_offset => *n += 1,
                _ => runs.push((1, x.cts_offset)),
            }
        }
        let p = begin_full_box(buf, b"ctts", if v1 { 1 } else { 0 }, 0);
        put_u32(buf, runs.len() as u32);
        for (n, c) in runs {
            put_u32(buf, n);
            put_u32(buf, c as u32);
        }
        end_box(buf, p);
    }
    // stss（映像で、全部が sync でないとき）
    if t.spec.kind() == MediaKind::Video && s.iter().any(|x| !x.keyframe) {
        let p = begin_full_box(buf, b"stss", 0, 0);
        let keys: Vec<u32> = s.iter().enumerate().filter(|(_, x)| x.keyframe).map(|(i, _)| i as u32 + 1).collect();
        put_u32(buf, keys.len() as u32);
        for k in keys {
            put_u32(buf, k);
        }
        end_box(buf, p);
    }
    // stsc（チャンクごとのサンプル数が変わるところだけ）
    let p = begin_full_box(buf, b"stsc", 0, 0);
    let mut entries: Vec<(u32, u32)> = Vec::new();
    for (i, c) in t.chunks.iter().enumerate() {
        if entries.last().map(|e| e.1) != Some(c.1) {
            entries.push((i as u32 + 1, c.1));
        }
    }
    put_u32(buf, entries.len() as u32);
    for (first, n) in entries {
        put_u32(buf, first);
        put_u32(buf, n);
        put_u32(buf, 1);
    }
    end_box(buf, p);
    // stsz
    let p = begin_full_box(buf, b"stsz", 0, 0);
    let same = s.first().map(|f| f.size).filter(|&z| s.iter().all(|x| x.size == z));
    match same {
        Some(z) => {
            put_u32(buf, z);
            put_u32(buf, s.len() as u32);
        }
        None => {
            put_u32(buf, 0);
            put_u32(buf, s.len() as u32);
            for x in s {
                put_u32(buf, x.size);
            }
        }
    }
    end_box(buf, p);
    // stco / co64
    let p = begin_full_box(buf, if co64 { b"co64" } else { b"stco" }, 0, 0);
    put_u32(buf, t.chunks.len() as u32);
    for c in &t.chunks {
        if co64 {
            put_u64(buf, base + c.2);
        } else {
            put_u32(buf, (base + c.2) as u32);
        }
    }
    end_box(buf, p);
}
