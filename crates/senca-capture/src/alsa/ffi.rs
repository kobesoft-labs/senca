//! `libasound.so.2`（ALSA）を dlopen して PCM の取り込みを呼ぶ。
//!
//! このモジュールだけ `unsafe` を使う（`alsa/mod.rs` で `#[allow(unsafe_code)]`）。関数の型は
//! `alsa/pcm.h` の宣言（ALSA の公開 API、LGPL-2.1 のライブラリを動的に読み込むだけ）に合わせる。
//!
//! # 未検証
//!
//! この開発機には `libasound.so.2` も取り込みの PCM 装置も無い。ここは実機で動かしていない。

use senca_codec::dl::{Library, LibrarySearch};
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::sync::Arc;

/// 読み込むライブラリの名前。
pub const LIB_NAMES: &[&str] = &["libasound.so.2"];

const SND_PCM_STREAM_CAPTURE: c_int = 1;
const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;
#[cfg(target_endian = "little")]
const SND_PCM_FORMAT_S16: c_int = 2; // S16_LE
#[cfg(target_endian = "big")]
const SND_PCM_FORMAT_S16: c_int = 3; // S16_BE

type OpenFn = unsafe extern "C" fn(*mut *mut c_void, *const c_char, c_int, c_int) -> c_int;
type PcmFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type HwAllocFn = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
type HwFreeFn = unsafe extern "C" fn(*mut c_void);
type HwAnyFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;
type HwSetIntFn = unsafe extern "C" fn(*mut c_void, *mut c_void, c_int) -> c_int;
type HwSetUintFn = unsafe extern "C" fn(*mut c_void, *mut c_void, c_uint) -> c_int;
type HwRateNearFn =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_uint, *mut c_int) -> c_int;
type HwPeriodNearFn =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_ulong, *mut c_int) -> c_int;
type HwBufferNearFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_ulong) -> c_int;
type WaitFn = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type ReadiFn = unsafe extern "C" fn(*mut c_void, *mut c_void, c_ulong) -> c_long;
type RecoverFn = unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int;
type DelayFn = unsafe extern "C" fn(*mut c_void, *mut c_long) -> c_int;
type StrerrorFn = unsafe extern "C" fn(c_int) -> *const c_char;

/// 読み込んだ ALSA の関数。
pub struct AlsaLib {
    _lib: Arc<Library>,
    pub name: String,
    open: OpenFn,
    close: PcmFn,
    prepare: PcmFn,
    start: PcmFn,
    hw_malloc: HwAllocFn,
    hw_free: HwFreeFn,
    hw_any: HwAnyFn,
    hw_set_access: HwSetIntFn,
    hw_set_format: HwSetIntFn,
    hw_set_channels: HwSetUintFn,
    hw_set_rate_near: HwRateNearFn,
    hw_set_period_near: HwPeriodNearFn,
    hw_set_buffer_near: HwBufferNearFn,
    hw_params: HwAnyFn,
    wait: WaitFn,
    readi: ReadiFn,
    recover: RecoverFn,
    delay: DelayFn,
    strerror: StrerrorFn,
}

impl std::fmt::Debug for AlsaLib {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlsaLib").field("name", &self.name).finish()
    }
}

