//! NVIDIA NVENC（符号化: H.264 / H.265 / AV1）と NVDEC（復号: H.264 / H.265 / AV1 / VP9）。
//!
//! - ライブラリ: `libcuda.so.1`（CUDA ドライバー API）、`libnvidia-encode.so.1`（NVENC）、`libnvcuvid.so.1`（NVDEC）、
//!   `libnvidia-ml.so.1`（NVML、ドライバーの版の表示だけ。無くてもよい）。どれも実行時に dlopen する。
//! - API の版: nv-codec-headers の NVENCAPI 13.1（Video Codec SDK 13.1、Linux ドライバー 610 以上）。
//!   ドライバーが古いときは NVENC を「使えない」とし、理由を出す（NVDEC は使える）。
//! - 入力はシステムメモリの NV12（I420 はここで並べ替える）または P010（H.265 / AV1 の 10 bit）。
//!   出力は NV12（希望があれば I420 に並べ替える）または P010（10 bit のストリーム、NVDEC の P016 と同じ配置）。
//! - 同時セッション数は API から分からないので `None`。消費者向け GPU ではドライバーが NVENC の同時セッション数を
//!   制限する（版により 3〜8）。上限に当たると `nvEncOpenEncodeSessionEx` が失敗し、`Unavailable` として返す
//!   （レジストリが次の候補を試す）。
//!
//! # 検証の状況
//!
//! 開発機（x86_64、GPU なし）で確かめたのは、ライブラリが無い・壊れているときの検出（落ちずに「使えない」）、
//! 生成した型の配置（コンパイル時の検査）、純粋な変換（パラメーターの対応、ピッチ付きの複製、
//! Annex B ⇔ 長さ前置、avcC/hvcC/av1C の組み立て）だけ。**GPU 上での符号化・復号は未検証**
//! （REQ-004: 実機の型番・ドライバー版を相互接続表に記録するまで「未検証」と表示する）。

#[allow(unsafe_code)]
mod ffi;

use super::common::{
    AuStamper, UnavailableProvider, capability, check_input_frame, frame_duration,
};
use crate::bitstream::{av1, h264, h265, nal};
use crate::caps::{Capability, DeviceInfo, Direction, ProviderId, ProviderStatus};
use crate::config::{DecoderConfig, EncoderConfig, RateControl, VideoEncoderConfig, VideoProfile};
use crate::error::{CodecError, ErrorKind, Result};
use crate::frame::{Frame, PixelFormat, VideoFrame};
use crate::provider::{CodecProvider, Decoder, Encoder};
use crate::registry::ProbeOptions;
use bytes::Bytes;
use senca_core::media::VideoParams;
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase};
use std::collections::VecDeque;
use std::sync::Arc;

/// 1 つの出力・入力パケットの上限（64 MiB）。
const MAX_PACKET_BYTES: usize = 64 << 20;
/// 符号化を依頼して出力を待っているフレーム数の上限。
const MAX_PENDING_FRAMES: usize = 64;

/// NVIDIA のプロバイダーを検出する。ライブラリ・GPU が無ければ「使えない」を返す（落ちない）。
pub fn probe(options: &ProbeOptions) -> Arc<dyn CodecProvider> {
    match ffi::probe(&options.libs) {
        Err(reason) => UnavailableProvider::arc(ProviderId::Nvidia, reason),
        Ok(p) if p.devices.is_empty() => {
            UnavailableProvider::arc(ProviderId::Nvidia, "no CUDA device found")
        }
        Ok(p) => Arc::new(NvidiaProvider::new(p)),
    }
}

/// 検出できた NVIDIA のプロバイダー。
pub struct NvidiaProvider {
    libs: Arc<ffi::NvLibs>,
    devices: Vec<ffi::ProbedDevice>,
    infos: Vec<DeviceInfo>,
    caps: Vec<Capability>,
    detail: String,
}

impl NvidiaProvider {
    fn new(p: ffi::Probe) -> Self {
        let mut infos = Vec::new();
        let mut caps = Vec::new();
        let mut notes = Vec::new();
        for d in &p.devices {
            let info = DeviceInfo {
                provider: ProviderId::Nvidia,
                index: d.index,
                name: d.name.clone(),
                driver_version: Some(p.libs.driver_version.clone()),
                path: None,
                hardware: true,
            };
            caps.extend(device_capabilities(&info, d));
            notes.extend(d.notes.iter().map(|n| format!("gpu{}: {n}", d.index)));
            infos.push(info);
        }
        let detail = if caps.is_empty() {
            format!("no usable NVENC/NVDEC: {}", notes.join("; "))
        } else if notes.is_empty() {
            format!("{} GPU(s), driver {}", infos.len(), p.libs.driver_version)
        } else {
            format!(
                "{} GPU(s), driver {} ({})",
                infos.len(),
                p.libs.driver_version,
                notes.join("; ")
            )
        };
        NvidiaProvider {
            libs: p.libs,
            devices: p.devices,
            infos,
            caps,
            detail,
        }
    }
}

