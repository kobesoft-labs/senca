//! TS の分離。

use crate::codec::{
    self, AAC_SAMPLE_RATES, H264Sps, H265Sps, build_asc, h264_nal, h265_nal, h265_nal_type, opus_packet_samples,
    parse_adts,
};
use crate::psi::{self, MAX_SECTION, SectionError};
use crate::{SYNC_BYTE, TS_PACKET_SIZE, stream_type};
use bytes::Bytes;
use senca_core::media::{AudioParams, VideoParams};
use senca_core::time::{Rescale, RolloverExtender, Timebase};
use senca_core::{AccessUnit, Codec, CodecConfig, TrackId};
use std::collections::{BTreeMap, HashMap, VecDeque};

const MASK33: u64 = (1 << 33) - 1;

/// 分離の上限。
#[derive(Debug, Clone)]
pub struct DemuxerConfig {
    /// 追跡するエレメンタリーストリームの数。超えた分は無視する。
    pub max_streams: usize,
    /// 1 つの PES の最大長（仕様 5.3 の最大 AU 16 MiB）。
    pub max_pes_size: usize,
    /// 溜めておくイベントの最大数。超えたら古い AU から捨てる。
    pub max_queued_events: usize,
}

impl Default for DemuxerConfig {
    fn default() -> Self {
        DemuxerConfig { max_streams: 16, max_pes_size: 16 << 20, max_queued_events: 4096 }
    }
}

/// 分離で起きたことの数え上げ。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DemuxStats {
    pub packets: u64,
    pub sync_losses: u64,
    pub transport_errors: u64,
    pub cc_errors: u64,
    pub duplicate_packets: u64,
    pub crc_errors: u64,
    pub psi_errors: u64,
    pub pes_errors: u64,
    pub pes_oversize: u64,
    pub es_errors: u64,
    /// 設定（SPS/PPS など）が揃う前に来て捨てた AU。
    pub dropped_before_config: u64,
    pub dropped_no_pts: u64,
    pub ignored_streams: u64,
    pub events_dropped: u64,
}

