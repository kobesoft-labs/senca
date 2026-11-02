//! ALSA の音声の取り込み（USB マイク、HDMI の USB キャプチャの音声）。
//!
//! - 検出: `/proc/asound/pcm` の `capture` を持つ行と `/proc/asound/cards` の名前から機器を作る
//!   （ライブラリを呼ばずに読める。解析は純粋な関数で試験する）。`libasound.so.2` が読み込めなければ
//!   バックエンドは「使えない」。
//! - 開始: `plughw:<card>,<device>` を開く（ALSA の plug 層で標本化周波数・チャンネル数・形式を変換する。
//!   PulseAudio・PipeWire を経由しないので socket は使わない。capture worker の seccomp で socket を拒否しても動く）。
//!   i16 インターリーブ、周期 10 ms、バッファ 80 ms。
//! - 時刻: 読み終えた時刻から `snd_pcm_delay`（まだ読んでいない量）と読んだ量を引いて、最初の標本の
//!   単調時刻を推定する。あふれ（overrun）は数えて立ち直る。
//! - 能力は代表値（48/44.1/32/16 kHz、1/2 ch）。実際の値は `set_rate_near` で決まり、[`Negotiated`] に返す。
//!
//! # 未検証
//!
//! 実機での試験はしていない（この開発機に取り込みの PCM 装置も `libasound.so.2` も無い）。

#[allow(unsafe_code)]
pub mod ffi;

use crate::clock;
use crate::device::{CaptureBackend, CaptureDevice};
use crate::error::{CaptureError, ErrorKind, Result};
use crate::negotiate::choose_audio;
use crate::types::{
    AudioCapability, AudioMode, Backend, BackendStatus, CaptureConfig, CapturedFrame,
    DeviceDescriptor, Negotiated, RawFormat, Timestamps, Verification,
};
use bytes::Bytes;
use senca_codec::dl::LibrarySearch;
use senca_codec::{AudioFrame, SampleFormat};
use senca_core::Timebase;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// `/proc/asound/pcm` の取り込みのある 1 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmEntry {
    pub card: u32,
    pub device: u32,
    pub name: String,
}

/// `/proc/asound/pcm` を解析する（`CC-DD: id : name : playback N : capture N`）。取り込みのある行だけ。
pub fn parse_proc_pcm(text: &str) -> Vec<PcmEntry> {
    text.lines()
        .filter_map(|line| {
            let (head, rest) = line.split_once(':')?;
            let (c, d) = head.trim().split_once('-')?;
            let card = c.parse().ok()?;
            let device = d.parse().ok()?;
            let parts: Vec<&str> = rest.split(" : ").map(str::trim).collect();
            if !parts.iter().any(|p| p.starts_with("capture")) {
                return None;
            }
            let name = parts
                .get(1)
                .or(parts.first())
                .map(|s| s.to_string())
                .unwrap_or_default();
            Some(PcmEntry { card, device, name })
        })
        .collect()
}

/// `/proc/asound/cards` を解析する（` N [id ]: driver - long name`）。（番号, 名前）。
pub fn parse_proc_cards(text: &str) -> Vec<(u32, String)> {
    text.lines()
        .filter_map(|line| {
            let t = line.trim_start();
            let (num, rest) = t.split_once(' ')?;
            let n: u32 = num.parse().ok()?;
            let rest = rest.trim_start();
            if !rest.starts_with('[') {
                return None;
            }
            let name = rest
                .split_once(" - ")
                .map(|(_, n)| n.trim().to_string())
                .unwrap_or_default();
            Some((n, name))
        })
        .collect()
}

fn capability() -> AudioCapability {
    AudioCapability {
        format: RawFormat::PcmS16,
        sample_rates: vec![48000, 44100, 32000, 16000],
        channels: vec![1, 2],
    }
}

