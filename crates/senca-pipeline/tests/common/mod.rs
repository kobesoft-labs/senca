//! 試験の共通部品: 本物のソフトウェアコーデック（libvpx・SVT-AV1・dav1d・libopus）で入力を作り、実行の出力を集め、
//! MP4 に書いて FFmpeg に読ませる。
//!
//! ライブラリは `SENCA_CODEC_LIB_DIR`（既定は ~/tools/codeclibs/usr/lib/x86_64-linux-gnu）から読む。
//! 見つからなければ「skipped」と出して通す（`SENCA_CODEC_REQUIRE_SW=1` なら失敗にする）。

#![allow(dead_code)]

use bytes::Bytes;
use senca_codec::caps::ProviderId;
use senca_codec::config::{AudioEncoderConfig, DecoderConfig, EncoderConfig, VideoEncoderConfig};
use senca_codec::dl::LibrarySearch;
use senca_codec::frame::{Frame, PixelFormat};
use senca_codec::registry::{Policy, ProbeOptions, ProviderRegistry};
use senca_core::{AccessUnit, Codec, CodecConfig, Rescale, Timebase, TrackId};
use senca_image::{I420Frame, Plane};
use senca_pipeline::runtime::{MediaOutput, RuntimeOutput};
use senca_pipeline::{Event, Runtime, Snapshot};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;

pub const DEFAULT_LIB_DIR: &str = "/home/cc/tools/codeclibs/usr/lib/x86_64-linux-gnu";

