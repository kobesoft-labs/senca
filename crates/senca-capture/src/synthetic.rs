//! 試験用の機器（決定的な試験パターンと音）。実機なしで取り込み → 変換 → 符号化の全体を通す。
//!
//! - 映像: 75% のカラーバー（BT.601 限定範囲）、横に動く白い四角、下端にフレーム番号の 2 進表示。
//!   フレーム番号だけで決まる（何度作っても同じ）。
//! - 音声: 1 kHz の正弦波（-12 dBFS）。標本番号だけで決まる。10 ms ごとに 1 つのフレームを出す。
//! - 機器の形式: I420・NV12・YUYV・UYVY・MJPEG を名乗り、取り決めた形式に詰めてから
//!   実機と同じ変換（[`crate::convert::decode_raw_video`]）で I420 に戻す。変換の経路の試験を兼ねる。
//! - 時刻: 開始時の単調時刻を起点に、フレーム番号 × 間隔の仮想の時計。`realtime` なら実時間に合わせて待つ。

use crate::clock;
use crate::convert;
use crate::device::{CaptureBackend, CaptureDevice};
use crate::error::{CaptureError, Result};
use crate::negotiate::{choose_audio, choose_video};
use crate::types::{
    AudioCapability, AudioMode, Backend, BackendStatus, CaptureConfig, CapturedFrame,
    DeviceDescriptor, Fps, IntervalRange, Negotiated, RawFormat, SizeRange, Timestamps,
    Verification, VideoCapability, VideoMode,
};
use bytes::Bytes;
use senca_codec::{AudioFrame, PixelFormat, SampleFormat, VideoFrame};
use senca_core::Timebase;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 試験用の機器の設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyntheticOptions {
    /// 実時間に合わせて出す（`false` ならできるだけ速く出す）。
    #[serde(default = "default_true")]
    pub realtime: bool,
    /// 映像をこの枚数出したら終わる（音声も同じ長さで終わる）。`None` なら終わらない。
    #[serde(default)]
    pub max_video_frames: Option<u64>,
    /// 音の周波数（Hz）。
    #[serde(default = "default_tone")]
    pub tone_hz: u32,
}

fn default_true() -> bool {
    true
}
fn default_tone() -> u32 {
    1000
}

impl Default for SyntheticOptions {
    fn default() -> Self {
        SyntheticOptions {
            realtime: true,
            max_video_frames: None,
            tone_hz: 1000,
        }
    }
}

/// 機器の識別子。
pub const DEVICE_ID: &str = "synthetic:0";

/// 試験用のバックエンド（機器は 1 つ）。
#[derive(Debug, Clone, Default)]
pub struct SyntheticBackend {
    pub options: SyntheticOptions,
}

impl SyntheticBackend {
    pub fn new(options: SyntheticOptions) -> Self {
        SyntheticBackend { options }
    }
}

/// 試験用の機器の説明。
pub fn descriptor() -> DeviceDescriptor {
    let size = SizeRange::Stepwise {
        min_width: 16,
        max_width: 3840,
        step_width: 2,
        min_height: 16,
        max_height: 2160,
        step_height: 2,
    };
    let fps = IntervalRange::Range {
        min: Fps::integer(1),
        max: Fps::integer(120),
    };
    DeviceDescriptor {
        id: DEVICE_ID.into(),
        backend: Backend::Synthetic,
        name: "SENCA synthetic test pattern".into(),
        path: None,
        driver: Some(format!("senca-capture {}", env!("CARGO_PKG_VERSION"))),
        bus_info: None,
        video: [
            RawFormat::I420,
            RawFormat::Nv12,
            RawFormat::Yuyv,
            RawFormat::Uyvy,
            RawFormat::Mjpeg,
        ]
        .into_iter()
        .map(|format| VideoCapability {
            format,
            size,
            fps: fps.clone(),
            mode: None,
            interlaced: false,
        })
        .collect(),
        audio: vec![AudioCapability {
            format: RawFormat::PcmS16,
            sample_rates: vec![48000, 44100, 16000],
            channels: vec![1, 2],
        }],
        verification: Verification::Verified,
        note: Some("deterministic test source".into()),
    }
}

impl CaptureBackend for SyntheticBackend {
    fn backend(&self) -> Backend {
        Backend::Synthetic
    }
    fn status(&self) -> BackendStatus {
        BackendStatus {
            backend: Backend::Synthetic,
            available: true,
            detail: "test pattern generator".into(),
            verification: Verification::Verified,
        }
    }
    fn enumerate(&self) -> Vec<DeviceDescriptor> {
        vec![descriptor()]
    }
    fn open(&self, id: &str) -> Result<Box<dyn CaptureDevice>> {
        if id != DEVICE_ID {
            return Err(CaptureError::not_found(format!(
                "no synthetic device {id:?}"
            )));
        }
        Ok(Box::new(SyntheticDevice::new(self.options.clone())))
    }
}

