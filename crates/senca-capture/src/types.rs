//! 機器の説明・能力・要求・取り込んだフレーム。
//!
//! 説明と能力は JSON にして `GET /devices` にそのまま出せる形にする（`serde`）。

use bytes::Bytes;
use senca_codec::{AudioFrame, VideoFrame};
use serde::{Deserialize, Serialize};
use std::fmt;

/// 機器の種類（バックエンド）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    V4l2,
    Alsa,
    Decklink,
    Synthetic,
}

impl Backend {
    pub const ALL: [Backend; 4] = [
        Backend::V4l2,
        Backend::Alsa,
        Backend::Decklink,
        Backend::Synthetic,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Backend::V4l2 => "v4l2",
            Backend::Alsa => "alsa",
            Backend::Decklink => "decklink",
            Backend::Synthetic => "synthetic",
        }
    }

    pub fn parse(s: &str) -> Option<Backend> {
        Backend::ALL.into_iter().find(|b| b.as_str() == s)
    }

    /// 実機で試験したか（REQ-004「実機が無いものは未検証と表示する」）。
    pub fn verification(self) -> Verification {
        match self {
            Backend::Synthetic => Verification::Verified,
            Backend::V4l2 | Backend::Alsa | Backend::Decklink => Verification::Unverified,
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 実機での検証の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verification {
    Verified,
    Unverified,
}

/// 機器が出す生の形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RawFormat {
    /// 8 bit 4:2:0 の 3 面（V4L2 `YU12`）。
    I420,
    /// 8 bit 4:2:0、Y と UV 交互（V4L2 `NV12`）。
    Nv12,
    /// 8 bit 4:2:2 の詰めた形式 Y0 U Y1 V（V4L2 `YUYV`、UVC の非圧縮）。
    Yuyv,
    /// 8 bit 4:2:2 の詰めた形式 U Y0 V Y1（V4L2 `UYVY`、DeckLink の `bmdFormat8BitYUV`）。
    Uyvy,
    /// Motion JPEG（1 フレーム 1 枚の JPEG）。
    Mjpeg,
    /// H.264 の Annex B（UVC カメラが符号化して出すもの。素通しする）。
    H264,
    /// 16 bit 符号付き PCM（インターリーブ）。
    #[serde(rename = "pcm_s16")]
    PcmS16,
}

impl RawFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            RawFormat::I420 => "i420",
            RawFormat::Nv12 => "nv12",
            RawFormat::Yuyv => "yuyv",
            RawFormat::Uyvy => "uyvy",
            RawFormat::Mjpeg => "mjpeg",
            RawFormat::H264 => "h264",
            RawFormat::PcmS16 => "pcm_s16",
        }
    }

    pub fn parse(s: &str) -> Option<RawFormat> {
        Some(match s {
            "i420" => RawFormat::I420,
            "nv12" => RawFormat::Nv12,
            "yuyv" => RawFormat::Yuyv,
            "uyvy" => RawFormat::Uyvy,
            "mjpeg" => RawFormat::Mjpeg,
            "h264" => RawFormat::H264,
            "pcm_s16" => RawFormat::PcmS16,
            _ => return None,
        })
    }

    pub fn is_video(self) -> bool {
        !matches!(self, RawFormat::PcmS16)
    }

    /// 符号化済み（素通しの対象）か。
    pub fn is_compressed_video(self) -> bool {
        matches!(self, RawFormat::H264)
    }
}

/// フレームレート（`num / den` 枚毎秒）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fps {
    pub num: u32,
    pub den: u32,
}

impl Fps {
    pub const fn new(num: u32, den: u32) -> Self {
        Fps { num, den }
    }
    pub const fn integer(n: u32) -> Self {
        Fps { num: n, den: 1 }
    }
    pub fn as_f64(self) -> f64 {
        if self.den == 0 {
            0.0
        } else {
            self.num as f64 / self.den as f64
        }
    }
    pub fn is_valid(self) -> bool {
        self.num > 0 && self.den > 0 && self.as_f64() <= 1000.0
    }
    /// 1 フレームの長さ（ナノ秒）。
    pub fn frame_duration_ns(self) -> i64 {
        if self.num == 0 {
            return 0;
        }
        (self.den as i128 * 1_000_000_000 / self.num as i128) as i64
    }
}

impl fmt::Display for Fps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.den == 1 {
            write!(f, "{}", self.num)
        } else {
            write!(f, "{}/{}", self.num, self.den)
        }
    }
}

/// 対応する解像度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SizeRange {
    Discrete {
        width: u32,
        height: u32,
    },
    /// `min..=max` を `step` 刻み（連続なら step = 1）。
    Stepwise {
        min_width: u32,
        max_width: u32,
        step_width: u32,
        min_height: u32,
        max_height: u32,
        step_height: u32,
    },
}

/// 対応するフレームレート。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum IntervalRange {
    Discrete {
        fps: Vec<Fps>,
    },
    /// `min..=max` の任意の値（V4L2 の stepwise・continuous）。
    Range {
        min: Fps,
        max: Fps,
    },
}

/// 映像の能力（形式ごと・解像度ごと）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoCapability {
    pub format: RawFormat,
    pub size: SizeRange,
    pub fps: IntervalRange,
    /// DeckLink の表示モード（`'Hp30'` などの 4 文字）。他の機器では `None`。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mode: Option<String>,
    /// 飛び越し走査（DeckLink の 1080i など）。
    #[serde(default)]
    pub interlaced: bool,
}

/// 音声の能力。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioCapability {
    pub format: RawFormat,
    pub sample_rates: Vec<u32>,
    pub channels: Vec<u8>,
}

