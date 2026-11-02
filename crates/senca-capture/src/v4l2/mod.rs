//! V4L2（Linux の映像の取り込み。UVC の Web カメラ、HDMI の USB キャプチャ）。
//!
//! - 検出: `/dev/video*` を開き、`VIDIOC_QUERYCAP` で単一平面の取り込み（`V4L2_CAP_VIDEO_CAPTURE`）と
//!   ストリーミング（`V4L2_CAP_STREAMING`）に対応するものだけを出す（UVC のメタデータ用の装置は除く）。
//!   能力は `ENUM_FMT`・`ENUM_FRAMESIZES`・`ENUM_FRAMEINTERVALS` で調べる。
//! - 開始: [`crate::negotiate::choose_video`] で選び、`S_FMT`・`S_PARM` で設定し、`REQBUFS`（MMAP）→
//!   `QUERYBUF`・mmap → `QBUF` → `STREAMON`。ドライバーが形式を変えたら、返ってきた値を使う
//!   （形式そのものが違えば `Unsupported`）。
//! - 読み取り: `poll` → `DQBUF` → I420 に変換（または H.264 を複製）→ `QBUF`。壊れたフレーム
//!   （`V4L2_BUF_FLAG_ERROR`、復号できない MJPEG）は数えて捨てる。
//! - 時刻: `V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC` ならバッファの時刻を `monotonic_ns` に使う（UVC は撮影の開始時刻）。
//!   それ以外は受け取った時刻。`device_ns` はバッファの時刻、`sequence` はドライバーの番号（欠落の検出）。
//! - 音声は扱わない（同じ機器の音声は ALSA の機器として別に出る。`bus_info` で対応付けられる）。
//!
//! 装置を触る部分は [`V4l2Io`] の後ろに置き、実装は [`ffi::V4l2Fd`]（ioctl）と試験用の模擬の 2 つ。
//!
//! # 未検証
//!
//! 実機・v4l2loopback での試験はしていない（この開発機に `/dev/video*` が無く、root 権限も無い）。

#[allow(unsafe_code)]
pub mod ffi;

use crate::clock;
use crate::convert::decode_raw_video;
use crate::device::{CaptureBackend, CaptureDevice};
use crate::error::{CaptureError, ErrorKind, Result};
use crate::negotiate::choose_video;
use crate::types::{
    Backend, BackendStatus, CaptureConfig, CapturedFrame, DeviceDescriptor, Fps, IntervalRange,
    Negotiated, RawFormat, SizeRange, Timestamps, Verification, VideoCapability, VideoMode,
};
use bytes::Bytes;
use ffi::fourcc;
use senca_core::Timebase;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `VIDIOC_QUERYCAP` の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapInfo {
    pub driver: String,
    pub card: String,
    pub bus_info: String,
    /// カーネルの版（`KERNEL_VERSION` の形）。
    pub version: u32,
    /// この装置ファイルの能力（`device_caps`）。
    pub device_caps: u32,
}

/// `VIDIOC_ENUM_FMT` の 1 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FmtDesc {
    pub fourcc: u32,
    pub description: String,
    pub flags: u32,
}

/// `VIDIOC_S_FMT` の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixFormat {
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub bytesperline: u32,
    pub sizeimage: u32,
}

/// `VIDIOC_DQBUF` の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dequeued {
    pub index: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub timestamp_ns: i64,
    pub sequence: u32,
}

/// V4L2 の装置の操作（実装は ioctl と、試験用の模擬）。
pub trait V4l2Io: Send {
    fn query_cap(&mut self) -> io::Result<CapInfo>;
    fn enum_formats(&mut self) -> io::Result<Vec<FmtDesc>>;
    fn enum_sizes(&mut self, fourcc: u32) -> io::Result<Vec<SizeRange>>;
    fn enum_intervals(&mut self, fourcc: u32, width: u32, height: u32)
    -> io::Result<IntervalRange>;
    fn set_format(&mut self, fourcc: u32, width: u32, height: u32) -> io::Result<PixFormat>;
    /// フレームレートを設定する。対応しない（`V4L2_CAP_TIMEPERFRAME` が無い）なら `Ok(None)`。
    fn set_frame_rate(&mut self, fps: Fps) -> io::Result<Option<Fps>>;
    /// バッファを確保・mmap・キューに入れて開始する。確保した数を返す。
    fn start_streaming(&mut self, buffers: u32) -> io::Result<u32>;
    /// 1 つ取り出す。`timeout` の間に無ければ `None`。
    fn dequeue(&mut self, timeout: Duration) -> io::Result<Option<Dequeued>>;
    /// 取り出したバッファの中身（mmap の全長。使うのは `bytesused` まで）。
    fn buffer(&self, index: u32) -> &[u8];
    fn requeue(&mut self, index: u32) -> io::Result<()>;
    fn stop_streaming(&mut self) -> io::Result<()>;
}

