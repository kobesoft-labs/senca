//! C ABI（仕様 11 章の安定境界）。宣言は `include/senca.h`（手書き。値・構造体の並びを変えるときは
//! 両方をそろえ、`SENCA_ABI_VERSION` を上げる）。
//!
//! 境界の約束:
//! - 文字列は NUL 終端の UTF-8。返す文字列・バッファは `senca_string_free`・`senca_buffer_free` で返す。
//! - 構造体の先頭の `struct_size` で版を見分ける（呼び出し側は `sizeof` を入れる。将来は末尾に足すだけ）。
//! - 失敗の理由は `senca_last_error()`（呼んだスレッドごと、次の呼び出しまで有効）。秘密は含めない。
//! - panic は境界の外へ出さない（`SENCA_ERR_INTERNAL` にする）。
//! - 秘密鍵や内部メモリへのポインタは出さない（`senca_sender` は不透明なハンドル）。
//!
//! この crate で `unsafe` を使うのはこのモジュールだけ。各所の SAFETY に前提を書く。
#![allow(unsafe_code)]

use crate::Error;
use crate::frame::{
    AudioCodec, AudioConfig, AudioFormat, AudioFrame, VideoCodec, VideoFormat, VideoFrame,
};
use crate::qr::QrCode;
use crate::sender::{Event, PushOutcome, Sender, SenderConfig};
use crate::uri::IngestUri;
use crate::visca::{self, Command, Exposure, Pan, Reply, Tilt, WhiteBalance};
use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

/// `include/senca.h` の `SENCA_ABI_VERSION`。
pub const ABI_VERSION: u32 = 1;

pub const SENCA_OK: i32 = 0;
pub const SENCA_DROPPED: i32 = 1;
pub const SENCA_ERR_INVALID_ARGUMENT: i32 = -1;
pub const SENCA_ERR_INVALID_CONFIG: i32 = -2;
pub const SENCA_ERR_UNSUPPORTED: i32 = -3;
pub const SENCA_ERR_INVALID_STATE: i32 = -4;
pub const SENCA_ERR_BAD_FRAME: i32 = -5;
pub const SENCA_ERR_BUFFER_TOO_SMALL: i32 = -6;
pub const SENCA_ERR_INTERNAL: i32 = -99;

const CODEC_H264: i32 = 1;
const CODEC_H265: i32 = 2;
const CODEC_AAC: i32 = 3;
const CODEC_OPUS: i32 = 4;
const FORMAT_ANNEXB: i32 = 1;
const FORMAT_LENGTH_PREFIXED: i32 = 2;
const FORMAT_RAW: i32 = 3;
const FORMAT_ADTS: i32 = 4;
const EVENT_STATE_CHANGED: i32 = 1;
const EVENT_KEYFRAME_REQUESTED: i32 = 2;

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn set_error(msg: &str) {
    let c = CString::new(msg.replace('\0', " ")).unwrap_or_default();
    LAST_ERROR.with(|e| *e.borrow_mut() = c);
}

fn fail(code: i32, msg: impl std::fmt::Display) -> i32 {
    set_error(&msg.to_string());
    code
}

fn err_code(e: &Error) -> i32 {
    let code = match e {
        Error::InvalidArgument(_) => SENCA_ERR_INVALID_ARGUMENT,
        Error::InvalidConfig(_) => SENCA_ERR_INVALID_CONFIG,
        Error::Unsupported(_) => SENCA_ERR_UNSUPPORTED,
        Error::InvalidState(_) => SENCA_ERR_INVALID_STATE,
        Error::BadFrame(_) => SENCA_ERR_BAD_FRAME,
        Error::Internal(_) => SENCA_ERR_INTERNAL,
    };
    fail(code, e)
}

/// panic を境界で止める。
fn guard(f: impl FnOnce() -> i32) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => fail(SENCA_ERR_INTERNAL, "internal panic"),
    }
}

/// # Safety
/// `p` は NULL か、NUL 終端の文字列を指すこと。
unsafe fn cstr<'a>(p: *const c_char, what: &'static str) -> Result<&'a str, i32> {
    if p.is_null() {
        return Err(fail(SENCA_ERR_INVALID_ARGUMENT, format!("{what} is NULL")));
    }
    // SAFETY: 呼び出し側の前提（NUL 終端で、呼び出しの間は有効）。
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|_| fail(SENCA_ERR_INVALID_ARGUMENT, format!("{what} is not UTF-8")))
}

/// # Safety
/// `p` は `len` バイト読める領域を指すこと（`len` が 0 なら NULL でもよい）。
unsafe fn bytes<'a>(p: *const u8, len: usize, what: &'static str) -> Result<&'a [u8], i32> {
    if len == 0 {
        return Ok(&[]);
    }
    if p.is_null() {
        return Err(fail(SENCA_ERR_INVALID_ARGUMENT, format!("{what} is NULL")));
    }
    if len > isize::MAX as usize {
        return Err(fail(SENCA_ERR_INVALID_ARGUMENT, format!("{what} too long")));
    }
    // SAFETY: 呼び出し側の前提（`len` バイト読めて、呼び出しの間は変更されない）。
    Ok(unsafe { std::slice::from_raw_parts(p, len) })
}

