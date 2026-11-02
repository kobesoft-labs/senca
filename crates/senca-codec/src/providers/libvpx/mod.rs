//! libvpx（VP8 / VP9 の符号化・復号、ソフトウェア、BSD-3。ロイヤリティフリーとして提供、REQ-007）。
//!
//! - AU はフレーム 1 つ（VP9 はスーパーフレームを含みうる）。設定（record）は空。
//! - 符号化の入力は 8 bit の I420（NV12 は I420 に並べ替える）。VP9 profile 2（10 bit）の符号化は未対応。
//! - VP8 は alt-ref を使わない（不可視フレームを出さない）。低遅延では先読み 0、リアルタイムの速度設定。
//! - 復号の出力は 8 bit なら I420（希望すれば NV12）、高ビット深度なら P010。

#[allow(unsafe_code)]
mod ffi;

use crate::caps::{Capability, DeviceInfo, Direction, ProviderId, ProviderStatus};
use crate::config::{DecoderConfig, EncoderConfig, RateControl, VideoEncoderConfig, VideoProfile};
use crate::error::{CodecError, Result};
use crate::frame::{Frame, PixelFormat};
use crate::provider::{CodecProvider, Decoder, Encoder};
use crate::providers::common::{
    AuStamper, UnavailableProvider, capability, check_input_frame, frame_duration,
    planar_yuv_to_frame, software_device,
};
use crate::registry::ProbeOptions;
use bytes::Bytes;
use ffi::Kind;
use senca_core::media::VideoParams;
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase};
use std::collections::VecDeque;
use std::sync::Arc;

pub fn probe(options: &ProbeOptions) -> Arc<dyn CodecProvider> {
    match ffi::VpxLib::load(&options.libs) {
        Ok(lib) => {
            let device = software_device(
                ProviderId::Libvpx,
                "libvpx",
                Some(lib.version()),
                lib.library_name(),
            );
            Arc::new(VpxProvider { lib, device })
        }
        Err(e) => UnavailableProvider::arc(ProviderId::Libvpx, e),
    }
}

pub struct VpxProvider {
    lib: Arc<ffi::VpxLib>,
    device: DeviceInfo,
}

fn kind_of(codec: Codec) -> Option<Kind> {
    match codec {
        Codec::Vp8 => Some(Kind::Vp8),
        Codec::Vp9 => Some(Kind::Vp9),
        _ => None,
    }
}

impl CodecProvider for VpxProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Libvpx
    }

    fn status(&self) -> ProviderStatus {
        ProviderStatus {
            provider: ProviderId::Libvpx,
            available: true,
            detail: format!("{} ({})", self.lib.library_name(), self.lib.version()),
        }
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        vec![self.device.clone()]
    }

    fn capabilities(&self) -> Vec<Capability> {
        let mut v = Vec::new();
        for (codec, kind) in [(Codec::Vp8, Kind::Vp8), (Codec::Vp9, Kind::Vp9)] {
            let max = if kind == Kind::Vp8 { 16383 } else { 16384 };
            if self.lib.has_encoder(kind) {
                v.push(capability(
                    &self.device,
                    codec,
                    Direction::Encode,
                    max,
                    max,
                    None,
                    &[PixelFormat::I420, PixelFormat::Nv12],
                ));
            }
            if self.lib.has_decoder(kind) {
                v.push(capability(
                    &self.device,
                    codec,
                    Direction::Decode,
                    max,
                    max,
                    None,
                    &[PixelFormat::I420, PixelFormat::Nv12, PixelFormat::P010],
                ));
            }
        }
        v
    }

    fn create_encoder(&self, config: &EncoderConfig) -> Result<Box<dyn Encoder>> {
        let EncoderConfig::Video(cfg) = config else {
            return Err(CodecError::unsupported("libvpx encodes video only"));
        };
        let kind = kind_of(cfg.codec).ok_or_else(|| {
            CodecError::unsupported(format!("libvpx cannot encode {}", cfg.codec))
        })?;
        if !self.lib.has_encoder(kind) {
            return Err(CodecError::unsupported(format!(
                "this libvpx has no {} encoder",
                cfg.codec
            )));
        }
        Ok(Box::new(VpxEncoder::new(
            self.lib.clone(),
            kind,
            cfg.clone(),
        )?))
    }

    fn create_decoder(&self, config: &DecoderConfig) -> Result<Box<dyn Decoder>> {
        config.validate()?;
        let kind = kind_of(config.codec).ok_or_else(|| {
            CodecError::unsupported(format!("libvpx cannot decode {}", config.codec))
        })?;
        if !self.lib.has_decoder(kind) {
            return Err(CodecError::unsupported(format!(
                "this libvpx has no {} decoder",
                config.codec
            )));
        }
        let raw = ffi::RawDecoder::new(self.lib.clone(), kind, config.threads)
            .map_err(CodecError::backend)?;
        Ok(Box::new(VpxDecoder {
            raw,
            codec: config.codec,
            config: config.clone(),
            timebase: Timebase::MPEG,
            out: VecDeque::new(),
            flushed: false,
        }))
    }
}

