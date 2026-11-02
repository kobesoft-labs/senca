//! VA-API（Intel / AMD、Linux）のプロバイダー。
//!
//! - 検出: DRM の render node（`/dev/dri/renderD128..`、または [`ProbeOptions::drm_nodes`]）ごとに
//!   `vaGetDisplayDRM` → `vaInitialize` → `vaQueryConfigProfiles` / `vaQueryConfigEntrypoints` で能力を調べる。
//!   ライブラリ・装置が無ければ「使えない」を理由付きで返す（落ちない）。
//! - 復号: **H.264**（Constrained Baseline / Main / High、8 bit 4:2:0、プログレッシブ）。ビットストリームの解析・
//!   参照ピクチャーの管理は自前（[`h264`]）で、復号そのものは GPU（`VAEntrypointVLD`）。出力は NV12（希望すれば I420）。
//! - 能力として出すのは、このプロバイダーが実際に作れるもの（H.264 の復号）だけ。装置が対応していても未実装のもの
//!   （H.265・AV1・VP9・VP8 の復号、すべての符号化）は状態の説明（`ProviderStatus::detail`）に「検出・未実装」として書く。
//!
//! # 未実装（TODO）
//!
//! - **符号化**（H.264/H.265/AV1 の `VAEntrypointEncSlice`/`EncSliceLP`）。VA-API の符号化は SPS・PPS・スライスヘッダーを
//!   自分で書いて packed header として渡す必要がある（`senca_core::bits::BitWriter` で書ける）。まだ入れていない。
//! - H.265・AV1・VP9・VP8 の復号（それぞれ別のパラメーター構造体と参照管理が要る）。
//! - フィールド・MBAFF（インターレース）、FMO/ASO、4:2:2/4:4:4、10 bit の H.264。
//!
//! # 検証の状態
//!
//! 開発機には GPU（/dev/dri）が無い。検証したのは、libva の読み込み、装置が無い・装置でないファイルを渡したときに
//! 落ちずに「使えない」を返すこと、自前の H.264 前処理（[`h264`]）が FFmpeg の解析・表示順と一致すること。
//! **GPU での復号（`ffi::DecodeSession` の経路）は未検証。**

// bindgen の定数（`VAProfile_VAProfileH264Main` など）は C の名前のまま照合に使う。
#![allow(non_upper_case_globals)]

#[allow(unsafe_code)]
mod ffi;
pub mod h264;

use self::ffi::sys::*;
use self::ffi::{DecodeSession, Display, VaLib};
use self::h264::{Action, H264Frontend, PictureJob, RefPic, StreamInfo};
use crate::bitstream::h264::parse_avcc;
use crate::bitstream::nal::split_length_prefixed;
use crate::caps::{Capability, DeviceInfo, Direction, ProviderId, ProviderStatus};
use crate::config::{DecoderConfig, EncoderConfig};
use crate::error::{CodecError, Result};
use crate::frame::{Frame, PixelFormat, VideoFrame};
use crate::provider::{CodecProvider, Decoder, Encoder};
use crate::registry::ProbeOptions;
use bytes::Bytes;
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

/// 解像度の上限が分からないときの値。
const FALLBACK_MAX: u32 = 4096;

/// 検出した 1 つの装置。
#[derive(Debug, Clone)]
struct VaDevice {
    info: DeviceInfo,
    path: PathBuf,
    /// H.264 の復号（VLD）に対応するプロファイル。
    h264_decode_profiles: Vec<VAProfile>,
    max_width: u32,
    max_height: u32,
    /// 検出したが未実装の機能（説明用）。
    unimplemented: Vec<String>,
}

/// VA-API のプロバイダー。
#[derive(Debug)]
pub struct VaapiProvider {
    lib: Arc<VaLib>,
    devices: Vec<VaDevice>,
}

/// render node を探す（`/dev/dri/renderD*`、番号順）。
fn render_nodes(options: &ProbeOptions) -> Vec<PathBuf> {
    if let Some(n) = &options.drm_nodes {
        return n.clone();
    }
    let mut v: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("renderD"))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v.truncate(64);
    v
}

