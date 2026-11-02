//! 録画の MP4 書き出し（REQ-006）。セグメントを分離して、先頭に moov を置いた MP4 にする。
//! `not_before_ms`・`not_after_ms` で切り出す（キーフレームの境目まで広げる。再エンコードしない）。

use crate::app::App;
use bytes::{Bytes, BytesMut};
use senca_control::{Problem, ProblemCode};
use senca_core::{AccessUnit, CodecConfig, Rescale, Timebase, TrackId};
use std::collections::BTreeMap;

/// 書き出せる大きさの上限（メモリに組み立てるため）。
const MAX_EXPORT: u64 = 4 << 30;

pub fn mp4(app: &App, id: &str) -> Result<Bytes, Problem> {
    let idx = app.read_index(id).ok_or_else(Problem::not_found)?;
    if idx.bytes() > MAX_EXPORT {
        return Err(Problem::new(ProblemCode::PayloadTooLarge, "録画が大きすぎて書き出せません"));
    }
    let mut configs: BTreeMap<TrackId, CodecConfig> = BTreeMap::new();
    let mut aus: Vec<AccessUnit> = Vec::new();
    // 不連続（epoch）をまたぐと時刻が飛ぶので、区間ごとにずらしてつなぐ
    let mut offset_us: i64 = 0;
    let mut last_end_us: Option<i64> = None;
    let mut cur_epoch = None;
    let mut first_us: Option<i64> = None;
    let mut epoch_base: Option<i64> = None;
    let read = |p: &str| std::fs::read(app.cfg.data_dir.join(p)).map(Bytes::from);
    let mut fmp4_reader = None;
    let mut tracks: BTreeMap<u32, (u32, CodecConfig)> = BTreeMap::new();
    if let Some((p, _)) = &idx.init {
        let b = read(p).map_err(|_| Problem::internal())?;
        let (init, _) = senca_fmp4::read_file(&b).map_err(|_| Problem::internal())?;
        for t in &init.tracks {
            if let Some(c) = &t.config {
                tracks.insert(t.track_id, (t.timescale, c.clone()));
                configs.insert(TrackId(t.track_id - 1), c.clone());
            }
        }
        fmp4_reader = Some(senca_fmp4::Mp4Reader::new(init));
    }
    for seg in &idx.segments {
        let b = read(&seg.path).map_err(|_| Problem::new(ProblemCode::InternalError, "セグメントが読めません"))?;
        let mut seg_aus = Vec::new();
        if let Some(r) = fmp4_reader.as_mut() {
            for (n, s) in r.read_fragments(&b).map_err(|_| Problem::internal())?.into_iter().enumerate() {
                let Some((ts, c)) = tracks.get(&s.track_id) else { continue };
                seg_aus.push(AccessUnit {
                    track: TrackId(s.track_id - 1),
                    codec: c.codec,
                    epoch: seg.epoch,
                    sequence: n as u64,
                    config_generation: c.generation,
                    pts: s.dts + s.cts_offset as i64,
                    dts: s.dts,
                    duration: s.duration as u64,
                    timebase: Timebase::new(1, *ts),
                    keyframe: s.keyframe,
                    discontinuity: false,
                    capture_time_ns: None,
                    payload: s.data,
                });
            }
        } else {
            let mut d = senca_mpegts::TsDemuxer::new(Default::default());
            d.push(&b);
            d.flush();
            while let Some(ev) = d.poll() {
                match ev {
                    senca_mpegts::DemuxEvent::Config { track, config } => {
                        configs.insert(track, config);
                    }
                    senca_mpegts::DemuxEvent::AccessUnit(au) => seg_aus.push(au),
                    _ => {}
                }
            }
        }
        if cur_epoch != Some(seg.epoch) {
            // 新しい区間: 前の区間の終わりに続ける
            cur_epoch = Some(seg.epoch);
            let start = seg_aus.iter().map(|a| a.dts.rescale(a.timebase, Timebase::MICROS).unwrap_or(0)).min().unwrap_or(0);
            epoch_base = Some(start);
            offset_us = last_end_us.unwrap_or(0);
        }
        for mut a in seg_aus {
            let base = epoch_base.unwrap_or(0);
            let d = a.dts.rescale(a.timebase, Timebase::MICROS).unwrap_or(0) - base + offset_us;
            let p = a.pts.rescale(a.timebase, Timebase::MICROS).unwrap_or(0) - base + offset_us;
            a.dts = d;
            a.pts = p;
            a.timebase = Timebase::MICROS;
            first_us.get_or_insert(d);
            last_end_us = Some(last_end_us.unwrap_or(0).max(d + 1));
            aus.push(a);
        }
        last_end_us = Some(offset_us + seg_end_us(&aus, offset_us));
    }
    if aus.is_empty() {
        return Err(Problem::new(ProblemCode::Conflict, "録画が空です"));
    }
    // 切り出し（壁時計 → 録画の先頭からの位置）
    let wall0 = idx.segments.first().map(|s| s.wall_ms).unwrap_or(idx.started_at_ms);
    let t0 = first_us.unwrap_or(0);
    let cut_from = idx.not_before_ms.map(|w| (w - wall0).max(0) * 1000 + t0);
    let cut_to = idx.end.as_ref().and_then(|e| e.2).map(|w| (w - wall0).max(0) * 1000 + t0);
    let video = configs.iter().find(|(_, c)| c.codec.kind() == senca_core::MediaKind::Video).map(|(t, _)| *t);
    if let Some(from) = cut_from {
        // from 以前で最後のキーフレームから
        let start = aus
            .iter()
            .filter(|a| Some(a.track) == video && a.keyframe && a.pts <= from)
            .map(|a| a.dts)
            .max()
            .unwrap_or(i64::MIN);
        aus.retain(|a| a.dts >= start.min(from));
    }
    if let Some(to) = cut_to {
        aus.retain(|a| a.pts <= to);
    }
    let mut specs = Vec::new();
    let mut ids = BTreeMap::new();
    for (i, (t, c)) in configs.iter().enumerate() {
        let s = senca_fmp4::TrackSpec::new(i as u32 + 1, c.clone());
        ids.insert(*t, (s.track_id, s.timescale));
        specs.push(s);
    }
    let mut w = senca_fmp4::Mp4Writer::new(specs).map_err(|_| Problem::internal())?;
    // トラックごとに長さを決めて追加
    let mut by_track: BTreeMap<TrackId, Vec<&AccessUnit>> = BTreeMap::new();
    for a in &aus {
        by_track.entry(a.track).or_default().push(a);
    }
    let mut order: Vec<(i64, u32, senca_fmp4::Sample)> = Vec::new();
    for (t, list) in by_track {
        let Some(&(tid, ts)) = ids.get(&t) else { continue };
        let tb = Timebase::new(1, ts);
        for (i, a) in list.iter().enumerate() {
            let dts = a.dts.rescale(a.timebase, tb).unwrap_or(0);
            let next = list.get(i + 1).map(|n| n.dts.rescale(n.timebase, tb).unwrap_or(dts));
            let dur = match next {
                Some(n) if n > dts => (n - dts) as u32,
                _ => {
                    if i > 0 {
                        ((dts - list[0].dts.rescale(a.timebase, tb).unwrap_or(0)) / i as i64).max(1) as u32
                    } else {
                        1
                    }
                }
            };
            let pts = a.pts.rescale(a.timebase, tb).unwrap_or(dts);
            order.push((
                a.dts,
                tid,
                senca_fmp4::Sample { dts, duration: dur, cts_offset: (pts - dts) as i32, keyframe: a.keyframe, data: a.payload.clone() },
            ));
        }
    }
    // 交互配置（DTS 順）
    order.sort_by_key(|(d, t, _)| (*d, *t));
    for (_, tid, s) in &order {
        w.add(*tid, s).map_err(|_| Problem::internal())?;
    }
    let head = w.finish().map_err(|_| Problem::internal())?;
    let mut out = BytesMut::with_capacity(head.len() + order.iter().map(|(_, _, s)| s.data.len()).sum::<usize>());
    out.extend_from_slice(&head);
    for (_, _, s) in &order {
        out.extend_from_slice(&s.data);
    }
    Ok(out.freeze())
}

fn seg_end_us(aus: &[AccessUnit], offset: i64) -> i64 {
    aus.iter().map(|a| a.dts + (a.duration as i64).rescale(a.timebase, Timebase::MICROS).unwrap_or(0)).max().unwrap_or(offset) - offset + 1
}