/// 75% のカラーバー（BT.601 限定範囲の Y, U, V）。
pub const BARS: [(u8, u8, u8); 8] = [
    (180, 128, 128),
    (162, 44, 142),
    (131, 156, 44),
    (112, 72, 58),
    (84, 184, 198),
    (65, 100, 212),
    (35, 212, 114),
    (16, 128, 128),
];

/// フレーム番号 `idx` の試験パターン（詰めた I420）。
pub fn pattern_i420(width: u32, height: u32, idx: u64) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut out = vec![0u8; w * h + 2 * cw * ch];
    let bar = |x: usize| BARS[(x * 8 / w.max(1)).min(7)];
    let (yp, rest) = out.split_at_mut(w * h);
    let (up, vp) = rest.split_at_mut(cw * ch);
    for y in 0..h {
        for x in 0..w {
            yp[y * w + x] = bar(x).0;
        }
    }
    for y in 0..ch {
        for x in 0..cw {
            let b = bar(x * 2);
            up[y * cw + x] = b.1;
            vp[y * cw + x] = b.2;
        }
    }
    // 動く四角（輝度だけ白、色差は無彩色）
    let bs = (h / 8).max(2) & !1;
    if w > bs && h > bs {
        let x0 = ((idx as usize * 4) % (w - bs)) & !1;
        let y0 = (h / 2 - bs / 2) & !1;
        for y in y0..y0 + bs {
            for x in x0..x0 + bs {
                yp[y * w + x] = 235;
            }
        }
        for y in y0 / 2..(y0 + bs) / 2 {
            for x in x0 / 2..(x0 + bs) / 2 {
                up[y * cw + x] = 128;
                vp[y * cw + x] = 128;
            }
        }
    }
    // 下端にフレーム番号の 2 進（16 bit、左が最上位）
    let cell = (w / 16).max(1);
    let strip = (h / 16).max(1);
    for y in h - strip..h {
        for x in 0..(cell * 16).min(w) {
            let bit = 15 - x / cell;
            yp[y * w + x] = if (idx >> bit) & 1 == 1 { 235 } else { 16 };
        }
    }
    out
}

/// 標本番号 `start` からの音（i16 インターリーブ）。
pub fn tone_s16(sample_rate: u32, channels: u8, start: u64, n: usize, hz: u32) -> Vec<u8> {
    let amp = 0.25 * 32767.0;
    let mut out = Vec::with_capacity(n * channels as usize * 2);
    for i in 0..n as u64 {
        let k = start + i;
        // 周期の中の位置を整数で求めてから三角関数に渡す（長時間でも誤差が溜まらない）
        let phase = (k * hz as u64 % sample_rate as u64) as f64 / sample_rate as f64;
        let v = (amp * (2.0 * std::f64::consts::PI * phase).sin()).round() as i16;
        for _ in 0..channels {
            out.extend_from_slice(&v.to_ne_bytes());
        }
    }
    out
}

/// 試験用の機器。
#[derive(Debug)]
pub struct SyntheticDevice {
    desc: DeviceDescriptor,
    options: SyntheticOptions,
    video: Option<VideoMode>,
    audio: Option<AudioMode>,
    t0: i64,
    next_video: u64,
    next_audio: u64,
    running: bool,
}

impl SyntheticDevice {
    pub fn new(options: SyntheticOptions) -> Self {
        SyntheticDevice {
            desc: descriptor(),
            options,
            video: None,
            audio: None,
            t0: 0,
            next_video: 0,
            next_audio: 0,
            running: false,
        }
    }

    fn video_due(&self) -> Option<i64> {
        let v = self.video.as_ref()?;
        if self
            .options
            .max_video_frames
            .is_some_and(|m| self.next_video >= m)
        {
            return None;
        }
        Some(self.t0 + (self.next_video as i128 * v.fps.frame_duration_ns() as i128) as i64)
    }