pub fn lib_dir() -> PathBuf {
    std::env::var_os("SENCA_CODEC_LIB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_LIB_DIR))
}

/// ソフトウェアのコーデックを読み込む。無ければ `None`（試験は飛ばす）。
pub fn registry() -> Option<Arc<ProviderRegistry>> {
    let mut o = ProbeOptions::from_env();
    o.libs = LibrarySearch::new(vec![lib_dir()]);
    let r = ProviderRegistry::probe(&o);
    for id in [
        ProviderId::Libvpx,
        ProviderId::SvtAv1,
        ProviderId::Dav1d,
        ProviderId::Opus,
    ] {
        let st = r.get(id).map(|p| p.status());
        if !st.as_ref().is_some_and(|s| s.available) {
            let msg = format!("{} unavailable: {:?}", id.as_str(), st.map(|s| s.detail));
            if std::env::var("SENCA_CODEC_REQUIRE_SW").as_deref() == Ok("1") {
                panic!("{msg}");
            }
            eprintln!("skipped: {msg}");
            return None;
        }
    }
    Some(Arc::new(r))
}

pub fn us(n: i64, fps: i64) -> i64 {
    (n * 3000)
        .rescale(Timebase::MPEG, Timebase::MICROS)
        .unwrap()
        * 30
        / fps
}

/// I420 を関数から作る。`f(x, y) -> (Y, U, V)`（色差は左上の画素で決める）。
pub fn image(w: usize, h: usize, f: impl Fn(usize, usize) -> (u8, u8, u8)) -> I420Frame {
    let y = Plane::from_fn(w, h, |x, y| f(x, y).0).unwrap();
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let u = Plane::from_fn(cw, ch, |x, y| f(2 * x, 2 * y).1).unwrap();
    let v = Plane::from_fn(cw, ch, |x, y| f(2 * x, 2 * y).2).unwrap();
    I420Frame::from_planes(y, u, v).unwrap()
}

/// 入力 A: 横の階調と動く縦の帯。
pub fn pattern_a(w: usize, h: usize, i: usize) -> I420Frame {
    let bar = (i * 4) % w;
    image(w, h, |x, _y| {
        let base = 50 + (x * 100 / w) as u8;
        let y = if x.abs_diff(bar) < 6 { base + 60 } else { base };
        (y, 110, 140)
    })
}

/// 入力 B: 縦の階調と動く市松。
pub fn pattern_b(w: usize, h: usize, i: usize) -> I420Frame {
    image(w, h, |x, y| {
        let base = 200 - (y * 80 / h) as i32;
        let c = ((x + i * 2) / 16 + y / 16).is_multiple_of(2);
        ((base + if c { 15 } else { -15 }) as u8, 150, 100)
    })
}

/// 映像を符号化する（90 kHz、`fps`）。
pub fn encode_video(
    reg: &ProviderRegistry,
    codec: Codec,
    frames: &[I420Frame],
    fps: u32,
    bitrate: u32,
) -> (CodecConfig, Vec<AccessUnit>) {
    let (w, h) = (frames[0].width() as u32, frames[0].height() as u32);
    let mut cfg = VideoEncoderConfig::new(codec, w, h, fps, bitrate);
    cfg.gop = fps;
    let (_, mut enc) = reg
        .create_encoder(&EncoderConfig::Video(cfg), &Policy::default())
        .unwrap();
    let mut aus = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        let pts = i as i64 * 90000 / fps as i64;
        let v = senca_pipeline::convert::i420_to_video(f, pts, Timebase::MPEG).unwrap();
        enc.send(&Frame::Video(v)).unwrap();
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

/// 正弦波（ステレオ、48 kHz）。
pub fn sine(freq: f32, amp: f32, seconds: f32) -> Vec<f32> {
    let n = (48000.0 * seconds) as usize;
    let mut v = Vec::with_capacity(n * 2);
    for k in 0..n {
        let s = (2.0 * std::f32::consts::PI * freq * k as f32 / 48000.0).sin() * amp;
        v.push(s);
        v.push(s);
    }
    v
}

/// Opus で符号化する（20 ms）。
pub fn encode_opus(reg: &ProviderRegistry, pcm: &[f32]) -> (CodecConfig, Vec<AccessUnit>) {
    let cfg = AudioEncoderConfig::opus(48000, 2, 128_000);
    let (_, mut enc) = reg
        .create_encoder(&EncoderConfig::Audio(cfg), &Policy::default())
        .unwrap();
    let mut aus = Vec::new();
    for (k, chunk) in pcm.chunks(960 * 2).enumerate() {
        let f = senca_pipeline::convert::f32_to_audio(chunk, 48000, 2, k as i64 * 960);
        enc.send(&Frame::Audio(f)).unwrap();
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

/// 映像を復号する（I420）。
pub fn decode_video(
    reg: &ProviderRegistry,
    config: &CodecConfig,
    aus: &[AccessUnit],
) -> Vec<(i64, I420Frame)> {
    let mut dc = DecoderConfig::new(config.codec).with_config(config.clone());
    dc.output_format = Some(PixelFormat::I420);
    let (_, mut dec) = reg.create_decoder(&dc, &Policy::default()).unwrap();
    let mut out = Vec::new();
    let take = |f: Frame, out: &mut Vec<(i64, I420Frame)>| {
        let v = f.as_video().unwrap();
        out.push((v.pts, senca_pipeline::convert::video_to_i420(v).unwrap()));
    };
    for a in aus {
        dec.send(a).unwrap();
        while let Some(f) = dec.receive().unwrap() {
            take(f, &mut out);
        }
    }
    dec.flush().unwrap();
    while let Some(f) = dec.receive().unwrap() {
        take(f, &mut out);
    }
    out
}

/// Opus を復号する（f32 ステレオ）。
pub fn decode_opus(reg: &ProviderRegistry, config: &CodecConfig, aus: &[AccessUnit]) -> Vec<f32> {
    let dc = DecoderConfig::new(Codec::Opus).with_config(config.clone());
    let (_, mut dec) = reg.create_decoder(&dc, &Policy::default()).unwrap();
    let mut out = Vec::new();
    for a in aus {
        dec.send(a).unwrap();
        while let Some(f) = dec.receive().unwrap() {
            out.extend(senca_pipeline::convert::audio_to_f32(f.as_audio().unwrap()));
        }
    }
    out
}

/// 1 つの入力（source）の流れ。
pub struct Feed {
    pub source: String,
    pub config: CodecConfig,
    pub aus: Vec<AccessUnit>,
}

/// 入力を時刻の順に混ぜて入れる（AU 1 つずつ。各入力の最初の AU と一緒に設定を渡す）。
pub fn feed_all(rt: &Runtime, feeds: &[Feed]) {
    let mut items: Vec<(i64, usize, usize)> = Vec::new();
    for (k, f) in feeds.iter().enumerate() {
        for (j, a) in f.aus.iter().enumerate() {
            let t = a.pts.rescale(a.timebase, Timebase::MICROS).unwrap();
            items.push((t, k, j));
        }
    }
    items.sort();
    let mut sent = vec![false; feeds.len()];
    for (_, k, j) in items {
        let f = &feeds[k];
        let cfg = (!sent[k]).then(|| f.config.clone());
        sent[k] = true;
        rt.push_input(&f.source, cfg, vec![f.aus[j].clone()])
            .unwrap();
    }
}

/// 出力を別スレッドで集める。
pub fn collect(rx: Receiver<RuntimeOutput>) -> JoinHandle<Vec<RuntimeOutput>> {
    std::thread::spawn(move || rx.into_iter().collect())
}

/// sink のトラックの出力（AU と最後の設定）。
pub fn track(
    outs: &[RuntimeOutput],
    sink: &str,
    track: u32,
) -> (Option<CodecConfig>, Vec<AccessUnit>, Vec<MediaOutput>) {
    let mut cfg = None;
    let mut aus = Vec::new();
    let mut batches = Vec::new();
    for o in outs {
        if let RuntimeOutput::Media(m) = o
            && m.sink == sink
            && m.track == track
        {
            if m.config.is_some() {
                cfg = m.config.clone();
            }
            aus.extend(m.aus.iter().cloned());
            batches.push(m.clone());
        }
    }
    (cfg, aus, batches)
}

pub fn events(outs: &[RuntimeOutput]) -> Vec<Event> {
    outs.iter()
        .filter_map(|o| match o {
            RuntimeOutput::Event(e) => Some(e.clone()),
            _ => None,
        })
        .collect()
}

pub fn snapshots(outs: &[RuntimeOutput]) -> Vec<Snapshot> {
    outs.iter()
        .filter_map(|o| match o {
            RuntimeOutput::Snapshot(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// 平面の PSNR（範囲 `x0..x1, y0..y1`）。
pub fn psnr_region(a: &Plane, b: &Plane, x0: usize, y0: usize, x1: usize, y1: usize) -> f64 {
    let mut se = 0f64;
    let mut n = 0f64;
    for y in y0..y1 {
        for x in x0..x1 {
            let d = a.get(x, y) as f64 - b.get(x, y) as f64;
            se += d * d;
            n += 1.0;
        }
    }
    let mse = se / n.max(1.0);
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

pub fn mean(p: &Plane, x0: usize, y0: usize, x1: usize, y1: usize) -> f64 {
    let mut s = 0f64;
    for y in y0..y1 {
        for x in x0..x1 {
            s += p.get(x, y) as f64;
        }
    }
    s / ((x1 - x0) * (y1 - y0)) as f64
}

/// 断片化しない MP4 に書く（トラックごとに設定と AU）。
pub fn write_mp4(path: &Path, tracks: &[(CodecConfig, Vec<AccessUnit>)]) {
    use senca_fmp4::{Mp4Writer, SampleInfo, TrackSpec};
    let specs: Vec<TrackSpec> = tracks
        .iter()
        .enumerate()
        .map(|(i, (c, _))| TrackSpec::new(i as u32 + 1, c.clone()))
        .collect();
    let mut w = Mp4Writer::new(specs.clone()).unwrap();
    let mut payload = Vec::new();
    // トラックを交互に（DTS の順）
    let mut items: Vec<(i64, usize, usize)> = Vec::new();
    for (k, (_, aus)) in tracks.iter().enumerate() {
        for (j, a) in aus.iter().enumerate() {
            items.push((a.dts.rescale(a.timebase, Timebase::MICROS).unwrap(), k, j));
        }
    }
    items.sort();
    for (_, k, j) in items {
        let aus = &tracks[k].1;
        let a = &aus[j];
        let ts = specs[k].timescale;
        let tb = Timebase::new(1, ts);
        let dts = a.dts.rescale(a.timebase, tb).unwrap();
        let next = aus
            .get(j + 1)
            .map(|n| n.dts.rescale(n.timebase, tb).unwrap())
            .unwrap_or(dts + (a.duration as i64).rescale(a.timebase, tb).unwrap().max(1));
        let pts = a.pts.rescale(a.timebase, tb).unwrap();
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
        .unwrap();
        payload.extend_from_slice(&a.payload);
    }
    let mut file = w.finish().unwrap();
    file.extend_from_slice(&payload);
    std::fs::write(path, file).unwrap();
}

fn tool(name: &str, env: &str) -> Option<PathBuf> {
    let p = std::env::var_os(env).map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(format!(
            "{}/tools/{name}",
            std::env::var("HOME").unwrap_or_default()
        ))
    });
    p.exists().then_some(p)
}

/// `ffmpeg -v error -i file -f null -` が誤りなく読めるか（FFmpeg が無ければ `None`）。
pub fn ffmpeg_reads(path: &Path) -> Option<Result<(), String>> {
    let ff = tool("ffmpeg", "FFMPEG")?;
    let out = Command::new(ff)
        .args(["-nostdin", "-v", "error", "-xerror", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .ok()?;
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    Some(if out.status.success() && err.trim().is_empty() {
        Ok(())
    } else {
        Err(err)
    })
}

/// ffprobe でストリーム 0 のフレーム数・長さ（秒）。
pub fn ffprobe_stream(path: &Path, stream: &str) -> Option<(u64, f64)> {
    let fp = tool("ffprobe", "FFPROBE")?;
    let out = Command::new(fp)
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            stream,
            "-show_entries",
            "stream=nb_read_frames,duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().next()?;
    let mut parts = line.split(',');
    let dur: f64 = parts.next()?.trim().parse().ok()?;
    let n: u64 = parts.next()?.trim().parse().ok()?;
    Some((n, dur))
}

pub fn tmp(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("senca-pipeline");
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

pub fn bytes_of(v: &[u8]) -> Bytes {
    Bytes::copy_from_slice(v)
}

pub fn track_id(a: &AccessUnit) -> TrackId {
    a.track
}
