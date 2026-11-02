//! FLV タグ（RTMP の audio/video message）を `senca_core` の [`AccessUnit`]・[`CodecConfig`] にする。
//!
//! - 時間軸は 1/1000（RTMP・FLV のミリ秒）。32 bit の時刻はトラックごとに [`RolloverExtender`] で広げる。
//! - DTS = タグの時刻、PTS = DTS + composition time。
//! - 映像 payload は AVCC/HVCC の長さ前置 NAL 列のまま（タグ本体の `Bytes::slice`、コピーしない）。
//!   長さ前置が payload の終わりとちょうど合うことを確かめ、合わなければ `Err`。
//! - 設定（sequence header）が前回と違えば `generation` を 1 増やして [`IngestEvent::Config`] を出す。
//!   同じ内容の再送では出さない。最初の世代は 1。
//! - 設定が必要なコーデック（H.264・H.265・AV1・AAC）で設定より前に来たフレームは捨てて数える
//!   （[`FlvIngest::dropped_before_config`]）。VP8・VP9・Opus・G.711 は最初のフレームで空の設定を作る。
//!
//! 設定レコードの細かい解釈（SPS の解像度など）はここではしない。`VideoParams` は
//! profile・compat・level だけを入れ、width・height は 0（不明）のまま。統合側で埋める。

use crate::Error;
use crate::build::needs_config;
use crate::tag::{AudioPacket, TagType, VideoPacket, parse_audio, parse_video};
use bytes::Bytes;
use senca_core::bits::BitReader;
use senca_core::media::{AudioParams, VideoParams};
use senca_core::time::RolloverExtender;
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase, TrackId};

/// 取り込みの結果。1 つのタグから 0〜2 個出る（暗黙の設定 + AU）。
#[derive(Debug, Clone)]
pub enum IngestEvent {
    /// 新しい設定（世代が増えた）。以後の AU はこの世代を参照する。
    Config {
        track: TrackId,
        config: CodecConfig,
    },
    AccessUnit(AccessUnit),
    /// 送り手が SequenceEnd を送った。
    EndOfSequence {
        track: TrackId,
    },
}

/// 取り込みの設定。
#[derive(Debug, Clone, Copy)]
pub struct IngestOptions {
    pub video_track: TrackId,
    pub audio_track: TrackId,
    /// AU に入れる epoch。
    pub epoch: u64,
}

impl Default for IngestOptions {
    fn default() -> Self {
        IngestOptions {
            video_track: TrackId(0),
            audio_track: TrackId(1),
            epoch: 0,
        }
    }
}

#[derive(Debug)]
struct TrackState {
    config: Option<CodecConfig>,
    /// AVCC/HVCC の長さ前置のバイト数（H.264・H.265 のときだけ使う）。
    nal_length_size: usize,
    sequence: u64,
    clock: RolloverExtender,
}

impl TrackState {
    fn new() -> Self {
        TrackState {
            config: None,
            nal_length_size: 4,
            sequence: 0,
            clock: RolloverExtender::new(32),
        }
    }

    fn next_generation(&self) -> u32 {
        self.config
            .as_ref()
            .map_or(1, |c| c.generation.wrapping_add(1).max(1))
    }
}

/// FLV タグの取り込み器（映像 1 本・音声 1 本）。
#[derive(Debug)]
pub struct FlvIngest {
    opts: IngestOptions,
    video: TrackState,
    audio: TrackState,
    dropped: u64,
}

impl Default for FlvIngest {
    fn default() -> Self {
        Self::new(IngestOptions::default())
    }
}

impl FlvIngest {
    pub fn new(opts: IngestOptions) -> Self {
        FlvIngest {
            opts,
            video: TrackState::new(),
            audio: TrackState::new(),
            dropped: 0,
        }
    }

    /// 設定より前に来て捨てたフレームの数。
    pub fn dropped_before_config(&self) -> u64 {
        self.dropped
    }

    pub fn video_config(&self) -> Option<&CodecConfig> {
        self.video.config.as_ref()
    }

    pub fn audio_config(&self) -> Option<&CodecConfig> {
        self.audio.config.as_ref()
    }

