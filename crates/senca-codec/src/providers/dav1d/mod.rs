//! dav1d（AV1 の復号、ソフトウェア、BSD-2。ロイヤリティフリーとして提供、REQ-007）。
//!
//! - 入力は内部形式（ADR-0003）の OBU 列。最初の AU にシーケンスヘッダーが無ければ、av1C の configOBUs を前に付ける。
//! - 出力は 8 bit なら I420（希望すれば NV12）、10 bit なら P010（希望すれば 8 bit に丸める）。
//! - 低遅延のため `max_frame_delay = 1`（1 AU 入れれば 1 枚出る）。

#[allow(unsafe_code)]
mod ffi;

use crate::bitstream::av1;
use crate::caps::{Capability, DeviceInfo, Direction, ProviderId, ProviderStatus};
use crate::config::{DecoderConfig, EncoderConfig};
use crate::error::{CodecError, Result};
use crate::frame::{Frame, PixelFormat};
use crate::provider::{CodecProvider, Decoder, Encoder};
use crate::providers::common::{
    UnavailableProvider, capability, planar_yuv_to_frame, software_device,
};
use crate::registry::ProbeOptions;
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase};
use std::collections::VecDeque;
use std::sync::Arc;

pub fn probe(options: &ProbeOptions) -> Arc<dyn CodecProvider> {
    match ffi::Dav1dLib::load(&options.libs) {
        Ok(lib) => {
            let device = software_device(
                ProviderId::Dav1d,
                "dav1d",
                Some(lib.version()),
                lib.library_name(),
            );
            Arc::new(Dav1dProvider { lib, device })
        }
        Err(e) => UnavailableProvider::arc(ProviderId::Dav1d, e),
    }
}

pub struct Dav1dProvider {
    lib: Arc<ffi::Dav1dLib>,
    device: DeviceInfo,
}

impl CodecProvider for Dav1dProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Dav1d
    }

    fn status(&self) -> ProviderStatus {
        ProviderStatus {
            provider: ProviderId::Dav1d,
            available: true,
            detail: format!("{} ({})", self.lib.library_name(), self.lib.version()),
        }
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        vec![self.device.clone()]
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![capability(
            &self.device,
            Codec::Av1,
            Direction::Decode,
            16384,
            16384,
            None,
            &[PixelFormat::I420, PixelFormat::Nv12, PixelFormat::P010],
        )]
    }

    fn create_encoder(&self, _config: &EncoderConfig) -> Result<Box<dyn Encoder>> {
        Err(CodecError::unsupported("dav1d is a decoder only"))
    }

    fn create_decoder(&self, config: &DecoderConfig) -> Result<Box<dyn Decoder>> {
        if config.codec != Codec::Av1 {
            return Err(CodecError::unsupported(format!(
                "dav1d cannot decode {}",
                config.codec
            )));
        }
        config.validate()?;
        Ok(Box::new(Dav1dDecoder::new(self.lib.clone(), config)?))
    }
}

struct Dav1dDecoder {
    ctx: ffi::Context,
    config: DecoderConfig,
    /// 次の AU の前に付ける configOBUs（シーケンスヘッダー）。
    pending_header: Option<Vec<u8>>,
    /// 入力 AU の時間軸（出力の pts に使う）。
    timebase: Timebase,
    out: VecDeque<Frame>,
    flushed: bool,
}

fn dav1d_err(what: &str, code: i32) -> CodecError {
    if code == -libc::EINVAL {
        CodecError::invalid_input(format!("dav1d {what}: invalid data"))
    } else if code == -libc::ENOMEM {
        CodecError::backend(format!("dav1d {what}: out of memory"))
    } else {
        CodecError::invalid_input(format!("dav1d {what}: error {code}"))
    }
}

impl Dav1dDecoder {
    fn new(lib: Arc<ffi::Dav1dLib>, config: &DecoderConfig) -> Result<Self> {
        let settings = ffi::Settings {
            threads: config.threads,
            max_frame_delay: 1,
            frame_size_limit: config.max_width.saturating_mul(config.max_height),
        };
        let ctx = ffi::Context::open(lib, &settings).map_err(|e| dav1d_err("open", e))?;
        let mut d = Dav1dDecoder {
            ctx,
            config: config.clone(),
            pending_header: None,
            timebase: Timebase::MPEG,
            out: VecDeque::new(),
            flushed: false,
        };
        if let Some(c) = &config.config {
            d.set_config(c)?;
        }
        Ok(d)
    }

    fn collect(&mut self, pics: Vec<ffi::Picture>) -> Result<()> {
        for p in pics {
            self.config.check_resolution(p.width, p.height)?;
            let layout = match p.layout {
                1 => Some(true),
                0 => Some(false),
                _ => None,
            };
            let mut f = planar_yuv_to_frame(
                p.width,
                p.height,
                layout,
                p.bpc,
                [&p.planes[0], &p.planes[1], &p.planes[2]],
                p.row_bytes,
                self.config.output_format,
                p.timestamp,
                self.timebase,
            )?;
            f.keyframe = p.keyframe;
            self.out.push_back(Frame::Video(f));
        }
        Ok(())
    }

    fn max_pixels(&self) -> u64 {
        self.config.max_width as u64 * self.config.max_height as u64
    }
}

impl Decoder for Dav1dDecoder {
    fn codec(&self) -> Codec {
        Codec::Av1
    }

    fn set_config(&mut self, config: &CodecConfig) -> Result<()> {
        if config.codec != Codec::Av1 {
            return Err(CodecError::invalid_config("not an AV1 config"));
        }
        if !config.record.is_empty() {
            let obus = av1::parse_av1c(&config.record)?;
            if !obus.is_empty() {
                self.pending_header = Some(av1::to_internal(obus)?);
            }
        }
        Ok(())
    }

    fn send(&mut self, au: &AccessUnit) -> Result<()> {
        if self.flushed {
            return Err(CodecError::internal("send after flush"));
        }
        if au.codec != Codec::Av1 {
            return Err(CodecError::invalid_input("not an AV1 access unit"));
        }
        self.timebase = au.timebase;
        // 構造を先に確かめる（壊れた OBU は dav1d に渡さない）。
        let obus = av1::parse_obus(&au.payload)?;
        let has_seq = obus.iter().any(|o| o.obu_type == av1::OBU_SEQUENCE_HEADER);
        let mut data = Vec::with_capacity(au.payload.len() + 64);
        if let Some(h) = self.pending_header.take()
            && !has_seq
        {
            data.extend_from_slice(&h);
        }
        data.extend_from_slice(&au.payload);
        let mut pics = Vec::new();
        let max = self.max_pixels();
        let r = self.ctx.send(&data, au.pts, &mut pics, max);
        self.collect(pics)?;
        r.map_err(|e| dav1d_err("send", e))?;
        let mut pics = Vec::new();
        let r = self.ctx.drain(&mut pics, max);
        self.collect(pics)?;
        r.map_err(|e| dav1d_err("decode", e))
    }

    fn receive(&mut self) -> Result<Option<Frame>> {
        Ok(self.out.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        self.flushed = true;
        let mut pics = Vec::new();
        let max = self.max_pixels();
        let r = self.ctx.drain(&mut pics, max);
        self.collect(pics)?;
        r.map_err(|e| dav1d_err("drain", e))
    }
}
