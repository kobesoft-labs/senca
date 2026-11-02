//! ライブ HLS のパッケージャー。stream タスクの中で AU を 1 回だけセグメントにする（ADR-0006）。
//! セグメントの区切りは `senca-hls::Segmenter`、入れ物は TS（`senca-mpegts`）か fMP4（`senca-fmp4`）。

use bytes::{Bytes, BytesMut};
use senca_control::model::{LatencyProfile, Stream};
use senca_core::{AccessUnit, CodecConfig, MediaKind, Rescale, Timebase, TrackId};
use senca_hls::segmenter::{PlaylistKind, SegmentEvent, Segmenter, SegmenterConfig, SimpleUris, SkipRequest};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct LiveSegment {
    pub name: String,
    pub epoch: u64,
    pub sequence: u64,
    pub duration_ms: u32,
    pub wall_ms: i64,
    pub discontinuity: bool,
    pub bytes: Bytes,
    /// この時点のプレイリスト（通常・delta）。
    pub playlist: String,
    pub playlist_skip: Option<String>,
    pub removed: Vec<u64>,
}

#[derive(Debug, Clone)]
pub struct LivePart {
    pub sequence: u64,
    pub part: u32,
    pub bytes: Bytes,
    pub playlist: String,
    pub playlist_skip: Option<String>,
}

#[derive(Debug)]
pub enum PackagerOut {
    Segment(LiveSegment),
    Part(LivePart),
    Init { bytes: Bytes, name: String },
}

enum Mux {
    Ts(senca_mpegts::TsMuxer),
    Fmp4 { writer: senca_fmp4::FragmentWriter, tracks: BTreeMap<TrackId, (u32, u32)>, pending: BTreeMap<TrackId, Vec<AccessUnit>> },
}

pub struct Packager {
    container: &'static str,
    target_us: u64,
    part_us: Option<u64>,
    window: usize,
    configs: BTreeMap<TrackId, CodecConfig>,
    seg: Option<Segmenter>,
    mux: Option<Mux>,
    buf: BytesMut,
    parts: Vec<Bytes>,
    cur: Option<(u64, bool, i64, u64)>, // (seq, discontinuity, wall_ms, discontinuity_index)
    epoch: u64,
    mux_disc: Option<u64>,
    resume_seq: Option<u64>,
    init_name: Option<String>,
    init_bytes: Option<Bytes>,
    removed: Vec<u64>,
}

impl Packager {
    pub fn new(s: &Stream) -> Self {
        let container = if s.hls.container == "fmp4" { "fmp4" } else { "ts" };
        let part_us = (s.latency_profile == LatencyProfile::Low).then_some(500_000u64.min(s.hls.segment_ms as u64 * 250));
        Packager {
            container,
            target_us: s.hls.segment_ms as u64 * 1000,
            part_us: part_us.map(|p| p.max(200_000)),
            window: s.hls.window as usize,
            configs: BTreeMap::new(),
            seg: None,
            mux: None,
            buf: BytesMut::new(),
            parts: Vec::new(),
            cur: None,
            epoch: 0,
            mux_disc: None,
            resume_seq: None,
            init_name: None,
            init_bytes: None,
            removed: Vec::new(),
        }
    }

