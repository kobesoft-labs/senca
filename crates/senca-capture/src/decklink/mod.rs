//! Blackmagic DeckLink（Desktop Video）の取り込み。
//!
//! - 検出: `libDeckLinkAPI.so` を dlopen し、`CreateDeckLinkIteratorInstance_000N` で装置を列挙する。
//!   ライブラリが無い（Desktop Video が入っていない）・ドライバーが動いていないときは「使えない」を理由付きで返す。
//!   各装置の表示モード（`IDeckLinkDisplayModeIterator`）を能力として出す（形式は 8 bit YUV = UYVY だけ）。
//! - 開始: 要求に最も近い表示モードを選び、`EnableVideoInput(mode, bmdFormat8BitYUV, EnableFormatDetection)`、
//!   音声は `EnableAudioInput(48 kHz, 16 bit, 2/8/16 ch)`、`StartStreams`。
//!   SDK のスレッドから呼ばれるコールバックはバッファを複製して上限付きの通り道に入れるだけ（満杯なら捨てて数える）。
//!   UYVY → I420 の変換は読み取り側のスレッドで行う。
//! - 入力の形式が変わったら（形式の自動検出）、SDK の見本と同じ手順で新しいモードで入れ直す。解像度が変われば
//!   [`crate::session::CaptureSession`] が符号化器を作り直す。
//! - 時刻: `monotonic_ns` はコールバックに届いた時刻（SDK の内部のバッファ分だけ遅れる。補正はしない）、
//!   `device_ns` はハードウェアの参照時刻、`sequence` はストリームの時刻 ÷ フレームの長さ。
//! - 飛び越し走査（1080i など）は 2 つのフィールドを織り込んだままのフレームとして出す（インターレース解除はしない）。
//! - RGB の信号は扱わない（8 bit YUV で受ける。カードが変換できない信号は未対応）。
//!
//! # 未検証
//!
//! 実機・ドライバーでの試験はしていない（この開発機に DeckLink のボードも Desktop Video も無い）。

#[allow(unsafe_code)]
pub mod ffi;

use crate::convert::decode_raw_video;
use crate::device::{CaptureBackend, CaptureDevice};
use crate::error::{CaptureError, ErrorKind, Result};
use crate::negotiate::{choose_audio, choose_video};
use crate::types::{
    AudioCapability, AudioMode, Backend, BackendStatus, CaptureConfig, CapturedFrame,
    DeviceDescriptor, Fps, IntervalRange, Negotiated, RawFormat, SizeRange, Timestamps,
    Verification, VideoCapability, VideoMode,
};
use bytes::Bytes;
use senca_codec::dl::LibrarySearch;
use senca_codec::{AudioFrame, SampleFormat};
use senca_core::Timebase;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::time::Duration;

/// 通り道の長さ（フレーム数）。
const QUEUE: usize = 16;

/// 表示モードを能力にする。
pub fn mode_capability(m: &ffi::ModeInfo) -> Option<VideoCapability> {
    if m.width == 0 || m.height == 0 || m.frame_duration <= 0 || m.time_scale <= 0 {
        return None;
    }
    let fps = Fps::new(
        u32::try_from(m.time_scale).ok()?,
        u32::try_from(m.frame_duration).ok()?,
    );
    Some(VideoCapability {
        format: RawFormat::Uyvy,
        size: SizeRange::Discrete {
            width: m.width,
            height: m.height,
        },
        fps: IntervalRange::Discrete { fps: vec![fps] },
        mode: Some(ffi::fourcc_str(m.id)),
        interlaced: m.interlaced,
    })
}

fn audio_capability() -> AudioCapability {
    AudioCapability {
        format: RawFormat::PcmS16,
        sample_rates: vec![48000],
        channels: vec![2, 8, 16],
    }
}

/// 識別子 `decklink:N` から N。
pub fn parse_id(id: &str) -> Option<u32> {
    let n = id.strip_prefix("decklink:")?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok()
}

fn describe(d: &ffi::DeviceInfo, api: &ffi::DeckLinkApi) -> DeviceDescriptor {
    let (video, note) = match &d.input {
        Ok(modes) => (modes.iter().filter_map(mode_capability).collect(), None),
        Err(e) => (Vec::new(), Some(e.clone())),
    };
    let has_input = d.input.is_ok();
    DeviceDescriptor {
        id: format!("decklink:{}", d.index),
        backend: Backend::Decklink,
        name: if d.display_name.is_empty() {
            d.model.clone()
        } else {
            d.display_name.clone()
        },
        path: None,
        driver: Some(format!("{} ({})", api.name, api.symbol)),
        bus_info: None,
        video,
        audio: if has_input {
            vec![audio_capability()]
        } else {
            Vec::new()
        },
        verification: Verification::Unverified,
        note,
    }
}

/// DeckLink のバックエンド。
#[derive(Debug, Clone)]
pub struct DeckLinkBackend {
    api: std::result::Result<Arc<ffi::DeckLinkApi>, String>,
}

