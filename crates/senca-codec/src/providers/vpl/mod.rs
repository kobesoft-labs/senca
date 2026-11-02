//! Intel oneVPL（Quick Sync）のプロバイダー。H.264 / H.265 / AV1 の符号化、H.264 / H.265 / AV1 / VP9 の復号。
//!
//! - 実行時に `libvpl.so.2`（oneVPL の dispatcher）を読み込む。dispatcher は GPU ドライバーの実装
//!   （`libmfx-gen.so` など）を探して読み込む。ハードウェアの実装（`MFX_IMPL_TYPE_HARDWARE`）だけを使い、
//!   ソフトウェア実装は選ばない（ADR-0004）。
//! - 面はシステムメモリ（`MFX_IOPATTERN_IN/OUT_SYSTEM_MEMORY`、VPL 2.x の内部割り当て）。入力は NV12（10 bit は P010）。
//!   I420 の入力はここで NV12 に並べ替える。
//! - 時刻は VPL の約束どおり 90 kHz にして渡し、出力で元の時間軸に戻す。
//! - 出力は内部形式（ADR-0003）に変換する（[`convert`]、AMF のプロバイダーも同じものを使う）。
//!
//! # 検証の状況
//!
//! 開発機（GPU なし）で確かめたのは、本物の dispatcher（libvpl 2.16）を読み込み、ハードウェアの実装が
//! 0 個であることを検出して「使えない」を返すところまで。セッションの作成・符号化・復号の経路は
//! ヘッダーに合わせて書いたが、実機では未検証（REQ-004 の「未検証」）。

pub mod convert;
#[allow(unsafe_code)]
mod ffi;

use crate::caps::{Capability, DeviceInfo, Direction, ProviderId, ProviderStatus};
use crate::config::{DecoderConfig, EncoderConfig, RateControl, VideoEncoderConfig, VideoProfile};
use crate::error::{CodecError, Result};
use crate::frame::{Frame, PixelFormat, VideoFrame};
use crate::provider::{CodecProvider, Decoder, Encoder};
use crate::providers::common::{
    UnavailableProvider, capability, check_input_frame, frame_duration,
};
use crate::registry::ProbeOptions;
use bytes::Bytes;
use convert::{InputConverter, OutputConverter};
use ffi::sys;
use senca_core::{AccessUnit, Codec, CodecConfig, Rescale, Timebase};
use std::collections::VecDeque;
use std::sync::Arc;

/// 検出する。ライブラリ・ハードウェアの実装が無ければ「使えない」を返す（落ちない）。
pub fn probe(options: &ProbeOptions) -> Arc<dyn CodecProvider> {
    let vpl = match ffi::Vpl::load(&options.libs) {
        Ok(v) => v,
        Err(e) => {
            return UnavailableProvider::arc(
                ProviderId::IntelVpl,
                format!("libvpl not loadable: {e}"),
            );
        }
    };
    match vpl.enumerate_hardware() {
        Err(e) => UnavailableProvider::arc(
            ProviderId::IntelVpl,
            format!("oneVPL enumeration failed: {e}"),
        ),
        Ok(list) if list.is_empty() => UnavailableProvider::arc(
            ProviderId::IntelVpl,
            format!(
                "no hardware oneVPL implementation found ({} loaded; no Intel GPU or driver)",
                vpl.library_name()
            ),
        ),
        Ok(list) => Arc::new(VplProvider::new(vpl, list)),
    }
}

/// VPL の CodecId とコーデックの対応。
fn codec_of(id: u32) -> Option<Codec> {
    Some(match id {
        sys::MFX_CODEC_AVC => Codec::H264,
        sys::MFX_CODEC_HEVC => Codec::H265,
        sys::MFX_CODEC_AV1 => Codec::Av1,
        sys::MFX_CODEC_VP9 => Codec::Vp9,
        _ => return None,
    })
}

