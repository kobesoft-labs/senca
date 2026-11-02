//! セッション（パイプラインを worker の中で動かし続ける）を、本物の worker のプロセスで試す。
//!
//! - 開く → 実時間で入力 → 操作（フェードで切り替え）→ 出力（復号できる、フレーム数）→ 閉じる
//! - 拒否（H.264 は GPU なしで CAPACITY_UNAVAILABLE。親の事前検証と worker の中の検証の両方）
//! - テナントごとの上限
//! - 異常終了の検出と作り直し（`--fault session-crash`）
//! - 入力の背圧（worker が遅れたら古い AU を捨て、映像は次のキーフレームから）

use bytes::Bytes;
use senca_codec::config::{DecoderConfig, EncoderConfig, VideoEncoderConfig};
use senca_codec::dl::LibrarySearch;
use senca_codec::frame::{Frame, PixelFormat, VideoFrame};
use senca_codec::registry::{Policy, ProbeOptions, ProviderRegistry};
use senca_codec_worker::session_pool::SessionFailure;
use senca_codec_worker::{SessionEventItem, SessionOutputItem, SessionPool, SessionPoolConfig};
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase};
use senca_pipeline::control::Event;
use senca_pipeline::model::{TransitionKindSpec, TransitionSpec};
use senca_pipeline::{Control, Graph};
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

const LIB_DIR: &str = "/home/cc/tools/codeclibs/usr/lib/x86_64-linux-gnu";
const W: u32 = 192;
const H: u32 = 144;

