//! 生のフレーム（未圧縮の映像・音声）。
//!
//! - 映像: 平面 YUV 4:2:0。8 bit の I420（Y・U・V の 3 面）と NV12（Y と UV 交互の 2 面）、
//!   10 bit の P010（Y と UV 交互の 2 面、1 標本 16 bit リトルエンディアン、上位 10 bit に値）。
//!   幅・高さが奇数のとき色差は切り上げ（`(w + 1) / 2`）。
//! - 音声: インターリーブした PCM（f32 または i16、ネイティブエンディアン）。
//!
//! 各面は `Bytes` で持ち、`stride`（1 行のバイト数）は行の実データ以上。

use crate::error::{CodecError, Result};
use bytes::Bytes;
use senca_core::Timebase;

/// 1 辺の上限（これを超える解像度は設定・入力とも拒否する）。
pub const MAX_DIMENSION: u32 = 16384;
/// 1 フレームの画素数の上限（8K = 7680x4320 の 4 倍まで）。
pub const MAX_PIXELS: u64 = 7680 * 4320 * 4;
/// 音声 1 フレームの標本数（1 チャンネルあたり）の上限（48 kHz で 1 秒）。
pub const MAX_AUDIO_SAMPLES: usize = 192_000;
/// 音声のチャンネル数の上限。
pub const MAX_AUDIO_CHANNELS: u8 = 8;

/// 映像の画素形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8 bit 4:2:0、Y・U・V の 3 面。
    I420,
    /// 8 bit 4:2:0、Y と UV 交互の 2 面。
    Nv12,
    /// 10 bit 4:2:0、Y と UV 交互の 2 面（16 bit LE、上位 10 bit）。
    P010,
}

impl PixelFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            PixelFormat::I420 => "i420",
            PixelFormat::Nv12 => "nv12",
            PixelFormat::P010 => "p010",
        }
    }

    pub fn parse(s: &str) -> Option<PixelFormat> {
        Some(match s {
            "i420" => PixelFormat::I420,
            "nv12" => PixelFormat::Nv12,
            "p010" => PixelFormat::P010,
            _ => return None,
        })
    }

    pub fn to_u8(self) -> u8 {
        match self {
            PixelFormat::I420 => 0,
            PixelFormat::Nv12 => 1,
            PixelFormat::P010 => 2,
        }
    }

    pub fn from_u8(v: u8) -> Option<PixelFormat> {
        Some(match v {
            0 => PixelFormat::I420,
            1 => PixelFormat::Nv12,
            2 => PixelFormat::P010,
            _ => return None,
        })
    }

    pub fn plane_count(self) -> usize {
        match self {
            PixelFormat::I420 => 3,
            PixelFormat::Nv12 | PixelFormat::P010 => 2,
        }
    }

    pub fn bit_depth(self) -> u8 {
        match self {
            PixelFormat::I420 | PixelFormat::Nv12 => 8,
            PixelFormat::P010 => 10,
        }
    }

    /// 面 `idx` の 1 行の実データのバイト数と行数。
    pub fn plane_size(self, width: u32, height: u32, idx: usize) -> Option<(usize, usize)> {
        let w = width as usize;
        let h = height as usize;
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);
        match (self, idx) {
            (PixelFormat::I420, 0) | (PixelFormat::Nv12, 0) => Some((w, h)),
            (PixelFormat::I420, 1 | 2) => Some((cw, ch)),
            (PixelFormat::Nv12, 1) => Some((cw * 2, ch)),
            (PixelFormat::P010, 0) => Some((w * 2, h)),
            (PixelFormat::P010, 1) => Some((cw * 4, ch)),
            _ => None,
        }
    }

    /// 詰めて並べたときの全体のバイト数。
    pub fn packed_size(self, width: u32, height: u32) -> usize {
        (0..self.plane_count())
            .filter_map(|i| self.plane_size(width, height, i))
            .map(|(r, n)| r * n)
            .sum()
    }
}

/// 解像度の検査。
pub fn check_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(CodecError::invalid_config("width/height must be non-zero"));
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION || width as u64 * height as u64 > MAX_PIXELS
    {
        return Err(CodecError::invalid_config(format!(
            "resolution {width}x{height} exceeds limit"
        )));
    }
    Ok(())
}

/// 映像の 1 面。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    pub data: Bytes,
    /// 1 行のバイト数。
    pub stride: usize,
}

/// 未圧縮の映像フレーム。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub planes: Vec<Plane>,
    pub pts: i64,
    pub timebase: Timebase,
    /// エンコーダーへの入力でキーフレームを強制する。デコーダーの出力では「キーフレームから復号した」。
    pub keyframe: bool,
}

