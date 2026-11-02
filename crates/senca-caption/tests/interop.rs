//! FFmpeg（試験の相手としてだけ使う）との突き合わせ。
//!
//! - FFmpeg で作った AAC（m4a）・Opus（mp4）を senca-fmp4 で AU に分け、AudioChunker で作った
//!   `.m4a`・`.aac`・`.ogg` を FFmpeg で復号して、誤りがないことと長さ（サンプル数）が合うことを確かめる。
//! - 書き出した `.vtt`・HLS の字幕 playlist を FFmpeg が読めて、Cue が出ることを確かめる。
//!
//! FFmpeg がなければ（`FFMPEG` 環境変数・`~/tools/ffmpeg`）飛ばす。

use bytes::Bytes;
use senca_caption::chunk::{AudioChunk, AudioChunker, ChunkFormat, ChunkerConfig};
use senca_caption::hls::{SubtitleSegmenter, SubtitleTrackConfig, vod_single_file_playlist};
use senca_caption::merge::Cue;
use senca_caption::vtt::VttDocument;
use senca_core::time::Timebase;
use senca_core::{AccessUnit, CodecConfig, TrackId};
use std::path::{Path, PathBuf};
use std::process::Command;

fn ffmpeg() -> Option<PathBuf> {
    let p = std::env::var_os("FFMPEG")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("tools/ffmpeg")))?;
    p.exists().then_some(p)
}

fn workdir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("senca-caption-interop").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(ff: &Path, args: &[&str]) -> (bool, Vec<u8>, String) {
    let o = Command::new(ff).args(["-nostdin", "-hide_banner", "-v", "error"]).args(args).output().unwrap();
    (o.status.success(), o.stdout, String::from_utf8_lossy(&o.stderr).into_owned())
}

/// FFmpeg で作ったファイルを読み、(設定, AU の列)。
fn demux(path: &Path) -> (CodecConfig, Vec<AccessUnit>) {
    let data = Bytes::from(std::fs::read(path).unwrap());
    let (init, samples) = senca_fmp4::read_file(&data).unwrap();
    let t = &init.tracks[0];
    let config = t.config.clone().expect("known sample entry");
    let tb = Timebase::new(1, t.timescale);
    let aus = samples
        .into_iter()
        .filter(|s| s.track_id == t.track_id)
        .enumerate()
        .map(|(i, s)| AccessUnit {
            track: TrackId(0),
            codec: config.codec,
            epoch: 0,
            sequence: i as u64,
            config_generation: 0,
            pts: s.dts + s.cts_offset as i64,
            dts: s.dts,
            duration: s.duration as u64,
            timebase: tb,
            keyframe: true,
            discontinuity: false,
            capture_time_ns: None,
            payload: s.data,
        })
        .collect();
    (config, aus)
}

fn chunks(config: &CodecConfig, aus: &[AccessUnit], format: Option<ChunkFormat>) -> Vec<AudioChunk> {
    let mut c = AudioChunker::new(ChunkerConfig { format, max_queued: 64, ..ChunkerConfig::default() }).unwrap();
    c.set_config(config).unwrap();
    for au in aus {
        c.push(au).unwrap();
    }
    c.flush();
    let mut out = Vec::new();
    while let Some(ch) = c.take_chunk().unwrap() {
        out.push(ch);
    }
    out
}

/// 復号して 48 kHz モノラルのサンプル数を数える。誤りの出力があれば失敗。
fn decoded_samples(ff: &Path, file: &Path) -> usize {
    let (ok, pcm, err) = run(ff, &["-i", file.to_str().unwrap(), "-f", "s16le", "-ac", "1", "-ar", "48000", "-"]);
    assert!(ok && err.trim().is_empty(), "{}: {err}", file.display());
    pcm.len() / 2
}

fn check_chunks(ff: &Path, dir: &Path, chunks: &[AudioChunk], tolerance: usize) {
    assert!(chunks.len() >= 3, "{}", chunks.len());
    for ch in chunks {
        let f = dir.join(ch.file_name());
        std::fs::write(&f, &ch.data).unwrap();
        let n = decoded_samples(ff, &f);
        let want = (ch.info.duration_us as u128 * 48_000 / 1_000_000) as usize;
        assert!(n.abs_diff(want) <= tolerance, "{}: decoded {n} samples, expected {want}", f.display());
    }
}

#[test]
fn aac_chunks_decode_with_ffmpeg() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skip: ffmpeg not found");
        return;
    };
    let dir = workdir("aac");
    let src = dir.join("src.m4a");
    let (ok, _, err) = run(
        &ff,
        &["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=13", "-c:a", "aac", "-b:a", "96k", src.to_str().unwrap()],
    );
    assert!(ok, "{err}");
    let (config, aus) = demux(&src);
    let m4a = chunks(&config, &aus, None);
    assert!(m4a.iter().all(|c| c.format == ChunkFormat::M4a));
    // AAC は 1024 サンプル単位。m4a は edit list なしなので全フレームが出る（µs への丸めで ±1 サンプル）
    check_chunks(&ff, &dir, &m4a, 1);
    let adts = chunks(&config, &aus, Some(ChunkFormat::Adts));
    check_chunks(&ff, &dir, &adts, 1);
    // 重なり: 隣り合うチャンクは約 1 秒重なる
    for w in m4a.windows(2) {
        let ov = w[0].info.end_us() - w[1].info.start_us;
        assert!((900_000..=1_100_000).contains(&ov), "{ov}");
    }
}