fn out_string(s: String, out: *mut *mut c_char) -> i32 {
    if out.is_null() {
        return fail(SENCA_ERR_INVALID_ARGUMENT, "out is NULL");
    }
    let c = match CString::new(s) {
        Ok(c) => c,
        Err(_) => return fail(SENCA_ERR_INTERNAL, "string contains NUL"),
    };
    // SAFETY: `out` は NULL でなく、呼び出し側が書き込める `char*` の置き場所を渡す前提。
    unsafe { *out = c.into_raw() };
    SENCA_OK
}

/// 不透明なハンドル。
pub struct SencaSender {
    sender: Sender,
}

#[repr(C)]
pub struct SencaSenderOptions {
    pub struct_size: u32,
    pub queue_max_ms: u32,
    pub queue_max_bytes: u32,
    pub connect_timeout_ms: u32,
    pub reconnect_initial_ms: u32,
    pub reconnect_max_ms: u32,
    pub max_reconnect_attempts: u32,
}

#[repr(C)]
pub struct SencaEvent {
    pub struct_size: u32,
    pub kind: i32,
    pub state: i32,
    pub reason: *const c_char,
}

#[repr(C)]
#[derive(Default)]
pub struct SencaStats {
    pub struct_size: u32,
    pub state: i32,
    pub bytes_sent: u64,
    pub bitrate_bps: u64,
    pub video_frames_sent: u64,
    pub audio_frames_sent: u64,
    pub video_frames_dropped: u64,
    pub audio_frames_dropped: u64,
    pub queue_frames: u64,
    pub queue_bytes: u64,
    pub queue_duration_ms: u64,
    pub rtt_us: i64,
    pub reconnects: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SencaViscaCommand {
    pub kind: i32,
    pub a: i32,
    pub b: i32,
    pub c: i32,
    pub d: i32,
}

pub type SencaEventCallback =
    Option<unsafe extern "C" fn(user_data: *mut c_void, event: *const SencaEvent)>;

/// 呼び出し側の user_data。
struct UserData(*mut c_void);
// SAFETY: senca.h の約束として、user_data は送信スレッドから使えるものを渡す（呼び出し側の責任）。
unsafe impl Send for UserData {}
// SAFETY: 同上。SDK は user_data を読み書きせず、そのまま callback に渡すだけ。
unsafe impl Sync for UserData {}

/// # Safety
/// `s` は `senca_sender_create` が返し、まだ `senca_sender_destroy` していないハンドルか NULL。
unsafe fn handle<'a>(s: *const SencaSender) -> Result<&'a SencaSender, i32> {
    if s.is_null() {
        return Err(fail(SENCA_ERR_INVALID_ARGUMENT, "sender is NULL"));
    }
    // SAFETY: 呼び出し側の前提（有効なハンドル）。`SencaSender` は内部で同期するので共有参照でよい。
    Ok(unsafe { &*s })
}

macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(code) => return code,
        }
    };
}

#[unsafe(no_mangle)]
pub extern "C" fn senca_abi_version() -> u32 {
    ABI_VERSION
}

#[unsafe(no_mangle)]
pub extern "C" fn senca_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

#[unsafe(no_mangle)]
pub extern "C" fn senca_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().as_ptr())
}

/// # Safety
/// `config` は NUL 終端の文字列、`options` は NULL か有効な構造体、`out` は書き込めるポインタ。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_sender_create(
    config: *const c_char,
    options: *const SencaSenderOptions,
    out: *mut *mut SencaSender,
) -> i32 {
    guard(|| {
        if out.is_null() {
            return fail(SENCA_ERR_INVALID_ARGUMENT, "out is NULL");
        }
        // SAFETY: 関数の前提。
        let s = tri!(unsafe { cstr(config, "config") });
        let mut cfg = match SenderConfig::from_uri(s) {
            Ok(c) => c,
            Err(e) => return err_code(&e),
        };
        if !options.is_null() {
            // SAFETY: NULL でない options は有効な構造体を指す前提。先頭の struct_size だけまず読む。
            let size = unsafe { (*options).struct_size } as usize;
            if size < std::mem::size_of::<SencaSenderOptions>() {
                return fail(SENCA_ERR_INVALID_ARGUMENT, "options.struct_size too small");
            }
            // SAFETY: 大きさを確かめた。
            let o = unsafe { &*options };
            if o.queue_max_ms > 0 {
                cfg.queue.max_duration_us = o.queue_max_ms as i64 * 1000;
            }
            if o.queue_max_bytes > 0 {
                cfg.queue.max_bytes = o.queue_max_bytes as usize;
            }
            if o.connect_timeout_ms > 0 {
                cfg.connect_timeout = Duration::from_millis(o.connect_timeout_ms as u64);
            }
            if o.reconnect_initial_ms > 0 {
                cfg.reconnect_initial = Duration::from_millis(o.reconnect_initial_ms as u64);
            }
            if o.reconnect_max_ms > 0 {
                cfg.reconnect_max = Duration::from_millis(o.reconnect_max_ms as u64);
            }
            if o.max_reconnect_attempts > 0 {
                cfg.max_reconnect_attempts = Some(o.max_reconnect_attempts);
            }
        }
        match Sender::new(cfg) {
            Ok(sender) => {
                // SAFETY: `out` は NULL でない（上で確かめた）。
                unsafe { *out = Box::into_raw(Box::new(SencaSender { sender })) };
                SENCA_OK
            }
            Err(e) => err_code(&e),
        }
    })
}