/// V4L2 の 4 文字の識別子と形式の対応。
pub fn raw_format(code: u32) -> Option<RawFormat> {
    Some(match code {
        c if c == fourcc(b"YU12") => RawFormat::I420,
        c if c == fourcc(b"NV12") => RawFormat::Nv12,
        c if c == fourcc(b"YUYV") => RawFormat::Yuyv,
        c if c == fourcc(b"UYVY") => RawFormat::Uyvy,
        c if c == fourcc(b"MJPG") || c == fourcc(b"JPEG") => RawFormat::Mjpeg,
        c if c == fourcc(b"H264") => RawFormat::H264,
        _ => return None,
    })
}

/// 形式から V4L2 の識別子（MJPEG は `MJPG`）。
pub fn fourcc_of(f: RawFormat) -> Option<u32> {
    Some(match f {
        RawFormat::I420 => fourcc(b"YU12"),
        RawFormat::Nv12 => fourcc(b"NV12"),
        RawFormat::Yuyv => fourcc(b"YUYV"),
        RawFormat::Uyvy => fourcc(b"UYVY"),
        RawFormat::Mjpeg => fourcc(b"MJPG"),
        RawFormat::H264 => fourcc(b"H264"),
        RawFormat::PcmS16 => return None,
    })
}

/// フレームレートが分からないときの仮の範囲。
const FALLBACK_FPS: IntervalRange = IntervalRange::Range {
    min: Fps::integer(1),
    max: Fps::integer(30),
};

/// 装置の能力を調べる。`fourcc_used` には実際に装置が名乗った識別子（`JPEG` など）を返す。
/// 形式と、装置が名乗った 4 文字の識別子の組。
pub type FormatCodes = Vec<(RawFormat, u32)>;

pub fn probe_capabilities(io: &mut dyn V4l2Io) -> io::Result<(Vec<VideoCapability>, FormatCodes)> {
    let mut caps = Vec::new();
    let mut codes = Vec::new();
    for d in io.enum_formats()? {
        let Some(format) = raw_format(d.fourcc) else {
            continue;
        };
        if codes.iter().any(|(f, _)| *f == format) {
            continue; // MJPG と JPEG の両方を名乗るときは最初のもの
        }
        codes.push((format, d.fourcc));
        let sizes = io.enum_sizes(d.fourcc).unwrap_or_default();
        for size in sizes.into_iter().take(64) {
            let (w, h) = match size {
                SizeRange::Discrete { width, height } => (width, height),
                // stepwise では最大の大きさで調べる（大きいほど遅いので控えめな値になる）
                SizeRange::Stepwise {
                    max_width,
                    max_height,
                    ..
                } => (max_width, max_height),
            };
            let fps = match io.enum_intervals(d.fourcc, w, h) {
                Ok(IntervalRange::Discrete { fps }) if fps.is_empty() => FALLBACK_FPS,
                Ok(r) => r,
                Err(_) => FALLBACK_FPS,
            };
            caps.push(VideoCapability {
                format,
                size,
                fps,
                mode: None,
                interlaced: false,
            });
        }
    }
    Ok((caps, codes))
}