/// 検出結果を能力の一覧にする。
fn device_capabilities(info: &DeviceInfo, d: &ffi::ProbedDevice) -> Vec<Capability> {
    let mut out = Vec::new();
    for e in &d.encode {
        let mut formats = vec![PixelFormat::I420, PixelFormat::Nv12];
        if e.ten_bit && e.codec != Codec::H264 {
            formats.push(PixelFormat::P010);
        }
        out.push(capability(
            info,
            e.codec,
            Direction::Encode,
            e.max_width,
            e.max_height,
            None,
            &formats,
        ));
    }
    for codec in [Codec::H264, Codec::H265, Codec::Av1, Codec::Vp9] {
        let caps: Vec<_> = d.decode.iter().filter(|c| c.codec == codec).collect();
        let Some(first) = caps.iter().find(|c| c.bit_depth == 8).or(caps.first()) else {
            continue;
        };
        let mut formats = vec![PixelFormat::Nv12, PixelFormat::I420];
        if caps.iter().any(|c| c.bit_depth == 10) {
            formats.push(PixelFormat::P010);
        }
        out.push(capability(
            info,
            codec,
            Direction::Decode,
            first.max_width,
            first.max_height,
            None,
            &formats,
        ));
    }
    out
}

impl CodecProvider for NvidiaProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Nvidia
    }

    fn status(&self) -> ProviderStatus {
        ProviderStatus {
            provider: ProviderId::Nvidia,
            available: !self.caps.is_empty(),
            detail: self.detail.clone(),
        }
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        self.infos.clone()
    }

    fn capabilities(&self) -> Vec<Capability> {
        self.caps.clone()
    }

    fn create_encoder(&self, config: &EncoderConfig) -> Result<Box<dyn Encoder>> {
        let EncoderConfig::Video(cfg) = config else {
            return Err(CodecError::unsupported("NVENC encodes video only"));
        };
        cfg.validate()?;
        let params = encode_params(cfg)?;
        let candidates: Vec<u32> = self
            .devices
            .iter()
            .filter(|d| {
                d.encode.iter().any(|e| {
                    e.codec == cfg.codec
                        && cfg.width <= e.max_width
                        && cfg.height <= e.max_height
                        && (!params.ten_bit || e.ten_bit)
                })
            })
            .map(|d| d.index)
            .collect();
        if candidates.is_empty() {
            return Err(CodecError::unavailable(format!(
                "no NVIDIA GPU can encode {} {}x{}{}",
                cfg.codec,
                cfg.width,
                cfg.height,
                if params.ten_bit { " 10-bit" } else { "" }
            )));
        }
        let mut last = None;
        for dev in candidates {
            match ffi::NvencSession::open(&self.libs, dev, &params) {
                Ok(s) => return Ok(Box::new(NvencEncoder::new(s, cfg.clone(), params.ten_bit))),
                Err(e) if e.kind == ErrorKind::Unavailable => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| CodecError::unavailable("NVENC session unavailable")))
    }

    fn create_decoder(&self, config: &DecoderConfig) -> Result<Box<dyn Decoder>> {
        config.validate()?;
        if !matches!(
            config.codec,
            Codec::H264 | Codec::H265 | Codec::Av1 | Codec::Vp9
        ) {
            return Err(CodecError::unsupported(format!(
                "NVDEC does not decode {}",
                config.codec
            )));
        }
        let (length_size, prefix) = match &config.config {
            Some(c) => record_prefix(config.codec, &c.record)?,
            None => (4, Vec::new()),
        };
        let res = config
            .config
            .as_ref()
            .and_then(|c| c.video)
            .map(|v| (v.width, v.height));
        let candidates: Vec<u32> = self
            .devices
            .iter()
            .filter(|d| {
                d.decode.iter().any(|c| {
                    c.codec == config.codec
                        && res.is_none_or(|(w, h)| w <= c.max_width && h <= c.max_height)
                })
            })
            .map(|d| d.index)
            .collect();
        let params = ffi::DecodeParams {
            codec: config.codec,
            max_width: config.max_width,
            max_height: config.max_height,
        };
        let mut last = None;
        for dev in candidates {
            match ffi::CuvidSession::open(&self.libs, dev, params) {
                Ok(session) => {
                    return Ok(Box::new(NvdecDecoder {
                        session,
                        codec: config.codec,
                        length_size,
                        send_prefix: !prefix.is_empty(),
                        prefix,
                        timebase: Timebase::MPEG,
                        output_format: config.output_format,
                        flushed: false,
                    }));
                }
                Err(e) if e.kind == ErrorKind::Unavailable => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| {
            CodecError::unavailable(format!("no NVIDIA GPU can decode {}", config.codec))
        }))
    }
}

// ---------------------------------------------------------------------------------------------
// 設定の対応（純粋な関数）
// ---------------------------------------------------------------------------------------------