fn profile_name(p: VAProfile) -> Option<(&'static str, Codec)> {
    Some(match p {
        VAProfile_VAProfileH264ConstrainedBaseline => ("h264-constrained-baseline", Codec::H264),
        VAProfile_VAProfileH264Main => ("h264-main", Codec::H264),
        VAProfile_VAProfileH264High => ("h264-high", Codec::H264),
        VAProfile_VAProfileHEVCMain => ("hevc-main", Codec::H265),
        VAProfile_VAProfileHEVCMain10 => ("hevc-main10", Codec::H265),
        VAProfile_VAProfileAV1Profile0 => ("av1-profile0", Codec::Av1),
        VAProfile_VAProfileVP9Profile0 => ("vp9-profile0", Codec::Vp9),
        VAProfile_VAProfileVP9Profile2 => ("vp9-profile2", Codec::Vp9),
        VAProfile_VAProfileVP8Version0_3 => ("vp8", Codec::Vp8),
        _ => return None,
    })
}

fn is_h264_decode_profile(p: VAProfile) -> bool {
    matches!(
        p,
        VAProfile_VAProfileH264ConstrainedBaseline
            | VAProfile_VAProfileH264Main
            | VAProfile_VAProfileH264High
    )
}

fn inspect(display: &Display, index: u32, path: &std::path::Path) -> VaDevice {
    let vendor = display.vendor();
    let driver_version = match vendor.rsplit_once(" - ") {
        Some((_, v)) if !v.trim().is_empty() => format!(
            "VA-API {}.{}, driver {}",
            display.major,
            display.minor,
            v.trim()
        ),
        _ => format!("VA-API {}.{}", display.major, display.minor),
    };
    let info = DeviceInfo {
        provider: ProviderId::Vaapi,
        index,
        name: if vendor.is_empty() {
            "VA-API device".to_string()
        } else {
            vendor
        },
        driver_version: Some(driver_version),
        path: Some(path.display().to_string()),
        hardware: true,
    };
    let mut dev = VaDevice {
        info,
        path: path.to_path_buf(),
        h264_decode_profiles: Vec::new(),
        max_width: FALLBACK_MAX,
        max_height: FALLBACK_MAX,
        unimplemented: Vec::new(),
    };
    for p in display.profiles() {
        let Some((name, _codec)) = profile_name(p) else {
            continue;
        };
        for ep in display.entrypoints(p) {
            match ep {
                VAEntrypoint_VAEntrypointVLD if is_h264_decode_profile(p) => {
                    dev.h264_decode_profiles.push(p);
                    let w = display.config_attribute(
                        p,
                        ep,
                        VAConfigAttribType_VAConfigAttribMaxPictureWidth,
                    );
                    let h = display.config_attribute(
                        p,
                        ep,
                        VAConfigAttribType_VAConfigAttribMaxPictureHeight,
                    );
                    if let (Some(w), Some(h)) = (w, h)
                        && w > 0
                        && h > 0
                    {
                        dev.max_width = w.min(16384);
                        dev.max_height = h.min(16384);
                    }
                }
                VAEntrypoint_VAEntrypointVLD => dev.unimplemented.push(format!("{name} decode")),
                VAEntrypoint_VAEntrypointEncSlice | VAEntrypoint_VAEntrypointEncSliceLP => {
                    let s = format!("{name} encode");
                    if !dev.unimplemented.contains(&s) {
                        dev.unimplemented.push(s);
                    }
                }
                _ => {}
            }
        }
    }
    dev
}

/// 検出する。ライブラリ・装置が無ければ「使えない」。
pub fn probe(options: &ProbeOptions) -> Arc<dyn CodecProvider> {
    let lib = match VaLib::load(&options.libs) {
        Ok(l) => l,
        Err(e) => return super::common::UnavailableProvider::arc(ProviderId::Vaapi, e),
    };
    let nodes = render_nodes(options);
    if nodes.is_empty() {
        return super::common::UnavailableProvider::arc(
            ProviderId::Vaapi,
            "no DRM render node (/dev/dri/renderD*)",
        );
    }
    let mut devices = Vec::new();
    let mut errors = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        match Display::open(&lib, node) {
            Ok(d) => devices.push(inspect(&d, i as u32, node)),
            Err(e) => errors.push(e),
        }
    }
    if devices.is_empty() {
        return super::common::UnavailableProvider::arc(ProviderId::Vaapi, errors.join("; "));
    }
    Arc::new(VaapiProvider { lib, devices })
}

