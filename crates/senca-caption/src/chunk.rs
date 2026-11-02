//! 音声 AU から、音声認識に送る自己完結した音声ファイル（チャンク）を作る。復号しない（ADR-0004）。
//!
//! - AAC（raw_data_block + AudioSpecificConfig）→ 音声だけの MP4（`.m4a`、既定）または ADTS（`.aac`）
//! - Opus（パケット + OpusHead）→ Ogg Opus（`.ogg`）
//!
//! # 切り方
//!
//! 固定の窓（既定 5 秒）を、重なり（既定 1 秒）を持たせてずらしていく。境界は AU の境目に合わせる
//! （窓の先頭は「目標の時刻以降で最初の AU」）。各チャンクは先頭の媒体時刻（µs、ストリームの時間軸）を持つので、
//! 認識結果のチャンク内の時刻に足せばストリームの時刻になる。
//!
//! 欠落（AU の `discontinuity`、時刻の飛び・逆行）があればそこで切り、残りを短いチャンクとして出して
//! 次から新しい窓を始める（重なりはない）。
//!
//! # 上限と背圧
//!
//! - 溜める AU は窓 1 つ分＋α。バイト数が上限を超えたら窓の途中でも切る。
//! - 出来上がったチャンクの待ち行列は `max_queued` 本まで。認識が実時間より遅く、行列があふれたら
//!   **古いチャンクから捨てる**（ライブを優先）。捨てた数は次に渡すチャンクの `dropped_before` に載る。
//! - ファイルへの詰め替えは [`AudioChunker::take_chunk`] で取り出すときに行う（捨てるチャンクには手間をかけない）。

use crate::error::{Error, Result};
use crate::ogg::ogg_opus_file;
use bytes::Bytes;
use senca_aac::AudioSpecificConfig;
use senca_core::time::{Rescale, Timebase};
use senca_core::{AccessUnit, Codec, CodecConfig};
use senca_fmp4::{Mp4Writer, SampleInfo, TrackSpec};
use senca_opus::OpusHead;
use std::collections::VecDeque;
use std::sync::Arc;

/// チャンクのファイル形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChunkFormat {
    /// AAC を音声だけの MP4 に（OpenAI の `/v1/audio/transcriptions` が受ける形式）。
    M4a,
    /// AAC を ADTS に（`.aac`。受ける相手は限られる）。
    Adts,
    /// Opus を Ogg Opus に。
    OggOpus,
}

impl ChunkFormat {
    pub fn mime(self) -> &'static str {
        match self {
            ChunkFormat::M4a => "audio/mp4",
            ChunkFormat::Adts => "audio/aac",
            ChunkFormat::OggOpus => "audio/ogg",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ChunkFormat::M4a => "m4a",
            ChunkFormat::Adts => "aac",
            ChunkFormat::OggOpus => "ogg",
        }
    }

    /// コーデックの既定の形式（AAC → m4a、Opus → ogg）。それ以外は対応しない。
    pub fn default_for(codec: Codec) -> Result<ChunkFormat> {
        match codec {
            Codec::Aac => Ok(ChunkFormat::M4a),
            Codec::Opus => Ok(ChunkFormat::OggOpus),
            _ => Err(Error::Unsupported("captions need AAC or Opus audio (no transcoding)")),
        }
    }

    fn accepts(self, codec: Codec) -> bool {
        matches!((self, codec), (ChunkFormat::M4a | ChunkFormat::Adts, Codec::Aac) | (ChunkFormat::OggOpus, Codec::Opus))
    }
}

