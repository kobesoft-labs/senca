//! libopus（Opus の符号化・復号、ソフトウェア、BSD-3。ロイヤリティフリーとして提供、REQ-007）。
//!
//! - 出力 AU は Opus パケット 1 つ、時間軸は 1/48000（Opus の基準）。設定は OpusHead（RFC 7845）。
//! - 1〜2 チャンネル（チャンネル割り当て 0）だけを扱う。多チャンネル（multistream）は未対応。
//! - 復号では OpusHead の pre_skip の分を先頭から取り除く。

#[allow(unsafe_code)]
mod ffi;

use crate::bitstream::opus::{build_opus_head, parse_opus_head};
use crate::caps::{Capability, DeviceInfo, Direction, ProviderId, ProviderStatus};
use crate::config::{AudioEncoderConfig, DecoderConfig, EncoderConfig, OpusApplication};
use crate::error::{CodecError, Result};
use crate::frame::{AudioFrame, Frame, SampleFormat};
use crate::provider::{CodecProvider, Decoder, Encoder};
use crate::providers::common::{AuStamper, UnavailableProvider, software_device};
use crate::registry::ProbeOptions;
use bytes::Bytes;
use ffi::bindings as b;
use senca_core::media::AudioParams;
use senca_core::{AccessUnit, Codec, CodecConfig, Rescale, Timebase};
use std::sync::Arc;

/// Opus の基準の時間軸。
pub const OPUS_TIMEBASE: Timebase = Timebase::new(1, 48000);
/// 1 パケットの最大長（120 ms、48 kHz）。
const MAX_PACKET_SAMPLES_48K: usize = 5760;
/// 出力パケットのバッファ（RFC 6716 の上限 1275 バイト × 最大 6 フレーム程度）。
const MAX_PACKET_BYTES: usize = 4000;

pub fn probe(options: &ProbeOptions) -> Arc<dyn CodecProvider> {
    match ffi::OpusLib::load(&options.libs) {
        Ok(lib) => Arc::new(OpusProvider::new(lib)),
        Err(e) => UnavailableProvider::arc(ProviderId::Opus, e),
    }
}

pub struct OpusProvider {
    lib: Arc<ffi::OpusLib>,
    device: DeviceInfo,
}

impl OpusProvider {
    fn new(lib: Arc<ffi::OpusLib>) -> Self {
        let device = software_device(
            ProviderId::Opus,
            "libopus",
            Some(lib.version()),
            lib.library_name(),
        );
        OpusProvider { lib, device }
    }
}

fn opus_err(what: &str, code: i32) -> CodecError {
    CodecError::backend(format!("opus {what}: {}", ffi::error_name(code)))
}

fn valid_rate(r: u32) -> bool {
    matches!(r, 8000 | 12000 | 16000 | 24000 | 48000)
}

impl CodecProvider for OpusProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Opus
    }

    fn status(&self) -> ProviderStatus {
        ProviderStatus {
            provider: ProviderId::Opus,
            available: true,
            detail: format!("{} ({})", self.lib.library_name(), self.lib.version()),
        }
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        vec![self.device.clone()]
    }

    fn capabilities(&self) -> Vec<Capability> {
        [Direction::Encode, Direction::Decode]
            .into_iter()
            .map(|d| Capability {
                provider: ProviderId::Opus,
                codec: Codec::Opus,
                direction: d,
                hardware: false,
                max_width: 0,
                max_height: 0,
                max_sessions: None,
                formats: Vec::new(),
                device_index: 0,
                device_name: self.device.name.clone(),
                driver_version: self.device.driver_version.clone(),
            })
            .collect()
    }

    fn create_encoder(&self, config: &EncoderConfig) -> Result<Box<dyn Encoder>> {
        let EncoderConfig::Audio(cfg) = config else {
            return Err(CodecError::unsupported("opus provider encodes audio only"));
        };
        if cfg.codec != Codec::Opus {
            return Err(CodecError::unsupported(format!(
                "opus provider cannot encode {}",
                cfg.codec
            )));
        }
        Ok(Box::new(OpusEncoder::new(self.lib.clone(), cfg.clone())?))
    }

    fn create_decoder(&self, config: &DecoderConfig) -> Result<Box<dyn Decoder>> {
        if config.codec != Codec::Opus {
            return Err(CodecError::unsupported(format!(
                "opus provider cannot decode {}",
                config.codec
            )));
        }
        Ok(Box::new(OpusDecoder::new(self.lib.clone(), config)?))
    }
}

struct OpusEncoder {
    raw: ffi::RawEncoder,
    cfg: AudioEncoderConfig,
    stamper: AuStamper,
    /// 1 パケットの標本数（符号化の標本化周波数で、1 チャンネルあたり）。
    frame_size: usize,
    /// 溜めている標本（インターリーブ）。
    pending: Vec<f32>,
    /// `pending` の先頭の時刻（1/48000）。
    pending_pts: Option<i64>,
    out: std::collections::VecDeque<AccessUnit>,
    flushed: bool,
    /// 符号化器の遅延（入力の標本化周波数での標本数）。flush でこの分の無音を足して出し切る。
    lookahead: usize,
}

