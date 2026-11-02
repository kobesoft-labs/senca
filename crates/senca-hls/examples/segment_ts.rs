//! FFmpeg が作った MPEG-TS を Segmenter で切り、segment・part のファイルと playlist を書く（相互運用試験用）。
//!
//! 使い方: `segment_ts <in.ts> <outdir> [--kind vod|live|event] [--target-ms N] [--part-ms N] [--window N] [--gap-at-sec S]`
//!
//! TS の多重化は別の crate の担当なので、ここでは TS packet を PES 単位でまとめたものを AU の payload とし、
//! 各 segment / part の先頭に PAT・PMT の packet を写すだけの最小の「多重化」をする。
//! keyframe は adaptation field の random_access_indicator で判定する。
//! `--gap-at-sec` を付けると、その時刻の後の映像 AU を GOP の途中から数枚捨て、不連続として渡す。

use bytes::Bytes;
use senca_core::time::{RolloverExtender, Timebase};
use senca_core::{AccessUnit, Codec, TrackId};
use senca_hls::{PlaylistKind, SegmentEvent, Segmenter, SegmenterConfig, SimpleUris, SkipRequest, UriScheme};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

const VIDEO: TrackId = TrackId(0);
const AUDIO: TrackId = TrackId(1);

struct Pending {
    track: TrackId,
    codec: Codec,
    packets: Vec<u8>,
    pts: i64,
    dts: i64,
    keyframe: bool,
}

fn read_ts_time(b: &[u8]) -> u64 {
    (((b[0] as u64 >> 1) & 7) << 30) | ((b[1] as u64) << 22) | ((b[2] as u64 >> 1) << 15) | ((b[3] as u64) << 7) | (b[4] as u64 >> 1)
}

/// TS を読み、PES ごとの AU を返す（ファイル順）。PAT と PMT の packet も返す。
fn demux(data: &[u8]) -> (Vec<AccessUnit>, Vec<u8>) {
    let mut pmt_pid = None;
    let mut streams: HashMap<u16, (TrackId, Codec)> = HashMap::new();
    let mut psi = Vec::new();
    let mut pending: HashMap<u16, Pending> = HashMap::new();
    let mut out = Vec::new();
    let mut seq = [0u64; 2];
    let mut ext: HashMap<u16, (RolloverExtender, RolloverExtender)> = HashMap::new();
    let mut flush = |p: Pending, out: &mut Vec<AccessUnit>| {
        let s = &mut seq[p.track.0 as usize];
        out.push(AccessUnit {
            track: p.track,
            codec: p.codec,
            epoch: 0,
            sequence: *s,
            config_generation: 0,
            pts: p.pts,
            dts: p.dts,
            duration: 0,
            timebase: Timebase::MPEG,
            keyframe: p.keyframe,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from(p.packets),
        });
        *s += 1;
    };
    for pkt in data.as_chunks::<188>().0 {
        if pkt[0] != 0x47 {
            continue;
        }
        let pid = (((pkt[1] & 0x1f) as u16) << 8) | pkt[2] as u16;
        let pusi = pkt[1] & 0x40 != 0;
        let afc = (pkt[3] >> 4) & 3;
        let mut off = 4;
        let mut rai = false;
        if afc & 2 != 0 {
            let len = pkt[4] as usize;
            rai = len > 0 && pkt[5] & 0x40 != 0;
            off = 5 + len;
        }
        if afc & 1 == 0 || off >= 188 {
            continue;
        }
        let payload = &pkt[off..];
        if pid == 0 && pusi {
            let sec = &payload[1 + payload[0] as usize..];
            let slen = (((sec[1] & 0x0f) as usize) << 8) | sec[2] as usize;
            let mut i = 8;
            while i + 4 <= 3 + slen - 4 {
                let prog = ((sec[i] as u16) << 8) | sec[i + 1] as u16;
                if prog != 0 {
                    pmt_pid = Some((((sec[i + 2] & 0x1f) as u16) << 8) | sec[i + 3] as u16);
                    break;
                }
                i += 4;
            }
            if psi.len() < 188 * 2 {
                psi.extend_from_slice(pkt);
            }
            continue;
        }
        if Some(pid) == pmt_pid && pusi {
            let sec = &payload[1 + payload[0] as usize..];
            let slen = (((sec[1] & 0x0f) as usize) << 8) | sec[2] as usize;
            let pil = (((sec[10] & 0x0f) as usize) << 8) | sec[11] as usize;
            let mut i = 12 + pil;
            while i + 5 <= 3 + slen - 4 {
                let st = sec[i];
                let epid = (((sec[i + 1] & 0x1f) as u16) << 8) | sec[i + 2] as u16;
                let esl = (((sec[i + 3] & 0x0f) as usize) << 8) | sec[i + 4] as usize;
                match st {
                    0x1b => {
                        streams.insert(epid, (VIDEO, Codec::H264));
                    }
                    0x0f => {
                        streams.insert(epid, (AUDIO, Codec::Aac));
                    }
                    _ => {}
                }
                i += 5 + esl;
            }
            if psi.len() < 188 * 2 {
                psi.extend_from_slice(pkt);
            }
            continue;
        }
        let Some(&(track, codec)) = streams.get(&pid) else { continue };
        if pusi {
            if let Some(p) = pending.remove(&pid) {
                flush(p, &mut out);
            }
            if payload.len() < 14 || payload[..3] != [0, 0, 1] {
                continue;
            }
            let flags = payload[7] >> 6;
            let (ep, ed) = ext.entry(pid).or_insert_with(|| (RolloverExtender::new(33), RolloverExtender::new(33)));
            let pts = ep.extend(read_ts_time(&payload[9..14]));
            let dts = if flags == 3 && payload.len() >= 19 { ed.extend(read_ts_time(&payload[14..19])) } else { pts };
            pending.insert(pid, Pending { track, codec, packets: pkt.to_vec(), pts, dts, keyframe: rai || track == AUDIO });
        } else if let Some(p) = pending.get_mut(&pid) {
            p.packets.extend_from_slice(pkt);
        }
    }
    let mut rest: Vec<_> = pending.into_values().collect();
    rest.sort_by_key(|p| p.dts);
    for p in rest {
        flush(p, &mut out);
    }
    (out, psi)
}