/// # Safety
/// `s` は有効なハンドル。`cb` は送信スレッドから呼ばれる。`user_data` はそのスレッドから使えること。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_sender_set_callback(
    s: *mut SencaSender,
    cb: SencaEventCallback,
    user_data: *mut c_void,
) -> i32 {
    guard(|| {
        // SAFETY: 関数の前提。
        let h = tri!(unsafe { handle(s) });
        let Some(cb) = cb else {
            return fail(SENCA_ERR_INVALID_ARGUMENT, "callback is NULL");
        };
        let ud = UserData(user_data);
        let r = h.sender.set_event_handler(move |ev| {
            let ud = &ud;
            let (kind, state, reason) = match ev {
                Event::StateChanged { state, reason } => {
                    (EVENT_STATE_CHANGED, *state as i32, reason.clone())
                }
                Event::KeyframeRequested => (EVENT_KEYFRAME_REQUESTED, -1, None),
            };
            let reason = reason.and_then(|r| CString::new(r).ok());
            let e = SencaEvent {
                struct_size: std::mem::size_of::<SencaEvent>() as u32,
                kind,
                state,
                reason: reason.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            };
            // SAFETY: 呼び出し側が登録した関数。`e` と reason は呼び出しの間だけ有効（senca.h に明記）。
            unsafe { cb(ud.0, &e) };
        });
        match r {
            Ok(()) => SENCA_OK,
            Err(e) => err_code(&e),
        }
    })
}

fn video_codec(c: i32) -> Result<VideoCodec, i32> {
    match c {
        CODEC_H264 => Ok(VideoCodec::H264),
        CODEC_H265 => Ok(VideoCodec::H265),
        _ => Err(fail(
            SENCA_ERR_INVALID_ARGUMENT,
            "video codec must be SENCA_CODEC_H264 or SENCA_CODEC_H265",
        )),
    }
}

fn audio_codec(c: i32) -> Result<AudioCodec, i32> {
    match c {
        CODEC_AAC => Ok(AudioCodec::Aac),
        CODEC_OPUS => Ok(AudioCodec::Opus),
        _ => Err(fail(
            SENCA_ERR_INVALID_ARGUMENT,
            "audio codec must be SENCA_CODEC_AAC or SENCA_CODEC_OPUS",
        )),
    }
}

/// # Safety
/// `s` は有効なハンドル、`record` は `len` バイト読めること。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_sender_set_video_config(
    s: *mut SencaSender,
    codec: i32,
    record: *const u8,
    len: usize,
) -> i32 {
    guard(|| {
        // SAFETY: 関数の前提。
        let h = tri!(unsafe { handle(s) });
        let codec = tri!(video_codec(codec));
        // SAFETY: 関数の前提。
        let rec = tri!(unsafe { bytes(record, len, "record") });
        match h.sender.set_video_config(codec, rec) {
            Ok(()) => SENCA_OK,
            Err(e) => err_code(&e),
        }
    })
}

/// # Safety
/// `s` は有効なハンドル、`extra` は `extra_len` バイト読めること。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_sender_set_audio_config(
    s: *mut SencaSender,
    codec: i32,
    sample_rate: u32,
    channels: u32,
    extra: *const u8,
    extra_len: usize,
) -> i32 {
    guard(|| {
        // SAFETY: 関数の前提。
        let h = tri!(unsafe { handle(s) });
        let codec = tri!(audio_codec(codec));
        // SAFETY: 関数の前提。
        let extra = tri!(unsafe { bytes(extra, extra_len, "extra") });
        let Ok(channels) = u8::try_from(channels) else {
            return fail(SENCA_ERR_INVALID_ARGUMENT, "channels out of range");
        };
        match h.sender.set_audio_config(&AudioConfig {
            codec,
            sample_rate,
            channels,
            extra: extra.to_vec(),
        }) {
            Ok(()) => SENCA_OK,
            Err(e) => err_code(&e),
        }
    })
}

/// # Safety
/// `s` は有効なハンドル。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_sender_start(s: *mut SencaSender) -> i32 {
    guard(|| {
        // SAFETY: 関数の前提。
        let h = tri!(unsafe { handle(s) });
        match h.sender.start() {
            Ok(()) => SENCA_OK,
            Err(e) => err_code(&e),
        }
    })
}