impl DeckLinkBackend {
    pub fn probe(libs: &LibrarySearch) -> Self {
        DeckLinkBackend {
            api: ffi::DeckLinkApi::load(libs),
        }
    }
}

impl CaptureBackend for DeckLinkBackend {
    fn backend(&self) -> Backend {
        Backend::Decklink
    }

    fn status(&self) -> BackendStatus {
        let (available, detail) = match &self.api {
            Err(e) => (
                false,
                format!(
                    "libDeckLinkAPI.so not available (Blackmagic Desktop Video not installed): {e}"
                ),
            ),
            Ok(api) => match api.devices() {
                Ok(d) => (
                    true,
                    format!(
                        "{} via {}: {} device(s) (unverified on real hardware)",
                        api.name,
                        api.symbol,
                        d.len()
                    ),
                ),
                Err(e) => (false, format!("{}: {e}", api.name)),
            },
        };
        BackendStatus {
            backend: Backend::Decklink,
            available,
            detail,
            verification: Verification::Unverified,
        }
    }

    fn enumerate(&self) -> Vec<DeviceDescriptor> {
        let Ok(api) = &self.api else {
            return Vec::new();
        };
        api.devices()
            .unwrap_or_default()
            .iter()
            .map(|d| describe(d, api))
            .collect()
    }

    fn open(&self, id: &str) -> Result<Box<dyn CaptureDevice>> {
        let api = self
            .api
            .as_ref()
            .map_err(|e| CaptureError::unavailable(format!("DeckLink not available: {e}")))?;
        let index = parse_id(id)
            .ok_or_else(|| CaptureError::not_found(format!("bad DeckLink device id {id:?}")))?;
        let info = api
            .devices()
            .map_err(CaptureError::unavailable)?
            .into_iter()
            .find(|d| d.index == index)
            .ok_or_else(|| CaptureError::not_found(format!("no DeckLink device #{index}")))?;
        let desc = describe(&info, api);
        if let Err(e) = &info.input {
            return Err(CaptureError::unsupported(e.clone()));
        }
        Ok(Box::new(DeckLinkDevice {
            api: api.clone(),
            index,
            desc,
            handle: None,
            rx: None,
            video: None,
            audio: None,
            no_signal: 0,
        }))
    }
}

/// 開いた DeckLink の入力。
pub struct DeckLinkDevice {
    api: Arc<ffi::DeckLinkApi>,
    index: u32,
    desc: DeviceDescriptor,
    handle: Option<ffi::InputHandle>,
    rx: Option<Receiver<ffi::Event>>,
    video: Option<VideoMode>,
    audio: Option<AudioMode>,
    /// 信号の無いフレームの数。
    pub no_signal: u64,
}

impl DeckLinkDevice {
    /// コールバックが満杯で捨てたフレームの数。
    pub fn dropped(&self) -> u64 {
        self.handle.as_ref().map_or(0, |h| h.dropped())
    }

    fn convert(&mut self, ev: ffi::Event) -> Result<Option<CapturedFrame>> {
        match ev {
            ffi::Event::NoSignal => {
                self.no_signal += 1;
                Ok(None)
            }
            ffi::Event::FormatChanged(m) => {
                if let (Some(v), Some(c)) = (self.video.as_mut(), mode_capability(&m)) {
                    v.width = m.width;
                    v.height = m.height;
                    if let IntervalRange::Discrete { fps } = &c.fps {
                        v.fps = fps[0];
                    }
                    v.mode = c.mode;
                }
                Ok(None)
            }
            ffi::Event::Video {
                data,
                width,
                height,
                row_bytes,
                pixel_format,
                arrival_ns,
                hardware_ns,
                stream_ns,
            } => {
                if pixel_format != ffi::BMD_FORMAT_8BIT_YUV || data.is_empty() {
                    return Err(CaptureError::invalid_input(format!(
                        "unexpected DeckLink frame (pixel format {})",
                        ffi::fourcc_str(pixel_format)
                    )));
                }
                let frame = decode_raw_video(
                    RawFormat::Uyvy,
                    width,
                    height,
                    row_bytes,
                    &data,
                    arrival_ns,
                    Timebase::NANOS,
                )?;
                let sequence = stream_ns
                    .filter(|(_, d)| *d > 0)
                    .map_or(0, |(t, d)| (t / d).max(0) as u64);
                Ok(Some(CapturedFrame::Video {
                    frame,
                    ts: Timestamps {
                        monotonic_ns: arrival_ns,
                        device_ns: hardware_ns,
                        sequence,
                    },
                }))
            }
            ffi::Event::Audio {
                data,
                frames,
                sample_time,
                arrival_ns,
            } => {
                let Some(a) = self.audio else {
                    return Ok(None);
                };
                // 届いた時刻はパケットの終わりに近いので、パケットの長さを引いて先頭の時刻にする。
                let mono = arrival_ns - frames as i64 * 1_000_000_000 / a.sample_rate as i64;
                Ok(Some(CapturedFrame::Audio {
                    frame: AudioFrame {
                        format: SampleFormat::I16,
                        sample_rate: a.sample_rate,
                        channels: a.channels,
                        data: Bytes::from(data),
                        pts: mono,
                        timebase: Timebase::NANOS,
                    },
                    ts: Timestamps {
                        monotonic_ns: mono,
                        device_ns: sample_time.map(|t| t * 1_000_000_000 / a.sample_rate as i64),
                        sequence: sample_time.unwrap_or(0).max(0) as u64,
                    },
                }))
            }
        }
    }
}

