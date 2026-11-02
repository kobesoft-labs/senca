//! V4L2 の ioctl と mmap（`linux/videodev2.h` の構造体を `#[repr(C)]` で写したもの）。
//!
//! このモジュールだけ `unsafe` を使う（`v4l2/mod.rs` で `#[allow(unsafe_code)]`）。
//! 構造体の大きさは 64 bit の Linux（x86_64・aarch64）で `videodev2.h` と一致することを
//! コンパイル時に確かめる（ioctl の番号に大きさが入るので、ずれると ioctl が `ENOTTY` で失敗するだけで、
//! 余分に書き込まれることはない）。32 bit の環境は未検証。
//!
//! # 未検証
//!
//! この開発機には V4L2 の装置が無く、v4l2loopback も読み込めない（root 権限が要る）。
//! ここは実機で動かしていない。模擬（`V4l2Io` の試験用の実装）で上の層を試験している。

use super::{CapInfo, Dequeued, FmtDesc, PixFormat, V4l2Io};
use crate::types::{Fps, IntervalRange, SizeRange};
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const V: u32 = b'V' as u32;

const fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | (V << 8) | nr
}

pub const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
pub const V4L2_MEMORY_MMAP: u32 = 1;
pub const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
pub const V4L2_CAP_VIDEO_CAPTURE_MPLANE: u32 = 0x0000_1000;
pub const V4L2_CAP_STREAMING: u32 = 0x0400_0000;
pub const V4L2_CAP_DEVICE_CAPS: u32 = 0x8000_0000;
pub const V4L2_CAP_TIMEPERFRAME: u32 = 0x1000;
pub const V4L2_FRMSIZE_TYPE_DISCRETE: u32 = 1;
pub const V4L2_FRMIVAL_TYPE_DISCRETE: u32 = 1;
pub const V4L2_FIELD_ANY: u32 = 0;
pub const V4L2_BUF_FLAG_ERROR: u32 = 0x0040;
pub const V4L2_BUF_FLAG_TIMESTAMP_MASK: u32 = 0xe000;
pub const V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC: u32 = 0x2000;

