//! libopus の FFI（`unsafe` はこのモジュールだけ）。
//!
//! 関数は `libopus.so.0` から dlopen で取り出す。宣言はヘッダー（libopus 1.6.1 の `opus.h`）に合わせて手で書き、
//! 定数は bindgen の [`bindings`] を使う。`opus_encoder_ctl` は C の可変長引数の関数なので、
//! 可変長引数の関数ポインター型で呼ぶ（引数は `opus_int32` か `opus_int32 *` だけを渡す。ヘッダーの
//! `__opus_check_int` / `__opus_check_int_ptr` と同じ型）。

#![allow(clippy::all)]

use crate::dl::{Library, LibrarySearch};
use std::ffi::{CStr, c_char, c_int, c_uchar, c_void};
use std::ptr::NonNull;
use std::sync::Arc;

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    unused_imports,
    unsafe_op_in_unsafe_fn
)]
#[path = "bindings.rs"]
pub mod bindings;

/// libopus の soname。
pub const LIB_NAMES: &[&str] = &["libopus.so.0"];

type EncoderCreate = unsafe extern "C" fn(i32, c_int, c_int, *mut c_int) -> *mut c_void;
type EncodeFloat = unsafe extern "C" fn(*mut c_void, *const f32, c_int, *mut c_uchar, i32) -> i32;
type EncoderCtl = unsafe extern "C" fn(*mut c_void, c_int, ...) -> c_int;
type Destroy = unsafe extern "C" fn(*mut c_void);
type DecoderCreate = unsafe extern "C" fn(i32, c_int, *mut c_int) -> *mut c_void;
type DecodeFloat =
    unsafe extern "C" fn(*mut c_void, *const c_uchar, i32, *mut f32, c_int, c_int) -> c_int;
type DecodeI16 =
    unsafe extern "C" fn(*mut c_void, *const c_uchar, i32, *mut i16, c_int, c_int) -> c_int;
type GetVersion = unsafe extern "C" fn() -> *const c_char;
type PacketNbSamples = unsafe extern "C" fn(*const c_uchar, i32, i32) -> c_int;

/// 読み込んだ libopus の関数表。
pub struct OpusLib {
    lib: Arc<Library>,
    encoder_create: EncoderCreate,
    encode_float: EncodeFloat,
    encoder_ctl: EncoderCtl,
    encoder_destroy: Destroy,
    decoder_create: DecoderCreate,
    decode_float: DecodeFloat,
    decode_i16: DecodeI16,
    decoder_destroy: Destroy,
    get_version_string: GetVersion,
    packet_get_nb_samples: PacketNbSamples,
}

impl OpusLib {
    pub fn load(search: &LibrarySearch) -> Result<Arc<OpusLib>, String> {
        let lib = search.open(LIB_NAMES)?;
        // SAFETY: 型は opus.h の宣言どおり（opus_int32 = i32、int = c_int）。`lib` を構造体に保持し、
        // 関数ポインターより先に解放されないようにする。
        unsafe {
            Ok(Arc::new(OpusLib {
                encoder_create: lib.sym("opus_encoder_create")?,
                encode_float: lib.sym("opus_encode_float")?,
                encoder_ctl: lib.sym("opus_encoder_ctl")?,
                encoder_destroy: lib.sym("opus_encoder_destroy")?,
                decoder_create: lib.sym("opus_decoder_create")?,
                decode_float: lib.sym("opus_decode_float")?,
                decode_i16: lib.sym("opus_decode")?,
                decoder_destroy: lib.sym("opus_decoder_destroy")?,
                get_version_string: lib.sym("opus_get_version_string")?,
                packet_get_nb_samples: lib.sym("opus_packet_get_nb_samples")?,
                lib,
            }))
        }
    }

    pub fn library_name(&self) -> &str {
        &self.lib.name
    }

