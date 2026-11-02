//! 相互接続試験の例（rtsp_pull・rtsp_serve・rtp_recv・rtp_send）で共有する部品。試験専用。
//!
//! - 出力: H.264/H.265 は Annex B（キーフレームの前に設定のパラメーターセット）、VP8/VP9/AV1 は IVF、
//!   音声は AU ごとの md5 を 1 行ずつ（FFmpeg の `-c copy -f framemd5` と比べる）。
//! - 入力: MPEG-TS（senca-mpegts）と IVF。

#![allow(dead_code)]

use bytes::Bytes;
use md5::{Digest, Md5};
use senca_core::media::{AccessUnit, Codec, CodecConfig, TrackId};
use senca_core::time::Timebase;
use senca_rtp::payload::PayloadFormat;
use senca_sdp::MediaDescription;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

pub fn md5_hex(b: &[u8]) -> String {
    Md5::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

/// m 行の最初の payload type の format（rtpmap・fmtp）。
pub fn media_format(m: &MediaDescription) -> Option<(u8, PayloadFormat)> {
    for pt in m.payload_types() {
        if let Some(rm) = m.rtpmap(pt) {
            let fmtp = m.fmtp(pt).map(|f| f.params).unwrap_or_default();
            return Some((
                pt,
                PayloadFormat {
                    encoding: rm.encoding,
                    clock_rate: rm.clock_rate,
                    channels: rm.channels,
                    fmtp,
                },
            ));
        }
    }
    None
}

/// AU の書き出し先。
pub enum Sink {
    AnnexB { codec: Codec, file: File },
    Ivf { file: File, frames: u32 },
    Md5 { file: File },
}

impl Sink {
    /// コーデックに合う書き出し先を `prefix.<拡張子>` に作る。戻り値の 2 番目はファイル名。
    pub fn create(codec: Codec, prefix: &str, clock_rate: u32) -> Res<(Sink, String)> {
        Ok(match codec {
            Codec::H264 | Codec::H265 => {
                let path = format!(
                    "{prefix}.{}",
                    if codec == Codec::H264 { "h264" } else { "h265" }
                );
                (
                    Sink::AnnexB {
                        codec,
                        file: File::create(&path)?,
                    },
                    path,
                )
            }
            Codec::Vp8 | Codec::Vp9 | Codec::Av1 => {
                let path = format!("{prefix}.ivf");
                let mut file = File::create(&path)?;
                let fourcc = match codec {
                    Codec::Vp8 => b"VP80",
                    Codec::Vp9 => b"VP90",
                    _ => b"AV01",
                };
                let mut h = Vec::with_capacity(32);
                h.extend_from_slice(b"DKIF");
                h.extend_from_slice(&0u16.to_le_bytes());
                h.extend_from_slice(&32u16.to_le_bytes());
                h.extend_from_slice(fourcc);
                h.extend_from_slice(&0u16.to_le_bytes());
                h.extend_from_slice(&0u16.to_le_bytes());
                h.extend_from_slice(&clock_rate.to_le_bytes());
                h.extend_from_slice(&1u32.to_le_bytes());
                h.extend_from_slice(&0u32.to_le_bytes());
                h.extend_from_slice(&0u32.to_le_bytes());
                file.write_all(&h)?;
                (Sink::Ivf { file, frames: 0 }, path)
            }
            _ => {
                let path = format!("{prefix}.md5");
                (
                    Sink::Md5 {
                        file: File::create(&path)?,
                    },
                    path,
                )
            }
        })
    }

    pub fn write(&mut self, au: &AccessUnit, cfg: Option<&CodecConfig>) -> Res<()> {
        match self {
            Sink::AnnexB { codec, file } => {
                let mut out = Vec::new();
                if au.keyframe
                    && let Some(c) = cfg
                {
                    for n in param_sets(*codec, c)? {
                        out.extend_from_slice(&[0, 0, 0, 1]);
                        out.extend_from_slice(&n);
                    }
                }
                let body = if *codec == Codec::H264 {
                    senca_h264::length_prefixed_to_annexb(&au.payload, 4)?
                } else {
                    senca_h265::length_prefixed_to_annexb(&au.payload, 4)?
                };
                out.extend_from_slice(&body);
                file.write_all(&out)?;
            }
            Sink::Ivf { file, frames } => {
                let mut data = au.payload.to_vec();
                if au.codec == Codec::Av1 {
                    // IVF の AV1 は TU ごとに Temporal Delimiter を置く
                    data.splice(0..0, [0x12u8, 0x00]);
                }
                file.write_all(&(data.len() as u32).to_le_bytes())?;
                file.write_all(&(au.pts as u64).to_le_bytes())?;
                file.write_all(&data)?;
                *frames += 1;
            }
            Sink::Md5 { file } => {
                writeln!(file, "{}", md5_hex(&au.payload))?;
            }
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Res<()> {
        if let Sink::Ivf { file, frames } = self {
            file.seek(SeekFrom::Start(24))?;
            file.write_all(&frames.to_le_bytes())?;
        }
        Ok(())
    }
}

/// 設定のパラメーターセット（H.264: SPS・PPS、H.265: VPS・SPS・PPS）。
pub fn param_sets(codec: Codec, c: &CodecConfig) -> Res<Vec<Bytes>> {
    Ok(match codec {
        Codec::H264 => {
            let r = senca_h264::AvcDecoderConfigurationRecord::parse(&c.record)?;
            r.sps.iter().chain(r.pps.iter()).cloned().collect()
        }
        Codec::H265 => {
            let r = senca_h265::HevcDecoderConfigurationRecord::parse(&c.record)?;
            let mut v = Vec::new();
            for ty in [32u8, 33, 34] {
                for a in &r.arrays {
                    if a.nal_unit_type.as_u8() == ty {
                        v.extend(a.nalus.iter().cloned());
                    }
                }
            }
            v
        }
        _ => Vec::new(),
    })
}

/// 入力ファイルのトラック 1 本。
pub struct SourceTrack {
    pub codec: Codec,
    pub config: Option<CodecConfig>,
    pub aus: Vec<AccessUnit>,
}

/// MPEG-TS を読む。
pub fn read_ts(path: &str) -> Res<Vec<SourceTrack>> {
    use senca_mpegts::{DemuxEvent, DemuxerConfig, TsDemuxer};
    let data = std::fs::read(path)?;
    let mut d = TsDemuxer::new(DemuxerConfig::default());
    let mut tracks: Vec<(TrackId, SourceTrack)> = Vec::new();
    let handle = |d: &mut TsDemuxer, tracks: &mut Vec<(TrackId, SourceTrack)>| {
        while let Some(ev) = d.poll() {
            match ev {
                DemuxEvent::StreamAdded { track, codec, .. } => tracks.push((
                    track,
                    SourceTrack {
                        codec,
                        config: None,
                        aus: Vec::new(),
                    },
                )),
                DemuxEvent::Config { track, config } => {
                    if let Some((_, t)) = tracks.iter_mut().find(|(id, _)| *id == track)
                        && t.config.is_none()
                    {
                        t.config = Some(config);
                    }
                }
                DemuxEvent::AccessUnit(au) => {
                    if let Some((_, t)) = tracks.iter_mut().find(|(id, _)| *id == au.track) {
                        t.aus.push(au);
                    }
                }
                DemuxEvent::Pcr { .. } => {}
            }
        }
    };
    for chunk in data.chunks(188 * 64) {
        d.push(chunk);
        handle(&mut d, &mut tracks);
    }
    d.flush();
    handle(&mut d, &mut tracks);
    Ok(tracks.into_iter().map(|(_, t)| t).collect())
}

/// IVF（VP8・VP9・AV1）を読む。
pub fn read_ivf(path: &str) -> Res<SourceTrack> {
    let data = std::fs::read(path)?;
    if data.len() < 32 || &data[..4] != b"DKIF" {
        return Err("IVF ではない".into());
    }
    let codec = match &data[8..12] {
        b"VP80" => Codec::Vp8,
        b"VP90" => Codec::Vp9,
        b"AV01" => Codec::Av1,
        _ => return Err("未対応の IVF fourcc".into()),
    };
    let den = u32::from_le_bytes(data[16..20].try_into()?);
    let num = u32::from_le_bytes(data[20..24].try_into()?);
    let hlen = u16::from_le_bytes(data[6..8].try_into()?) as usize;
    let tb = Timebase::new(num.max(1), den.max(1));
    let mut pos = hlen;
    let mut aus = Vec::new();
    while pos + 12 <= data.len() {
        let size = u32::from_le_bytes(data[pos..pos + 4].try_into()?) as usize;
        let pts = u64::from_le_bytes(data[pos + 4..pos + 12].try_into()?) as i64;
        pos += 12;
        let frame = Bytes::copy_from_slice(data.get(pos..pos + size).ok_or("IVF が途中で終わる")?);
        pos += size;
        let (payload, key) = match codec {
            Codec::Vp8 => (frame.clone(), senca_rtp::payload::vp8::is_keyframe(&frame)),
            Codec::Vp9 => (
                frame.clone(),
                senca_rtp::payload::vp9::parse_frame_header(&frame).is_some_and(|h| h.0),
            ),
            _ => {
                let tu = strip_td(&frame);
                let key = senca_rtp_av1_is_key(&tu);
                (tu, key)
            }
        };
        aus.push(AccessUnit {
            track: TrackId(0),
            codec,
            epoch: 0,
            sequence: aus.len() as u64,
            config_generation: 0,
            pts,
            dts: pts,
            duration: 0,
            timebase: tb,
            keyframe: key,
            discontinuity: false,
            capture_time_ns: None,
            payload,
        });
    }
    Ok(SourceTrack {
        codec,
        config: None,
        aus,
    })
}

/// 先頭の Temporal Delimiter（0x12 0x00）を除く。
fn strip_td(tu: &Bytes) -> Bytes {
    if tu.len() >= 2 && tu[0] == 0x12 && tu[1] == 0x00 {
        tu.slice(2..)
    } else {
        tu.clone()
    }
}

/// AV1 の TU がキーフレームか（シーケンスヘッダーを含む OBU_FRAME の先頭が KEY_FRAME）。簡易判定。
fn senca_rtp_av1_is_key(tu: &[u8]) -> bool {
    // OBU を順に見る（obu_size 付きを仮定）
    let mut pos = 0usize;
    let mut has_seq = false;
    while pos < tu.len() {
        let h = tu[pos];
        let ty = (h >> 3) & 0x0f;
        let ext = h & 0x04 != 0;
        let mut p = pos + 1 + ext as usize;
        let mut size = 0usize;
        let mut shift = 0;
        loop {
            let Some(&b) = tu.get(p) else { return false };
            p += 1;
            size |= ((b & 0x7f) as usize) << shift;
            shift += 7;
            if b & 0x80 == 0 || shift > 56 {
                break;
            }
        }
        if ty == 1 {
            has_seq = true;
        }
        if ty == 6 || ty == 3 {
            let Some(&b) = tu.get(p) else { return false };
            // show_existing_frame=0、frame_type=KEY
            return has_seq && b & 0x80 == 0 && (b >> 5) & 3 == 0;
        }
        pos = p + size;
    }
    false
}

/// 時刻（ns）。
pub fn au_time_ns(au: &AccessUnit) -> i64 {
    au.dts as i128 as i64 * au.timebase.num as i64 * 1_000_000_000 / au.timebase.den as i64
}
