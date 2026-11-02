//! 実行の試験（本物のソフトウェアコーデック）: 合成（PinP・フェード・スレート）、板書の補正と変化の静止画、
//! 音声のミキシングとメーター、世代の切り替えと失敗、運用の操作。
//!
//! 時刻は入力時刻のモード（`clock: input`、`overflow: block`）で、結果が実行の速さに左右されないようにする。

mod common;

use common::*;
use senca_codec::registry::Policy;
use senca_compose::{Compositor, SourceId};
use senca_core::{Codec, Rescale, Timebase};
use senca_pipeline::control::Event;
use senca_pipeline::convert::ComposeNames;
use senca_pipeline::model::{ComposeNode, Node};
use senca_pipeline::runtime::RuntimeOutput;
use senca_pipeline::{Control, Graph, Runtime, RuntimeOptions};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const W: usize = 320;
const H: usize = 240;
const N: usize = 60;

fn options() -> serde_json::Value {
    // 負荷の高い機械でも結果が変わらないよう、待つ上限は長めにする（入力が来ないと判断するのは終わった時だけ）。
    json!({"clock": "input", "overflow": "block", "status_interval_ms": 50, "missing_timeout_ms": 30000, "block_timeout_ms": 60000})
}

fn compose_graph(enc_codec: &str, program: &str, extra_options: serde_json::Value) -> Graph {
    let mut opts = options();
    if let (Some(o), Some(e)) = (opts.as_object_mut(), extra_options.as_object()) {
        for (k, v) in e {
            o.insert(k.clone(), v.clone());
        }
    }
    let g = json!({
        "version": 1,
        "options": opts,
        "nodes": [
            {"type": "source", "id": "cam_a", "stream": "teacher", "video": {"codec": "vp9", "width": W, "height": H, "fps": 30}},
            {"type": "source", "id": "cam_b", "stream": "board", "video": {"codec": "av1", "width": W, "height": H, "fps": 30}},
            {"type": "decode", "id": "dec_a", "input": "cam_a.video"},
            {"type": "decode", "id": "dec_b", "input": "cam_b.video"},
            {"type": "compose", "id": "prod", "inputs": {"a": "dec_a", "b": "dec_b"},
             "canvas": {"width": W, "height": H, "fps_num": 30},
             "scenes": {
                "pinp": {"layers": [
                    {"source": "a", "x": 0, "y": 0, "width": W, "height": H, "fit": "stretch"},
                    {"source": "b", "x": 176, "y": 128, "width": 128, "height": 96, "fit": "stretch", "z": 1}
                ]},
                "a_only": {"layers": [{"source": "a", "x": 0, "y": 0, "width": W, "height": H, "fit": "stretch"}]},
                "b_only": {"layers": [{"source": "b", "x": 0, "y": 0, "width": W, "height": H, "fit": "stretch"}]}
             },
             "program": program,
             "preview": "b_only",
             "transition": {"kind": "fade", "duration_ms": 500},
             "missing": {"hold_ms": 200, "slate_color": "#ff00ff"},
             "threads": 4},
            {"type": "encode", "id": "enc", "input": "prod", "codec": enc_codec, "bitrate_kbps": 2500, "gop": 30},
            {"type": "sink", "id": "out", "inputs": ["enc"], "stream": "program"}
        ]
    });
    Graph::from_json(&g.to_string()).unwrap()
}

struct Sources {
    a: Vec<senca_image::I420Frame>,
    b: Vec<senca_image::I420Frame>,
    feeds: Vec<Feed>,
}

fn sources(reg: &senca_codec::ProviderRegistry, n_a: usize, n_b: usize) -> Sources {
    let a: Vec<_> = (0..n_a).map(|i| pattern_a(W, H, i)).collect();
    let b: Vec<_> = (0..n_b).map(|i| pattern_b(W, H, i)).collect();
    let (ca, aa) = encode_video(reg, Codec::Vp9, &a, 30, 3_000_000);
    let (cb, ab) = encode_video(reg, Codec::Av1, &b, 30, 3_000_000);
    Sources {
        a,
        b,
        feeds: vec![
            Feed {
                source: "cam_a".into(),
                config: ca,
                aus: aa,
            },
            Feed {
                source: "cam_b".into(),
                config: cb,
                aus: ab,
            },
        ],
    }
}