impl AlsaLib {
    /// ライブラリを読み込む。無ければ理由を返す。
    pub fn load(search: &LibrarySearch) -> Result<Arc<AlsaLib>, String> {
        let lib = search.open(LIB_NAMES)?;
        // SAFETY: 各記号の型は alsa/pcm.h・alsa/error.h の宣言に合わせた関数ポインター型
        // （snd_pcm_t*・snd_pcm_hw_params_t* は不透明なポインター、snd_pcm_uframes_t は unsigned long、
        // snd_pcm_sframes_t は long、列挙型は int）。`_lib` を保持するので関数ポインターはその間有効。
        unsafe {
            Ok(Arc::new(AlsaLib {
                name: lib.name.clone(),
                open: lib.sym("snd_pcm_open")?,
                close: lib.sym("snd_pcm_close")?,
                prepare: lib.sym("snd_pcm_prepare")?,
                start: lib.sym("snd_pcm_start")?,
                hw_malloc: lib.sym("snd_pcm_hw_params_malloc")?,
                hw_free: lib.sym("snd_pcm_hw_params_free")?,
                hw_any: lib.sym("snd_pcm_hw_params_any")?,
                hw_set_access: lib.sym("snd_pcm_hw_params_set_access")?,
                hw_set_format: lib.sym("snd_pcm_hw_params_set_format")?,
                hw_set_channels: lib.sym("snd_pcm_hw_params_set_channels")?,
                hw_set_rate_near: lib.sym("snd_pcm_hw_params_set_rate_near")?,
                hw_set_period_near: lib.sym("snd_pcm_hw_params_set_period_size_near")?,
                hw_set_buffer_near: lib.sym("snd_pcm_hw_params_set_buffer_size_near")?,
                hw_params: lib.sym("snd_pcm_hw_params")?,
                wait: lib.sym("snd_pcm_wait")?,
                readi: lib.sym("snd_pcm_readi")?,
                recover: lib.sym("snd_pcm_recover")?,
                delay: lib.sym("snd_pcm_delay")?,
                strerror: lib.sym("snd_strerror")?,
                _lib: lib,
            }))
        }
    }

    pub fn error_text(&self, code: c_int) -> String {
        // SAFETY: snd_strerror は静的な NUL 終端の文字列（または NULL）を返す。
        let p = unsafe { (self.strerror)(code) };
        if p.is_null() {
            return format!("alsa error {code}");
        }
        // SAFETY: NULL でない静的な NUL 終端の文字列。
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// 設定した PCM の形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmParams {
    pub rate: u32,
    pub channels: u8,
    pub period_frames: usize,
}

/// 開いた取り込みの PCM（i16 インターリーブ）。
pub struct Pcm {
    lib: Arc<AlsaLib>,
    pcm: *mut c_void,
    pub params: PcmParams,
}

// SAFETY: `pcm` はこの値だけが持つ ALSA のハンドル。ALSA の PCM は 1 つのスレッドから順に使えば
// どのスレッドから使ってもよい（スレッドに縛られた状態を持たない）。`&mut self` で同時の使用を防ぐ。
unsafe impl Send for Pcm {}

impl std::fmt::Debug for Pcm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pcm").field("params", &self.params).finish()
    }
}

/// 読み取りの結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadResult {
    /// 読めたフレーム数と、その最初のフレームが何フレーム前に取り込まれたか（遅延の推定）。
    Frames { frames: usize, delay_frames: i64 },
    /// 時間切れ。
    Timeout,
    /// あふれ（overrun）から立ち直った。データは欠落した。
    Overrun,
}

impl Pcm {
    /// `name`（`plughw:1,0` など）を開いて設定し、取り込みを始める。
    pub fn open(lib: &Arc<AlsaLib>, name: &str, rate: u32, channels: u8) -> Result<Pcm, String> {
        let cname = CString::new(name).map_err(|_| "bad PCM name".to_string())?;
        let mut pcm: *mut c_void = std::ptr::null_mut();
        // SAFETY: `pcm` は書き込み先、`cname` は NUL 終端。mode 0（ブロックする）。
        let r = unsafe { (lib.open)(&mut pcm, cname.as_ptr(), SND_PCM_STREAM_CAPTURE, 0) };
        if r < 0 || pcm.is_null() {
            return Err(format!("snd_pcm_open({name}): {}", lib.error_text(r)));
        }
        let mut me = Pcm {
            lib: lib.clone(),
            pcm,
            params: PcmParams {
                rate,
                channels,
                period_frames: 0,
            },
        };
        me.configure(rate, channels)?;
        Ok(me)
    }

    fn check(&self, what: &str, r: c_int) -> Result<(), String> {
        if r < 0 {
            Err(format!("{what}: {}", self.lib.error_text(r)))
        } else {
            Ok(())
        }
    }

