//! 最小限の読み取り（初期化セグメント、フラグメント、断片化しない MP4 の stbl）。
//!
//! 上限: 箱の大きさは入力の長さを超えない、サンプル数は run ごと・ファイルごとに上限あり。
//! 壊れた入力は `Err` を返す。

use crate::entry::{boxes, find_box, parse_sample_entry};
use crate::error::Mp4Error;
use crate::fragment::MAX_SAMPLES_PER_RUN;
use bytes::Bytes;
use senca_core::CodecConfig;
use std::collections::HashMap;

/// 1 回の読み取りで返すサンプルの上限。
const MAX_SAMPLES_TOTAL: usize = 1 << 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Trex {
    sample_description_index: u32,
    duration: u32,
    size: u32,
    flags: u32,
}

/// 断片化しない MP4 のサンプル表。
#[derive(Debug, Clone, Default)]
struct SampleTable {
    durations: Vec<(u32, u32)>,
    ctts: Vec<(u32, i32)>,
    stss: Option<Vec<u32>>,
    sizes: Vec<u32>,
    stsc: Vec<(u32, u32)>,
    chunk_offsets: Vec<u64>,
    /// elst の先頭の media_time（なければ 0）。
    edit_media_time: i64,
}

/// トラックの情報。
#[derive(Debug, Clone)]
pub struct Mp4Track {
    pub track_id: u32,
    pub timescale: u32,
    /// hdlr の handler_type（"vide"・"soun" など）。
    pub handler: [u8; 4],
    /// 知らないサンプル記述なら `None`。
    pub config: Option<CodecConfig>,
    trex: Trex,
    table: SampleTable,
}

/// moov から読んだもの。
#[derive(Debug, Clone)]
pub struct InitSegment {
    pub tracks: Vec<Mp4Track>,
    /// mvex がある（フラグメントで続く）。
    pub fragmented: bool,
}

/// 読み出したサンプル。時刻はトラックの timescale。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadSample {
    pub track_id: u32,
    pub dts: i64,
    pub duration: u32,
    pub cts_offset: i32,
    pub keyframe: bool,
    pub data: Bytes,
}

