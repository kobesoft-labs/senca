//! プロバイダーの共通部品（`unsafe` を含まない）。

use crate::caps::{Capability, DeviceInfo, Direction, ProviderId, ProviderStatus};
use crate::config::{DecoderConfig, EncoderConfig, VideoEncoderConfig};
use crate::error::{CodecError, Result};
use crate::frame::{PixelFormat, VideoFrame};
use crate::provider::{CodecProvider, Decoder, Encoder};
use bytes::Bytes;
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase, TrackId};
use std::sync::Arc;

/// 使えないプロバイダー（ライブラリ・装置が無い、無効にした）。
#[derive(Debug, Clone)]
pub struct UnavailableProvider {
    pub id: ProviderId,
    pub reason: String,
}

impl UnavailableProvider {
    pub fn arc(id: ProviderId, reason: impl Into<String>) -> Arc<dyn CodecProvider> {
        Arc::new(UnavailableProvider {
            id,
            reason: reason.into(),
        })
    }

    fn error(&self) -> CodecError {
        CodecError::unavailable(format!(
            "{} is not available: {}",
            self.id.as_str(),
            self.reason
        ))
    }
}

impl CodecProvider for UnavailableProvider {
    fn id(&self) -> ProviderId {
        self.id
    }
    fn status(&self) -> ProviderStatus {
        ProviderStatus {
            provider: self.id,
            available: false,
            detail: self.reason.clone(),
        }
    }
    fn devices(&self) -> Vec<DeviceInfo> {
        Vec::new()
    }
    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }
    fn create_encoder(&self, _config: &EncoderConfig) -> Result<Box<dyn Encoder>> {
        Err(self.error())
    }
    fn create_decoder(&self, _config: &DecoderConfig) -> Result<Box<dyn Decoder>> {
        Err(self.error())
    }
}

/// 能力を 1 つ作る。
#[allow(clippy::too_many_arguments)]
pub fn capability(
    device: &DeviceInfo,
    codec: Codec,
    direction: Direction,
    max_width: u32,
    max_height: u32,
    max_sessions: Option<u32>,
    formats: &[PixelFormat],
) -> Capability {
    Capability {
        provider: device.provider,
        codec,
        direction,
        hardware: device.hardware,
        max_width,
        max_height,
        max_sessions,
        formats: formats.to_vec(),
        device_index: device.index,
        device_name: device.name.clone(),
        driver_version: device.driver_version.clone(),
    }
}

/// ソフトウェアのライブラリを 1 つの「装置」として表す。
pub fn software_device(
    provider: ProviderId,
    name: &str,
    version: Option<String>,
    library: &str,
) -> DeviceInfo {
    DeviceInfo {
        provider,
        index: 0,
        name: name.to_string(),
        driver_version: version,
        path: Some(library.to_string()),
        hardware: false,
    }
}

/// 出力 AU の通し番号と設定の世代を管理する。
#[derive(Debug, Clone)]
pub struct AuStamper {
    pub codec: Codec,
    pub track: TrackId,
    pub timebase: Timebase,
    sequence: u64,
    config: Option<CodecConfig>,
}

impl AuStamper {
    pub fn new(codec: Codec, track: TrackId, timebase: Timebase) -> Self {
        AuStamper {
            codec,
            track,
            timebase,
            sequence: 0,
            config: None,
        }
    }

    /// 設定を差し替える。中身が変わったときだけ世代を増やす。
    pub fn set_config(
        &mut self,
        record: Bytes,
        video: Option<senca_core::media::VideoParams>,
        audio: Option<senca_core::media::AudioParams>,
    ) {
        if let Some(c) = &self.config
            && c.record == record
            && c.video == video
            && c.audio == audio
        {
            return;
        }
        let generation = self
            .config
            .as_ref()
            .map_or(0, |c| c.generation.wrapping_add(1));
        self.config = Some(CodecConfig {
            codec: self.codec,
            generation,
            record,
            video,
            audio,
        });
    }

    pub fn config(&self) -> Option<CodecConfig> {
        self.config.clone()
    }

    pub fn generation(&self) -> u32 {
        self.config.as_ref().map_or(0, |c| c.generation)
    }

    pub fn stamp(
        &mut self,
        payload: Bytes,
        pts: i64,
        dts: i64,
        duration: u64,
        keyframe: bool,
    ) -> AccessUnit {
        let au = AccessUnit {
            track: self.track,
            codec: self.codec,
            epoch: 0,
            sequence: self.sequence,
            config_generation: self.generation(),
            pts,
            dts,
            duration,
            timebase: self.timebase,
            keyframe,
            discontinuity: false,
            capture_time_ns: None,
            payload,
        };
        self.sequence += 1;
        au
    }
}