impl CodecProvider for VaapiProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Vaapi
    }

    fn status(&self) -> ProviderStatus {
        let mut parts = vec![self.lib.description.clone()];
        for d in &self.devices {
            let mut s = format!("{}: {}", d.path.display(), d.info.name);
            if !d.unimplemented.is_empty() {
                s.push_str(&format!(
                    " (detected but not implemented: {})",
                    d.unimplemented.join(", ")
                ));
            }
            parts.push(s);
        }
        ProviderStatus {
            provider: ProviderId::Vaapi,
            available: true,
            detail: parts.join("; "),
        }
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        self.devices.iter().map(|d| d.info.clone()).collect()
    }

    fn capabilities(&self) -> Vec<Capability> {
        self.devices
            .iter()
            .filter(|d| !d.h264_decode_profiles.is_empty())
            .map(|d| {
                super::common::capability(
                    &d.info,
                    Codec::H264,
                    Direction::Decode,
                    d.max_width,
                    d.max_height,
                    None,
                    &[PixelFormat::Nv12, PixelFormat::I420],
                )
            })
            .collect()
    }

    fn create_encoder(&self, config: &EncoderConfig) -> Result<Box<dyn Encoder>> {
        Err(CodecError::unsupported(format!(
            "VA-API {} encoding is not implemented yet",
            config.codec()
        )))
    }

    fn create_decoder(&self, config: &DecoderConfig) -> Result<Box<dyn Decoder>> {
        config.validate()?;
        if config.codec != Codec::H264 {
            return Err(CodecError::unsupported(format!(
                "VA-API {} decoding is not implemented",
                config.codec
            )));
        }
        if matches!(config.output_format, Some(PixelFormat::P010)) {
            return Err(CodecError::unsupported(
                "VA-API H.264 decoder outputs 8-bit only",
            ));
        }
        let dev = self
            .devices
            .iter()
            .find(|d| !d.h264_decode_profiles.is_empty())
            .ok_or_else(|| CodecError::unavailable("no VA-API device with H.264 decoding"))?;
        let display = Display::open(&self.lib, &dev.path).map_err(CodecError::unavailable)?;
        let max_w = config.max_width.min(dev.max_width);
        let max_h = config.max_height.min(dev.max_height);
        let mut d = VaapiH264Decoder {
            frontend: H264Frontend::new(max_w, max_h),
            session: DecodeSession::new(display),
            profiles: dev.h264_decode_profiles.clone(),
            length_size: 4,
            output_format: config.output_format.unwrap_or(PixelFormat::Nv12),
            info: None,
            timebase: Timebase::MPEG,
            out: VecDeque::new(),
            flushed: false,
        };
        if let Some(c) = &config.config {
            d.set_config(c)?;
        }
        Ok(Box::new(d))
    }
}

/// VA-API の H.264 デコーダー（GPU の経路は未検証）。
#[derive(Debug)]
struct VaapiH264Decoder {
    frontend: H264Frontend,
    session: DecodeSession,
    profiles: Vec<VAProfile>,
    length_size: usize,
    output_format: PixelFormat,
    info: Option<StreamInfo>,
    timebase: Timebase,
    out: VecDeque<Frame>,
    flushed: bool,
}

/// 出力を溜めておける上限（呼び出し側が受け取らないとき）。
const MAX_PENDING_FRAMES: usize = 32;