/// 切り方の設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkerConfig {
    /// 窓の長さ（µs）。
    pub window_us: u64,
    /// 前のチャンクとの重なり（µs）。窓より短いこと。
    pub overlap_us: u64,
    /// 欠落や終わりで出す短いチャンクの最短（これより短いものは捨てる）。
    pub min_chunk_us: u64,
    /// 待ち行列に置けるチャンクの数（1 以上）。あふれたら古いものを捨てる。
    pub max_queued: usize,
    /// 1 チャンクの音声データの上限（バイト）。超えそうなら窓の途中で切る。
    pub max_chunk_bytes: usize,
    /// 予想した次の AU の時刻からこれ以上ずれたら欠落とみなす（µs）。
    pub max_gap_us: u64,
    /// 形式。`None` ならコーデックの既定（[`ChunkFormat::default_for`]）。
    pub format: Option<ChunkFormat>,
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        ChunkerConfig {
            window_us: 5_000_000,
            overlap_us: 1_000_000,
            min_chunk_us: 300_000,
            max_queued: 2,
            max_chunk_bytes: 4 << 20,
            max_gap_us: 200_000,
            format: None,
        }
    }
}

impl ChunkerConfig {
    pub fn validate(&self) -> Result<()> {
        if !(500_000..=60_000_000).contains(&self.window_us) {
            return Err(Error::InvalidConfig("window must be 0.5..60 s"));
        }
        if self.overlap_us * 2 > self.window_us {
            return Err(Error::InvalidConfig("overlap must be at most half of the window"));
        }
        if self.max_queued == 0 || self.max_queued > 64 {
            return Err(Error::InvalidConfig("max_queued must be 1..64"));
        }
        if self.max_chunk_bytes < 4096 || self.max_chunk_bytes > 64 << 20 {
            return Err(Error::InvalidConfig("max_chunk_bytes must be 4 KiB..64 MiB"));
        }
        if self.min_chunk_us > self.window_us {
            return Err(Error::InvalidConfig("min_chunk_us must not exceed the window"));
        }
        Ok(())
    }

    /// 窓をずらす量（窓 − 重なり）。
    pub fn step_us(&self) -> u64 {
        self.window_us - self.overlap_us
    }
}

/// チャンクの時刻の情報（認識結果をまとめるときに使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkInfo {
    pub seq: u64,
    /// ファイルの再生位置 0 に当たる媒体時刻（µs）。
    pub start_us: i64,
    pub duration_us: u64,
    /// このチャンクの直前に背圧で捨てたチャンクの数。
    pub dropped_before: u64,
    /// 前のチャンクと続いていない（欠落・設定の変更の後）。
    pub discontinuity: bool,
}

impl ChunkInfo {
    pub fn end_us(&self) -> i64 {
        self.start_us.saturating_add(self.duration_us as i64)
    }
}

/// 音声認識に送る音声ファイル。
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub info: ChunkInfo,
    pub format: ChunkFormat,
    pub data: Bytes,
}

impl AudioChunk {
    /// multipart で送るときのファイル名（媒体時刻は入れない）。
    pub fn file_name(&self) -> String {
        format!("chunk-{}.{}", self.info.seq, self.format.extension())
    }

    pub fn mime(&self) -> &'static str {
        self.format.mime()
    }
}

/// 数え上げ。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChunkerStats {
    pub aus: u64,
    pub chunks_made: u64,
    pub chunks_taken: u64,
    /// 背圧で捨てたチャンク。
    pub chunks_dropped: u64,
    /// 短すぎて捨てた端のチャンク。
    pub short_chunks_dropped: u64,
    pub discontinuities: u64,
}

#[derive(Debug)]
enum Source {
    Aac { asc: AudioSpecificConfig, config: CodecConfig, samples_per_frame: u32, rate: u32 },
    Opus { head: OpusHead },
}

impl Source {
    fn codec(&self) -> Codec {
        match self {
            Source::Aac { .. } => Codec::Aac,
            Source::Opus { .. } => Codec::Opus,
        }
    }
}

#[derive(Debug, Clone)]
struct Au {
    pts_us: i64,
    dur_us: u64,
    payload: Bytes,
}

#[derive(Debug)]
struct Pending {
    seq: u64,
    source: Arc<Source>,
    format: ChunkFormat,
    aus: Vec<Au>,
    dropped_before: u64,
    discontinuity: bool,
}