fn kernel_version(v: u32) -> String {
    format!("{}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff)
}

/// 取り込みに使える装置か。
pub fn is_capture_device(c: &CapInfo) -> bool {
    c.device_caps & ffi::V4L2_CAP_VIDEO_CAPTURE != 0 && c.device_caps & ffi::V4L2_CAP_STREAMING != 0
}

/// 装置の説明を作る。
pub fn describe(io: &mut dyn V4l2Io, path: &Path) -> io::Result<(DeviceDescriptor, FormatCodes)> {
    let cap = io.query_cap()?;
    if !is_capture_device(&cap) {
        return Err(io::Error::other(
            if cap.device_caps & ffi::V4L2_CAP_VIDEO_CAPTURE_MPLANE != 0 {
                "multi-planar capture devices are not supported"
            } else {
                "not a streaming video capture device"
            },
        ));
    }
    let (video, codes) = probe_capabilities(io)?;
    let note = video.is_empty().then(|| {
        "no supported pixel format (supported: YU12, NV12, YUYV, UYVY, MJPG, H264)".to_string()
    });
    Ok((
        DeviceDescriptor {
            id: format!("v4l2:{}", path.display()),
            backend: Backend::V4l2,
            name: cap.card.clone(),
            path: Some(path.display().to_string()),
            driver: Some(format!("{} {}", cap.driver, kernel_version(cap.version))),
            bus_info: Some(cap.bus_info.clone()),
            video,
            audio: Vec::new(),
            verification: Verification::Unverified,
            note,
        },
        codes,
    ))
}

/// V4L2 のバックエンド。
#[derive(Debug, Clone)]
pub struct V4l2Backend {
    dev_dir: PathBuf,
}

impl V4l2Backend {
    pub fn new(dev_dir: PathBuf) -> Self {
        V4l2Backend { dev_dir }
    }

    /// `dev_dir` の下の `video<数字>` を番号順に。
    pub fn candidates(&self) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(&self.dev_dir) else {
            return Vec::new();
        };
        let mut v: Vec<(u32, PathBuf)> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let n: u32 = name.strip_prefix("video")?.parse().ok()?;
                Some((n, e.path()))
            })
            .collect();
        v.sort();
        v.into_iter().map(|(_, p)| p).collect()
    }

    /// 識別子のパスが `dev_dir` の下の `video<数字>` か（任意のファイルを開かせない）。
    fn check_path(&self, id: &str) -> Result<PathBuf> {
        let path = id
            .strip_prefix("v4l2:")
            .ok_or_else(|| CaptureError::invalid_config(format!("bad V4L2 device id {id:?}")))?;
        let p = PathBuf::from(path);
        let ok = p.parent() == Some(self.dev_dir.as_path())
            && p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("video"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if !ok {
            return Err(CaptureError::not_found(format!(
                "{path:?} is not a V4L2 device under {}",
                self.dev_dir.display()
            )));
        }
        Ok(p)
    }
}

impl CaptureBackend for V4l2Backend {
    fn backend(&self) -> Backend {
        Backend::V4l2
    }

    fn status(&self) -> BackendStatus {
        let n = self.candidates().len();
        BackendStatus {
            backend: Backend::V4l2,
            available: true,
            detail: format!(
                "{n} video node(s) under {} (unverified on real hardware)",
                self.dev_dir.display()
            ),
            verification: Verification::Unverified,
        }
    }

    fn enumerate(&self) -> Vec<DeviceDescriptor> {
        self.candidates()
            .into_iter()
            .filter_map(|p| {
                let mut fd = ffi::V4l2Fd::open(&p).ok()?;
                describe(&mut fd, &p).ok().map(|(d, _)| d)
            })
            .collect()
    }

    fn open(&self, id: &str) -> Result<Box<dyn CaptureDevice>> {
        let path = self.check_path(id)?;
        let fd = ffi::V4l2Fd::open(&path)
            .map_err(|e| CaptureError::from_io(&path.display().to_string(), &e))?;
        Ok(Box::new(V4l2Device::new(Box::new(fd), &path)?))
    }
}

/// バッファの数。
const BUFFERS: u32 = 4;

/// 開いた V4L2 の装置。
pub struct V4l2Device {
    io: Box<dyn V4l2Io>,
    desc: DeviceDescriptor,
    codes: FormatCodes,
    mode: Option<VideoMode>,
    pix: Option<PixFormat>,
    /// 捨てた壊れたフレームの数。
    pub corrupt_frames: u64,
    received: u64,
}

impl std::fmt::Debug for V4l2Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V4l2Device")
            .field("id", &self.desc.id)
            .field("mode", &self.mode)
            .finish()
    }
}

impl V4l2Device {
    /// 装置の操作から作る（能力を調べる）。
    pub fn new(mut io: Box<dyn V4l2Io>, path: &Path) -> Result<Self> {
        let (desc, codes) = describe(io.as_mut(), path).map_err(|e| {
            CaptureError::new(ErrorKind::Unsupported, format!("{}: {e}", path.display()))
        })?;
        Ok(V4l2Device {
            io,
            desc,
            codes,
            mode: None,
            pix: None,
            corrupt_frames: 0,
            received: 0,
        })
    }