fn outcome(r: Result<PushOutcome, Error>) -> i32 {
    match r {
        Ok(PushOutcome::Queued) => SENCA_OK,
        Ok(PushOutcome::Dropped) => SENCA_DROPPED,
        Err(e) => err_code(&e),
    }
}

/// # Safety
/// `s` は有効なハンドル、`data` は `len` バイト読めること（呼び出しの間だけ使い、内部でコピーする）。
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn senca_sender_push_video(
    s: *mut SencaSender,
    codec: i32,
    format: i32,
    data: *const u8,
    len: usize,
    pts_us: i64,
    dts_us: i64,
    keyframe: i32,
) -> i32 {
    guard(|| {
        // SAFETY: 関数の前提。
        let h = tri!(unsafe { handle(s) });
        let codec = tri!(video_codec(codec));
        let format = match format {
            FORMAT_ANNEXB => VideoFormat::AnnexB,
            FORMAT_LENGTH_PREFIXED => VideoFormat::LengthPrefixed,
            _ => {
                return fail(
                    SENCA_ERR_INVALID_ARGUMENT,
                    "video format must be ANNEXB or LENGTH_PREFIXED",
                );
            }
        };
        // SAFETY: 関数の前提。
        let data = tri!(unsafe { bytes(data, len, "data") });
        let keyframe = match keyframe {
            k if k < 0 => None,
            0 => Some(false),
            _ => Some(true),
        };
        outcome(h.sender.push_video(&VideoFrame {
            codec,
            format,
            data,
            pts_us,
            dts_us,
            keyframe,
        }))
    })
}

/// # Safety
/// `s` は有効なハンドル、`data` は `len` バイト読めること（内部でコピーする）。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_sender_push_audio(
    s: *mut SencaSender,
    codec: i32,
    format: i32,
    data: *const u8,
    len: usize,
    pts_us: i64,
) -> i32 {
    guard(|| {
        // SAFETY: 関数の前提。
        let h = tri!(unsafe { handle(s) });
        let codec = tri!(audio_codec(codec));
        let format = match format {
            FORMAT_RAW => AudioFormat::Raw,
            FORMAT_ADTS => AudioFormat::Adts,
            _ => {
                return fail(
                    SENCA_ERR_INVALID_ARGUMENT,
                    "audio format must be RAW or ADTS",
                );
            }
        };
        // SAFETY: 関数の前提。
        let data = tri!(unsafe { bytes(data, len, "data") });
        outcome(h.sender.push_audio(&AudioFrame {
            codec,
            format,
            data,
            pts_us,
        }))
    })
}

/// # Safety
/// `s` は有効なハンドル、`out` は `struct_size` を入れた `senca_stats`。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_sender_get_stats(
    s: *const SencaSender,
    out: *mut SencaStats,
) -> i32 {
    guard(|| {
        // SAFETY: 関数の前提。
        let h = tri!(unsafe { handle(s) });
        if out.is_null() {
            return fail(SENCA_ERR_INVALID_ARGUMENT, "out is NULL");
        }
        // SAFETY: NULL でない out は先頭に struct_size を持つ構造体を指す前提。
        let size = unsafe { (*out).struct_size } as usize;
        if size < std::mem::size_of::<SencaStats>() {
            return fail(SENCA_ERR_INVALID_ARGUMENT, "stats.struct_size too small");
        }
        let st = h.sender.stats();
        let v = SencaStats {
            struct_size: std::mem::size_of::<SencaStats>() as u32,
            state: h.sender.state() as i32,
            bytes_sent: st.bytes_sent,
            bitrate_bps: st.bitrate_bps,
            video_frames_sent: st.video_frames_sent,
            audio_frames_sent: st.audio_frames_sent,
            video_frames_dropped: st.video_frames_dropped,
            audio_frames_dropped: st.audio_frames_dropped,
            queue_frames: st.queue_frames,
            queue_bytes: st.queue_bytes,
            queue_duration_ms: st.queue_duration_ms,
            rtt_us: st.rtt_us.map_or(-1, |r| r.min(i64::MAX as u64) as i64),
            reconnects: st.reconnects,
        };
        // SAFETY: 大きさを確かめた。
        unsafe { out.write(v) };
        SENCA_OK
    })
}

/// # Safety
/// `s` は有効なハンドル。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_sender_state(s: *const SencaSender) -> i32 {
    guard(|| {
        // SAFETY: 関数の前提。
        let h = tri!(unsafe { handle(s) });
        h.sender.state() as i32
    })
}

/// # Safety
/// `s` は有効なハンドル。callback の中から呼ばないこと。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_sender_stop(s: *mut SencaSender) -> i32 {
    guard(|| {
        // SAFETY: 関数の前提。
        let h = tri!(unsafe { handle(s) });
        h.sender.stop();
        SENCA_OK
    })
}

/// # Safety
/// `s` は NULL か有効なハンドル。以後 `s` を使わないこと。他のスレッドが同じハンドルを使っていないこと。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_sender_destroy(s: *mut SencaSender) {
    if s.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `senca_sender_create` が Box::into_raw で作ったもの。所有権をここで戻す。
        drop(unsafe { Box::from_raw(s) });
    }));
}

