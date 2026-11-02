//! 試験用の機器 → 変換 → 符号化（VP9・AV1・Opus）→ fMP4（senca-fmp4）→ FFmpeg で復号、の通しの試験。
//!
//! 符号化のライブラリは `SENCA_CODEC_LIB_DIR`（既定 `~/tools/codeclibs/usr/lib/x86_64-linux-gnu`）から、
//! FFmpeg は `FFMPEG`・`FFPROBE`（既定 `~/tools/ffmpeg`・`~/tools/ffprobe`）から使う。
//! 無ければその部分を飛ばす（理由を表示する）。

use senca_capture::client::{CaptureWorker, WorkerCommand, WorkerEvent, list_devices};
use senca_capture::device::{DeviceRegistry, ProbeOptions};
use senca_capture::session::CaptureSession;
use senca_capture::spec::CaptureSpec;
use senca_capture::synthetic::SyntheticOptions;
use senca_capture::{Backend, ErrorKind};
use senca_codec::caps::{Direction, ProviderId};
use senca_codec::dl::LibrarySearch;
use senca_codec::registry::{Policy, ProbeOptions as CodecProbe, ProviderRegistry};
use senca_core::{AccessUnit, Codec, CodecConfig};
use senca_fmp4::{FragmentWriter, SampleQueue, TrackRun, TrackSpec, write_init_segment};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn lib_dir() -> PathBuf {
    std::env::var_os("SENCA_CODEC_LIB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("tools/codeclibs/usr/lib/x86_64-linux-gnu"))
}

fn tool(name: &str, env: &str) -> Option<PathBuf> {
    let p = std::env::var_os(env)
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("tools").join(name));
    p.exists().then_some(p)
}