/// ストリームのプロファイルに使える VA のプロファイル（上位互換も含めて優先順）。
fn va_profile_for(info: &StreamInfo, available: &[VAProfile]) -> Result<VAProfile> {
    let cands: &[VAProfile] = match info.profile_idc {
        66 => &[
            VAProfile_VAProfileH264ConstrainedBaseline,
            VAProfile_VAProfileH264Main,
            VAProfile_VAProfileH264High,
        ],
        77 => &[VAProfile_VAProfileH264Main, VAProfile_VAProfileH264High],
        100 => &[VAProfile_VAProfileH264High],
        p => {
            return Err(CodecError::unsupported(format!(
                "H.264 profile_idc {p} is not supported by VA-API decoding"
            )));
        }
    };
    cands
        .iter()
        .copied()
        .find(|p| available.contains(p))
        .ok_or_else(|| {
            CodecError::unsupported("the VA-API device does not support this H.264 profile")
        })
}

fn va_pic(session: &DecodeSession, r: Option<&RefPic>) -> VAPictureH264 {
    let invalid = VAPictureH264 {
        picture_id: VA_INVALID_SURFACE,
        frame_idx: 0,
        flags: VA_PICTURE_H264_INVALID,
        TopFieldOrderCnt: 0,
        BottomFieldOrderCnt: 0,
        va_reserved: [0; 4],
    };
    let Some(r) = r else { return invalid };
    let Some(s) = r.surface else { return invalid };
    VAPictureH264 {
        picture_id: session.surface_id(s),
        frame_idx: r.frame_idx,
        flags: if r.long_term {
            VA_PICTURE_H264_LONG_TERM_REFERENCE
        } else {
            VA_PICTURE_H264_SHORT_TERM_REFERENCE
        },
        TopFieldOrderCnt: r.top_poc,
        BottomFieldOrderCnt: r.bottom_poc,
        va_reserved: [0; 4],
    }
}

/// ピクチャーの値を VA-API の構造体にする。
fn build_pic_params(session: &DecodeSession, job: &PictureJob) -> VAPictureParameterBufferH264 {
    let sps = &job.sps;
    let pps = &job.pps;
    let mut pp = ffi::zeroed_pic_params();
    pp.CurrPic = VAPictureH264 {
        picture_id: session.surface_id(job.surface),
        frame_idx: job.frame_num,
        flags: if job.is_reference {
            VA_PICTURE_H264_SHORT_TERM_REFERENCE
        } else {
            0
        },
        TopFieldOrderCnt: job.top_poc,
        BottomFieldOrderCnt: job.bottom_poc,
        va_reserved: [0; 4],
    };
    for (i, slot) in pp.ReferenceFrames.iter_mut().enumerate() {
        *slot = va_pic(session, job.refs.get(i));
    }
    pp.picture_width_in_mbs_minus1 = (sps.pic_width_in_mbs - 1) as u16;
    pp.picture_height_in_mbs_minus1 = (sps.frame_height_in_mbs() - 1) as u16;
    pp.bit_depth_luma_minus8 = sps.bit_depth_luma - 8;
    pp.bit_depth_chroma_minus8 = sps.bit_depth_chroma - 8;
    pp.num_ref_frames = sps.max_num_ref_frames as u8;
    let mut seq = _VAPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1 {
        _bindgen_align: [],
        _bitfield_1: __BindgenBitfieldUnit::new([0; 3]),
        __bindgen_padding_0: 0,
    };
    seq.set_chroma_format_idc(sps.chroma_format_idc);
    seq.set_residual_colour_transform_flag(sps.separate_colour_plane as u32);
    seq.set_gaps_in_frame_num_value_allowed_flag(sps.gaps_in_frame_num_allowed as u32);
    seq.set_frame_mbs_only_flag(sps.frame_mbs_only as u32);
    seq.set_mb_adaptive_frame_field_flag(sps.mb_adaptive_frame_field as u32);
    seq.set_direct_8x8_inference_flag(sps.direct_8x8_inference as u32);
    seq.set_MinLumaBiPredSize8x8((sps.level_idc >= 31) as u32);
    seq.set_log2_max_frame_num_minus4(sps.log2_max_frame_num - 4);
    seq.set_pic_order_cnt_type(sps.poc_type);
    seq.set_log2_max_pic_order_cnt_lsb_minus4(sps.log2_max_poc_lsb - 4);
    seq.set_delta_pic_order_always_zero_flag(sps.delta_pic_order_always_zero as u32);
    pp.seq_fields.bits = seq;
    pp.pic_init_qp_minus26 = pps.pic_init_qp_minus26 as i8;
    pp.pic_init_qs_minus26 = pps.pic_init_qs_minus26 as i8;
    pp.chroma_qp_index_offset = pps.chroma_qp_index_offset as i8;
    pp.second_chroma_qp_index_offset = pps.second_chroma_qp_index_offset as i8;
    let mut pf = _VAPictureParameterBufferH264__bindgen_ty_2__bindgen_ty_1 {
        _bindgen_align: [],
        _bitfield_1: __BindgenBitfieldUnit::new([0; 2]),
        __bindgen_padding_0: 0,
    };
    pf.set_entropy_coding_mode_flag(pps.entropy_coding_mode as u32);
    pf.set_weighted_pred_flag(pps.weighted_pred as u32);
    pf.set_weighted_bipred_idc(pps.weighted_bipred_idc);
    pf.set_transform_8x8_mode_flag(pps.transform_8x8_mode as u32);
    pf.set_field_pic_flag(0);
    pf.set_constrained_intra_pred_flag(pps.constrained_intra_pred as u32);
    pf.set_pic_order_present_flag(pps.bottom_field_pic_order_in_frame_present as u32);
    pf.set_deblocking_filter_control_present_flag(pps.deblocking_filter_control_present as u32);
    pf.set_redundant_pic_cnt_present_flag(pps.redundant_pic_cnt_present as u32);
    pf.set_reference_pic_flag(job.is_reference as u32);
    pp.pic_fields.bits = pf;
    pp.frame_num = job.frame_num as u16;
    pp
}

