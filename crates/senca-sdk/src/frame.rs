//! 入力フレームを共通メディアモデル（ADR-0003 の内部表現）にそろえる。
//!
//! - 映像: Annex B（開始符号）または 4 バイト長さ前置（AVCC/HVCC）を受け、長さ前置 4 バイトの NAL 列にする。
//!   AU の中のパラメーターセット（H.264: SPS/PPS、H.265: VPS/SPS/PPS）は取り出して設定にし、内容が
//!   変わったときだけ設定の世代を 1 増やす。AUD は捨てる。
//! - 音声: AAC は raw_data_block（設定は AudioSpecificConfig）または ADTS（設定はヘッダーから作る）。
//!   Opus はパケット 1 つ（設定は OpusHead、無ければ作る）。
//! - 時刻はマイクロ秒（呼び出し側の時計、単調増加であること）。

use crate::Error;
use bytes::Bytes;
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase, TrackId};

/// 映像トラックの番号（TS・FLV とも 0 番）。
pub const VIDEO_TRACK: TrackId = TrackId(0);
pub const AUDIO_TRACK: TrackId = TrackId(1);
/// 1 フレームの上限（仕様 5.3 の最大 AU）。
pub const MAX_FRAME_BYTES: usize = 16 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoFormat {
    /// 開始符号（00 00 01 / 00 00 00 01）区切り。
    AnnexB,
    /// 4 バイト長さ前置（AVCC / HVCC）。
    LengthPrefixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Aac,
    Opus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    /// AAC の raw_data_block、または Opus のパケット。
    Raw,
    /// AAC の ADTS（1 つ以上のフレーム）。
    Adts,
}

/// 映像フレーム（1 アクセスユニット）。
#[derive(Debug, Clone, Copy)]
pub struct VideoFrame<'a> {
    pub codec: VideoCodec,
    pub format: VideoFormat,
    pub data: &'a [u8],
    pub pts_us: i64,
    pub dts_us: i64,
    /// None なら IDR / IRAP の有無で決める。
    pub keyframe: Option<bool>,
}

/// 音声フレーム。
#[derive(Debug, Clone, Copy)]
pub struct AudioFrame<'a> {
    pub codec: AudioCodec,
    pub format: AudioFormat,
    pub data: &'a [u8],
    pub pts_us: i64,
}

/// 音声の設定（raw の AAC と Opus で使う）。
#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u8,
    /// AAC: AudioSpecificConfig（無ければ AAC-LC として作る）。Opus: OpusHead（無ければ作る）。
    pub extra: Vec<u8>,
}

/// 送信キューに入れるもの。
#[derive(Debug, Clone)]
pub enum Item {
    Config(CodecConfig),
    Au(AccessUnit),
}

#[derive(Debug, Default)]
struct VideoState {
    codec: Option<VideoCodec>,
    /// 今の設定のパラメーターセット（VPS, SPS, PPS、NAL ヘッダー付き）。
    params: [Vec<Bytes>; 3],
    generation: u32,
    seq: u64,
}

#[derive(Debug, Default)]
struct AudioState {
    codec: Option<AudioCodec>,
    record: Option<Bytes>,
    generation: u32,
    seq: u64,
    sample_rate: u32,
}

/// 入力の正規化の状態（映像 1 本・音声 1 本）。
#[derive(Debug, Default)]
pub struct Normalizer {
    video: VideoState,
    audio: AudioState,
}

fn video_codec(c: VideoCodec) -> Codec {
    match c {
        VideoCodec::H264 => Codec::H264,
        VideoCodec::H265 => Codec::H265,
    }
}

/// NAL の分類。
#[derive(Clone, Copy, PartialEq, Eq)]
enum NalClass {
    Vps,
    Sps,
    Pps,
    Aud,
    Key,
    Other,
}

