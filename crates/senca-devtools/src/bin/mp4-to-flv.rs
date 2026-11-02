//! MP4（progressive / fragmented）を FLV にする。H.265・AV1・VP9・Opus は Enhanced RTMP の形で書く。
//! FFmpeg 7.0 の FLV 書き出しが AV1 などに対応していないため、試験の素材作りに使う。
//!
//! `mp4-to-flv <in.mp4> <out.flv>`

use bytes::Bytes;
use senca_core::{AccessUnit, Timebase, TrackId};
use std::io::Write;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 3 {
        eprintln!("usage: mp4-to-flv <in.mp4> <out.flv>");
        std::process::exit(2);
    }
    let data = Bytes::from(std::fs::read(&a[1]).expect("read input"));
    let (init, samples) = senca_fmp4::read_file(&data).expect("parse mp4");
    let mut mux = senca_flv::build::FlvMuxer::new();
    let mut tracks = std::collections::BTreeMap::new();
    let (mut has_a, mut has_v) = (false, false);
    for (i, t) in init.tracks.iter().enumerate() {
        if let Some(c) = &t.config {
            mux.set_config(c.clone()).expect("config");
            tracks.insert(t.track_id, (TrackId(i as u32), t.timescale, c.clone()));
            match c.codec.kind() {
                senca_core::MediaKind::Video => has_v = true,
                senca_core::MediaKind::Audio => has_a = true,
                _ => {}
            }
        }
    }
    let mut aus: Vec<AccessUnit> = samples
        .into_iter()
        .filter_map(|s| {
            let (track, ts, c) = tracks.get(&s.track_id)?.clone();
            let tb = Timebase::new(1, ts);
            let to_ms = |v: i64| senca_core::Rescale::rescale(v, tb, Timebase::MILLIS).unwrap_or(0);
            Some(AccessUnit {
                track,
                codec: c.codec,
                epoch: 0,
                sequence: 0,
                config_generation: c.generation,
                pts: to_ms(s.dts + s.cts_offset as i64),
                dts: to_ms(s.dts),
                duration: 0,
                timebase: Timebase::MILLIS,
                keyframe: s.keyframe,
                discontinuity: false,
                capture_time_ns: None,
                payload: s.data,
            })
        })
        .collect();
    aus.sort_by_key(|a| (a.dts, a.track.0));
    // FLV の時刻は符号なし。負の時刻（Opus の先読み分など）があれば全体をずらす
    let min = aus.iter().map(|a| a.dts.min(a.pts)).min().unwrap_or(0);
    if min < 0 {
        for a in &mut aus {
            a.dts -= min;
            a.pts -= min;
        }
    }
    let mut out = Vec::new();
    senca_flv::file::write_header(senca_flv::file::FlvHeader { has_audio: has_a, has_video: has_v }, &mut out);
    let mut tags = Vec::new();
    for au in &aus {
        mux.access_unit(au, &mut tags).expect("mux");
    }
    for t in &tags {
        senca_flv::file::write_out_tag(t, &mut out).expect("tag");
    }
    std::fs::File::create(&a[2]).and_then(|mut f| f.write_all(&out)).expect("write output");
}