/// 検出した機器。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceDescriptor {
    /// 機器の識別子（`v4l2:/dev/video0`、`alsa:hw:1,0`、`decklink:0`、`synthetic:0`）。
    pub id: String,
    pub backend: Backend,
    pub name: String,
    /// 機器のパス（V4L2 の装置ファイル、ALSA の PCM 名）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
    /// ドライバー名と版（分かるとき）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub driver: Option<String>,
    /// バスの位置（USB の位置など。映像と音声の機器を対応付けるのに使える）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bus_info: Option<String>,
    pub video: Vec<VideoCapability>,
    pub audio: Vec<AudioCapability>,
    pub verification: Verification,
    /// 補足（使えない理由など）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub note: Option<String>,
}

impl DeviceDescriptor {
    pub fn has_video(&self) -> bool {
        !self.video.is_empty()
    }
    pub fn has_audio(&self) -> bool {
        !self.audio.is_empty()
    }
}

/// バックエンドの検出結果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendStatus {
    pub backend: Backend,
    pub available: bool,
    /// 使えない理由、または読み込んだライブラリの説明。
    pub detail: String,
    pub verification: Verification,
}

/// 映像の要求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoRequest {
    pub width: u32,
    pub height: u32,
    pub fps: Fps,
    /// 使ってよい形式（優先順）。空なら自動（非圧縮 → MJPEG の順）。
    #[serde(default)]
    pub formats: Vec<RawFormat>,
}

/// 音声の要求。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioRequest {
    pub sample_rate: u32,
    pub channels: u8,
}

/// 機器を開始するときの要求（映像・音声のどちらか、または両方）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureConfig {
    #[serde(default)]
    pub video: Option<VideoRequest>,
    #[serde(default)]
    pub audio: Option<AudioRequest>,
}

/// 取り決めた映像の形式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoMode {
    pub format: RawFormat,
    pub width: u32,
    pub height: u32,
    pub fps: Fps,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mode: Option<String>,
}

/// 取り決めた音声の形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioMode {
    pub format: RawFormat,
    pub sample_rate: u32,
    pub channels: u8,
}

/// 機器が開始した形式。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Negotiated {
    pub video: Option<VideoMode>,
    pub audio: Option<AudioMode>,
}

/// フレームの時刻。
///
/// - `monotonic_ns`: `CLOCK_MONOTONIC`（ナノ秒）。機器をまたいだ同期（映像と別機器の音声）はこれで行う。
///   V4L2 はドライバーの単調時刻（`V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC` のとき）、それ以外は受け取った時刻から
///   機器の中の遅延を引いた推定。
/// - `device_ns`: 機器の時計（DeckLink のハードウェア参照時刻、V4L2 のバッファの時刻、ALSA は無し）。
/// - `sequence`: 機器のフレーム番号（欠落の検出に使う。分からなければ受け取った順）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timestamps {
    pub monotonic_ns: i64,
    pub device_ns: Option<i64>,
    pub sequence: u64,
}

/// 機器から取り込んだもの（共通メディアモデルに変換済み）。
///
/// 映像フレーム・音声フレームの `pts` は `monotonic_ns`（時間軸 1/1e9）。
#[derive(Debug, Clone)]
pub enum CapturedFrame {
    /// 非圧縮の映像（I420 にそろえる）。
    Video { frame: VideoFrame, ts: Timestamps },
    /// 音声（インターリーブの i16）。
    Audio { frame: AudioFrame, ts: Timestamps },
    /// 機器が符号化した映像（UVC の H.264、Annex B の 1 アクセスユニット）。素通しする。
    H264 { annexb: Bytes, ts: Timestamps },
}

impl CapturedFrame {
    pub fn timestamps(&self) -> &Timestamps {
        match self {
            CapturedFrame::Video { ts, .. }
            | CapturedFrame::Audio { ts, .. }
            | CapturedFrame::H264 { ts, .. } => ts,
        }
    }

    pub fn is_video(&self) -> bool {
        !matches!(self, CapturedFrame::Audio { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_roundtrip() {
        for b in Backend::ALL {
            assert_eq!(Backend::parse(b.as_str()), Some(b));
        }
        for f in [
            RawFormat::I420,
            RawFormat::Nv12,
            RawFormat::Yuyv,
            RawFormat::Uyvy,
            RawFormat::Mjpeg,
            RawFormat::H264,
            RawFormat::PcmS16,
        ] {
            assert_eq!(RawFormat::parse(f.as_str()), Some(f));
            let j = serde_json::to_string(&f).unwrap();
            assert_eq!(j, format!("\"{}\"", f.as_str()));
        }
        assert_eq!(Fps::new(30000, 1001).frame_duration_ns(), 33_366_666);
        assert!(!Fps::new(0, 1).is_valid());
    }

    #[test]
    fn descriptor_json() {
        let d = DeviceDescriptor {
            id: "synthetic:0".into(),
            backend: Backend::Synthetic,
            name: "x".into(),
            path: None,
            driver: None,
            bus_info: None,
            video: vec![VideoCapability {
                format: RawFormat::Yuyv,
                size: SizeRange::Discrete {
                    width: 640,
                    height: 480,
                },
                fps: IntervalRange::Discrete {
                    fps: vec![Fps::integer(30)],
                },
                mode: None,
                interlaced: false,
            }],
            audio: vec![],
            verification: Verification::Verified,
            note: None,
        };
        let j = serde_json::to_value(&d).unwrap();
        assert_eq!(j["backend"], "synthetic");
        assert_eq!(j["video"][0]["size"]["type"], "discrete");
        let back: DeviceDescriptor = serde_json::from_value(j).unwrap();
        assert_eq!(back, d);
    }
}