fn build_iq_matrix(job: &PictureJob) -> VAIQMatrixBufferH264 {
    let mut iq = ffi::zeroed_iq_matrix();
    iq.ScalingList4x4 = job.pps.scaling.l4x4;
    // 4:2:0 では 8x8 は輝度のイントラ・インターの 2 つだけ。
    iq.ScalingList8x8 = [job.pps.scaling.l8x8[0], job.pps.scaling.l8x8[1]];
    iq
}

fn build_slice_params(
    session: &DecodeSession,
    s: &h264::SliceJob,
) -> Result<VASliceParameterBufferH264> {
    let h = &s.header;
    let mut sp = ffi::zeroed_slice_params();
    sp.slice_data_size =
        u32::try_from(s.nal.len()).map_err(|_| CodecError::invalid_input("slice too large"))?;
    sp.slice_data_offset = 0;
    sp.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
    sp.slice_data_bit_offset = u16::try_from(h.header_bits)
        .map_err(|_| CodecError::invalid_input("H.264 slice header too long"))?;
    sp.first_mb_in_slice = u16::try_from(h.first_mb_in_slice)
        .map_err(|_| CodecError::invalid_input("first_mb_in_slice too large"))?;
    sp.slice_type = h.slice_type;
    sp.direct_spatial_mv_pred_flag = h.direct_spatial_mv_pred as u8;
    if h.slice_type != h264::slice::SLICE_I {
        sp.num_ref_idx_l0_active_minus1 = h.num_ref_idx_l0_active_minus1 as u8;
    }
    if h.slice_type == h264::slice::SLICE_B {
        sp.num_ref_idx_l1_active_minus1 = h.num_ref_idx_l1_active_minus1 as u8;
    }
    sp.cabac_init_idc = h.cabac_init_idc as u8;
    sp.slice_qp_delta = h.slice_qp_delta as i8;
    sp.disable_deblocking_filter_idc = h.disable_deblocking_filter_idc as u8;
    sp.slice_alpha_c0_offset_div2 = h.slice_alpha_c0_offset_div2 as i8;
    sp.slice_beta_offset_div2 = h.slice_beta_offset_div2 as i8;
    for i in 0..32 {
        sp.RefPicList0[i] = va_pic(session, s.list0.get(i).and_then(|r| r.as_ref()));
        sp.RefPicList1[i] = va_pic(session, s.list1.get(i).and_then(|r| r.as_ref()));
    }
    if let Some(w) = &h.pred_weight {
        sp.luma_log2_weight_denom = w.luma_log2_weight_denom;
        sp.chroma_log2_weight_denom = w.chroma_log2_weight_denom;
        sp.luma_weight_l0_flag = w.luma_weight_flag[0] as u8;
        sp.chroma_weight_l0_flag = w.chroma_weight_flag[0] as u8;
        sp.luma_weight_l1_flag = w.luma_weight_flag[1] as u8;
        sp.chroma_weight_l1_flag = w.chroma_weight_flag[1] as u8;
        sp.luma_weight_l0 = w.luma_weight[0];
        sp.luma_offset_l0 = w.luma_offset[0];
        sp.chroma_weight_l0 = w.chroma_weight[0];
        sp.chroma_offset_l0 = w.chroma_offset[0];
        sp.luma_weight_l1 = w.luma_weight[1];
        sp.luma_offset_l1 = w.luma_offset[1];
        sp.chroma_weight_l1 = w.chroma_weight[1];
        sp.chroma_offset_l1 = w.chroma_offset[1];
    }
    Ok(sp)
}

