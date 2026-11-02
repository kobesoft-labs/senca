//! AMD AMF のプロバイダー。H.264 / H.265 / AV1 の符号化、H.264 / H.265 の復号。
//!
//! - 実行時に `libamfrt64.so.1`（AMD のドライバーに含まれる AMF ランタイム）を読み込み、`AMFInit` で工場を得る。
//!   Linux の AMF は Vulkan の装置で動くので、context は `AMFContext1::InitVulkan(NULL)` で初期化する。
//! - AMF は C++ のインターフェースだが、ヘッダーを C として解釈したときの vtable 構造体（`pVtbl`）で呼ぶ
//!   （bindgen で生成、`bindings/amf.rs`）。プロパティ名・コンポーネント ID の `L"..."` はここで定義する。
//! - 入力はホストメモリの NV12（10 bit は P010）面。I420 はここで NV12 に並べ替える。
//! - 加速の種類が `AMF_ACCEL_HARDWARE` のコンポーネントだけを使う（ADR-0004）。
//! - B フレームは使わない（AMF は DTS を返さないため。`b_frames` は 0 として扱う）。
//! - 出力は oneVPL と同じ変換（[`super::vpl::convert`]）で内部形式（ADR-0003）にする。
//!
//! # 検証の状況
//!
//! 開発機（GPU なし、AMF ランタイムなし）で確かめたのは、ライブラリが無い・壊れているときに「使えない」を返すことと、
//! 純粋な部分（プロパティ名の wchar_t 化、設定値の対応）だけ。ランタイムの初期化・能力の取得・符号化・復号は
//! ヘッダーに合わせて書いたが、実機では未検証（REQ-004 の「未検証」）。

#[allow(unsafe_code)]
mod ffi;

use super::vpl::convert::{InputConverter, OutputConverter};
use crate::bitstream::{h264, h265, nal};
use crate::caps::{Capability, DeviceInfo, Direction, ProviderId, ProviderStatus};
use crate::config::{DecoderConfig, EncoderConfig, RateControl, VideoEncoderConfig, VideoProfile};
use crate::error::{CodecError, ErrorKind, Result};
use crate::frame::{Frame, PixelFormat, VideoFrame};
use crate::provider::{CodecProvider, Decoder, Encoder};
use crate::providers::common::{
    UnavailableProvider, capability, check_input_frame, frame_duration,
};
use crate::registry::ProbeOptions;
use bytes::Bytes;
use ffi::sys;
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase};
use std::collections::VecDeque;
use std::sync::Arc;

/// ASCII の文字列を NUL 終端の wchar_t（Linux では 4 バイト）列にする（コンパイル時）。
pub(crate) const fn widen<const N: usize>(s: &str) -> [i32; N] {
    let b = s.as_bytes();
    assert!(b.len() + 1 == N, "length mismatch");
    let mut out = [0i32; N];
    let mut i = 0;
    while i < b.len() {
        assert!(b[i] < 0x80, "ASCII only");
        out[i] = b[i] as i32;
        i += 1;
    }
    out
}

/// `L"..."` に当たる `&'static [i32]`（NUL 終端）。
macro_rules! w {
    ($s:literal) => {{
        const S: &str = $s;
        const W: [i32; S.len() + 1] = widen::<{ S.len() + 1 }>(S);
        &W
    }};
}

/// ヘッダー（v1.5.3）の `L"..."` の写し。
mod names {
    use super::widen;

    // コンポーネント ID
    pub const ENC_AVC: &[i32] = w!("AMFVideoEncoderVCE_AVC");
    pub const ENC_HEVC: &[i32] = w!("AMFVideoEncoderHW_HEVC");
    pub const ENC_AV1: &[i32] = w!("AMFVideoEncoderHW_AV1");
    pub const DEC_AVC: &[i32] = w!("AMFVideoDecoderUVD_H264_AVC");
    pub const DEC_HEVC: &[i32] = w!("AMFVideoDecoderHW_H265_HEVC");

    // H.264（components/VideoEncoderVCE.h）
    pub const AVC_USAGE: &[i32] = w!("Usage");
    pub const AVC_PROFILE: &[i32] = w!("Profile");
    pub const AVC_FRAMESIZE: &[i32] = w!("FrameSize");
    pub const AVC_FRAMERATE: &[i32] = w!("FrameRate");
    pub const AVC_RATE_CONTROL_METHOD: &[i32] = w!("RateControlMethod");
    pub const AVC_TARGET_BITRATE: &[i32] = w!("TargetBitrate");
    pub const AVC_PEAK_BITRATE: &[i32] = w!("PeakBitrate");
    pub const AVC_QP_I: &[i32] = w!("QPI");
    pub const AVC_QP_P: &[i32] = w!("QPP");
    pub const AVC_B_PIC_PATTERN: &[i32] = w!("BPicturesPattern");
    pub const AVC_IDR_PERIOD: &[i32] = w!("IDRPeriod");
    pub const AVC_LOWLATENCY_MODE: &[i32] = w!("LowLatencyInternal");
    pub const AVC_FORCE_PICTURE_TYPE: &[i32] = w!("ForcePictureType");
    pub const AVC_INSERT_SPS: &[i32] = w!("InsertSPS");
    pub const AVC_INSERT_PPS: &[i32] = w!("InsertPPS");
    pub const AVC_OUTPUT_DATA_TYPE: &[i32] = w!("OutputDataType");
    pub const AVC_CAP_NUM_OF_STREAMS: &[i32] = w!("NumOfStreams");

