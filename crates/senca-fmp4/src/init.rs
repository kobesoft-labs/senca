//! 初期化セグメントと、moov の共通部分（trak）。

use crate::bmff::{MATRIX, begin_box, begin_full_box, end_box, ftyp, put_u16, put_u32, put_u64};
use crate::entry::{default_timescale, write_sample_entry};
use crate::error::Mp4Error;
use senca_core::{CodecConfig, MediaKind};

/// moov の時間軸（mvhd）。
pub(crate) const MOVIE_TIMESCALE: u32 = 1000;

/// トラックの宣言。
#[derive(Debug, Clone)]
pub struct TrackSpec {
    /// 1 以上。
    pub track_id: u32,
    /// メディアの時間軸（mdhd）。サンプルの時刻はこの単位。
    pub timescale: u32,
    pub config: CodecConfig,
    /// ISO 639-2/T の言語（既定 "und"）。
    pub language: [u8; 3],
}

impl TrackSpec {
    /// 既定の時間軸（映像 90 kHz、AAC は標本化周波数、Opus は 48 kHz）で作る。
    pub fn new(track_id: u32, config: CodecConfig) -> Self {
        let timescale = default_timescale(&config);
        TrackSpec { track_id, timescale, config, language: *b"und" }
    }

    pub(crate) fn kind(&self) -> MediaKind {
        self.config.codec.kind()
    }
}

pub(crate) fn validate_tracks(tracks: &[TrackSpec]) -> Result<(), Mp4Error> {
    if tracks.is_empty() {
        return Err(Mp4Error::BadConfig("no tracks"));
    }
    for (i, t) in tracks.iter().enumerate() {
        if t.track_id == 0 || t.track_id == u32::MAX {
            return Err(Mp4Error::BadConfig("track_id must be 1..u32::MAX-1"));
        }
        if t.timescale == 0 {
            return Err(Mp4Error::BadConfig("timescale must be non-zero"));
        }
        if !matches!(t.kind(), MediaKind::Video | MediaKind::Audio) {
            return Err(Mp4Error::UnsupportedCodec(t.config.codec));
        }
        if tracks[..i].iter().any(|o| o.track_id == t.track_id) {
            return Err(Mp4Error::DuplicateTrack(t.track_id));
        }
    }
    Ok(())
}

pub(crate) fn mvhd(buf: &mut Vec<u8>, duration: u64, next_track_id: u32) {
    let p = begin_full_box(buf, b"mvhd", 1, 0);
    put_u64(buf, 0);
    put_u64(buf, 0);
    put_u32(buf, MOVIE_TIMESCALE);
    put_u64(buf, duration);
    put_u32(buf, 0x0001_0000); // rate
    put_u16(buf, 0x0100); // volume
    buf.extend_from_slice(&[0; 10]);
    for m in MATRIX {
        put_u32(buf, m);
    }
    buf.extend_from_slice(&[0; 24]);
    put_u32(buf, next_track_id);
    end_box(buf, p);
}

/// trak の中で、サンプル表（stbl の stsd 以外）を呼び出し側が書く。
pub(crate) struct TrakTiming<'a> {
    /// tkhd の長さ（movie timescale）。
    pub track_duration: u64,
    /// mdhd の長さ（media timescale）。
    pub media_duration: u64,
    /// edts/elst に書く項目 (segment_duration, media_time)。空なら書かない。
    pub edits: &'a [(u64, i64)],
}