fn lib_dir() -> PathBuf {
    std::env::var_os("SENCA_CODEC_LIB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(LIB_DIR))
}

fn registry() -> Option<ProviderRegistry> {
    let mut o = ProbeOptions::from_env();
    o.libs = LibrarySearch::new(vec![lib_dir()]);
    let r = ProviderRegistry::probe(&o);
    let ok = r
        .get(senca_codec::ProviderId::Libvpx)
        .is_some_and(|p| p.status().available);
    if !ok {
        if std::env::var("SENCA_CODEC_REQUIRE_SW").as_deref() == Ok("1") {
            panic!("libvpx is required");
        }
        eprintln!("skipped: libvpx unavailable");
        return None;
    }
    Some(r)
}

fn config(args: &[&str]) -> SessionPoolConfig {
    let mut c = SessionPoolConfig::new(PathBuf::from(env!("CARGO_BIN_EXE_senca-codec-worker")));
    c.worker.args = args.iter().map(|s| s.into()).collect();
    c.worker.env.retain(|(k, _)| k != "SENCA_CODEC_LIB_DIR");
    c.worker
        .env
        .push(("SENCA_CODEC_LIB_DIR".into(), lib_dir().into_os_string()));
    c.control_timeout = Duration::from_secs(30);
    c
}

/// 単色に近い模様（`y` が明るさ）。
fn frame(y: u8, i: u32) -> VideoFrame {
    let (cw, ch) = (W.div_ceil(2), H.div_ceil(2));
    let mut d = Vec::with_capacity(PixelFormat::I420.packed_size(W, H));
    for yy in 0..H {
        for x in 0..W {
            let bar = (x + i * 3) % 48 < 4;
            d.push(if bar { y.saturating_add(40) } else { y } ^ ((yy & 1) as u8));
        }
    }
    d.extend(std::iter::repeat_n(128u8, (cw * ch * 2) as usize));
    VideoFrame::from_packed(
        PixelFormat::I420,
        W,
        H,
        Bytes::from(d),
        i as i64 * 3000,
        Timebase::MPEG,
    )
    .unwrap()
}

fn encode(reg: &ProviderRegistry, y: u8, n: u32) -> (CodecConfig, Vec<AccessUnit>) {
    let mut cfg = VideoEncoderConfig::new(Codec::Vp9, W, H, 30, 800_000);
    cfg.gop = 15;
    let (_, mut enc) = reg
        .create_encoder(&EncoderConfig::Video(cfg), &Policy::default())
        .unwrap();
    let mut aus = Vec::new();
    for i in 0..n {
        enc.send(&Frame::Video(frame(y, i))).unwrap();
        while let Some(a) = enc.receive().unwrap() {
            aus.push(a);
        }
    }
    enc.flush().unwrap();
    while let Some(a) = enc.receive().unwrap() {
        aus.push(a);
    }
    (enc.codec_config().unwrap(), aus)
}

fn graph(enc_codec: &str, options: serde_json::Value) -> Graph {
    let g = json!({
        "version": 1,
        "options": options,
        "nodes": [
            {"type": "source", "id": "cam_a", "stream": "teacher", "video": {"codec": "vp9", "width": W, "height": H, "fps": 30}},
            {"type": "source", "id": "cam_b", "stream": "board", "video": {"codec": "vp9", "width": W, "height": H, "fps": 30}},
            {"type": "decode", "id": "da", "input": "cam_a.video"},
            {"type": "decode", "id": "db", "input": "cam_b.video"},
            {"type": "compose", "id": "prod", "inputs": {"a": "da", "b": "db"},
             "canvas": {"width": W, "height": H, "fps_num": 30},
             "scenes": {
                "pinp": {"layers": [
                    {"source": "a", "x": 0, "y": 0, "width": W, "height": H, "fit": "stretch"},
                    {"source": "b", "x": 96, "y": 72, "width": 80, "height": 60, "fit": "stretch", "z": 1}]},
                "b_only": {"layers": [{"source": "b", "x": 0, "y": 0, "width": W, "height": H, "fit": "stretch"}]}
             },
             "program": "pinp"},
            {"type": "encode", "id": "enc", "input": "prod", "codec": enc_codec, "bitrate_kbps": 800, "gop": 30},
            {"type": "sink", "id": "out", "inputs": ["enc"], "stream": "program"}
        ]
    });
    Graph::from_json(&g.to_string()).unwrap()
}

fn decode_count(reg: &ProviderRegistry, cfg: &CodecConfig, aus: &[AccessUnit]) -> usize {
    let dc = DecoderConfig::new(cfg.codec).with_config(cfg.clone());
    let (_, mut dec) = reg.create_decoder(&dc, &Policy::default()).unwrap();
    let mut n = 0;
    for a in aus {
        dec.send(a).unwrap();
        while dec.receive().unwrap().is_some() {
            n += 1;
        }
    }
    dec.flush().unwrap();
    while dec.receive().unwrap().is_some() {
        n += 1;
    }
    n
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn realtime_session_through_worker_process() {
    let Some(reg) = registry() else { return };
    let (ca, aa) = encode(&reg, 60, 75);
    let (cb, ab) = encode(&reg, 190, 75);
    let pool = SessionPool::new(config(&[]));
    let mut s = pool
        .open_session("tenant-a", Policy::royalty_free(), &graph("vp9", json!({})))
        .await
        .expect("open");
    assert_eq!(s.plan().sinks[0].tracks[0].codec, "vp9");
    assert_eq!(pool.stats().active, 1);
    let mut outs = s.outputs().unwrap();
    let mut evs = s.events().unwrap();
    let collector = tokio::spawn(async move {
        let mut v = Vec::new();
        while let Some(o) = outs.recv().await {
            v.push(o);
        }
        v
    });
    let ev_collector = tokio::spawn(async move {
        let mut v = Vec::new();
        while let Some(e) = evs.recv().await {
            v.push(e);
        }
        v
    });
    // 実時間（30 fps）で 2.5 秒入れる。1 秒でフェードの切り替え。
    let mut tick = tokio::time::interval(Duration::from_micros(33_333));
    for i in 0..75 {
        tick.tick().await;
        let first = i == 0;
        s.send_input("cam_a", first.then(|| ca.clone()), vec![aa[i].clone()])
            .unwrap();
        s.send_input("cam_b", first.then(|| cb.clone()), vec![ab[i].clone()])
            .unwrap();
        if i == 30 {
            s.control(&Control::SwitchProgram {
                node: "prod".into(),
                scene: "b_only".into(),
                transition: Some(TransitionSpec {
                    kind: TransitionKindSpec::Fade,
                    duration_ms: 300,
                }),
            })
            .await
            .unwrap();
            let e = s
                .control(&Control::SwitchProgram {
                    node: "prod".into(),
                    scene: "nope".into(),
                    transition: None,
                })
                .await
                .unwrap_err();
            assert_eq!(e.cause, SessionFailure::Control);
            assert!(e.message.contains("unknown scene"), "{e}");
            s.control(&Control::RequestKeyframe { node: None })
                .await
                .unwrap();
        }
    }
    s.end_input("cam_a").unwrap();
    s.end_input("cam_b").unwrap();
    s.close().await.unwrap();
    let outs = collector.await.unwrap();
    let evs = ev_collector.await.unwrap();
    assert_eq!(pool.stats().active, 0);
    let mut cfg = None;
    let mut aus = Vec::new();
    for o in &outs {
        if let SessionOutputItem::Media {
            sink,
            stream,
            track,
            config,
            aus: a,
            ..
        } = o
        {
            assert_eq!(
                (sink.as_str(), stream.as_str(), *track),
                ("out", "program", 0)
            );
            if config.is_some() {
                cfg = config.clone();
            }
            aus.extend(a.iter().cloned());
        }
    }
    let cfg = cfg.expect("config");
    eprintln!("realtime session: {} output frames", aus.len());
    assert!(
        aus.len() >= 60 && aus.len() <= 85,
        "{} frames for 2.5 s",
        aus.len()
    );
    assert!(aus[0].keyframe);
    for w in aus.windows(2) {
        let d = w[1].pts - w[0].pts;
        assert!(d > 0, "pts increase");
    }
    assert_eq!(decode_count(&reg, &cfg, &aus), aus.len(), "output decodes");
    // 状態の通知は約 10 Hz、切り替えの後はプログラムが b_only
    let statuses: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            SessionEventItem::Pipeline(Event::Status {
                compose, sources, ..
            }) => Some((compose.clone(), sources.clone())),
            _ => None,
        })
        .collect();
    assert!(statuses.len() >= 15, "{} status events", statuses.len());
    let (compose, sources) = statuses.last().unwrap();
    assert_eq!(compose[0].program, "b_only");
    assert!(sources.iter().any(|s| s.id == "cam_b" && s.on_program));
    assert!(sources.iter().any(|s| s.id == "cam_a" && !s.on_program));
    assert!(evs.contains(&SessionEventItem::Closed));
    assert!(
        !evs.iter()
            .any(|e| matches!(e, SessionEventItem::Crashed { .. }))
    );
}

