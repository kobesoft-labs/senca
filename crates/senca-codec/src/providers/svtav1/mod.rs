//! SVT-AV1（AV1 の符号化、ソフトウェア、BSD-3-Clause-Clear と AOM の特許許諾。ロイヤリティフリーとして提供、REQ-007）。
//!
//! - 入力は 8 bit の I420（NV12 は I420 に並べ替える）。10 bit の符号化は未対応。
//! - 出力 AU は内部形式（ADR-0003）の OBU 列（Temporal Delimiter を除く）。設定は av1C（シーケンスヘッダーから作る）。
//! - 低遅延では予測構造を low delay、先読み 0、速いプリセットにする。
//! - SVT-AV1 は幅・高さ 64 以上、偶数を要求する（満たさなければ `InvalidConfig`）。
//! - VBR の上限（`max_bitrate_bps`）は SVT-AV1 が CRF でしか受け付けないので使わない。
//! - B フレームあり（random access）かつ VBR では、キーフレームの強制要求は無視する（SVT-AV1 の制約）。

#[allow(unsafe_code)]
mod ffi;

use crate::bitstream::av1;
use crate::caps::{Capability, DeviceInfo, Direction, ProviderId, ProviderStatus};
use crate::config::{DecoderConfig, EncoderConfig, RateControl, VideoEncoderConfig};
use crate::error::{CodecError, Result};
use crate::frame::{Frame, PixelFormat};
use crate::provider::{CodecProvider, Decoder, Encoder};
use crate::providers::common::{
    AuStamper, UnavailableProvider, capability, check_input_frame, frame_duration, software_device,
};
use crate::registry::ProbeOptions;
use bytes::Bytes;
use senca_core::{AccessUnit, Codec, CodecConfig};
use std::collections::VecDeque;
use std::sync::Arc;

pub fn probe(options: &ProbeOptions) -> Arc<dyn CodecProvider> {
    match ffi::SvtLib::load(&options.libs) {
        Ok(lib) => {
            let device = software_device(
                ProviderId::SvtAv1,
                "SVT-AV1",
                Some(lib.version()),
                lib.library_name(),
            );
            Arc::new(SvtProvider { lib, device })
        }
        Err(e) => UnavailableProvider::arc(ProviderId::SvtAv1, e),
    }
}

pub struct SvtProvider {
    lib: Arc<ffi::SvtLib>,
    device: DeviceInfo,
}

impl CodecProvider for SvtProvider {
    fn id(&self) -> ProviderId {
        ProviderId::SvtAv1
    }

    fn status(&self) -> ProviderStatus {
        ProviderStatus {
            provider: ProviderId::SvtAv1,
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
            Direction::Encode,
            16384,
            8704,
            None,
            &[PixelFormat::I420, PixelFormat::Nv12],
        )]
    }

    fn create_encoder(&self, config: &EncoderConfig) -> Result<Box<dyn Encoder>> {
        let EncoderConfig::Video(cfg) = config else {
            return Err(CodecError::unsupported("SVT-AV1 encodes video only"));
        };
        if cfg.codec != Codec::Av1 {
            return Err(CodecError::unsupported(format!(
                "SVT-AV1 cannot encode {}",
                cfg.codec
            )));
        }
        Ok(Box::new(SvtEncoder::new(self.lib.clone(), cfg.clone())?))
    }

    fn create_decoder(&self, _config: &DecoderConfig) -> Result<Box<dyn Decoder>> {
        Err(CodecError::unsupported("SVT-AV1 is an encoder only"))
    }
}

struct SvtEncoder {
    raw: ffi::RawEncoder,
    cfg: VideoEncoderConfig,
    stamper: AuStamper,
    duration: u64,
    force_key: bool,
    out: VecDeque<AccessUnit>,
    flushed: bool,
    eos_seen: bool,
    /// low delay の予測構造か。SVT-AV1 はこのとき `get_packet` が常に待つ（1 枚入れると 1 パケット出る）ので、
    /// 送った枚数だけ取り出す。
    low_delay: bool,
    /// 送ったがまだパケットを受け取っていない枚数。
    outstanding: usize,
    /// キーフレームを強制できるか（low delay か CQP）。
    can_force_key: bool,
}