impl VideoFrame {
    /// 詰めて並べたバッファ（stride = 行の実データ）から作る。サイズが合わなければ `Err`。
    pub fn from_packed(
        format: PixelFormat,
        width: u32,
        height: u32,
        data: Bytes,
        pts: i64,
        timebase: Timebase,
    ) -> Result<VideoFrame> {
        check_dimensions(width, height).map_err(|e| CodecError::invalid_input(e.message))?;
        let need = format.packed_size(width, height);
        if data.len() != need {
            return Err(CodecError::invalid_input(format!(
                "{} {width}x{height} needs {need} bytes, got {}",
                format.as_str(),
                data.len()
            )));
        }
        let mut planes = Vec::with_capacity(format.plane_count());
        let mut off = 0;
        for i in 0..format.plane_count() {
            let (row, rows) = format
                .plane_size(width, height, i)
                .expect("plane index in range");
            planes.push(Plane {
                data: data.slice(off..off + row * rows),
                stride: row,
            });
            off += row * rows;
        }
        Ok(VideoFrame {
            format,
            width,
            height,
            planes,
            pts,
            timebase,
            keyframe: false,
        })
    }

    /// 面の数・stride・長さを検査する。
    pub fn validate(&self) -> Result<()> {
        check_dimensions(self.width, self.height)
            .map_err(|e| CodecError::invalid_input(e.message))?;
        if self.planes.len() != self.format.plane_count() {
            return Err(CodecError::invalid_input("wrong number of planes"));
        }
        for (i, p) in self.planes.iter().enumerate() {
            let (row, rows) = self
                .format
                .plane_size(self.width, self.height, i)
                .expect("plane index in range");
            if p.stride < row {
                return Err(CodecError::invalid_input(format!(
                    "plane {i}: stride too small"
                )));
            }
            // 最後の行は stride 分なくてもよい。
            let need = p
                .stride
                .checked_mul(rows - 1)
                .and_then(|v| v.checked_add(row))
                .ok_or_else(|| CodecError::invalid_input("plane size overflow"))?;
            if p.data.len() < need {
                return Err(CodecError::invalid_input(format!(
                    "plane {i}: data too short"
                )));
            }
        }
        Ok(())
    }

    /// 面 `idx` の行 `y` の実データ。
    pub fn row(&self, idx: usize, y: usize) -> &[u8] {
        let (row, _) = self
            .format
            .plane_size(self.width, self.height, idx)
            .expect("plane index in range");
        let p = &self.planes[idx];
        &p.data[y * p.stride..y * p.stride + row]
    }

    /// stride を詰めて 1 つのバッファにする（比較・保存用）。
    pub fn to_packed(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut out = Vec::with_capacity(self.format.packed_size(self.width, self.height));
        for i in 0..self.format.plane_count() {
            let (_, rows) = self
                .format
                .plane_size(self.width, self.height, i)
                .expect("plane index in range");
            for y in 0..rows {
                out.extend_from_slice(self.row(i, y));
            }
        }
        Ok(out)
    }

    /// 8 bit の I420 と NV12 の相互変換（同じ形式ならそのまま複製）。P010 は対象外。
    /// 色差の並べ替えだけで、色空間の変換はしない（仕様 2.1: 色変換は自前）。
    pub fn convert(&self, to: PixelFormat) -> Result<VideoFrame> {
        self.validate()?;
        if self.format == to {
            return Ok(self.clone());
        }
        let (w, h) = (self.width, self.height);
        let (cw, ch) = ((w as usize).div_ceil(2), (h as usize).div_ceil(2));
        let mut out = Vec::with_capacity(to.packed_size(w, h));
        for y in 0..h as usize {
            out.extend_from_slice(self.row(0, y));
        }
        match (self.format, to) {
            (PixelFormat::I420, PixelFormat::Nv12) => {
                for y in 0..ch {
                    let (u, v) = (self.row(1, y), self.row(2, y));
                    for x in 0..cw {
                        out.push(u[x]);
                        out.push(v[x]);
                    }
                }
            }
            (PixelFormat::Nv12, PixelFormat::I420) => {
                for y in 0..ch {
                    out.extend(self.row(1, y).iter().step_by(2));
                }
                for y in 0..ch {
                    out.extend(self.row(1, y).iter().skip(1).step_by(2));
                }
            }
            _ => {
                return Err(CodecError::unsupported(format!(
                    "conversion {} -> {} not supported",
                    self.format.as_str(),
                    to.as_str()
                )));
            }
        }
        let mut f = VideoFrame::from_packed(to, w, h, Bytes::from(out), self.pts, self.timebase)?;
        f.keyframe = self.keyframe;
        Ok(f)
    }
}

/// 音声の標本形式（インターリーブ、ネイティブエンディアン）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SampleFormat {
    F32,
    I16,
}

impl SampleFormat {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::F32 => 4,
            SampleFormat::I16 => 2,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            SampleFormat::F32 => 0,
            SampleFormat::I16 => 1,
        }
    }

    pub fn from_u8(v: u8) -> Option<SampleFormat> {
        Some(match v {
            0 => SampleFormat::F32,
            1 => SampleFormat::I16,
            _ => return None,
        })
    }
}