fn codec_id(c: Codec) -> Option<u32> {
    Some(match c {
        Codec::H264 => sys::MFX_CODEC_AVC,
        Codec::H265 => sys::MFX_CODEC_HEVC,
        Codec::Av1 => sys::MFX_CODEC_AV1,
        Codec::Vp9 => sys::MFX_CODEC_VP9,
        _ => return None,
    })
}

/// 符号化に使うコーデック（VP9 の符号化は扱わない）。
const ENCODE_CODECS: [Codec; 3] = [Codec::H264, Codec::H265, Codec::Av1];
/// 範囲が報告されないときの最大解像度。
const DEFAULT_MAX: u32 = 4096;

struct VplProvider {
    vpl: Arc<ffi::Vpl>,
    impls: Vec<ffi::ImplInfo>,
    devices: Vec<DeviceInfo>,
    caps: Vec<Capability>,
}

fn formats_of(fourccs: &[u32]) -> Vec<PixelFormat> {
    let mut v = Vec::new();
    if fourccs.is_empty() || fourccs.contains(&sys::MFX_FOURCC_NV12) {
        v.push(PixelFormat::Nv12);
        v.push(PixelFormat::I420); // 並べ替えて渡す
    }
    if fourccs.contains(&sys::MFX_FOURCC_P010) {
        v.push(PixelFormat::P010);
    }
    v
}

impl VplProvider {
    fn new(vpl: Arc<ffi::Vpl>, impls: Vec<ffi::ImplInfo>) -> Self {
        let mut devices = Vec::new();
        let mut caps = Vec::new();
        for (i, imp) in impls.iter().enumerate() {
            let dev = DeviceInfo {
                provider: ProviderId::IntelVpl,
                index: i as u32,
                name: if imp.device_id.is_empty() {
                    imp.name.clone()
                } else {
                    format!("{} (device {})", imp.name, imp.device_id)
                },
                driver_version: Some(format!(
                    "oneVPL API {}.{}",
                    imp.api_version.0, imp.api_version.1
                )),
                path: None,
                hardware: true,
            };
            for d in &imp.decoders {
                if let Some(c) = codec_of(d.codec_id) {
                    caps.push(capability(
                        &dev,
                        c,
                        Direction::Decode,
                        nz(d.max_width),
                        nz(d.max_height),
                        None,
                        &formats_of(&d.color_formats),
                    ));
                }
            }
            for e in &imp.encoders {
                if let Some(c) = codec_of(e.codec_id).filter(|c| ENCODE_CODECS.contains(c)) {
                    caps.push(capability(
                        &dev,
                        c,
                        Direction::Encode,
                        nz(e.max_width),
                        nz(e.max_height),
                        None,
                        &formats_of(&e.color_formats),
                    ));
                }
            }
            devices.push(dev);
        }
        VplProvider {
            vpl,
            impls,
            devices,
            caps,
        }
    }

    /// 対応する最初の実装（`ImplInfo.index`）。
    fn pick(&self, codec: Codec, dir: Direction, w: u32, h: u32) -> Result<u32> {
        for (i, imp) in self.impls.iter().enumerate() {
            if self.caps.iter().any(|c| {
                c.device_index == i as u32
                    && c.codec == codec
                    && c.direction == dir
                    && w <= c.max_width
                    && h <= c.max_height
            }) {
                return Ok(imp.index);
            }
        }
        Err(CodecError::unavailable(format!(
            "no oneVPL hardware {} {} for {w}x{h}",
            codec,
            dir.as_str()
        )))
    }
}

fn nz(v: u32) -> u32 {
    if v == 0 { DEFAULT_MAX } else { v }
}

impl CodecProvider for VplProvider {
    fn id(&self) -> ProviderId {
        ProviderId::IntelVpl
    }

    fn status(&self) -> ProviderStatus {
        ProviderStatus {
            provider: ProviderId::IntelVpl,
            available: true,
            detail: format!(
                "{} ({} hardware implementation(s))",
                self.vpl.library_name(),
                self.impls.len()
            ),
        }
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        self.devices.clone()
    }