impl OpusEncoder {
    fn new(lib: Arc<ffi::OpusLib>, cfg: AudioEncoderConfig) -> Result<Self> {
        cfg.validate()?;
        if !valid_rate(cfg.sample_rate) {
            return Err(CodecError::invalid_config(
                "opus sample rate must be 8/12/16/24/48 kHz",
            ));
        }
        if cfg.channels > 2 {
            return Err(CodecError::unsupported(
                "opus multistream (>2 channels) is not supported",
            ));
        }
        if !matches!(
            cfg.frame_duration_us,
            2500 | 5000 | 10000 | 20000 | 40000 | 60000
        ) {
            return Err(CodecError::invalid_config(
                "opus frame duration must be 2.5/5/10/20/40/60 ms",
            ));
        }
        let frame_size =
            (cfg.sample_rate as u64 * cfg.frame_duration_us as u64 / 1_000_000) as usize;
        let app = match cfg.application {
            OpusApplication::Voip => b::OPUS_APPLICATION_VOIP,
            OpusApplication::Audio => b::OPUS_APPLICATION_AUDIO,
            OpusApplication::LowDelay => b::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        } as i32;
        let mut raw = ffi::RawEncoder::new(lib, cfg.sample_rate as i32, cfg.channels, app)
            .map_err(|e| opus_err("encoder create", e))?;
        if cfg.bitrate_bps > 0 {
            let br = cfg.bitrate_bps.clamp(500, 512_000) as i32;
            raw.set(b::OPUS_SET_BITRATE_REQUEST, br)
                .map_err(|e| opus_err("set bitrate", e))?;
        }
        let lookahead = raw
            .get(b::OPUS_GET_LOOKAHEAD_REQUEST)
            .map_err(|e| opus_err("get lookahead", e))?;
        let pre_skip = (lookahead.max(0) as u64 * 48000 / cfg.sample_rate as u64) as u16;
        let mut stamper = AuStamper::new(Codec::Opus, cfg.track, OPUS_TIMEBASE);
        stamper.set_config(
            build_opus_head(cfg.channels, pre_skip, cfg.sample_rate),
            None,
            Some(AudioParams {
                sample_rate: 48000,
                channels: cfg.channels,
                object_type: 0,
            }),
        );
        Ok(OpusEncoder {
            raw,
            cfg,
            stamper,
            frame_size,
            pending: Vec::new(),
            pending_pts: None,
            out: Default::default(),
            flushed: false,
            lookahead: lookahead.max(0) as usize,
        })
    }

    fn encode_one(&mut self, n: usize) -> Result<()> {
        let ch = self.cfg.channels as usize;
        let mut packet = vec![0u8; MAX_PACKET_BYTES];
        let samples: Vec<f32> = self.pending.drain(..n * ch).collect();
        let len = self
            .raw
            .encode_f32(&samples, n, &mut packet)
            .map_err(|e| opus_err("encode", e))?;
        packet.truncate(len);
        let dur48 = (n as u64 * 48000 / self.cfg.sample_rate as u64) as i64;
        let pts = self.pending_pts.unwrap_or(0);
        self.pending_pts = Some(pts + dur48);
        let au = self
            .stamper
            .stamp(Bytes::from(packet), pts, pts, dur48 as u64, true);
        self.out.push_back(au);
        Ok(())
    }
}

/// 音声フレームの標本を f32 に揃える。
fn samples_f32(f: &AudioFrame) -> Vec<f32> {
    match f.format {
        SampleFormat::F32 => f
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_ne_bytes(*c))
            .collect(),
        SampleFormat::I16 => f
            .data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_ne_bytes(*c) as f32 / 32768.0)
            .collect(),
    }
}

impl Encoder for OpusEncoder {
    fn codec(&self) -> Codec {
        Codec::Opus
    }

    fn codec_config(&self) -> Option<CodecConfig> {
        self.stamper.config()
    }