/// 実行して出力を集める（`before` は入力の前に呼ぶ）。
fn run(
    reg: Arc<senca_codec::ProviderRegistry>,
    g: &Graph,
    feeds: &[Feed],
    before: impl FnOnce(&Runtime),
) -> Vec<RuntimeOutput> {
    let rt = Runtime::start(g, RuntimeOptions::new(Policy::default(), reg)).unwrap();
    let col = collect(rt.take_outputs().unwrap());
    before(&rt);
    feed_all(&rt, feeds);
    for f in feeds {
        rt.end_input(&f.source);
    }
    rt.close(Duration::from_secs(120));
    drop(rt);
    col.join().unwrap()
}

/// 期待する合成（同じ合成器に元のフレームを入れて描く）。
fn expected(
    g: &Graph,
    scene: &str,
    a: &senca_image::I420Frame,
    b: &senca_image::I420Frame,
) -> senca_image::I420Frame {
    let Some(Node::Compose(c)) = g.node("prod") else {
        panic!()
    };
    let c: &ComposeNode = c;
    let names = ComposeNames::new(c);
    let mut comp = Compositor::new(senca_compose::Canvas::new(W, H, 30, 1), 1).unwrap();
    comp.set_program(names.scene(&c.scenes[scene]).unwrap())
        .unwrap();
    comp.push_frame(SourceId(0), Arc::new(a.clone()), 0);
    comp.push_frame(SourceId(1), Arc::new(b.clone()), 0);
    comp.render(0).unwrap()
}

#[test]
fn pinp_compose_vp9() {
    let Some(reg) = registry() else { return };
    let s = sources(&reg, N, N);
    let g = compose_graph("vp9", "pinp", json!({}));
    let outs = run(reg.clone(), &g, &s.feeds, |_| {});
    let (cfg, aus, batches) = track(&outs, "out", 0);
    let cfg = cfg.expect("codec config sent with the first batch");
    assert_eq!(cfg.codec, Codec::Vp9);
    assert!(batches[0].config.is_some());
    assert_eq!(batches[0].stream, "program");
    assert_eq!(aus.len(), N, "one output frame per canvas tick");
    assert!(aus[0].keyframe);
    assert!(aus.iter().all(|a| a.epoch == 1 && a.track.0 == 0));
    // 30 fps の一定の刻み
    for (i, a) in aus.iter().enumerate() {
        let us = a.pts.rescale(a.timebase, Timebase::MICROS).unwrap();
        assert!(
            (us - i as i64 * 1_000_000 / 30).abs() <= 20,
            "frame {i} at {us} us"
        );
    }
    // 復号して、期待する合成と比べる
    let dec = decode_video(&reg, &cfg, &aus);
    assert_eq!(dec.len(), N);
    let mut worst_full = 99.0f64;
    let mut worst_inset = 99.0f64;
    for (i, (_, f)) in dec.iter().enumerate() {
        assert_eq!((f.width(), f.height()), (W, H));
        let e = expected(&g, "pinp", &s.a[i], &s.b[i]);
        worst_full = worst_full.min(psnr_region(f.y(), e.y(), 0, 0, W, H));
        worst_inset = worst_inset.min(psnr_region(f.y(), e.y(), 180, 132, 300, 220));
    }
    eprintln!("pinp: worst PSNR full {worst_full:.1} dB, inset {worst_inset:.1} dB");
    assert!(worst_full > 28.0, "full-frame PSNR {worst_full:.1}");
    assert!(worst_inset > 26.0, "inset PSNR {worst_inset:.1}");
    // 子画面の中身は B（明るい）、外は A
    let (_, f) = &dec[30];
    assert!(mean(f.y(), 200, 140, 290, 210) > 150.0);
    assert!(mean(f.y(), 10, 10, 150, 100) < 130.0);
    // MP4 にして FFmpeg が誤りなく読める、フレーム数と長さ
    let path = tmp("pinp_vp9.mp4");
    write_mp4(&path, &[(cfg, aus)]);
    if let Some(r) = ffmpeg_reads(&path) {
        r.expect("ffmpeg decodes the pipeline output without errors");
        let (n, dur) = ffprobe_stream(&path, "v:0").expect("ffprobe");
        assert_eq!(n, N as u64);
        assert!((dur - 2.0).abs() < 0.05, "duration {dur}");
    } else {
        eprintln!("ffmpeg not found; skipped external check");
    }
    // 状態の通知: 入力は live/ended、タリー
    let ev = events(&outs);
    let last_status = ev
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Status {
                sources, compose, ..
            } => Some((sources.clone(), compose.clone())),
            _ => None,
        })
        .expect("status events");
    assert_eq!(last_status.1[0].program, "pinp");
    assert_eq!(last_status.1[0].frames, N as u64);
    assert!(
        last_status
            .0
            .iter()
            .all(|s| s.on_program && s.decoded_frames == N as u64)
    );
    assert!(last_status.0.iter().all(|s| s.state == "ended"));
}