/// AU を受け取り、チャンクを待ち行列に積む状態機械。
#[derive(Debug)]
pub struct AudioChunker {
    cfg: ChunkerConfig,
    source: Option<(Arc<Source>, Bytes, ChunkFormat)>,
    aus: VecDeque<Au>,
    bytes: usize,
    expected_next_us: Option<i64>,
    /// 次に作るチャンクが前と続いていない。
    next_discontinuity: bool,
    /// 最後に切ってから（欠落・設定の変更の後）チャンクを作ったか。
    emitted_in_run: bool,
    queue: VecDeque<Pending>,
    next_seq: u64,
    stats: ChunkerStats,
}

impl AudioChunker {
    pub fn new(cfg: ChunkerConfig) -> Result<Self> {
        cfg.validate()?;
        Ok(AudioChunker {
            cfg,
            source: None,
            aus: VecDeque::new(),
            bytes: 0,
            expected_next_us: None,
            next_discontinuity: false,
            emitted_in_run: false,
            queue: VecDeque::new(),
            next_seq: 0,
            stats: ChunkerStats::default(),
        })
    }

    pub fn config(&self) -> &ChunkerConfig {
        &self.cfg
    }

    pub fn stats(&self) -> &ChunkerStats {
        &self.stats
    }

    /// 音声トラックの設定。前と違えば、溜めた分を出してから切り替える。
    pub fn set_config(&mut self, config: &CodecConfig) -> Result<()> {
        if let Some((_, record, _)) = &self.source
            && *record == config.record
        {
            return Ok(());
        }
        let format = match self.cfg.format {
            Some(f) => f,
            None => ChunkFormat::default_for(config.codec)?,
        };
        if !format.accepts(config.codec) {
            return Err(Error::InvalidConfig("chunk format does not match the codec"));
        }
        let source = match config.codec {
            Codec::Aac => {
                let asc = AudioSpecificConfig::parse(&config.record)?;
                let rate = asc.sampling_frequency;
                if rate == 0 {
                    return Err(Error::Unsupported("AAC sampling frequency"));
                }
                if format == ChunkFormat::Adts {
                    // ADTS で表せるかを先に確かめる
                    senca_aac::AdtsHeader::for_payload(&asc, 0)?;
                }
                Source::Aac { samples_per_frame: asc.samples_per_frame(), asc, config: config.clone(), rate }
            }
            Codec::Opus => Source::Opus { head: OpusHead::parse(&config.record)? },
            _ => return Err(Error::Unsupported("captions need AAC or Opus audio (no transcoding)")),
        };
        if self.source.is_some() {
            self.cut();
        }
        self.source = Some((Arc::new(source), config.record.clone(), format));
        Ok(())
    }

    /// AU を 1 つ入れる。音声トラックの AU だけを入れること。
    pub fn push(&mut self, au: &AccessUnit) -> Result<()> {
        let Some((source, _, _)) = &self.source else {
            return Err(Error::InvalidInput("codec config must be set before audio"));
        };
        if au.codec != source.codec() {
            return Err(Error::InvalidInput("access unit codec differs from the config"));
        }
        if au.payload.is_empty() {
            return Ok(());
        }
        let dur_us = match source.as_ref() {
            Source::Aac { samples_per_frame, rate, .. } => {
                (*samples_per_frame as i64).rescale(Timebase::new(1, *rate), Timebase::MICROS).unwrap_or(0) as u64
            }
            Source::Opus { .. } => {
                let n = senca_opus::packet_samples(&au.payload)?;
                (n as i64).rescale(Timebase::new(1, 48_000), Timebase::MICROS).unwrap_or(0) as u64
            }
        };
        let pts_us = au.pts.rescale(au.timebase, Timebase::MICROS).ok_or(Error::InvalidInput("timestamp overflow"))?;
        self.stats.aus += 1;
        let jumped = self.expected_next_us.is_some_and(|e| (pts_us - e).unsigned_abs() > self.cfg.max_gap_us);
        if au.discontinuity || jumped {
            if self.expected_next_us.is_some() {
                self.stats.discontinuities += 1;
            }
            self.cut();
        }
        self.expected_next_us = Some(pts_us.saturating_add(dur_us as i64));
        self.bytes += au.payload.len();
        self.aus.push_back(Au { pts_us, dur_us, payload: au.payload.clone() });
        self.emit_full_windows();
        Ok(())
    }