    pub fn version(&self) -> String {
        // SAFETY: opus_get_version_string は静的な NUL 終端文字列を返す（解放しない）。
        unsafe {
            let p = (self.get_version_string)();
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }

    /// パケットの標本数（`fs` の標本化周波数での 1 チャンネルあたり）。壊れていれば `Err`。
    pub fn packet_samples(&self, packet: &[u8], fs: i32) -> Result<usize, i32> {
        if packet.is_empty() || packet.len() > i32::MAX as usize {
            return Err(bindings::OPUS_INVALID_PACKET);
        }
        // SAFETY: packet は長さどおりの有効なスライス。関数は読み取りだけ。
        let n = unsafe { (self.packet_get_nb_samples)(packet.as_ptr(), packet.len() as i32, fs) };
        if n < 0 { Err(n) } else { Ok(n as usize) }
    }
}

/// opus のエラーコードを文字列にする（opus_strerror を使わず、固定の表で）。
pub fn error_name(code: i32) -> &'static str {
    match code {
        bindings::OPUS_BAD_ARG => "bad argument",
        bindings::OPUS_BUFFER_TOO_SMALL => "buffer too small",
        bindings::OPUS_INTERNAL_ERROR => "internal error",
        bindings::OPUS_INVALID_PACKET => "invalid packet",
        bindings::OPUS_UNIMPLEMENTED => "unimplemented",
        bindings::OPUS_INVALID_STATE => "invalid state",
        bindings::OPUS_ALLOC_FAIL => "allocation failure",
        _ => "unknown error",
    }
}

/// OpusEncoder の所有ハンドル。
pub struct RawEncoder {
    lib: Arc<OpusLib>,
    ptr: NonNull<c_void>,
    channels: usize,
}

// SAFETY: OpusEncoder はスレッドに結び付いた状態を持たない。`&mut self` でだけ使うので同時に触られない。
unsafe impl Send for RawEncoder {}

impl RawEncoder {
    pub fn new(
        lib: Arc<OpusLib>,
        sample_rate: i32,
        channels: u8,
        application: i32,
    ) -> Result<Self, i32> {
        let mut err: c_int = 0;
        // SAFETY: 引数は値渡し、err は有効なポインター。戻り値は NULL か、opus_encoder_destroy で解放するハンドル。
        let p =
            unsafe { (lib.encoder_create)(sample_rate, channels as c_int, application, &mut err) };
        match NonNull::new(p) {
            Some(ptr) if err == 0 => Ok(RawEncoder {
                lib,
                ptr,
                channels: channels as usize,
            }),
            Some(ptr) => {
                // SAFETY: 作られたハンドルは失敗扱いでも解放する。
                unsafe { (lib.encoder_destroy)(ptr.as_ptr()) };
                Err(err)
            }
            None => Err(if err == 0 {
                bindings::OPUS_ALLOC_FAIL
            } else {
                err
            }),
        }
    }

    /// `OPUS_SET_*`（opus_int32 を 1 つ取る要求）。
    pub fn set(&mut self, request: u32, value: i32) -> Result<(), i32> {
        // SAFETY: SET 系の要求は opus_int32 を 1 つだけ可変長引数で受ける（opus_defines.h の __opus_check_int）。
        let r = unsafe { (self.lib.encoder_ctl)(self.ptr.as_ptr(), request as c_int, value) };
        if r == 0 { Ok(()) } else { Err(r) }
    }

    /// `OPUS_GET_*`（opus_int32 * を 1 つ取る要求）。
    pub fn get(&mut self, request: u32) -> Result<i32, i32> {
        let mut v: i32 = 0;
        // SAFETY: GET 系の要求は opus_int32 * を 1 つ受け、そこへ書く。v は呼び出しの間有効。
        let r = unsafe {
            (self.lib.encoder_ctl)(self.ptr.as_ptr(), request as c_int, &mut v as *mut i32)
        };
        if r == 0 { Ok(v) } else { Err(r) }
    }