    fn convert(&self, d: &Dequeued) -> Result<CapturedFrame> {
        let mode = self.mode.as_ref().expect("started");
        let pix = self.pix.expect("started");
        let buf = self.io.buffer(d.index);
        let data = &buf[..(d.bytesused as usize).min(buf.len())];
        let monotonic_ns = if d.flags & ffi::V4L2_BUF_FLAG_TIMESTAMP_MASK
            == ffi::V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC
            && d.timestamp_ns > 0
        {
            d.timestamp_ns
        } else {
            clock::monotonic_ns()
        };
        let ts = Timestamps {
            monotonic_ns,
            device_ns: Some(d.timestamp_ns),
            sequence: d.sequence as u64,
        };
        if mode.format == RawFormat::H264 {
            if data.is_empty() {
                return Err(CaptureError::invalid_input("empty H.264 buffer"));
            }
            return Ok(CapturedFrame::H264 {
                annexb: Bytes::copy_from_slice(data),
                ts,
            });
        }
        let frame = decode_raw_video(
            mode.format,
            pix.width,
            pix.height,
            pix.bytesperline as usize,
            data,
            monotonic_ns,
            Timebase::NANOS,
        )?;
        Ok(CapturedFrame::Video { frame, ts })
    }
}

impl CaptureDevice for V4l2Device {
    fn descriptor(&self) -> &DeviceDescriptor {
        &self.desc
    }

    fn start(&mut self, config: &CaptureConfig) -> Result<Negotiated> {
        if self.mode.is_some() {
            return Err(CaptureError::new(ErrorKind::Busy, "already started"));
        }
        if config.audio.is_some() {
            return Err(CaptureError::unsupported(
                "V4L2 devices capture video only; open the matching ALSA device for audio",
            ));
        }
        let req = config
            .video
            .as_ref()
            .ok_or_else(|| CaptureError::invalid_config("no video request"))?;
        let mut mode = choose_video(&self.desc.video, req)?;
        let code = self
            .codes
            .iter()
            .find(|(f, _)| *f == mode.format)
            .map(|(_, c)| *c)
            .or_else(|| fourcc_of(mode.format))
            .ok_or_else(|| CaptureError::internal("no fourcc"))?;
        let what = self.desc.id.clone();
        let pix = self
            .io
            .set_format(code, mode.width, mode.height)
            .map_err(|e| CaptureError::from_io(&format!("{what}: S_FMT"), &e))?;
        if raw_format(pix.fourcc) != Some(mode.format) {
            return Err(CaptureError::unsupported(format!(
                "{what}: driver switched pixel format to {:#x}",
                pix.fourcc
            )));
        }
        mode.width = pix.width;
        mode.height = pix.height;
        senca_codec::frame::check_dimensions(pix.width, pix.height)?;
        match self.io.set_frame_rate(mode.fps) {
            Ok(Some(f)) if f.is_valid() => mode.fps = f,
            Ok(_) => {}
            Err(e) => return Err(CaptureError::from_io(&format!("{what}: S_PARM"), &e)),
        }
        self.io
            .start_streaming(BUFFERS)
            .map_err(|e| CaptureError::from_io(&format!("{what}: start streaming"), &e))?;
        self.pix = Some(pix);
        self.mode = Some(mode.clone());
        Ok(Negotiated {
            video: Some(mode),
            audio: None,
        })
    }