#[test]
fn opus_chunks_decode_with_ffmpeg() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skip: ffmpeg not found");
        return;
    };
    let dir = workdir("opus");
    let src = dir.join("src.mp4");
    let (ok, _, err) = run(
        &ff,
        &["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=13", "-c:a", "libopus", "-b:a", "64k", src.to_str().unwrap()],
    );
    assert!(ok, "{err}");
    let (config, aus) = demux(&src);
    let ogg = chunks(&config, &aus, None);
    assert!(ogg.iter().all(|c| c.format == ChunkFormat::OggOpus));
    // granule で pre-skip を捨てた長さになる
    check_chunks(&ff, &dir, &ogg, 1);
    // ffprobe 相当: FFmpeg が Ogg を Opus として認識する
    let f = dir.join(ogg[0].file_name());
    let (_, _, info) = Command::new(&ff)
        .args(["-nostdin", "-hide_banner", "-i", f.to_str().unwrap()])
        .output()
        .map(|o| (o.status.success(), o.stdout, String::from_utf8_lossy(&o.stderr).into_owned()))
        .unwrap();
    assert!(info.contains("Input #0, ogg") && info.contains("Audio: opus"), "{info}");
}

fn cues() -> Vec<Cue> {
    vec![
        Cue { id: 1, start_us: 500_000, end_us: 2_500_000, text: "皆さんこんにちは。".into() },
        Cue { id: 2, start_us: 3_000_000, end_us: 5_200_000, text: "今日は <微分> & 積分の話を\nします。".into() },
        Cue { id: 3, start_us: 6_100_000, end_us: 7_900_000, text: "まず定義から。".into() },
        Cue { id: 4, start_us: 9_000_000, end_us: 11_000_000, text: "以上です。".into() },
    ]
}

#[test]
fn vtt_and_hls_subtitles_read_by_ffmpeg() {
    let Some(ff) = ffmpeg() else {
        eprintln!("skip: ffmpeg not found");
        return;
    };
    let dir = workdir("subs");
    // 録画用の .vtt 全体
    let mut doc = VttDocument::new(0);
    for c in cues() {
        doc.push(&c);
    }
    let vtt = dir.join("captions_ja.vtt");
    std::fs::write(&vtt, doc.to_vtt().unwrap()).unwrap();
    let (ok, out, err) = run(&ff, &["-i", vtt.to_str().unwrap(), "-f", "webvtt", "-"]);
    assert!(ok && err.trim().is_empty(), "{err}");
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("00:00.500 --> 00:02.500") || out.contains("00:00:00.500 --> 00:00:02.500"), "{out}");
    // FFmpeg は文字参照を戻して出す（エスケープが往復する）
    assert!(out.contains("今日は <微分> & 積分の話を"), "{out}");

    // ライブの字幕セグメント（2 秒ごと）と playlist
    let mut cfg = SubtitleTrackConfig::new("ja", "日本語");
    cfg.target_duration_s = 2;
    let mut seg = SubtitleSegmenter::new(cfg).unwrap();
    for c in cues() {
        seg.push_cue(c);
    }
    for i in 0..6u64 {
        let s = seg.close_segment(i, i as i64 * 2_000_000, 2_000_000, false).unwrap();
        std::fs::write(dir.join(&s.uri), s.body).unwrap();
    }
    seg.end();
    let pl = dir.join("sub_ja.m3u8");
    std::fs::write(&pl, seg.playlist().unwrap()).unwrap();
    // FFmpeg 7 の hls demuxer は既定で .vtt の拡張子を拒むので許可する（FFmpeg 側の方針）
    let (ok, out, err) = run(&ff, &["-allowed_extensions", "ALL", "-i", pl.to_str().unwrap(), "-f", "webvtt", "-"]);
    assert!(ok, "{err}");
    let out = String::from_utf8(out).unwrap();
    for t in ["皆さんこんにちは。", "まず定義から。", "以上です。"] {
        assert!(out.contains(t), "{t} missing in {out}");
    }

    // 録画の VOD playlist（1 ファイル）
    let vod = dir.join("captions_ja.m3u8");
    std::fs::write(&vod, vod_single_file_playlist("captions_ja.vtt", 12_000_000).unwrap()).unwrap();
    let (ok, out, err) = run(&ff, &["-allowed_extensions", "ALL", "-i", vod.to_str().unwrap(), "-f", "webvtt", "-"]);
    assert!(ok, "{err}");
    assert!(String::from_utf8(out).unwrap().contains("以上です。"));
}