/// 識別子 `alsa:hw:C,D` から (C, D)。
pub fn parse_id(id: &str) -> Option<(u32, u32)> {
    let rest = id.strip_prefix("alsa:hw:")?;
    let (c, d) = rest.split_once(',')?;
    if c.is_empty() || d.is_empty() || !c.bytes().chain(d.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((c.parse().ok()?, d.parse().ok()?))
}

/// ALSA のバックエンド。
#[derive(Debug, Clone)]
pub struct AlsaBackend {
    lib: std::result::Result<Arc<ffi::AlsaLib>, String>,
    proc_asound: PathBuf,
}

impl AlsaBackend {
    pub fn probe(libs: &LibrarySearch, proc_asound: PathBuf) -> Self {
        AlsaBackend {
            lib: ffi::AlsaLib::load(libs),
            proc_asound,
        }
    }

    fn descriptors(&self) -> Vec<DeviceDescriptor> {
        let pcm = std::fs::read_to_string(self.proc_asound.join("pcm")).unwrap_or_default();
        let cards = std::fs::read_to_string(self.proc_asound.join("cards")).unwrap_or_default();
        let cards = parse_proc_cards(&cards);
        parse_proc_pcm(&pcm)
            .into_iter()
            .map(|e| {
                let card_name = cards
                    .iter()
                    .find(|(n, _)| *n == e.card)
                    .map(|(_, s)| s.clone())
                    .unwrap_or_default();
                let bus = std::fs::read_link(format!("/sys/class/sound/card{}/device", e.card))
                    .ok()
                    .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
                DeviceDescriptor {
                    id: format!("alsa:hw:{},{}", e.card, e.device),
                    backend: Backend::Alsa,
                    name: if card_name.is_empty() {
                        e.name.clone()
                    } else {
                        format!("{card_name}: {}", e.name)
                    },
                    path: Some(format!("plughw:{},{}", e.card, e.device)),
                    driver: self.lib.as_ref().ok().map(|l| l.name.clone()),
                    bus_info: bus,
                    video: Vec::new(),
                    audio: vec![capability()],
                    verification: Verification::Unverified,
                    note: None,
                }
            })
            .collect()
    }
}

impl CaptureBackend for AlsaBackend {
    fn backend(&self) -> Backend {
        Backend::Alsa
    }

    fn status(&self) -> BackendStatus {
        BackendStatus {
            backend: Backend::Alsa,
            available: self.lib.is_ok(),
            detail: match &self.lib {
                Ok(l) => format!("{} loaded (unverified on real hardware)", l.name),
                Err(e) => format!("libasound.so.2 not available: {e}"),
            },
            verification: Verification::Unverified,
        }
    }

    fn enumerate(&self) -> Vec<DeviceDescriptor> {
        if self.lib.is_err() {
            return Vec::new();
        }
        self.descriptors()
    }

    fn open(&self, id: &str) -> Result<Box<dyn CaptureDevice>> {
        let lib = self
            .lib
            .as_ref()
            .map_err(|e| CaptureError::unavailable(format!("ALSA not available: {e}")))?;
        let (c, d) = parse_id(id)
            .ok_or_else(|| CaptureError::not_found(format!("bad ALSA device id {id:?}")))?;
        let desc = self
            .descriptors()
            .into_iter()
            .find(|x| x.id == id)
            .ok_or_else(|| CaptureError::not_found(format!("no ALSA capture device hw:{c},{d}")))?;
        Ok(Box::new(AlsaDevice {
            lib: lib.clone(),
            desc,
            pcm: None,
            buf: Vec::new(),
            overruns: 0,
            seq: 0,
        }))
    }
}

/// 開いた ALSA の取り込み機器。
pub struct AlsaDevice {
    lib: Arc<ffi::AlsaLib>,
    desc: DeviceDescriptor,
    pcm: Option<ffi::Pcm>,
    buf: Vec<i16>,
    /// あふれの回数。
    pub overruns: u64,
    seq: u64,
}

impl CaptureDevice for AlsaDevice {
    fn descriptor(&self) -> &DeviceDescriptor {
        &self.desc
    }

    fn start(&mut self, config: &CaptureConfig) -> Result<Negotiated> {
        if self.pcm.is_some() {
            return Err(CaptureError::new(ErrorKind::Busy, "already started"));
        }
        if config.video.is_some() {
            return Err(CaptureError::unsupported("ALSA devices capture audio only"));
        }
        let req = config
            .audio
            .as_ref()
            .ok_or_else(|| CaptureError::invalid_config("no audio request"))?;
        let mode = choose_audio(&self.desc.audio, req)?;
        let name = self.desc.path.clone().unwrap_or_default();
        let pcm =
            ffi::Pcm::open(&self.lib, &name, mode.sample_rate, mode.channels).map_err(|e| {
                let kind = if e.contains("busy") {
                    ErrorKind::Busy
                } else {
                    ErrorKind::Device
                };
                CaptureError::new(kind, e)
            })?;
        let p = pcm.params;
        self.buf = vec![0; p.period_frames * p.channels as usize];
        self.pcm = Some(pcm);
        Ok(Negotiated {
            video: None,
            audio: Some(AudioMode {
                format: RawFormat::PcmS16,
                sample_rate: p.rate,
                channels: p.channels,
            }),
        })
    }

    fn read(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>> {
        let pcm = self
            .pcm
            .as_mut()
            .ok_or_else(|| CaptureError::internal("not started"))?;
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        match pcm.read(&mut self.buf, ms).map_err(CaptureError::device)? {
            ffi::ReadResult::Timeout => Ok(None),
            ffi::ReadResult::Overrun => {
                self.overruns += 1;
                Ok(None)
            }
            ffi::ReadResult::Frames {
                frames,
                delay_frames,
            } => {
                let p = pcm.params;
                let now = clock::monotonic_ns();
                let mono = now - delay_frames * 1_000_000_000 / p.rate as i64;
                let n = frames * p.channels as usize;
                let mut data = Vec::with_capacity(n * 2);
                for s in &self.buf[..n] {
                    data.extend_from_slice(&s.to_ne_bytes());
                }
                self.seq += 1;
                Ok(Some(CapturedFrame::Audio {
                    frame: AudioFrame {
                        format: SampleFormat::I16,
                        sample_rate: p.rate,
                        channels: p.channels,
                        data: Bytes::from(data),
                        pts: mono,
                        timebase: Timebase::NANOS,
                    },
                    ts: Timestamps {
                        monotonic_ns: mono,
                        device_ns: None,
                        sequence: self.seq - 1,
                    },
                }))
            }
        }
    }

    fn stop(&mut self) -> Result<()> {
        self.pcm = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_parsing() {
        let pcm = "00-00: ALC892 Analog : ALC892 Analog : playback 1 : capture 1\n\
                   00-01: ALC892 Digital : ALC892 Digital : playback 1\n\
                   01-00: USB Audio : USB Audio : capture 1\n\
                   garbage line\n";
        let e = parse_proc_pcm(pcm);
        assert_eq!(
            e,
            vec![
                PcmEntry {
                    card: 0,
                    device: 0,
                    name: "ALC892 Analog".into()
                },
                PcmEntry {
                    card: 1,
                    device: 0,
                    name: "USB Audio".into()
                }
            ]
        );
        let cards = " 0 [PCH            ]: HDA-Intel - HDA Intel PCH\n\
                     \x20                     HDA Intel PCH at 0xf7f10000 irq 32\n\
                     \x201 [Capture        ]: USB-Audio - USB3.0 Capture\n";
        assert_eq!(
            parse_proc_cards(cards),
            vec![(0, "HDA Intel PCH".into()), (1, "USB3.0 Capture".into())]
        );
    }

    #[test]
    fn ids() {
        assert_eq!(parse_id("alsa:hw:1,0"), Some((1, 0)));
        for bad in [
            "alsa:hw:1",
            "alsa:hw:a,0",
            "alsa:default",
            "alsa:hw:1,0,1",
            "alsa:hw:,0",
            "v4l2:hw:1,0",
        ] {
            assert_eq!(parse_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn missing_library_is_unavailable() {
        let b = AlsaBackend::probe(
            &LibrarySearch {
                dirs: vec!["/nonexistent-senca".into()],
                dirs_only: true,
            },
            "/proc/asound".into(),
        );
        let s = b.status();
        assert!(!s.available);
        assert!(s.detail.contains("libasound"));
        assert!(b.enumerate().is_empty());
        assert_eq!(
            b.open("alsa:hw:0,0").err().unwrap().kind,
            ErrorKind::Unavailable
        );
    }
}