    /// 終わり（ストリームの終了）。溜めた分を最後のチャンクにする。
    pub fn flush(&mut self) {
        self.cut();
    }

    /// 待っているチャンクの数。
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// いちばん古いチャンクを取り出してファイルにする（認識の相手が空いたときに呼ぶ）。
    pub fn take_chunk(&mut self) -> Result<Option<AudioChunk>> {
        let Some(p) = self.queue.pop_front() else {
            return Ok(None);
        };
        self.stats.chunks_taken += 1;
        encode(&p).map(Some)
    }

    /// 窓が埋まった分をチャンクにする。
    fn emit_full_windows(&mut self) {
        loop {
            let (Some(first), Some(last)) = (self.aus.front(), self.aus.back()) else {
                return;
            };
            let ws = first.pts_us;
            let we = ws.saturating_add(self.cfg.window_us as i64);
            let buffered_end = last.pts_us.saturating_add(last.dur_us as i64);
            let over_bytes = self.bytes > self.cfg.max_chunk_bytes;
            if buffered_end < we && !over_bytes {
                return;
            }
            // 窓に入る AU（先頭の AU の時刻 + 窓まで。バイトの上限を超えない範囲）
            let mut taken = Vec::new();
            let mut size = 0usize;
            for a in &self.aus {
                if a.pts_us.saturating_add(a.dur_us as i64) > we && !taken.is_empty() {
                    break;
                }
                if size + a.payload.len() > self.cfg.max_chunk_bytes && !taken.is_empty() {
                    break;
                }
                size += a.payload.len();
                taken.push(a.clone());
            }
            let chunk_end = taken.last().map(|a| a.pts_us + a.dur_us as i64).unwrap_or(we);
            let span = (chunk_end - ws).max(0) as u64;
            // 次の窓は（チャンクの終わり − 重なり）以降で最初の AU から
            let next = if span > self.cfg.overlap_us { chunk_end - self.cfg.overlap_us as i64 } else { chunk_end };
            self.make_pending(taken);
            while let Some(a) = self.aus.front() {
                if a.pts_us < next {
                    self.bytes -= a.payload.len();
                    self.aus.pop_front();
                } else {
                    break;
                }
            }
        }
    }

    /// 欠落・終わり: 溜めた分を（短くても最短以上なら）チャンクにして、次は新しい窓から。
    fn cut(&mut self) {
        let rest: Vec<Au> = self.aus.drain(..).collect();
        self.bytes = 0;
        self.expected_next_us = None;
        if let (Some(f), Some(l)) = (rest.first(), rest.last()) {
            let span = (l.pts_us + l.dur_us as i64 - f.pts_us).max(0) as u64;
            // 直前のチャンクと重なる分しかない残りは、もう送ってある
            if span >= self.cfg.min_chunk_us && (!self.emitted_in_run || span > self.cfg.overlap_us) {
                self.make_pending(rest);
            } else {
                self.stats.short_chunks_dropped += 1;
            }
        }
        self.next_discontinuity = true;
        self.emitted_in_run = false;
    }

    fn make_pending(&mut self, aus: Vec<Au>) {
        let Some((source, _, format)) = &self.source else {
            return;
        };
        self.emitted_in_run = true;
        let mut p = Pending {
            seq: self.next_seq,
            source: source.clone(),
            format: *format,
            aus,
            dropped_before: 0,
            discontinuity: std::mem::take(&mut self.next_discontinuity),
        };
        self.next_seq += 1;
        self.stats.chunks_made += 1;
        if self.queue.len() >= self.cfg.max_queued
            && let Some(old) = self.queue.pop_front()
        {
            self.stats.chunks_dropped += 1;
            let carry = old.dropped_before + 1;
            match self.queue.front_mut() {
                Some(next) => {
                    next.dropped_before += carry;
                    next.discontinuity |= old.discontinuity;
                }
                None => {
                    p.dropped_before += carry;
                    p.discontinuity |= old.discontinuity;
                }
            }
        }
        self.queue.push_back(p);
    }
}