/// 未圧縮の音声フレーム（インターリーブ PCM）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    pub format: SampleFormat,
    pub sample_rate: u32,
    pub channels: u8,
    pub data: Bytes,
    pub pts: i64,
    pub timebase: Timebase,
}

impl AudioFrame {
    /// 1 チャンネルあたりの標本数。
    pub fn samples_per_channel(&self) -> usize {
        let unit = self.format.bytes_per_sample() * self.channels.max(1) as usize;
        self.data.len() / unit
    }

    pub fn validate(&self) -> Result<()> {
        if self.channels == 0 || self.channels > MAX_AUDIO_CHANNELS {
            return Err(CodecError::invalid_input("bad channel count"));
        }
        if self.sample_rate == 0 || self.sample_rate > 384_000 {
            return Err(CodecError::invalid_input("bad sample rate"));
        }
        let unit = self.format.bytes_per_sample() * self.channels as usize;
        if !self.data.len().is_multiple_of(unit) {
            return Err(CodecError::invalid_input(
                "audio data is not a whole number of samples",
            ));
        }
        if self.data.len() / unit > MAX_AUDIO_SAMPLES {
            return Err(CodecError::invalid_input("audio frame too long"));
        }
        Ok(())
    }
}

/// 映像または音声のフレーム。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Video(VideoFrame),
    Audio(AudioFrame),
}

impl Frame {
    pub fn pts(&self) -> i64 {
        match self {
            Frame::Video(v) => v.pts,
            Frame::Audio(a) => a.pts,
        }
    }

    pub fn as_video(&self) -> Option<&VideoFrame> {
        match self {
            Frame::Video(v) => Some(v),
            Frame::Audio(_) => None,
        }
    }

    pub fn as_audio(&self) -> Option<&AudioFrame> {
        match self {
            Frame::Audio(a) => Some(a),
            Frame::Video(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tb() -> Timebase {
        Timebase::MPEG
    }

    #[test]
    fn packed_sizes() {
        assert_eq!(PixelFormat::I420.packed_size(4, 2), 8 + 2 + 2);
        assert_eq!(PixelFormat::I420.packed_size(3, 3), 9 + 4 + 4);
        assert_eq!(PixelFormat::Nv12.packed_size(3, 3), 9 + 8);
        assert_eq!(PixelFormat::P010.packed_size(4, 2), 16 + 8);
    }

    #[test]
    fn rejects_bad_sizes() {
        assert!(
            VideoFrame::from_packed(PixelFormat::I420, 4, 2, Bytes::from(vec![0; 11]), 0, tb())
                .is_err()
        );
        assert!(VideoFrame::from_packed(PixelFormat::I420, 0, 2, Bytes::new(), 0, tb()).is_err());
        assert!(
            VideoFrame::from_packed(PixelFormat::I420, 20000, 2, Bytes::new(), 0, tb()).is_err()
        );
        let mut f =
            VideoFrame::from_packed(PixelFormat::I420, 4, 2, Bytes::from(vec![0; 12]), 0, tb())
                .unwrap();
        f.planes[1].stride = 1;
        assert!(f.validate().is_err());
        f.planes.pop();
        assert!(f.validate().is_err());
    }

    #[test]
    fn i420_nv12_roundtrip() {
        let data: Vec<u8> = (0..PixelFormat::I420.packed_size(5, 3) as u32)
            .map(|v| v as u8)
            .collect();
        let f =
            VideoFrame::from_packed(PixelFormat::I420, 5, 3, Bytes::from(data.clone()), 7, tb())
                .unwrap();
        let n = f.convert(PixelFormat::Nv12).unwrap();
        assert_eq!(n.planes.len(), 2);
        // U0 V0 が交互に並ぶ
        assert_eq!(n.row(1, 0)[0], f.row(1, 0)[0]);
        assert_eq!(n.row(1, 0)[1], f.row(2, 0)[0]);
        let back = n.convert(PixelFormat::I420).unwrap();
        assert_eq!(back.to_packed().unwrap(), data);
        assert_eq!(back.pts, 7);
    }

    #[test]
    fn audio_validate() {
        let a = AudioFrame {
            format: SampleFormat::I16,
            sample_rate: 48000,
            channels: 2,
            data: Bytes::from(vec![0; 960 * 4]),
            pts: 0,
            timebase: Timebase::new(1, 48000),
        };
        assert!(a.validate().is_ok());
        assert_eq!(a.samples_per_channel(), 960);
        let bad = AudioFrame {
            data: Bytes::from(vec![0; 3]),
            ..a.clone()
        };
        assert!(bad.validate().is_err());
        let bad = AudioFrame { channels: 0, ..a };
        assert!(bad.validate().is_err());
    }
}
