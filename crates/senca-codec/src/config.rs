//! エンコーダー・デコーダーの設定。

use crate::error::{CodecError, Result};
use crate::frame::{PixelFormat, SampleFormat, check_dimensions};
use senca_core::media::MediaKind;
use senca_core::{Codec, CodecConfig, Timebase, TrackId};

/// ビットレートの上限（800 Mbps）。
pub const MAX_BITRATE_BPS: u32 = 800_000_000;

/// 符号量の制御。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateControl {
    /// 固定ビットレート。
    Cbr,
    /// 可変ビットレート（`bitrate_bps` は目標、`max_bitrate_bps` は上限）。
    Vbr,
    /// 固定量子化（値はコーデックの範囲に丸める: H.264/H.265 は 0..=51、AV1 は 0..=63、VP8/VP9 は 0..=63）。
    ConstQp(u8),
}

/// 映像のプロファイル。`None` のときはプロバイダーの既定（8 bit の標準的なもの）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoProfile {
    H264Baseline,
    H264ConstrainedBaseline,
    H264Main,
    H264High,
    HevcMain,
    HevcMain10,
    Av1Main,
    Vp9Profile0,
    Vp9Profile2,
}

impl VideoProfile {
    /// このプロファイルが属するコーデック。
    pub fn codec(self) -> Codec {
        match self {
            VideoProfile::H264Baseline
            | VideoProfile::H264ConstrainedBaseline
            | VideoProfile::H264Main
            | VideoProfile::H264High => Codec::H264,
            VideoProfile::HevcMain | VideoProfile::HevcMain10 => Codec::H265,
            VideoProfile::Av1Main => Codec::Av1,
            VideoProfile::Vp9Profile0 | VideoProfile::Vp9Profile2 => Codec::Vp9,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            VideoProfile::H264Baseline => 0,
            VideoProfile::H264ConstrainedBaseline => 1,
            VideoProfile::H264Main => 2,
            VideoProfile::H264High => 3,
            VideoProfile::HevcMain => 4,
            VideoProfile::HevcMain10 => 5,
            VideoProfile::Av1Main => 6,
            VideoProfile::Vp9Profile0 => 7,
            VideoProfile::Vp9Profile2 => 8,
        }
    }

    pub fn from_u8(v: u8) -> Option<VideoProfile> {
        Some(match v {
            0 => VideoProfile::H264Baseline,
            1 => VideoProfile::H264ConstrainedBaseline,
            2 => VideoProfile::H264Main,
            3 => VideoProfile::H264High,
            4 => VideoProfile::HevcMain,
            5 => VideoProfile::HevcMain10,
            6 => VideoProfile::Av1Main,
            7 => VideoProfile::Vp9Profile0,
            8 => VideoProfile::Vp9Profile2,
            _ => return None,
        })
    }
}

/// 映像エンコーダーの設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoEncoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    /// フレームレート（fps_num / fps_den）。
    pub fps_num: u32,
    pub fps_den: u32,
    pub bitrate_bps: u32,
    /// VBR の上限。`None` なら `bitrate_bps` の 2 倍程度をプロバイダーが決める。
    pub max_bitrate_bps: Option<u32>,
    /// キーフレームの間隔（フレーム数）。0 は「最初の 1 枚だけ」。
    pub gop: u32,
    pub rate_control: RateControl,
    pub profile: Option<VideoProfile>,
    /// 低遅延（先読み無し、1 フレーム入れたら 1 AU が出る方向に設定する）。
    pub low_latency: bool,
    /// B フレームの最大数（0 = B フレーム無し。ライブの既定）。
    pub b_frames: u32,
    /// 入力フレームの画素形式。
    pub input_format: PixelFormat,
    /// ソフトウェアコーデックのスレッド数（0 = 自動）。
    pub threads: u32,
    /// 出力 AU に付けるトラックと時間軸。入力フレームの `pts` はこの時間軸で解釈する。
    pub track: TrackId,
    pub timebase: Timebase,
}

impl VideoEncoderConfig {
    /// 低遅延・B フレーム無し・CBR の既定値。
    pub fn new(codec: Codec, width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Self {
        VideoEncoderConfig {
            codec,
            width,
            height,
            fps_num: fps,
            fps_den: 1,
            bitrate_bps,
            max_bitrate_bps: None,
            gop: fps.saturating_mul(2),
            rate_control: RateControl::Cbr,
            profile: None,
            low_latency: true,
            b_frames: 0,
            input_format: PixelFormat::I420,
            threads: 0,
            track: TrackId(0),
            timebase: Timebase::MPEG,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.codec.kind() != MediaKind::Video {
            return Err(CodecError::invalid_config(format!(
                "{} is not a video codec",
                self.codec
            )));
        }
        check_dimensions(self.width, self.height)?;
        if self.fps_num == 0 || self.fps_den == 0 || self.fps_num / self.fps_den > 1000 {
            return Err(CodecError::invalid_config("bad frame rate"));
        }
        if self.bitrate_bps == 0 && !matches!(self.rate_control, RateControl::ConstQp(_)) {
            return Err(CodecError::invalid_config("bitrate must be non-zero"));
        }
        if self.bitrate_bps > MAX_BITRATE_BPS
            || self.max_bitrate_bps.is_some_and(|m| m > MAX_BITRATE_BPS)
        {
            return Err(CodecError::invalid_config("bitrate exceeds limit"));
        }
        if self.b_frames > 16 {
            return Err(CodecError::invalid_config("too many b-frames"));
        }
        if self.threads > 256 {
            return Err(CodecError::invalid_config("too many threads"));
        }
        if let Some(p) = self.profile
            && p.codec() != self.codec
        {
            return Err(CodecError::invalid_config("profile does not match codec"));
        }
        Ok(())
    }
}

/// Opus の用途（`OPUS_APPLICATION_*`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpusApplication {
    Voip,
    Audio,
    LowDelay,
}

/// 音声エンコーダーの設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioEncoderConfig {
    pub codec: Codec,
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate_bps: u32,
    /// 1 パケットの長さ（マイクロ秒）。Opus は 2500/5000/10000/20000/40000/60000。
    pub frame_duration_us: u32,
    pub application: OpusApplication,
    pub input_format: SampleFormat,
    pub track: TrackId,
}