struct VpxEncoder {
    raw: ffi::RawEncoder,
    cfg: VideoEncoderConfig,
    stamper: AuStamper,
    duration: u64,
    force_key: bool,
    out: VecDeque<AccessUnit>,
    flushed: bool,
}

impl VpxEncoder {
    fn new(lib: Arc<ffi::VpxLib>, kind: Kind, cfg: VideoEncoderConfig) -> Result<Self> {
        cfg.validate()?;
        if matches!(cfg.profile, Some(VideoProfile::Vp9Profile2))
            || cfg.input_format == PixelFormat::P010
        {
            return Err(CodecError::unsupported(
                "10-bit VP9 encoding is not supported",
            ));
        }
        if kind == Kind::Vp8 && (cfg.width > 16383 || cfg.height > 16383) {
            return Err(CodecError::invalid_config("VP8 resolution limit is 16383"));
        }
        let tb_num =
            i32::try_from(cfg.timebase.num).map_err(|_| CodecError::invalid_config("timebase"))?;
        let tb_den =
            i32::try_from(cfg.timebase.den).map_err(|_| CodecError::invalid_config("timebase"))?;
        let (end_usage, min_q, max_q, cq) = match cfg.rate_control {
            RateControl::Cbr => (ffi::bindings::vpx_rc_mode_VPX_CBR, 4, 56, None),
            RateControl::Vbr => (ffi::bindings::vpx_rc_mode_VPX_VBR, 4, 56, None),
            RateControl::ConstQp(q) => {
                let q = (q as u32).min(63);
                (ffi::bindings::vpx_rc_mode_VPX_Q, q, q, Some(q))
            }
        };
        let threads = if cfg.threads == 0 {
            std::thread::available_parallelism()
                .map_or(1, |n| n.get() as u32)
                .min(8)
        } else {
            cfg.threads
        };
        let realtime = cfg.low_latency;
        let params = ffi::EncParams {
            kind,
            width: cfg.width,
            height: cfg.height,
            tb_num,
            tb_den,
            threads,
            bitrate_kbps: (cfg.bitrate_bps / 1000).max(1),
            end_usage,
            min_q,
            max_q,
            cq_level: cq,
            kf_max_dist: if cfg.gop == 0 { 99999 } else { cfg.gop },
            lag_in_frames: if realtime || kind == Kind::Vp8 { 0 } else { 16 },
            realtime,
            cpu_used: match (kind, realtime) {
                (Kind::Vp8, true) => 8,
                (Kind::Vp8, false) => 2,
                (Kind::Vp9, true) => 7,
                (Kind::Vp9, false) => 4,
            },
        };
        let raw = ffi::RawEncoder::new(lib, &params).map_err(CodecError::backend)?;
        let mut stamper = AuStamper::new(cfg.codec, cfg.track, cfg.timebase);
        stamper.set_config(
            Bytes::new(),
            Some(VideoParams {
                width: cfg.width,
                height: cfg.height,
                profile: 0,
                compat: 0,
                level: 0,
            }),
            None,
        );
        Ok(VpxEncoder {
            raw,
            duration: frame_duration(&cfg),
            stamper,
            cfg,
            force_key: false,
            out: VecDeque::new(),
            flushed: false,
        })
    }