/// 分離の出力。
#[derive(Debug, Clone)]
pub enum DemuxEvent {
    /// PMT に対応するストリームが現れた。
    StreamAdded { track: TrackId, pid: u16, codec: Codec },
    /// 設定が決まった・変わった（AU より先に出る）。
    Config { track: TrackId, config: CodecConfig },
    AccessUnit(AccessUnit),
    /// PCR（27 MHz、33 bit の折り返しを広げた値）。
    Pcr { pid: u16, pcr: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EsKind {
    H264,
    H265,
    Aac,
    Opus { channel_code: u8 },
}

impl EsKind {
    fn codec(self) -> Codec {
        match self {
            EsKind::H264 => Codec::H264,
            EsKind::H265 => Codec::H265,
            EsKind::Aac => Codec::Aac,
            EsKind::Opus { .. } => Codec::Opus,
        }
    }
}

#[derive(Debug, Default)]
struct ParamSets {
    vps: BTreeMap<u32, Vec<u8>>,
    sps: BTreeMap<u32, Vec<u8>>,
    pps: BTreeMap<u32, Vec<u8>>,
    changed: bool,
}

impl ParamSets {
    fn put(map: &mut BTreeMap<u32, Vec<u8>>, id: u32, nal: &[u8], changed: &mut bool) {
        if map.get(&id).map(|v| v.as_slice()) != Some(nal) {
            map.insert(id, nal.to_vec());
            *changed = true;
        }
    }
}

#[derive(Debug)]
struct EsStream {
    track: TrackId,
    kind: EsKind,
    cc: Option<u8>,
    pes: Vec<u8>,
    pes_active: bool,
    discontinuity: bool,
    sequence: u64,
    config: Option<CodecConfig>,
    next_generation: u32,
    params: ParamSets,
    /// AAC: 前の PES から続く ADTS の断片とその PTS。
    carry: Vec<u8>,
    carry_pts: Option<i64>,
    /// 音声: 次のフレームの PTS（PES に PTS がないとき用）。
    next_pts: Option<i64>,
}

#[derive(Debug, Default)]
struct SectionBuf {
    buf: Vec<u8>,
    active: bool,
    cc: Option<u8>,
}

/// TS の分離器。[`push`](TsDemuxer::push) でバイト列を入れ、[`poll`](TsDemuxer::poll) でイベントを取り出す。
///
/// 最初に見つけた番組（PAT の program_number ≠ 0）だけを扱う。
#[derive(Debug)]
pub struct TsDemuxer {
    cfg: DemuxerConfig,
    buf: Vec<u8>,
    synced: bool,
    pmt_pid: Option<u16>,
    program_number: u16,
    pmt_version: Option<u8>,
    pcr_pid: Option<u16>,
    pat: SectionBuf,
    pmt: SectionBuf,
    streams: HashMap<u16, EsStream>,
    next_track: u32,
    ts_ext: RolloverExtender,
    pcr_ext: RolloverExtender,
    events: VecDeque<DemuxEvent>,
    stats: DemuxStats,
}

/// PES の 5 バイトの時刻を読む。
pub(crate) fn read_ts(b: &[u8]) -> Option<u64> {
    if b.len() < 5 {
        return None;
    }
    let v = (((b[0] >> 1) & 7) as u64) << 30
        | (b[1] as u64) << 22
        | ((b[2] >> 1) as u64) << 15
        | (b[3] as u64) << 7
        | (b[4] >> 1) as u64;
    Some(v)
}

impl TsDemuxer {
    pub fn new(cfg: DemuxerConfig) -> Self {
        TsDemuxer {
            cfg,
            buf: Vec::new(),
            synced: false,
            pmt_pid: None,
            program_number: 0,
            pmt_version: None,
            pcr_pid: None,
            pat: SectionBuf::default(),
            pmt: SectionBuf::default(),
            streams: HashMap::new(),
            next_track: 0,
            ts_ext: RolloverExtender::new(33),
            pcr_ext: RolloverExtender::new(33),
            events: VecDeque::new(),
            stats: DemuxStats::default(),
        }
    }

    pub fn stats(&self) -> &DemuxStats {
        &self.stats
    }

    /// 次のイベント。
    pub fn poll(&mut self) -> Option<DemuxEvent> {
        self.events.pop_front()
    }

    fn emit(&mut self, ev: DemuxEvent) {
        if self.events.len() >= self.cfg.max_queued_events {
            // AU を優先して捨てる（設定の通知は残す）
            if let Some(i) = self.events.iter().position(|e| matches!(e, DemuxEvent::AccessUnit(_) | DemuxEvent::Pcr { .. })) {
                self.events.remove(i);
            } else {
                self.events.pop_front();
            }
            self.stats.events_dropped += 1;
        }
        self.events.push_back(ev);
    }

    /// バイト列を入れる。任意の位置で切れていてよい。
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        let mut pos = 0usize;
        let buf = std::mem::take(&mut self.buf);
        loop {
            if !self.synced {
                // 3 パケット続けて同期バイトが並ぶ位置を探す
                let need = TS_PACKET_SIZE * 2 + 1;
                let mut found = None;
                while pos + need <= buf.len() {
                    if buf[pos] == SYNC_BYTE
                        && buf[pos + TS_PACKET_SIZE] == SYNC_BYTE
                        && buf[pos + 2 * TS_PACKET_SIZE] == SYNC_BYTE
                    {
                        found = Some(pos);
                        break;
                    }
                    pos += 1;
                }
                match found {
                    Some(p) => {
                        pos = p;
                        self.synced = true;
                    }
                    None => break,
                }
            }
            if pos + TS_PACKET_SIZE > buf.len() {
                break;
            }
            if buf[pos] != SYNC_BYTE {
                self.lose_sync();
                pos += 1;
                continue;
            }
            let mut pkt = [0u8; TS_PACKET_SIZE];
            pkt.copy_from_slice(&buf[pos..pos + TS_PACKET_SIZE]);
            self.packet(&pkt);
            pos += TS_PACKET_SIZE;
        }
        let mut rest = buf;
        rest.drain(..pos);
        // 同期が取れないまま溜め込まない
        let keep = TS_PACKET_SIZE * 3;
        if rest.len() > keep {
            let cut = rest.len() - keep;
            rest.drain(..cut);
        }
        self.buf = rest;
    }

    /// 入力の終わり。組み立て途中の PES を出す。
    pub fn flush(&mut self) {
        let pids: Vec<u16> = self.streams.keys().copied().collect();
        for pid in pids {
            self.finish_pes(pid);
        }
        self.buf.clear();
    }

    fn lose_sync(&mut self) {
        self.synced = false;
        self.stats.sync_losses += 1;
        for s in self.streams.values_mut() {
            if s.pes_active {
                s.pes_active = false;
                s.pes.clear();
                self.stats.pes_errors += 1;
            }
            s.cc = None;
            s.discontinuity = true;
        }
        self.pat = SectionBuf::default();
        self.pmt = SectionBuf::default();
    }

    fn packet(&mut self, p: &[u8; TS_PACKET_SIZE]) {
        self.stats.packets += 1;
        let tei = p[1] & 0x80 != 0;
        let pusi = p[1] & 0x40 != 0;
        let pid = ((p[1] as u16 & 0x1F) << 8) | p[2] as u16;
        let afc = (p[3] >> 4) & 3;
        let cc = p[3] & 0x0F;
        if pid == 0x1FFF {
            return;
        }
        if tei || afc == 0 {
            self.stats.transport_errors += 1;
            if let Some(s) = self.streams.get_mut(&pid) {
                s.pes_active = false;
                s.pes.clear();
                s.discontinuity = true;
            }
            return;
        }
        let mut pos = 4usize;
        let mut disc_ind = false;
        if afc & 2 != 0 {
            let afl = p[4] as usize;
            let max = if afc == 3 { 182 } else { 183 };
            if afl > max {
                self.stats.transport_errors += 1;
                return;
            }
            if afl > 0 {
                let flags = p[5];
                disc_ind = flags & 0x80 != 0;
                if flags & 0x10 != 0 && afl >= 7 && Some(pid) == self.pcr_pid {
                    let b = &p[6..12];
                    let base = (b[0] as u64) << 25 | (b[1] as u64) << 17 | (b[2] as u64) << 9 | (b[3] as u64) << 1 | (b[4] >> 7) as u64;
                    let ext = ((b[4] as u64 & 1) << 8) | b[5] as u64;
                    let base = self.pcr_ext.extend(base);
                    self.emit(DemuxEvent::Pcr { pid, pcr: base * 300 + ext as i64 });
                }
            }
            pos += 1 + afl;
        }
        let has_payload = afc & 1 != 0;
        let payload: &[u8] = if has_payload { &p[pos..] } else { &[] };

        // 連続性カウンター
        let cc_slot: Option<&mut Option<u8>> = if pid == 0 {
            Some(&mut self.pat.cc)
        } else if Some(pid) == self.pmt_pid {
            Some(&mut self.pmt.cc)
        } else {
            self.streams.get_mut(&pid).map(|s| &mut s.cc)
        };
        let Some(slot) = cc_slot else { return };
        let mut lost = false;
        if has_payload {
            if let Some(last) = *slot
                && !disc_ind
            {
                if cc == last {
                    self.stats.duplicate_packets += 1;
                    return;
                }
                if cc != (last + 1) & 0x0F {
                    lost = true;
                }
            }
            *slot = Some(cc);
        }
        if lost {
            self.stats.cc_errors += 1;
        }

        if pid == 0 {
            if lost {
                self.pat.active = false;
            }
            self.section_data(true, pusi, payload);
        } else if Some(pid) == self.pmt_pid {
            if lost {
                self.pmt.active = false;
            }
            self.section_data(false, pusi, payload);
        } else {
            self.es_data(pid, pusi, payload, lost);
        }
    }

    fn section_data(&mut self, is_pat: bool, pusi: bool, payload: &[u8]) {
        let mut sections: Vec<Vec<u8>> = Vec::new();
        {
            let sb = if is_pat { &mut self.pat } else { &mut self.pmt };
            let mut data = payload;
            if pusi {
                let Some((&ptr, rest)) = data.split_first() else { return };
                let ptr = ptr as usize;
                if ptr > rest.len() {
                    sb.active = false;
                    return;
                }
                if sb.active {
                    sb.buf.extend_from_slice(&rest[..ptr]);
                    take_sections(sb, &mut sections);
                }
                sb.buf.clear();
                sb.active = true;
                data = &rest[ptr..];
            } else if !sb.active {
                return;
            }
            if sb.active {
                sb.buf.extend_from_slice(data);
                if sb.buf.len() > MAX_SECTION * 2 {
                    sb.buf.clear();
                    sb.active = false;
                }
                take_sections(sb, &mut sections);
            }
        }
        for s in sections {
            match psi::parse_section(&s) {
                Ok(sec) if sec.current => {
                    if is_pat && sec.table_id == psi::TID_PAT {
                        self.on_pat(sec.body);
                    } else if !is_pat && sec.table_id == psi::TID_PMT && sec.ext == self.program_number {
                        let version = sec.version;
                        let body = sec.body.to_vec();
                        self.on_pmt(version, &body);
                    }
                }
                Ok(_) => {}
                Err(SectionError::Crc) => self.stats.crc_errors += 1,
                Err(SectionError::Syntax) => self.stats.psi_errors += 1,
            }
        }
    }

    fn on_pat(&mut self, body: &[u8]) {
        let Some(&(prog, pid)) = psi::pat_programs(body).iter().find(|(n, _)| *n != 0) else { return };
        if self.pmt_pid != Some(pid) || self.program_number != prog {
            self.pmt_pid = Some(pid);
            self.program_number = prog;
            self.pmt_version = None;
            self.pmt = SectionBuf::default();
        }
    }

    fn on_pmt(&mut self, version: u8, body: &[u8]) {
        if self.pmt_version == Some(version) {
            return;
        }
        let Some(info) = psi::parse_pmt(body) else {
            self.stats.psi_errors += 1;
            return;
        };
        self.pmt_version = Some(version);
        self.pcr_pid = (info.pcr_pid != 0x1FFF).then_some(info.pcr_pid);
        let prog_opus = psi::descriptors(info.program_descriptors).any(|(t, d)| t == psi::DESC_REGISTRATION && d.get(..4) == Some(b"Opus"));
        let mut added = Vec::new();
        let mut keep: Vec<u16> = Vec::new();
        for st in &info.streams {
            let kind = match st.stream_type {
                stream_type::H264 => Some(EsKind::H264),
                stream_type::H265 => Some(EsKind::H265),
                stream_type::AAC_ADTS => Some(EsKind::Aac),
                stream_type::PRIVATE_PES => {
                    let reg = psi::descriptors(st.descriptors).any(|(t, d)| t == psi::DESC_REGISTRATION && d.get(..4) == Some(b"Opus"));
                    if reg || prog_opus {
                        let code = psi::descriptors(st.descriptors)
                            .find(|(t, d)| *t == psi::DESC_EXTENSION && d.first() == Some(&psi::EXT_OPUS))
                            .and_then(|(_, d)| d.get(1).copied())
                            .unwrap_or(2);
                        Some(EsKind::Opus { channel_code: code })
                    } else {
                        None
                    }
                }
                _ => None,
            };
            let Some(kind) = kind else {
                self.stats.ignored_streams += 1;
                continue;
            };
            if matches!(st.pid, 0 | 1 | 0x1FFF) || Some(st.pid) == self.pmt_pid {
                self.stats.psi_errors += 1;
                continue;
            }
            keep.push(st.pid);
            if self.streams.get(&st.pid).map(|s| s.kind) == Some(kind) {
                continue;
            }
            if !self.streams.contains_key(&st.pid) && self.streams.len() >= self.cfg.max_streams {
                self.stats.ignored_streams += 1;
                continue;
            }
            let track = TrackId(self.next_track);
            self.next_track += 1;
            self.streams.insert(
                st.pid,
                EsStream {
                    track,
                    kind,
                    cc: None,
                    pes: Vec::new(),
                    pes_active: false,
                    discontinuity: false,
                    sequence: 0,
                    config: None,
                    next_generation: 0,
                    params: ParamSets::default(),
                    carry: Vec::new(),
                    carry_pts: None,
                    next_pts: None,
                },
            );
            added.push((track, st.pid, kind.codec()));
        }
        // PMT から消えた PID は捨てる
        self.streams.retain(|pid, _| keep.contains(pid));
        for (track, pid, codec) in added {
            self.emit(DemuxEvent::StreamAdded { track, pid, codec });
        }
    }

    fn es_data(&mut self, pid: u16, pusi: bool, payload: &[u8], lost: bool) {
        let max = self.cfg.max_pes_size;
        let Some(s) = self.streams.get_mut(&pid) else { return };
        if lost {
            if s.pes_active {
                self.stats.pes_errors += 1;
            }
            s.pes_active = false;
            s.pes.clear();
            s.discontinuity = true;
            s.carry.clear();
            s.carry_pts = None;
        }
        if pusi {
            if s.pes_active {
                self.finish_pes(pid);
            }
            let Some(s) = self.streams.get_mut(&pid) else { return };
            s.pes.clear();
            s.pes.extend_from_slice(payload);
            s.pes_active = true;
        } else if s.pes_active {
            if s.pes.len() + payload.len() > max + 64 {
                s.pes_active = false;
                s.pes.clear();
                s.discontinuity = true;
                self.stats.pes_oversize += 1;
                return;
            }
            s.pes.extend_from_slice(payload);
        } else {
            return;
        }
        // 長さが分かっていて揃ったら出す
        let Some(s) = self.streams.get(&pid) else { return };
        if s.pes.len() >= 6 {
            let plen = u16::from_be_bytes([s.pes[4], s.pes[5]]) as usize;
            if plen != 0 && s.pes.len() >= plen + 6 {
                self.finish_pes(pid);
            }
        }
    }

    fn finish_pes(&mut self, pid: u16) {
        let Some(s) = self.streams.get_mut(&pid) else { return };
        if !s.pes_active {
            return;
        }
        s.pes_active = false;
        let mut pes = std::mem::take(&mut s.pes);
        match self.parse_pes(&pes) {
            Some((pts, dts, start, end)) => self.on_es(pid, &pes[start..end], pts, dts),
            None => {
                self.stats.pes_errors += 1;
                if let Some(s) = self.streams.get_mut(&pid) {
                    s.discontinuity = true;
                }
            }
        }
        // 確保済みの領域を使い回す
        pes.clear();
        if let Some(s) = self.streams.get_mut(&pid)
            && !s.pes_active
            && s.pes.is_empty()
        {
            s.pes = pes;
        }
    }

    /// PES のヘッダーを読む。(PTS, DTS, ES の始まり, 終わり)。
    #[allow(clippy::type_complexity)]
    fn parse_pes(&mut self, b: &[u8]) -> Option<(Option<i64>, Option<i64>, usize, usize)> {
        if b.len() < 9 || b[0] != 0 || b[1] != 0 || b[2] != 1 {
            return None;
        }
        let sid = b[3];
        let plen = u16::from_be_bytes([b[4], b[5]]) as usize;
        let end = if plen == 0 {
            b.len()
        } else {
            if b.len() < plen + 6 {
                return None;
            }
            plen + 6
        };
        if matches!(sid, 0xBC | 0xBE | 0xBF | 0xF0 | 0xF1 | 0xFF | 0xF2 | 0xF8) {
            return Some((None, None, 6, end));
        }
        if b[6] >> 6 != 2 {
            return None;
        }
        let flags = b[7] >> 6;
        let hdl = b[8] as usize;
        let start = 9 + hdl;
        if start > end {
            return None;
        }
        let (pts_raw, dts_raw) = match flags {
            2 if hdl >= 5 => (Some(read_ts(&b[9..14])?), None),
            3 if hdl >= 10 => (Some(read_ts(&b[9..14])?), Some(read_ts(&b[14..19])?)),
            0 => (None, None),
            _ => return None,
        };
        let Some(pts_raw) = pts_raw else { return Some((None, None, start, end)) };
        // DTS が無いときは PTS と同じ（13818-1 2.4.3.7）
        let dts_raw = dts_raw.unwrap_or(pts_raw);
        let dts = self.ts_ext.extend(dts_raw);
        let mut d = (pts_raw.wrapping_sub(dts_raw) & MASK33) as i64;
        if d >= 1 << 32 {
            d -= 1 << 33;
        }
        Some((Some(dts + d), Some(dts), start, end))
    }

    fn on_es(&mut self, pid: u16, es: &[u8], pts: Option<i64>, dts: Option<i64>) {
        let Some(kind) = self.streams.get(&pid).map(|s| s.kind) else { return };
        match kind {
            EsKind::H264 | EsKind::H265 => self.on_video(pid, kind, es, pts, dts),
            EsKind::Aac => self.on_aac(pid, es, pts),
            EsKind::Opus { channel_code } => self.on_opus(pid, channel_code, es, pts),
        }
    }

    fn set_config(&mut self, pid: u16, codec: Codec, record: Vec<u8>, video: Option<VideoParams>, audio: Option<AudioParams>) {
        let Some(s) = self.streams.get_mut(&pid) else { return };
        if let Some(c) = &s.config
            && c.record == record
            && c.video == video
            && c.audio == audio
        {
            return;
        }
        let config = CodecConfig { codec, generation: s.next_generation, record: Bytes::from(record), video, audio };
        s.next_generation = s.next_generation.wrapping_add(1);
        s.config = Some(config.clone());
        let track = s.track;
        self.emit(DemuxEvent::Config { track, config });
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_au(&mut self, pid: u16, pts: i64, dts: i64, duration: u64, keyframe: bool, payload: Vec<u8>) {
        let Some(s) = self.streams.get_mut(&pid) else { return };
        let Some(cfg) = &s.config else {
            self.stats.dropped_before_config += 1;
            return;
        };
        let au = AccessUnit {
            track: s.track,
            codec: cfg.codec,
            epoch: 0,
            sequence: s.sequence,
            config_generation: cfg.generation,
            pts,
            dts,
            duration,
            timebase: Timebase::MPEG,
            keyframe,
            discontinuity: std::mem::take(&mut s.discontinuity),
            capture_time_ns: None,
            payload: Bytes::from(payload),
        };
        s.sequence += 1;
        self.emit(DemuxEvent::AccessUnit(au));
    }

    fn on_video(&mut self, pid: u16, kind: EsKind, es: &[u8], pts: Option<i64>, dts: Option<i64>) {
        let h265 = kind == EsKind::H265;
        let nals = codec::annexb_nals(es);
        let mut payload = Vec::with_capacity(es.len() + 16);
        let mut keyframe = false;
        let mut bad = false;
        {
            let Some(s) = self.streams.get_mut(&pid) else { return };
            let ps = &mut s.params;
            for nal in nals {
                if h265 {
                    if nal.len() < 2 {
                        bad = true;
                        continue;
                    }
                    let t = h265_nal_type(nal);
                    match t {
                        h265_nal::AUD => continue,
                        h265_nal::VPS => {
                            match codec::parse_h265_vps_id(nal) {
                                Some(id) => ParamSets::put(&mut ps.vps, id, nal, &mut ps.changed),
                                None => bad = true,
                            }
                            continue;
                        }
                        h265_nal::SPS => {
                            match codec::parse_h265_sps(nal) {
                                Some(sps) => ParamSets::put(&mut ps.sps, sps.id, nal, &mut ps.changed),
                                None => bad = true,
                            }
                            continue;
                        }
                        h265_nal::PPS => {
                            match codec::parse_h265_pps_id(nal) {
                                Some(id) => ParamSets::put(&mut ps.pps, id, nal, &mut ps.changed),
                                None => bad = true,
                            }
                            continue;
                        }
                        16..=21 => keyframe = true,
                        _ => {}
                    }
                } else {
                    match nal[0] & 0x1F {
                        h264_nal::AUD => continue,
                        h264_nal::SPS => {
                            match codec::parse_h264_sps(nal) {
                                Some(sps) => ParamSets::put(&mut ps.sps, sps.id, nal, &mut ps.changed),
                                None => bad = true,
                            }
                            continue;
                        }
                        h264_nal::PPS => {
                            match codec::parse_h264_pps_id(nal) {
                                Some(id) => ParamSets::put(&mut ps.pps, id, nal, &mut ps.changed),
                                None => bad = true,
                            }
                            continue;
                        }
                        h264_nal::IDR => keyframe = true,
                        _ => {}
                    }
                }
                payload.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                payload.extend_from_slice(nal);
            }
        }
        if bad {
            self.stats.es_errors += 1;
        }
        self.update_video_config(pid, h265);
        if payload.is_empty() {
            return;
        }
        let (Some(pts), Some(dts)) = (pts, dts) else {
            self.stats.dropped_no_pts += 1;
            return;
        };
        self.emit_au(pid, pts, dts, 0, keyframe, payload);
    }

    fn update_video_config(&mut self, pid: u16, h265: bool) {
        let Some(s) = self.streams.get_mut(&pid) else { return };
        if !s.params.changed {
            return;
        }
        let ps = &s.params;
        if ps.sps.is_empty() || ps.pps.is_empty() || (h265 && ps.vps.is_empty()) {
            return;
        }
        s.params.changed = false;
        let ps = &s.params;
        let sps: Vec<&[u8]> = ps.sps.values().map(|v| v.as_slice()).collect();
        let pps: Vec<&[u8]> = ps.pps.values().map(|v| v.as_slice()).collect();
        let built = if h265 {
            let vps: Vec<&[u8]> = ps.vps.values().map(|v| v.as_slice()).collect();
            let info: Option<H265Sps> = codec::parse_h265_sps(sps[0]);
            info.and_then(|i| {
                let rec = codec::build_hvcc(&vps, &sps, &pps, &i)?;
                let vp = VideoParams {
                    width: i.width,
                    height: i.height,
                    profile: i.ptl[0] & 0x1F,
                    compat: (i.ptl[0] >> 5) & 1,
                    level: i.ptl[11],
                };
                Some((Codec::H265, rec, vp))
            })
        } else {
            let info: Option<H264Sps> = codec::parse_h264_sps(sps[0]);
            info.and_then(|i| {
                let rec = codec::build_avcc(&sps, &pps, &i)?;
                let vp = VideoParams {
                    width: i.width,
                    height: i.height,
                    profile: i.profile_idc,
                    compat: i.constraint,
                    level: i.level_idc,
                };
                Some((Codec::H264, rec, vp))
            })
        };
        match built {
            Some((c, rec, vp)) => self.set_config(pid, c, rec, Some(vp), None),
            None => self.stats.es_errors += 1,
        }
    }

    fn on_aac(&mut self, pid: u16, es: &[u8], pts: Option<i64>) {
        let Some(s) = self.streams.get_mut(&pid) else { return };
        let carry_len = s.carry.len();
        let mut data = std::mem::take(&mut s.carry);
        data.extend_from_slice(es);
        let carry_pts = s.carry_pts.take();
        let mut next_pts = s.next_pts;
        let mut p = 0usize;
        let mut first_new = true;
        let mut frames: Vec<(i64, u32, AdtsInfo, std::ops::Range<usize>)> = Vec::new();
        let mut errors = 0u64;
        let mut new_carry: Option<(usize, Option<i64>)> = None;
        while p < data.len() {
            // このフレームの PTS（PES の PTS は、その PES で始まる最初のフレームのもの）
            let fpts = if p < carry_len {
                carry_pts
            } else if first_new {
                pts.or(next_pts)
            } else {
                next_pts
            };
            if data.len() - p < 7 {
                new_carry = Some((p, fpts));
                break;
            }
            let Some(h) = parse_adts(&data[p..]) else {
                errors += 1;
                // 次の同期語を探す
                p += 1;
                while p + 1 < data.len() && !(data[p] == 0xFF && data[p + 1] & 0xF6 == 0xF0) {
                    p += 1;
                }
                if p + 1 >= data.len() {
                    p = data.len();
                }
                continue;
            };
            if p + h.frame_len > data.len() {
                new_carry = Some((p, fpts));
                break;
            }
            if p >= carry_len {
                first_new = false;
            }
            let rate = AAC_SAMPLE_RATES[h.sf_index as usize];
            let dur = 1024i64.rescale(Timebase::new(1, rate), Timebase::MPEG).unwrap_or(0);
            next_pts = fpts.map(|t| t + dur);
            if h.raw_blocks != 0 || h.channel_config == 0 {
                errors += 1;
            } else if let Some(t) = fpts {
                let info = AdtsInfo { ot: h.object_type, sf: h.sf_index, ch: h.channel_config, rate };
                frames.push((t, dur as u32, info, p + h.header_len..p + h.frame_len));
            } else {
                self.stats.dropped_no_pts += 1;
            }
            p += h.frame_len;
        }
        let s = self.streams.get_mut(&pid).expect("stream");
        s.next_pts = next_pts;
        if let Some((at, cpts)) = new_carry {
            s.carry = data[at..].to_vec();
            s.carry_pts = cpts;
            if s.carry.len() > 8192 {
                s.carry.clear();
                s.carry_pts = None;
                errors += 1;
            }
        }
        self.stats.es_errors += errors;
        for (t, dur, info, range) in frames {
            let asc = build_asc(info.ot, info.sf, info.ch);
            let channels = match info.ch {
                7 => 8,
                c => c,
            };
            self.set_config(
                pid,
                Codec::Aac,
                asc.to_vec(),
                None,
                Some(AudioParams { sample_rate: info.rate, channels, object_type: info.ot }),
            );
            self.emit_au(pid, t, t, dur as u64, true, data[range].to_vec());
        }
    }

    fn on_opus(&mut self, pid: u16, channel_code: u8, es: &[u8], pts: Option<i64>) {
        let Some(head) = codec::opus_default_head(channel_code) else {
            self.stats.es_errors += 1;
            return;
        };
        let record = codec::build_opus_head(&head);
        self.set_config(
            pid,
            Codec::Opus,
            record,
            None,
            Some(AudioParams { sample_rate: 48000, channels: head.channels, object_type: 0 }),
        );
        let Some(s) = self.streams.get(&pid) else { return };
        let mut next_pts = pts.or(s.next_pts);
        let mut p = 0usize;
        let mut packets = Vec::new();
        let mut bad = false;
        while p < es.len() {
            // opus_control_header: 0x7FE0 の 11 bit + フラグ
            let Some(h) = es.get(p..p + 2) else {
                bad = true;
                break;
            };
            if h[0] != 0x7F || h[1] & 0xE0 != 0xE0 {
                bad = true;
                break;
            }
            let start_trim = h[1] & 0x10 != 0;
            let end_trim = h[1] & 0x08 != 0;
            let ext = h[1] & 0x04 != 0;
            p += 2;
            let mut size = 0usize;
            loop {
                let Some(&b) = es.get(p) else {
                    bad = true;
                    break;
                };
                p += 1;
                size += b as usize;
                if b != 0xFF {
                    break;
                }
            }
            if bad {
                break;
            }
            if start_trim {
                p += 2;
            }
            if end_trim {
                p += 2;
            }
            if ext {
                let Some(&l) = es.get(p) else {
                    bad = true;
                    break;
                };
                p += 1 + l as usize;
            }
            let Some(pkt) = es.get(p..p + size) else {
                bad = true;
                break;
            };
            p += size;
            let Some(samples) = opus_packet_samples(pkt) else {
                bad = true;
                continue;
            };
            let dur = (samples as i64).rescale(Timebase::new(1, 48000), Timebase::MPEG).unwrap_or(0);
            match next_pts {
                Some(t) => packets.push((t, dur as u64, pkt.to_vec())),
                None => self.stats.dropped_no_pts += 1,
            }
            next_pts = next_pts.map(|t| t + dur);
        }
        if bad {
            self.stats.es_errors += 1;
        }
        if let Some(s) = self.streams.get_mut(&pid) {
            s.next_pts = next_pts;
        }
        for (t, dur, pkt) in packets {
            self.emit_au(pid, t, t, dur, true, pkt);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct AdtsInfo {
    ot: u8,
    sf: u8,
    ch: u8,
    rate: u32,
}

fn take_sections(sb: &mut SectionBuf, out: &mut Vec<Vec<u8>>) {
    loop {
        if sb.buf.is_empty() {
            return;
        }
        if sb.buf[0] == 0xFF {
            // 詰め物。残りは捨てる
            sb.buf.clear();
            sb.active = false;
            return;
        }
        if sb.buf.len() < 3 {
            return;
        }
        let len = (((sb.buf[1] & 0x0F) as usize) << 8) | sb.buf[2] as usize;
        if len + 3 > MAX_SECTION {
            sb.buf.clear();
            sb.active = false;
            return;
        }
        if sb.buf.len() < len + 3 {
            return;
        }
        out.push(sb.buf[..len + 3].to_vec());
        sb.buf.drain(..len + 3);
    }
}