/// `VideoEncoderConfig` を NVENC の設定にする。
fn encode_params(cfg: &VideoEncoderConfig) -> Result<ffi::EncodeParams> {
    if !matches!(cfg.codec, Codec::H264 | Codec::H265 | Codec::Av1) {
        return Err(CodecError::unsupported(format!(
            "NVENC does not encode {}",
            cfg.codec
        )));
    }
    if !cfg.width.is_multiple_of(2) || !cfg.height.is_multiple_of(2) {
        return Err(CodecError::invalid_config(
            "NVENC 4:2:0 input needs even width and height",
        ));
    }
    if cfg.b_frames > 4 {
        return Err(CodecError::invalid_config(
            "NVENC supports at most 4 b-frames",
        ));
    }
    let ten_bit = cfg.input_format == PixelFormat::P010;
    if ten_bit && cfg.codec == Codec::H264 {
        return Err(CodecError::unsupported(
            "10-bit H.264 encoding is not supported (use H.265 or AV1)",
        ));
    }
    let profile = match (cfg.profile, cfg.codec) {
        (None, Codec::H265) if ten_bit => ffi::ProfileSel::HevcMain10,
        (None, Codec::Av1) => ffi::ProfileSel::Av1Main,
        (None, _) => ffi::ProfileSel::Auto,
        (Some(VideoProfile::H264Baseline | VideoProfile::H264ConstrainedBaseline), _) => {
            ffi::ProfileSel::H264Baseline
        }
        (Some(VideoProfile::H264Main), _) => ffi::ProfileSel::H264Main,
        (Some(VideoProfile::H264High), _) => ffi::ProfileSel::H264High,
        (Some(VideoProfile::HevcMain), _) if ten_bit => {
            return Err(CodecError::invalid_config(
                "HEVC Main cannot carry 10-bit input (use Main10)",
            ));
        }
        (Some(VideoProfile::HevcMain), _) => ffi::ProfileSel::HevcMain,
        (Some(VideoProfile::HevcMain10), _) => ffi::ProfileSel::HevcMain10,
        (Some(VideoProfile::Av1Main), _) => ffi::ProfileSel::Av1Main,
        (Some(p), _) => {
            return Err(CodecError::invalid_config(format!(
                "profile {p:?} is not supported by NVENC"
            )));
        }
    };
    let max_qp = if cfg.codec == Codec::Av1 { 255 } else { 51 };
    let rc = match cfg.rate_control {
        RateControl::Cbr => ffi::RcMode::Cbr,
        RateControl::Vbr => ffi::RcMode::Vbr,
        RateControl::ConstQp(q) => ffi::RcMode::ConstQp(q.min(max_qp)),
    };
    let max_bitrate = cfg
        .max_bitrate_bps
        .unwrap_or(cfg.bitrate_bps.saturating_mul(2))
        .max(cfg.bitrate_bps);
    // 低遅延なら VBV を 1 フレーム分にする（フレームごとの符号量の揺れを抑える）。
    let vbv_size = if cfg.low_latency && !matches!(rc, ffi::RcMode::ConstQp(_)) {
        let v = cfg.bitrate_bps as u64 * cfg.fps_den as u64 / cfg.fps_num.max(1) as u64;
        u32::try_from(v).unwrap_or(u32::MAX).max(1)
    } else {
        0
    };
    Ok(ffi::EncodeParams {
        codec: cfg.codec,
        width: cfg.width,
        height: cfg.height,
        fps_num: cfg.fps_num,
        fps_den: cfg.fps_den,
        bitrate: cfg.bitrate_bps,
        max_bitrate,
        vbv_size,
        gop: cfg.gop,
        rc,
        profile,
        low_latency: cfg.low_latency,
        b_frames: cfg.b_frames,
        ten_bit,
    })
}

/// NVENC の入力バッファ（NV12 / P010、輝度 pitch*height の後に色差 pitch*ceil(height/2)）の大きさ。
fn input_buffer_len(format: PixelFormat, pitch: usize, height: u32) -> Option<usize> {
    if !matches!(format, PixelFormat::Nv12 | PixelFormat::P010) || pitch == 0 {
        return None;
    }
    let h = height as usize;
    pitch
        .checked_mul(h)?
        .checked_add(pitch.checked_mul(h.div_ceil(2))?)
}

/// フレームを pitch 付きの入力バッファへ写す（`alloc_height` は輝度面の行数）。
fn copy_frame_to_pitched(
    frame: &VideoFrame,
    dst: &mut [u8],
    pitch: usize,
    alloc_height: usize,
) -> Result<()> {
    frame.validate()?;
    if frame.format.plane_count() != 2 {
        return Err(CodecError::invalid_input(
            "NVENC input must be NV12 or P010",
        ));
    }
    let mut base = 0usize;
    for plane in 0..2 {
        let (row, rows) = frame
            .format
            .plane_size(frame.width, frame.height, plane)
            .expect("plane in range");
        if row > pitch {
            return Err(CodecError::invalid_input("pitch smaller than a row"));
        }
        for y in 0..rows {
            let off = base + y * pitch;
            let d = dst
                .get_mut(off..off + row)
                .ok_or_else(|| CodecError::invalid_input("input buffer too small"))?;
            d.copy_from_slice(frame.row(plane, y));
        }
        base += pitch * alloc_height;
    }
    Ok(())
}