/// # Safety
/// `input` は NUL 終端、`out` は書き込めるポインタ。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_uri_normalize(input: *const c_char, out: *mut *mut c_char) -> i32 {
    guard(|| {
        // SAFETY: 関数の前提。
        let s = tri!(unsafe { cstr(input, "input") });
        match IngestUri::parse(s) {
            Ok(u) => out_string(u.to_senca_uri(), out),
            Err(e) => fail(SENCA_ERR_INVALID_CONFIG, e),
        }
    })
}

/// # Safety
/// `input` は NUL 終端、`out` は書き込めるポインタ。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_uri_to_standard(input: *const c_char, out: *mut *mut c_char) -> i32 {
    guard(|| {
        // SAFETY: 関数の前提。
        let s = tri!(unsafe { cstr(input, "input") });
        match IngestUri::parse(s) {
            Ok(u) => out_string(u.to_standard_url(), out),
            Err(e) => fail(SENCA_ERR_INVALID_CONFIG, e),
        }
    })
}

/// # Safety
/// `s` は NULL か、この SDK が返した文字列。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_string_free(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: CString::into_raw で返したもの。
        drop(unsafe { CString::from_raw(s) });
    }
}

/// # Safety
/// `text` は NUL 終端、`out` は書き込めるポインタ。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_qr_svg(
    text: *const c_char,
    border: u32,
    out: *mut *mut c_char,
) -> i32 {
    guard(|| {
        // SAFETY: 関数の前提。
        let s = tri!(unsafe { cstr(text, "text") });
        match QrCode::encode(s) {
            Ok(q) => out_string(q.to_svg(border.min(16)), out),
            Err(e) => fail(SENCA_ERR_INVALID_ARGUMENT, e),
        }
    })
}

/// # Safety
/// `text` は NUL 終端、`out`・`out_len` は書き込めるポインタ。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_qr_png(
    text: *const c_char,
    scale: u32,
    border: u32,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    guard(|| {
        // SAFETY: 関数の前提。
        let s = tri!(unsafe { cstr(text, "text") });
        if out.is_null() || out_len.is_null() {
            return fail(SENCA_ERR_INVALID_ARGUMENT, "out is NULL");
        }
        match QrCode::encode(s) {
            Ok(q) => {
                let v = q.to_png(scale, border.min(16)).into_boxed_slice();
                let len = v.len();
                // SAFETY: NULL でないことを確かめた。所有権は senca_buffer_free で戻る。
                unsafe {
                    *out_len = len;
                    *out = Box::into_raw(v).cast::<u8>();
                }
                SENCA_OK
            }
            Err(e) => fail(SENCA_ERR_INVALID_ARGUMENT, e),
        }
    })
}

/// # Safety
/// `p`・`len` は NULL・0 か、この SDK が返したバッファとその長さ。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_buffer_free(p: *mut u8, len: usize) {
    if !p.is_null() {
        // SAFETY: Box<[u8]> を into_raw したもの（長さも同じ）。
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, len)) });
    }
}

fn speed_arg(v: i32) -> Result<Option<u8>, i32> {
    match v {
        v if v < 0 => Ok(None),
        0..=7 => Ok(Some(v as u8)),
        _ => Err(fail(SENCA_ERR_INVALID_ARGUMENT, "speed must be -1 or 0..7")),
    }
}

fn u8_arg(v: i32) -> Result<u8, i32> {
    u8::try_from(v).map_err(|_| fail(SENCA_ERR_INVALID_ARGUMENT, "argument out of range"))
}

fn u16_arg(v: i32) -> Result<u16, i32> {
    u16::try_from(v).map_err(|_| fail(SENCA_ERR_INVALID_ARGUMENT, "argument out of range"))
}

fn command_from_c(c: &SencaViscaCommand) -> Result<Command, i32> {
    let bad = || fail(SENCA_ERR_INVALID_ARGUMENT, "invalid VISCA command");
    Ok(match c.kind {
        1 => Command::Power(c.a != 0),
        2 => Command::ZoomStop,
        3 => Command::ZoomTele(speed_arg(c.a)?),
        4 => Command::ZoomWide(speed_arg(c.a)?),
        5 => Command::ZoomDirect(u16_arg(c.a)?),
        6 => Command::FocusStop,
        7 => Command::FocusFar(speed_arg(c.a)?),
        8 => Command::FocusNear(speed_arg(c.a)?),
        9 => Command::FocusDirect(u16_arg(c.a)?),
        10 => Command::FocusAuto,
        11 => Command::FocusManual,
        12 => Command::FocusOnePush,
        13 => Command::WhiteBalance(match c.a {
            0x00 => WhiteBalance::Auto,
            0x01 => WhiteBalance::Indoor,
            0x02 => WhiteBalance::Outdoor,
            0x03 => WhiteBalance::OnePush,
            0x05 => WhiteBalance::Manual,
            _ => return Err(bad()),
        }),
        14 => Command::Exposure(match c.a {
            0x00 => Exposure::FullAuto,
            0x03 => Exposure::Manual,
            0x0A => Exposure::ShutterPriority,
            0x0B => Exposure::IrisPriority,
            0x0D => Exposure::Bright,
            _ => return Err(bad()),
        }),
        15 => Command::PresetReset(u8_arg(c.a)?),
        16 => Command::PresetSet(u8_arg(c.a)?),
        17 => Command::PresetRecall(u8_arg(c.a)?),
        18 => Command::PanTilt {
            pan_speed: u8_arg(c.a)?,
            tilt_speed: u8_arg(c.b)?,
            pan: match c.c {
                1 => Pan::Left,
                2 => Pan::Right,
                3 => Pan::Stop,
                _ => return Err(bad()),
            },
            tilt: match c.d {
                1 => Tilt::Up,
                2 => Tilt::Down,
                3 => Tilt::Stop,
                _ => return Err(bad()),
            },
        },
        19 => Command::PanTiltHome,
        20 => Command::ZoomPositionInquiry,
        _ => return Err(bad()),
    })
}