impl CaptureDevice for DeckLinkDevice {
    fn descriptor(&self) -> &DeviceDescriptor {
        &self.desc
    }

    fn start(&mut self, config: &CaptureConfig) -> Result<Negotiated> {
        if self.handle.is_some() {
            return Err(CaptureError::new(ErrorKind::Busy, "already started"));
        }
        let vreq = config.video.as_ref().ok_or_else(|| {
            CaptureError::invalid_config(
                "DeckLink capture needs a video request (audio is embedded in the video input)",
            )
        })?;
        let mut vreq = vreq.clone();
        if !vreq.formats.is_empty() && !vreq.formats.contains(&RawFormat::Uyvy) {
            return Err(CaptureError::unsupported(
                "DeckLink capture provides 8-bit YUV (uyvy) only",
            ));
        }
        vreq.formats = vec![RawFormat::Uyvy];
        let video = choose_video(&self.desc.video, &vreq)?;
        let audio = config
            .audio
            .as_ref()
            .map(|r| choose_audio(&self.desc.audio, r))
            .transpose()?;
        let mode_id = video
            .mode
            .as_deref()
            .and_then(ffi::parse_fourcc)
            .ok_or_else(|| CaptureError::internal("display mode missing"))?;
        let mut h = self
            .api
            .open_input(self.index)
            .map_err(CaptureError::device)?;
        let (tx, rx) = sync_channel(QUEUE);
        h.start(mode_id, audio.map(|a| a.channels as u32), tx)
            .map_err(|e| {
                let kind = if e.contains("in use") {
                    ErrorKind::Busy
                } else {
                    ErrorKind::Device
                };
                CaptureError::new(kind, e)
            })?;
        self.handle = Some(h);
        self.rx = Some(rx);
        self.video = Some(video.clone());
        self.audio = audio;
        Ok(Negotiated {
            video: Some(video),
            audio,
        })
    }

    fn read(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>> {
        let rx = self
            .rx
            .as_ref()
            .ok_or_else(|| CaptureError::internal("not started"))?;
        match rx.recv_timeout(timeout) {
            Ok(ev) => self.convert(ev),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                Err(CaptureError::device("DeckLink input stopped"))
            }
        }
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(mut h) = self.handle.take() {
            h.stop();
        }
        self.rx = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_to_capability() {
        let m = ffi::ModeInfo {
            id: ffi::parse_fourcc("Hp29").unwrap(),
            name: "1080p29.97".into(),
            width: 1920,
            height: 1080,
            frame_duration: 1001,
            time_scale: 30000,
            interlaced: false,
        };
        let c = mode_capability(&m).unwrap();
        assert_eq!(c.format, RawFormat::Uyvy);
        assert_eq!(c.mode.as_deref(), Some("Hp29"));
        assert_eq!(
            c.fps,
            IntervalRange::Discrete {
                fps: vec![Fps::new(30000, 1001)]
            }
        );
        assert!(
            mode_capability(&ffi::ModeInfo {
                frame_duration: 0,
                ..m
            })
            .is_none()
        );
    }

    #[test]
    fn ids() {
        assert_eq!(parse_id("decklink:0"), Some(0));
        assert_eq!(parse_id("decklink:12"), Some(12));
        for bad in ["decklink:", "decklink:-1", "decklink:a", "v4l2:0"] {
            assert_eq!(parse_id(bad), None);
        }
    }

    #[test]
    fn missing_library_is_reported_cleanly() {
        let b = DeckLinkBackend::probe(&LibrarySearch {
            dirs: vec!["/nonexistent-senca".into()],
            dirs_only: true,
        });
        let s = b.status();
        assert!(!s.available);
        assert_eq!(s.verification, Verification::Unverified);
        assert!(s.detail.contains("libDeckLinkAPI.so"), "{}", s.detail);
        assert!(b.enumerate().is_empty());
        assert_eq!(
            b.open("decklink:0").err().unwrap().kind,
            ErrorKind::Unavailable
        );
        // システムの既定の場所でも（この開発機には無い）落ちない
        let b = DeckLinkBackend::probe(&LibrarySearch::default());
        let _ = b.status();
        let _ = b.enumerate();
    }
}