impl VaapiH264Decoder {
    fn run(&mut self, actions: Vec<Action>) -> Result<()> {
        for a in actions {
            match a {
                Action::Reinit(info) => {
                    let profile = va_profile_for(&info, &self.profiles)?;
                    self.session.configure(
                        profile,
                        info.coded_width,
                        info.coded_height,
                        info.surfaces,
                    )?;
                    self.info = Some(info);
                }
                Action::Submit(job) => {
                    let pic = build_pic_params(&self.session, &job);
                    let iq = build_iq_matrix(&job);
                    let mut slices = Vec::with_capacity(job.slices.len());
                    for s in &job.slices {
                        slices.push((build_slice_params(&self.session, s)?, &s.nal[..]));
                    }
                    self.session.decode_h264(job.surface, &pic, &iq, &slices)?;
                }
                Action::Output(o) => {
                    let info = self
                        .info
                        .as_ref()
                        .ok_or_else(|| CodecError::internal("output before configuration"))?;
                    let (w, h) = (info.display_width(), info.display_height());
                    let data =
                        self.session
                            .read_nv12(o.surface, info.crop[0], info.crop[2], w, h)?;
                    let mut f = VideoFrame::from_packed(
                        PixelFormat::Nv12,
                        w,
                        h,
                        Bytes::from(data),
                        o.pts,
                        self.timebase,
                    )?;
                    f.keyframe = o.keyframe;
                    if self.output_format == PixelFormat::I420 {
                        f = f.convert(PixelFormat::I420)?;
                    }
                    if self.out.len() >= MAX_PENDING_FRAMES {
                        return Err(CodecError::internal(
                            "too many undelivered frames (call receive after each send)",
                        ));
                    }
                    self.out.push_back(Frame::Video(f));
                }
            }
        }
        Ok(())
    }
}

impl Decoder for VaapiH264Decoder {
    fn codec(&self) -> Codec {
        Codec::H264
    }

    fn set_config(&mut self, config: &CodecConfig) -> Result<()> {
        if config.codec != Codec::H264 {
            return Err(CodecError::invalid_config("codec config is not H.264"));
        }
        if config.record.is_empty() {
            return Ok(());
        }
        let rec = parse_avcc(&config.record)?;
        self.length_size = rec.length_size;
        for ps in rec.sps.iter().chain(rec.pps.iter()) {
            self.frontend.add_parameter_set(ps)?;
        }
        Ok(())
    }

    fn send(&mut self, au: &AccessUnit) -> Result<()> {
        if self.flushed {
            return Err(CodecError::internal("send after flush"));
        }
        if au.codec != Codec::H264 {
            return Err(CodecError::invalid_input("access unit is not H.264"));
        }
        let nals: Vec<Bytes> = split_length_prefixed(&au.payload, self.length_size)?
            .into_iter()
            .map(|n| au.payload.slice_ref(n))
            .collect();
        self.timebase = au.timebase;
        let actions = self.frontend.decode_au(&nals, au.pts, au.keyframe)?;
        self.run(actions)
    }

