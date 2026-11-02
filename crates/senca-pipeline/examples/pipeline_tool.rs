//! 相互運用の試験用の道具: MP4 の入力（senca-fmp4 で読む）でパイプラインを動かし、最初の sink を MP4 に書く。
//!
//! ```text
//! pipeline_tool <graph.json> <out.mp4> <source_id>=<in.mp4>... [--snapshots DIR]
//! ```
//!
//! 入力の時刻で動かす（グラフの `options.clock` は `input` を推奨）。最後に 1 行の要約（JSON）を出す。
//! コーデックは `SENCA_CODEC_LIB_DIR` から読み込む（試験の道具なので、このプロセスで読み込む）。

use senca_codec::registry::{Policy, ProbeOptions, ProviderRegistry};
use senca_core::{AccessUnit, CodecConfig, MediaKind, Rescale, Timebase, TrackId};
use senca_fmp4::{Mp4Writer, SampleInfo, TrackSpec};
use senca_pipeline::runtime::RuntimeOutput;
use senca_pipeline::{Event, Graph, Runtime, RuntimeOptions};
use std::collections::BTreeMap;
use std::time::Duration;

fn die(m: impl std::fmt::Display) -> ! {
    eprintln!("pipeline_tool: {m}");
    std::process::exit(1);
}

struct Input {
    source: String,
    items: Vec<(i64, Option<CodecConfig>, AccessUnit)>,
}

fn load(source: &str, path: &str) -> Input {
    let data =
        bytes::Bytes::from(std::fs::read(path).unwrap_or_else(|e| die(format!("{path}: {e}"))));
    let (init, samples) =
        senca_fmp4::read_file(&data).unwrap_or_else(|e| die(format!("{path}: {e:?}")));
    let mut items = Vec::new();
    let mut first: BTreeMap<u32, bool> = BTreeMap::new();
    for s in samples {
        let Some(t) = init.tracks.iter().find(|t| t.track_id == s.track_id) else {
            continue;
        };
        let Some(cfg) = &t.config else { continue };
        let tb = Timebase::new(1, t.timescale);
        let au = AccessUnit {
            track: TrackId(s.track_id),
            codec: cfg.codec,
            epoch: 0,
            sequence: 0,
            config_generation: cfg.generation,
            pts: s.dts + s.cts_offset as i64,
            dts: s.dts,
            duration: s.duration as u64,
            timebase: tb,
            keyframe: s.keyframe,
            discontinuity: false,
            capture_time_ns: None,
            payload: s.data,
        };
        let us = s.dts.rescale(tb, Timebase::MICROS).unwrap_or(0);
        let cfg_once = (!first.contains_key(&s.track_id)).then(|| cfg.clone());
        first.insert(s.track_id, true);
        items.push((us, cfg_once, au));
    }
    Input {
        source: source.to_string(),
        items,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        die(
            "usage: pipeline_tool <graph.json> <out.mp4> <source_id>=<in.mp4>... [--snapshots DIR]",
        );
    }
    let graph_json =
        std::fs::read_to_string(&args[0]).unwrap_or_else(|e| die(format!("{}: {e}", args[0])));
    let graph = Graph::from_json(&graph_json).unwrap_or_else(|e| die(e));
    let out_path = &args[1];
    let mut inputs = Vec::new();
    let mut snap_dir = None;
    let mut it = args[2..].iter();
    while let Some(a) = it.next() {
        if a == "--snapshots" {
            snap_dir = Some(
                it.next()
                    .unwrap_or_else(|| die("--snapshots needs a directory"))
                    .clone(),
            );
            continue;
        }
        let (s, p) = a
            .split_once('=')
            .unwrap_or_else(|| die(format!("bad input {a}")));
        inputs.push(load(s, p));
    }
    let registry = std::sync::Arc::new(ProviderRegistry::probe(&ProbeOptions::from_env()));
    let rt = Runtime::start(&graph, RuntimeOptions::new(Policy::default(), registry))
        .unwrap_or_else(|e| die(e));
    let rx = rt.take_outputs().expect("outputs");
    let collector = std::thread::spawn(move || rx.into_iter().collect::<Vec<_>>());
    // 時刻の順に 1 つずつ入れる
    let mut order: Vec<(i64, usize, usize)> = Vec::new();
    for (k, i) in inputs.iter().enumerate() {
        for (j, (us, _, _)) in i.items.iter().enumerate() {
            order.push((*us, k, j));
        }
    }
    order.sort();
    for (_, k, j) in order {
        let (_, cfg, au) = &inputs[k].items[j];
        if let Err(e) = rt.push_input(&inputs[k].source, cfg.clone(), vec![au.clone()]) {
            die(e);
        }
    }
    for i in &inputs {
        rt.end_input(&i.source);
    }
    rt.close(Duration::from_secs(300));
    drop(rt);
    let outs = collector.join().expect("collector");
    // 最初の sink のトラック
    let mut tracks: BTreeMap<(String, u32), (Option<CodecConfig>, Vec<AccessUnit>)> =
        BTreeMap::new();
    let mut snaps = 0;
    let mut last_meters = None;
    let mut errors = Vec::new();
    for o in &outs {
        match o {
            RuntimeOutput::Media(m) => {
                let e = tracks.entry((m.sink.clone(), m.track)).or_default();
                if m.config.is_some() {
                    e.0 = m.config.clone();
                }
                e.1.extend(m.aus.iter().cloned());
            }
            RuntimeOutput::Snapshot(s) => {
                if let Some(d) = &snap_dir {
                    let p = format!("{d}/snap{snaps}.jpg");
                    std::fs::write(&p, &s.jpeg).unwrap_or_else(|e| die(format!("{p}: {e}")));
                }
                snaps += 1;
            }
            RuntimeOutput::Event(Event::Meters { .. }) => {
                if let RuntimeOutput::Event(e) = o {
                    last_meters = Some(serde_json::to_value(e).unwrap_or_default());
                }
            }
            RuntimeOutput::Event(Event::Error {
                node,
                code,
                message,
            }) => {
                errors.push(format!("{node:?} {code} {message}"));
            }
            RuntimeOutput::Event(_) => {}
        }
    }
    let first_sink = tracks.keys().next().map(|k| k.0.clone());
    let mine: Vec<(CodecConfig, Vec<AccessUnit>)> = tracks
        .iter()
        .filter(|(k, _)| Some(&k.0) == first_sink.as_ref())
        .filter_map(|(_, (c, a))| c.clone().map(|c| (c, a.clone())))
        .collect();
    let mut summary = serde_json::json!({
        "snapshots": snaps,
        "errors": errors,
        "meters": last_meters,
    });
    for (c, a) in &mine {
        let k = if c.codec.kind() == MediaKind::Video {
            "video"
        } else {
            "audio"
        };
        summary[k] = serde_json::json!({"codec": c.codec.as_str(), "units": a.len()});
    }
    if !mine.is_empty() {
        write_mp4(out_path, &mine);
    }
    println!("{summary}");
}