    // H.265（components/VideoEncoderHEVC.h）
    pub const HEVC_USAGE: &[i32] = w!("HevcUsage");
    pub const HEVC_PROFILE: &[i32] = w!("HevcProfile");
    pub const HEVC_FRAMESIZE: &[i32] = w!("HevcFrameSize");
    pub const HEVC_FRAMERATE: &[i32] = w!("HevcFrameRate");
    pub const HEVC_RATE_CONTROL_METHOD: &[i32] = w!("HevcRateControlMethod");
    pub const HEVC_TARGET_BITRATE: &[i32] = w!("HevcTargetBitrate");
    pub const HEVC_PEAK_BITRATE: &[i32] = w!("HevcPeakBitrate");
    pub const HEVC_QP_I: &[i32] = w!("HevcQP_I");
    pub const HEVC_QP_P: &[i32] = w!("HevcQP_P");
    pub const HEVC_GOP_SIZE: &[i32] = w!("HevcGOPSize");
    pub const HEVC_NUM_GOPS_PER_IDR: &[i32] = w!("HevcGOPSPerIDR");
    pub const HEVC_HEADER_INSERTION_MODE: &[i32] = w!("HevcHeaderInsertionMode");
    pub const HEVC_LOWLATENCY_MODE: &[i32] = w!("LowLatencyInternal");
    pub const HEVC_FORCE_PICTURE_TYPE: &[i32] = w!("HevcForcePictureType");
    pub const HEVC_INSERT_HEADER: &[i32] = w!("HevcInsertHeader");
    pub const HEVC_OUTPUT_DATA_TYPE: &[i32] = w!("HevcOutputDataType");
    pub const HEVC_CAP_NUM_OF_STREAMS: &[i32] = w!("HevcNumOfStreams");

    // AV1（components/VideoEncoderAV1.h）
    pub const AV1_USAGE: &[i32] = w!("Av1Usage");
    pub const AV1_PROFILE: &[i32] = w!("Av1Profile");
    pub const AV1_FRAMESIZE: &[i32] = w!("Av1FrameSize");
    pub const AV1_FRAMERATE: &[i32] = w!("Av1FrameRate");
    pub const AV1_ALIGNMENT_MODE: &[i32] = w!("Av1AlignmentMode");
    pub const AV1_RATE_CONTROL_METHOD: &[i32] = w!("Av1RateControlMethod");
    pub const AV1_TARGET_BITRATE: &[i32] = w!("Av1TargetBitrate");
    pub const AV1_PEAK_BITRATE: &[i32] = w!("Av1PeakBitrate");
    pub const AV1_Q_INDEX_INTRA: &[i32] = w!("Av1QIndex_Intra");
    pub const AV1_Q_INDEX_INTER: &[i32] = w!("Av1QIndex_Inter");
    pub const AV1_GOP_SIZE: &[i32] = w!("Av1GOPSize");
    pub const AV1_HEADER_INSERTION_MODE: &[i32] = w!("Av1HeaderInsertionMode");
    pub const AV1_ENCODING_LATENCY_MODE: &[i32] = w!("Av1EncodingLatencyMode");
    pub const AV1_FORCE_FRAME_TYPE: &[i32] = w!("Av1ForceFrameType");
    pub const AV1_FORCE_INSERT_SEQUENCE_HEADER: &[i32] = w!("Av1ForceInsertSequenceHeader");
    pub const AV1_OUTPUT_FRAME_TYPE: &[i32] = w!("Av1OutputFrameType");
    pub const AV1_CAP_NUM_OF_HW_INSTANCES: &[i32] = w!("Av1CapNumOfHwInstances");

    // デコーダー（components/VideoDecoderUVD.h）
    pub const DEC_CAP_NUM_OF_STREAMS: &[i32] = w!("NumOfStreams");
}

/// 範囲が分からないときの最大解像度。
const DEFAULT_MAX: u32 = 4096;
/// 送り直し・出し切りを待つ回数（1 ms ずつ、約 10 秒）。
const WAIT_RETRIES: u32 = 10_000;