    fn send(&mut self, frame: &Frame) -> Result<()> {
        if self.flushed {
            return Err(CodecError::internal("send after flush"));
        }
        let Frame::Audio(f) = frame else {
            return Err(CodecError::invalid_input("opus encoder needs audio frames"));
        };
        f.validate()?;
        if f.sample_rate != self.cfg.sample_rate || f.channels != self.cfg.channels {
            return Err(CodecError::invalid_input(
                "audio frame format does not match encoder",
            ));
        }
        if self.pending.is_empty() {
            let pts = f
                .pts
                .rescale(f.timebase, OPUS_TIMEBASE)
                .ok_or_else(|| CodecError::invalid_input("pts out of range"))?;
            self.pending_pts = Some(pts);
        }
        self.pending.extend(samples_f32(f));
        let ch = self.cfg.channels as usize;
        while self.pending.len() >= self.frame_size * ch {
            self.encode_one(self.frame_size)?;
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<AccessUnit>> {
        Ok(self.out.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        if self.flushed {
            return Ok(());
        }
        self.flushed = true;
        if self.pending_pts.is_none() {
            return Ok(());
        }
        // 符号化器の遅延（lookahead）分の無音を足し、フレームの倍数まで埋めて出し切る。
        // 末尾の余り（詰め物）を切るのは入れ物の役目（Ogg の granule など）。
        let ch = self.cfg.channels as usize;
        let total = self.pending.len() / ch + self.lookahead;
        let frames = total.div_ceil(self.frame_size);
        self.pending.resize(frames * self.frame_size * ch, 0.0);
        for _ in 0..frames {
            self.encode_one(self.frame_size)?;
        }
        Ok(())
    }

    fn request_keyframe(&mut self) {}
}

struct OpusDecoder {
    raw: ffi::RawDecoder,
    lib: Arc<ffi::OpusLib>,
    sample_rate: u32,
    channels: u8,
    format: SampleFormat,
    /// まだ捨てていない pre_skip（出力の標本化周波数での標本数）。
    skip: usize,
    out: std::collections::VecDeque<Frame>,
}

impl OpusDecoder {
    fn new(lib: Arc<ffi::OpusLib>, config: &DecoderConfig) -> Result<Self> {
        config.validate()?;
        let head = match &config.config {
            Some(c) if !c.record.is_empty() => Some(parse_opus_head(&c.record)?),
            _ => None,
        };
        let sample_rate = if config.sample_rate == 0 {
            48000
        } else {
            config.sample_rate
        };
        if !valid_rate(sample_rate) {
            return Err(CodecError::invalid_config(
                "opus output rate must be 8/12/16/24/48 kHz",
            ));
        }
        let channels = match (config.channels, head) {
            (0, Some(h)) => h.channels,
            (0, None) => 2,
            (c, _) => c,
        };
        if channels > 2 || head.is_some_and(|h| h.mapping_family != 0) {
            return Err(CodecError::unsupported(
                "opus multistream (>2 channels) is not supported",
            ));
        }
        let skip = head.map_or(0, |h| h.pre_skip as usize * sample_rate as usize / 48000);
        let raw = ffi::RawDecoder::new(lib.clone(), sample_rate as i32, channels)
            .map_err(|e| opus_err("decoder create", e))?;
        Ok(OpusDecoder {
            raw,
            lib,
            sample_rate,
            channels,
            format: config.sample_format,
            skip,
            out: Default::default(),
        })
    }
}

impl Decoder for OpusDecoder {
    fn codec(&self) -> Codec {
        Codec::Opus
    }

    fn set_config(&mut self, config: &CodecConfig) -> Result<()> {
        if !config.record.is_empty() {
            let h = parse_opus_head(&config.record)?;
            if h.channels != self.channels {
                return Err(CodecError::unsupported(
                    "opus channel count change mid-stream",
                ));
            }
        }
        Ok(())
    }

    fn send(&mut self, au: &AccessUnit) -> Result<()> {
        if au.codec != Codec::Opus {
            return Err(CodecError::invalid_input("not an opus access unit"));
        }
        let rate = self.sample_rate as i32;
        let n = self
            .lib
            .packet_samples(&au.payload, rate)
            .map_err(|_| CodecError::invalid_input("malformed opus packet"))?;
        if n == 0 || n > MAX_PACKET_SAMPLES_48K * self.sample_rate as usize / 48000 {
            return Err(CodecError::invalid_input(
                "opus packet duration out of range",
            ));
        }
        let ch = self.channels as usize;
        let (data, got) = match self.format {
            SampleFormat::F32 => {
                let mut buf = vec![0f32; n * ch];
                let got = self
                    .raw
                    .decode_f32(&au.payload, &mut buf)
                    .map_err(|e| opus_err("decode", e))?;
                let skip = self.skip.min(got);
                let bytes: Vec<u8> = buf[skip * ch..got * ch]
                    .iter()
                    .flat_map(|v| v.to_ne_bytes())
                    .collect();
                (bytes, skip)
            }
            SampleFormat::I16 => {
                let mut buf = vec![0i16; n * ch];
                let got = self
                    .raw
                    .decode_i16(&au.payload, &mut buf)
                    .map_err(|e| opus_err("decode", e))?;
                let skip = self.skip.min(got);
                let bytes: Vec<u8> = buf[skip * ch..got * ch]
                    .iter()
                    .flat_map(|v| v.to_ne_bytes())
                    .collect();
                (bytes, skip)
            }
        };
        self.skip -= got;
        if data.is_empty() {
            return Ok(());
        }
        // 捨てた分だけ時刻を進める。
        let skipped = (got as i64)
            .rescale(Timebase::new(1, self.sample_rate), au.timebase)
            .ok_or_else(|| CodecError::invalid_input("pts out of range"))?;
        self.out.push_back(Frame::Audio(AudioFrame {
            format: self.format,
            sample_rate: self.sample_rate,
            channels: self.channels,
            data: Bytes::from(data),
            pts: au.pts.saturating_add(skipped),
            timebase: au.timebase,
        }));
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<Frame>> {
        Ok(self.out.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