    fn capabilities(&self) -> Vec<Capability> {
        self.caps.clone()
    }

    fn create_encoder(&self, config: &EncoderConfig) -> Result<Box<dyn Encoder>> {
        let EncoderConfig::Video(cfg) = config else {
            return Err(CodecError::unsupported(
                "oneVPL provider encodes video only",
            ));
        };
        cfg.validate()?;
        if !ENCODE_CODECS.contains(&cfg.codec) {
            return Err(CodecError::unsupported(format!(
                "oneVPL provider does not encode {}",
                cfg.codec
            )));
        }
        let idx = self.pick(cfg.codec, Direction::Encode, cfg.width, cfg.height)?;
        let params = encode_params(cfg)?;
        let session = ffi::EncodeSession::new(self.vpl.clone(), idx, &params)?;
        Ok(Box::new(VplEncoder {
            session,
            out: OutputConverter::new(cfg.codec, cfg.track, cfg.timebase)?,
            cfg: cfg.clone(),
            ten_bit: params.ten_bit,
            queue: VecDeque::new(),
            force_key: false,
            flushed: false,
        }))
    }

    fn create_decoder(&self, config: &DecoderConfig) -> Result<Box<dyn Decoder>> {
        config.validate()?;
        let id = codec_id(config.codec).ok_or_else(|| {
            CodecError::unsupported(format!("oneVPL provider does not decode {}", config.codec))
        })?;
        let (w, h) = config
            .config
            .as_ref()
            .and_then(InputConverter::config_resolution)
            .unwrap_or((1, 1));
        config.check_resolution(w.max(1), h.max(1))?;
        let idx = self.pick(config.codec, Direction::Decode, w, h)?;
        let session = ffi::DecodeSession::new(
            self.vpl.clone(),
            idx,
            id,
            config.max_width,
            config.max_height,
        )?;
        Ok(Box::new(VplDecoder {
            session,
            input: InputConverter::new(config.codec, config.config.as_ref())?,
            cfg: config.clone(),
            timebase: Timebase::MPEG,
            queue: VecDeque::new(),
        }))
    }
}

/// `BRCParamMultiplier` を決め、kbps を u16 に収める。
pub(crate) fn brc_multiplier(target_kbps: u32, max_kbps: u32) -> (u16, u16, u16) {
    let top = target_kbps.max(max_kbps).max(1);
    let m = top.div_ceil(u16::MAX as u32).max(1);
    let t = target_kbps.div_ceil(m).min(u16::MAX as u32) as u16;
    let x = max_kbps.div_ceil(m).min(u16::MAX as u32) as u16;
    (m.min(u16::MAX as u32) as u16, t, x)
}

fn align(v: u32, a: u32) -> u32 {
    v.div_ceil(a) * a
}

