//! メディアフラグメント（moof + mdat）。

use crate::bmff::{begin_box, begin_full_box, end_box, put_u32, put_u64};
use crate::error::Mp4Error;
use bytes::Bytes;
use senca_core::time::{Rescale, Timebase};
use senca_core::{AccessUnit, MediaKind};

/// 1 トラック・1 フラグメントあたりのサンプル数の上限。
pub(crate) const MAX_SAMPLES_PER_RUN: usize = 1 << 20;

/// sample_flags: 独立（sync）。
pub(crate) const FLAGS_SYNC: u32 = 0x0200_0000;
/// sample_flags: 他に依存する非 sync。
pub(crate) const FLAGS_NON_SYNC: u32 = 0x0101_0000;

/// トラックの時間軸で表したサンプル。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// DTS（トラックの timescale）。
    pub dts: i64,
    pub duration: u32,
    /// PTS − DTS。
    pub cts_offset: i32,
    pub keyframe: bool,
    pub data: Bytes,
}

/// 1 つのトラックの run。
#[derive(Debug, Clone, Copy)]
pub struct TrackRun<'a> {
    pub track_id: u32,
    /// DTS の昇順。先頭の DTS が tfdt になる。
    pub samples: &'a [Sample],
}

/// フラグメントを書く。mfhd の sequence_number を 1 から数える。
#[derive(Debug, Clone)]
pub struct FragmentWriter {
    sequence: u32,
}

impl Default for FragmentWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FragmentWriter {
    pub fn new() -> Self {
        FragmentWriter { sequence: 1 }
    }

    /// 次に使う sequence_number。
    pub fn sequence(&self) -> u32 {
        self.sequence
    }