/// 4 文字の識別子（リトルエンディアンで詰めた `v4l2_fourcc`）。
pub const fn fourcc(s: &[u8; 4]) -> u32 {
    s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Fmtdesc {
    index: u32,
    type_: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Frmsizeenum {
    index: u32,
    pixel_format: u32,
    type_: u32,
    /// discrete: [width, height]、stepwise: [min_w, max_w, step_w, min_h, max_h, step_h]
    u: [u32; 6],
    reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Frmivalenum {
    index: u32,
    pixel_format: u32,
    width: u32,
    height: u32,
    type_: u32,
    /// discrete: [num, den]、stepwise: [min.num, min.den, max.num, max.den, step.num, step.den]
    u: [u32; 6],
    reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// `v4l2_format.fmt` の共用体（200 バイト。`v4l2_window` がポインターを含むので 64 bit では 8 バイト境界）。
#[cfg_attr(target_pointer_width = "64", repr(C, align(8)))]
#[cfg_attr(not(target_pointer_width = "64"), repr(C))]
#[derive(Clone, Copy)]
struct V4l2FormatUnion {
    pix: V4l2PixFormat,
    raw: [u8; 200 - 48],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Format {
    type_: u32,
    fmt: V4l2FormatUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Timecode {
    type_: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
union V4l2BufferM {
    offset: u32,
    userptr: libc::c_ulong,
    planes: *mut c_void,
    fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: libc::timeval,
    timecode: V4l2Timecode,
    sequence: u32,
    memory: u32,
    m: V4l2BufferM,
    length: u32,
    reserved2: u32,
    request_fd: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2Fract {
    numerator: u32,
    denominator: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2Captureparm {
    capability: u32,
    capturemode: u32,
    timeperframe: V4l2Fract,
    extendedmode: u32,
    readbuffers: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Streamparm {
    type_: u32,
    capture: V4l2Captureparm,
    raw: [u8; 200 - 40],
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
const _: () = {
    assert!(size_of::<V4l2Capability>() == 104);
    assert!(size_of::<V4l2Fmtdesc>() == 64);
    assert!(size_of::<V4l2Frmsizeenum>() == 44);
    assert!(size_of::<V4l2Frmivalenum>() == 52);
    assert!(size_of::<V4l2Format>() == 208);
    assert!(size_of::<V4l2RequestBuffers>() == 20);
    assert!(size_of::<V4l2Buffer>() == 88);
    assert!(size_of::<V4l2Streamparm>() == 204);
};

pub const VIDIOC_QUERYCAP: u32 = ioc(IOC_READ, 0, size_of::<V4l2Capability>());
pub const VIDIOC_ENUM_FMT: u32 = ioc(IOC_READ | IOC_WRITE, 2, size_of::<V4l2Fmtdesc>());
pub const VIDIOC_G_FMT: u32 = ioc(IOC_READ | IOC_WRITE, 4, size_of::<V4l2Format>());
pub const VIDIOC_S_FMT: u32 = ioc(IOC_READ | IOC_WRITE, 5, size_of::<V4l2Format>());
pub const VIDIOC_REQBUFS: u32 = ioc(IOC_READ | IOC_WRITE, 8, size_of::<V4l2RequestBuffers>());
pub const VIDIOC_QUERYBUF: u32 = ioc(IOC_READ | IOC_WRITE, 9, size_of::<V4l2Buffer>());
pub const VIDIOC_QBUF: u32 = ioc(IOC_READ | IOC_WRITE, 15, size_of::<V4l2Buffer>());
pub const VIDIOC_DQBUF: u32 = ioc(IOC_READ | IOC_WRITE, 17, size_of::<V4l2Buffer>());
pub const VIDIOC_STREAMON: u32 = ioc(IOC_WRITE, 18, size_of::<libc::c_int>());
pub const VIDIOC_STREAMOFF: u32 = ioc(IOC_WRITE, 19, size_of::<libc::c_int>());
pub const VIDIOC_G_PARM: u32 = ioc(IOC_READ | IOC_WRITE, 21, size_of::<V4l2Streamparm>());
pub const VIDIOC_S_PARM: u32 = ioc(IOC_READ | IOC_WRITE, 22, size_of::<V4l2Streamparm>());
pub const VIDIOC_ENUM_FRAMESIZES: u32 = ioc(IOC_READ | IOC_WRITE, 74, size_of::<V4l2Frmsizeenum>());
pub const VIDIOC_ENUM_FRAMEINTERVALS: u32 =
    ioc(IOC_READ | IOC_WRITE, 75, size_of::<V4l2Frmivalenum>());

/// バッファの数の上限。
const MAX_BUFFERS: u32 = 32;

fn zeroed<T: Copy>() -> T {
    // SAFETY: ここで使う型はすべて整数・整数の配列・整数とポインターの共用体だけからなる `#[repr(C)]` の構造体で、
    // 全ビット 0 が有効な値（カーネルの ABI も 0 で初期化して渡すことを求める）。
    unsafe { std::mem::zeroed() }
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// mmap した 1 つのバッファ。
struct Mapping {
    ptr: *mut c_void,
    len: usize,
}

/// 開いた V4L2 の装置。
pub struct V4l2Fd {
    file: File,
    maps: Vec<Mapping>,
    streaming: bool,
}

// SAFETY: `maps` のポインターはこの値だけが所有する mmap の領域を指す（複製しない）。別のスレッドへ移しても、
// 同時に 2 つのスレッドから触ることは `&mut self` の規則で起きない。
unsafe impl Send for V4l2Fd {}

impl std::fmt::Debug for V4l2Fd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V4l2Fd")
            .field("fd", &self.file.as_raw_fd())
            .field("buffers", &self.maps.len())
            .field("streaming", &self.streaming)
            .finish()
    }
}

impl V4l2Fd {
    /// 装置ファイルを開く（`O_RDWR | O_NONBLOCK | O_CLOEXEC`）。
    pub fn open(path: &Path) -> io::Result<V4l2Fd> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        Ok(V4l2Fd {
            file,
            maps: Vec::new(),
            streaming: false,
        })
    }

    /// ioctl を呼ぶ（`EINTR` なら繰り返す）。
    ///
    /// `T` は `req` の大きさ（番号に埋め込まれた値）と一致する `#[repr(C)]` の構造体でなければならない。
    /// 呼び出し元はすべてこのモジュールの中で、番号と型の組は上の定数で固定している。
    fn xioctl<T>(&self, req: u32, arg: &mut T) -> io::Result<()> {
        debug_assert_eq!(((req >> 16) & 0x3fff) as usize, size_of::<T>());
        loop {
            // SAFETY: fd は開いている。`arg` は `size_of::<T>()` バイトの有効な読み書き先で、その大きさは
            // ioctl の番号に埋め込まれた大きさと一致する（上の debug_assert と定数の定義）。カーネルはその範囲だけを読み書きする。
            let r = unsafe {
                libc::ioctl(
                    self.file.as_raw_fd(),
                    req as _,
                    arg as *mut T as *mut c_void,
                )
            };
            if r != -1 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                return Err(e);
            }
        }
    }

    fn unmap_all(&mut self) {
        for m in self.maps.drain(..) {
            // SAFETY: `m` は mmap が返した領域と長さ。ここで一度だけ解放し、以後は使わない（drain で取り除く）。
            unsafe {
                libc::munmap(m.ptr, m.len);
            }
        }
    }

    fn release_buffers(&mut self) {
        let mut rb = zeroed::<V4l2RequestBuffers>();
        rb.count = 0;
        rb.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        rb.memory = V4L2_MEMORY_MMAP;
        let _ = self.xioctl(VIDIOC_REQBUFS, &mut rb);
    }
}

impl Drop for V4l2Fd {
    fn drop(&mut self) {
        let _ = self.stop_streaming();
    }
}

impl V4l2Io for V4l2Fd {
    fn query_cap(&mut self) -> io::Result<CapInfo> {
        let mut c = zeroed::<V4l2Capability>();
        self.xioctl(VIDIOC_QUERYCAP, &mut c)?;
        let device_caps = if c.capabilities & V4L2_CAP_DEVICE_CAPS != 0 {
            c.device_caps
        } else {
            c.capabilities
        };
        Ok(CapInfo {
            driver: cstr(&c.driver),
            card: cstr(&c.card),
            bus_info: cstr(&c.bus_info),
            version: c.version,
            device_caps,
        })
    }

    fn enum_formats(&mut self) -> io::Result<Vec<FmtDesc>> {
        let mut out = Vec::new();
        for index in 0..64 {
            let mut d = zeroed::<V4l2Fmtdesc>();
            d.index = index;
            d.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            match self.xioctl(VIDIOC_ENUM_FMT, &mut d) {
                Ok(()) => out.push(FmtDesc {
                    fourcc: d.pixelformat,
                    description: cstr(&d.description),
                    flags: d.flags,
                }),
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    fn enum_sizes(&mut self, fourcc: u32) -> io::Result<Vec<SizeRange>> {
        let mut out = Vec::new();
        for index in 0..256 {
            let mut s = zeroed::<V4l2Frmsizeenum>();
            s.index = index;
            s.pixel_format = fourcc;
            match self.xioctl(VIDIOC_ENUM_FRAMESIZES, &mut s) {
                Ok(()) if s.type_ == V4L2_FRMSIZE_TYPE_DISCRETE => out.push(SizeRange::Discrete {
                    width: s.u[0],
                    height: s.u[1],
                }),
                Ok(()) => {
                    // stepwise（type 3）・continuous（type 2、step は 1）は 1 つだけ返る。
                    out.push(SizeRange::Stepwise {
                        min_width: s.u[0],
                        max_width: s.u[1],
                        step_width: s.u[2].max(1),
                        min_height: s.u[3],
                        max_height: s.u[4],
                        step_height: s.u[5].max(1),
                    });
                    break;
                }
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    fn enum_intervals(
        &mut self,
        fourcc: u32,
        width: u32,
        height: u32,
    ) -> io::Result<IntervalRange> {
        let mut fps = Vec::new();
        for index in 0..64 {
            let mut f = zeroed::<V4l2Frmivalenum>();
            f.index = index;
            f.pixel_format = fourcc;
            f.width = width;
            f.height = height;
            match self.xioctl(VIDIOC_ENUM_FRAMEINTERVALS, &mut f) {
                // 間隔（秒）num/den → フレームレート den/num
                Ok(()) if f.type_ == V4L2_FRMIVAL_TYPE_DISCRETE => {
                    fps.push(Fps::new(f.u[1], f.u[0]))
                }
                Ok(()) => {
                    // 最短の間隔 = 最大のフレームレート
                    return Ok(IntervalRange::Range {
                        min: Fps::new(f.u[3], f.u[2]),
                        max: Fps::new(f.u[1], f.u[0]),
                    });
                }
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) && index > 0 => break,
                Err(e) => return Err(e),
            }
        }
        Ok(IntervalRange::Discrete { fps })
    }

    fn set_format(&mut self, fourcc: u32, width: u32, height: u32) -> io::Result<PixFormat> {
        let mut f = zeroed::<V4l2Format>();
        f.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        f.fmt.pix.width = width;
        f.fmt.pix.height = height;
        f.fmt.pix.pixelformat = fourcc;
        f.fmt.pix.field = V4L2_FIELD_ANY;
        self.xioctl(VIDIOC_S_FMT, &mut f)?;
        Ok(PixFormat {
            fourcc: f.fmt.pix.pixelformat,
            width: f.fmt.pix.width,
            height: f.fmt.pix.height,
            bytesperline: f.fmt.pix.bytesperline,
            sizeimage: f.fmt.pix.sizeimage,
        })
    }

    fn set_frame_rate(&mut self, fps: Fps) -> io::Result<Option<Fps>> {
        let mut p = zeroed::<V4l2Streamparm>();
        p.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        self.xioctl(VIDIOC_G_PARM, &mut p)?;
        if p.capture.capability & V4L2_CAP_TIMEPERFRAME == 0 {
            return Ok(None);
        }
        p.capture.timeperframe = V4l2Fract {
            numerator: fps.den,
            denominator: fps.num,
        };
        self.xioctl(VIDIOC_S_PARM, &mut p)?;
        let t = p.capture.timeperframe;
        Ok((t.numerator > 0).then(|| Fps::new(t.denominator, t.numerator)))
    }

    fn start_streaming(&mut self, buffers: u32) -> io::Result<u32> {
        if self.streaming {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        let mut rb = zeroed::<V4l2RequestBuffers>();
        rb.count = buffers.clamp(2, MAX_BUFFERS);
        rb.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        rb.memory = V4L2_MEMORY_MMAP;
        self.xioctl(VIDIOC_REQBUFS, &mut rb)?;
        if rb.count < 2 || rb.count > MAX_BUFFERS {
            self.release_buffers();
            return Err(io::Error::other(format!(
                "driver granted {} buffers",
                rb.count
            )));
        }
        for index in 0..rb.count {
            let mut b = zeroed::<V4l2Buffer>();
            b.index = index;
            b.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            b.memory = V4L2_MEMORY_MMAP;
            let r = self.xioctl(VIDIOC_QUERYBUF, &mut b).and_then(|()| {
                // SAFETY: memory = MMAP のバッファでは共用体 `m` は `offset` が有効（カーネルが書く）。
                let offset = unsafe { b.m.offset };
                let len = b.length as usize;
                if len == 0 {
                    return Err(io::Error::other("zero-length buffer"));
                }
                // SAFETY: 新しい対応付けを作る（既存のメモリーに重ねない: addr = NULL）。fd はこの装置、
                // offset・len はカーネルが QUERYBUF で返した値。失敗は MAP_FAILED で判定する。
                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        len,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED,
                        self.file.as_raw_fd(),
                        offset as libc::off_t,
                    )
                };
                if ptr == libc::MAP_FAILED {
                    return Err(io::Error::last_os_error());
                }
                self.maps.push(Mapping { ptr, len });
                Ok(())
            });
            if let Err(e) = r {
                self.unmap_all();
                self.release_buffers();
                return Err(e);
            }
        }
        for index in 0..rb.count {
            if let Err(e) = self.requeue(index) {
                self.unmap_all();
                self.release_buffers();
                return Err(e);
            }
        }
        let mut ty: libc::c_int = V4L2_BUF_TYPE_VIDEO_CAPTURE as libc::c_int;
        if let Err(e) = self.xioctl(VIDIOC_STREAMON, &mut ty) {
            self.unmap_all();
            self.release_buffers();
            return Err(e);
        }
        self.streaming = true;
        Ok(rb.count)
    }

    fn dequeue(&mut self, timeout: Duration) -> io::Result<Option<Dequeued>> {
        let mut pfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        // SAFETY: `pfd` は 1 要素の有効な配列として渡す。
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(libc::EINTR) {
                Ok(None)
            } else {
                Err(e)
            };
        }
        if r == 0 {
            return Ok(None);
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
            && pfd.revents & libc::POLLIN == 0
        {
            return Err(io::Error::from_raw_os_error(libc::ENODEV));
        }
        let mut b = zeroed::<V4l2Buffer>();
        b.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        b.memory = V4L2_MEMORY_MMAP;
        match self.xioctl(VIDIOC_DQBUF, &mut b) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => return Ok(None),
            Err(e) => return Err(e),
        }
        if b.index as usize >= self.maps.len() {
            return Err(io::Error::other("driver returned an unknown buffer index"));
        }
        #[allow(clippy::useless_conversion)]
        let ts =
            i64::from(b.timestamp.tv_sec) * 1_000_000_000 + i64::from(b.timestamp.tv_usec) * 1000;
        Ok(Some(Dequeued {
            index: b.index,
            bytesused: b.bytesused.min(self.maps[b.index as usize].len as u32),
            flags: b.flags,
            timestamp_ns: ts,
            sequence: b.sequence,
        }))
    }

    fn buffer(&self, index: u32) -> &[u8] {
        match self.maps.get(index as usize) {
            // SAFETY: `ptr`・`len` は mmap した読み書きできる領域。対応付けは `&self` の寿命の間は解除されない
            // （解除は `&mut self` の stop_streaming・drop だけ）。ドライバーが書き込むのはキューに入れている間だけで、
            // 呼び出し側は DQBUF した（キューから出した）バッファだけを読み、読み終えてから requeue する。
            Some(m) => unsafe { std::slice::from_raw_parts(m.ptr as *const u8, m.len) },
            None => &[],
        }
    }

    fn requeue(&mut self, index: u32) -> io::Result<()> {
        let mut b = zeroed::<V4l2Buffer>();
        b.index = index;
        b.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        b.memory = V4L2_MEMORY_MMAP;
        self.xioctl(VIDIOC_QBUF, &mut b)
    }

    fn stop_streaming(&mut self) -> io::Result<()> {
        if self.streaming {
            let mut ty: libc::c_int = V4L2_BUF_TYPE_VIDEO_CAPTURE as libc::c_int;
            let _ = self.xioctl(VIDIOC_STREAMOFF, &mut ty);
            self.streaming = false;
        }
        if !self.maps.is_empty() {
            self.unmap_all();
            self.release_buffers();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `videodev2.h` から求めた ioctl の番号（x86_64 の Linux で `strace` に出る値）と一致する。
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    #[test]
    fn ioctl_numbers_match_videodev2() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_G_FMT, 0xc0d0_5604);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xc058_5609);
        assert_eq!(VIDIOC_QBUF, 0xc058_560f);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
        assert_eq!(VIDIOC_G_PARM, 0xc0cc_5615);
        assert_eq!(VIDIOC_S_PARM, 0xc0cc_5616);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
        assert_eq!(VIDIOC_ENUM_FRAMEINTERVALS, 0xc034_564b);
        assert_eq!(fourcc(b"YUYV"), 0x5659_5559);
        assert_eq!(fourcc(b"MJPG"), 0x4750_4a4d);
    }

    /// V4L2 でない装置（/dev/null）は QUERYCAP が失敗するだけで落ちない。
    #[test]
    fn non_v4l2_device_fails_cleanly() {
        let mut f = V4l2Fd::open(Path::new("/dev/null")).unwrap();
        assert!(f.query_cap().is_err());
        assert!(f.enum_formats().is_err());
        assert!(f.start_streaming(4).is_err());
        // /dev/null は読めるので poll は戻るが、DQBUF は失敗する
        assert!(f.dequeue(Duration::from_millis(1)).is_err());
        assert!(f.buffer(0).is_empty());
        f.stop_streaming().unwrap();
        assert!(V4l2Fd::open(Path::new("/nonexistent/video0")).is_err());
    }
}