/// 表示領域（left, top, right, bottom）の幅と高さ。
fn display_size(left: i32, top: i32, right: i32, bottom: i32) -> Option<(u32, u32)> {
    let w = right.checked_sub(left)?;
    let h = bottom.checked_sub(top)?;
    (left >= 0 && top >= 0 && w > 0 && h > 0).then_some((w as u32, h as u32))
}

/// 内部形式に直したパケットと、パラメーターセットがあればトラックの設定。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Packaged {
    payload: Vec<u8>,
    config: Option<(Bytes, VideoParams)>,
}

/// エンコーダーの出力（Annex B / OBU 列）を内部形式（ADR-0003）にする。
fn package(codec: Codec, data: &[u8]) -> Result<Packaged> {
    match codec {
        Codec::H264 => {
            let (sps, pps, rest) = h264::split_parameter_sets(data);
            let config = if !sps.is_empty() && !pps.is_empty() {
                Some((h264::build_avcc(&sps, &pps)?, h264::video_params(sps[0])?))
            } else {
                None
            };
            Ok(Packaged {
                payload: nal::to_length_prefixed(rest),
                config,
            })
        }
        Codec::H265 => {
            let ps = h265::split_parameter_sets(data);
            let config = if !ps.vps.is_empty() && !ps.sps.is_empty() && !ps.pps.is_empty() {
                Some((
                    h265::build_hvcc(&ps.vps, &ps.sps, &ps.pps)?,
                    h265::video_params(ps.sps[0])?,
                ))
            } else {
                None
            };
            Ok(Packaged {
                payload: nal::to_length_prefixed(ps.rest),
                config,
            })
        }
        Codec::Av1 => {
            let payload = av1::to_internal(data)?;
            let config = match av1::find_sequence_header(&payload)? {
                Some(seq) => {
                    let (rec, sh) = av1::build_av1c(seq)?;
                    Some((rec, av1::video_params(&sh)))
                }
                None => None,
            };
            Ok(Packaged { payload, config })
        }
        _ => Err(CodecError::unsupported(format!(
            "NVENC does not encode {codec}"
        ))),
    }
}

/// トラックの設定（avcC・hvcC・av1C）から、長さ前置のバイト数と、デコーダーへ先に渡すデータを作る。
fn record_prefix(codec: Codec, record: &[u8]) -> Result<(usize, Vec<u8>)> {
    if record.is_empty() {
        return Ok((4, Vec::new()));
    }
    match codec {
        Codec::H264 => {
            let r = h264::parse_avcc(record)?;
            Ok((
                r.length_size,
                nal::nals_to_annexb(r.sps.iter().chain(&r.pps).map(|b| b.as_ref())),
            ))
        }
        Codec::H265 => {
            let r = h265::parse_hvcc(record)?;
            Ok((
                r.length_size,
                nal::nals_to_annexb(r.vps.iter().chain(&r.sps).chain(&r.pps).map(|b| b.as_ref())),
            ))
        }
        Codec::Av1 => Ok((4, av1::to_internal(av1::parse_av1c(record)?)?)),
        _ => Ok((4, Vec::new())),
    }
}

/// 内部形式の AU を NVDEC のパーサーに渡す形にする（H.264/H.265 は Annex B、AV1 は Temporal Unit）。
fn nvdec_input(codec: Codec, payload: &[u8], length_size: usize, prefix: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > MAX_PACKET_BYTES {
        return Err(CodecError::invalid_input("access unit too large"));
    }
    match codec {
        Codec::H264 | Codec::H265 => {
            let body = nal::length_prefixed_to_annexb(payload, length_size)?;
            let mut v = Vec::with_capacity(prefix.len() + body.len());
            v.extend_from_slice(prefix);
            v.extend_from_slice(&body);
            Ok(v)
        }
        Codec::Av1 => {
            av1::parse_obus(payload)?;
            let mut v = Vec::with_capacity(prefix.len() + payload.len());
            v.extend_from_slice(prefix);
            v.extend_from_slice(payload);
            Ok(av1::with_temporal_delimiter(&v))
        }
        Codec::Vp9 => Ok(payload.to_vec()),
        _ => Err(CodecError::unsupported(format!(
            "NVDEC does not decode {codec}"
        ))),
    }
}

// ---------------------------------------------------------------------------------------------
// エンコーダー
// ---------------------------------------------------------------------------------------------

struct NvencEncoder {
    session: ffi::NvencSession,
    cfg: VideoEncoderConfig,
    ten_bit: bool,
    stamper: AuStamper,
    out: VecDeque<AccessUnit>,
    /// 符号化を依頼したフレームの pts（昇順）。DTS を作るのに使う。
    pending_pts: Vec<i64>,
    duration: u64,
    force_idr: bool,
    flushed: bool,
}