#[test]
fn fade_transition_produces_intermediate_frames() {
    let Some(reg) = registry() else { return };
    let s = sources(&reg, 40, 40);
    let g = compose_graph("av1", "a_only", json!({}));
    let outs = run(reg.clone(), &g, &s.feeds, |rt| {
        // 入力の前に切り替えを出す: 最初の刻みから 500 ms のフェード
        rt.control(Control::SwitchProgram {
            node: "prod".into(),
            scene: "b_only".into(),
            transition: Some(senca_pipeline::model::TransitionSpec {
                kind: senca_pipeline::model::TransitionKindSpec::Fade,
                duration_ms: 500,
            }),
        })
        .unwrap();
    });
    let (cfg, aus, _) = track(&outs, "out", 0);
    let cfg = cfg.unwrap();
    assert_eq!(cfg.codec, Codec::Av1);
    assert_eq!(aus.len(), 40);
    let dec = decode_video(&reg, &cfg, &aus);
    assert_eq!(dec.len(), 40);
    let mut prev = 0.0;
    let mut intermediate = 0;
    for (i, (_, f)) in dec.iter().enumerate() {
        let ma = mean(s.a[i].y(), 0, 0, W, H);
        let mb = mean(s.b[i].y(), 0, 0, W, H);
        let p = (i as f64 / 15.0).min(1.0);
        let want = ma * (1.0 - p) + mb * p;
        let got = mean(f.y(), 0, 0, W, H);
        assert!(
            (got - want).abs() < 6.0,
            "frame {i}: mean {got:.1}, expected {want:.1}"
        );
        if p > 0.0 && p < 1.0 {
            intermediate += 1;
            assert!(got > prev - 1.0, "fade is monotonic at frame {i}");
        }
        prev = got;
    }
    assert_eq!(intermediate, 14);
    let path = tmp("fade_av1.mp4");
    write_mp4(&path, &[(cfg, aus)]);
    if let Some(r) = ffmpeg_reads(&path) {
        r.expect("ffmpeg decodes the AV1 output");
    }
}

#[test]
fn missing_source_shows_slate_after_hold() {
    let Some(reg) = registry() else { return };
    // B は 1 秒で終わり、A は 2 秒続く
    let s = sources(&reg, N, 30);
    let g = compose_graph("vp9", "pinp", json!({}));
    let outs = run(reg.clone(), &g, &s.feeds, |_| {});
    let (cfg, aus, _) = track(&outs, "out", 0);
    assert_eq!(aus.len(), N, "output keeps running when a source ends");
    let dec = decode_video(&reg, &cfg.unwrap(), &aus);
    // 子画面の色差: B は U=150 V=100、スレート（#ff00ff）は U・V とも高い
    let inset_uv =
        |f: &senca_image::I420Frame| (mean(f.u(), 95, 70, 145, 105), mean(f.v(), 95, 70, 145, 105));
    let (u, v) = inset_uv(&dec[20].1);
    assert!(
        (u - 150.0).abs() < 8.0 && (v - 100.0).abs() < 8.0,
        "live inset {u} {v}"
    );
    // 1 秒 + 保持 200 ms の後はスレート
    let (u, v) = inset_uv(&dec[45].1);
    assert!(u > 180.0 && v > 180.0, "slate inset {u} {v}");
    // 保持の間は最後のフレーム
    let (u, _) = inset_uv(&dec[32].1);
    assert!((u - 150.0).abs() < 8.0, "held inset {u}");
    let ev = events(&outs);
    let missing = ev.iter().rev().find_map(|e| match e {
        Event::Status { compose, .. } => Some(compose[0].missing.clone()),
        _ => None,
    });
    assert_eq!(missing, Some(vec!["b".to_string()]));
}

