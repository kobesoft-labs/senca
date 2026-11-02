//! 取り込みの指定（JSON）。ノードが capture worker に渡す（`SessionOpen.graph` の中身）。
//!
//! ```json
//! {
//!   "video_device": "v4l2:/dev/video0",
//!   "audio_device": "alsa:hw:1,0",
//!   "video": {"width": 1280, "height": 720, "fps": 30, "codec": "vp9", "bitrate_bps": 2000000},
//!   "audio": {"sample_rate": 48000, "channels": 2, "codec": "opus", "bitrate_bps": 96000}
//! }
//! ```
//!
//! - `audio_device` を省くと、`video_device` が音声も持つ機器（DeckLink・試験用）ならそれを使う。
//! - `fps` は整数か `{"num": 30000, "den": 1001}`。
//! - `formats` は機器の形式の優先順（省けば自動）。`passthrough: true` は UVC の H.264 をそのまま使う
//!   （`codec` は `h264`、`formats` は `["h264"]`）。
//! - トラック番号は映像 0、音声 1（映像が無ければ音声 0）。
//! - 未知の項目は拒否する。

use crate::device::{CaptureDevice, DeviceRegistry};
use crate::error::{CaptureError, Result};
use crate::session::{AudioOutput, DeviceRole, SessionConfig, VideoOutput};
use crate::synthetic::SyntheticOptions;
use crate::types::{AudioRequest, Fps, RawFormat, VideoRequest};
use senca_codec::registry::Policy;
use senca_core::{Codec, TrackId};
use serde::{Deserialize, Serialize};

/// 機器の識別子の長さの上限。
pub const MAX_DEVICE_ID: usize = 256;

/// フレームレートの書き方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FpsSpec {
    Integer(u32),
    Fraction(Fps),
}

impl FpsSpec {
    pub fn to_fps(self) -> Fps {
        match self {
            FpsSpec::Integer(n) => Fps::integer(n),
            FpsSpec::Fraction(f) => f,
        }
    }
}

/// 映像の指定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoSpec {
    pub width: u32,
    pub height: u32,
    pub fps: FpsSpec,
    #[serde(default)]
    pub formats: Vec<RawFormat>,
    pub codec: String,
    pub bitrate_bps: u32,
    #[serde(default)]
    pub gop: Option<u32>,
    #[serde(default)]
    pub passthrough: bool,
    #[serde(default)]
    pub threads: u32,
}

fn default_rate() -> u32 {
    48000
}
fn default_channels() -> u8 {
    2
}
fn default_opus() -> String {
    "opus".into()
}
fn default_abr() -> u32 {
    96_000
}

/// 音声の指定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioSpec {
    #[serde(default = "default_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_channels")]
    pub channels: u8,
    #[serde(default = "default_opus")]
    pub codec: String,
    #[serde(default = "default_abr")]
    pub bitrate_bps: u32,
}

/// 取り込みの指定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureSpec {
    #[serde(default)]
    pub video_device: Option<String>,
    #[serde(default)]
    pub audio_device: Option<String>,
    #[serde(default)]
    pub video: Option<VideoSpec>,
    #[serde(default)]
    pub audio: Option<AudioSpec>,
    /// 試験用の機器の設定（`synthetic:0` のときだけ使う）。
    #[serde(default)]
    pub synthetic: Option<SyntheticOptions>,
}

fn codec(name: &str) -> Result<Codec> {
    Codec::parse(name)
        .ok_or_else(|| CaptureError::invalid_config(format!("unknown codec {name:?}")))
}

impl CaptureSpec {
    pub fn from_json(s: &str) -> Result<CaptureSpec> {
        let spec: CaptureSpec = serde_json::from_str(s)
            .map_err(|e| CaptureError::invalid_config(format!("capture spec: {e}")))?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("serializable")
    }

    /// 形の検査（機器・符号化器の有無は見ない）。
    pub fn validate(&self) -> Result<()> {
        for id in [&self.video_device, &self.audio_device]
            .into_iter()
            .flatten()
        {
            if id.is_empty() || id.len() > MAX_DEVICE_ID || id.chars().any(|c| c.is_control()) {
                return Err(CaptureError::invalid_config("bad device id"));
            }
        }
        if self.video.is_none() && self.audio.is_none() {
            return Err(CaptureError::invalid_config(
                "capture spec needs video and/or audio",
            ));
        }
        if self.video.is_some() && self.video_device.is_none() {
            return Err(CaptureError::invalid_config("video requires video_device"));
        }
        if self.audio.is_some() && self.audio_device.is_none() && self.video_device.is_none() {
            return Err(CaptureError::invalid_config(
                "audio requires audio_device or video_device",
            ));
        }
        if let Some(v) = &self.video {
            codec(&v.codec)?;
            if v.formats.len() > 8 {
                return Err(CaptureError::invalid_config("too many formats"));
            }
        }
        if let Some(a) = &self.audio {
            codec(&a.codec)?;
        }
        self.session_config(Policy::default()).map(|_| ())
    }