fn write_mp4(path: &str, tracks: &[(CodecConfig, Vec<AccessUnit>)]) {
    let specs: Vec<TrackSpec> = tracks
        .iter()
        .enumerate()
        .map(|(i, (c, _))| TrackSpec::new(i as u32 + 1, c.clone()))
        .collect();
    let mut w = Mp4Writer::new(specs.clone()).unwrap_or_else(|e| die(format!("{e:?}")));
    let mut items: Vec<(i64, usize, usize)> = Vec::new();
    for (k, (_, aus)) in tracks.iter().enumerate() {
        for (j, a) in aus.iter().enumerate() {
            items.push((
                a.dts.rescale(a.timebase, Timebase::MICROS).unwrap_or(0),
                k,
                j,
            ));
        }
    }
    items.sort();
    let mut payload = Vec::new();
    for (_, k, j) in items {
        let aus = &tracks[k].1;
        let a = &aus[j];
        let tb = Timebase::new(1, specs[k].timescale);
        let dts = a.dts.rescale(a.timebase, tb).unwrap_or(0);
        let next = aus
            .get(j + 1)
            .and_then(|n| n.dts.rescale(n.timebase, tb))
            .unwrap_or(
                dts + (a.duration as i64)
                    .rescale(a.timebase, tb)
                    .unwrap_or(1)
                    .max(1),
            );
        let pts = a.pts.rescale(a.timebase, tb).unwrap_or(dts);
        w.add_sample(
            k as u32 + 1,
            SampleInfo {
                dts,
                duration: (next - dts).max(1) as u32,
                cts_offset: (pts - dts) as i32,
                keyframe: a.keyframe,
                size: a.payload.len() as u32,
            },
        )
        .unwrap_or_else(|e| die(format!("{e:?}")));
        payload.extend_from_slice(&a.payload);
    }
    let mut file = w.finish().unwrap_or_else(|e| die(format!("{e:?}")));
    file.extend_from_slice(&payload);
    std::fs::write(path, file).unwrap_or_else(|e| die(format!("{path}: {e}")));
}