fn command_to_c(c: &Command) -> SencaViscaCommand {
    let sp = |s: Option<u8>| s.map_or(-1, i32::from);
    let k = |kind: i32, a: i32| SencaViscaCommand {
        kind,
        a,
        ..Default::default()
    };
    match *c {
        Command::Power(on) => k(1, i32::from(on)),
        Command::ZoomStop => k(2, 0),
        Command::ZoomTele(s) => k(3, sp(s)),
        Command::ZoomWide(s) => k(4, sp(s)),
        Command::ZoomDirect(p) => k(5, p.into()),
        Command::FocusStop => k(6, 0),
        Command::FocusFar(s) => k(7, sp(s)),
        Command::FocusNear(s) => k(8, sp(s)),
        Command::FocusDirect(p) => k(9, p.into()),
        Command::FocusAuto => k(10, 0),
        Command::FocusManual => k(11, 0),
        Command::FocusOnePush => k(12, 0),
        Command::WhiteBalance(m) => k(13, m as i32),
        Command::Exposure(m) => k(14, m as i32),
        Command::PresetReset(n) => k(15, n.into()),
        Command::PresetSet(n) => k(16, n.into()),
        Command::PresetRecall(n) => k(17, n.into()),
        Command::PanTilt {
            pan_speed,
            tilt_speed,
            pan,
            tilt,
        } => SencaViscaCommand {
            kind: 18,
            a: pan_speed.into(),
            b: tilt_speed.into(),
            c: pan as i32,
            d: tilt as i32,
        },
        Command::PanTiltHome => k(19, 0),
        Command::ZoomPositionInquiry => k(20, 0),
    }
}

fn write_out(v: &[u8], buf: *mut u8, cap: usize, out_len: *mut usize) -> i32 {
    if out_len.is_null() {
        return fail(SENCA_ERR_INVALID_ARGUMENT, "out_len is NULL");
    }
    // SAFETY: NULL でないことを確かめた。
    unsafe { *out_len = v.len() };
    if buf.is_null() || cap < v.len() {
        return fail(SENCA_ERR_BUFFER_TOO_SMALL, "buffer too small");
    }
    // SAFETY: `buf` は `cap` バイト書ける前提で、`v.len() <= cap`。
    unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), buf, v.len()) };
    SENCA_OK
}

/// # Safety
/// `packet` は `len` バイト読めること。`out_seq`・`out` は書き込めるポインタ。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_visca_parse(
    packet: *const u8,
    len: usize,
    out_seq: *mut u32,
    out: *mut SencaViscaCommand,
) -> i32 {
    guard(|| {
        // SAFETY: 関数の前提。
        let b = tri!(unsafe { bytes(packet, len, "packet") });
        if out_seq.is_null() || out.is_null() {
            return fail(SENCA_ERR_INVALID_ARGUMENT, "out is NULL");
        }
        let p = match visca::Packet::parse(b) {
            Ok(p) => p,
            Err(e) => return fail(SENCA_ERR_BAD_FRAME, e),
        };
        // SAFETY: NULL でないことを確かめた。
        unsafe { *out_seq = p.seq };
        match Command::parse(&p.payload) {
            Ok(c) => {
                // SAFETY: NULL でないことを確かめた。
                unsafe { *out = command_to_c(&c) };
                SENCA_OK
            }
            Err(e) => fail(SENCA_ERR_UNSUPPORTED, e),
        }
    })
}

/// # Safety
/// `cmd` は有効な構造体、`buf` は `cap` バイト書けること、`out_len` は書き込めるポインタ。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn senca_visca_build(
    cmd: *const SencaViscaCommand,
    seq: u32,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    guard(|| {
        if cmd.is_null() {
            return fail(SENCA_ERR_INVALID_ARGUMENT, "cmd is NULL");
        }
        // SAFETY: NULL でない cmd は有効な構造体を指す前提。
        let c = tri!(command_from_c(unsafe { &*cmd }));
        match c.to_packet(seq) {
            Ok(v) => write_out(&v, buf, cap, out_len),
            Err(e) => fail(SENCA_ERR_INVALID_ARGUMENT, e),
        }
    })
}