#[tokio::test]
async fn h264_is_rejected_without_gpu() {
    if registry().is_none() {
        return;
    }
    // 親の事前検証
    let pool = SessionPool::new(config(&[]));
    let e = pool
        .open_session("t", Policy::default(), &graph("h264", json!({})))
        .await
        .err()
        .expect("rejected");
    assert_eq!(e.cause, SessionFailure::Rejected);
    assert_eq!(e.code, "CAPACITY_UNAVAILABLE");
    assert_eq!(e.http_status(), 503);
    assert_eq!(e.issues[0].node.as_deref(), Some("enc"));
    // worker の中の検証（事前検証なし）
    let mut c = config(&[]);
    c.prevalidate = false;
    let pool = SessionPool::new(c);
    let e = pool
        .open_session("t", Policy::default(), &graph("h264", json!({})))
        .await
        .err()
        .expect("rejected by worker");
    assert_eq!(e.cause, SessionFailure::Rejected);
    assert_eq!(e.code, "CAPACITY_UNAVAILABLE", "{e}");
    assert!(e.message.contains("ADR-0004"), "{e}");
    // ロイヤリティフリー限定
    let e = pool
        .open_session("t", Policy::royalty_free(), &graph("h264", json!({})))
        .await
        .err()
        .unwrap();
    assert_eq!(e.code, "INVALID_CONFIG");
    assert_eq!(pool.stats().active, 0);
    assert_eq!(pool.stats().rejected, 2);
}