    fn configure(&mut self, rate: u32, channels: u8) -> Result<(), String> {
        let l = self.lib.clone();
        let mut hw: *mut c_void = std::ptr::null_mut();
        // SAFETY: `hw` は書き込み先。成功すれば下で必ず解放する。
        self.check("hw_params_malloc", unsafe { (l.hw_malloc)(&mut hw) })?;
        let result = (|| {
            let mut rate_v: c_uint = rate;
            let mut dir: c_int = 0;
            let mut period: c_ulong = (rate / 100).max(64) as c_ulong;
            let mut buffer: c_ulong = period * 8;
            // SAFETY: `self.pcm` は開いた PCM、`hw` は確保した hw_params。ポインターの引数は有効な書き込み先。
            unsafe {
                self.check("hw_params_any", (l.hw_any)(self.pcm, hw))?;
                self.check(
                    "set_access",
                    (l.hw_set_access)(self.pcm, hw, SND_PCM_ACCESS_RW_INTERLEAVED),
                )?;
                self.check(
                    "set_format",
                    (l.hw_set_format)(self.pcm, hw, SND_PCM_FORMAT_S16),
                )?;
                self.check(
                    "set_channels",
                    (l.hw_set_channels)(self.pcm, hw, channels as c_uint),
                )?;
                self.check(
                    "set_rate_near",
                    (l.hw_set_rate_near)(self.pcm, hw, &mut rate_v, &mut dir),
                )?;
                self.check(
                    "set_period_size_near",
                    (l.hw_set_period_near)(self.pcm, hw, &mut period, &mut dir),
                )?;
                self.check(
                    "set_buffer_size_near",
                    (l.hw_set_buffer_near)(self.pcm, hw, &mut buffer),
                )?;
                self.check("hw_params", (l.hw_params)(self.pcm, hw))?;
                self.check("prepare", (l.prepare)(self.pcm))?;
                self.check("start", (l.start)(self.pcm))?;
            }
            if period == 0 || period > 192_000 {
                return Err(format!("unusable period size {period}"));
            }
            self.params = PcmParams {
                rate: rate_v,
                channels,
                period_frames: period as usize,
            };
            Ok(())
        })();
        // SAFETY: 上で確保した hw_params を一度だけ解放する。
        unsafe { (l.hw_free)(hw) };
        result
    }

    /// 最大 1 周期分を `buf`（i16、`period_frames * channels` 個以上）に読む。
    pub fn read(&mut self, buf: &mut [i16], timeout_ms: i32) -> Result<ReadResult, String> {
        let ch = self.params.channels as usize;
        let frames = (buf.len() / ch).min(self.params.period_frames);
        if frames == 0 {
            return Err("buffer too small".into());
        }
        // SAFETY: 開いた PCM。
        let w = unsafe { (self.lib.wait)(self.pcm, timeout_ms) };
        if w == 0 {
            return Ok(ReadResult::Timeout);
        }
        if w < 0 {
            return self.recover(w);
        }
        // SAFETY: `buf` は `frames * ch` 個の i16 を書ける（上で frames を buf.len() / ch 以下にした）。
        let n = unsafe {
            (self.lib.readi)(self.pcm, buf.as_mut_ptr() as *mut c_void, frames as c_ulong)
        };
        if n == -(libc::EAGAIN as c_long) {
            return Ok(ReadResult::Timeout);
        }
        if n < 0 {
            return self.recover(n as c_int);
        }
        let mut delay: c_long = 0;
        // SAFETY: `delay` は有効な書き込み先。
        let r = unsafe { (self.lib.delay)(self.pcm, &mut delay) };
        // c_long は 32 bit の環境では i32（そこでは変換が要る）。
        #[allow(clippy::useless_conversion)]
        let delay_frames = if r < 0 { 0 } else { i64::from(delay) } + i64::from(n);
        Ok(ReadResult::Frames {
            frames: n as usize,
            delay_frames,
        })
    }

    fn recover(&mut self, err: c_int) -> Result<ReadResult, String> {
        // SAFETY: 開いた PCM。silent = 1。
        let r = unsafe { (self.lib.recover)(self.pcm, err, 1) };
        if r < 0 {
            return Err(format!("capture failed: {}", self.lib.error_text(err)));
        }
        // SAFETY: 開いた PCM（recover の後は準備状態なので取り込みを再開する）。
        unsafe { (self.lib.start)(self.pcm) };
        Ok(ReadResult::Overrun)
    }
}

impl Drop for Pcm {
    fn drop(&mut self) {
        // SAFETY: 開いた PCM を一度だけ閉じる。
        unsafe { (self.lib.close)(self.pcm) };
    }
}