/// 設定を `mfxVideoParam` の値に写す（純粋な計算）。
pub(crate) fn encode_params(cfg: &VideoEncoderConfig) -> Result<ffi::EncParams> {
    let codec_id = codec_id(cfg.codec)
        .filter(|_| ENCODE_CODECS.contains(&cfg.codec))
        .ok_or_else(|| {
            CodecError::unsupported(format!(
                "{} encode not supported by oneVPL provider",
                cfg.codec
            ))
        })?;
    let profile = match cfg.profile {
        None => 0,
        Some(VideoProfile::H264Baseline) => sys::MFX_PROFILE_AVC_BASELINE,
        Some(VideoProfile::H264ConstrainedBaseline) => sys::MFX_PROFILE_AVC_CONSTRAINED_BASELINE,
        Some(VideoProfile::H264Main) => sys::MFX_PROFILE_AVC_MAIN,
        Some(VideoProfile::H264High) => sys::MFX_PROFILE_AVC_HIGH,
        Some(VideoProfile::HevcMain) => sys::MFX_PROFILE_HEVC_MAIN,
        Some(VideoProfile::HevcMain10) => sys::MFX_PROFILE_HEVC_MAIN10,
        Some(VideoProfile::Av1Main) => sys::MFX_PROFILE_AV1_MAIN,
        Some(p) => {
            return Err(CodecError::invalid_config(format!(
                "profile {p:?} not supported by oneVPL provider"
            )));
        }
    } as u16;
    let ten_bit =
        cfg.input_format == PixelFormat::P010 || cfg.profile == Some(VideoProfile::HevcMain10);
    if ten_bit && cfg.codec == Codec::H264 {
        return Err(CodecError::invalid_config("10-bit H.264 is not supported"));
    }
    if ten_bit && cfg.input_format != PixelFormat::P010 {
        return Err(CodecError::invalid_config(
            "10-bit profile needs P010 input",
        ));
    }
    let (rc, qp) = match cfg.rate_control {
        RateControl::Cbr => (sys::MFX_RATECONTROL_CBR, 0),
        RateControl::Vbr => (sys::MFX_RATECONTROL_VBR, 0),
        RateControl::ConstQp(q) => {
            let q = if cfg.codec == Codec::Av1 {
                (q as u16 * 4).clamp(1, 255)
            } else {
                (q as u16).clamp(1, 51)
            };
            (sys::MFX_RATECONTROL_CQP, q)
        }
    };
    let target = cfg.bitrate_bps.div_ceil(1000);
    let max = match cfg.rate_control {
        RateControl::Vbr => cfg
            .max_bitrate_bps
            .map_or(target.saturating_mul(2), |m| m.div_ceil(1000)),
        _ => target,
    };
    let (mult, t, m) = brc_multiplier(target, max);
    let a = if cfg.codec == Codec::H265 { 32 } else { 16 };
    let gop = if cfg.gop == 0 {
        u16::MAX
    } else {
        cfg.gop.min(u16::MAX as u32) as u16
    };
    Ok(ffi::EncParams {
        codec_id,
        profile,
        target_usage: if cfg.low_latency {
            sys::MFX_TARGETUSAGE_BEST_SPEED
        } else {
            sys::MFX_TARGETUSAGE_BALANCED
        } as u16,
        rate_control: rc as u16,
        brc_multiplier: mult,
        target_kbps: t,
        max_kbps: m,
        qp,
        gop_pic_size: gop,
        gop_ref_dist: (cfg.b_frames + 1).min(16) as u16,
        // AVC: 0 = すべての I を IDR。HEVC: 1 = すべての I を IDR（0 だと最初だけ）。
        idr_interval: if cfg.codec == Codec::H265 { 1 } else { 0 },
        width: align(cfg.width, a) as u16,
        height: align(cfg.height, a) as u16,
        crop_w: cfg.width as u16,
        crop_h: cfg.height as u16,
        fps_n: cfg.fps_num,
        fps_d: cfg.fps_den,
        ten_bit,
    })
}

struct VplEncoder {
    session: ffi::EncodeSession,
    out: OutputConverter,
    cfg: VideoEncoderConfig,
    ten_bit: bool,
    queue: VecDeque<AccessUnit>,
    force_key: bool,
    flushed: bool,
}

impl VplEncoder {
    fn take(&mut self, f: Option<ffi::EncodedFrame>) -> Result<()> {
        let Some(f) = f else { return Ok(()) };
        let tb = self.cfg.timebase;
        let back = |v: i64| {
            v.rescale(Timebase::MPEG, tb)
                .ok_or_else(|| CodecError::backend("encoder timestamp out of range"))
        };
        let pts = back(f.pts90)?;
        let dts = if self.cfg.b_frames == 0 {
            pts
        } else {
            back(f.dts90)?
        };
        let key = f.frame_type & (sys::MFX_FRAMETYPE_I | sys::MFX_FRAMETYPE_IDR) as u16 != 0;
        if let Some(au) = self
            .out
            .push(&f.data, pts, dts, frame_duration(&self.cfg), Some(key))?
        {
            self.queue.push_back(au);
        }
        Ok(())
    }
}