/// 検出する。ランタイム・GPU が無ければ「使えない」を返す（落ちない）。
pub fn probe(options: &ProbeOptions) -> Arc<dyn CodecProvider> {
    let amf = match ffi::Amf::load(&options.libs) {
        Ok(a) => a,
        Err(e) => {
            return UnavailableProvider::arc(
                ProviderId::Amf,
                format!("AMF runtime not loadable: {e}"),
            );
        }
    };
    let ctx = match amf.create_context() {
        Ok(c) => c,
        Err(e) => {
            return UnavailableProvider::arc(
                ProviderId::Amf,
                format!("AMF context init failed: {}", e.message),
            );
        }
    };
    let dev = DeviceInfo {
        provider: ProviderId::Amf,
        index: 0,
        name: "AMD GPU (AMF)".into(),
        driver_version: Some(format!(
            "AMF runtime {}",
            ffi::version_string(amf.version())
        )),
        path: None,
        hardware: true,
    };
    let mut caps = Vec::new();
    let list: [(Codec, Direction, &[i32], &[i32]); 5] = [
        (
            Codec::H264,
            Direction::Encode,
            names::ENC_AVC,
            names::AVC_CAP_NUM_OF_STREAMS,
        ),
        (
            Codec::H265,
            Direction::Encode,
            names::ENC_HEVC,
            names::HEVC_CAP_NUM_OF_STREAMS,
        ),
        (
            Codec::Av1,
            Direction::Encode,
            names::ENC_AV1,
            names::AV1_CAP_NUM_OF_HW_INSTANCES,
        ),
        (
            Codec::H264,
            Direction::Decode,
            names::DEC_AVC,
            names::DEC_CAP_NUM_OF_STREAMS,
        ),
        (
            Codec::H265,
            Direction::Decode,
            names::DEC_HEVC,
            names::DEC_CAP_NUM_OF_STREAMS,
        ),
    ];
    for (codec, dir, id, streams) in list {
        let Ok(comp) = ctx.create_component(id) else {
            continue;
        };
        let Ok(info) = comp.caps(Some(streams)) else {
            continue;
        };
        if !info.hardware {
            continue;
        }
        let max_w = if info.max_width == 0 {
            DEFAULT_MAX
        } else {
            info.max_width
        };
        let max_h = if info.max_height == 0 {
            DEFAULT_MAX
        } else {
            info.max_height
        };
        let formats: &[PixelFormat] = match (codec, dir) {
            (Codec::H264, _) => &[PixelFormat::Nv12, PixelFormat::I420],
            _ => &[PixelFormat::Nv12, PixelFormat::I420, PixelFormat::P010],
        };
        // AV1 の値は「ハードウェアの数」で、同時セッション数ではないので出さない。
        let sessions = if codec == Codec::Av1 {
            None
        } else {
            info.num_streams
        };
        caps.push(capability(
            &dev, codec, dir, max_w, max_h, sessions, formats,
        ));
    }
    drop(ctx);
    if caps.is_empty() {
        return UnavailableProvider::arc(
            ProviderId::Amf,
            format!(
                "AMF runtime {} loaded but no hardware codec component",
                ffi::version_string(amf.version())
            ),
        );
    }
    Arc::new(AmfProvider { amf, dev, caps })
}

struct AmfProvider {
    amf: Arc<ffi::Amf>,
    dev: DeviceInfo,
    caps: Vec<Capability>,
}

impl AmfProvider {
    fn check_cap(&self, codec: Codec, dir: Direction, w: u32, h: u32) -> Result<()> {
        if self.caps.iter().any(|c| {
            c.codec == codec && c.direction == dir && w <= c.max_width && h <= c.max_height
        }) {
            Ok(())
        } else {
            Err(CodecError::unavailable(format!(
                "no AMF hardware {} {} for {w}x{h}",
                codec,
                dir.as_str()
            )))
        }
    }
}

impl CodecProvider for AmfProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Amf
    }

    fn status(&self) -> ProviderStatus {
        ProviderStatus {
            provider: ProviderId::Amf,
            available: true,
            detail: format!(
                "{} (runtime {})",
                self.amf.library_name(),
                ffi::version_string(self.amf.version())
            ),
        }
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        vec![self.dev.clone()]
    }

    fn capabilities(&self) -> Vec<Capability> {
        self.caps.clone()
    }

    fn create_encoder(&self, config: &EncoderConfig) -> Result<Box<dyn Encoder>> {
        let EncoderConfig::Video(cfg) = config else {
            return Err(CodecError::unsupported("AMF provider encodes video only"));
        };
        cfg.validate()?;
        let plan = EncodePlan::new(cfg)?;
        self.check_cap(cfg.codec, Direction::Encode, cfg.width, cfg.height)?;
        let ctx = self.amf.create_context()?;
        let mut comp = ctx.create_component(plan.component)?;
        plan.apply(&mut comp)?;
        comp.init(
            if plan.ten_bit {
                PixelFormat::P010
            } else {
                PixelFormat::Nv12
            },
            cfg.width,
            cfg.height,
        )?;
        Ok(Box::new(AmfEncoder {
            comp,
            ctx,
            out: OutputConverter::new(cfg.codec, cfg.track, cfg.timebase)?,
            plan,
            cfg: cfg.clone(),
            queue: VecDeque::new(),
            force_key: false,
            flushed: false,
        }))
    }

    fn create_decoder(&self, config: &DecoderConfig) -> Result<Box<dyn Decoder>> {
        config.validate()?;
        if !matches!(config.codec, Codec::H264 | Codec::H265) {
            return Err(CodecError::unsupported(format!(
                "AMF provider does not decode {}",
                config.codec
            )));
        }
        let (w, h) = config
            .config
            .as_ref()
            .and_then(InputConverter::config_resolution)
            .unwrap_or((1, 1));
        config.check_resolution(w, h)?;
        self.check_cap(config.codec, Direction::Decode, w, h)?;
        let ctx = self.amf.create_context()?;
        Ok(Box::new(AmfDecoder {
            comp: None,
            ctx,
            input: InputConverter::new(config.codec, config.config.as_ref())?,
            cfg: config.clone(),
            timebase: Timebase::MPEG,
            queue: VecDeque::new(),
            eof: false,
        }))
    }
}