fn encode(p: &Pending) -> Result<AudioChunk> {
    let format = p.format;
    let first = p.aus.first().ok_or(Error::InvalidInput("empty chunk"))?;
    let last = p.aus.last().ok_or(Error::InvalidInput("empty chunk"))?;
    let mut start_us = first.pts_us;
    let mut duration_us = (last.pts_us + last.dur_us as i64 - first.pts_us).max(0) as u64;
    let data = match (p.source.as_ref(), format) {
        (Source::Aac { asc, .. }, ChunkFormat::Adts) => {
            let mut out = Vec::with_capacity(p.aus.iter().map(|a| a.payload.len() + 9).sum());
            for a in &p.aus {
                out.extend_from_slice(&senca_aac::wrap_adts(asc, &a.payload)?);
            }
            out
        }
        (Source::Aac { config, samples_per_frame, .. }, ChunkFormat::M4a) => m4a(config, *samples_per_frame, &p.aus)?,
        (Source::Opus { head }, ChunkFormat::OggOpus) => {
            let packets: Vec<Bytes> = p.aus.iter().map(|a| a.payload.clone()).collect();
            let serial = 0x5E4C_A000 ^ (p.seq as u32);
            // 再生位置 0 は pre-skip を捨てた後
            let skip_us = (head.pre_skip as i64).rescale(Timebase::new(1, 48_000), Timebase::MICROS).unwrap_or(0);
            start_us += skip_us;
            duration_us = duration_us.saturating_sub(skip_us as u64);
            ogg_opus_file(head, &packets, serial)?
        }
        _ => return Err(Error::InvalidConfig("chunk format does not match the codec")),
    };
    Ok(AudioChunk {
        info: ChunkInfo { seq: p.seq, start_us, duration_us, dropped_before: p.dropped_before, discontinuity: p.discontinuity },
        format,
        data: Bytes::from(data),
    })
}