impl Encoder for VplEncoder {
    fn codec(&self) -> Codec {
        self.cfg.codec
    }

    fn codec_config(&self) -> Option<CodecConfig> {
        self.out.config()
    }

    fn send(&mut self, frame: &Frame) -> Result<()> {
        if self.flushed {
            return Err(CodecError::internal("send after flush"));
        }
        let Frame::Video(f) = frame else {
            return Err(CodecError::invalid_input("video encoder got audio frame"));
        };
        check_input_frame(&self.cfg, f)?;
        let want = if self.ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        let f = if f.format == want {
            f.clone()
        } else {
            f.convert(want)?
        };
        let pts90 = f
            .pts
            .rescale(self.cfg.timebase, Timebase::MPEG)
            .ok_or_else(|| CodecError::invalid_input("pts out of range"))?;
        let key = std::mem::take(&mut self.force_key) || f.keyframe;
        let r = self.session.encode(Some((&f, pts90)), key)?;
        self.take(r)
    }

    fn receive(&mut self) -> Result<Option<AccessUnit>> {
        Ok(self.queue.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        if self.flushed {
            return Ok(());
        }
        self.flushed = true;
        for _ in 0..256 {
            match self.session.encode(None, false)? {
                Some(f) => self.take(Some(f))?,
                None => return Ok(()),
            }
        }
        Err(CodecError::backend("encoder did not finish draining"))
    }

    fn request_keyframe(&mut self) {
        self.force_key = true;
    }
}

struct VplDecoder {
    session: ffi::DecodeSession,
    input: InputConverter,
    cfg: DecoderConfig,
    timebase: Timebase,
    queue: VecDeque<Frame>,
}

impl VplDecoder {
    fn take(&mut self, list: Vec<ffi::DecodedSurface>) -> Result<()> {
        for s in list {
            self.cfg.check_resolution(s.width, s.height)?;
            let pts = s.pts90.rescale(Timebase::MPEG, self.timebase).unwrap_or(0);
            let f = VideoFrame::from_packed(
                s.format,
                s.width,
                s.height,
                Bytes::from(s.data),
                pts,
                self.timebase,
            )?;
            let f = match (self.cfg.output_format, f.format) {
                (Some(PixelFormat::I420), PixelFormat::Nv12) => f.convert(PixelFormat::I420)?,
                (None, _) => f,
                (Some(want), got) if want == got => f,
                (Some(want), got) => {
                    return Err(CodecError::unsupported(format!(
                        "decoder output {} cannot be converted to {}",
                        got.as_str(),
                        want.as_str()
                    )));
                }
            };
            self.queue.push_back(Frame::Video(f));
        }
        Ok(())
    }
}

impl Decoder for VplDecoder {
    fn codec(&self) -> Codec {
        self.cfg.codec
    }

    fn set_config(&mut self, config: &CodecConfig) -> Result<()> {
        self.input.set_config(config)
    }

    fn send(&mut self, au: &AccessUnit) -> Result<()> {
        let data = self.input.convert(au)?;
        self.timebase = au.timebase;
        let pts90 = au
            .pts
            .rescale(au.timebase, Timebase::MPEG)
            .ok_or_else(|| CodecError::invalid_input("pts out of range"))?;
        let list = self.session.decode(Some(&data), pts90)?;
        self.take(list)
    }