fn arg<T: std::str::FromStr>(args: &[String], name: &str) -> Option<T> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: segment_ts <in.ts> <outdir> [--kind vod|live|event] [--target-ms N] [--part-ms N] [--window N] [--gap-at-sec S]");
        std::process::exit(2);
    }
    let input = std::fs::read(&args[1])?;
    let outdir = PathBuf::from(&args[2]);
    std::fs::create_dir_all(&outdir)?;
    let kind = match arg::<String>(&args, "--kind").as_deref() {
        Some("live") => PlaylistKind::Live,
        Some("event") => PlaylistKind::Event,
        _ => PlaylistKind::Vod,
    };
    let (aus, psi) = demux(&input);
    let has_audio = aus.iter().any(|a| a.track == AUDIO);
    let has_video = aus.iter().any(|a| a.track == VIDEO);
    let mut cfg = SegmenterConfig::live(has_video.then_some(VIDEO), has_audio.then_some(AUDIO));
    cfg.kind = kind;
    cfg.target_duration_us = arg::<u64>(&args, "--target-ms").unwrap_or(2000) * 1000;
    cfg.part_target_us = arg::<u64>(&args, "--part-ms").map(|v| v * 1000);
    cfg.window = arg(&args, "--window").unwrap_or(6);
    cfg.delta_updates = cfg.part_target_us.is_some() && kind == PlaylistKind::Live;
    let gap_at = arg::<f64>(&args, "--gap-at-sec").map(|s| (s * 90_000.0) as i64);
    let uris = SimpleUris { prefix: "seg".into(), ext: "ts".into(), fmp4_init: false };
    let mut seg = Segmenter::new(cfg)?;

    let mut ev = Vec::new();
    let mut seg_file: Option<File> = None;
    let mut part_file: Option<File> = None;
    let open = |p: &Path| -> std::io::Result<File> {
        let mut f = File::create(p)?;
        f.write_all(&psi)?;
        Ok(f)
    };
    let mut gap_state = 0; // 0: 前, 1: 捨てている, 2: 後
    let mut dropped_by_gap = 0;
    let mut midway_written = false;
    let total = aus.len();
    for (i, mut au) in aus.into_iter().enumerate() {
        if let Some(g) = gap_at
            && au.track == VIDEO
        {
            if gap_state == 0 && au.dts >= g && !au.keyframe {
                gap_state = 1;
            }
            if gap_state == 1 {
                if dropped_by_gap < 5 {
                    dropped_by_gap += 1;
                    continue;
                }
                au.discontinuity = true;
                gap_state = 2;
            }
        }
        seg.push(au, &mut ev)?;
        for e in ev.drain(..) {
            match e {
                SegmentEvent::SegmentStart { seq, .. } => seg_file = Some(open(&outdir.join(uris.segment(seq)))?),
                SegmentEvent::PartStart { seq, part, .. } => part_file = Some(open(&outdir.join(uris.part(seq, part)))?),
                SegmentEvent::Au { au, part, .. } => {
                    if let Some(f) = seg_file.as_mut() {
                        f.write_all(&au.payload)?;
                    }
                    if part.is_some()
                        && let Some(f) = part_file.as_mut()
                    {
                        f.write_all(&au.payload)?;
                    }
                }
                SegmentEvent::PartEnd { .. } => part_file = None,
                SegmentEvent::SegmentEnd { .. } => seg_file = None,
                SegmentEvent::SegmentRemoved { .. } => {}
            }
        }
        if !midway_written && i >= total / 2 {
            // 途中の playlist（live なら blocking reload の判定も確かめられる形）
            let pl = seg.playlist(&uris, SkipRequest::No);
            std::fs::write(outdir.join("midway.m3u8"), pl.to_m3u8()?)?;
            if seg.config().delta_updates {
                std::fs::write(outdir.join("midway_delta.m3u8"), seg.playlist(&uris, SkipRequest::Yes).to_m3u8()?)?;
            }
            midway_written = true;
        }
    }
    seg.finish(&mut ev);
    for e in ev.drain(..) {
        if let SegmentEvent::SegmentEnd { .. } = e {
            seg_file = None;
            part_file = None;
        }
    }
    drop((seg_file, part_file));
    let pl = seg.playlist(&uris, SkipRequest::No);
    pl.validate()?;
    std::fs::write(outdir.join("index.m3u8"), pl.to_m3u8()?)?;
    eprintln!(
        "segments={} media_sequence={} target={} dropped={} discontinuity_sequence={}",
        pl.segments.len(),
        pl.media_sequence,
        pl.target_duration,
        seg.dropped(),
        pl.discontinuity_sequence
    );
    Ok(())
}