fn work_dir(name: &str) -> PathBuf {
    let d = home()
        .join(".cache/senca-capture-test")
        .join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn codecs() -> ProviderRegistry {
    let o = CodecProbe {
        libs: LibrarySearch::new(vec![lib_dir()]),
        ..Default::default()
    };
    ProviderRegistry::probe(&o)
}

fn have(r: &ProviderRegistry, p: ProviderId) -> bool {
    r.statuses().iter().any(|s| s.provider == p && s.available)
}

fn devices(opts: SyntheticOptions) -> DeviceRegistry {
    let mut o = ProbeOptions {
        dev_dir: "/nonexistent-senca".into(),
        proc_asound: "/nonexistent-senca".into(),
        libs: LibrarySearch {
            dirs: vec!["/nonexistent-senca".into()],
            dirs_only: true,
        },
        ..Default::default()
    };
    o.synthetic = true;
    o.synthetic_options = opts;
    DeviceRegistry::probe(&o)
}

/// トラックごとの設定と AU。
#[derive(Default)]
struct Collected {
    configs: BTreeMap<u32, Vec<CodecConfig>>,
    aus: BTreeMap<u32, Vec<AccessUnit>>,
}

impl Collected {
    fn add(&mut self, track: u32, config: Option<CodecConfig>, aus: Vec<AccessUnit>) {
        if let Some(c) = config {
            self.configs.entry(track).or_default().push(c);
        }
        self.aus.entry(track).or_default().extend(aus);
    }

    /// fMP4（初期化セグメント + 1 つのフラグメント）を書く。
    fn write_fmp4(&self, path: &Path) {
        let tracks: Vec<u32> = self.aus.keys().copied().collect();
        let specs: Vec<TrackSpec> = tracks
            .iter()
            .map(|t| TrackSpec::new(t + 1, self.configs[t].last().unwrap().clone()))
            .collect();
        let mut file = Vec::new();
        write_init_segment(&specs, &mut file).unwrap();
        let mut samples = Vec::new();
        for (t, spec) in tracks.iter().zip(&specs) {
            let mut q = SampleQueue::new(spec.timescale);
            for au in &self.aus[t] {
                q.push(au).unwrap();
            }
            q.flush();
            samples.push(q.take_ready());
        }
        let runs: Vec<TrackRun> = specs
            .iter()
            .zip(&samples)
            .map(|(s, v)| TrackRun {
                track_id: s.track_id,
                samples: v,
            })
            .collect();
        FragmentWriter::new().write(&runs, &mut file).unwrap();
        std::fs::write(path, file).unwrap();
    }
}

/// FFmpeg で全体を復号し、誤りが無いことと映像のフレーム数を確かめる。FFmpeg が無ければ `None`。
fn ffmpeg_check(path: &Path) -> Option<(String, u64, bool)> {
    let ff = tool("ffmpeg", "FFMPEG")?;
    let probe = tool("ffprobe", "FFPROBE")?;
    let out = Command::new(ff)
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    let errors = String::from_utf8_lossy(&out.stderr).into_owned();
    let ok = out.status.success();
    let out = Command::new(&probe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    let frames = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    let audio = Command::new(&probe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    let has_opus = String::from_utf8_lossy(&audio.stdout).trim() == "opus";
    Some((
        if ok {
            errors
        } else {
            format!("exit failure: {errors}")
        },
        frames,
        has_opus,
    ))
}

fn spec_json(device: &str, codec: &str, format: &str, frames: u64, audio: bool) -> String {
    let audio = if audio {
        r#","audio":{"sample_rate":48000,"channels":2,"codec":"opus","bitrate_bps":64000}"#
    } else {
        ""
    };
    format!(
        r#"{{"video_device":"{device}","video":{{"width":320,"height":240,"fps":25,"formats":["{format}"],"codec":"{codec}","bitrate_bps":400000,"gop":25}}{audio},"synthetic":{{"realtime":false,"max_video_frames":{frames}}}}}"#
    )
}

fn run_in_process(codec: &str, format: &str, frames: u64) -> Option<Collected> {
    let reg = codecs();
    let need = if codec == "av1" {
        ProviderId::SvtAv1
    } else {
        ProviderId::Libvpx
    };
    if !have(&reg, need) || !have(&reg, ProviderId::Opus) {
        eprintln!(
            "SKIP: {need:?}/opus not available in {}",
            lib_dir().display()
        );
        return None;
    }
    let spec =
        CaptureSpec::from_json(&spec_json("synthetic:0", codec, format, frames, true)).unwrap();
    let devs = devices(spec.synthetic.clone().unwrap());
    let cfg = spec.session_config(Policy::royalty_free()).unwrap();
    let mut s = CaptureSession::start(spec.open_devices(&devs).unwrap(), &cfg, &reg).unwrap();
    assert_eq!(
        s.negotiated()[0].1.video.as_ref().unwrap().format.as_str(),
        format
    );
    let mut c = Collected::default();
    let mut guard = 0;
    while !s.is_finished() {
        for o in s.poll(Duration::from_millis(200)).unwrap() {
            c.add(o.track.0, o.config, o.aus);
        }
        guard += 1;
        assert!(guard < 10_000, "session did not finish");
    }
    for o in s.finish().unwrap() {
        c.add(o.track.0, o.config, o.aus);
    }
    let st = s.stats().clone();
    assert_eq!(st.video_frames, frames);
    assert_eq!(st.sequence_gaps, 0);
    assert_eq!(st.audio_resyncs, 0);
    Some(c)
}

fn check_collected(c: &Collected, codec: Codec, frames: u64, name: &str) {
    let v = &c.aus[&0];
    assert_eq!(v.len() as u64, frames, "video AUs");
    assert!(v.iter().all(|a| a.codec == codec));
    assert!(v[0].keyframe);
    assert!(
        v.windows(2)
            .all(|w| w[1].pts > w[0].pts && w[1].dts >= w[0].dts)
    );
    assert!(v.iter().all(|a| a.capture_time_ns.is_some()));
    // 25 fps の 1 フレーム = 3600（90 kHz）
    assert_eq!(v[1].pts - v[0].pts, 3600);
    let cfg = &c.configs[&0][0];
    assert_eq!(cfg.codec, codec);
    assert_eq!(
        (cfg.video.unwrap().width, cfg.video.unwrap().height),
        (320, 240)
    );
    let a = &c.aus[&1];
    assert!(a.iter().all(|x| x.codec == Codec::Opus));
    // 1.2 秒分の音声（20 ms ごと）。符号化器の遅延の詰め物の分だけ多い。
    let dur = frames as usize * 40 / 20;
    assert!(
        a.len() >= dur && a.len() <= dur + 3,
        "opus packets {}",
        a.len()
    );
    assert!(a.windows(2).all(|w| w[1].pts - w[0].pts == 960));
    let dir = work_dir(name);
    let path = dir.join(format!("{name}.mp4"));
    c.write_fmp4(&path);
    match ffmpeg_check(&path) {
        Some((errors, n, has_opus)) => {
            assert!(errors.trim().is_empty(), "ffmpeg errors: {errors}");
            assert_eq!(n, frames, "ffprobe frame count");
            assert!(has_opus);
            eprintln!("ffmpeg decoded {} ({n} video frames)", path.display());
        }
        None => eprintln!("SKIP: ffmpeg/ffprobe not found"),
    }
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn synthetic_yuyv_to_vp9_opus_fmp4() {
    if let Some(c) = run_in_process("vp9", "yuyv", 30) {
        check_collected(&c, Codec::Vp9, 30, "vp9-yuyv");
    }
}

#[test]
fn synthetic_mjpeg_to_av1_opus_fmp4() {
    if let Some(c) = run_in_process("av1", "mjpeg", 30) {
        check_collected(&c, Codec::Av1, 30, "av1-mjpeg");
    }
}

#[test]
fn synthetic_uyvy_to_vp8() {
    // DeckLink と同じ UYVY の経路（音声なし）。
    let reg = codecs();
    if !have(&reg, ProviderId::Libvpx) {
        eprintln!("SKIP: libvpx not available");
        return;
    }
    let spec = CaptureSpec::from_json(&spec_json("synthetic:0", "vp8", "uyvy", 10, false)).unwrap();
    let devs = devices(spec.synthetic.clone().unwrap());
    let cfg = spec.session_config(Policy::default()).unwrap();
    let mut s = CaptureSession::start(spec.open_devices(&devs).unwrap(), &cfg, &reg).unwrap();
    let mut n = 0;
    while !s.is_finished() {
        for o in s.poll(Duration::from_millis(200)).unwrap() {
            n += o.aus.len();
        }
    }
    assert_eq!(n, 10);
    assert_eq!(s.tracks()[0].provider, "libvpx");
}

#[test]
fn h264_without_hardware_is_rejected_before_capture() {
    let reg = codecs();
    if reg
        .candidates(Codec::H264, Direction::Encode, None, &Policy::default())
        .is_ok()
    {
        eprintln!("SKIP: a hardware H.264 encoder is present");
        return;
    }
    let spec = CaptureSpec::from_json(&spec_json("synthetic:0", "h264", "i420", 5, false)).unwrap();
    let devs = devices(spec.synthetic.clone().unwrap());
    let cfg = spec.session_config(Policy::default()).unwrap();
    let e = CaptureSession::start(spec.open_devices(&devs).unwrap(), &cfg, &reg).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unavailable);
    assert_eq!(e.kind.api_code(), "CAPACITY_UNAVAILABLE");
}

fn worker_cmd() -> WorkerCommand {
    let mut c = WorkerCommand::new(env!("CARGO_BIN_EXE_senca-capture-worker"));
    c.args.push("--synthetic".into());
    c.env.push((
        "SENCA_CODEC_LIB_DIR".into(),
        lib_dir().display().to_string(),
    ));
    c.env
        .push(("SENCA_CAPTURE_LIB_DIR".into(), "/nonexistent-senca".into()));
    c
}

#[test]
fn worker_lists_devices() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let l = rt
        .block_on(list_devices(&worker_cmd(), Duration::from_secs(20)))
        .unwrap();
    assert!(l.devices.iter().any(|d| d.id == "synthetic:0"));
    let dl = l
        .backends
        .iter()
        .find(|b| b.backend == Backend::Decklink)
        .unwrap();
    assert!(!dl.available);
    assert!(dl.detail.contains("libDeckLinkAPI.so"));
    assert!(l.backends.iter().all(|b| b.backend == Backend::Synthetic
        || b.verification == senca_capture::Verification::Unverified));
}

#[test]
fn worker_process_end_to_end() {
    let reg = codecs();
    if !have(&reg, ProviderId::Libvpx) || !have(&reg, ProviderId::Opus) {
        eprintln!("SKIP: libvpx/opus not available");
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let frames = 20;
        let spec =
            CaptureSpec::from_json(&spec_json("synthetic:0", "vp9", "nv12", frames, true)).unwrap();
        let (mut w, mut rx) = CaptureWorker::spawn(
            &worker_cmd(),
            7,
            "tenant-a",
            Policy::royalty_free(),
            &spec,
            Duration::from_secs(20),
        )
        .await
        .unwrap();
        assert!(
            w.hello.sandbox.contains("no_new_privs"),
            "{}",
            w.hello.sandbox
        );
        assert_eq!(w.opened["tracks"][0]["codec"], "vp9");
        assert_eq!(w.opened["tracks"][1]["codec"], "opus");
        assert_eq!(w.opened["negotiated"][0]["video"]["format"], "nv12");
        let mut c = Collected::default();
        let mut closed = None;
        while let Some(ev) = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .unwrap()
        {
            match ev {
                WorkerEvent::Media { track, config, aus } => c.add(track, config, aus),
                WorkerEvent::Event(_) => {}
                WorkerEvent::Error(e) => panic!("worker error: {e}"),
                WorkerEvent::Closed(stats) => closed = Some(stats),
                WorkerEvent::Exited(_) => break,
            }
        }
        let stats = closed.expect("closed");
        assert_eq!(stats["video_frames"], frames);
        assert_eq!(w.wait(Duration::from_secs(10)).await, Some(0));
        check_collected(&c, Codec::Vp9, frames, "worker-vp9");

        // H.264 はハードウェアが無ければ worker が CAPACITY_UNAVAILABLE で断る
        if reg
            .candidates(Codec::H264, Direction::Encode, None, &Policy::default())
            .is_err()
        {
            let spec = CaptureSpec::from_json(&spec_json("synthetic:0", "h264", "i420", 5, false))
                .unwrap();
            let e = CaptureWorker::spawn(
                &worker_cmd(),
                8,
                "t",
                Policy::default(),
                &spec,
                Duration::from_secs(20),
            )
            .await
            .unwrap_err();
            assert_eq!(e.kind, ErrorKind::Unavailable, "{e}");
        }
        // 無い機器は NOT_FOUND
        let spec = CaptureSpec::from_json(&spec_json("v4l2:/dev/video0", "vp9", "yuyv", 5, false))
            .unwrap();
        let e = CaptureWorker::spawn(
            &worker_cmd(),
            9,
            "t",
            Policy::default(),
            &spec,
            Duration::from_secs(20),
        )
        .await
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotFound, "{e}");
    });
}

#[test]
fn worker_close_stops_a_live_capture() {
    let reg = codecs();
    if !have(&reg, ProviderId::Libvpx) {
        eprintln!("SKIP: libvpx not available");
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // 実時間・終わりなし。少し受け取ってから閉じる。
        let json = r#"{"video_device":"synthetic:0","video":{"width":160,"height":120,"fps":30,"codec":"vp8","bitrate_bps":200000},"synthetic":{"realtime":true}}"#;
        let spec = CaptureSpec::from_json(json).unwrap();
        let (mut w, mut rx) =
            CaptureWorker::spawn(&worker_cmd(), 1, "t", Policy::default(), &spec, Duration::from_secs(20))
                .await
                .unwrap();
        let mut n = 0;
        while n < 10 {
            if let Some(WorkerEvent::Media { aus, .. }) = rx.recv().await {
                n += aus.len();
            }
        }
        w.close().await.unwrap();
        let mut closed = false;
        while let Some(ev) = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap() {
            match ev {
                WorkerEvent::Closed(_) => closed = true,
                WorkerEvent::Exited(_) => break,
                WorkerEvent::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
        assert!(closed);
        assert_eq!(w.wait(Duration::from_secs(10)).await, Some(0));
    });
}
