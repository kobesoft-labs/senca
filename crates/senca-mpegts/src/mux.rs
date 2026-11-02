//! TS の多重化。

use crate::codec::{
    self, avcc_nals, h264_nal, h265_nal, h265_nal_type, opus_packet_samples, parse_asc, parse_opus_head,
    write_adts,
};
use crate::psi::{self, PmtStream};
use crate::{SYNC_BYTE, TS_PACKET_SIZE, stream_type};
use bytes::BufMut;
use senca_core::time::{Rescale, Timebase};
use senca_core::{AccessUnit, Codec, CodecConfig, TrackId};
use std::fmt;

const MASK33: i64 = (1 << 33) - 1;
const PID_PAT: u16 = 0;

/// 多重化の設定。
#[derive(Debug, Clone)]
pub struct MuxerConfig {
    pub transport_stream_id: u16,
    pub program_number: u16,
    pub pmt_pid: u16,
    /// 最初のエレメンタリーストリームの PID（以降は追加順に 1 ずつ増える）。
    pub first_es_pid: u16,
    /// PCR を DTS からどれだけ前にするか（90 kHz）。PTS/DTS は入力のまま変えない。
    pub pcr_delay: u32,
    /// PCR の最大間隔（90 kHz）。PCR を載せる PID の PES ごとに、この間隔を過ぎていれば載せる。0 なら毎回。
    pub pcr_interval: u32,
    /// PAT/PMT を自動で繰り返す間隔（DTS、90 kHz）。`None` なら [`TsMuxer::start_segment`] と最初の AU の前だけ。
    pub tables_interval: Option<u32>,
    /// 映像のキーフレームの前に PAT/PMT を入れる。
    pub tables_on_keyframe: bool,
    /// [`TsMuxer::start_segment`] で連続性カウンターを 0 に戻す（HLS のセグメントを独立させるとき）。
    pub reset_cc_on_segment: bool,
}