    fn receive(&mut self) -> Result<Option<Frame>> {
        Ok(self.queue.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        let list = self.session.decode(None, 0)?;
        self.take(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dl::LibrarySearch;
    use crate::error::ErrorKind;

    fn opts(dirs: Vec<std::path::PathBuf>) -> ProbeOptions {
        ProbeOptions {
            libs: LibrarySearch {
                dirs,
                dirs_only: true,
            },
            ..Default::default()
        }
    }

    #[test]
    fn missing_library_is_unavailable() {
        let p = probe(&opts(vec!["/nonexistent".into()]));
        let s = p.status();
        assert!(!s.available);
        assert!(s.detail.contains("libvpl"), "{}", s.detail);
        assert!(p.capabilities().is_empty());
        let cfg = EncoderConfig::Video(VideoEncoderConfig::new(
            Codec::H264,
            640,
            480,
            30,
            1_000_000,
        ));
        assert_eq!(
            p.create_encoder(&cfg).err().unwrap().kind,
            ErrorKind::Unavailable
        );
    }

    #[test]
    fn fake_library_is_unavailable() {
        let dir = std::env::temp_dir().join(format!("senca-vpl-fake-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("libvpl.so.2"), b"not an ELF file").unwrap();
        let p = probe(&opts(vec![dir.clone()]));
        assert!(!p.status().available);
        assert!(p.devices().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 本物の dispatcher（`SENCA_TEST_VPL_DIR`、無ければ開発機の既定の場所）で、GPU が無ければ
    /// 「ハードウェアの実装が無い」と返すこと。ライブラリが無い環境では何もしない。
    #[test]
    fn real_dispatcher_without_gpu() {
        let dir = std::env::var_os("SENCA_TEST_VPL_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                    .join("tools/codeclibs/usr/lib/x86_64-linux-gnu")
            });
        if !dir.join("libvpl.so.2").exists() {
            eprintln!("skip: libvpl.so.2 not found in {}", dir.display());
            return;
        }
        let p = probe(&opts(vec![dir]));
        let s = p.status();
        eprintln!("vpl status: {s:?}");
        if std::path::Path::new("/dev/dri").exists() && s.available {
            // GPU がある機械では使えてよい（能力が並ぶこと）。
            assert!(!p.capabilities().is_empty());
            return;
        }
        assert!(!s.available);
        assert!(
            s.detail.contains("no hardware oneVPL implementation"),
            "{}",
            s.detail
        );
        let cfg = EncoderConfig::Video(VideoEncoderConfig::new(
            Codec::H265,
            640,
            480,
            30,
            1_000_000,
        ));
        assert_eq!(
            p.create_encoder(&cfg).err().unwrap().kind,
            ErrorKind::Unavailable
        );
    }

    #[test]
    fn multiplier() {
        assert_eq!(brc_multiplier(5000, 10000), (1, 5000, 10000));
        let (m, t, x) = brc_multiplier(100_000, 200_000);
        assert_eq!(m, 4);
        assert_eq!((t as u32 * 4, x as u32 * 4), (100_000, 200_000));
        assert_eq!(brc_multiplier(0, 0), (1, 0, 0));
        let (m, t, x) = brc_multiplier(800_000, 800_000);
        assert!(m as u32 * t as u32 >= 800_000 && x == t);
    }

    #[test]
    fn params_mapping() {
        let mut c = VideoEncoderConfig::new(Codec::H264, 1918, 1080, 30, 4_000_000);
        c.profile = Some(VideoProfile::H264High);
        let p = encode_params(&c).unwrap();
        assert_eq!(p.codec_id, sys::MFX_CODEC_AVC);
        assert_eq!(p.profile, 100);
        assert_eq!(
            (p.width, p.height, p.crop_w, p.crop_h),
            (1920, 1088, 1918, 1080)
        );
        assert_eq!(p.rate_control, sys::MFX_RATECONTROL_CBR as u16);
        assert_eq!(
            (p.target_kbps, p.max_kbps, p.brc_multiplier),
            (4000, 4000, 1)
        );
        assert_eq!((p.gop_pic_size, p.gop_ref_dist, p.idr_interval), (60, 1, 0));
        assert_eq!(p.target_usage, sys::MFX_TARGETUSAGE_BEST_SPEED as u16);
        assert!(!p.ten_bit);

        let mut c = VideoEncoderConfig::new(Codec::H265, 1280, 720, 60, 6_000_000);
        c.rate_control = RateControl::Vbr;
        c.low_latency = false;
        c.b_frames = 2;
        c.gop = 0;
        let p = encode_params(&c).unwrap();
        assert_eq!((p.width, p.height), (1280, 736));
        assert_eq!((p.target_kbps, p.max_kbps), (6000, 12000));
        assert_eq!(
            (p.gop_pic_size, p.gop_ref_dist, p.idr_interval),
            (u16::MAX, 3, 1)
        );

        let mut c = VideoEncoderConfig::new(Codec::Av1, 640, 360, 30, 0);
        c.rate_control = RateControl::ConstQp(40);
        let p = encode_params(&c).unwrap();
        assert_eq!(
            (p.rate_control, p.qp),
            (sys::MFX_RATECONTROL_CQP as u16, 160)
        );

        let mut c = VideoEncoderConfig::new(Codec::H265, 640, 360, 30, 1_000_000);
        c.profile = Some(VideoProfile::HevcMain10);
        assert!(encode_params(&c).is_err()); // P010 の入力が要る
        c.input_format = PixelFormat::P010;
        assert!(encode_params(&c).unwrap().ten_bit);
        let c = VideoEncoderConfig::new(Codec::Vp8, 640, 360, 30, 1_000_000);
        assert_eq!(
            encode_params(&c).err().unwrap().kind,
            ErrorKind::Unsupported
        );
    }

    #[test]
    fn codec_ids() {
        for c in [Codec::H264, Codec::H265, Codec::Av1, Codec::Vp9] {
            assert_eq!(codec_of(codec_id(c).unwrap()), Some(c));
        }
        assert_eq!(codec_id(Codec::Opus), None);
        assert_eq!(formats_of(&[]), vec![PixelFormat::Nv12, PixelFormat::I420]);
        assert!(formats_of(&[sys::MFX_FOURCC_P010]).contains(&PixelFormat::P010));
    }

    #[test]
    fn provider_from_fake_impls() {
        // 列挙結果から能力を組み立てる部分（GPU 無しで試せる純粋な部分）。
        let Ok(vpl) = ffi::Vpl::load(&LibrarySearch {
            dirs: vec![
                std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                    .join("tools/codeclibs/usr/lib/x86_64-linux-gnu"),
            ],
            dirs_only: true,
        }) else {
            eprintln!("skip: libvpl not available");
            return;
        };
        let imp = ffi::ImplInfo {
            index: 0,
            name: "mfx-gen".into(),
            api_version: (2, 16),
            vendor_id: 0x8086,
            device_id: "56a0/0".into(),
            decoders: vec![ffi::CodecRange {
                codec_id: sys::MFX_CODEC_VP9,
                max_width: 8192,
                max_height: 8192,
                color_formats: vec![sys::MFX_FOURCC_NV12],
            }],
            encoders: vec![
                ffi::CodecRange {
                    codec_id: sys::MFX_CODEC_AVC,
                    max_width: 4096,
                    max_height: 2304,
                    color_formats: vec![],
                },
                ffi::CodecRange {
                    codec_id: sys::MFX_CODEC_VP9,
                    max_width: 0,
                    max_height: 0,
                    color_formats: vec![],
                },
            ],
        };
        let p = VplProvider::new(vpl, vec![imp]);
        assert!(p.status().available);
        assert_eq!(p.devices()[0].name, "mfx-gen (device 56a0/0)");
        let caps = p.capabilities();
        assert_eq!(caps.len(), 2); // VP9 の符号化は出さない
        assert!(
            caps.iter()
                .all(|c| c.hardware && c.provider == ProviderId::IntelVpl)
        );
        assert_eq!(
            p.pick(Codec::H264, Direction::Encode, 1920, 1080).unwrap(),
            0
        );
        assert_eq!(
            p.pick(Codec::H264, Direction::Encode, 8192, 1080)
                .err()
                .unwrap()
                .kind,
            ErrorKind::Unavailable
        );
        let cfg = EncoderConfig::Audio(crate::config::AudioEncoderConfig::opus(48000, 2, 64000));
        assert_eq!(
            p.create_encoder(&cfg).err().unwrap().kind,
            ErrorKind::Unsupported
        );
    }
}