/// 斜めから撮ったホワイトボード（板の 4 隅は既知）。`strokes` 本の字を板の座標で描く。
fn board_frame(
    w: usize,
    h: usize,
    quad: &[senca_board::Point; 4],
    strokes: usize,
) -> senca_image::I420Frame {
    // 出力（板の座標）→ 入力の変換の逆: 入力の画素 → 板の座標
    let inv = senca_board::homography::rectify_homography(quad, w, h)
        .unwrap()
        .inverse()
        .unwrap();
    image(w, h, |x, y| {
        let p = inv.apply(senca_board::Point::new(x as f64 + 0.5, y as f64 + 0.5));
        match p {
            Some(q) if q.x >= 0.0 && q.y >= 0.0 && q.x < w as f64 && q.y < h as f64 => {
                let (u, v) = (q.x, q.y);
                // 字のような短い線の並び（1 文字 24 px、間 6 px）。長い 1 本の線は人の隠れと見分けがつかない。
                let letter = (u as usize) % 30 < 24;
                let stroke1 = strokes >= 1
                    && letter
                    && (100.0..124.0).contains(&v)
                    && (100.0..400.0).contains(&u);
                let stroke2 = strokes >= 2
                    && letter
                    && (200.0..222.0).contains(&v)
                    && (150.0..500.0).contains(&u);
                let lum = if stroke1 || stroke2 {
                    40
                } else {
                    (190.0 + 25.0 * u / w as f64) as u8
                };
                (lum, 128, 128)
            }
            _ => (50, 120, 135),
        }
    })
}

#[test]
fn board_keystone_and_change_snapshots() {
    let Some(reg) = registry() else { return };
    let (bw, bh) = (640usize, 360usize);
    let quad = [
        senca_board::Point::new(120.0, 60.0),
        senca_board::Point::new(540.0, 90.0),
        senca_board::Point::new(560.0, 320.0),
        senca_board::Point::new(90.0, 300.0),
    ];
    let frames: Vec<_> = (0..90)
        .map(|i| {
            board_frame(
                bw,
                bh,
                &quad,
                if i < 15 {
                    0
                } else if i < 50 {
                    1
                } else {
                    2
                },
            )
        })
        .collect();
    let (cfg, aus) = encode_video(&reg, Codec::Vp9, &frames, 30, 4_000_000);
    let g = json!({
        "version": 1,
        "options": options(),
        "nodes": [
            {"type": "source", "id": "boardcam", "stream": "board-raw", "video": {"codec": "vp9", "width": bw, "height": bh, "fps": 30}},
            {"type": "decode", "id": "dec", "input": "boardcam.video"},
            {"type": "board", "id": "fix", "input": "dec", "params": {
                "mode": "whiteboard",
                "corners": [[120.0, 60.0], [540.0, 90.0], [560.0, 320.0], [90.0, 300.0]],
                "output_width": bw, "output_height": bh, "enhance": true}},
            {"type": "snapshot_on_change", "id": "snap", "input": "fix", "params": {"analysis_fps": 0, "stable_frames": 3}},
            {"type": "encode", "id": "enc", "input": "fix", "codec": "vp9", "bitrate_kbps": 2000},
            {"type": "sink", "id": "out", "inputs": ["enc"], "stream": "board-clean"}
        ]
    });
    let g = Graph::from_json(&g.to_string()).unwrap();
    let feeds = vec![Feed {
        source: "boardcam".into(),
        config: cfg,
        aus,
    }];
    let outs = run(reg.clone(), &g, &feeds, |_| {});
    let (ocfg, oaus, _) = track(&outs, "out", 0);
    let ocfg = ocfg.unwrap();
    assert_eq!(oaus.len(), 90);
    let dec = decode_video(&reg, &ocfg, &oaus);
    let (_, f) = &dec[70];
    assert_eq!((f.width(), f.height()), (bw, bh));
    // 入力の左上は壁（暗い）、補正後は板（白）
    assert!(mean(frames[70].y(), 5, 5, 40, 40) < 60.0);
    assert!(
        mean(f.y(), 5, 5, 40, 40) > 170.0,
        "corrected corner is board white"
    );
    // 字は板の座標の場所に出る（台形補正）
    assert!(
        mean(f.y(), 150, 104, 350, 120) < 110.0,
        "stroke 1 at its board position"
    );
    assert!(
        mean(f.y(), 200, 205, 450, 218) < 110.0,
        "stroke 2 at its board position"
    );
    assert!(
        mean(f.y(), 150, 150, 350, 180) > 170.0,
        "background between strokes"
    );
    // 変化の静止画: 字が増えて落ち着くたびに 1 枚
    let snaps = snapshots(&outs);
    eprintln!(
        "snapshots: {:?}",
        snaps
            .iter()
            .map(|s| (s.pts_us, s.changed_fraction))
            .collect::<Vec<_>>()
    );
    assert!(snaps.len() >= 2, "{} snapshots", snaps.len());
    assert!(
        snaps[0].pts_us > 500_000,
        "first snapshot after stroke 1 appears"
    );
    assert!(
        snaps.iter().any(|s| s.pts_us > 50 * 1_000_000 / 30),
        "a snapshot after stroke 2"
    );
    let jpeg =
        senca_image::jpeg::decode_jpeg(&snaps[0].jpeg, &senca_image::Limits::default()).unwrap();
    assert_eq!((jpeg.width, jpeg.height), (bw, bh));
    let path = tmp("board_vp9.mp4");
    write_mp4(&path, &[(ocfg, oaus)]);
    if let Some(r) = ffmpeg_reads(&path) {
        r.expect("ffmpeg decodes the board output");
    }
}