    fn read(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>> {
        if self.mode.is_none() {
            return Err(CaptureError::internal("not started"));
        }
        let what = self.desc.id.clone();
        let Some(d) = self
            .io
            .dequeue(timeout)
            .map_err(|e| CaptureError::from_io(&format!("{what}: DQBUF"), &e))?
        else {
            return Ok(None);
        };
        self.received += 1;
        let result = if d.flags & ffi::V4L2_BUF_FLAG_ERROR != 0 || d.bytesused == 0 {
            Err(CaptureError::invalid_input("buffer flagged as corrupt"))
        } else {
            self.convert(&d)
        };
        // 変換（複製）が終わってからキューに戻す。
        self.io
            .requeue(d.index)
            .map_err(|e| CaptureError::from_io(&format!("{what}: QBUF"), &e))?;
        match result {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.kind == ErrorKind::InvalidInput => {
                self.corrupt_frames += 1;
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn stop(&mut self) -> Result<()> {
        self.mode = None;
        self.pix = None;
        self.io
            .stop_streaming()
            .map_err(|e| CaptureError::from_io("stop streaming", &e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::{i420_to_mjpeg, i420_to_yuyv};
    use crate::synthetic::pattern_i420;
    use crate::types::VideoRequest;
    use std::sync::{Arc, Mutex};

    type MockFormat = (u32, Vec<(u32, u32)>, Vec<u32>);

    /// 模擬の装置。UVC の Web カメラのように YUYV・MJPEG・H.264 を名乗る。
    struct Mock {
        log: Arc<Mutex<Vec<String>>>,
        caps: u32,
        /// (4 文字, 解像度の一覧, fps の一覧)
        formats: Vec<MockFormat>,
        /// S_FMT で返す形式を差し替える（ドライバーが形式を変える場合の試験）。
        force_fourcc: Option<u32>,
        pix: Option<PixFormat>,
        bufs: Vec<Vec<u8>>,
        queued: Vec<bool>,
        frame: u32,
        corrupt_every: u32,
        streaming: bool,
    }

    impl Mock {
        fn webcam(log: Arc<Mutex<Vec<String>>>) -> Self {
            Mock {
                log,
                caps: ffi::V4L2_CAP_VIDEO_CAPTURE | ffi::V4L2_CAP_STREAMING,
                formats: vec![
                    (fourcc(b"YUYV"), vec![(64, 48), (1280, 720)], vec![30, 10]),
                    (fourcc(b"MJPG"), vec![(64, 48), (1280, 720)], vec![30]),
                    (fourcc(b"H264"), vec![(1280, 720)], vec![30]),
                    (fourcc(b"GREY"), vec![(64, 48)], vec![30]),
                ],
                force_fourcc: None,
                pix: None,
                bufs: Vec::new(),
                queued: Vec::new(),
                frame: 0,
                corrupt_every: 0,
                streaming: false,
            }
        }
        fn note(&self, s: impl Into<String>) {
            self.log.lock().unwrap().push(s.into());
        }
    }

    impl V4l2Io for Mock {
        fn query_cap(&mut self) -> io::Result<CapInfo> {
            Ok(CapInfo {
                driver: "uvcvideo".into(),
                card: "Mock Webcam".into(),
                bus_info: "usb-0000:00:14.0-1".into(),
                version: (6 << 16) | (8 << 8) | 12,
                device_caps: self.caps,
            })
        }
        fn enum_formats(&mut self) -> io::Result<Vec<FmtDesc>> {
            Ok(self
                .formats
                .iter()
                .map(|(c, _, _)| FmtDesc {
                    fourcc: *c,
                    description: String::new(),
                    flags: 0,
                })
                .collect())
        }
        fn enum_sizes(&mut self, code: u32) -> io::Result<Vec<SizeRange>> {
            let f = self.formats.iter().find(|f| f.0 == code).unwrap();
            Ok(f.1
                .iter()
                .map(|(w, h)| SizeRange::Discrete {
                    width: *w,
                    height: *h,
                })
                .collect())
        }
        fn enum_intervals(&mut self, code: u32, w: u32, _h: u32) -> io::Result<IntervalRange> {
            let f = self.formats.iter().find(|f| f.0 == code).unwrap();
            // YUYV の 720p は 10 fps だけ
            let fps = if code == fourcc(b"YUYV") && w == 1280 {
                vec![10]
            } else {
                f.2.clone()
            };
            Ok(IntervalRange::Discrete {
                fps: fps.into_iter().map(Fps::integer).collect(),
            })
        }
        fn set_format(&mut self, code: u32, w: u32, h: u32) -> io::Result<PixFormat> {
            self.note(format!("S_FMT {code:#x} {w}x{h}"));
            let code = self.force_fourcc.unwrap_or(code);
            let bpl = if code == fourcc(b"YUYV") {
                w * 2 + 16
            } else {
                0
            };
            let p = PixFormat {
                fourcc: code,
                width: w,
                height: h,
                bytesperline: bpl,
                sizeimage: bpl * h,
            };
            self.pix = Some(p);
            Ok(p)
        }
        fn set_frame_rate(&mut self, fps: Fps) -> io::Result<Option<Fps>> {
            self.note(format!("S_PARM {fps}"));
            Ok(Some(fps))
        }
        fn start_streaming(&mut self, n: u32) -> io::Result<u32> {
            self.note(format!("STREAMON {n}"));
            self.bufs = vec![Vec::new(); n as usize];
            self.queued = vec![true; n as usize];
            self.streaming = true;
            Ok(n)
        }
        fn dequeue(&mut self, _t: Duration) -> io::Result<Option<Dequeued>> {
            assert!(self.streaming);
            let Some(i) = self.queued.iter().position(|q| *q) else {
                return Ok(None); // すべて取り出し済み（呼び出し側が戻していない）
            };
            self.queued[i] = false;
            let p = self.pix.unwrap();
            let n = self.frame;
            self.frame += 1;
            let i420 = pattern_i420(p.width, p.height, n as u64);
            let data = if p.fourcc == fourcc(b"YUYV") {
                let packed = i420_to_yuyv(&i420, p.width, p.height).unwrap();
                let mut padded = Vec::new();
                for row in packed.chunks(p.width as usize * 2) {
                    padded.extend_from_slice(row);
                    padded.extend_from_slice(&[0xAA; 16]);
                }
                padded
            } else if p.fourcc == fourcc(b"MJPG") {
                let mut j = i420_to_mjpeg(&i420, p.width, p.height, 90).unwrap();
                j.extend_from_slice(&[0; 100]); // bytesused より後ろのごみ
                j
            } else {
                let mut v = vec![0, 0, 0, 1, 0x09, 0xf0];
                v.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9a, n as u8]);
                v
            };
            let used = if p.fourcc == fourcc(b"MJPG") {
                data.len() as u32 - 100
            } else {
                data.len() as u32
            };
            self.bufs[i] = data;
            let corrupt = self.corrupt_every > 0 && n % self.corrupt_every == 1;
            Ok(Some(Dequeued {
                index: i as u32,
                bytesused: used,
                flags: ffi::V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC
                    | if corrupt { ffi::V4L2_BUF_FLAG_ERROR } else { 0 },
                timestamp_ns: 1_000_000_000 + n as i64 * 33_333_333,
                sequence: n * 2, // 1 枚おきに欠落
            }))
        }
        fn buffer(&self, index: u32) -> &[u8] {
            &self.bufs[index as usize]
        }
        fn requeue(&mut self, index: u32) -> io::Result<()> {
            assert!(!self.queued[index as usize], "double QBUF");
            self.queued[index as usize] = true;
            Ok(())
        }
        fn stop_streaming(&mut self) -> io::Result<()> {
            self.note("STREAMOFF");
            self.streaming = false;
            Ok(())
        }
    }

    fn req(w: u32, h: u32, fps: u32, formats: &[RawFormat]) -> CaptureConfig {
        CaptureConfig {
            video: Some(VideoRequest {
                width: w,
                height: h,
                fps: Fps::integer(fps),
                formats: formats.to_vec(),
            }),
            audio: None,
        }
    }

    #[test]
    fn describe_lists_supported_formats_only() {
        let log = Arc::default();
        let mut m = Mock::webcam(log);
        let (d, codes) = describe(&mut m, Path::new("/dev/video0")).unwrap();
        assert_eq!(d.id, "v4l2:/dev/video0");
        assert_eq!(d.driver.as_deref(), Some("uvcvideo 6.8.12"));
        assert_eq!(d.verification, Verification::Unverified);
        assert_eq!(codes.len(), 3); // GREY は除く
        assert_eq!(d.video.len(), 5);
        let mut meta = Mock::webcam(Arc::default());
        meta.caps = ffi::V4L2_CAP_STREAMING; // UVC のメタデータ用の装置
        assert!(describe(&mut meta, Path::new("/dev/video1")).is_err());
    }

    #[test]
    fn yuyv_capture_with_stride() {
        let log: Arc<Mutex<Vec<String>>> = Arc::default();
        let mut dev = V4l2Device::new(
            Box::new(Mock::webcam(log.clone())),
            Path::new("/dev/video0"),
        )
        .unwrap();
        let n = dev.start(&req(64, 48, 30, &[])).unwrap();
        let v = n.video.unwrap();
        assert_eq!(
            (v.format, v.width, v.height, v.fps),
            (RawFormat::Yuyv, 64, 48, Fps::integer(30))
        );
        assert!(dev.start(&req(64, 48, 30, &[])).is_err());
        for i in 0..10u64 {
            let f = dev.read(Duration::from_millis(10)).unwrap().unwrap();
            let CapturedFrame::Video { frame, ts } = f else {
                panic!()
            };
            assert_eq!(frame.to_packed().unwrap(), pattern_i420(64, 48, i));
            assert_eq!(ts.monotonic_ns, 1_000_000_000 + i as i64 * 33_333_333);
            assert_eq!(frame.pts, ts.monotonic_ns);
            assert_eq!(ts.sequence, i * 2);
        }
        dev.stop().unwrap();
        let log = log.lock().unwrap();
        assert_eq!(
            *log,
            vec![
                format!("S_FMT {:#x} 64x48", fourcc(b"YUYV")),
                "S_PARM 30".to_string(),
                "STREAMON 4".to_string(),
                "STREAMOFF".to_string()
            ]
        );
    }

    #[test]
    fn mjpeg_for_720p30_and_corrupt_frames_are_skipped() {
        let mut m = Mock::webcam(Arc::default());
        m.corrupt_every = 3;
        let mut dev = V4l2Device::new(Box::new(m), Path::new("/dev/video0")).unwrap();
        let n = dev.start(&req(1280, 720, 30, &[])).unwrap();
        assert_eq!(n.video.unwrap().format, RawFormat::Mjpeg);
        let mut got = 0;
        for _ in 0..6 {
            if let Some(CapturedFrame::Video { frame, .. }) =
                dev.read(Duration::from_millis(10)).unwrap()
            {
                assert_eq!((frame.width, frame.height), (1280, 720));
                got += 1;
            }
        }
        assert_eq!(got, 4);
        assert_eq!(dev.corrupt_frames, 2);
    }

    #[test]
    fn h264_passthrough_buffer() {
        let mut dev = V4l2Device::new(
            Box::new(Mock::webcam(Arc::default())),
            Path::new("/dev/video0"),
        )
        .unwrap();
        let n = dev.start(&req(1280, 720, 30, &[RawFormat::H264])).unwrap();
        assert_eq!(n.video.unwrap().format, RawFormat::H264);
        let Some(CapturedFrame::H264 { annexb, .. }) = dev.read(Duration::from_millis(10)).unwrap()
        else {
            panic!()
        };
        assert_eq!(&annexb[4..6], &[0x09, 0xf0]);
    }

    #[test]
    fn driver_switching_format_is_rejected() {
        let mut m = Mock::webcam(Arc::default());
        m.force_fourcc = Some(fourcc(b"GREY"));
        let mut dev = V4l2Device::new(Box::new(m), Path::new("/dev/video0")).unwrap();
        let e = dev.start(&req(64, 48, 30, &[])).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unsupported);
        let mut dev = V4l2Device::new(
            Box::new(Mock::webcam(Arc::default())),
            Path::new("/dev/video0"),
        )
        .unwrap();
        let mut c = req(64, 48, 30, &[]);
        c.audio = Some(crate::types::AudioRequest {
            sample_rate: 48000,
            channels: 2,
        });
        assert_eq!(dev.start(&c).unwrap_err().kind, ErrorKind::Unsupported);
        assert!(dev.read(Duration::from_millis(1)).is_err());
    }

    #[test]
    fn backend_paths_are_confined() {
        let b = V4l2Backend::new(PathBuf::from("/dev"));
        assert_eq!(
            b.check_path("v4l2:/dev/video0").unwrap(),
            PathBuf::from("/dev/video0")
        );
        for bad in [
            "v4l2:/etc/passwd",
            "v4l2:/dev/../etc/video0",
            "v4l2:/dev/videoX",
            "v4l2:/dev/video",
            "x:/dev/video0",
            "v4l2:/dev/sub/video0",
        ] {
            assert!(b.check_path(bad).is_err(), "{bad}");
        }
        // 装置が無い環境（この開発機）では一覧は空で、開くと NotFound
        let b = V4l2Backend::new(PathBuf::from("/nonexistent-senca"));
        assert!(b.enumerate().is_empty());
        assert!(b.status().available);
        assert_eq!(raw_format(fourcc(b"JPEG")), Some(RawFormat::Mjpeg));
        assert_eq!(fourcc_of(RawFormat::PcmS16), None);
    }
}
