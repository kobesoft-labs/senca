//! tools/interop/captions.sh から使う: 音声チャンクと字幕ファイルを書き出す。
//!
//! ```text
//! caption_files chunks <入力 .m4a/.mp4> <出力dir> [m4a|adts]
//!     → chunk-N.{m4a,aac,ogg} と chunks.tsv（ファイル名・開始 µs・長さ µs）
//! caption_files subs <出力dir> <セグメント数> <セグメント秒>
//!     → sub_ja_N.vtt・sub_ja.m3u8・captions_ja.vtt・captions_ja.m3u8・master.m3u8（video.m3u8 を参照）
//! ```

use bytes::Bytes;
use senca_caption::chunk::{AudioChunker, ChunkFormat, ChunkerConfig};
use senca_caption::hls::{SubtitleSegmenter, SubtitleTrackConfig, vod_single_file_playlist};
use senca_caption::merge::Cue;
use senca_caption::vtt::VttDocument;
use senca_core::time::Timebase;
use senca_core::{AccessUnit, TrackId};
use senca_hls::{MultivariantPlaylist, VariantStream};
use std::fmt::Write as _;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let r = match args.get(1).map(String::as_str) {
        Some("chunks") if args.len() >= 4 => chunks(&args[2], &args[3], args.get(4).map(String::as_str)),
        Some("subs") if args.len() == 5 => subs(&args[2], args[3].parse().unwrap_or(6), args[4].parse().unwrap_or(2)),
        _ => Err("usage: caption_files chunks <in> <outdir> [m4a|adts] | subs <outdir> <segments> <seconds>".into()),
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn chunks(input: &str, out: &str, fmt: Option<&str>) -> Result<(), String> {
    let data = Bytes::from(std::fs::read(input).map_err(|e| e.to_string())?);
    let (init, samples) = senca_fmp4::read_file(&data).map_err(|e| e.to_string())?;
    let t = init.tracks.first().ok_or("no track")?;
    let config = t.config.clone().ok_or("unknown codec")?;
    let format = parse_format(fmt)?;
    let mut c = AudioChunker::new(ChunkerConfig { format, max_queued: 64, ..ChunkerConfig::default() }).map_err(|e| e.to_string())?;
    c.set_config(&config).map_err(|e| e.to_string())?;
    let tb = Timebase::new(1, t.timescale);
    for (i, s) in samples.into_iter().filter(|s| s.track_id == t.track_id).enumerate() {
        let au = AccessUnit {
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
        };
        c.push(&au).map_err(|e| e.to_string())?;
    }
    c.flush();
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let mut tsv = String::new();
    while let Some(ch) = c.take_chunk().map_err(|e| e.to_string())? {
        std::fs::write(Path::new(out).join(ch.file_name()), &ch.data).map_err(|e| e.to_string())?;
        let _ = writeln!(tsv, "{}\t{}\t{}", ch.file_name(), ch.info.start_us, ch.info.duration_us);
    }
    std::fs::write(Path::new(out).join("chunks.tsv"), tsv).map_err(|e| e.to_string())
}

fn parse_format(f: Option<&str>) -> Result<Option<ChunkFormat>, String> {
    match f {
        None => Ok(None),
        Some("m4a") => Ok(Some(ChunkFormat::M4a)),
        Some("adts") => Ok(Some(ChunkFormat::Adts)),
        Some("ogg") => Ok(Some(ChunkFormat::OggOpus)),
        Some(x) => Err(format!("unknown format {x}")),
    }
}

fn subs(out: &str, segments: u64, seconds: u64) -> Result<(), String> {
    let dir = Path::new(out);
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let seg_us = seconds * 1_000_000;
    let total = segments * seg_us;
    // 1.5 秒ごとに 1 件、1.2 秒の Cue（境界をまたぐものを含む）
    let cues: Vec<Cue> = (0..)
        .map(|i: u64| Cue {
            id: i + 1,
            start_us: (i * 1_500_000 + 200_000) as i64,
            end_us: (i * 1_500_000 + 1_400_000) as i64,
            text: format!("字幕 {} <テスト> & 確認", i + 1),
        })
        .take_while(|c| (c.end_us as u64) <= total)
        .collect();
    let mut cfg = SubtitleTrackConfig::new("ja", "日本語");
    cfg.target_duration_s = seconds;
    cfg.default = true;
    let mut seg = SubtitleSegmenter::new(cfg).map_err(|e| e.to_string())?;
    let mut doc = VttDocument::new(0);
    for c in &cues {
        seg.push_cue(c.clone());
        doc.push(c);
    }
    for i in 0..segments {
        let s = seg.close_segment(i, (i * seg_us) as i64, seg_us, false).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(&s.uri), s.body).map_err(|e| e.to_string())?;
    }
    seg.end();
    std::fs::write(dir.join("sub_ja.m3u8"), seg.playlist().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("captions_ja.vtt"), doc.to_vtt().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("captions_ja.m3u8"), vod_single_file_playlist("captions_ja.vtt", total).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let mut v = VariantStream::new("video.m3u8", 500_000);
    v.subtitles = Some(seg.config().group_id.clone());
    let mv = MultivariantPlaylist { renditions: vec![seg.rendition()], variants: vec![v], ..MultivariantPlaylist::default() };
    std::fs::write(dir.join("master.m3u8"), mv.to_m3u8().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    println!("{}", cues.len());
    Ok(())
}