pub(crate) fn trak(
    buf: &mut Vec<u8>,
    t: &TrackSpec,
    timing: &TrakTiming<'_>,
    write_tables: &mut dyn FnMut(&mut Vec<u8>),
) -> Result<(), Mp4Error> {
    let video = t.kind() == MediaKind::Video;
    let tr = begin_box(buf, b"trak");
    // tkhd（enabled | in_movie）
    let p = begin_full_box(buf, b"tkhd", 1, 3);
    put_u64(buf, 0);
    put_u64(buf, 0);
    put_u32(buf, t.track_id);
    put_u32(buf, 0);
    put_u64(buf, timing.track_duration);
    buf.extend_from_slice(&[0; 8]);
    put_u16(buf, 0); // layer
    put_u16(buf, if video { 0 } else { 1 }); // alternate_group
    put_u16(buf, if video { 0 } else { 0x0100 });
    put_u16(buf, 0);
    for m in MATRIX {
        put_u32(buf, m);
    }
    let (w, h) = match (video, t.config.video) {
        (true, Some(v)) => (v.width, v.height),
        _ => (0, 0),
    };
    put_u32(buf, w.min(0xFFFF) << 16);
    put_u32(buf, h.min(0xFFFF) << 16);
    end_box(buf, p);

    if !timing.edits.is_empty() {
        let e = begin_box(buf, b"edts");
        let l = begin_full_box(buf, b"elst", 1, 0);
        put_u32(buf, timing.edits.len() as u32);
        for &(dur, mt) in timing.edits {
            put_u64(buf, dur);
            put_u64(buf, mt as u64);
            put_u32(buf, 0x0001_0000);
        }
        end_box(buf, l);
        end_box(buf, e);
    }

    let md = begin_box(buf, b"mdia");
    let p = begin_full_box(buf, b"mdhd", 1, 0);
    put_u64(buf, 0);
    put_u64(buf, 0);
    put_u32(buf, t.timescale);
    put_u64(buf, timing.media_duration);
    let l = &t.language;
    let lang = if l.iter().all(|c| c.is_ascii_lowercase()) {
        ((l[0] - 0x60) as u16) << 10 | ((l[1] - 0x60) as u16) << 5 | (l[2] - 0x60) as u16
    } else {
        0x55C4 // und
    };
    put_u16(buf, lang);
    put_u16(buf, 0);
    end_box(buf, p);

    let p = begin_full_box(buf, b"hdlr", 0, 0);
    put_u32(buf, 0);
    buf.extend_from_slice(if video { b"vide" } else { b"soun" });
    buf.extend_from_slice(&[0; 12]);
    buf.extend_from_slice(if video { b"VideoHandler\0" } else { b"SoundHandler\0" });
    end_box(buf, p);

    let mi = begin_box(buf, b"minf");
    if video {
        let p = begin_full_box(buf, b"vmhd", 0, 1);
        buf.extend_from_slice(&[0; 8]);
        end_box(buf, p);
    } else {
        let p = begin_full_box(buf, b"smhd", 0, 0);
        buf.extend_from_slice(&[0; 4]);
        end_box(buf, p);
    }
    let di = begin_box(buf, b"dinf");
    let dr = begin_full_box(buf, b"dref", 0, 0);
    put_u32(buf, 1);
    let u = begin_full_box(buf, b"url ", 0, 1);
    end_box(buf, u);
    end_box(buf, dr);
    end_box(buf, di);

    let st = begin_box(buf, b"stbl");
    let sd = begin_full_box(buf, b"stsd", 0, 0);
    put_u32(buf, 1);
    write_sample_entry(buf, &t.config, t.track_id)?;
    end_box(buf, sd);
    write_tables(buf);
    end_box(buf, st);
    end_box(buf, mi);
    end_box(buf, md);
    end_box(buf, tr);
    Ok(())
}

fn empty_tables(buf: &mut Vec<u8>) {
    for ty in [b"stts", b"stsc", b"stco"] {
        let p = begin_full_box(buf, ty, 0, 0);
        put_u32(buf, 0);
        end_box(buf, p);
    }
    let p = begin_full_box(buf, b"stsz", 0, 0);
    put_u32(buf, 0);
    put_u32(buf, 0);
    end_box(buf, p);
}

/// 初期化セグメント（ftyp + moov）を書く。サンプル表は空で、mvex/trex を持つ。
pub fn write_init_segment(tracks: &[TrackSpec], out: &mut Vec<u8>) -> Result<(), Mp4Error> {
    validate_tracks(tracks)?;
    let mut buf = Vec::with_capacity(1024);
    ftyp(&mut buf, b"iso6", 0, &[b"iso6", b"mp41"]);
    let moov = begin_box(&mut buf, b"moov");
    let next = tracks.iter().map(|t| t.track_id).max().unwrap_or(0) + 1;
    mvhd(&mut buf, 0, next);
    let timing = TrakTiming { track_duration: 0, media_duration: 0, edits: &[] };
    for t in tracks {
        trak(&mut buf, t, &timing, &mut empty_tables)?;
    }
    let mv = begin_box(&mut buf, b"mvex");
    for t in tracks {
        let p = begin_full_box(&mut buf, b"trex", 0, 0);
        put_u32(&mut buf, t.track_id);
        put_u32(&mut buf, 1); // default_sample_description_index
        put_u32(&mut buf, 0);
        put_u32(&mut buf, 0);
        put_u32(&mut buf, 0);
        end_box(&mut buf, p);
    }
    end_box(&mut buf, mv);
    end_box(&mut buf, moov);
    out.extend_from_slice(&buf);
    Ok(())
}