impl Default for MuxerConfig {
    fn default() -> Self {
        MuxerConfig {
            transport_stream_id: 1,
            program_number: 1,
            pmt_pid: 0x1000,
            first_es_pid: 0x100,
            pcr_delay: 0,
            pcr_interval: 0,
            tables_interval: Some(45_000),
            tables_on_keyframe: true,
            reset_cc_on_segment: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MuxError {
    /// TS に載せられないコーデック。
    UnsupportedCodec(Codec),
    /// 設定レコードが読めない・TS で表せない。
    BadConfig(&'static str),
    /// AU の中身が内部表現（ADR-0003）に合わない。
    BadPayload(&'static str),
    UnknownTrack(TrackId),
    DuplicateTrack(TrackId),
    /// 時刻の換算が溢れた。
    Timestamp,
    /// PID・stream_id が足りない。
    TooManyTracks,
}

impl fmt::Display for MuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MuxError::UnsupportedCodec(c) => write!(f, "codec {c} is not supported in MPEG-TS"),
            MuxError::BadConfig(s) => write!(f, "bad codec config: {s}"),
            MuxError::BadPayload(s) => write!(f, "bad access unit payload: {s}"),
            MuxError::UnknownTrack(t) => write!(f, "unknown track {}", t.0),
            MuxError::DuplicateTrack(t) => write!(f, "duplicate track {}", t.0),
            MuxError::Timestamp => f.write_str("timestamp overflow"),
            MuxError::TooManyTracks => f.write_str("too many tracks"),
        }
    }
}

impl std::error::Error for MuxError {}

#[derive(Debug, Clone)]
enum TrackKind {
    /// パラメーターセットを Annex B（開始コード付き）にして連結したもの。
    H264 { param_sets: Vec<u8> },
    H265 { param_sets: Vec<u8> },
    Aac { object_type: u8, sf_index: u8, channel_config: u8 },
    Opus { channel_code: u8, pending_trim: u32 },
}

#[derive(Debug, Clone)]
struct MuxTrack {
    track: TrackId,
    pid: u16,
    stream_id: u8,
    stream_type: u8,
    descriptors: Vec<u8>,
    kind: TrackKind,
    generation: u32,
    /// 次の AU の前にパラメーターセットを入れる。
    need_params: bool,
}

/// TS の多重化器。I/O を持たず、出力は呼び出し側のバッファに足す。
#[derive(Debug)]
pub struct TsMuxer {
    cfg: MuxerConfig,
    tracks: Vec<MuxTrack>,
    /// PID ごとの連続性カウンター（次に使う値）。
    cc: std::collections::HashMap<u16, u8>,
    pmt_version: u8,
    pmt_dirty: bool,
    tables_written: bool,
    last_tables_dts: Option<i64>,
    last_pcr_dts: Option<i64>,
    next_video_sid: u8,
    next_audio_sid: u8,
}

fn annexb_params(nals: &[&[u8]]) -> Vec<u8> {
    let mut v = Vec::new();
    for n in nals {
        v.extend_from_slice(&[0, 0, 0, 1]);
        v.extend_from_slice(n);
    }
    v
}

impl TsMuxer {
    pub fn new(cfg: MuxerConfig) -> Self {
        TsMuxer {
            cfg,
            tracks: Vec::new(),
            cc: Default::default(),
            pmt_version: 0,
            pmt_dirty: false,
            tables_written: false,
            last_tables_dts: None,
            last_pcr_dts: None,
            next_video_sid: 0xE0,
            next_audio_sid: 0xC0,
        }
    }

    /// トラックを足す。割り当てた PID を返す。PMT は次に出すときに更新される。
    pub fn add_track(&mut self, track: TrackId, config: &CodecConfig) -> Result<u16, MuxError> {
        if self.tracks.iter().any(|t| t.track == track) {
            return Err(MuxError::DuplicateTrack(track));
        }
        let pid = self.cfg.first_es_pid.checked_add(self.tracks.len() as u16).ok_or(MuxError::TooManyTracks)?;
        if pid >= 0x1FFF || pid == self.cfg.pmt_pid || self.tracks.len() >= 16 {
            return Err(MuxError::TooManyTracks);
        }
        let (kind, stream_type, descriptors) = Self::track_kind(config)?;
        let stream_id = match config.codec {
            Codec::H264 | Codec::H265 => {
                let s = self.next_video_sid;
                if s > 0xEF {
                    return Err(MuxError::TooManyTracks);
                }
                self.next_video_sid += 1;
                s
            }
            Codec::Aac => {
                let s = self.next_audio_sid;
                if s > 0xDF {
                    return Err(MuxError::TooManyTracks);
                }
                self.next_audio_sid += 1;
                s
            }
            _ => 0xBD,
        };
        self.tracks.push(MuxTrack {
            track,
            pid,
            stream_id,
            stream_type,
            descriptors,
            kind,
            generation: config.generation,
            need_params: true,
        });
        if self.tables_written {
            self.pmt_dirty = true;
        }
        Ok(pid)
    }

    /// 設定を差し替える（世代が変わったとき）。次の AU の前にパラメーターセットを入れる。
    pub fn update_config(&mut self, track: TrackId, config: &CodecConfig) -> Result<(), MuxError> {
        let (kind, st, desc) = Self::track_kind(config)?;
        let t = self.tracks.iter_mut().find(|t| t.track == track).ok_or(MuxError::UnknownTrack(track))?;
        if st != t.stream_type {
            return Err(MuxError::BadConfig("codec change on a track"));
        }
        if desc != t.descriptors {
            t.descriptors = desc;
            self.pmt_dirty = true;
        }
        // Opus の先頭の切り詰めは最初だけ
        t.kind = match (kind, &t.kind) {
            (TrackKind::Opus { channel_code, .. }, TrackKind::Opus { pending_trim, .. }) => {
                TrackKind::Opus { channel_code, pending_trim: *pending_trim }
            }
            (k, _) => k,
        };
        t.generation = config.generation;
        t.need_params = true;
        Ok(())
    }

    /// トラックの PID。
    pub fn pid(&self, track: TrackId) -> Option<u16> {
        self.tracks.iter().find(|t| t.track == track).map(|t| t.pid)
    }

    fn track_kind(config: &CodecConfig) -> Result<(TrackKind, u8, Vec<u8>), MuxError> {
        Ok(match config.codec {
            Codec::H264 => {
                let nals = codec::parse_avcc(&config.record).ok_or(MuxError::BadConfig("AVCDecoderConfigurationRecord"))?;
                (TrackKind::H264 { param_sets: annexb_params(&nals) }, stream_type::H264, Vec::new())
            }
            Codec::H265 => {
                let nals = codec::parse_hvcc(&config.record).ok_or(MuxError::BadConfig("HEVCDecoderConfigurationRecord"))?;
                (TrackKind::H265 { param_sets: annexb_params(&nals) }, stream_type::H265, Vec::new())
            }
            Codec::Aac => {
                let asc = parse_asc(&config.record).ok_or(MuxError::BadConfig("AudioSpecificConfig"))?;
                if !(1..=4).contains(&asc.object_type) {
                    return Err(MuxError::BadConfig("AAC object type not representable in ADTS"));
                }
                if asc.sf_index > 12 {
                    return Err(MuxError::BadConfig("AAC sample rate not representable in ADTS"));
                }
                if asc.channel_config == 0 || asc.channel_config > 7 {
                    return Err(MuxError::BadConfig("AAC channel configuration"));
                }
                (
                    TrackKind::Aac {
                        object_type: asc.object_type,
                        sf_index: asc.sf_index,
                        channel_config: asc.channel_config,
                    },
                    stream_type::AAC_ADTS,
                    Vec::new(),
                )
            }
            Codec::Opus => {
                let h = parse_opus_head(&config.record).ok_or(MuxError::BadConfig("OpusHead"))?;
                let code = match (h.family, h.channels) {
                    (0, 1 | 2) => h.channels,
                    (1, 3..=8) => {
                        let d = codec::opus_default_head(h.channels).ok_or(MuxError::BadConfig("Opus channels"))?;
                        if d.streams != h.streams || d.coupled != h.coupled || d.mapping != h.mapping {
                            return Err(MuxError::BadConfig("Opus channel mapping not representable in TS"));
                        }
                        h.channels
                    }
                    _ => return Err(MuxError::BadConfig("Opus channel mapping not representable in TS")),
                };
                let desc = vec![
                    psi::DESC_REGISTRATION,
                    4,
                    b'O',
                    b'p',
                    b'u',
                    b's',
                    psi::DESC_EXTENSION,
                    2,
                    psi::EXT_OPUS,
                    code,
                ];
                (
                    TrackKind::Opus { channel_code: code, pending_trim: h.pre_skip as u32 },
                    stream_type::PRIVATE_PES,
                    desc,
                )
            }
            c => return Err(MuxError::UnsupportedCodec(c)),
        })
    }

    fn pcr_pid(&self) -> u16 {
        self.tracks
            .iter()
            .find(|t| matches!(t.kind, TrackKind::H264 { .. } | TrackKind::H265 { .. }))
            .or(self.tracks.first())
            .map(|t| t.pid)
            .unwrap_or(0x1FFF)
    }

    fn next_cc(&mut self, pid: u16) -> u8 {
        let c = self.cc.entry(pid).or_insert(0);
        let v = *c;
        *c = (v + 1) & 0x0F;
        v
    }

    /// セグメントの先頭にする。PAT/PMT を書き、設定によっては連続性カウンターを 0 に戻す。
    /// 次の映像 AU にはパラメーターセットを入れる。
    pub fn start_segment(&mut self, out: &mut impl BufMut) {
        if self.cfg.reset_cc_on_segment {
            self.cc.clear();
        }
        for t in &mut self.tracks {
            t.need_params = true;
        }
        self.last_pcr_dts = None;
        self.write_tables(out);
    }

    /// PAT と PMT を書く。
    pub fn write_tables(&mut self, out: &mut impl BufMut) {
        if self.pmt_dirty {
            self.pmt_version = (self.pmt_version + 1) & 0x1F;
            self.pmt_dirty = false;
        }
        let pat = psi::build_pat(self.cfg.transport_stream_id, self.cfg.program_number, self.cfg.pmt_pid, 0);
        let streams: Vec<PmtStream<'_>> = self
            .tracks
            .iter()
            .map(|t| PmtStream { stream_type: t.stream_type, pid: t.pid, descriptors: &t.descriptors })
            .collect();
        let pmt = psi::build_pmt(self.cfg.program_number, self.pcr_pid(), self.pmt_version, &streams);
        drop(streams);
        self.write_section(PID_PAT, &pat, out);
        self.write_section(self.cfg.pmt_pid, &pmt, out);
        self.tables_written = true;
    }

    fn write_section(&mut self, pid: u16, sec: &[u8], out: &mut impl BufMut) {
        // pointer_field を含めて 1 パケットに収まらないときは続きのパケットに分ける
        let mut data = Vec::with_capacity(sec.len() + 1);
        data.push(0);
        data.extend_from_slice(sec);
        let mut first = true;
        for chunk in data.chunks(TS_PACKET_SIZE - 4) {
            let cc = self.next_cc(pid);
            out.put_u8(SYNC_BYTE);
            out.put_u8(if first { 0x40 } else { 0 } | (pid >> 8) as u8);
            out.put_u8(pid as u8);
            out.put_u8(0x10 | cc);
            out.put_slice(chunk);
            out.put_bytes(0xFF, TS_PACKET_SIZE - 4 - chunk.len());
            first = false;
        }
    }

    /// AU を 1 つ書く。必要なら先に PAT/PMT を書く。
    pub fn push(&mut self, au: &AccessUnit, out: &mut impl BufMut) -> Result<(), MuxError> {
        let idx = self.tracks.iter().position(|t| t.track == au.track).ok_or(MuxError::UnknownTrack(au.track))?;
        let pts = au.pts.rescale(au.timebase, Timebase::MPEG).ok_or(MuxError::Timestamp)?;
        let dts = au.dts.rescale(au.timebase, Timebase::MPEG).ok_or(MuxError::Timestamp)?;
        let is_video = matches!(self.tracks[idx].kind, TrackKind::H264 { .. } | TrackKind::H265 { .. });

        // ES を作る（失敗したら何も書かない）
        let es = self.build_es(idx, au)?;

        let due_interval = match (self.cfg.tables_interval, self.last_tables_dts) {
            (Some(iv), Some(last)) => dts.saturating_sub(last) >= iv as i64 || dts < last,
            _ => false,
        };
        if !self.tables_written || self.pmt_dirty || due_interval || (is_video && au.keyframe && self.cfg.tables_on_keyframe) {
            self.write_tables(out);
            self.last_tables_dts = Some(dts);
        } else if self.last_tables_dts.is_none() {
            self.last_tables_dts = Some(dts);
        }

        let t = &self.tracks[idx];
        let pid = t.pid;
        let with_pcr = pid == self.pcr_pid()
            && match self.last_pcr_dts {
                None => true,
                Some(last) => dts.saturating_sub(last) >= self.cfg.pcr_interval as i64 || dts < last,
            };
        let pcr = with_pcr.then(|| (dts - self.cfg.pcr_delay as i64) & MASK33);
        if with_pcr {
            self.last_pcr_dts = Some(dts);
        }
        let mut pes = Vec::with_capacity(es.len() + 19);
        pes.extend_from_slice(&[0, 0, 1, t.stream_id]);
        let has_dts = dts != pts;
        let hdr_len: usize = if has_dts { 10 } else { 5 };
        let body_len = 3 + hdr_len + es.len();
        // 映像で 65535 を超えるときは 0（長さ未定）
        let plen = if body_len > 0xFFFF || is_video { 0 } else { body_len as u16 };
        pes.extend_from_slice(&plen.to_be_bytes());
        pes.push(if is_video { 0x84 } else { 0x80 }); // data_alignment_indicator（映像）
        pes.push(if has_dts { 0xC0 } else { 0x80 });
        pes.push(hdr_len as u8);
        put_ts(&mut pes, if has_dts { 0x3 } else { 0x2 }, pts);
        if has_dts {
            put_ts(&mut pes, 0x1, dts);
        }
        pes.extend_from_slice(&es);
        let rai = au.keyframe && (is_video || !self.tracks.iter().any(|t| matches!(t.kind, TrackKind::H264 { .. } | TrackKind::H265 { .. })));
        self.write_pes(pid, &pes, pcr, rai, out);
        Ok(())
    }

    fn build_es(&mut self, idx: usize, au: &AccessUnit) -> Result<Vec<u8>, MuxError> {
        let t = &mut self.tracks[idx];
        if au.config_generation != t.generation {
            // 呼び出し側が update_config を呼ぶ前提だが、念のためパラメーターセットを入れる
            t.need_params = true;
        }
        match &mut t.kind {
            TrackKind::H264 { param_sets } | TrackKind::H265 { param_sets } => {
                let h265 = t.stream_type == stream_type::H265;
                let nals = avcc_nals(&au.payload).ok_or(MuxError::BadPayload("length-prefixed NAL units"))?;
                let mut es = Vec::with_capacity(au.payload.len() + param_sets.len() + 8);
                if h265 {
                    es.extend_from_slice(&[0, 0, 0, 1, (h265_nal::AUD << 1), 1, 0x50]);
                } else {
                    es.extend_from_slice(&[0, 0, 0, 1, h264_nal::AUD, 0xF0]);
                }
                let is_ps = |n: &[u8]| {
                    if h265 {
                        matches!(h265_nal_type(n), h265_nal::VPS | h265_nal::SPS | h265_nal::PPS)
                    } else {
                        matches!(n[0] & 0x1F, h264_nal::SPS | h264_nal::PPS)
                    }
                };
                let is_aud = |n: &[u8]| {
                    if h265 { h265_nal_type(n) == h265_nal::AUD } else { n[0] & 0x1F == h264_nal::AUD }
                };
                let has_ps = nals.iter().any(|n| is_ps(n));
                if (au.keyframe || t.need_params) && !has_ps {
                    es.extend_from_slice(param_sets);
                }
                t.need_params = false;
                for n in nals {
                    if is_aud(n) {
                        continue;
                    }
                    if h265 && n.len() < 2 {
                        return Err(MuxError::BadPayload("short NAL unit"));
                    }
                    es.extend_from_slice(&[0, 0, 0, 1]);
                    es.extend_from_slice(n);
                }
                Ok(es)
            }
            TrackKind::Aac { object_type, sf_index, channel_config } => {
                if au.payload.len() + 7 > 0x1FFF {
                    return Err(MuxError::BadPayload("AAC frame too large for ADTS"));
                }
                if au.payload.len() >= 7 && codec::parse_adts(&au.payload).is_some() {
                    return Err(MuxError::BadPayload("AAC payload already has an ADTS header"));
                }
                let mut es = Vec::with_capacity(au.payload.len() + 7);
                write_adts(&mut es, *object_type, *sf_index, *channel_config, au.payload.len());
                es.extend_from_slice(&au.payload);
                Ok(es)
            }
            TrackKind::Opus { pending_trim, .. } => {
                let samples = opus_packet_samples(&au.payload).ok_or(MuxError::BadPayload("Opus packet"))?;
                let trim = (*pending_trim).min(samples);
                *pending_trim -= trim;
                let mut es = Vec::with_capacity(au.payload.len() + 16);
                es.push(0x7F);
                es.push(0xE0 | if trim > 0 { 0x10 } else { 0 });
                let mut n = au.payload.len();
                while n >= 255 {
                    es.push(0xFF);
                    n -= 255;
                }
                es.push(n as u8);
                if trim > 0 {
                    es.extend_from_slice(&(trim as u16 & 0x1FFF).to_be_bytes());
                }
                es.extend_from_slice(&au.payload);
                Ok(es)
            }
        }
    }

    fn write_pes(&mut self, pid: u16, pes: &[u8], pcr: Option<i64>, rai: bool, out: &mut impl BufMut) {
        let mut pos = 0;
        let mut first = true;
        while pos < pes.len() {
            let remaining = pes.len() - pos;
            // 適応フィールドの中身（長さバイトを除く）
            let mut af: Vec<u8> = Vec::new();
            let mut af_present = false;
            if first && (pcr.is_some() || rai) {
                af_present = true;
                let mut flags = 0u8;
                if rai {
                    flags |= 0x40;
                }
                if pcr.is_some() {
                    flags |= 0x10;
                }
                af.push(flags);
                if let Some(base) = pcr {
                    let b = base as u64;
                    af.extend_from_slice(&[
                        (b >> 25) as u8,
                        (b >> 17) as u8,
                        (b >> 9) as u8,
                        (b >> 1) as u8,
                        ((b & 1) as u8) << 7 | 0x7E,
                        0,
                    ]);
                }
            }
            let mut space = TS_PACKET_SIZE - 4 - if af_present { 1 + af.len() } else { 0 };
            if remaining < space {
                let pad = space - remaining;
                if af_present {
                    af.resize(af.len() + pad, 0xFF);
                } else {
                    af_present = true;
                    if pad >= 2 {
                        af.push(0);
                        af.resize(pad - 1, 0xFF);
                    }
                }
                space = remaining;
            }
            let cc = self.next_cc(pid);
            out.put_u8(SYNC_BYTE);
            out.put_u8(if first { 0x40 } else { 0 } | (pid >> 8) as u8);
            out.put_u8(pid as u8);
            out.put_u8(if af_present { 0x30 } else { 0x10 } | cc);
            if af_present {
                out.put_u8(af.len() as u8);
                out.put_slice(&af);
            }
            out.put_slice(&pes[pos..pos + space]);
            pos += space;
            first = false;
        }
    }
}

fn put_ts(v: &mut Vec<u8>, prefix: u8, ts: i64) {
    let t = (ts & MASK33) as u64;
    v.push((prefix << 4) | (((t >> 30) as u8 & 7) << 1) | 1);
    v.extend_from_slice(&((((t >> 15) & 0x7FFF) << 1 | 1) as u16).to_be_bytes());
    v.extend_from_slice(&(((t & 0x7FFF) << 1 | 1) as u16).to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use senca_core::media::AudioParams;

    pub(crate) fn aac_config() -> CodecConfig {
        CodecConfig {
            codec: Codec::Aac,
            generation: 0,
            record: Bytes::from_static(&[0x11, 0x90]),
            video: None,
            audio: Some(AudioParams { sample_rate: 48000, channels: 2, object_type: 2 }),
        }
    }

    fn au(track: u32, codec: Codec, pts: i64, payload: &'static [u8]) -> AccessUnit {
        AccessUnit {
            track: TrackId(track),
            codec,
            epoch: 0,
            sequence: 0,
            config_generation: 0,
            pts,
            dts: pts,
            duration: 0,
            timebase: Timebase::MPEG,
            keyframe: true,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from_static(payload),
        }
    }

    #[test]
    fn packets_are_188_and_cc_increments() {
        let mut m = TsMuxer::new(MuxerConfig::default());
        m.add_track(TrackId(0), &aac_config()).unwrap();
        let mut out = Vec::new();
        for i in 0..20 {
            m.push(&au(0, Codec::Aac, i * 1920, &[0x21; 300]), &mut out).unwrap();
        }
        assert_eq!(out.len() % 188, 0);
        let mut last: Option<u8> = None;
        for p in out.chunks(188) {
            assert_eq!(p[0], 0x47);
            let pid = ((p[1] as u16 & 0x1F) << 8) | p[2] as u16;
            if pid == 0x100 {
                let cc = p[3] & 0xF;
                if let Some(l) = last {
                    assert_eq!(cc, (l + 1) & 0xF);
                }
                last = Some(cc);
            }
        }
    }

    #[test]
    fn rejects_bad_payload_and_codec() {
        let mut m = TsMuxer::new(MuxerConfig::default());
        let mut c = aac_config();
        c.codec = Codec::Vp8;
        assert_eq!(m.add_track(TrackId(0), &c), Err(MuxError::UnsupportedCodec(Codec::Vp8)));
        let mut out = Vec::new();
        assert!(matches!(m.push(&au(0, Codec::Aac, 0, &[1]), &mut out), Err(MuxError::UnknownTrack(_))));
        assert!(out.is_empty());
    }

    #[test]
    fn pts_encoding() {
        let mut v = Vec::new();
        put_ts(&mut v, 2, 0x1_2345_6789);
        assert_eq!(v[0] >> 4, 2);
        assert_eq!(crate::demux::read_ts(&v), Some(0x1_2345_6789));
        v.clear();
        put_ts(&mut v, 3, -1);
        assert_eq!(crate::demux::read_ts(&v), Some((1 << 33) - 1));
    }
}