impl NvencEncoder {
    fn new(mut session: ffi::NvencSession, cfg: VideoEncoderConfig, ten_bit: bool) -> Self {
        let mut stamper = AuStamper::new(cfg.codec, cfg.track, cfg.timebase);
        // 最初の AU の前に設定が分かるよう、パラメーターセットを先に取り出す（失敗しても最初のキーフレームで分かる）。
        if let Ok(seq) = session.sequence_params()
            && let Ok(Packaged {
                config: Some((rec, vp)),
                ..
            }) = package(cfg.codec, &seq)
        {
            stamper.set_config(rec, Some(vp), None);
        }
        let duration = frame_duration(&cfg);
        NvencEncoder {
            session,
            cfg,
            ten_bit,
            stamper,
            out: VecDeque::new(),
            pending_pts: Vec::new(),
            duration,
            force_idr: false,
            flushed: false,
        }
    }

    fn handle(&mut self, packets: Vec<ffi::EncodedPacket>) -> Result<()> {
        for p in packets {
            let pk = package(self.cfg.codec, &p.data)?;
            if let Some((rec, vp)) = pk.config {
                self.stamper.set_config(rec, Some(vp), None);
            }
            // 復号順の i 番目の DTS = 入力 pts の小さい方から i 番目 - 並べ替えの遅れ（B フレーム数）。
            let base = if self.pending_pts.is_empty() {
                p.pts
            } else {
                self.pending_pts.remove(0)
            };
            let delay = self.cfg.b_frames.min(1) as i64 * self.duration as i64;
            let dts = base.saturating_sub(delay).min(p.pts);
            if pk.payload.is_empty() {
                continue;
            }
            let au = self.stamper.stamp(
                Bytes::from(pk.payload),
                p.pts,
                dts,
                self.duration,
                p.keyframe,
            );
            self.out.push_back(au);
        }
        Ok(())
    }
}

impl Encoder for NvencEncoder {
    fn codec(&self) -> Codec {
        self.cfg.codec
    }

    fn codec_config(&self) -> Option<CodecConfig> {
        self.stamper.config()
    }

    fn send(&mut self, frame: &Frame) -> Result<()> {
        if self.flushed {
            return Err(CodecError::internal("encoder already flushed"));
        }
        let Frame::Video(f) = frame else {
            return Err(CodecError::invalid_input("NVENC needs video frames"));
        };
        check_input_frame(&self.cfg, f)?;
        let input = if self.ten_bit {
            if f.format != PixelFormat::P010 {
                return Err(CodecError::invalid_input(
                    "10-bit session needs P010 frames",
                ));
            }
            f.clone()
        } else {
            f.convert(PixelFormat::Nv12)?
        };
        if self.pending_pts.len() >= MAX_PENDING_FRAMES {
            return Err(CodecError::internal("too many frames pending in NVENC"));
        }
        let pos = self.pending_pts.partition_point(|&x| x <= f.pts);
        self.pending_pts.insert(pos, f.pts);
        let force = std::mem::take(&mut self.force_idr) || f.keyframe;
        match self.session.encode(&input, f.pts, force) {
            Ok(packets) => self.handle(packets),
            Err(e) => {
                if let Some(i) = self.pending_pts.iter().position(|&x| x == f.pts) {
                    self.pending_pts.remove(i);
                }
                Err(e)
            }
        }
    }

    fn receive(&mut self) -> Result<Option<AccessUnit>> {
        Ok(self.out.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        if self.flushed {
            return Ok(());
        }
        self.flushed = true;
        let packets = self.session.flush()?;
        self.handle(packets)
    }

    fn request_keyframe(&mut self) {
        self.force_idr = true;
    }
}

// ---------------------------------------------------------------------------------------------
// デコーダー
// ---------------------------------------------------------------------------------------------

struct NvdecDecoder {
    session: ffi::CuvidSession,
    codec: Codec,
    length_size: usize,
    /// 次の AU の前に渡すパラメーターセット（Annex B）・設定 OBU。
    prefix: Vec<u8>,
    send_prefix: bool,
    timebase: Timebase,
    output_format: Option<PixelFormat>,
    flushed: bool,
}

impl Decoder for NvdecDecoder {
    fn codec(&self) -> Codec {
        self.codec
    }

    fn set_config(&mut self, config: &CodecConfig) -> Result<()> {
        if config.codec != self.codec {
            return Err(CodecError::invalid_config(
                "codec config does not match decoder",
            ));
        }
        let (ls, prefix) = record_prefix(self.codec, &config.record)?;
        self.length_size = ls;
        self.send_prefix = !prefix.is_empty();
        self.prefix = prefix;
        Ok(())
    }