/// 周波数 `f` の成分の振幅（Goertzel、左チャネル）。
fn tone_amplitude(pcm: &[f32], f: f32) -> f32 {
    let x: Vec<f32> = pcm.iter().step_by(2).copied().collect();
    let n = x.len() as f32;
    let k = 2.0 * std::f32::consts::PI * f / 48000.0;
    let (mut re, mut im) = (0f64, 0f64);
    for (i, v) in x.iter().enumerate() {
        re += (*v as f64) * ((k * i as f32) as f64).cos();
        im += (*v as f64) * ((k * i as f32) as f64).sin();
    }
    (2.0 * (re * re + im * im).sqrt() / n as f64) as f32
}

#[test]
fn audio_mix_opus_with_meters() {
    let Some(reg) = registry() else { return };
    let (ca, aa) = encode_opus(&reg, &sine(440.0, 0.25, 3.0));
    let (cb, ab) = encode_opus(&reg, &sine(1000.0, 0.25, 3.0));
    let g = json!({
        "version": 1,
        "options": options(),
        "nodes": [
            {"type": "source", "id": "mic_a", "stream": "teacher", "audio": {"codec": "opus", "channels": 2}},
            {"type": "source", "id": "mic_b", "stream": "room", "audio": {"codec": "opus", "channels": 2}},
            {"type": "decode", "id": "da", "input": "mic_a.audio"},
            {"type": "decode", "id": "db", "input": "mic_b.audio"},
            {"type": "audio_mix", "id": "mix", "inputs": {
                "a": {"from": "da"},
                "b": {"from": "db", "gain_db": -6.0}
            }},
            {"type": "encode", "id": "aenc", "input": "mix", "codec": "opus", "bitrate_kbps": 128},
            {"type": "sink", "id": "out", "inputs": ["aenc"], "stream": "program"}
        ]
    });
    let g = Graph::from_json(&g.to_string()).unwrap();
    let feeds = vec![
        Feed {
            source: "mic_a".into(),
            config: ca,
            aus: aa,
        },
        Feed {
            source: "mic_b".into(),
            config: cb,
            aus: ab,
        },
    ];
    let outs = run(reg.clone(), &g, &feeds, |_| {});
    let (cfg, aus, _) = track(&outs, "out", 0);
    let cfg = cfg.unwrap();
    assert_eq!(cfg.codec, Codec::Opus);
    assert!(aus.len() >= 145, "{} packets", aus.len());
    // 時刻は 20 ms ずつ続く
    for w in aus.windows(2) {
        assert_eq!(w[1].pts - w[0].pts, 960);
    }
    let pcm = decode_opus(&reg, &cfg, &aus);
    // 100 ms ごとの RMS: 途切れが無い（2 つの正弦波の和の RMS は約 0.198）
    let rms: Vec<f32> = pcm
        .chunks(9600)
        .map(|c| (c.iter().map(|v| v * v).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    for (k, r) in rms.iter().enumerate().skip(1).take(27) {
        assert!((r - 0.198).abs() < 0.02, "100 ms window {k}: rms {r}");
    }
    let mid = &pcm[48000 * 2 / 2..48000 * 2 * 5 / 2];
    let a = tone_amplitude(mid, 440.0);
    let b = tone_amplitude(mid, 1000.0);
    eprintln!("tone amplitudes: 440 Hz {a:.3}, 1000 Hz {b:.3}");
    assert!((a - 0.25).abs() < 0.04, "440 Hz amplitude {a}");
    assert!(
        (b - 0.125).abs() < 0.03,
        "1000 Hz amplitude {b} (gain -6 dB)"
    );

    // メーター
    let meters: Vec<_> = events(&outs)
        .into_iter()
        .filter_map(|e| match e {
            Event::Meters { inputs, master, .. } => Some((inputs, master)),
            _ => None,
        })
        .collect();
    assert!(!meters.is_empty(), "meter events");
    let (inputs, master) = meters.last().unwrap();
    let pa = inputs.iter().find(|m| m.name == "a").unwrap();
    let pb = inputs.iter().find(|m| m.name == "b").unwrap();
    eprintln!(
        "meters: a {:?} b {:?} master {:?}",
        pa.peak_db, pb.peak_db, master.peak_db
    );
    assert!(
        (pa.peak_db[0] + 12.0).abs() < 1.5,
        "a peak {:?}",
        pa.peak_db
    );
    assert!(
        (pb.peak_db[0] + 18.0).abs() < 1.5,
        "b peak {:?}",
        pb.peak_db
    );
    assert!((pa.rms_db[0] + 15.0).abs() < 1.5, "a rms {:?}", pa.rms_db);
    assert!(
        master.peak_db[0] > -10.0 && master.peak_db[0] < -7.0,
        "master {:?}",
        master.peak_db
    );
    assert_eq!(pb.gain_db, -6.0);
    let path = tmp("mix_opus.mp4");
    write_mp4(&path, &[(cfg, aus)]);
    if let Some(r) = ffmpeg_reads(&path) {
        r.expect("ffmpeg decodes the opus output");
    }
}

#[test]
fn operator_controls() {
    let Some(reg) = registry() else { return };
    let s = sources(&reg, 30, 30);
    let mut g = compose_graph("vp9", "pinp", json!({}));
    // プレビューも符号化して出す
    g.nodes.push(serde_json::from_value(json!({"type": "encode", "id": "penc", "input": "prod.preview", "codec": "vp8", "bitrate_kbps": 500})).unwrap());
    g.nodes.push(
        serde_json::from_value(
            json!({"type": "sink", "id": "pv", "inputs": ["penc"], "stream": "preview"}),
        )
        .unwrap(),
    );
    let rt = Runtime::start(&g, RuntimeOptions::new(Policy::default(), reg.clone())).unwrap();
    let col = collect(rt.take_outputs().unwrap());
    let err = |c: Control| rt.control(c).unwrap_err().to_string();
    assert!(
        err(Control::SwitchProgram {
            node: "prod".into(),
            scene: "nope".into(),
            transition: None
        })
        .contains("unknown scene")
    );
    assert!(
        err(Control::SwitchProgram {
            node: "nope".into(),
            scene: "pinp".into(),
            transition: None
        })
        .contains("unknown compose")
    );
    assert!(
        err(Control::SetAudio {
            node: "prod".into(),
            input: "a".into(),
            gain_db: Some(0.0),
            mute: None,
            solo: None,
            pan: None,
            delay_ms: None
        })
        .contains("audio_mix")
    );
    assert!(
        err(Control::Snapshot {
            node: "prod".into(),
            enabled: true
        })
        .contains("snapshot_on_change")
    );
    // 不正なレイヤーの変更は拒否し、元のまま
    let bad = Control::UpdateLayer {
        node: "prod".into(),
        scene: "pinp".into(),
        layer: 1,
        patch: senca_pipeline::control::LayerPatch {
            opacity: Some(3.0),
            ..Default::default()
        },
    };
    assert!(err(bad).contains("opacity"));
    assert!(
        err(Control::UpdateLayer {
            node: "prod".into(),
            scene: "pinp".into(),
            layer: 9,
            patch: Default::default()
        })
        .contains("no layer")
    );
    // プレビュー → take（カット）: プログラムが b_only、プレビューが pinp に
    rt.control(Control::SetPreview {
        node: "prod".into(),
        scene: "b_only".into(),
    })
    .unwrap();
    rt.control(Control::Take {
        node: "prod".into(),
        transition: Some(senca_pipeline::model::TransitionSpec::default()),
    })
    .unwrap();
    // レイヤーを隠す（プレビュー側の pinp の子画面）
    rt.control(Control::UpdateLayer {
        node: "prod".into(),
        scene: "pinp".into(),
        layer: 1,
        patch: senca_pipeline::control::LayerPatch {
            visible: Some(false),
            ..Default::default()
        },
    })
    .unwrap();
    rt.control(Control::RequestKeyframe {
        node: Some("enc".into()),
    })
    .unwrap();
    assert!(
        rt.control(Control::RequestKeyframe {
            node: Some("zzz".into())
        })
        .is_err()
    );
    let st = rt.status();
    let Event::Status {
        compose, sources, ..
    } = &st[0]
    else {
        panic!()
    };
    assert_eq!(compose[0].program, "b_only");
    assert_eq!(compose[0].preview.as_deref(), Some("pinp"));
    assert_eq!(compose[0].program_inputs, vec!["b".to_string()]);
    assert_eq!(compose[0].preview_inputs, vec!["a".to_string()]);
    let tally: Vec<(String, bool, bool)> = sources
        .iter()
        .map(|s| (s.id.clone(), s.on_program, s.on_preview))
        .collect();
    assert!(tally.contains(&("cam_a".into(), false, true)), "{tally:?}");
    assert!(tally.contains(&("cam_b".into(), true, false)), "{tally:?}");
    feed_all(&rt, &s.feeds);
    for f in &s.feeds {
        rt.end_input(&f.source);
    }
    rt.close(Duration::from_secs(120));
    drop(rt);
    let outs = col.join().unwrap();
    // プログラムは B 全面（明るい）、プレビューは A だけ（子画面は隠した）
    let (pcfg, paus, _) = track(&outs, "out", 0);
    let prog = decode_video(&reg, &pcfg.unwrap(), &paus);
    assert!(mean(prog[10].1.y(), 10, 10, 150, 100) > 150.0);
    let (vcfg, vaus, _) = track(&outs, "pv", 0);
    let vcfg = vcfg.unwrap();
    assert_eq!(vcfg.codec, Codec::Vp8);
    assert_eq!(vaus.len(), 30);
    let pv = decode_video(&reg, &vcfg, &vaus);
    assert!(
        (mean(pv[10].1.u(), 95, 70, 145, 105) - 110.0).abs() < 8.0,
        "preview inset hidden -> shows A"
    );
}

#[test]
fn generation_commit_failure_and_rollback() {
    let Some(reg) = registry() else { return };
    let n = 240; // 8 秒
    let s = sources(&reg, n, n);
    let g1 = compose_graph("vp9", "pinp", json!({"commit_timeout_ms": 1500}));
    let rt =
        Arc::new(Runtime::start(&g1, RuntimeOptions::new(Policy::default(), reg.clone())).unwrap());
    let col = collect(rt.take_outputs().unwrap());
    let feeder = {
        let rt = rt.clone();
        let feeds: Vec<Feed> = s
            .feeds
            .iter()
            .map(|f| Feed {
                source: f.source.clone(),
                config: f.config.clone(),
                aus: f.aus.clone(),
            })
            .collect();
        std::thread::spawn(move || {
            // ゆっくり入れる（切り替えの間も入力が続くように）
            let mut items: Vec<(i64, usize, usize)> = Vec::new();
            for (k, f) in feeds.iter().enumerate() {
                for (j, a) in f.aus.iter().enumerate() {
                    items.push((a.pts.rescale(a.timebase, Timebase::MICROS).unwrap(), k, j));
                }
            }
            items.sort();
            let mut sent = [false; 2];
            for (_, k, j) in items {
                let cfg = (!sent[k]).then(|| feeds[k].config.clone());
                sent[k] = true;
                rt.push_input(&feeds[k].source, cfg, vec![feeds[k].aus[j].clone()])
                    .unwrap();
                std::thread::sleep(Duration::from_millis(8));
            }
            for f in &feeds {
                rt.end_input(&f.source);
            }
        })
    };
    std::thread::sleep(Duration::from_millis(500));
    // 1. H.264 の世代は準備で拒否（GPU なし）。active は 1 のまま。
    let e = rt
        .prepare(&compose_graph("h264", "pinp", json!({})))
        .unwrap_err();
    assert_eq!(e.api_code(), "CAPACITY_UNAVAILABLE", "{e}");
    assert_eq!(rt.active_generation(), Some(1));
    // 2. 準備して rollback
    let (g2, plan) = rt
        .prepare(&compose_graph("av1", "b_only", json!({})))
        .unwrap();
    assert_eq!(plan.sinks[0].tracks[0].codec, "av1");
    rt.rollback(g2).unwrap();
    assert!(rt.commit(g2).is_err());
    // 3. 入力の来ない世代: キーフレームの境界に届かず時間切れ → 失敗、旧世代が続く
    let stuck = json!({
        "version": 1, "options": {"clock": "input", "overflow": "block", "block_timeout_ms": 60000, "commit_timeout_ms": 800},
        "nodes": [
            {"type": "source", "id": "cam_z", "stream": "nobody", "video": {"codec": "vp9"}},
            {"type": "decode", "id": "dz", "input": "cam_z.video"},
            {"type": "encode", "id": "enc", "input": "dz", "codec": "vp9", "bitrate_kbps": 500, "width": W, "height": H},
            {"type": "sink", "id": "out", "inputs": ["enc"], "stream": "program"}
        ]
    });
    let (g3, _) = rt
        .prepare(&Graph::from_json(&stuck.to_string()).unwrap())
        .unwrap();
    let e = rt.commit(g3).unwrap_err();
    assert_eq!(e.api_code(), "CAPACITY_UNAVAILABLE", "{e}");
    assert_eq!(rt.active_generation(), Some(1));
    assert_eq!(
        rt.generation_state(g3),
        Some(senca_pipeline::GenerationState::Failed)
    );
    // 4. AV1 の b_only へ切り替える（次のキーフレームで）
    let (g4, _) = rt
        .prepare(&compose_graph("av1", "b_only", json!({})))
        .unwrap();
    rt.commit(g4).unwrap();
    assert_eq!(rt.active_generation(), Some(g4));
    assert_eq!(
        rt.generation_state(1),
        Some(senca_pipeline::GenerationState::Retired)
    );
    feeder.join().unwrap();
    let rt = Arc::try_unwrap(rt).ok().expect("feeder done");
    rt.close(Duration::from_secs(120));
    drop(rt);
    let outs = col.join().unwrap();
    let (_, aus, batches) = track(&outs, "out", 0);
    // 出力の時刻は単調に増え、世代は 1 → g4 の順で 1 回だけ切り替わる
    for w in aus.windows(2) {
        assert!(w[1].pts > w[0].pts, "pts increase across the switch");
    }
    let gens: Vec<u64> = batches.iter().map(|b| b.generation).collect();
    let switch_at = gens
        .iter()
        .position(|g| *g == g4)
        .expect("new generation output");
    assert!(gens[..switch_at].iter().all(|g| *g == 1));
    assert!(gens[switch_at..].iter().all(|g| *g == g4));
    let first_new = &batches[switch_at];
    let ncfg = first_new.config.clone().expect("new config at the switch");
    assert_eq!(ncfg.codec, Codec::Av1);
    assert!(first_new.aus[0].keyframe, "switch at a keyframe");
    assert_eq!(first_new.aus[0].epoch, g4);
    // 両方の区間がそれぞれ復号できる
    let old: Vec<_> = aus.iter().filter(|a| a.epoch == 1).cloned().collect();
    let new: Vec<_> = aus.iter().filter(|a| a.epoch == g4).cloned().collect();
    let old_cfg = batches[0].config.clone().unwrap();
    assert_eq!(decode_video(&reg, &old_cfg, &old).len(), old.len());
    let nd = decode_video(&reg, &ncfg, &new);
    assert_eq!(nd.len(), new.len());
    assert!(
        mean(nd[nd.len() / 2].1.y(), 10, 10, 150, 100) > 150.0,
        "new generation shows b_only"
    );
    let total = old.len() + new.len();
    assert!(total > n / 2, "{total} frames");
    let ev = events(&outs);
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::GenerationFailed { generation, .. } if *generation == g3))
    );
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::GenerationRolledBack { generation } if *generation == g2))
    );
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::GenerationCommitted { generation } if *generation == g4))
    );
}