    /// セッションの設定にする。
    pub fn session_config(&self, policy: Policy) -> Result<SessionConfig> {
        let video = match &self.video {
            Some(v) => Some((
                VideoRequest {
                    width: v.width,
                    height: v.height,
                    fps: v.fps.to_fps(),
                    formats: v.formats.clone(),
                },
                VideoOutput {
                    codec: codec(&v.codec)?,
                    bitrate_bps: v.bitrate_bps,
                    gop: v.gop,
                    passthrough: v.passthrough,
                    threads: v.threads,
                    track: TrackId(0),
                },
            )),
            None => None,
        };
        let audio = match &self.audio {
            Some(a) => Some((
                AudioRequest {
                    sample_rate: a.sample_rate,
                    channels: a.channels,
                },
                AudioOutput {
                    codec: codec(&a.codec)?,
                    bitrate_bps: a.bitrate_bps,
                    track: TrackId(if video.is_some() { 1 } else { 0 }),
                },
            )),
            None => None,
        };
        Ok(SessionConfig {
            video,
            audio,
            policy,
        })
    }

    /// 機器を開き、役割を決める。
    pub fn open_devices(
        &self,
        registry: &DeviceRegistry,
    ) -> Result<Vec<(Box<dyn CaptureDevice>, DeviceRole)>> {
        let mut out: Vec<(Box<dyn CaptureDevice>, DeviceRole)> = Vec::new();
        let want_audio = self.audio.is_some();
        let separate_audio = self
            .audio_device
            .as_ref()
            .filter(|a| Some(*a) != self.video_device.as_ref());
        if let Some(id) = &self.video_device {
            let dev = registry.open(id)?;
            let audio_here = want_audio && separate_audio.is_none();
            if audio_here && !dev.descriptor().has_audio() {
                return Err(CaptureError::invalid_config(format!(
                    "{id} has no audio input; set audio_device (e.g. the matching ALSA device)"
                )));
            }
            let video_here = self.video.is_some();
            if video_here || audio_here {
                out.push((
                    dev,
                    DeviceRole {
                        video: video_here,
                        audio: audio_here,
                    },
                ));
            }
        }
        if want_audio && let Some(id) = separate_audio {
            let dev = registry.open(id)?;
            if !dev.descriptor().has_audio() {
                return Err(CaptureError::invalid_config(format!(
                    "{id} has no audio input"
                )));
            }
            out.push((
                dev,
                DeviceRole {
                    video: false,
                    audio: true,
                },
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_validate() {
        let s = CaptureSpec::from_json(
            r#"{"video_device":"synthetic:0","video":{"width":320,"height":240,"fps":{"num":30000,"den":1001},"codec":"vp9","bitrate_bps":500000},"audio":{}}"#,
        )
        .unwrap();
        assert_eq!(
            s.video.as_ref().unwrap().fps.to_fps(),
            Fps::new(30000, 1001)
        );
        let a = s.audio.as_ref().unwrap();
        assert_eq!(
            (a.sample_rate, a.channels, a.codec.as_str(), a.bitrate_bps),
            (48000, 2, "opus", 96000)
        );
        let c = s.session_config(Policy::default()).unwrap();
        assert_eq!(c.audio.unwrap().1.track, TrackId(1));
        let back = CaptureSpec::from_json(&s.to_json()).unwrap();
        assert_eq!(back, s);
        for bad in [
            r#"{}"#,
            r#"{"video":{"width":1,"height":1,"fps":1,"codec":"vp9","bitrate_bps":1}}"#,
            r#"{"video_device":"x","video":{"width":1,"height":1,"fps":1,"codec":"nope","bitrate_bps":1}}"#,
            r#"{"video_device":"x","bogus":1,"audio":{}}"#,
            r#"{"video_device":"","audio":{}}"#,
        ] {
            assert!(CaptureSpec::from_json(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn device_roles() {
        let mut o = crate::device::ProbeOptions {
            dev_dir: "/nonexistent-senca".into(),
            proc_asound: "/nonexistent-senca".into(),
            ..Default::default()
        };
        o.synthetic = true;
        o.libs.dirs_only = true;
        let r = DeviceRegistry::probe(&o);
        let s = CaptureSpec::from_json(
            r#"{"video_device":"synthetic:0","video":{"width":64,"height":64,"fps":10,"codec":"vp9","bitrate_bps":100000},"audio":{}}"#,
        )
        .unwrap();
        let d = s.open_devices(&r).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(
            d[0].1,
            DeviceRole {
                video: true,
                audio: true
            }
        );
        // 音声だけ
        let s = CaptureSpec::from_json(r#"{"audio_device":"synthetic:0","audio":{}}"#).unwrap();
        let d = s.open_devices(&r).unwrap();
        assert_eq!(
            d[0].1,
            DeviceRole {
                video: false,
                audio: true
            }
        );
        // 別の音声機器（無いもの）
        let s = CaptureSpec::from_json(r#"{"video_device":"synthetic:0","audio_device":"alsa:hw:0,0","video":{"width":64,"height":64,"fps":10,"codec":"vp9","bitrate_bps":1},"audio":{}}"#).unwrap();
        assert!(s.open_devices(&r).is_err());
    }
}