    fn send(&mut self, au: &AccessUnit) -> Result<()> {
        if self.flushed {
            return Err(CodecError::internal("decoder already flushed"));
        }
        if au.codec != self.codec {
            return Err(CodecError::invalid_input(
                "access unit codec does not match decoder",
            ));
        }
        let prefix: &[u8] = if self.send_prefix { &self.prefix } else { &[] };
        let data = nvdec_input(self.codec, &au.payload, self.length_size, prefix)?;
        self.timebase = au.timebase;
        self.session.parse(&data, au.pts, false)?;
        self.send_prefix = false;
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<Frame>> {
        let Some(p) = self.session.pop() else {
            return Ok(None);
        };
        let f = VideoFrame::from_packed(
            p.format,
            p.width,
            p.height,
            Bytes::from(p.data),
            p.pts,
            self.timebase,
        )?;
        let f = match (self.output_format, p.format) {
            (None, _)
            | (Some(PixelFormat::Nv12), PixelFormat::Nv12)
            | (Some(PixelFormat::P010), PixelFormat::P010) => f,
            (Some(PixelFormat::I420), PixelFormat::Nv12) => f.convert(PixelFormat::I420)?,
            (Some(want), got) => {
                return Err(CodecError::unsupported(format!(
                    "NVDEC produced {} but {} was requested",
                    got.as_str(),
                    want.as_str()
                )));
            }
        };
        Ok(Some(Frame::Video(f)))
    }

    fn flush(&mut self) -> Result<()> {
        if self.flushed {
            return Ok(());
        }
        self.flushed = true;
        self.session.parse(&[], 0, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitstream::h264::tests::{PPS, SPS_320X240_HIGH};
    use crate::dl::LibrarySearch;

    const HEVC_VPS: &[u8] = &[
        0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00,
        0x03, 0x00, 0x00, 0x03, 0x00, 0x3c, 0x95, 0x98, 0x09,
    ];
    const HEVC_SPS: &[u8] = &[
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x3c, 0xa0, 0x0a, 0x08, 0x0f, 0x16, 0x59, 0x59, 0xa4, 0x93, 0x2b, 0xc0, 0x5a,
        0x02, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0x3c, 0x10,
    ];
    const HEVC_PPS: &[u8] = &[0x44, 0x01, 0xc1, 0x72, 0xb4, 0x62, 0x40];

    fn no_libs() -> ProbeOptions {
        ProbeOptions {
            libs: LibrarySearch {
                dirs: vec!["/nonexistent/senca".into()],
                dirs_only: true,
            },
            ..Default::default()
        }
    }

    fn video_cfg(codec: Codec) -> VideoEncoderConfig {
        VideoEncoderConfig::new(codec, 640, 480, 30, 2_000_000)
    }

    #[test]
    fn absent_libraries_are_unavailable() {
        let p = probe(&no_libs());
        let s = p.status();
        assert!(!s.available);
        assert!(s.detail.contains("libcuda.so.1"), "{}", s.detail);
        assert!(p.capabilities().is_empty());
        assert!(p.devices().is_empty());
        let e = p
            .create_encoder(&EncoderConfig::Video(video_cfg(Codec::H264)))
            .err()
            .unwrap();
        assert_eq!(e.kind, ErrorKind::Unavailable);
        let e = p
            .create_decoder(&DecoderConfig::new(Codec::H265))
            .err()
            .unwrap();
        assert_eq!(e.kind, ErrorKind::Unavailable);
    }

    #[test]
    fn broken_library_is_unavailable() {
        let dir = std::env::temp_dir().join(format!("senca-nvidia-fake-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["libcuda.so.1", "libnvidia-encode.so.1", "libnvcuvid.so.1"] {
            std::fs::write(dir.join(name), b"this is not an ELF file").unwrap();
        }
        let o = ProbeOptions {
            libs: LibrarySearch {
                dirs: vec![dir.clone()],
                dirs_only: true,
            },
            ..Default::default()
        };
        let p = probe(&o);
        let s = p.status();
        assert!(!s.available);
        assert!(s.detail.contains("libcuda.so.1"), "{}", s.detail);
        assert!(p.capabilities().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn system_probe_does_not_crash() {
        // 開発機には NVIDIA のドライバーが無い（あれば能力と状態が食い違わないことだけ確かめる）。
        let p = probe(&ProbeOptions::default());
        let s = p.status();
        assert_eq!(s.available, !p.capabilities().is_empty(), "{}", s.detail);
        assert!(!s.detail.is_empty());
    }

    #[test]
    fn params_mapping() {
        let p = encode_params(&video_cfg(Codec::H264)).unwrap();
        assert_eq!(
            (p.rc, p.profile, p.b_frames, p.ten_bit),
            (ffi::RcMode::Cbr, ffi::ProfileSel::Auto, 0, false)
        );
        assert_eq!(p.vbv_size, 2_000_000 / 30);
        assert_eq!(p.max_bitrate, 4_000_000);
        let mut c = video_cfg(Codec::H265);
        c.input_format = PixelFormat::P010;
        assert_eq!(
            encode_params(&c).unwrap().profile,
            ffi::ProfileSel::HevcMain10
        );
        c.profile = Some(VideoProfile::HevcMain);
        assert_eq!(
            encode_params(&c).unwrap_err().kind,
            ErrorKind::InvalidConfig
        );
        let mut c = video_cfg(Codec::H264);
        c.input_format = PixelFormat::P010;
        assert_eq!(encode_params(&c).unwrap_err().kind, ErrorKind::Unsupported);
        let mut c = video_cfg(Codec::H264);
        c.width = 641;
        assert_eq!(
            encode_params(&c).unwrap_err().kind,
            ErrorKind::InvalidConfig
        );
        let mut c = video_cfg(Codec::Av1);
        c.rate_control = RateControl::ConstQp(200);
        c.low_latency = false;
        let p = encode_params(&c).unwrap();
        assert_eq!(
            (p.rc, p.profile, p.vbv_size),
            (ffi::RcMode::ConstQp(200), ffi::ProfileSel::Av1Main, 0)
        );
        let mut c = video_cfg(Codec::H264);
        c.rate_control = RateControl::ConstQp(200);
        assert_eq!(encode_params(&c).unwrap().rc, ffi::RcMode::ConstQp(51));
        assert_eq!(
            encode_params(&video_cfg(Codec::Vp9)).unwrap_err().kind,
            ErrorKind::Unsupported
        );
    }

    #[test]
    fn pitched_copy() {
        let data: Vec<u8> = (0..PixelFormat::Nv12.packed_size(4, 2) as u8).collect();
        let f = VideoFrame::from_packed(
            PixelFormat::Nv12,
            4,
            2,
            Bytes::from(data),
            0,
            Timebase::MPEG,
        )
        .unwrap();
        let pitch = 8;
        let len = input_buffer_len(PixelFormat::Nv12, pitch, 2).unwrap();
        assert_eq!(len, 8 * 2 + 8);
        let mut dst = vec![0xeeu8; len];
        copy_frame_to_pitched(&f, &mut dst, pitch, 2).unwrap();
        assert_eq!(&dst[0..4], &[0, 1, 2, 3]);
        assert_eq!(&dst[4..8], &[0xee; 4]);
        assert_eq!(&dst[8..12], &[4, 5, 6, 7]);
        assert_eq!(&dst[16..20], &[8, 9, 10, 11]);
        // 小さすぎるバッファ・pitch は拒否
        assert!(copy_frame_to_pitched(&f, &mut dst[..19], pitch, 2).is_err());
        assert!(copy_frame_to_pitched(&f, &mut dst, 2, 2).is_err());
        assert_eq!(input_buffer_len(PixelFormat::I420, 8, 2), None);
        assert_eq!(
            input_buffer_len(PixelFormat::P010, 16, 3),
            Some(16 * 3 + 16 * 2)
        );
    }

    #[test]
    fn display_area() {
        assert_eq!(display_size(0, 0, 1920, 1080), Some((1920, 1080)));
        assert_eq!(display_size(0, 0, 0, 1080), None);
        assert_eq!(display_size(-1, 0, 10, 10), None);
    }

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        nal::nals_to_annexb(nals.iter().copied())
    }

    #[test]
    fn package_h264() {
        let idr: &[u8] = &[0x65, 0x88, 0x84, 0x21];
        let d = annexb(&[&[0x09, 0xf0], SPS_320X240_HIGH, PPS, idr]);
        let p = package(Codec::H264, &d).unwrap();
        assert_eq!(p.payload, nal::to_length_prefixed([idr]));
        let (rec, vp) = p.config.unwrap();
        assert_eq!((vp.width, vp.height, vp.profile), (320, 240, 100));
        let r = h264::parse_avcc(&rec).unwrap();
        assert_eq!(r.sps[0].as_ref(), SPS_320X240_HIGH);
        // P フレームだけなら設定なし
        let p = package(Codec::H264, &annexb(&[&[0x41, 0x9a]])).unwrap();
        assert!(p.config.is_none());
        assert_eq!(p.payload, vec![0, 0, 0, 2, 0x41, 0x9a]);
    }

    #[test]
    fn package_h265_and_decoder_input() {
        let idr: &[u8] = &[0x26, 0x01, 0xaf, 0x21];
        let d = annexb(&[HEVC_VPS, HEVC_SPS, HEVC_PPS, idr]);
        let p = package(Codec::H265, &d).unwrap();
        let (rec, vp) = p.config.unwrap();
        assert_eq!((vp.width, vp.height), (320, 240));
        // hvcC から作る前置きは VPS・SPS・PPS の Annex B
        let (ls, prefix) = record_prefix(Codec::H265, &rec).unwrap();
        assert_eq!(ls, 4);
        assert_eq!(prefix, annexb(&[HEVC_VPS, HEVC_SPS, HEVC_PPS]));
        let input = nvdec_input(Codec::H265, &p.payload, ls, &prefix).unwrap();
        assert_eq!(input, d);
        assert!(nvdec_input(Codec::H265, &[0, 0, 0, 9, 1], 4, &[]).is_err());
    }

    #[test]
    fn h264_decoder_input() {
        let rec = h264::build_avcc(&[SPS_320X240_HIGH], &[PPS]).unwrap();
        let (ls, prefix) = record_prefix(Codec::H264, &rec).unwrap();
        let au = nal::to_length_prefixed([&[0x65u8, 0x88][..]]);
        let input = nvdec_input(Codec::H264, &au, ls, &prefix).unwrap();
        assert_eq!(input, annexb(&[SPS_320X240_HIGH, PPS, &[0x65, 0x88]]));
        assert_eq!(
            nvdec_input(Codec::Vp9, &[1, 2, 3], 4, &[]).unwrap(),
            vec![1, 2, 3]
        );
        assert_eq!(
            nvdec_input(Codec::Vp8, &[1], 4, &[]).unwrap_err().kind,
            ErrorKind::Unsupported
        );
    }

    /// 320x240、profile 0、reduced_still_picture_header の最小のシーケンスヘッダー OBU。
    fn av1_seq_obu() -> Vec<u8> {
        let mut w = senca_core::bits::BitWriter::new();
        w.write_bits(0, 3); // seq_profile
        w.write_bits(1, 1); // still_picture
        w.write_bits(1, 1); // reduced_still_picture_header
        w.write_bits(8, 5); // seq_level_idx[0]
        w.write_bits(8, 4); // frame_width_bits_minus_1
        w.write_bits(7, 4); // frame_height_bits_minus_1
        w.write_bits(319, 9);
        w.write_bits(239, 8);
        w.write_bits(0, 3); // 128x128, filter_intra, intra_edge
        w.write_bits(0, 3); // superres, cdef, restoration
        w.write_bits(0, 1); // high_bitdepth
        w.write_bits(0, 1); // mono_chrome
        w.write_bits(0, 1); // color_description_present_flag
        w.write_bits(0, 1); // color_range
        w.write_bits(0, 2); // chroma_sample_position
        w.write_bits(0, 1); // film_grain_params_present
        w.write_bits(1, 1); // trailing_one_bit
        let payload = w.finish();
        let mut obu = vec![(av1::OBU_SEQUENCE_HEADER << 3) | 0x02, payload.len() as u8];
        obu.extend_from_slice(&payload);
        obu
    }

    #[test]
    fn package_av1_and_decoder_input() {
        let seq = av1_seq_obu();
        let mut d = vec![0x12, 0x00]; // Temporal Delimiter
        d.extend_from_slice(&seq);
        d.extend_from_slice(&[(av1::OBU_FRAME << 3) | 0x02, 2, 0xaa, 0xbb]);
        let p = package(Codec::Av1, &d).unwrap();
        assert_eq!(&p.payload[..], &d[2..]);
        let (rec, vp) = p.config.unwrap();
        assert_eq!((vp.width, vp.height), (320, 240));
        let (_, prefix) = record_prefix(Codec::Av1, &rec).unwrap();
        assert_eq!(prefix, seq);
        let frame = [(av1::OBU_FRAME << 3) | 0x02, 1, 0xcc];
        let input = nvdec_input(Codec::Av1, &frame, 4, &prefix).unwrap();
        assert_eq!(&input[..2], &[0x12, 0x00]);
        assert_eq!(&input[2..2 + seq.len()], &seq[..]);
        assert_eq!(&input[2 + seq.len()..], &frame);
        assert!(nvdec_input(Codec::Av1, &[0x32, 9, 1], 4, &[]).is_err());
    }

    #[test]
    fn capabilities_from_probe() {
        let info = DeviceInfo {
            provider: ProviderId::Nvidia,
            index: 0,
            name: "Test GPU".into(),
            driver_version: Some("610.1".into()),
            path: None,
            hardware: true,
        };
        let d = ffi::ProbedDevice {
            index: 0,
            name: "Test GPU".into(),
            compute_capability: (8, 9),
            encode: vec![
                ffi::EncodeCap {
                    codec: Codec::H264,
                    max_width: 4096,
                    max_height: 4096,
                    ten_bit: true,
                },
                ffi::EncodeCap {
                    codec: Codec::Av1,
                    max_width: 8192,
                    max_height: 8192,
                    ten_bit: true,
                },
            ],
            decode: vec![
                ffi::DecodeCap {
                    codec: Codec::Vp9,
                    bit_depth: 8,
                    max_width: 8192,
                    max_height: 8192,
                },
                ffi::DecodeCap {
                    codec: Codec::Vp9,
                    bit_depth: 10,
                    max_width: 8192,
                    max_height: 8192,
                },
                ffi::DecodeCap {
                    codec: Codec::H264,
                    bit_depth: 8,
                    max_width: 4096,
                    max_height: 4096,
                },
            ],
            notes: vec![],
        };
        let caps = device_capabilities(&info, &d);
        assert_eq!(caps.len(), 4);
        let h264e = caps
            .iter()
            .find(|c| c.codec == Codec::H264 && c.direction == Direction::Encode)
            .unwrap();
        assert!(!h264e.formats.contains(&PixelFormat::P010));
        assert!(h264e.hardware);
        let av1e = caps.iter().find(|c| c.codec == Codec::Av1).unwrap();
        assert!(av1e.formats.contains(&PixelFormat::P010));
        let vp9 = caps.iter().find(|c| c.codec == Codec::Vp9).unwrap();
        assert!(vp9.formats.contains(&PixelFormat::P010));
        assert_eq!(vp9.max_sessions, None);
    }
}