    /// moof と mdat のヘッダーを返す。サンプルの中身は `runs` の順にこの後ろへ続ける（コピーを避けたいとき）。
    pub fn write_header(&mut self, runs: &[TrackRun<'_>]) -> Result<Vec<u8>, Mp4Error> {
        let mut total: u64 = 0;
        for (i, r) in runs.iter().enumerate() {
            if runs[..i].iter().any(|o| o.track_id == r.track_id) {
                return Err(Mp4Error::DuplicateTrack(r.track_id));
            }
            if r.samples.len() > MAX_SAMPLES_PER_RUN {
                return Err(Mp4Error::TooLarge("samples in a fragment"));
            }
            for s in r.samples {
                if s.data.len() > u32::MAX as usize {
                    return Err(Mp4Error::TooLarge("sample"));
                }
                total += s.data.len() as u64;
            }
            if let Some(f) = r.samples.first()
                && f.dts < 0
            {
                return Err(Mp4Error::Timestamp("negative decode time in fragment"));
            }
        }
        if total > u32::MAX as u64 - 16 {
            return Err(Mp4Error::TooLarge("fragment"));
        }
        let mut buf = Vec::with_capacity(64 + runs.iter().map(|r| 40 + r.samples.len() * 16).sum::<usize>());
        let moof = begin_box(&mut buf, b"moof");
        let p = begin_full_box(&mut buf, b"mfhd", 0, 0);
        put_u32(&mut buf, self.sequence);
        end_box(&mut buf, p);
        let mut offset_slots = Vec::new();
        for r in runs.iter().filter(|r| !r.samples.is_empty()) {
            let traf = begin_box(&mut buf, b"traf");
            // default-base-is-moof
            let p = begin_full_box(&mut buf, b"tfhd", 0, 0x02_0000);
            put_u32(&mut buf, r.track_id);
            end_box(&mut buf, p);
            let p = begin_full_box(&mut buf, b"tfdt", 1, 0);
            put_u64(&mut buf, r.samples[0].dts as u64);
            end_box(&mut buf, p);
            // data_offset, duration, size, flags, composition offset
            let p = begin_full_box(&mut buf, b"trun", 1, 0x000F01);
            put_u32(&mut buf, r.samples.len() as u32);
            offset_slots.push(buf.len());
            put_u32(&mut buf, 0);
            for s in r.samples {
                put_u32(&mut buf, s.duration);
                put_u32(&mut buf, s.data.len() as u32);
                put_u32(&mut buf, if s.keyframe { FLAGS_SYNC } else { FLAGS_NON_SYNC });
                put_u32(&mut buf, s.cts_offset as u32);
            }
            end_box(&mut buf, p);
            end_box(&mut buf, traf);
        }
        end_box(&mut buf, moof);
        let moof_len = buf.len() as u64;
        // data_offset は moof の先頭から
        let mut off = moof_len + 8;
        for (slot, r) in offset_slots.iter().zip(runs.iter().filter(|r| !r.samples.is_empty())) {
            let v = u32::try_from(off).map_err(|_| Mp4Error::TooLarge("fragment"))?;
            buf[*slot..*slot + 4].copy_from_slice(&v.to_be_bytes());
            off += r.samples.iter().map(|s| s.data.len() as u64).sum::<u64>();
        }
        put_u32(&mut buf, (total + 8) as u32);
        buf.extend_from_slice(b"mdat");
        self.sequence = self.sequence.wrapping_add(1);
        Ok(buf)
    }

    /// moof と mdat を `out` に足す。
    pub fn write(&mut self, runs: &[TrackRun<'_>], out: &mut Vec<u8>) -> Result<(), Mp4Error> {
        let hdr = self.write_header(runs)?;
        out.extend_from_slice(&hdr);
        for r in runs {
            for s in r.samples {
                out.extend_from_slice(&s.data);
            }
        }
        Ok(())
    }
}

/// AU をトラックの時間軸のサンプルに直す。長さは次の AU の DTS との差で決めるので、
/// 最後の 1 つは次の AU が来るか [`flush`](SampleQueue::flush) するまで残る。
#[derive(Debug, Clone)]
pub struct SampleQueue {
    timescale: u32,
    pending: Option<(Sample, u64)>,
    ready: Vec<Sample>,
    last_duration: u32,
}

impl SampleQueue {
    pub fn new(timescale: u32) -> Self {
        assert!(timescale != 0, "timescale must be non-zero");
        SampleQueue { timescale, pending: None, ready: Vec::new(), last_duration: 0 }
    }

    /// AU を入れる。DTS が逆行したら `Err`（入れない）。
    pub fn push(&mut self, au: &AccessUnit) -> Result<(), Mp4Error> {
        let tb = Timebase::new(1, self.timescale);
        let dts = au.dts.rescale(au.timebase, tb).ok_or(Mp4Error::Timestamp("overflow"))?;
        let pts = au.pts.rescale(au.timebase, tb).ok_or(Mp4Error::Timestamp("overflow"))?;
        let cts = i32::try_from(pts - dts).map_err(|_| Mp4Error::Timestamp("composition offset out of range"))?;
        let dur_hint = (au.duration as i64).rescale(au.timebase, tb).unwrap_or(0).clamp(0, u32::MAX as i64) as u64;
        if let Some((p, _)) = &self.pending {
            let d = dts - p.dts;
            if d < 0 {
                return Err(Mp4Error::Timestamp("decode time went backwards"));
            }
            let d = u32::try_from(d).map_err(|_| Mp4Error::Timestamp("sample duration out of range"))?;
            let (mut s, _) = self.pending.take().expect("pending");
            s.duration = d;
            self.last_duration = d;
            self.ready.push(s);
        }
        let keyframe = au.keyframe || au.codec.kind() == MediaKind::Audio;
        self.pending = Some((Sample { dts, duration: 0, cts_offset: cts, keyframe, data: au.payload.clone() }, dur_hint));
        Ok(())
    }

    /// 長さが決まったサンプル。
    pub fn ready(&self) -> &[Sample] {
        &self.ready
    }

    /// 長さが決まったサンプルを取り出す。
    pub fn take_ready(&mut self) -> Vec<Sample> {
        std::mem::take(&mut self.ready)
    }

    /// 残りの 1 つの長さを AU の duration（無ければ直前の長さ）で決めて出す。
    pub fn flush(&mut self) {
        if let Some((mut s, hint)) = self.pending.take() {
            s.duration = if hint > 0 { hint.min(u32::MAX as u64) as u32 } else { self.last_duration };
            self.ready.push(s);
        }
    }

    /// 次に出るサンプル（長さ未定）の DTS。
    pub fn pending_dts(&self) -> Option<i64> {
        self.pending.as_ref().map(|(s, _)| s.dts)
    }
}