    fn audio_due(&self) -> Option<i64> {
        let a = self.audio.as_ref()?;
        let t = self.t0 + (self.next_audio as i128 * 1_000_000_000 / a.sample_rate as i128) as i64;
        if let (Some(m), Some(v)) = (self.options.max_video_frames, self.video.as_ref())
            && t >= self.t0 + (m as i128 * v.fps.frame_duration_ns() as i128) as i64
        {
            return None;
        }
        if self.options.max_video_frames.is_some() && self.video.is_none() {
            // 映像なしで枚数指定なら、その枚数 × 10 ms で終わる
            let m = self.options.max_video_frames.unwrap_or(0);
            if self.next_audio >= m * (a.sample_rate as u64 / 100) {
                return None;
            }
        }
        Some(t)
    }

    fn make_video(&mut self, due: i64) -> Result<CapturedFrame> {
        let v = self.video.clone().expect("video mode");
        let idx = self.next_video;
        self.next_video += 1;
        let i420 = pattern_i420(v.width, v.height, idx);
        let (raw, stride) = match v.format {
            RawFormat::I420 => (i420, v.width as usize),
            RawFormat::Nv12 => {
                let f = VideoFrame::from_packed(
                    PixelFormat::I420,
                    v.width,
                    v.height,
                    Bytes::from(i420),
                    0,
                    Timebase::NANOS,
                )?;
                (f.convert(PixelFormat::Nv12)?.to_packed()?, v.width as usize)
            }
            RawFormat::Yuyv => (
                convert::i420_to_yuyv(&i420, v.width, v.height)?,
                v.width as usize * 2,
            ),
            RawFormat::Uyvy => (
                convert::i420_to_uyvy(&i420, v.width, v.height)?,
                v.width as usize * 2,
            ),
            RawFormat::Mjpeg => (convert::i420_to_mjpeg(&i420, v.width, v.height, 90)?, 0),
            RawFormat::H264 | RawFormat::PcmS16 => {
                return Err(CaptureError::internal("synthetic: bad video format"));
            }
        };
        let frame = convert::decode_raw_video(
            v.format,
            v.width,
            v.height,
            stride,
            &raw,
            due,
            Timebase::NANOS,
        )?;
        Ok(CapturedFrame::Video {
            frame,
            ts: Timestamps {
                monotonic_ns: due,
                device_ns: Some(due - self.t0),
                sequence: idx,
            },
        })
    }

    fn make_audio(&mut self, due: i64) -> CapturedFrame {
        let a = self.audio.expect("audio mode");
        let n = (a.sample_rate / 100) as usize;
        let start = self.next_audio;
        self.next_audio += n as u64;
        let data = tone_s16(a.sample_rate, a.channels, start, n, self.options.tone_hz);
        CapturedFrame::Audio {
            frame: AudioFrame {
                format: SampleFormat::I16,
                sample_rate: a.sample_rate,
                channels: a.channels,
                data: Bytes::from(data),
                pts: due,
                timebase: Timebase::NANOS,
            },
            ts: Timestamps {
                monotonic_ns: due,
                device_ns: Some(due - self.t0),
                sequence: start / n as u64,
            },
        }
    }
}

impl CaptureDevice for SyntheticDevice {
    fn descriptor(&self) -> &DeviceDescriptor {
        &self.desc
    }

    fn start(&mut self, config: &CaptureConfig) -> Result<Negotiated> {
        if self.running {
            return Err(CaptureError::new(
                crate::ErrorKind::Busy,
                "synthetic device already started",
            ));
        }
        if config.video.is_none() && config.audio.is_none() {
            return Err(CaptureError::invalid_config("nothing to capture"));
        }
        self.video = config
            .video
            .as_ref()
            .map(|r| choose_video(&self.desc.video, r))
            .transpose()?;
        self.audio = config
            .audio
            .as_ref()
            .map(|r| choose_audio(&self.desc.audio, r))
            .transpose()?;
        self.t0 = clock::monotonic_ns();
        self.next_video = 0;
        self.next_audio = 0;
        self.running = true;
        Ok(Negotiated {
            video: self.video.clone(),
            audio: self.audio,
        })
    }