    /// タグを 1 つ取り込み、結果を `out` に追記する。script タグは扱わない（何も足さない）。
    pub fn push(
        &mut self,
        tag_type: TagType,
        timestamp: u32,
        body: &Bytes,
        out: &mut Vec<IngestEvent>,
    ) -> Result<(), Error> {
        let ev = match tag_type {
            TagType::Video => self.video_tag(timestamp, body, out)?,
            TagType::Audio => self.audio_tag(timestamp, body, out)?,
            TagType::Script => None,
        };
        out.extend(ev);
        Ok(())
    }

    /// 映像タグ本体を取り込み、結果を `out` に追記する。
    pub fn push_video(
        &mut self,
        timestamp: u32,
        body: &Bytes,
        out: &mut Vec<IngestEvent>,
    ) -> Result<(), Error> {
        self.push(TagType::Video, timestamp, body, out)
    }

    /// 音声タグ本体を取り込み、結果を `out` に追記する。
    pub fn push_audio(
        &mut self,
        timestamp: u32,
        body: &Bytes,
        out: &mut Vec<IngestEvent>,
    ) -> Result<(), Error> {
        self.push(TagType::Audio, timestamp, body, out)
    }

    fn video_tag(
        &mut self,
        timestamp: u32,
        body: &Bytes,
        out: &mut Vec<IngestEvent>,
    ) -> Result<Option<IngestEvent>, Error> {
        let tag = parse_video(body)?;
        let track = self.opts.video_track;
        let Some(codec) = tag.codec else {
            // コマンドフレームは無視する。
            return Ok(None);
        };
        match tag.packet {
            VideoPacket::SequenceStart(record) => {
                let (video, nal_len) = parse_video_record(codec, &record)?;
                let st = &mut self.video;
                if let Some(c) = &st.config
                    && c.codec == codec
                    && c.record == record
                {
                    return Ok(None);
                }
                let config = CodecConfig {
                    codec,
                    generation: st.next_generation(),
                    record,
                    video,
                    audio: None,
                };
                st.nal_length_size = nal_len;
                st.config = Some(config.clone());
                Ok(Some(IngestEvent::Config { track, config }))
            }
            VideoPacket::CodedFrames {
                composition_time,
                data,
            } => {
                let st = &mut self.video;
                match &st.config {
                    Some(c) if c.codec != codec => return Err(Error::CodecChanged),
                    Some(_) => {}
                    None if needs_config(codec) => {
                        self.dropped += 1;
                        return Ok(None);
                    }
                    None => {
                        let c = CodecConfig {
                            codec,
                            generation: 1,
                            record: Bytes::new(),
                            video: None,
                            audio: None,
                        };
                        st.config = Some(c.clone());
                        out.push(IngestEvent::Config { track, config: c });
                    }
                }
                if matches!(codec, Codec::H264 | Codec::H265) {
                    check_length_prefixed(&data, st.nal_length_size)?;
                }
                let dts = st.clock.extend(timestamp as u64);
                let au = AccessUnit {
                    track,
                    codec,
                    epoch: self.opts.epoch,
                    sequence: st.sequence,
                    config_generation: st.config.as_ref().map_or(0, |c| c.generation),
                    pts: dts + composition_time as i64,
                    dts,
                    duration: 0,
                    timebase: Timebase::MILLIS,
                    keyframe: tag.frame_type == crate::tag::VideoFrameType::Key,
                    discontinuity: false,
                    capture_time_ns: None,
                    payload: data,
                };
                st.sequence += 1;
                Ok(Some(IngestEvent::AccessUnit(au)))
            }
            VideoPacket::SequenceEnd => Ok(Some(IngestEvent::EndOfSequence { track })),
            VideoPacket::Metadata(_)
            | VideoPacket::Mpeg2TsSequenceStart(_)
            | VideoPacket::Command(_) => Ok(None),
        }
    }