impl SvtEncoder {
    fn new(lib: Arc<ffi::SvtLib>, cfg: VideoEncoderConfig) -> Result<Self> {
        cfg.validate()?;
        if cfg.width < 64
            || cfg.height < 64
            || !cfg.width.is_multiple_of(2)
            || !cfg.height.is_multiple_of(2)
        {
            return Err(CodecError::invalid_config(
                "SVT-AV1 needs even width/height of at least 64",
            ));
        }
        if cfg.input_format == PixelFormat::P010 {
            return Err(CodecError::unsupported(
                "10-bit AV1 encoding is not supported",
            ));
        }
        let (rc_mode, qp) = match cfg.rate_control {
            RateControl::Cbr => (2, 0),
            RateControl::Vbr => (1, 0),
            RateControl::ConstQp(q) => (0, (q as u32).clamp(1, 63)),
        };
        // CBR は low delay の予測構造でしか使えない（SVT-AV1 の制約）。
        let low_delay = cfg.low_latency || cfg.b_frames == 0 || rc_mode == 2;
        let params = ffi::Params {
            width: cfg.width,
            height: cfg.height,
            fps_num: cfg.fps_num,
            fps_den: cfg.fps_den,
            preset: if cfg.low_latency { 10 } else { 8 },
            intra_period: if cfg.gop == 0 {
                -1
            } else {
                (cfg.gop - 1).min(i32::MAX as u32) as i32
            },
            low_delay,
            rc_mode,
            qp,
            target_bitrate: cfg.bitrate_bps,
            threads: cfg.threads,
        };
        let mut raw = ffi::RawEncoder::new(lib, &params)
            .map_err(|e| CodecError::backend(format!("SVT-AV1 {e}")))?;
        let header = raw
            .stream_header()
            .map_err(|e| CodecError::backend(format!("SVT-AV1 {e}")))?;
        let mut stamper = AuStamper::new(Codec::Av1, cfg.track, cfg.timebase);
        if let Some(seq) = av1::find_sequence_header(&header)? {
            let (rec, sh) = av1::build_av1c(seq)?;
            stamper.set_config(rec, Some(av1::video_params(&sh)), None);
        }
        Ok(SvtEncoder {
            raw,
            duration: frame_duration(&cfg),
            stamper,
            cfg,
            force_key: false,
            out: VecDeque::new(),
            flushed: false,
            eos_seen: false,
            low_delay,
            outstanding: 0,
            can_force_key: low_delay || rc_mode == 0,
        })
    }

    /// パケットを取り出す。`done`（EOS 送信後）なら終わりまで、そうでなければ待たずに取れる分
    /// （low delay では送った枚数分）だけ。
    fn take_packets(&mut self, done: bool) -> Result<()> {
        // 1 回の呼び出しで取り出す数に上限を置く（ライブラリの異常で無限に回らない）。
        for _ in 0..4096 {
            if self.eos_seen || (!done && self.low_delay && self.outstanding == 0) {
                return Ok(());
            }
            let Some(p) = self
                .raw
                .get_packet(done)
                .map_err(|e| CodecError::backend(format!("SVT-AV1 {e}")))?
            else {
                return Ok(());
            };
            self.outstanding = self.outstanding.saturating_sub(1);
            if p.eos {
                self.eos_seen = true;
            }
            if p.data.is_empty() {
                continue;
            }
            let payload = av1::to_internal(&p.data)?;
            if payload.is_empty() {
                continue;
            }
            // シーケンスヘッダーが変わったら設定を更新する。
            if let Some(seq) = av1::find_sequence_header(&payload)? {
                let (rec, sh) = av1::build_av1c(seq)?;
                self.stamper
                    .set_config(rec, Some(av1::video_params(&sh)), None);
            }
            let au = self.stamper.stamp(
                Bytes::from(payload),
                p.pts,
                p.pts,
                self.duration,
                p.keyframe,
            );
            self.out.push_back(au);
        }
        Ok(())
    }
}

impl Encoder for SvtEncoder {
    fn codec(&self) -> Codec {
        Codec::Av1
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
                "SVT-AV1 encoder needs video frames",
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
        // random access の VBR では SVT-AV1 がキーフレームの強制を受け付けないので、要求は捨てる（GOP の周期に任せる）。
        let key = (std::mem::take(&mut self.force_key) || f.keyframe) && self.can_force_key;
        self.raw
            .send(
                [&f.planes[0].data, &f.planes[1].data, &f.planes[2].data],
                [f.planes[0].stride, f.planes[1].stride, f.planes[2].stride],
                f.pts,
                key,
            )
            .map_err(|e| CodecError::backend(format!("SVT-AV1 {e}")))?;
        self.outstanding += 1;
        self.take_packets(false)
    }

    fn receive(&mut self) -> Result<Option<AccessUnit>> {
        Ok(self.out.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        if self.flushed {
            return Ok(());
        }
        self.flushed = true;
        self.raw
            .send_eos()
            .map_err(|e| CodecError::backend(format!("SVT-AV1 {e}")))?;
        self.take_packets(true)
    }

    fn request_keyframe(&mut self) {
        self.force_key = true;
    }
}