fn classify(codec: VideoCodec, nal: &[u8]) -> NalClass {
    match codec {
        VideoCodec::H264 => match nal[0] & 0x1F {
            7 => NalClass::Sps,
            8 => NalClass::Pps,
            9 => NalClass::Aud,
            5 => NalClass::Key,
            _ => NalClass::Other,
        },
        VideoCodec::H265 => match (nal[0] >> 1) & 0x3F {
            32 => NalClass::Vps,
            33 => NalClass::Sps,
            34 => NalClass::Pps,
            35 => NalClass::Aud,
            16..=21 => NalClass::Key,
            _ => NalClass::Other,
        },
    }
}

fn split_nals(format: VideoFormat, data: &[u8]) -> Result<Vec<&[u8]>, Error> {
    let nals: Vec<&[u8]> = match format {
        VideoFormat::AnnexB => senca_h264::annexb_nals(data).collect(),
        VideoFormat::LengthPrefixed => senca_h264::length_prefixed_nals(data, 4)
            .map_err(|_| Error::BadFrame("length-prefixed NAL units do not match the data"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| Error::BadFrame("length-prefixed NAL units do not match the data"))?,
    };
    Ok(nals.into_iter().filter(|n| !n.is_empty()).collect())
}

impl Normalizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 映像の設定レコード（AVCDecoderConfigurationRecord / HEVCDecoderConfigurationRecord）を入れる。
    pub fn set_video_config(
        &mut self,
        codec: VideoCodec,
        record: &[u8],
        out: &mut Vec<Item>,
    ) -> Result<(), Error> {
        fn bad<E>(_: E) -> Error {
            Error::BadFrame("invalid decoder configuration record")
        }
        let params: [Vec<Bytes>; 3] = match codec {
            VideoCodec::H264 => {
                let r = senca_h264::AvcDecoderConfigurationRecord::parse(record).map_err(bad)?;
                [Vec::new(), r.sps, r.pps]
            }
            VideoCodec::H265 => {
                let r = senca_h265::HevcDecoderConfigurationRecord::parse(record).map_err(bad)?;
                let pick = |t| r.nalus(t).cloned().collect::<Vec<_>>();
                [
                    pick(senca_h265::NalUnitType::Vps),
                    pick(senca_h265::NalUnitType::Sps),
                    pick(senca_h265::NalUnitType::Pps),
                ]
            }
        };
        self.update_video_params(codec, params, out)
    }

    fn update_video_params(
        &mut self,
        codec: VideoCodec,
        params: [Vec<Bytes>; 3],
        out: &mut Vec<Item>,
    ) -> Result<(), Error> {
        let v = &mut self.video;
        if v.codec == Some(codec) && v.params == params {
            return Ok(());
        }
        if v.codec.is_some_and(|c| c != codec) {
            return Err(Error::BadFrame("video codec changed"));
        }
        let gen_ = v.generation.wrapping_add(1).max(1);
        let refs = |i: usize| params[i].iter().map(|b| &b[..]).collect::<Vec<&[u8]>>();
        fn bad<E>(_: E) -> Error {
            Error::BadFrame("invalid parameter sets")
        }
        let cfg = match codec {
            VideoCodec::H264 => {
                senca_h264::AvcDecoderConfigurationRecord::from_parameter_sets(&refs(1), &refs(2))
                    .and_then(|r| r.codec_config(gen_))
                    .map_err(bad)?
            }
            VideoCodec::H265 => senca_h265::HevcDecoderConfigurationRecord::from_parameter_sets(
                &refs(0),
                &refs(1),
                &refs(2),
                &[],
            )
            .and_then(|r| r.codec_config(gen_))
            .map_err(bad)?,
        };
        v.codec = Some(codec);
        v.params = params;
        v.generation = gen_;
        out.push(Item::Config(cfg));
        Ok(())
    }

    /// 映像フレームを入れる。設定が無いうちの AU は捨てる（`Ok(false)`）。
    pub fn push_video(&mut self, f: &VideoFrame<'_>, out: &mut Vec<Item>) -> Result<bool, Error> {
        if f.data.is_empty() || f.data.len() > MAX_FRAME_BYTES {
            return Err(Error::BadFrame("frame size out of range"));
        }
        let nals = split_nals(f.format, f.data)?;
        let mut found: [Vec<Bytes>; 3] = Default::default();
        let mut payload = Vec::with_capacity(f.data.len() + 16);
        let mut has_key = false;
        let mut has_vcl = false;
        for nal in nals {
            let c = classify(f.codec, nal);
            match c {
                NalClass::Vps => found[0].push(Bytes::copy_from_slice(nal)),
                NalClass::Sps => found[1].push(Bytes::copy_from_slice(nal)),
                NalClass::Pps => found[2].push(Bytes::copy_from_slice(nal)),
                NalClass::Aud => {}
                NalClass::Key | NalClass::Other => {
                    has_key |= c == NalClass::Key;
                    has_vcl = true;
                    payload.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    payload.extend_from_slice(nal);
                }
            }
        }
        let need_vps = f.codec == VideoCodec::H265;
        if !found[1].is_empty() && !found[2].is_empty() && (!need_vps || !found[0].is_empty()) {
            self.update_video_params(f.codec, found, out)?;
        }
        if !has_vcl {
            return Ok(true);
        }
        let v = &mut self.video;
        if v.codec != Some(f.codec) || v.generation == 0 {
            return Ok(false);
        }
        v.seq += 1;
        out.push(Item::Au(AccessUnit {
            track: VIDEO_TRACK,
            codec: video_codec(f.codec),
            epoch: 0,
            sequence: v.seq,
            config_generation: v.generation,
            pts: f.pts_us,
            dts: f.dts_us,
            duration: 0,
            timebase: Timebase::MICROS,
            keyframe: f.keyframe.unwrap_or(has_key),
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from(payload),
        }));
        Ok(true)
    }

    /// 音声の設定を入れる。
    pub fn set_audio_config(&mut self, c: &AudioConfig, out: &mut Vec<Item>) -> Result<(), Error> {
        let cfg = match c.codec {
            AudioCodec::Aac => {
                let asc = if c.extra.is_empty() {
                    senca_aac::AudioSpecificConfig::new(2, c.sample_rate, c.channels)
                        .and_then(|a| a.to_bytes())
                        .map_err(|_| {
                            Error::InvalidArgument("unsupported AAC sample rate or channels")
                        })?
                } else {
                    c.extra.clone()
                };
                senca_aac::codec_config(&asc, 0)
                    .map_err(|_| Error::InvalidArgument("invalid AudioSpecificConfig"))?
            }
            AudioCodec::Opus => {
                let head = if c.extra.is_empty() {
                    // 符号化器の既定の先行サンプル（libopus の 48 kHz で 312）
                    senca_opus::OpusHead::new(c.channels, 312, c.sample_rate)
                        .map_err(|_| Error::InvalidArgument("Opus channels must be 1 or 2"))?
                } else {
                    senca_opus::OpusHead::parse(&c.extra)
                        .map_err(|_| Error::InvalidArgument("invalid OpusHead"))?
                };
                head.codec_config(0)
                    .map_err(|_| Error::InvalidArgument("invalid OpusHead"))?
            }
        };
        self.update_audio(c.codec, cfg, out)
    }

    fn update_audio(
        &mut self,
        codec: AudioCodec,
        mut cfg: CodecConfig,
        out: &mut Vec<Item>,
    ) -> Result<(), Error> {
        let a = &mut self.audio;
        if a.codec == Some(codec) && a.record.as_ref() == Some(&cfg.record) {
            return Ok(());
        }
        if a.codec.is_some_and(|c| c != codec) {
            return Err(Error::BadFrame("audio codec changed"));
        }
        a.generation = a.generation.wrapping_add(1).max(1);
        cfg.generation = a.generation;
        a.codec = Some(codec);
        a.record = Some(cfg.record.clone());
        a.sample_rate = cfg.audio.map_or(48_000, |p| p.sample_rate).max(1);
        out.push(Item::Config(cfg));
        Ok(())
    }

    fn audio_au(&mut self, codec: Codec, pts: i64, payload: Bytes, out: &mut Vec<Item>) {
        let a = &mut self.audio;
        a.seq += 1;
        out.push(Item::Au(AccessUnit {
            track: AUDIO_TRACK,
            codec,
            epoch: 0,
            sequence: a.seq,
            config_generation: a.generation,
            pts,
            dts: pts,
            duration: 0,
            timebase: Timebase::MICROS,
            keyframe: true,
            discontinuity: false,
            capture_time_ns: None,
            payload,
        }));
    }

    /// 音声フレームを入れる。設定が無いうちのフレームは捨てる（`Ok(false)`）。
    pub fn push_audio(&mut self, f: &AudioFrame<'_>, out: &mut Vec<Item>) -> Result<bool, Error> {
        if f.data.is_empty() || f.data.len() > 1 << 20 {
            return Err(Error::BadFrame("frame size out of range"));
        }
        match (f.codec, f.format) {
            (AudioCodec::Aac, AudioFormat::Adts) => {
                let data = Bytes::copy_from_slice(f.data);
                let frames =
                    senca_aac::split_adts(&data).map_err(|_| Error::BadFrame("invalid ADTS"))?;
                let mut pts = f.pts_us;
                for fr in frames {
                    let asc = fr
                        .header
                        .audio_specific_config()
                        .map_err(|_| Error::BadFrame("invalid ADTS header"))?;
                    let rec = asc
                        .to_bytes()
                        .map_err(|_| Error::BadFrame("invalid ADTS header"))?;
                    let cfg = senca_aac::codec_config(&rec, 0)
                        .map_err(|_| Error::BadFrame("invalid ADTS header"))?;
                    self.update_audio(AudioCodec::Aac, cfg, out)?;
                    self.audio_au(Codec::Aac, pts, fr.payload.clone(), out);
                    pts += 1024 * 1_000_000 / self.audio.sample_rate as i64;
                }
                Ok(true)
            }
            (AudioCodec::Aac, AudioFormat::Raw) | (AudioCodec::Opus, AudioFormat::Raw) => {
                let codec = if f.codec == AudioCodec::Aac {
                    Codec::Aac
                } else {
                    Codec::Opus
                };
                if self.audio.codec != Some(f.codec) {
                    return Ok(false);
                }
                self.audio_au(codec, f.pts_us, Bytes::copy_from_slice(f.data), out);
                Ok(true)
            }
            (AudioCodec::Opus, AudioFormat::Adts) => {
                Err(Error::InvalidArgument("ADTS is only for AAC"))
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 試験用の H.264 SPS（Baseline、320x240）と PPS。
    pub(crate) const SPS: &[u8] = &[
        0x67, 0x42, 0xC0, 0x0D, 0xD9, 0x01, 0x41, 0xFB, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10,
        0x00, 0x00, 0x03, 0x03, 0xC0, 0xF1, 0x42, 0xA4, 0x80,
    ];
    pub(crate) const PPS: &[u8] = &[0x68, 0xCB, 0x83, 0xCB, 0x20];

    pub(crate) fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&[0, 0, 0, 1]);
            v.extend_from_slice(n);
        }
        v
    }

    fn vf(data: &[u8], format: VideoFormat) -> VideoFrame<'_> {
        VideoFrame {
            codec: VideoCodec::H264,
            format,
            data,
            pts_us: 0,
            dts_us: 0,
            keyframe: None,
        }
    }

    #[test]
    fn annexb_extracts_config_once() {
        let mut n = Normalizer::new();
        let mut out = Vec::new();
        // 設定より前の AU は捨てる
        assert!(
            !n.push_video(
                &vf(&annexb(&[&[0x41, 1, 2]]), VideoFormat::AnnexB),
                &mut out
            )
            .unwrap()
        );
        assert!(out.is_empty());
        let idr = annexb(&[&[0x09, 0xF0], SPS, PPS, &[0x65, 0x88, 0x84]]);
        assert!(
            n.push_video(&vf(&idr, VideoFormat::AnnexB), &mut out)
                .unwrap()
        );
        assert_eq!(out.len(), 2);
        let Item::Config(c) = &out[0] else { panic!() };
        assert_eq!(
            (
                c.generation,
                c.video.unwrap().width,
                c.video.unwrap().height
            ),
            (1, 320, 240)
        );
        let Item::Au(au) = &out[1] else { panic!() };
        assert!(au.keyframe);
        assert_eq!(&au.payload[..], &[0, 0, 0, 3, 0x65, 0x88, 0x84]);
        // 同じパラメーターセットの再送では設定を出さない
        out.clear();
        n.push_video(&vf(&idr, VideoFormat::AnnexB), &mut out)
            .unwrap();
        assert_eq!(out.len(), 1);
        // 長さ前置の入力
        out.clear();
        n.push_video(
            &vf(&[0, 0, 0, 2, 0x41, 9], VideoFormat::LengthPrefixed),
            &mut out,
        )
        .unwrap();
        let Item::Au(au) = &out[0] else { panic!() };
        assert!(!au.keyframe);
        assert!(
            n.push_video(
                &vf(&[0, 0, 0, 9, 0x41], VideoFormat::LengthPrefixed),
                &mut out
            )
            .is_err()
        );
        // 設定レコードからも同じ設定になる（世代は増えない）
        let rec = senca_h264::AvcDecoderConfigurationRecord::from_parameter_sets(&[SPS], &[PPS])
            .unwrap()
            .to_bytes()
            .unwrap();
        out.clear();
        n.set_video_config(VideoCodec::H264, &rec, &mut out)
            .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn adts_and_raw_audio() {
        let mut n = Normalizer::new();
        let mut out = Vec::new();
        assert!(
            !n.push_audio(
                &AudioFrame {
                    codec: AudioCodec::Aac,
                    format: AudioFormat::Raw,
                    data: &[1, 2],
                    pts_us: 0
                },
                &mut out
            )
            .unwrap()
        );
        let asc = senca_aac::AudioSpecificConfig::new(2, 48000, 2).unwrap();
        let mut two = senca_aac::wrap_adts(&asc, &[1, 2, 3]).unwrap();
        two.extend(senca_aac::wrap_adts(&asc, &[4, 5]).unwrap());
        n.push_audio(
            &AudioFrame {
                codec: AudioCodec::Aac,
                format: AudioFormat::Adts,
                data: &two,
                pts_us: 1000,
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(out.len(), 3);
        let Item::Au(b) = &out[2] else { panic!() };
        assert_eq!((b.pts, &b.payload[..]), (1000 + 21333, &[4u8, 5][..]));
        // 同じ設定を raw 用に入れても世代は増えない
        out.clear();
        n.set_audio_config(
            &AudioConfig {
                codec: AudioCodec::Aac,
                sample_rate: 48000,
                channels: 2,
                extra: vec![],
            },
            &mut out,
        )
        .unwrap();
        assert!(out.is_empty());
        // Opus は設定が無ければ作る
        let mut o = Normalizer::new();
        o.set_audio_config(
            &AudioConfig {
                codec: AudioCodec::Opus,
                sample_rate: 48000,
                channels: 2,
                extra: vec![],
            },
            &mut out,
        )
        .unwrap();
        let Item::Config(c) = &out[0] else { panic!() };
        assert_eq!(&c.record[..8], b"OpusHead");
    }
}