/// # Safety
/// `data` は `data_len` バイト読めること、`buf` は `cap` バイト書けること、`out_len` は書き込めるポインタ。
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn senca_visca_build_reply(
    kind: i32,
    socket: u32,
    error_code: u32,
    data: *const u8,
    data_len: usize,
    seq: u32,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    guard(|| {
        let socket = (socket & 15) as u8;
        // SAFETY: 関数の前提。
        let d = tri!(unsafe { bytes(data, data_len, "data") });
        let r = match kind {
            1 => Reply::Ack { socket },
            2 => Reply::Completion {
                socket,
                data: d.to_vec(),
            },
            3 => Reply::Error {
                socket,
                code: (error_code & 0x7F) as u8,
            },
            _ => {
                return fail(
                    SENCA_ERR_INVALID_ARGUMENT,
                    "reply kind must be 1 (ACK), 2 (COMPLETION) or 3 (ERROR)",
                );
            }
        };
        write_out(&r.to_packet(seq), buf, cap, out_len)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sender::SenderState;

    /// include/senca.h の値・構造体の大きさが Rust 側とそろっていること。
    #[test]
    fn header_matches_rust_side() {
        let h = include_str!("../include/senca.h");
        let defs: std::collections::HashMap<&str, i64> = h
            .lines()
            .filter_map(|l| {
                let mut it = l.strip_prefix("#define ")?.split_whitespace();
                let name = it.next()?;
                let v = it
                    .next()?
                    .trim_start_matches('(')
                    .trim_end_matches(')')
                    .trim_end_matches('u');
                let v = if let Some(x) = v.strip_prefix("0x") {
                    i64::from_str_radix(x, 16).ok()?
                } else {
                    v.parse().ok()?
                };
                Some((name, v))
            })
            .collect();
        let want: &[(&str, i64)] = &[
            ("SENCA_ABI_VERSION", ABI_VERSION as i64),
            ("SENCA_OK", SENCA_OK as i64),
            ("SENCA_DROPPED", SENCA_DROPPED as i64),
            (
                "SENCA_ERR_INVALID_ARGUMENT",
                SENCA_ERR_INVALID_ARGUMENT as i64,
            ),
            ("SENCA_ERR_INVALID_CONFIG", SENCA_ERR_INVALID_CONFIG as i64),
            ("SENCA_ERR_UNSUPPORTED", SENCA_ERR_UNSUPPORTED as i64),
            ("SENCA_ERR_INVALID_STATE", SENCA_ERR_INVALID_STATE as i64),
            ("SENCA_ERR_BAD_FRAME", SENCA_ERR_BAD_FRAME as i64),
            (
                "SENCA_ERR_BUFFER_TOO_SMALL",
                SENCA_ERR_BUFFER_TOO_SMALL as i64,
            ),
            ("SENCA_ERR_INTERNAL", SENCA_ERR_INTERNAL as i64),
            ("SENCA_CODEC_H264", CODEC_H264 as i64),
            ("SENCA_CODEC_H265", CODEC_H265 as i64),
            ("SENCA_CODEC_AAC", CODEC_AAC as i64),
            ("SENCA_CODEC_OPUS", CODEC_OPUS as i64),
            ("SENCA_FORMAT_ANNEXB", FORMAT_ANNEXB as i64),
            (
                "SENCA_FORMAT_LENGTH_PREFIXED",
                FORMAT_LENGTH_PREFIXED as i64,
            ),
            ("SENCA_FORMAT_RAW", FORMAT_RAW as i64),
            ("SENCA_FORMAT_ADTS", FORMAT_ADTS as i64),
            ("SENCA_EVENT_STATE_CHANGED", EVENT_STATE_CHANGED as i64),
            (
                "SENCA_EVENT_KEYFRAME_REQUESTED",
                EVENT_KEYFRAME_REQUESTED as i64,
            ),
            ("SENCA_STATE_IDLE", SenderState::Idle as i64),
            ("SENCA_STATE_CONNECTING", SenderState::Connecting as i64),
            ("SENCA_STATE_CONNECTED", SenderState::Connected as i64),
            ("SENCA_STATE_RECONNECTING", SenderState::Reconnecting as i64),
            ("SENCA_STATE_STOPPED", SenderState::Stopped as i64),
            ("SENCA_STATE_FAILED", SenderState::Failed as i64),
            ("SENCA_VISCA_PORT", crate::visca::VISCA_PORT as i64),
        ];
        for (k, v) in want {
            assert_eq!(defs.get(k), Some(v), "{k}");
        }
        // VISCA の種類の番号: 構造体の往復で確かめる（1..=20 がすべて定義されている）
        for kind in 1..=20 {
            let name = defs.iter().find(|(n, v)| {
                n.starts_with("SENCA_VISCA_")
                    && **v == kind
                    && !n.contains("REPLY")
                    && !n.contains("PORT")
            });
            assert!(name.is_some(), "VISCA kind {kind} missing in header");
        }
        assert_eq!(std::mem::size_of::<SencaStats>(), 96);
        assert_eq!(std::mem::size_of::<SencaSenderOptions>(), 28);
        assert_eq!(std::mem::size_of::<SencaViscaCommand>(), 20);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<SencaEvent>(), 24);
    }

    #[test]
    fn create_rejects_bad_config_with_message() {
        let mut out = std::ptr::null_mut();
        let cfg = c"senca://ingest?v=1&proto=srt&host=h";
        // SAFETY: 有効な文字列と置き場所。
        let r = unsafe { senca_sender_create(cfg.as_ptr(), std::ptr::null(), &mut out) };
        assert_eq!(r, SENCA_ERR_INVALID_CONFIG);
        // SAFETY: senca_last_error は常に有効な文字列を返す。
        let msg = unsafe { CStr::from_ptr(senca_last_error()) }
            .to_str()
            .unwrap();
        assert!(msg.contains("port"), "{msg}");
        let whip = c"senca://ingest?v=1&proto=whip&url=https://h/whip/x";
        // SAFETY: 同上。
        assert_eq!(
            unsafe { senca_sender_create(whip.as_ptr(), std::ptr::null(), &mut out) },
            SENCA_ERR_UNSUPPORTED
        );
    }

    #[test]
    fn lifecycle_without_connecting() {
        let mut s = std::ptr::null_mut();
        let cfg = c"srt://127.0.0.1:9?streamid=publish:k";
        // SAFETY: 有効な引数。
        unsafe {
            assert_eq!(
                senca_sender_create(cfg.as_ptr(), std::ptr::null(), &mut s),
                SENCA_OK
            );
            let mut st = SencaStats {
                struct_size: std::mem::size_of::<SencaStats>() as u32,
                ..Default::default()
            };
            assert_eq!(senca_sender_get_stats(s, &mut st), SENCA_OK);
            assert_eq!(st.state, 0);
            assert_eq!(st.rtt_us, -1);
            // 設定前の P フレームは捨てる
            let p = [0u8, 0, 0, 1, 0x41, 1];
            assert_eq!(
                senca_sender_push_video(
                    s,
                    CODEC_H264,
                    FORMAT_ANNEXB,
                    p.as_ptr(),
                    p.len(),
                    0,
                    0,
                    -1
                ),
                SENCA_DROPPED
            );
            assert_eq!(
                senca_sender_push_video(s, 9, FORMAT_ANNEXB, p.as_ptr(), p.len(), 0, 0, -1),
                SENCA_ERR_INVALID_ARGUMENT
            );
            assert_eq!(senca_sender_stop(s), SENCA_OK);
            assert_eq!(senca_sender_state(s), 4);
            senca_sender_destroy(s);
        }
    }

    #[test]
    fn uri_and_qr_and_visca_through_c_abi() {
        // SAFETY: 有効な引数。返したものは free する。
        unsafe {
            let mut out = std::ptr::null_mut();
            assert_eq!(
                senca_uri_normalize(c"rtmp://h/live/k".as_ptr(), &mut out),
                SENCA_OK
            );
            assert_eq!(
                CStr::from_ptr(out).to_str().unwrap(),
                "senca://ingest?v=1&proto=rtmp&url=rtmp://h/live&key=k"
            );
            senca_string_free(out);
            let mut png = std::ptr::null_mut();
            let mut len = 0usize;
            assert_eq!(
                senca_qr_png(c"hello".as_ptr(), 4, 4, &mut png, &mut len),
                SENCA_OK
            );
            assert_eq!(std::slice::from_raw_parts(png, 4), b"\x89PNG");
            senca_buffer_free(png, len);
            let cmd = SencaViscaCommand {
                kind: 18,
                a: 3,
                b: 4,
                c: 1,
                d: 2,
            };
            let mut buf = [0u8; 32];
            let mut n = 0usize;
            assert_eq!(
                senca_visca_build(&cmd, 5, buf.as_mut_ptr(), 4, &mut n),
                SENCA_ERR_BUFFER_TOO_SMALL
            );
            assert_eq!(
                senca_visca_build(&cmd, 5, buf.as_mut_ptr(), buf.len(), &mut n),
                SENCA_OK
            );
            let mut seq = 0;
            let mut back = SencaViscaCommand::default();
            assert_eq!(
                senca_visca_parse(buf.as_ptr(), n, &mut seq, &mut back),
                SENCA_OK
            );
            assert_eq!((seq, back), (5, cmd));
            assert_eq!(
                senca_visca_build_reply(
                    1,
                    1,
                    0,
                    std::ptr::null(),
                    0,
                    5,
                    buf.as_mut_ptr(),
                    buf.len(),
                    &mut n
                ),
                SENCA_OK
            );
            assert_eq!(&buf[..n], &[0x01, 0x11, 0, 3, 0, 0, 0, 5, 0x90, 0x41, 0xFF]);
        }
    }
}