#[tokio::test]
async fn per_tenant_session_limits() {
    if registry().is_none() {
        return;
    }
    let mut c = config(&[]);
    c.max_sessions_per_tenant = 1;
    c.max_sessions_total = 2;
    let pool = SessionPool::new(c);
    let g = graph("vp9", json!({}));
    let s1 = pool.open_session("a", Policy::default(), &g).await.unwrap();
    let e = pool
        .open_session("a", Policy::default(), &g)
        .await
        .err()
        .unwrap();
    assert_eq!(e.cause, SessionFailure::LimitReached);
    assert_eq!(e.code, "CAPACITY_UNAVAILABLE");
    let s2 = pool.open_session("b", Policy::default(), &g).await.unwrap();
    let e = pool
        .open_session("c", Policy::default(), &g)
        .await
        .err()
        .unwrap();
    assert_eq!(e.cause, SessionFailure::LimitReached);
    assert_eq!(pool.stats().per_tenant.get("a"), Some(&1));
    s1.close().await.unwrap();
    drop(s2); // 閉じずに捨てても枠は返る
    assert_eq!(pool.stats().active, 0);
    let s3 = pool.open_session("a", Policy::default(), &g).await.unwrap();
    s3.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_is_detected_and_session_can_be_reopened() {
    let Some(reg) = registry() else { return };
    let (ca, aa) = encode(&reg, 60, 60);
    let (cb, ab) = encode(&reg, 190, 60);
    let g = graph(
        "vp9",
        json!({"clock": "input", "overflow": "block", "block_timeout_ms": 60000}),
    );
    let pool = SessionPool::new(config(&["--fault", "session-crash", "--fault-after", "20"]));
    let mut s = pool.open_session("t", Policy::default(), &g).await.unwrap();
    let mut evs = s.events().unwrap();
    let _outs = s.outputs().unwrap();
    for i in 0..40 {
        let first = i == 0;
        if s.send_input("cam_a", first.then(|| ca.clone()), vec![aa[i].clone()])
            .is_err()
        {
            break;
        }
        let _ = s.send_input("cam_b", first.then(|| cb.clone()), vec![ab[i].clone()]);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let crashed = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(e) = evs.recv().await {
            if let SessionEventItem::Crashed { reason } = e {
                return Some(reason);
            }
        }
        None
    })
    .await
    .unwrap()
    .expect("crash event");
    eprintln!("crash detected: {crashed}");
    assert!(!s.is_alive());
    let e = s
        .send_input("cam_a", None, vec![aa[0].clone()])
        .unwrap_err();
    assert_eq!(e.cause, SessionFailure::Closed);
    let e = s
        .control(&Control::RequestKeyframe { node: None })
        .await
        .unwrap_err();
    assert_eq!(e.cause, SessionFailure::Closed);
    assert_eq!(pool.stats().crashed, 1);
    drop(s);
    // ノードが作り直す: 故障の無い worker で開き直し、出力が出る
    let pool = SessionPool::new(config(&[]));
    let mut s = pool.open_session("t", Policy::default(), &g).await.unwrap();
    let mut outs = s.outputs().unwrap();
    let col = tokio::spawn(async move {
        let mut n = 0;
        while let Some(SessionOutputItem::Media { aus, .. }) = outs.recv().await {
            n += aus.len();
        }
        n
    });
    for i in 0..60 {
        let first = i == 0;
        s.send_input("cam_a", first.then(|| ca.clone()), vec![aa[i].clone()])
            .unwrap();
        s.send_input("cam_b", first.then(|| cb.clone()), vec![ab[i].clone()])
            .unwrap();
    }
    s.end_input("cam_a").unwrap();
    s.end_input("cam_b").unwrap();
    s.close().await.unwrap();
    assert_eq!(col.await.unwrap(), 60);
}

#[tokio::test]
async fn input_backpressure_drops_until_keyframe() {
    let Some(reg) = registry() else { return };
    let (ca, aa) = encode(&reg, 60, 90);
    let (cb, ab) = encode(&reg, 190, 90);
    let mut c = config(&[]);
    c.input_queue_aus = 40;
    let pool = SessionPool::new(c);
    let g = graph(
        "vp9",
        json!({"clock": "input", "overflow": "block", "missing_timeout_ms": 1000, "block_timeout_ms": 60000}),
    );
    let mut s = pool.open_session("t", Policy::default(), &g).await.unwrap();
    let mut evs = s.events().unwrap();
    let mut outs = s.outputs().unwrap();
    let col = tokio::spawn(async move {
        let mut cfg = None;
        let mut aus = Vec::new();
        while let Some(o) = outs.recv().await {
            if let SessionOutputItem::Media { config, aus: a, .. } = o {
                if config.is_some() {
                    cfg = config;
                }
                aus.extend(a);
            }
        }
        (cfg, aus)
    });
    // 単一スレッドの実行器で、書き手に順番を渡さずに一気に入れる → 溢れる
    let mut dropped = 0;
    for i in 0..90 {
        let first = i == 0;
        dropped += s
            .send_input("cam_a", first.then(|| ca.clone()), vec![aa[i].clone()])
            .unwrap()
            .dropped;
        dropped += s
            .send_input("cam_b", first.then(|| cb.clone()), vec![ab[i].clone()])
            .unwrap()
            .dropped;
    }
    assert!(dropped > 0, "queue overflowed");
    s.end_input("cam_a").unwrap();
    s.end_input("cam_b").unwrap();
    s.close().await.unwrap();
    let (cfg, aus) = col.await.unwrap();
    let mut reported = 0;
    let mut decode_errors = 0;
    while let Ok(e) = evs.try_recv() {
        match e {
            SessionEventItem::InputDropped { count, .. } => reported += count,
            SessionEventItem::Pipeline(Event::Error { message, .. })
                if message.contains("decode") =>
            {
                decode_errors += 1
            }
            _ => {}
        }
    }
    eprintln!(
        "backpressure: dropped {dropped}, reported {reported}, output {} frames",
        aus.len()
    );
    assert_eq!(reported, dropped, "every drop is reported");
    // 残した映像はキーフレームから始まるので、worker の復号器は誤らない
    assert_eq!(decode_errors, 0);
    let cfg = cfg.expect("output");
    assert_eq!(decode_count(&reg, &cfg, &aus), aus.len());
}