/// 入力フレームが設定と合っているか。
pub fn check_input_frame(cfg: &VideoEncoderConfig, f: &VideoFrame) -> Result<()> {
    f.validate()?;
    if f.width != cfg.width || f.height != cfg.height {
        return Err(CodecError::invalid_input(format!(
            "frame {}x{} does not match encoder {}x{}",
            f.width, f.height, cfg.width, cfg.height
        )));
    }
    Ok(())
}

/// 1 フレームの長さ（設定の時間軸）。求まらなければ 0。
pub fn frame_duration(cfg: &VideoEncoderConfig) -> u64 {
    // 1 フレーム = fps_den / fps_num 秒 = fps_den * tb.den / (fps_num * tb.num) 刻み
    let n = cfg.fps_den as u128 * cfg.timebase.den as u128;
    let d = cfg.fps_num as u128 * cfg.timebase.num as u128;
    n.checked_div(d)
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(0)
}

/// 復号器が出した平面 YUV（4:2:0 または 4:0:0、8〜12 bit）を [`VideoFrame`] にする。
///
/// - `planes[i]` は詰めた面（1 行 `row_bytes[i]` バイト）。高ビット深度は 1 標本 16 bit（ネイティブエンディアン、下位詰め）。
/// - 8 bit は I420（`want` が NV12 なら NV12）、高ビット深度は P010（`want` が I420/NV12 なら 8 bit に丸める）。
/// - 4:0:0 は色差 128 で埋める。4:2:2・4:4:4 は `Unsupported`。
#[allow(clippy::too_many_arguments)]
pub fn planar_yuv_to_frame(
    width: u32,
    height: u32,
    layout_420: Option<bool>,
    bit_depth: u32,
    planes: [&[u8]; 3],
    row_bytes: [usize; 3],
    want: Option<PixelFormat>,
    pts: i64,
    timebase: Timebase,
) -> Result<VideoFrame> {
    let mono = match layout_420 {
        Some(true) => false,
        Some(false) => true,
        None => {
            return Err(CodecError::unsupported(
                "only 4:2:0 (or monochrome) output is supported",
            ));
        }
    };
    crate::frame::check_dimensions(width, height)
        .map_err(|e| CodecError::invalid_input(e.message))?;
    let (w, h) = (width as usize, height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let high = bit_depth > 8;
    let bps = if high { 2 } else { 1 };
    // 入力の大きさを確かめる。
    let need = [(w, h), (cw, ch), (cw, ch)];
    for i in 0..if mono { 1 } else { 3 } {
        let (pw, ph) = need[i];
        if row_bytes[i] < pw * bps || planes[i].len() < row_bytes[i] * ph {
            return Err(CodecError::backend("decoder returned a short plane"));
        }
    }
    let sample = |i: usize, x: usize, y: usize| -> u16 {
        if mono && i > 0 {
            return 128u16 << (bit_depth.max(8) - 8);
        }
        let off = y * row_bytes[i] + x * bps;
        if high {
            u16::from_ne_bytes([planes[i][off], planes[i][off + 1]])
        } else {
            planes[i][off] as u16
        }
    };
    let to8 = |v: u16| -> u8 {
        if high {
            let sh = bit_depth - 8;
            ((v as u32 + (1 << (sh - 1))) >> sh).min(255) as u8
        } else {
            v as u8
        }
    };
    let format = match (high, want) {
        (false, Some(PixelFormat::Nv12)) | (true, Some(PixelFormat::Nv12)) => PixelFormat::Nv12,
        (false, _) | (true, Some(PixelFormat::I420)) => PixelFormat::I420,
        (true, _) => PixelFormat::P010,
    };
    let mut out = Vec::with_capacity(format.packed_size(width, height));
    match format {
        PixelFormat::I420 | PixelFormat::Nv12 => {
            for y in 0..h {
                if !high {
                    out.extend_from_slice(&planes[0][y * row_bytes[0]..y * row_bytes[0] + w]);
                } else {
                    out.extend((0..w).map(|x| to8(sample(0, x, y))));
                }
            }
            if format == PixelFormat::I420 {
                for i in 1..3 {
                    for y in 0..ch {
                        out.extend((0..cw).map(|x| to8(sample(i, x, y))));
                    }
                }
            } else {
                for y in 0..ch {
                    for x in 0..cw {
                        out.push(to8(sample(1, x, y)));
                        out.push(to8(sample(2, x, y)));
                    }
                }
            }
        }
        PixelFormat::P010 => {
            let sh = 16 - bit_depth.min(16);
            for y in 0..h {
                for x in 0..w {
                    out.extend_from_slice(&(sample(0, x, y) << sh).to_le_bytes());
                }
            }
            for y in 0..ch {
                for x in 0..cw {
                    out.extend_from_slice(&(sample(1, x, y) << sh).to_le_bytes());
                    out.extend_from_slice(&(sample(2, x, y) << sh).to_le_bytes());
                }
            }
        }
    }
    VideoFrame::from_packed(format, width, height, Bytes::from(out), pts, timebase)
}

/// ネイティブの C 文字列（NUL 終端の可能性がある固定長配列）を String にする。
pub fn c_chars_to_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamper_generations() {
        let mut s = AuStamper::new(Codec::Vp8, TrackId(1), Timebase::MPEG);
        assert_eq!(s.config(), None);
        s.set_config(Bytes::new(), None, None);
        assert_eq!(s.generation(), 0);
        s.set_config(Bytes::new(), None, None);
        assert_eq!(s.generation(), 0);
        s.set_config(Bytes::from_static(b"x"), None, None);
        assert_eq!(s.generation(), 1);
        let a = s.stamp(Bytes::new(), 1, 1, 0, true);
        let b = s.stamp(Bytes::new(), 2, 2, 0, false);
        assert_eq!(
            (a.sequence, b.sequence, b.config_generation, b.track),
            (0, 1, 1, TrackId(1))
        );
    }

    #[test]
    fn durations() {
        let mut c = VideoEncoderConfig::new(Codec::Vp8, 64, 64, 30, 100_000);
        assert_eq!(frame_duration(&c), 3000);
        c.fps_num = 30000;
        c.fps_den = 1001;
        assert_eq!(frame_duration(&c), 3003);
    }

    #[test]
    fn planar_conversion() {
        // 2x2、8 bit I420
        let y = [1u8, 2, 3, 4];
        let f = planar_yuv_to_frame(
            2,
            2,
            Some(true),
            8,
            [&y, &[5], &[6]],
            [2, 1, 1],
            None,
            0,
            Timebase::MPEG,
        )
        .unwrap();
        assert_eq!(f.to_packed().unwrap(), vec![1, 2, 3, 4, 5, 6]);
        let f = planar_yuv_to_frame(
            2,
            2,
            Some(true),
            8,
            [&y, &[5], &[6]],
            [2, 1, 1],
            Some(PixelFormat::Nv12),
            0,
            Timebase::MPEG,
        )
        .unwrap();
        assert_eq!(f.format, PixelFormat::Nv12);
        // 単色（4:0:0）
        let f = planar_yuv_to_frame(
            2,
            2,
            Some(false),
            8,
            [&y, &[], &[]],
            [2, 0, 0],
            None,
            0,
            Timebase::MPEG,
        )
        .unwrap();
        assert_eq!(f.to_packed().unwrap(), vec![1, 2, 3, 4, 128, 128]);
        // 10 bit → P010 と 8 bit
        let y10: Vec<u8> = [1023u16, 0, 512, 4]
            .iter()
            .flat_map(|v| v.to_ne_bytes())
            .collect();
        let u10 = 512u16.to_ne_bytes();
        let f = planar_yuv_to_frame(
            2,
            2,
            Some(true),
            10,
            [&y10, &u10, &u10],
            [4, 2, 2],
            None,
            0,
            Timebase::MPEG,
        )
        .unwrap();
        assert_eq!(f.format, PixelFormat::P010);
        assert_eq!(&f.to_packed().unwrap()[..2], &(1023u16 << 6).to_le_bytes());
        let f = planar_yuv_to_frame(
            2,
            2,
            Some(true),
            10,
            [&y10, &u10, &u10],
            [4, 2, 2],
            Some(PixelFormat::I420),
            0,
            Timebase::MPEG,
        )
        .unwrap();
        assert_eq!(f.to_packed().unwrap(), vec![255, 0, 128, 1, 128, 128]);
        // 短い面は拒否
        assert!(
            planar_yuv_to_frame(
                2,
                2,
                Some(true),
                8,
                [&y[..3], &[5], &[6]],
                [2, 1, 1],
                None,
                0,
                Timebase::MPEG
            )
            .is_err()
        );
        assert!(
            planar_yuv_to_frame(
                2,
                2,
                None,
                8,
                [&y, &[5], &[6]],
                [2, 1, 1],
                None,
                0,
                Timebase::MPEG
            )
            .is_err()
        );
    }

    #[test]
    fn unavailable() {
        let p = UnavailableProvider::arc(ProviderId::Nvidia, "libcuda.so.1 not found");
        assert!(!p.status().available);
        let e = p
            .create_decoder(&DecoderConfig::new(Codec::H264))
            .err()
            .unwrap();
        assert_eq!(e.kind, crate::ErrorKind::Unavailable);
    }
}