    fn audio_tag(
        &mut self,
        timestamp: u32,
        body: &Bytes,
        out: &mut Vec<IngestEvent>,
    ) -> Result<Option<IngestEvent>, Error> {
        let tag = parse_audio(body)?;
        let track = self.opts.audio_track;
        let codec = tag.codec;
        match tag.packet {
            AudioPacket::SequenceStart(record) => {
                let audio = parse_audio_record(codec, &record)?;
                let st = &mut self.audio;
                if let Some(c) = &st.config
                    && c.codec == codec
                    && c.record == record
                {
                    return Ok(None);
                }
                let config = CodecConfig {
                    codec,
                    generation: st.next_generation(),
                    record,
                    video: None,
                    audio,
                };
                st.config = Some(config.clone());
                Ok(Some(IngestEvent::Config { track, config }))
            }
            AudioPacket::CodedFrames(data) => {
                let st = &mut self.audio;
                match &st.config {
                    Some(c) if c.codec != codec => return Err(Error::CodecChanged),
                    Some(_) => {}
                    None if needs_config(codec) => {
                        self.dropped += 1;
                        return Ok(None);
                    }
                    None => {
                        let channels = match (codec, tag.legacy) {
                            (_, Some(l)) => 1 + l.stereo as u8,
                            _ => 2,
                        };
                        let sample_rate = if codec == Codec::Opus { 48_000 } else { 8_000 };
                        let c = CodecConfig {
                            codec,
                            generation: 1,
                            record: Bytes::new(),
                            video: None,
                            audio: Some(AudioParams {
                                sample_rate,
                                channels,
                                object_type: 0,
                            }),
                        };
                        st.config = Some(c.clone());
                        out.push(IngestEvent::Config { track, config: c });
                    }
                }
                let dts = st.clock.extend(timestamp as u64);
                let au = AccessUnit {
                    track,
                    codec,
                    epoch: self.opts.epoch,
                    sequence: st.sequence,
                    config_generation: st.config.as_ref().map_or(0, |c| c.generation),
                    pts: dts,
                    dts,
                    duration: 0,
                    timebase: Timebase::MILLIS,
                    keyframe: true,
                    discontinuity: false,
                    capture_time_ns: None,
                    payload: data,
                };
                st.sequence += 1;
                Ok(Some(IngestEvent::AccessUnit(au)))
            }
            AudioPacket::SequenceEnd => Ok(Some(IngestEvent::EndOfSequence { track })),
            AudioPacket::MultichannelConfig(_) => Ok(None),
        }
    }
}

/// 長さ前置の NAL 列が payload の終わりとちょうど合うか。
fn check_length_prefixed(data: &[u8], len_size: usize) -> Result<(), Error> {
    let mut pos = 0usize;
    while pos < data.len() {
        let hdr = data
            .get(pos..pos + len_size)
            .ok_or(Error::Malformed("truncated NAL length"))?;
        let n = hdr.iter().fold(0usize, |a, &b| (a << 8) | b as usize);
        pos = pos
            .checked_add(len_size)
            .and_then(|p| p.checked_add(n))
            .ok_or(Error::Malformed("NAL length overflow"))?;
        if pos > data.len() {
            return Err(Error::Malformed("NAL length exceeds payload"));
        }
    }
    Ok(())
}

/// 映像の設定レコードから簡単に取れる値（profile 等）と NAL 長さ前置のバイト数。
fn parse_video_record(codec: Codec, r: &[u8]) -> Result<(Option<VideoParams>, usize), Error> {
    match codec {
        Codec::H264 => {
            // configurationVersion, AVCProfileIndication, profile_compatibility, AVCLevelIndication,
            // reserved(6) lengthSizeMinusOne(2), ...
            if r.len() < 7 || r[0] != 1 {
                return Err(Error::Malformed("invalid AVCDecoderConfigurationRecord"));
            }
            let len = (r[4] & 0x03) as usize + 1;
            if len == 3 {
                return Err(Error::Malformed("invalid NAL length size"));
            }
            Ok((
                Some(VideoParams {
                    width: 0,
                    height: 0,
                    profile: r[1],
                    compat: r[2],
                    level: r[3],
                }),
                len,
            ))
        }
        Codec::H265 => {
            if r.len() < 23 || r[0] != 1 {
                return Err(Error::Malformed("invalid HEVCDecoderConfigurationRecord"));
            }
            let len = (r[21] & 0x03) as usize + 1;
            if len == 3 {
                return Err(Error::Malformed("invalid NAL length size"));
            }
            Ok((
                Some(VideoParams {
                    width: 0,
                    height: 0,
                    profile: r[1] & 0x1F,
                    compat: 0,
                    level: r[12],
                }),
                len,
            ))
        }
        Codec::Av1 => {
            // marker(1) version(7) = 0x81, seq_profile(3) seq_level_idx_0(5), ...
            if r.len() < 4 || r[0] != 0x81 {
                return Err(Error::Malformed("invalid AV1CodecConfigurationRecord"));
            }
            Ok((
                Some(VideoParams {
                    width: 0,
                    height: 0,
                    profile: r[1] >> 5,
                    compat: 0,
                    level: r[1] & 0x1F,
                }),
                4,
            ))
        }
        _ => Ok((None, 4)),
    }
}