    fn read(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>> {
        if !self.running {
            return Err(CaptureError::internal("synthetic device not started"));
        }
        let (due, is_video) = match (self.video_due(), self.audio_due()) {
            (None, None) => return Err(CaptureError::end_of_stream()),
            (Some(v), None) => (v, true),
            (None, Some(a)) => (a, false),
            (Some(v), Some(a)) => {
                if v <= a {
                    (v, true)
                } else {
                    (a, false)
                }
            }
        };
        if self.options.realtime {
            let wait = due - clock::monotonic_ns();
            if wait > 0 {
                let wait = Duration::from_nanos(wait as u64);
                if wait > timeout {
                    std::thread::sleep(timeout);
                    return Ok(None);
                }
                std::thread::sleep(wait);
            }
        }
        Ok(Some(if is_video {
            self.make_video(due)?
        } else {
            self.make_audio(due)
        }))
    }

    fn stop(&mut self) -> Result<()> {
        self.running = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AudioRequest, VideoRequest};

    fn cfg(format: RawFormat) -> CaptureConfig {
        CaptureConfig {
            video: Some(VideoRequest {
                width: 64,
                height: 48,
                fps: Fps::integer(25),
                formats: vec![format],
            }),
            audio: Some(AudioRequest {
                sample_rate: 48000,
                channels: 2,
            }),
        }
    }

    fn opts(n: u64) -> SyntheticOptions {
        SyntheticOptions {
            realtime: false,
            max_video_frames: Some(n),
            tone_hz: 1000,
        }
    }

    #[test]
    fn deterministic_and_ordered() {
        let mut d = SyntheticDevice::new(opts(5));
        let n = d.start(&cfg(RawFormat::I420)).unwrap();
        assert_eq!(n.video.as_ref().unwrap().width, 64);
        let mut video = 0;
        let mut audio_samples = 0;
        let mut last = i64::MIN;
        loop {
            match d.read(Duration::from_millis(10)) {
                Ok(Some(f)) => {
                    let ts = *f.timestamps();
                    assert!(ts.monotonic_ns >= last);
                    last = ts.monotonic_ns;
                    match f {
                        CapturedFrame::Video { frame, ts } => {
                            assert_eq!(
                                frame.to_packed().unwrap(),
                                pattern_i420(64, 48, ts.sequence)
                            );
                            video += 1;
                        }
                        CapturedFrame::Audio { frame, .. } => {
                            audio_samples += frame.samples_per_channel();
                        }
                        CapturedFrame::H264 { .. } => unreachable!(),
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    assert_eq!(e.kind, crate::ErrorKind::EndOfStream);
                    break;
                }
            }
        }
        assert_eq!(video, 5);
        // 5 枚 × 40 ms = 200 ms = 9600 標本
        assert_eq!(audio_samples, 9600);
        assert_ne!(pattern_i420(64, 48, 1), pattern_i420(64, 48, 2));
    }

    #[test]
    fn every_raw_format_goes_through_conversion() {
        for f in [
            RawFormat::Nv12,
            RawFormat::Yuyv,
            RawFormat::Uyvy,
            RawFormat::Mjpeg,
        ] {
            let mut d = SyntheticDevice::new(opts(1));
            let n = d.start(&cfg(f)).unwrap();
            assert_eq!(n.video.unwrap().format, f);
            let got = loop {
                if let Some(CapturedFrame::Video { frame, .. }) =
                    d.read(Duration::from_millis(10)).unwrap()
                {
                    break frame;
                }
            };
            let want = pattern_i420(64, 48, 0);
            let got = got.to_packed().unwrap();
            if f == RawFormat::Mjpeg {
                let se: f64 = got
                    .iter()
                    .zip(&want)
                    .map(|(a, b)| (*a as f64 - *b as f64).powi(2))
                    .sum();
                let psnr = 10.0 * (255.0f64.powi(2) / (se / want.len() as f64)).log10();
                assert!(psnr > 28.0, "mjpeg psnr {psnr}");
            } else {
                // 色差は縦に同じ値なので 4:2:2 経由でも一致する
                assert_eq!(got, want, "{f:?}");
            }
        }
    }

    #[test]
    fn tone_is_deterministic() {
        let a = tone_s16(48000, 2, 0, 480, 1000);
        let b = tone_s16(48000, 2, 0, 480, 1000);
        assert_eq!(a, b);
        assert_eq!(a.len(), 480 * 4);
        // 1 kHz は 48 標本で 1 周期
        assert_eq!(
            tone_s16(48000, 1, 48, 10, 1000),
            tone_s16(48000, 1, 0, 10, 1000)
        );
    }

    #[test]
    fn realtime_respects_timeout() {
        let mut d = SyntheticDevice::new(SyntheticOptions {
            realtime: true,
            max_video_frames: None,
            tone_hz: 440,
        });
        d.start(&CaptureConfig {
            video: Some(VideoRequest {
                width: 32,
                height: 32,
                fps: Fps::integer(1),
                formats: vec![],
            }),
            audio: None,
        })
        .unwrap();
        // 最初のフレームはすぐ、次は 1 秒後なので時間切れ
        assert!(d.read(Duration::from_millis(50)).unwrap().is_some());
        assert!(d.read(Duration::from_millis(20)).unwrap().is_none());
        assert!(d.start(&CaptureConfig::default()).is_err());
        d.stop().unwrap();
    }
}