impl AudioEncoderConfig {
    pub fn opus(sample_rate: u32, channels: u8, bitrate_bps: u32) -> Self {
        AudioEncoderConfig {
            codec: Codec::Opus,
            sample_rate,
            channels,
            bitrate_bps,
            frame_duration_us: 20_000,
            application: OpusApplication::Audio,
            input_format: SampleFormat::F32,
            track: TrackId(0),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.codec.kind() != MediaKind::Audio {
            return Err(CodecError::invalid_config(format!(
                "{} is not an audio codec",
                self.codec
            )));
        }
        if self.channels == 0 || self.channels > crate::frame::MAX_AUDIO_CHANNELS {
            return Err(CodecError::invalid_config("bad channel count"));
        }
        if self.sample_rate == 0 || self.sample_rate > 384_000 {
            return Err(CodecError::invalid_config("bad sample rate"));
        }
        if self.bitrate_bps > MAX_BITRATE_BPS {
            return Err(CodecError::invalid_config("bitrate exceeds limit"));
        }
        Ok(())
    }
}

/// エンコーダーの設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderConfig {
    Video(VideoEncoderConfig),
    Audio(AudioEncoderConfig),
}

impl EncoderConfig {
    pub fn codec(&self) -> Codec {
        match self {
            EncoderConfig::Video(v) => v.codec,
            EncoderConfig::Audio(a) => a.codec,
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            EncoderConfig::Video(v) => v.validate(),
            EncoderConfig::Audio(a) => a.validate(),
        }
    }

    pub fn resolution(&self) -> Option<(u32, u32)> {
        match self {
            EncoderConfig::Video(v) => Some((v.width, v.height)),
            EncoderConfig::Audio(_) => None,
        }
    }
}

/// デコーダーの設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecoderConfig {
    pub codec: Codec,
    /// 入力トラックの設定（avcC・hvcC・av1C・OpusHead など）。H.264/H.265 では必須（AU にパラメーターセットが
    /// 入っていれば無くても良いが、長さ前置のバイト数はここから決まる。無ければ 4）。
    pub config: Option<CodecConfig>,
    /// 出力の画素形式の希望。`None` ならプロバイダーの自然な形式（8 bit は I420 か NV12、10 bit は P010）。
    pub output_format: Option<PixelFormat>,
    /// 受け付ける解像度の上限（これを超えるストリームは `InvalidInput` で止める）。
    pub max_width: u32,
    pub max_height: u32,
    /// ソフトウェアコーデックのスレッド数（0 = 自動）。
    pub threads: u32,
    /// 音声の出力形式。
    pub sample_format: SampleFormat,
    /// 音声の出力標本化周波数（Opus は 8000/12000/16000/24000/48000）。0 なら 48000。
    pub sample_rate: u32,
    /// 音声のチャンネル数（0 なら設定から決める）。
    pub channels: u8,
}

impl DecoderConfig {
    pub fn new(codec: Codec) -> Self {
        DecoderConfig {
            codec,
            config: None,
            output_format: None,
            max_width: 7680,
            max_height: 4320,
            threads: 0,
            sample_format: SampleFormat::F32,
            sample_rate: 0,
            channels: 0,
        }
    }

    pub fn with_config(mut self, config: CodecConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(c) = &self.config
            && c.codec != self.codec
        {
            return Err(CodecError::invalid_config(
                "codec config does not match codec",
            ));
        }
        if self.codec.kind() == MediaKind::Video {
            check_dimensions(self.max_width, self.max_height)?;
        }
        if self.threads > 256 {
            return Err(CodecError::invalid_config("too many threads"));
        }
        Ok(())
    }

    /// 解像度が上限内か。
    pub fn check_resolution(&self, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 || width > self.max_width || height > self.max_height {
            return Err(CodecError::invalid_input(format!(
                "stream resolution {width}x{height} outside allowed range"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_video() {
        let mut c = VideoEncoderConfig::new(Codec::Vp8, 640, 480, 30, 1_000_000);
        assert!(c.validate().is_ok());
        c.profile = Some(VideoProfile::H264High);
        assert!(c.validate().is_err());
        c.profile = None;
        c.codec = Codec::Opus;
        assert!(c.validate().is_err());
        c.codec = Codec::Vp8;
        c.fps_num = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn profile_roundtrip() {
        for v in 0..=8u8 {
            assert_eq!(VideoProfile::from_u8(v).unwrap().to_u8(), v);
        }
        assert!(VideoProfile::from_u8(9).is_none());
    }
}