/// 音声の設定レコードからサンプリング周波数・チャンネル数を取る。
fn parse_audio_record(codec: Codec, r: &[u8]) -> Result<Option<AudioParams>, Error> {
    match codec {
        Codec::Aac => parse_audio_specific_config(r).map(Some),
        Codec::Opus => {
            if r.is_empty() {
                return Ok(None);
            }
            if r.len() < 19 || &r[..8] != b"OpusHead" {
                return Err(Error::Malformed("invalid OpusHead"));
            }
            Ok(Some(AudioParams {
                sample_rate: 48_000,
                channels: r[9],
                object_type: 0,
            }))
        }
        _ => Ok(None),
    }
}

/// AudioSpecificConfig（ISO/IEC 14496-3 1.6.2.1）の先頭だけを読む。
pub fn parse_audio_specific_config(r: &[u8]) -> Result<AudioParams, Error> {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    let bad = Error::Malformed("invalid AudioSpecificConfig");
    let mut br = BitReader::new(r);
    let mut ot = br.read_u8(5).ok_or(bad.clone())?;
    if ot == 31 {
        ot = 32 + br.read_u8(6).ok_or(bad.clone())?;
    }
    let idx = br.read_u8(4).ok_or(bad.clone())?;
    let sample_rate = if idx == 15 {
        br.read_u32(24).ok_or(bad.clone())?
    } else {
        *RATES.get(idx as usize).ok_or(bad.clone())?
    };
    let ch = br.read_u8(4).ok_or(bad.clone())?;
    let channels = match ch {
        1..=6 => ch,
        7 => 8,
        // 0 は PCE で決まる。ここでは不明として 0。
        _ => 0,
    };
    if ot == 0 || sample_rate == 0 {
        return Err(bad);
    }
    Ok(AudioParams {
        sample_rate,
        channels,
        object_type: ot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build;

    fn one_v(ing: &mut FlvIngest, ts: u32, b: &Bytes) -> Result<Option<IngestEvent>, Error> {
        let mut out = Vec::new();
        ing.push_video(ts, b, &mut out)?;
        Ok(out.pop())
    }

    fn one_a(ing: &mut FlvIngest, ts: u32, b: &Bytes) -> Result<Option<IngestEvent>, Error> {
        let mut out = Vec::new();
        ing.push_audio(ts, b, &mut out)?;
        Ok(out.pop())
    }

    const AVCC: &[u8] = &[1, 0x64, 0x00, 0x1F, 0xFF, 0xE1, 0x00, 0x00, 0x01, 0x00];

    #[test]
    fn h264_flow() {
        let mut ing = FlvIngest::default();
        // 設定前のフレームは捨てる
        let f = build::video_frame(
            Codec::H264,
            true,
            0,
            Bytes::from_static(&[0, 0, 0, 1, 0x65]),
        )
        .unwrap()
        .to_bytes();
        assert!(one_v(&mut ing, 0, &f).unwrap().is_none());
        assert_eq!(ing.dropped_before_config(), 1);

        let seq = build::video_sequence_start(Codec::H264, Bytes::from_static(AVCC))
            .unwrap()
            .to_bytes();
        let Some(IngestEvent::Config { config, .. }) = one_v(&mut ing, 0, &seq).unwrap() else {
            panic!()
        };
        assert_eq!(config.generation, 1);
        assert_eq!(config.video.unwrap().profile, 0x64);
        // 同じ設定の再送では何も出ない
        assert!(one_v(&mut ing, 0, &seq).unwrap().is_none());

        let f = build::video_frame(
            Codec::H264,
            true,
            66,
            Bytes::from_static(&[0, 0, 0, 1, 0x65]),
        )
        .unwrap()
        .to_bytes();
        let Some(IngestEvent::AccessUnit(au)) = one_v(&mut ing, 100, &f).unwrap() else {
            panic!()
        };
        assert_eq!((au.dts, au.pts), (100, 166));
        assert!(au.keyframe);
        assert_eq!(au.config_generation, 1);
        assert_eq!(au.timebase, Timebase::MILLIS);
        assert_eq!(&au.payload[..], &[0, 0, 0, 1, 0x65]);
        // ゼロコピー
        assert_eq!(au.payload.as_ptr(), f[5..].as_ptr());

        // 設定が変わると世代が増える
        let seq2 = build::video_sequence_start(
            Codec::H264,
            Bytes::from_static(&[1, 0x4D, 0x00, 0x1E, 0xFF, 0xE1, 0, 0, 1, 0]),
        )
        .unwrap()
        .to_bytes();
        let Some(IngestEvent::Config { config, .. }) = one_v(&mut ing, 200, &seq2).unwrap() else {
            panic!()
        };
        assert_eq!(config.generation, 2);

        // 長さ前置が合わない
        let bad = build::video_frame(Codec::H264, false, 0, Bytes::from_static(&[0, 0, 0, 9, 1]))
            .unwrap()
            .to_bytes();
        assert!(one_v(&mut ing, 300, &bad).is_err());
    }

    #[test]
    fn rollover() {
        let mut ing = FlvIngest::default();
        let asc = Bytes::from_static(&[0x12, 0x10]);
        let seq = build::audio_sequence_start(&CodecConfig {
            codec: Codec::Aac,
            generation: 1,
            record: asc,
            video: None,
            audio: None,
        })
        .unwrap()
        .unwrap()
        .to_bytes();
        let Some(IngestEvent::Config { config, .. }) = one_a(&mut ing, 0, &seq).unwrap() else {
            panic!()
        };
        assert_eq!(
            config.audio,
            Some(AudioParams {
                sample_rate: 44100,
                channels: 2,
                object_type: 2
            })
        );
        let f = build::audio_frame(Codec::Aac, None, Bytes::from_static(&[1]))
            .unwrap()
            .to_bytes();
        let mut last = 0;
        for ts in [u32::MAX - 10, 5u32] {
            let Some(IngestEvent::AccessUnit(au)) = one_a(&mut ing, ts, &f).unwrap() else {
                panic!()
            };
            last = au.dts;
        }
        assert_eq!(last, (1i64 << 32) + 5);
    }

    #[test]
    fn asc_parsing() {
        assert!(parse_audio_specific_config(&[]).is_err());
        assert!(parse_audio_specific_config(&[0x17, 0x80]).is_err()); // idx 15 で 24 bit 足りない
        let p = parse_audio_specific_config(&[0x11, 0x90]).unwrap();
        assert_eq!((p.sample_rate, p.channels), (48000, 2));
    }

    #[test]
    fn implicit_config_for_vp9() {
        let mut ing = FlvIngest::default();
        let f = build::video_frame(Codec::Vp9, true, 0, Bytes::from_static(&[1, 2]))
            .unwrap()
            .to_bytes();
        let mut out = Vec::new();
        ing.push_video(0, &f, &mut out).unwrap();
        assert!(
            matches!(&out[0], IngestEvent::Config { config, .. } if config.codec == Codec::Vp9)
        );
        let IngestEvent::AccessUnit(au) = &out[1] else {
            panic!()
        };
        assert_eq!(au.codec, Codec::Vp9);
        assert_eq!(au.config_generation, 1);
        assert_eq!(ing.video_config().unwrap().codec, Codec::Vp9);
    }
}