fn rd_u32(d: &[u8], p: usize) -> Result<u32, Mp4Error> {
    d.get(p..p + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(Mp4Error::Malformed("truncated box"))
}

fn rd_u64(d: &[u8], p: usize) -> Result<u64, Mp4Error> {
    d.get(p..p + 8)
        .map(|b| u64::from_be_bytes(b.try_into().expect("8 bytes")))
        .ok_or(Mp4Error::Malformed("truncated box"))
}

fn full(d: &[u8]) -> Result<(u8, u32, &[u8]), Mp4Error> {
    let v = rd_u32(d, 0)?;
    Ok(((v >> 24) as u8, v & 0xFF_FFFF, &d[4..]))
}

/// (件数, 1 件の大きさ) を検査して表の本体を返す。
fn counted(d: &[u8], entry: usize) -> Result<(usize, &[u8]), Mp4Error> {
    let n = rd_u32(d, 0)? as usize;
    let body = &d[4..];
    if n.checked_mul(entry).is_none_or(|x| x > body.len()) {
        return Err(Mp4Error::Malformed("table count exceeds box"));
    }
    Ok((n, body))
}

/// moov を探して読む（ファイル全体でも初期化セグメントだけでもよい）。
pub fn parse_init(data: &[u8]) -> Result<InitSegment, Mp4Error> {
    let moov = find_box(data, b"moov")?.ok_or(Mp4Error::Malformed("no moov"))?;
    parse_moov(moov)
}

fn parse_moov(moov: &[u8]) -> Result<InitSegment, Mp4Error> {
    let mut tracks = Vec::new();
    let mut trex: HashMap<u32, Trex> = HashMap::new();
    let mut fragmented = false;
    for b in boxes(moov) {
        let (ty, body) = b?;
        match &ty {
            b"trak" => {
                if tracks.len() >= 64 {
                    return Err(Mp4Error::TooLarge("track count"));
                }
                tracks.push(parse_trak(body)?);
            }
            b"mvex" => {
                fragmented = true;
                for b in boxes(body) {
                    let (ty, body) = b?;
                    if &ty == b"trex" {
                        let (_, _, d) = full(body)?;
                        trex.insert(
                            rd_u32(d, 0)?,
                            Trex {
                                sample_description_index: rd_u32(d, 4)?,
                                duration: rd_u32(d, 8)?,
                                size: rd_u32(d, 12)?,
                                flags: rd_u32(d, 16)?,
                            },
                        );
                    }
                }
            }
            _ => {}
        }
    }
    for t in &mut tracks {
        if let Some(x) = trex.get(&t.track_id) {
            t.trex = *x;
        }
    }
    Ok(InitSegment { tracks, fragmented })
}

fn parse_trak(trak: &[u8]) -> Result<Mp4Track, Mp4Error> {
    let tkhd = find_box(trak, b"tkhd")?.ok_or(Mp4Error::Malformed("no tkhd"))?;
    let (v, _, d) = full(tkhd)?;
    let track_id = if v == 1 { rd_u32(d, 16)? } else { rd_u32(d, 8)? };
    let mut st = SampleTable::default();
    if let Some(edts) = find_box(trak, b"edts")?
        && let Some(elst) = find_box(edts, b"elst")?
    {
        let (v, _, d) = full(elst)?;
        let (n, body) = counted(d, if v == 1 { 20 } else { 12 })?;
        // 空の編集を飛ばして最初のメディア編集の media_time
        for i in 0..n {
            let mt = if v == 1 { rd_u64(body, i * 20 + 8)? as i64 } else { rd_u32(body, i * 12 + 4)? as i32 as i64 };
            if mt >= 0 {
                st.edit_media_time = mt;
                break;
            }
        }
    }
    let mdia = find_box(trak, b"mdia")?.ok_or(Mp4Error::Malformed("no mdia"))?;
    let mdhd = find_box(mdia, b"mdhd")?.ok_or(Mp4Error::Malformed("no mdhd"))?;
    let (v, _, d) = full(mdhd)?;
    let timescale = if v == 1 { rd_u32(d, 16)? } else { rd_u32(d, 8)? };
    if timescale == 0 {
        return Err(Mp4Error::Malformed("zero timescale"));
    }
    let hdlr = find_box(mdia, b"hdlr")?.ok_or(Mp4Error::Malformed("no hdlr"))?;
    let handler: [u8; 4] = hdlr.get(8..12).ok_or(Mp4Error::Malformed("hdlr"))?.try_into().expect("4 bytes");
    let minf = find_box(mdia, b"minf")?.ok_or(Mp4Error::Malformed("no minf"))?;
    let stbl = find_box(minf, b"stbl")?.ok_or(Mp4Error::Malformed("no stbl"))?;
    let mut config = None;
    for b in boxes(stbl) {
        let (ty, body) = b?;
        match &ty {
            b"stsd" => {
                let (_, _, d) = full(body)?;
                if rd_u32(d, 0)? >= 1 {
                    if let Some(Ok((ety, ebody))) = boxes(&d[4..]).next() {
                        config = parse_sample_entry(ety, ebody)?;
                    } else {
                        return Err(Mp4Error::Malformed("stsd"));
                    }
                }
            }
            b"stts" => {
                let (_, _, d) = full(body)?;
                let (n, t) = counted(d, 8)?;
                st.durations = (0..n).map(|i| Ok((rd_u32(t, i * 8)?, rd_u32(t, i * 8 + 4)?))).collect::<Result<_, Mp4Error>>()?;
            }
            b"ctts" => {
                let (_, _, d) = full(body)?;
                let (n, t) = counted(d, 8)?;
                st.ctts = (0..n).map(|i| Ok((rd_u32(t, i * 8)?, rd_u32(t, i * 8 + 4)? as i32))).collect::<Result<_, Mp4Error>>()?;
            }
            b"stss" => {
                let (_, _, d) = full(body)?;
                let (n, t) = counted(d, 4)?;
                st.stss = Some((0..n).map(|i| rd_u32(t, i * 4)).collect::<Result<_, _>>()?);
            }
            b"stsz" => {
                let (_, _, d) = full(body)?;
                let fixed = rd_u32(d, 0)?;
                let n = rd_u32(d, 4)? as usize;
                if n > MAX_SAMPLES_TOTAL {
                    return Err(Mp4Error::TooLarge("sample count"));
                }
                st.sizes = if fixed != 0 {
                    vec![fixed; n]
                } else {
                    let (n, t) = counted(&d[4..], 4)?;
                    (0..n).map(|i| rd_u32(t, i * 4)).collect::<Result<_, _>>()?
                };
            }
            b"stsc" => {
                let (_, _, d) = full(body)?;
                let (n, t) = counted(d, 12)?;
                st.stsc = (0..n).map(|i| Ok((rd_u32(t, i * 12)?, rd_u32(t, i * 12 + 4)?))).collect::<Result<_, Mp4Error>>()?;
            }
            b"stco" => {
                let (_, _, d) = full(body)?;
                let (n, t) = counted(d, 4)?;
                st.chunk_offsets = (0..n).map(|i| rd_u32(t, i * 4).map(|v| v as u64)).collect::<Result<_, _>>()?;
            }
            b"co64" => {
                let (_, _, d) = full(body)?;
                let (n, t) = counted(d, 8)?;
                st.chunk_offsets = (0..n).map(|i| rd_u64(t, i * 8)).collect::<Result<_, _>>()?;
            }
            _ => {}
        }
    }
    Ok(Mp4Track { track_id, timescale, handler, config, trex: Trex::default(), table: st })
}

/// 断片化しない MP4 のサンプルを、stbl から組み立てる（`data` はファイル全体）。
fn progressive_samples(t: &Mp4Track, data: &Bytes, out: &mut Vec<ReadSample>) -> Result<(), Mp4Error> {
    let tb = &t.table;
    let n = tb.sizes.len();
    if out.len() + n > MAX_SAMPLES_TOTAL {
        return Err(Mp4Error::TooLarge("sample count"));
    }
    // 各サンプルの位置
    let mut offsets = Vec::with_capacity(n);
    let mut si = 0usize;
    for (ci, &off) in tb.chunk_offsets.iter().enumerate() {
        let chunk_no = ci as u32 + 1;
        let per = tb
            .stsc
            .iter()
            .rev()
            .find(|(first, _)| *first <= chunk_no)
            .map(|e| e.1)
            .ok_or(Mp4Error::Malformed("stsc"))?;
        let mut p = off;
        for _ in 0..per {
            if si >= n {
                break;
            }
            offsets.push(p);
            p += tb.sizes[si] as u64;
            si += 1;
        }
    }
    if offsets.len() != n {
        return Err(Mp4Error::Malformed("chunk table does not cover samples"));
    }
    let mut durs = tb.durations.iter().flat_map(|&(c, d)| std::iter::repeat_n(d, c as usize));
    let mut ctts = tb.ctts.iter().flat_map(|&(c, o)| std::iter::repeat_n(o, c as usize));
    let mut dts: i64 = 0;
    for (i, &offset) in offsets.iter().enumerate() {
        let dur = durs.next().ok_or(Mp4Error::Malformed("stts shorter than samples"))?;
        let cts = if tb.ctts.is_empty() { 0 } else { ctts.next().ok_or(Mp4Error::Malformed("ctts shorter than samples"))? };
        let start = usize::try_from(offset).map_err(|_| Mp4Error::Malformed("offset"))?;
        let end = start.checked_add(tb.sizes[i] as usize).ok_or(Mp4Error::Malformed("offset"))?;
        if end > data.len() {
            return Err(Mp4Error::Malformed("sample outside file"));
        }
        let keyframe = match &tb.stss {
            None => true,
            Some(k) => k.binary_search(&(i as u32 + 1)).is_ok(),
        };
        out.push(ReadSample {
            track_id: t.track_id,
            dts: dts - tb.edit_media_time,
            duration: dur,
            cts_offset: cts,
            keyframe,
            data: data.slice(start..end),
        });
        dts += dur as i64;
    }
    Ok(())
}

/// フラグメントを順に読む。トラックごとの次の DTS（tfdt が無いとき用）を覚えている。
#[derive(Debug, Clone)]
pub struct Mp4Reader {
    init: InitSegment,
    next_dts: HashMap<u32, i64>,
}

impl Mp4Reader {
    pub fn new(init: InitSegment) -> Self {
        Mp4Reader { init, next_dts: HashMap::new() }
    }

    pub fn init(&self) -> &InitSegment {
        &self.init
    }

    /// moof（と mdat）を含むバイト列を読む。data_offset は `data` の先頭を基準にする
    /// （default-base-is-moof なら moof の先頭、base_data_offset があればその値）。
    pub fn read_fragments(&mut self, data: &Bytes) -> Result<Vec<ReadSample>, Mp4Error> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos < data.len() {
            let (ty, body, rest) = crate::entry::split_box(&data[pos..])?;
            let box_len = data.len() - pos - rest.len();
            if &ty == b"moof" {
                let hdr = box_len - body.len();
                self.moof(data, pos, pos + hdr, box_len - hdr, &mut out)?;
            }
            pos += box_len;
        }
        Ok(out)
    }

    fn moof(&mut self, data: &Bytes, moof_start: usize, body_start: usize, body_len: usize, out: &mut Vec<ReadSample>) -> Result<(), Mp4Error> {
        let moof = &data[body_start..body_start + body_len];
        // 前の traf のデータの終わり（base が明示されず default-base-is-moof でもないとき用）
        let mut prev_end: Option<u64> = None;
        for b in boxes(moof) {
            let (ty, traf) = b?;
            if &ty != b"traf" {
                continue;
            }
            let tfhd = find_box(traf, b"tfhd")?.ok_or(Mp4Error::Malformed("no tfhd"))?;
            let (_, flags, d) = full(tfhd)?;
            let track_id = rd_u32(d, 0)?;
            let track = self.init.tracks.iter().find(|t| t.track_id == track_id).ok_or(Mp4Error::UnknownTrack(track_id))?;
            let mut p = 4;
            let mut base = if flags & 0x02_0000 != 0 { moof_start as u64 } else { prev_end.unwrap_or(moof_start as u64) };
            if flags & 0x01 != 0 {
                base = rd_u64(d, p)?;
                p += 8;
            }
            if flags & 0x02 != 0 {
                p += 4;
            }
            let mut def = track.trex;
            if flags & 0x08 != 0 {
                def.duration = rd_u32(d, p)?;
                p += 4;
            }
            if flags & 0x10 != 0 {
                def.size = rd_u32(d, p)?;
                p += 4;
            }
            if flags & 0x20 != 0 {
                def.flags = rd_u32(d, p)?;
            }
            let mut dts = match find_box(traf, b"tfdt")? {
                Some(t) => {
                    let (v, _, d) = full(t)?;
                    let v = if v == 1 { rd_u64(d, 0)? } else { rd_u32(d, 0)? as u64 };
                    i64::try_from(v).map_err(|_| Mp4Error::Malformed("tfdt"))?
                }
                None => *self.next_dts.get(&track_id).unwrap_or(&0),
            };
            let mut data_pos = base;
            for b in boxes(traf) {
                let (ty, trun) = b?;
                if &ty != b"trun" {
                    continue;
                }
                let (version, tf, d) = full(trun)?;
                let count = rd_u32(d, 0)? as usize;
                if count > MAX_SAMPLES_PER_RUN || out.len() + count > MAX_SAMPLES_TOTAL {
                    return Err(Mp4Error::TooLarge("sample count"));
                }
                let mut q = 4;
                if tf & 0x01 != 0 {
                    let off = rd_u32(d, q)? as i32 as i64;
                    data_pos = u64::try_from(base as i64 + off).map_err(|_| Mp4Error::Malformed("data_offset"))?;
                    q += 4;
                }
                let mut first_flags = None;
                if tf & 0x04 != 0 {
                    first_flags = Some(rd_u32(d, q)?);
                    q += 4;
                }
                let per = [0x100, 0x200, 0x400, 0x800].iter().filter(|&&f| tf & f != 0).count() * 4;
                if count.checked_mul(per).is_none_or(|x| x > d.len().saturating_sub(q)) {
                    return Err(Mp4Error::Malformed("trun count exceeds box"));
                }
                for i in 0..count {
                    let mut dur = def.duration;
                    let mut size = def.size;
                    let mut sflags = if i == 0 { first_flags.unwrap_or(def.flags) } else { def.flags };
                    let mut cts = 0i32;
                    if tf & 0x100 != 0 {
                        dur = rd_u32(d, q)?;
                        q += 4;
                    }
                    if tf & 0x200 != 0 {
                        size = rd_u32(d, q)?;
                        q += 4;
                    }
                    if tf & 0x400 != 0 {
                        sflags = rd_u32(d, q)?;
                        q += 4;
                    }
                    if tf & 0x800 != 0 {
                        let v = rd_u32(d, q)?;
                        cts = if version == 0 { i32::try_from(v).unwrap_or(i32::MAX) } else { v as i32 };
                        q += 4;
                    }
                    let start = usize::try_from(data_pos).map_err(|_| Mp4Error::Malformed("offset"))?;
                    let end = start.checked_add(size as usize).ok_or(Mp4Error::Malformed("offset"))?;
                    if end > data.len() {
                        return Err(Mp4Error::Malformed("sample outside data"));
                    }
                    out.push(ReadSample {
                        track_id,
                        dts,
                        duration: dur,
                        cts_offset: cts,
                        keyframe: sflags & 0x0001_0000 == 0,
                        data: data.slice(start..end),
                    });
                    dts = dts.checked_add(dur as i64).ok_or(Mp4Error::Malformed("decode time overflow"))?;
                    data_pos = end as u64;
                }
            }
            prev_end = Some(data_pos);
            self.next_dts.insert(track_id, dts);
        }
        Ok(())
    }
}

/// ファイル全体（断片化しない MP4、または初期化セグメント＋フラグメント）を読む。
/// サンプルはトラックごとに DTS 順、トラックは moov の順ではなくファイル内の順。
pub fn read_file(data: &Bytes) -> Result<(InitSegment, Vec<ReadSample>), Mp4Error> {
    let init = parse_init(data)?;
    let mut out = Vec::new();
    for t in &init.tracks {
        if !t.table.sizes.is_empty() {
            progressive_samples(t, data, &mut out)?;
        }
    }
    let mut r = Mp4Reader::new(init);
    out.extend(r.read_fragments(data)?);
    Ok((r.init, out))
}