    pub fn container(&self) -> &'static str {
        self.container
    }

    pub fn has_open_segment(&self) -> bool {
        self.cur.is_some()
    }

    pub fn init_segment(&self) -> Option<(Bytes, String)> {
        Some((self.init_bytes.clone()?, self.init_name.clone()?))
    }

    fn ext(&self) -> &'static str {
        if self.container == "fmp4" { "m4s" } else { "ts" }
    }

    pub fn config(&mut self, track: TrackId, c: &CodecConfig) -> Vec<PackagerOut> {
        let new_track = !self.configs.contains_key(&track);
        self.configs.insert(track, c.clone());
        let mut out = Vec::new();
        if new_track && self.seg.is_some() {
            // トラックの構成が変わった: 今の区切りを閉じて作り直す
            out.extend(self.end());
        } else if let Some(Mux::Ts(m)) = self.mux.as_mut() {
            let _ = m.update_config(track, c);
        } else if self.mux.is_some() && self.container == "fmp4" {
            // 設定が変わった fMP4: 不連続にして、次の区間で新しい init を作る
            self.epoch += 1;
        }
        out
    }

    fn ensure(&mut self) -> bool {
        if self.seg.is_none() {
            let video = self.configs.iter().find(|(_, c)| c.codec.kind() == MediaKind::Video).map(|(t, _)| *t);
            let audio = self.configs.iter().find(|(_, c)| c.codec.kind() == MediaKind::Audio).map(|(t, _)| *t);
            if video.is_none() && audio.is_none() {
                return false;
            }
            let mut cfg = SegmenterConfig::live(video, audio);
            cfg.target_duration_us = self.target_us;
            cfg.part_target_us = self.part_us;
            cfg.window = self.window;
            cfg.kind = PlaylistKind::Live;
            cfg.delta_updates = self.part_us.is_some();
            cfg.first_sequence = self.next_first_sequence();
            match Segmenter::new(cfg) {
                Ok(s) => self.seg = Some(s),
                Err(_) => return false,
            }
        }
        true
    }

    fn next_first_sequence(&self) -> u64 {
        self.resume_seq.unwrap_or(0)
    }

    /// 入れ物を作る（区間の先頭で）。fMP4 は区間ごとに init を作る。
    fn make_mux(&mut self, disc_index: u64, out: &mut Vec<PackagerOut>) -> bool {
        match self.container {
            "fmp4" => {
                let mut tracks = BTreeMap::new();
                let mut specs = Vec::new();
                for (i, (t, c)) in self.configs.iter().filter(|(_, c)| c.codec.kind() != MediaKind::Subtitle).enumerate() {
                    let spec = senca_fmp4::TrackSpec::new(i as u32 + 1, c.clone());
                    tracks.insert(*t, (spec.track_id, spec.timescale));
                    specs.push(spec);
                }
                let mut init = Vec::new();
                if senca_fmp4::write_init_segment(&specs, &mut init).is_err() {
                    return false;
                }
                let name = format!("init{disc_index}.mp4");
                let bytes = Bytes::from(init);
                self.init_name = Some(name.clone());
                self.init_bytes = Some(bytes.clone());
                out.push(PackagerOut::Init { bytes, name });
                self.mux = Some(Mux::Fmp4 { writer: senca_fmp4::FragmentWriter::new(), tracks, pending: BTreeMap::new() });
            }
            _ => {
                let cfg = senca_mpegts::MuxerConfig { pcr_delay: 9000, ..Default::default() };
                let mut m = senca_mpegts::TsMuxer::new(cfg);
                for (t, c) in &self.configs {
                    if m.add_track(*t, c).is_err() {
                        return false;
                    }
                }
                self.mux = Some(Mux::Ts(m));
            }
        }
        self.mux_disc = Some(disc_index);
        true
    }

    pub fn discontinuity(&mut self) {
        self.epoch += 1;
    }

    pub fn push(&mut self, au: &AccessUnit) -> Vec<PackagerOut> {
        let mut out = Vec::new();
        if !self.ensure() {
            return out;
        }
        let mut au = au.clone();
        au.epoch = self.epoch.max(au.epoch);
        let mut evs = Vec::new();
        if self.seg.as_mut().unwrap().push(au, &mut evs).is_err() {
            return out;
        }
        self.handle(evs, &mut out);
        out
    }

    /// 配信の終わり: 作りかけのセグメントを閉じる。次の publish では新しい窓で続きの番号から始める。
    pub fn end(&mut self) -> Vec<PackagerOut> {
        let mut out = Vec::new();
        if let Some(seg) = self.seg.as_mut() {
            let mut evs = Vec::new();
            seg.finish(&mut evs);
            self.handle(evs, &mut out);
            let last = self.seg.as_ref().and_then(|s| s.last_complete());
            self.resume_seq = last.map(|l| l + 1).or(self.resume_seq);
        }
        self.seg = None;
        self.mux = None;
        self.mux_disc = None;
        self.cur = None;
        out
    }

    fn handle(&mut self, evs: Vec<SegmentEvent>, out: &mut Vec<PackagerOut>) {
        for ev in evs {
            match ev {
                SegmentEvent::SegmentStart { seq, discontinuity, discontinuity_index, program_date_time_ms, .. } => {
                    self.buf.clear();
                    self.parts.clear();
                    let wall = program_date_time_ms.unwrap_or_else(crate::util::now_ms);
                    self.cur = Some((seq, discontinuity, wall, discontinuity_index));
                    if self.mux_disc != Some(discontinuity_index) && (self.container == "fmp4" || self.mux.is_none()) {
                        self.make_mux(discontinuity_index, out);
                    }
                    if let Some(Mux::Ts(m)) = self.mux.as_mut() {
                        m.start_segment(&mut self.buf);
                    }
                }
                SegmentEvent::PartStart { .. } => {}
                SegmentEvent::Au { au, .. } => match self.mux.as_mut() {
                    Some(Mux::Ts(m)) => {
                        let _ = m.push(&au, &mut self.buf);
                    }
                    Some(Mux::Fmp4 { pending, .. }) => pending.entry(au.track).or_default().push(au),
                    None => {}
                },
                SegmentEvent::PartEnd { seq, part, .. } => {
                    self.close_fragment();
                    let b = self.buf.split().freeze();
                    self.parts.push(b.clone());
                    let (pl, pls) = self.render();
                    out.push(PackagerOut::Part(LivePart { sequence: seq, part, bytes: b, playlist: pl, playlist_skip: pls }));
                }
                SegmentEvent::SegmentEnd { seq, duration_us } => {
                    self.close_fragment();
                    let rest = self.buf.split().freeze();
                    let bytes = if self.parts.is_empty() {
                        rest
                    } else {
                        let mut all = BytesMut::with_capacity(self.parts.iter().map(|p| p.len()).sum::<usize>() + rest.len());
                        for p in self.parts.drain(..) {
                            all.extend_from_slice(&p);
                        }
                        all.extend_from_slice(&rest);
                        all.freeze()
                    };
                    let (s0, disc, wall, _) = self.cur.take().unwrap_or((seq, false, crate::util::now_ms(), 0));
                    debug_assert_eq!(s0, seq);
                    let (pl, pls) = self.render();
                    out.push(PackagerOut::Segment(LiveSegment {
                        name: format!("{seq}.{}", self.ext()),
                        epoch: self.epoch,
                        sequence: seq,
                        duration_ms: (duration_us / 1000) as u32,
                        wall_ms: wall,
                        discontinuity: disc,
                        bytes,
                        playlist: pl,
                        playlist_skip: pls,
                        removed: std::mem::take(&mut self.removed),
                    }));
                }
                SegmentEvent::SegmentRemoved { seq } => self.removed.push(seq),
            }
        }
    }

    fn close_fragment(&mut self) {
        let Some(Mux::Fmp4 { writer, tracks, pending }) = self.mux.as_mut() else { return };
        let mut runs_data: Vec<(u32, Vec<senca_fmp4::Sample>)> = Vec::new();
        for (t, aus) in pending.iter_mut() {
            let Some(&(tid, ts)) = tracks.get(t) else { continue };
            let tsb = Timebase::new(1, ts);
            let mut samples = Vec::with_capacity(aus.len());
            let dts: Vec<i64> = aus.iter().map(|a| a.dts.rescale(a.timebase, tsb).unwrap_or(0)).collect();
            for (i, a) in aus.iter().enumerate() {
                let pts = a.pts.rescale(a.timebase, tsb).unwrap_or(dts[i]);
                let dur = if i + 1 < dts.len() {
                    (dts[i + 1] - dts[i]).max(0) as u32
                } else if a.duration > 0 {
                    (a.duration as i64).rescale(a.timebase, tsb).unwrap_or(0) as u32
                } else if i > 0 {
                    ((dts[i] - dts[0]) / i as i64).max(0) as u32
                } else if a.codec.kind() == MediaKind::Audio {
                    if a.codec == senca_core::Codec::Opus { 960 } else { 1024 }
                } else {
                    ts / 30
                };
                samples.push(senca_fmp4::Sample {
                    dts: dts[i],
                    duration: dur,
                    cts_offset: (pts - dts[i]) as i32,
                    keyframe: a.keyframe || a.codec.kind() == MediaKind::Audio,
                    data: a.payload.clone(),
                });
            }
            aus.clear();
            if !samples.is_empty() {
                runs_data.push((tid, samples));
            }
        }
        if runs_data.is_empty() {
            return;
        }
        let runs: Vec<senca_fmp4::TrackRun> = runs_data.iter().map(|(id, s)| senca_fmp4::TrackRun { track_id: *id, samples: s }).collect();
        let mut v = Vec::new();
        if writer.write(&runs, &mut v).is_ok() {
            self.buf.extend_from_slice(&v);
        }
    }

    fn render(&self) -> (String, Option<String>) {
        let Some(seg) = self.seg.as_ref() else { return (String::new(), None) };
        let uris = SimpleUris { prefix: String::new(), ext: self.ext().into(), fmp4_init: self.container == "fmp4" };
        let pl = seg.playlist(&uris, SkipRequest::No).to_m3u8().unwrap_or_default();
        let pls = self.part_us.and_then(|_| seg.playlist(&uris, SkipRequest::Yes).to_m3u8().ok());
        (pl, pls)
    }
}