/// 設定値の写し（純粋な計算。試験する）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct EncodePlan {
    codec: Codec,
    component: &'static [i32],
    ten_bit: bool,
    /// 順に設定するプロパティ（Usage を最初に。Usage は他の値の既定をまとめて決めるため）。
    props: Vec<(&'static [i32], PropValue)>,
    /// 失敗しても続けるプロパティ（古いドライバーが知らないもの）。
    optional: Vec<(&'static [i32], PropValue)>,
    force_key: Vec<(&'static [i32], PropValue)>,
    /// 出力のフレーム種別のプロパティと、キーフレームを表す値。
    output_type: (&'static [i32], i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PropValue {
    I64(i64),
    Bool(bool),
    Size(u32, u32),
    Rate(u32, u32),
}

impl EncodePlan {
    fn new(cfg: &VideoEncoderConfig) -> Result<EncodePlan> {
        use PropValue::*;
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
        let target = cfg.bitrate_bps as i64;
        let peak = match cfg.rate_control {
            RateControl::Vbr => cfg.max_bitrate_bps.map_or(target * 2, |m| m as i64),
            _ => target,
        };
        let gop = cfg.gop as i64;
        let ll = cfg.low_latency;
        let plan = match cfg.codec {
            Codec::H264 => {
                let usage = if ll {
                    sys::AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY
                } else {
                    sys::AMF_VIDEO_ENCODER_USAGE_TRANSCODING
                };
                let profile = match cfg.profile {
                    None | Some(VideoProfile::H264High) => sys::AMF_VIDEO_ENCODER_PROFILE_HIGH,
                    Some(VideoProfile::H264Main) => sys::AMF_VIDEO_ENCODER_PROFILE_MAIN,
                    Some(VideoProfile::H264Baseline) => sys::AMF_VIDEO_ENCODER_PROFILE_BASELINE,
                    Some(VideoProfile::H264ConstrainedBaseline) => {
                        sys::AMF_VIDEO_ENCODER_PROFILE_CONSTRAINED_BASELINE
                    }
                    Some(p) => {
                        return Err(CodecError::invalid_config(format!(
                            "profile {p:?} is not H.264"
                        )));
                    }
                };
                let rc = match cfg.rate_control {
                    RateControl::Cbr => sys::AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR,
                    RateControl::Vbr => {
                        sys::AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_PEAK_CONSTRAINED_VBR
                    }
                    RateControl::ConstQp(_) => {
                        sys::AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CONSTANT_QP
                    }
                };
                let mut props = vec![
                    (names::AVC_USAGE, I64(usage as i64)),
                    (names::AVC_PROFILE, I64(profile as i64)),
                    (names::AVC_FRAMESIZE, Size(cfg.width, cfg.height)),
                    (names::AVC_FRAMERATE, Rate(cfg.fps_num, cfg.fps_den)),
                    (names::AVC_RATE_CONTROL_METHOD, I64(rc as i64)),
                    (names::AVC_B_PIC_PATTERN, I64(0)),
                    (names::AVC_IDR_PERIOD, I64(gop)),
                ];
                match cfg.rate_control {
                    RateControl::ConstQp(q) => {
                        let q = q.clamp(0, 51) as i64;
                        props.push((names::AVC_QP_I, I64(q)));
                        props.push((names::AVC_QP_P, I64(q)));
                    }
                    _ => {
                        props.push((names::AVC_TARGET_BITRATE, I64(target)));
                        props.push((names::AVC_PEAK_BITRATE, I64(peak)));
                    }
                }
                EncodePlan {
                    codec: Codec::H264,
                    component: names::ENC_AVC,
                    ten_bit,
                    props,
                    optional: vec![(names::AVC_LOWLATENCY_MODE, Bool(ll))],
                    force_key: vec![
                        (
                            names::AVC_FORCE_PICTURE_TYPE,
                            I64(sys::AMF_VIDEO_ENCODER_PICTURE_TYPE_IDR as i64),
                        ),
                        (names::AVC_INSERT_SPS, Bool(true)),
                        (names::AVC_INSERT_PPS, Bool(true)),
                    ],
                    output_type: (
                        names::AVC_OUTPUT_DATA_TYPE,
                        sys::AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_IDR as i64,
                    ),
                }
            }
            Codec::H265 => {
                let usage = if ll {
                    sys::AMF_VIDEO_ENCODER_HEVC_USAGE_LOW_LATENCY
                } else {
                    sys::AMF_VIDEO_ENCODER_HEVC_USAGE_TRANSCODING
                };
                let profile = match cfg.profile {
                    None | Some(VideoProfile::HevcMain) if !ten_bit => {
                        sys::AMF_VIDEO_ENCODER_HEVC_PROFILE_MAIN
                    }
                    None | Some(VideoProfile::HevcMain) | Some(VideoProfile::HevcMain10) => {
                        sys::AMF_VIDEO_ENCODER_HEVC_PROFILE_MAIN_10
                    }
                    Some(p) => {
                        return Err(CodecError::invalid_config(format!(
                            "profile {p:?} is not H.265"
                        )));
                    }
                };
                let rc = match cfg.rate_control {
                    RateControl::Cbr => sys::AMF_VIDEO_ENCODER_HEVC_RATE_CONTROL_METHOD_CBR,
                    RateControl::Vbr => {
                        sys::AMF_VIDEO_ENCODER_HEVC_RATE_CONTROL_METHOD_PEAK_CONSTRAINED_VBR
                    }
                    RateControl::ConstQp(_) => {
                        sys::AMF_VIDEO_ENCODER_HEVC_RATE_CONTROL_METHOD_CONSTANT_QP
                    }
                };
                let mut props = vec![
                    (names::HEVC_USAGE, I64(usage as i64)),
                    (names::HEVC_PROFILE, I64(profile as i64)),
                    (names::HEVC_FRAMESIZE, Size(cfg.width, cfg.height)),
                    (names::HEVC_FRAMERATE, Rate(cfg.fps_num, cfg.fps_den)),
                    (names::HEVC_RATE_CONTROL_METHOD, I64(rc as i64)),
                    (names::HEVC_GOP_SIZE, I64(gop)),
                    (names::HEVC_NUM_GOPS_PER_IDR, I64(1)),
                    (
                        names::HEVC_HEADER_INSERTION_MODE,
                        I64(sys::AMF_VIDEO_ENCODER_HEVC_HEADER_INSERTION_MODE_IDR_ALIGNED as i64),
                    ),
                ];
                match cfg.rate_control {
                    RateControl::ConstQp(q) => {
                        let q = q.clamp(0, 51) as i64;
                        props.push((names::HEVC_QP_I, I64(q)));
                        props.push((names::HEVC_QP_P, I64(q)));
                    }
                    _ => {
                        props.push((names::HEVC_TARGET_BITRATE, I64(target)));
                        props.push((names::HEVC_PEAK_BITRATE, I64(peak)));
                    }
                }
                EncodePlan {
                    codec: Codec::H265,
                    component: names::ENC_HEVC,
                    ten_bit,
                    props,
                    optional: vec![(names::HEVC_LOWLATENCY_MODE, Bool(ll))],
                    force_key: vec![
                        (
                            names::HEVC_FORCE_PICTURE_TYPE,
                            I64(sys::AMF_VIDEO_ENCODER_HEVC_PICTURE_TYPE_IDR as i64),
                        ),
                        (names::HEVC_INSERT_HEADER, Bool(true)),
                    ],
                    output_type: (
                        names::HEVC_OUTPUT_DATA_TYPE,
                        sys::AMF_VIDEO_ENCODER_HEVC_OUTPUT_DATA_TYPE_IDR as i64,
                    ),
                }
            }
            Codec::Av1 => {
                if ten_bit {
                    return Err(CodecError::invalid_config(
                        "10-bit AV1 encode is not supported by AMF provider",
                    ));
                }
                if let Some(p) = cfg.profile.filter(|p| *p != VideoProfile::Av1Main) {
                    return Err(CodecError::invalid_config(format!(
                        "profile {p:?} is not AV1 Main"
                    )));
                }
                let usage = if ll {
                    sys::AMF_VIDEO_ENCODER_AV1_USAGE_LOW_LATENCY
                } else {
                    sys::AMF_VIDEO_ENCODER_AV1_USAGE_TRANSCODING
                };
                let rc = match cfg.rate_control {
                    RateControl::Cbr => sys::AMF_VIDEO_ENCODER_AV1_RATE_CONTROL_METHOD_CBR,
                    RateControl::Vbr => {
                        sys::AMF_VIDEO_ENCODER_AV1_RATE_CONTROL_METHOD_PEAK_CONSTRAINED_VBR
                    }
                    RateControl::ConstQp(_) => {
                        sys::AMF_VIDEO_ENCODER_AV1_RATE_CONTROL_METHOD_CONSTANT_QP
                    }
                };
                let mut props = vec![
                    (names::AV1_USAGE, I64(usage as i64)),
                    (
                        names::AV1_PROFILE,
                        I64(sys::AMF_VIDEO_ENCODER_AV1_PROFILE_MAIN as i64),
                    ),
                    (names::AV1_FRAMESIZE, Size(cfg.width, cfg.height)),
                    (names::AV1_FRAMERATE, Rate(cfg.fps_num, cfg.fps_den)),
                    (names::AV1_RATE_CONTROL_METHOD, I64(rc as i64)),
                    (names::AV1_GOP_SIZE, I64(gop)),
                    (
                        names::AV1_HEADER_INSERTION_MODE,
                        I64(
                            sys::AMF_VIDEO_ENCODER_AV1_HEADER_INSERTION_MODE_KEY_FRAME_ALIGNED
                                as i64,
                        ),
                    ),
                ];
                match cfg.rate_control {
                    RateControl::ConstQp(q) => {
                        let qi = av1_qindex(q);
                        props.push((names::AV1_Q_INDEX_INTRA, I64(qi)));
                        props.push((names::AV1_Q_INDEX_INTER, I64(qi)));
                    }
                    _ => {
                        props.push((names::AV1_TARGET_BITRATE, I64(target)));
                        props.push((names::AV1_PEAK_BITRATE, I64(peak)));
                    }
                }
                let mut optional = vec![(
                    names::AV1_ALIGNMENT_MODE,
                    I64(sys::AMF_VIDEO_ENCODER_AV1_ALIGNMENT_MODE_NO_RESTRICTIONS as i64),
                )];
                if ll {
                    optional.push((
                        names::AV1_ENCODING_LATENCY_MODE,
                        I64(sys::AMF_VIDEO_ENCODER_AV1_ENCODING_LATENCY_MODE_LOWEST_LATENCY as i64),
                    ));
                }
                EncodePlan {
                    codec: Codec::Av1,
                    component: names::ENC_AV1,
                    ten_bit,
                    props,
                    optional,
                    force_key: vec![
                        (
                            names::AV1_FORCE_FRAME_TYPE,
                            I64(sys::AMF_VIDEO_ENCODER_AV1_FORCE_FRAME_TYPE_KEY as i64),
                        ),
                        (names::AV1_FORCE_INSERT_SEQUENCE_HEADER, Bool(true)),
                    ],
                    output_type: (
                        names::AV1_OUTPUT_FRAME_TYPE,
                        sys::AMF_VIDEO_ENCODER_AV1_OUTPUT_FRAME_TYPE_KEY as i64,
                    ),
                }
            }
            other => {
                return Err(CodecError::unsupported(format!(
                    "AMF provider does not encode {other}"
                )));
            }
        };
        Ok(plan)
    }

    fn apply(&self, comp: &mut ffi::Component) -> Result<()> {
        for (name, v) in &self.props {
            set_prop(comp, name, *v)?;
        }
        for (name, v) in &self.optional {
            let _ = set_prop(comp, name, *v);
        }
        Ok(())
    }
}

fn set_prop(comp: &mut ffi::Component, name: &[i32], v: PropValue) -> Result<()> {
    match v {
        PropValue::I64(x) => comp.set_i64(name, x),
        PropValue::Bool(b) => comp.set_bool(name, b),
        PropValue::Size(w, h) => comp.set_size(name, w, h),
        PropValue::Rate(n, d) => comp.set_rate(name, n, d),
    }
}

/// AV1 の量子化（設定は 0..=63）を AMF の q index（1..=255）に写す。
fn av1_qindex(q: u8) -> i64 {
    (q as i64 * 4).clamp(1, 255)
}

struct AmfEncoder {
    // 解放の順: comp → ctx（フィールドの宣言順に drop される）。
    comp: ffi::Component,
    ctx: ffi::Context,
    out: OutputConverter,
    plan: EncodePlan,
    cfg: VideoEncoderConfig,
    queue: VecDeque<AccessUnit>,
    force_key: bool,
    flushed: bool,
}

impl AmfEncoder {
    /// 出力を取り出す。出し切りが終わったら `true`。
    fn poll(&mut self) -> Result<bool> {
        for _ in 0..64 {
            match self.comp.query_output()? {
                ffi::Output::None => return Ok(false),
                ffi::Output::Eof => return Ok(true),
                ffi::Output::Data(d) => {
                    let bytes = d.buffer_bytes()?;
                    let pts = d.pts();
                    let key = d
                        .get_i64(self.plan.output_type.0)
                        .map(|t| t == self.plan.output_type.1);
                    if let Some(au) =
                        self.out
                            .push(&bytes, pts, pts, frame_duration(&self.cfg), key)?
                    {
                        self.queue.push_back(au);
                    }
                }
            }
        }
        Ok(false)
    }
}

impl Encoder for AmfEncoder {
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
        let want = if self.plan.ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        let conv;
        let f = if f.format == want {
            f
        } else {
            conv = f.convert(want)?;
            &conv
        };
        let mut surf = self.ctx.upload(f, f.pts)?;
        if std::mem::take(&mut self.force_key) || f.keyframe {
            for (name, v) in &self.plan.force_key {
                match v {
                    PropValue::I64(x) => surf.set_i64(name, *x)?,
                    PropValue::Bool(b) => surf.set_bool(name, *b)?,
                    _ => {}
                }
            }
        }
        for _ in 0..WAIT_RETRIES {
            match self.comp.submit_surface(&surf)? {
                ffi::Submit::Accepted => {
                    self.poll()?;
                    return Ok(());
                }
                ffi::Submit::InputFull => {
                    self.poll()?;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
        Err(CodecError::unavailable("AMF encoder input stayed full"))
    }

    fn receive(&mut self) -> Result<Option<AccessUnit>> {
        if self.queue.is_empty() && !self.flushed {
            self.poll()?;
        }
        Ok(self.queue.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        if self.flushed {
            return Ok(());
        }
        self.flushed = true;
        self.comp.drain()?;
        for _ in 0..WAIT_RETRIES {
            if self.poll()? {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Err(CodecError::backend("AMF encoder did not finish draining"))
    }

    fn request_keyframe(&mut self) {
        self.force_key = true;
    }
}

/// Annex B の中の SPS から解像度とビット深度を探す。
fn sps_in_annexb(codec: Codec, data: &[u8]) -> Option<(u32, u32, bool)> {
    for n in nal::split_annexb(data) {
        match codec {
            Codec::H264 if h264::nal_type(n) == h264::NAL_SPS => {
                let i = h264::parse_sps(n).ok()?;
                return Some((i.width, i.height, i.bit_depth_luma > 8));
            }
            Codec::H265 if h265::nal_type(n) == h265::NAL_SPS => {
                let i = h265::parse_sps(n).ok()?;
                return Some((i.width, i.height, i.bit_depth_luma > 8));
            }
            _ => {}
        }
    }
    None
}

struct AmfDecoder {
    // 解放の順: comp → ctx。
    comp: Option<ffi::Component>,
    ctx: ffi::Context,
    input: InputConverter,
    cfg: DecoderConfig,
    timebase: Timebase,
    queue: VecDeque<Frame>,
    eof: bool,
}

impl AmfDecoder {
    fn open(&mut self, w: u32, h: u32, ten_bit: bool) -> Result<()> {
        self.cfg.check_resolution(w, h)?;
        let id = if self.cfg.codec == Codec::H264 {
            names::DEC_AVC
        } else {
            names::DEC_HEVC
        };
        let mut comp = self.ctx.create_component(id)?;
        comp.init(
            if ten_bit {
                PixelFormat::P010
            } else {
                PixelFormat::Nv12
            },
            w,
            h,
        )?;
        self.comp = Some(comp);
        Ok(())
    }

    fn poll(&mut self) -> Result<()> {
        let Some(comp) = self.comp.as_mut() else {
            return Ok(());
        };
        for _ in 0..64 {
            match comp.query_output()? {
                ffi::Output::None => return Ok(()),
                ffi::Output::Eof => {
                    self.eof = true;
                    return Ok(());
                }
                ffi::Output::Data(d) => {
                    let s = d.surface(self.cfg.max_width, self.cfg.max_height)?;
                    let f = VideoFrame::from_packed(
                        s.format,
                        s.width,
                        s.height,
                        Bytes::from(s.data),
                        s.pts,
                        self.timebase,
                    )?;
                    let f = match (self.cfg.output_format, f.format) {
                        (Some(PixelFormat::I420), PixelFormat::Nv12) => {
                            f.convert(PixelFormat::I420)?
                        }
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
            }
        }
        Ok(())
    }
}

impl Decoder for AmfDecoder {
    fn codec(&self) -> Codec {
        self.cfg.codec
    }

    fn set_config(&mut self, config: &CodecConfig) -> Result<()> {
        self.input.set_config(config)
    }

    fn send(&mut self, au: &AccessUnit) -> Result<()> {
        let data = self.input.convert(au)?;
        self.timebase = au.timebase;
        if self.comp.is_none() {
            let found = sps_in_annexb(self.cfg.codec, &data);
            let Some((w, h, ten)) = found else {
                return Err(CodecError::invalid_input(
                    "no SPS before the first frame (needs codec config or in-band SPS)",
                ));
            };
            self.open(w, h, ten)?;
        }
        let buf = self.ctx.upload_buffer(&data, au.pts)?;
        for _ in 0..WAIT_RETRIES {
            let r = self
                .comp
                .as_mut()
                .map_or(Ok(ffi::Submit::Accepted), |c| c.submit_buffer(&buf))?;
            self.poll()?;
            if r == ffi::Submit::Accepted {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Err(CodecError::unavailable("AMF decoder input stayed full"))
    }

    fn receive(&mut self) -> Result<Option<Frame>> {
        if self.queue.is_empty() {
            self.poll()?;
        }
        Ok(self.queue.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        let Some(comp) = self.comp.as_mut() else {
            return Ok(());
        };
        comp.drain()?;
        for _ in 0..WAIT_RETRIES {
            self.poll()?;
            if self.eof {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Err(CodecError::new(
            ErrorKind::Backend,
            "AMF decoder did not finish draining",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dl::LibrarySearch;

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
    fn missing_runtime_is_unavailable() {
        let p = probe(&opts(vec!["/nonexistent".into()]));
        let s = p.status();
        assert!(!s.available);
        assert!(
            s.detail.contains("AMF runtime not loadable"),
            "{}",
            s.detail
        );
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
        let d = DecoderConfig::new(Codec::H265);
        assert_eq!(
            p.create_decoder(&d).err().unwrap().kind,
            ErrorKind::Unavailable
        );
    }

    #[test]
    fn fake_runtime_is_unavailable() {
        let dir = std::env::temp_dir().join(format!("senca-amf-fake-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("libamfrt64.so.1"), b"\x7fELF but truncated").unwrap();
        let p = probe(&opts(vec![dir.clone()]));
        assert!(!p.status().available);
        assert!(p.devices().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn library_without_symbols_is_unavailable() {
        // 実在するが AMF ではないライブラリ（libc）を同じ名前で置く → 記号が無い。
        let dir = std::env::temp_dir().join(format!("senca-amf-nosym-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let libc = [
            "/lib/x86_64-linux-gnu/libc.so.6",
            "/usr/lib/x86_64-linux-gnu/libc.so.6",
            "/lib64/libc.so.6",
        ]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists());
        let Some(libc) = libc else { return };
        std::fs::copy(libc, dir.join("libamfrt64.so.1")).unwrap();
        let p = probe(&opts(vec![dir.clone()]));
        let s = p.status();
        assert!(!s.available);
        assert!(s.detail.contains("AMFQueryVersion"), "{}", s.detail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wide_strings() {
        assert_eq!(
            names::AVC_USAGE,
            &[
                b'U' as i32,
                b's' as i32,
                b'a' as i32,
                b'g' as i32,
                b'e' as i32,
                0
            ]
        );
        assert_eq!(ffi::wide_to_string(names::ENC_AV1), "AMFVideoEncoderHW_AV1");
        assert_eq!(
            ffi::wide_to_string(names::DEC_HEVC),
            "AMFVideoDecoderHW_H265_HEVC"
        );
        for n in [
            names::ENC_AVC,
            names::HEVC_TARGET_BITRATE,
            names::AV1_OUTPUT_FRAME_TYPE,
        ] {
            assert_eq!(n.last(), Some(&0));
            assert!(n[..n.len() - 1].iter().all(|&c| c != 0));
        }
    }

    #[test]
    fn version_and_iids() {
        assert_eq!(
            ffi::AMF_FULL_VERSION,
            (1u64 << 48) | (5u64 << 32) | (3u64 << 16)
        );
        assert_eq!(ffi::version_string(ffi::AMF_FULL_VERSION), "1.5.3.0");
        assert_eq!(ffi::IID_AMF_SURFACE.data1, 0x3075dbe3);
        assert_eq!(ffi::IID_AMF_BUFFER.data48, 0xcb);
    }

    #[test]
    fn plan_h264() {
        let mut c = VideoEncoderConfig::new(Codec::H264, 1280, 720, 30, 3_000_000);
        c.b_frames = 2; // 無視される
        let p = EncodePlan::new(&c).unwrap();
        assert_eq!(p.component, names::ENC_AVC);
        assert_eq!(
            p.props[0],
            (
                names::AVC_USAGE,
                PropValue::I64(sys::AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY as i64)
            )
        );
        assert!(
            p.props
                .contains(&(names::AVC_FRAMESIZE, PropValue::Size(1280, 720)))
        );
        assert!(
            p.props
                .contains(&(names::AVC_B_PIC_PATTERN, PropValue::I64(0)))
        );
        assert!(
            p.props
                .contains(&(names::AVC_TARGET_BITRATE, PropValue::I64(3_000_000)))
        );
        assert!(
            p.props
                .contains(&(names::AVC_IDR_PERIOD, PropValue::I64(60)))
        );
        assert!(!p.ten_bit);

        c.rate_control = RateControl::ConstQp(60);
        let p = EncodePlan::new(&c).unwrap();
        assert!(p.props.contains(&(names::AVC_QP_I, PropValue::I64(51))));
        assert!(!p.props.iter().any(|(n, _)| *n == names::AVC_TARGET_BITRATE));

        c.input_format = PixelFormat::P010;
        assert_eq!(
            EncodePlan::new(&c).err().unwrap().kind,
            ErrorKind::InvalidConfig
        );
    }

    #[test]
    fn plan_hevc_av1() {
        let mut c = VideoEncoderConfig::new(Codec::H265, 1920, 1080, 60, 8_000_000);
        c.rate_control = RateControl::Vbr;
        c.low_latency = false;
        let p = EncodePlan::new(&c).unwrap();
        assert_eq!(
            p.props[0],
            (
                names::HEVC_USAGE,
                PropValue::I64(sys::AMF_VIDEO_ENCODER_HEVC_USAGE_TRANSCODING as i64)
            )
        );
        assert!(
            p.props
                .contains(&(names::HEVC_PEAK_BITRATE, PropValue::I64(16_000_000)))
        );
        assert!(p.props.contains(&(
            names::HEVC_PROFILE,
            PropValue::I64(sys::AMF_VIDEO_ENCODER_HEVC_PROFILE_MAIN as i64)
        )));
        c.input_format = PixelFormat::P010;
        let p = EncodePlan::new(&c).unwrap();
        assert!(p.ten_bit);
        assert!(p.props.contains(&(
            names::HEVC_PROFILE,
            PropValue::I64(sys::AMF_VIDEO_ENCODER_HEVC_PROFILE_MAIN_10 as i64)
        )));

        let mut c = VideoEncoderConfig::new(Codec::Av1, 640, 360, 30, 1_000_000);
        c.rate_control = RateControl::ConstQp(30);
        let p = EncodePlan::new(&c).unwrap();
        assert!(
            p.props
                .contains(&(names::AV1_Q_INDEX_INTRA, PropValue::I64(120)))
        );
        assert_eq!(p.output_type.0, names::AV1_OUTPUT_FRAME_TYPE);
        assert_eq!(av1_qindex(0), 1);
        assert_eq!(av1_qindex(255), 255);
        let c = VideoEncoderConfig::new(Codec::Vp9, 640, 360, 30, 1_000_000);
        assert_eq!(
            EncodePlan::new(&c).err().unwrap().kind,
            ErrorKind::Unsupported
        );
    }

    #[test]
    fn finds_sps() {
        use crate::bitstream::h264::tests::{PPS, SPS_320X240_HIGH};
        let d = nal::nals_to_annexb([SPS_320X240_HIGH, PPS]);
        assert_eq!(sps_in_annexb(Codec::H264, &d), Some((320, 240, false)));
        assert_eq!(sps_in_annexb(Codec::H265, &d), None);
    }
}