/// 音声だけの断片化しない MP4。サンプルの時刻は先頭からの差を標本化周波数に換算したもの。
fn m4a(config: &CodecConfig, samples_per_frame: u32, aus: &[Au]) -> Result<Vec<u8>> {
    let spec = TrackSpec::new(1, config.clone());
    let ts = Timebase::new(1, spec.timescale);
    let base = aus[0].pts_us;
    let mut w = Mp4Writer::new(vec![spec])?;
    let dts: Vec<i64> = aus
        .iter()
        .map(|a| (a.pts_us - base).rescale(Timebase::MICROS, ts).ok_or(Error::InvalidInput("timestamp overflow")))
        .collect::<Result<_>>()?;
    let mut prev = -1i64;
    for (i, a) in aus.iter().enumerate() {
        let d = dts[i].max(prev + 1);
        let next = dts.get(i + 1).copied().unwrap_or(d + samples_per_frame as i64);
        let duration = u32::try_from((next - d).max(1)).map_err(|_| Error::InvalidInput("sample duration"))?;
        let size = u32::try_from(a.payload.len()).map_err(|_| Error::TooLarge("sample"))?;
        w.add_sample(1, SampleInfo { dts: d, duration, cts_offset: 0, keyframe: true, size })?;
        prev = d;
    }
    let mut out = w.finish()?;
    for a in aus {
        out.extend_from_slice(&a.payload);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use senca_core::TrackId;

    fn aac_config() -> CodecConfig {
        // AAC-LC 48 kHz ステレオ
        senca_aac::codec_config(&[0x11, 0x90], 0).unwrap()
    }

    fn opus_config() -> CodecConfig {
        OpusHead::new(2, 312, 48000).unwrap().codec_config(0).unwrap()
    }

    fn au(codec: Codec, pts: i64, tb: Timebase, payload: Bytes) -> AccessUnit {
        AccessUnit {
            track: TrackId(1),
            codec,
            epoch: 0,
            sequence: 0,
            config_generation: 0,
            pts,
            dts: pts,
            duration: 0,
            timebase: tb,
            keyframe: true,
            discontinuity: false,
            capture_time_ns: None,
            payload,
        }
    }

    /// AAC の AU（中身は試験用の適当なバイト列。復号しないので構わない）。
    fn feed_aac(c: &mut AudioChunker, from_frame: i64, frames: i64) {
        let tb = Timebase::new(1, 48_000);
        for i in from_frame..from_frame + frames {
            c.push(&au(Codec::Aac, i * 1024, tb, Bytes::from(vec![0x21u8; 100]))).unwrap();
        }
    }

    #[test]
    fn config_validation() {
        assert!(ChunkerConfig::default().validate().is_ok());
        let bad = ChunkerConfig { overlap_us: 3_000_000, ..ChunkerConfig::default() };
        assert!(bad.validate().is_err());
        let bad = ChunkerConfig { max_queued: 0, ..ChunkerConfig::default() };
        assert!(bad.validate().is_err());
        let bad = ChunkerConfig { window_us: 100, ..ChunkerConfig::default() };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn rejects_other_codecs_and_missing_config() {
        let mut c = AudioChunker::new(ChunkerConfig::default()).unwrap();
        assert!(c.push(&au(Codec::Aac, 0, Timebase::MILLIS, Bytes::from_static(b"x"))).is_err());
        let mut v = aac_config();
        v.codec = Codec::H264;
        assert!(matches!(c.set_config(&v), Err(Error::Unsupported(_))));
        let wrong = AudioChunker::new(ChunkerConfig { format: Some(ChunkFormat::OggOpus), ..ChunkerConfig::default() });
        assert!(wrong.unwrap().set_config(&aac_config()).is_err());
    }

    #[test]
    fn windows_with_overlap_on_au_boundaries() {
        let mut c = AudioChunker::new(ChunkerConfig { max_queued: 64, ..ChunkerConfig::default() }).unwrap();
        c.set_config(&aac_config()).unwrap();
        // 20 秒分（48 kHz、1024 サンプル = 21.333 ms）
        feed_aac(&mut c, 0, 938);
        let mut chunks = Vec::new();
        while let Some(ch) = c.take_chunk().unwrap() {
            chunks.push(ch);
        }
        assert!(chunks.len() >= 4, "{}", chunks.len());
        let frame_us = 1024.0 * 1e6 / 48000.0;
        for (i, ch) in chunks.iter().enumerate() {
            assert_eq!(ch.info.seq, i as u64);
            assert_eq!(ch.format, ChunkFormat::M4a);
            assert!(ch.info.duration_us <= 5_000_000 && ch.info.duration_us > 4_900_000, "{:?}", ch.info);
            // AU の境界
            let k = ch.info.start_us as f64 / frame_us;
            assert!((k - k.round()).abs() < 0.01);
            if i > 0 {
                let prev = &chunks[i - 1].info;
                let overlap = prev.end_us() - ch.info.start_us;
                assert!((900_000..=1_100_000).contains(&overlap), "overlap {overlap}");
                assert!(!ch.info.discontinuity);
                assert_eq!(ch.info.dropped_before, 0);
            }
        }
        // m4a として読める（自前の読み手で）
        let (init, samples) = senca_fmp4::read_file(&chunks[1].data).unwrap();
        assert_eq!(init.tracks[0].timescale, 48000);
        assert!(samples.len() > 230);
        assert!(samples.windows(2).all(|w| w[1].dts - w[0].dts == 1024));
    }

    #[test]
    fn backpressure_drops_oldest_with_marker() {
        let mut c = AudioChunker::new(ChunkerConfig::default()).unwrap();
        c.set_config(&aac_config()).unwrap();
        feed_aac(&mut c, 0, 2000); // 約 42 秒 → 10 本ほど作られるが行列は 2 本まで
        assert_eq!(c.queued(), 2);
        assert!(c.stats().chunks_dropped >= 7);
        let a = c.take_chunk().unwrap().unwrap();
        let b = c.take_chunk().unwrap().unwrap();
        assert_eq!(a.info.dropped_before, c.stats().chunks_dropped);
        assert_eq!(b.info.dropped_before, 0);
        assert_eq!(b.info.seq, a.info.seq + 1);
        assert!(c.take_chunk().unwrap().is_none());
    }

    #[test]
    fn gap_cuts_and_marks_discontinuity() {
        let mut c = AudioChunker::new(ChunkerConfig { max_queued: 16, ..ChunkerConfig::default() }).unwrap();
        c.set_config(&aac_config()).unwrap();
        feed_aac(&mut c, 0, 100); // 2.1 秒
        feed_aac(&mut c, 1000, 300); // 時刻が飛ぶ
        c.flush();
        let a = c.take_chunk().unwrap().unwrap();
        assert!(!a.info.discontinuity);
        assert!(a.info.duration_us > 2_000_000 && a.info.duration_us < 2_200_000);
        let b = c.take_chunk().unwrap().unwrap();
        assert!(b.info.discontinuity);
        assert_eq!(b.info.start_us, 1000 * 1024 * 1_000_000 / 48000);
        assert_eq!(c.stats().discontinuities, 1);
    }

    #[test]
    fn adts_chunks_are_framed() {
        let mut c = AudioChunker::new(ChunkerConfig { format: Some(ChunkFormat::Adts), ..ChunkerConfig::default() }).unwrap();
        c.set_config(&aac_config()).unwrap();
        feed_aac(&mut c, 0, 300);
        let ch = c.take_chunk().unwrap().unwrap();
        assert_eq!(ch.file_name(), "chunk-0.aac");
        let (asc, frames) = senca_aac::adts_to_raw(&ch.data).unwrap();
        assert_eq!(asc.sampling_frequency, 48000);
        assert_eq!(frames.len(), 234); // 5 秒に収まる AU 数
        assert!(frames.iter().all(|f| f.len() == 100));
    }

    #[test]
    fn opus_chunks_are_ogg() {
        let mut c = AudioChunker::new(ChunkerConfig::default()).unwrap();
        c.set_config(&opus_config()).unwrap();
        let tb = Timebase::new(1, 48_000);
        for i in 0..500 {
            // 20 ms の CELT パケット
            c.push(&au(Codec::Opus, i * 960, tb, Bytes::from_static(&[0xFC, 0, 0]))).unwrap();
        }
        let ch = c.take_chunk().unwrap().unwrap();
        assert_eq!(ch.format, ChunkFormat::OggOpus);
        assert_eq!(ch.mime(), "audio/ogg");
        // pre-skip（312 サンプル = 6.5 ms）の分だけ先頭がずれる
        assert_eq!(ch.info.start_us, 6500);
        assert_eq!(ch.info.duration_us, 5_000_000 - 6500);
        let ps = crate::ogg::tests::packets(&ch.data);
        assert_eq!(ps.len(), 2 + 250);
        // 次のチャンクの先頭は 4 秒の AU
        let ch2 = c.take_chunk().unwrap().unwrap();
        assert_eq!(ch2.info.start_us, 4_000_000 + 6500);
    }

    #[test]
    fn byte_limit_cuts_early() {
        let cfg = ChunkerConfig { max_chunk_bytes: 8192, max_queued: 64, ..ChunkerConfig::default() };
        let mut c = AudioChunker::new(cfg).unwrap();
        c.set_config(&aac_config()).unwrap();
        feed_aac(&mut c, 0, 500);
        while let Some(ch) = c.take_chunk().unwrap() {
            assert!(ch.data.len() < 8192 + 2048, "{}", ch.data.len());
        }
    }

    #[test]
    fn config_change_flushes() {
        let mut c = AudioChunker::new(ChunkerConfig { max_queued: 8, ..ChunkerConfig::default() }).unwrap();
        c.set_config(&aac_config()).unwrap();
        feed_aac(&mut c, 0, 100);
        // 同じ設定は何もしない
        c.set_config(&aac_config()).unwrap();
        assert_eq!(c.queued(), 0);
        let other = senca_aac::codec_config(&[0x12, 0x10], 1).unwrap(); // 44.1 kHz ステレオ
        c.set_config(&other).unwrap();
        assert_eq!(c.queued(), 1);
    }
}