    fn receive(&mut self) -> Result<Option<Frame>> {
        Ok(self.out.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        if self.flushed {
            return Ok(());
        }
        self.flushed = true;
        let actions = self.frontend.flush();
        self.run(actions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dl::LibrarySearch;

    const CODECLIBS: &str = "/home/cc/tools/codeclibs/usr/lib/x86_64-linux-gnu";

    fn libs_dir() -> Option<PathBuf> {
        if let Some(d) = std::env::var_os(crate::dl::LIB_DIR_ENV) {
            return std::env::split_paths(&d).find(|p| p.join("libva.so.2").exists());
        }
        let p = PathBuf::from(CODECLIBS);
        p.join("libva.so.2").exists().then_some(p)
    }

    #[test]
    fn missing_library_is_unavailable() {
        let o = ProbeOptions {
            libs: LibrarySearch {
                dirs: vec!["/nonexistent".into()],
                dirs_only: true,
            },
            ..Default::default()
        };
        let p = probe(&o);
        let s = p.status();
        assert!(!s.available);
        assert!(s.detail.contains("libva"), "{}", s.detail);
        assert!(p.capabilities().is_empty());
        assert_eq!(
            p.create_decoder(&DecoderConfig::new(Codec::H264))
                .err()
                .unwrap()
                .kind,
            crate::ErrorKind::Unavailable
        );
    }

    #[test]
    fn no_render_node_is_unavailable() {
        let Some(dir) = libs_dir() else {
            eprintln!("libva not found; skipped");
            return;
        };
        let o = ProbeOptions {
            libs: LibrarySearch {
                dirs: vec![dir],
                dirs_only: true,
            },
            drm_nodes: Some(vec![]),
            ..Default::default()
        };
        let s = probe(&o).status();
        assert!(!s.available);
        assert!(s.detail.contains("no DRM render node"), "{}", s.detail);

        // 存在しない render node
        let o = ProbeOptions {
            drm_nodes: Some(vec!["/nonexistent/renderD128".into()]),
            ..o
        };
        let s = probe(&o).status();
        assert!(!s.available);
        assert!(s.detail.contains("renderD128"), "{}", s.detail);
    }

    #[test]
    fn regular_file_as_render_node_fails_cleanly() {
        let Some(dir) = libs_dir() else {
            eprintln!("libva not found; skipped");
            return;
        };
        let tmp =
            std::env::temp_dir().join(format!("senca-vaapi-fake-node-{}", std::process::id()));
        std::fs::write(&tmp, b"not a drm device").unwrap();
        let o = ProbeOptions {
            libs: LibrarySearch {
                dirs: vec![dir],
                dirs_only: true,
            },
            drm_nodes: Some(vec![tmp.clone(), "/dev/null".into()]),
            ..Default::default()
        };
        let p = probe(&o);
        let _ = std::fs::remove_file(&tmp);
        let s = p.status();
        assert!(!s.available, "{}", s.detail);
        eprintln!("{}", s.detail);
        assert!(
            s.detail.contains("vaGetDisplayDRM") || s.detail.contains("vaInitialize"),
            "{}",
            s.detail
        );
        assert!(p.capabilities().is_empty());
    }

    #[test]
    fn profile_mapping() {
        let mut info = StreamInfo {
            profile_idc: 66,
            constraint_flags: 0xc0,
            coded_width: 176,
            coded_height: 144,
            crop: [0; 4],
            bit_depth: 8,
            chroma_format_idc: 1,
            dpb_size: 4,
            surfaces: 6,
        };
        let all = [VAProfile_VAProfileH264Main, VAProfile_VAProfileH264High];
        assert_eq!(
            va_profile_for(&info, &all).unwrap(),
            VAProfile_VAProfileH264Main
        );
        info.profile_idc = 100;
        assert_eq!(
            va_profile_for(&info, &all).unwrap(),
            VAProfile_VAProfileH264High
        );
        assert!(va_profile_for(&info, &[VAProfile_VAProfileH264Main]).is_err());
        info.profile_idc = 110;
        assert_eq!(
            va_profile_for(&info, &all).unwrap_err().kind,
            crate::ErrorKind::Unsupported
        );
    }
}