    /// `frame_size` 標本（1 チャンネルあたり）を符号化する。
    pub fn encode_f32(
        &mut self,
        pcm: &[f32],
        frame_size: usize,
        out: &mut [u8],
    ) -> Result<usize, i32> {
        if pcm.len() != frame_size * self.channels || out.len() > i32::MAX as usize {
            return Err(bindings::OPUS_BAD_ARG);
        }
        // SAFETY: pcm は frame_size*channels 個の標本を持ち、out は out.len() バイト書ける。
        let n = unsafe {
            (self.lib.encode_float)(
                self.ptr.as_ptr(),
                pcm.as_ptr(),
                frame_size as c_int,
                out.as_mut_ptr(),
                out.len() as i32,
            )
        };
        if n < 0 { Err(n) } else { Ok(n as usize) }
    }
}

impl Drop for RawEncoder {
    fn drop(&mut self) {
        // SAFETY: opus_encoder_create で作ったハンドルを一度だけ解放する。
        unsafe { (self.lib.encoder_destroy)(self.ptr.as_ptr()) }
    }
}

/// OpusDecoder の所有ハンドル。
pub struct RawDecoder {
    lib: Arc<OpusLib>,
    ptr: NonNull<c_void>,
    channels: usize,
}

// SAFETY: RawEncoder と同じ。
unsafe impl Send for RawDecoder {}

impl RawDecoder {
    pub fn new(lib: Arc<OpusLib>, sample_rate: i32, channels: u8) -> Result<Self, i32> {
        let mut err: c_int = 0;
        // SAFETY: RawEncoder::new と同じ。
        let p = unsafe { (lib.decoder_create)(sample_rate, channels as c_int, &mut err) };
        match NonNull::new(p) {
            Some(ptr) if err == 0 => Ok(RawDecoder {
                lib,
                ptr,
                channels: channels as usize,
            }),
            Some(ptr) => {
                // SAFETY: 作られたハンドルは失敗扱いでも解放する。
                unsafe { (lib.decoder_destroy)(ptr.as_ptr()) };
                Err(err)
            }
            None => Err(if err == 0 {
                bindings::OPUS_ALLOC_FAIL
            } else {
                err
            }),
        }
    }

    /// 1 パケットを復号する。`out` は最大の標本数（1 チャンネルあたり `out.len() / channels`）。
    /// 戻り値は 1 チャンネルあたりの標本数。
    pub fn decode_f32(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize, i32> {
        let frame_size = out.len() / self.channels;
        if packet.is_empty() || packet.len() > i32::MAX as usize {
            return Err(bindings::OPUS_INVALID_PACKET);
        }
        // SAFETY: packet は有効なスライス。out は frame_size*channels 個の標本を書ける。
        let n = unsafe {
            (self.lib.decode_float)(
                self.ptr.as_ptr(),
                packet.as_ptr(),
                packet.len() as i32,
                out.as_mut_ptr(),
                frame_size as c_int,
                0,
            )
        };
        if n < 0 { Err(n) } else { Ok(n as usize) }
    }

    pub fn decode_i16(&mut self, packet: &[u8], out: &mut [i16]) -> Result<usize, i32> {
        let frame_size = out.len() / self.channels;
        if packet.is_empty() || packet.len() > i32::MAX as usize {
            return Err(bindings::OPUS_INVALID_PACKET);
        }
        // SAFETY: decode_f32 と同じ。
        let n = unsafe {
            (self.lib.decode_i16)(
                self.ptr.as_ptr(),
                packet.as_ptr(),
                packet.len() as i32,
                out.as_mut_ptr(),
                frame_size as c_int,
                0,
            )
        };
        if n < 0 { Err(n) } else { Ok(n as usize) }
    }
}

impl Drop for RawDecoder {
    fn drop(&mut self) {
        // SAFETY: opus_decoder_create で作ったハンドルを一度だけ解放する。
        unsafe { (self.lib.decoder_destroy)(self.ptr.as_ptr()) }
    }
}