    fn push(&mut self, packets: Vec<ffi::Packet>) {
        for p in packets {
            let au = self
                .stamper
                .stamp(Bytes::from(p.data), p.pts, p.pts, p.duration, p.keyframe);
            self.out.push_back(au);
        }
    }
}

impl Encoder for VpxEncoder {
    fn codec(&self) -> Codec {
        self.cfg.codec
    }

    fn codec_config(&self) -> Option<CodecConfig> {
        self.stamper.config()
    }

    fn send(&mut self, frame: &Frame) -> Result<()> {
        if self.flushed {
            return Err(CodecError::internal("send after flush"));
        }
        let Frame::Video(f) = frame else {
            return Err(CodecError::invalid_input(
                "libvpx encoder needs video frames",
            ));
        };
        check_input_frame(&self.cfg, f)?;
        let converted;
        let f = match f.format {
            PixelFormat::I420 => f,
            PixelFormat::Nv12 => {
                converted = f.convert(PixelFormat::I420)?;
                &converted
            }
            PixelFormat::P010 => {
                return Err(CodecError::unsupported("10-bit input is not supported"));
            }
        };
        let img = ffi::Image {
            width: f.width,
            height: f.height,
            planes: [&f.planes[0].data, &f.planes[1].data, &f.planes[2].data],
            strides: [f.planes[0].stride, f.planes[1].stride, f.planes[2].stride],
        };
        let key = std::mem::take(&mut self.force_key) || f.keyframe;
        let packets = self
            .raw
            .encode(Some(&img), f.pts, self.duration, key)
            .map_err(CodecError::backend)?;
        self.push(packets);
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
        // 出し切るまで NULL を渡す（上限付き）。
        for _ in 0..64 {
            let packets = self
                .raw
                .encode(None, 0, 0, false)
                .map_err(CodecError::backend)?;
            if packets.is_empty() {
                break;
            }
            self.push(packets);
        }
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_key = true;
    }
}

struct VpxDecoder {
    raw: ffi::RawDecoder,
    codec: Codec,
    config: DecoderConfig,
    timebase: Timebase,
    out: VecDeque<Frame>,
    flushed: bool,
}

impl VpxDecoder {
    fn collect(&mut self, images: Vec<ffi::DecodedImage>) -> Result<()> {
        for im in images {
            self.config.check_resolution(im.width, im.height)?;
            let f = planar_yuv_to_frame(
                im.width,
                im.height,
                im.layout_420,
                im.bit_depth,
                [&im.planes[0], &im.planes[1], &im.planes[2]],
                im.row_bytes,
                self.config.output_format,
                im.pts,
                self.timebase,
            )?;
            self.out.push_back(Frame::Video(f));
        }
        Ok(())
    }

    fn max_pixels(&self) -> u64 {
        self.config.max_width as u64 * self.config.max_height as u64
    }
}

impl Decoder for VpxDecoder {
    fn codec(&self) -> Codec {
        self.codec
    }

    fn set_config(&mut self, config: &CodecConfig) -> Result<()> {
        if config.codec != self.codec {
            return Err(CodecError::invalid_config(
                "codec config does not match decoder",
            ));
        }
        Ok(())
    }

    fn send(&mut self, au: &AccessUnit) -> Result<()> {
        if self.flushed {
            return Err(CodecError::internal("send after flush"));
        }
        if au.codec != self.codec {
            return Err(CodecError::invalid_input(
                "access unit codec does not match decoder",
            ));
        }
        self.timebase = au.timebase;
        let max = self.max_pixels();
        // pts は libvpx の user_priv で絵に付いて戻る。
        let images = self
            .raw
            .decode(Some(&au.payload), au.pts, max)
            .map_err(|e| CodecError::invalid_input(format!("libvpx decode: {e}")))?;
        self.collect(images)
    }

    fn receive(&mut self) -> Result<Option<Frame>> {
        Ok(self.out.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        if self.flushed {
            return Ok(());
        }
        self.flushed = true;
        let max = self.max_pixels();
        let images = self.raw.decode(None, 0, max).map_err(CodecError::backend)?;
        self.collect(images)
    }
}
