//! Blackmagic DeckLink（Desktop Video）の C++ の COM 風 API を、dlopen と手書きの vtable で呼ぶ。
//!
//! SDK のヘッダーは配布条件があるので取り込まない。ここにあるのは、公開されている SDK の説明書
//! （「Blackmagic DeckLink SDK」の各インターフェースのメソッドの並び）から書いた最小限の vtable の配置だけ。
//!
//! # ABI の前提（Linux x86_64・aarch64、Itanium C++ ABI）
//!
//! - オブジェクトの先頭の 1 語が vtable（関数ポインターの配列）を指す。各メソッドは第 1 引数に `this` を取る。
//! - 仮想関数は宣言の順に並ぶ。仮想デストラクターは宣言の位置に 2 つの項目（完全オブジェクト用・削除用）を取る。
//!   `IUnknown`（`LinuxCOM.h`）にはデストラクターが無いので、各インターフェースのメソッドは `Release` の直後に続く。
//!   派生インターフェース（`IDeckLinkVideoInputFrame : IDeckLinkVideoFrame`）では、基底の末尾のデストラクター 2 項目の
//!   後に派生のメソッドが続く。
//! - `REFIID` は 16 バイトの構造体の値渡し（C の ABI では 2 つの整数レジスターで渡る。`#[repr(C)]` の
//!   `[u8; 16]` の構造体で同じになる）。`HRESULT` は `int`、`ULONG` は `unsigned long`、`bool` は 1 バイト、
//!   `long` は 64 bit、`BMDTimeValue`・`BMDTimeScale` は `int64_t`、`BMD*` の列挙型は `uint32_t`。
//! - 文字列（`GetModelName` など）は呼び出し側が `free` で解放する `const char*`。
//! - インターフェースの版は IID で決まる。`QueryInterface` に下の IID（SDK 11.5〜12.x の
//!   `IDeckLinkInput`・`IDeckLinkInputCallback`）を渡し、ドライバーが知らなければ `E_NOINTERFACE` で
//!   「この版のドライバーには対応しない」として扱う（vtable の配置を推測で使わない）。
//!   新しいドライバーは古い IID にも答える（互換のインターフェースを残す方針）が、実機では確かめていない。
//!   SDK 14 以降の `IDeckLinkVideoFrame` は `GetBytes` を `IDeckLinkVideoBuffer` に移したが、
//!   ここで使う IID の版の vtable には `GetBytes` がある。
//!
//! # 未検証
//!
//! この開発機には DeckLink のボードもドライバー（`libDeckLinkAPI.so`）も無い。ビルドと「ライブラリが無い」の
//! 検出だけを試験している。vtable の配置は説明書と公開されているヘッダーの写しを読んで書いたが、実機では動かしていない。

use senca_codec::dl::{Library, LibrarySearch};
use std::ffi::{CStr, c_char, c_long, c_ulong, c_void};
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

/// 読み込むライブラリ。
pub const LIB_NAMES: &[&str] = &["libDeckLinkAPI.so"];
/// 反復子を作る関数（SDK の `DeckLinkAPIDispatch.cpp` が探す名前。版の新しい順に試す）。
pub const ITERATOR_SYMBOLS: &[&str] = &[
    "CreateDeckLinkIteratorInstance_0004",
    "CreateDeckLinkIteratorInstance_0003",
    "CreateDeckLinkIteratorInstance_0002",
    "CreateDeckLinkIteratorInstance_0001",
];

pub type HResult = i32;
pub const S_OK: HResult = 0;
pub const E_NOINTERFACE: HResult = 0x8000_0004_u32 as i32;
pub const E_FAIL: HResult = 0x8000_0008_u32 as i32;

/// `REFIID`（16 バイトの値渡し）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefIid(pub [u8; 16]);

/// GUID の文字列（`XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`）から、書いた順のバイト列（`CFUUIDBytes`）にする。
pub const fn iid(s: &str) -> RefIid {
    let b = s.as_bytes();
    let mut out = [0u8; 16];
    let mut i = 0;
    let mut k = 0;
    while i < b.len() {
        if b[i] == b'-' {
            i += 1;
            continue;
        }
        out[k] = hex(b[i]) << 4 | hex(b[i + 1]);
        k += 1;
        i += 2;
    }
    assert!(k == 16);
    RefIid(out)
}

const fn hex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("bad hex"),
    }
}

pub const IID_IUNKNOWN: RefIid = iid("00000000-0000-0000-C000-000000000046");
pub const IID_IDECKLINK_INPUT: RefIid = iid("C21CDB6E-F414-46E4-A636-80A566E0ED37");
pub const IID_IDECKLINK_INPUT_CALLBACK: RefIid = iid("C6FCE4C9-C4E4-4047-82FB-5D238232A902");

/// `bmdFormat8BitYUV`（`'2vuy'`、UYVY）。
pub const BMD_FORMAT_8BIT_YUV: u32 = u32::from_be_bytes(*b"2vuy");
/// `bmdVideoInputEnableFormatDetection`。
pub const BMD_VIDEO_INPUT_ENABLE_FORMAT_DETECTION: u32 = 1;
/// `bmdAudioSampleRate48kHz`。
pub const BMD_AUDIO_SAMPLE_RATE_48K: u32 = 48000;
/// `bmdAudioSampleType16bitInteger`。
pub const BMD_AUDIO_SAMPLE_TYPE_16BIT: u32 = 16;
/// `bmdFrameHasNoInputSource`。
pub const BMD_FRAME_HAS_NO_INPUT_SOURCE: u32 = 1 << 31;
/// `bmdVideoInputDisplayModeChanged`。
pub const BMD_VIDEO_INPUT_DISPLAY_MODE_CHANGED: u32 = 1;
/// `BMDFieldDominance` の飛び越し走査の値（`'lowr'`・`'uppr'`）。
pub const BMD_LOWER_FIELD_FIRST: u32 = u32::from_be_bytes(*b"lowr");
pub const BMD_UPPER_FIELD_FIRST: u32 = u32::from_be_bytes(*b"uppr");

type QiFn = unsafe extern "C" fn(*mut c_void, RefIid, *mut *mut c_void) -> HResult;
type RefFn = unsafe extern "C" fn(*mut c_void) -> c_ulong;
type DtorFn = unsafe extern "C" fn(*mut c_void);

#[repr(C)]
struct UnknownVtbl {
    query_interface: QiFn,
    add_ref: RefFn,
    release: RefFn,
}

#[repr(C)]
struct IteratorVtbl {
    unk: UnknownVtbl,
    next: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> HResult,
}

#[repr(C)]
struct DeckLinkVtbl {
    unk: UnknownVtbl,
    get_model_name: unsafe extern "C" fn(*mut c_void, *mut *const c_char) -> HResult,
    get_display_name: unsafe extern "C" fn(*mut c_void, *mut *const c_char) -> HResult,
}

#[repr(C)]
struct InputVtbl {
    unk: UnknownVtbl,
    does_support_video_mode:
        unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32, u32, *mut u32, *mut bool) -> HResult,
    get_display_mode: unsafe extern "C" fn(*mut c_void, u32, *mut *mut c_void) -> HResult,
    get_display_mode_iterator: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> HResult,
    set_screen_preview_callback: unsafe extern "C" fn(*mut c_void, *mut c_void) -> HResult,
    enable_video_input: unsafe extern "C" fn(*mut c_void, u32, u32, u32) -> HResult,
    disable_video_input: unsafe extern "C" fn(*mut c_void) -> HResult,
    get_available_video_frame_count: unsafe extern "C" fn(*mut c_void, *mut u32) -> HResult,
    set_video_input_frame_memory_allocator:
        unsafe extern "C" fn(*mut c_void, *mut c_void) -> HResult,
    enable_audio_input: unsafe extern "C" fn(*mut c_void, u32, u32, u32) -> HResult,
    disable_audio_input: unsafe extern "C" fn(*mut c_void) -> HResult,
    get_available_audio_sample_frame_count: unsafe extern "C" fn(*mut c_void, *mut u32) -> HResult,
    start_streams: unsafe extern "C" fn(*mut c_void) -> HResult,
    stop_streams: unsafe extern "C" fn(*mut c_void) -> HResult,
    pause_streams: unsafe extern "C" fn(*mut c_void) -> HResult,
    flush_streams: unsafe extern "C" fn(*mut c_void) -> HResult,
    set_callback: unsafe extern "C" fn(*mut c_void, *mut c_void) -> HResult,
    get_hardware_reference_clock:
        unsafe extern "C" fn(*mut c_void, i64, *mut i64, *mut i64, *mut i64) -> HResult,
}

#[repr(C)]
struct DisplayModeIteratorVtbl {
    unk: UnknownVtbl,
    next: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> HResult,
}

#[repr(C)]
struct DisplayModeVtbl {
    unk: UnknownVtbl,
    get_name: unsafe extern "C" fn(*mut c_void, *mut *const c_char) -> HResult,
    get_display_mode: unsafe extern "C" fn(*mut c_void) -> u32,
    get_width: unsafe extern "C" fn(*mut c_void) -> c_long,
    get_height: unsafe extern "C" fn(*mut c_void) -> c_long,
    get_frame_rate: unsafe extern "C" fn(*mut c_void, *mut i64, *mut i64) -> HResult,
    get_field_dominance: unsafe extern "C" fn(*mut c_void) -> u32,
    get_flags: unsafe extern "C" fn(*mut c_void) -> u32,
}

/// `IDeckLinkVideoInputFrame`（`IDeckLinkVideoFrame` の後に基底のデストラクター 2 項目、その後に派生のメソッド）。
#[repr(C)]
struct VideoInputFrameVtbl {
    unk: UnknownVtbl,
    get_width: unsafe extern "C" fn(*mut c_void) -> c_long,
    get_height: unsafe extern "C" fn(*mut c_void) -> c_long,
    get_row_bytes: unsafe extern "C" fn(*mut c_void) -> c_long,
    get_pixel_format: unsafe extern "C" fn(*mut c_void) -> u32,
    get_flags: unsafe extern "C" fn(*mut c_void) -> u32,
    get_bytes: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> HResult,
    get_timecode: unsafe extern "C" fn(*mut c_void, u32, *mut *mut c_void) -> HResult,
    get_ancillary_data: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> HResult,
    _dtor_complete: DtorFn,
    _dtor_deleting: DtorFn,
    get_stream_time: unsafe extern "C" fn(*mut c_void, *mut i64, *mut i64, i64) -> HResult,
    get_hardware_reference_timestamp:
        unsafe extern "C" fn(*mut c_void, i64, *mut i64, *mut i64) -> HResult,
}

#[repr(C)]
struct AudioInputPacketVtbl {
    unk: UnknownVtbl,
    get_sample_frame_count: unsafe extern "C" fn(*mut c_void) -> c_long,
    get_bytes: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> HResult,
    get_packet_time: unsafe extern "C" fn(*mut c_void, *mut i64, i64) -> HResult,
}

/// `IDeckLinkInputCallback` の vtable（こちらが実装して SDK に渡す）。
#[repr(C)]
struct CallbackVtbl {
    unk: UnknownVtbl,
    video_input_format_changed: unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32) -> HResult,
    video_input_frame_arrived:
        unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> HResult,
    _dtor_complete: DtorFn,
    _dtor_deleting: DtorFn,
}

/// vtable を取り出す。
///
/// # Safety
///
/// `obj` は NULL でない、`V` を vtable に持つ生きた COM オブジェクトを指すこと。
unsafe fn vtbl<'a, V>(obj: *mut c_void) -> &'a V {
    // SAFETY: 関数の契約により、obj の先頭の 1 語は V の配置の vtable を指す。
    unsafe { &**(obj as *const *const V) }
}

/// 参照を 1 つ持つ COM のポインター（`Drop` で `Release`）。
struct ComPtr(*mut c_void);

impl ComPtr {
    fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for ComPtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` は参照を 1 つ持つ生きたオブジェクトで、どのインターフェースも IUnknown で始まる。
            unsafe { (vtbl::<UnknownVtbl>(self.0).release)(self.0) };
        }
    }
}

/// SDK が返した文字列を取り出して解放する。
///
/// # Safety
///
/// `p` は NULL か、SDK が `malloc` で確保して呼び出し側に渡した NUL 終端の文字列。
unsafe fn take_string(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: 関数の契約により NUL 終端。
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    // SAFETY: Linux の SDK は呼び出し側が free で解放する文字列を返す（SDK の説明書）。一度だけ解放する。
    unsafe { libc::free(p as *mut c_void) };
    s
}

/// 表示モードの情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeInfo {
    /// `BMDDisplayMode`（`'Hp30'` などの 4 文字）。
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// 1 フレームの長さ（`time_scale` 単位）。fps = time_scale / frame_duration。
    pub frame_duration: i64,
    pub time_scale: i64,
    pub interlaced: bool,
}

/// 検出した装置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub index: u32,
    pub model: String,
    pub display_name: String,
    /// `IDeckLinkInput`（この版）が得られたか。得られなければ理由。
    pub input: Result<Vec<ModeInfo>, String>,
}

/// 読み込んだ DeckLink の API。
pub struct DeckLinkApi {
    _lib: Arc<Library>,
    pub name: String,
    pub symbol: &'static str,
    create_iterator: unsafe extern "C" fn() -> *mut c_void,
}

impl std::fmt::Debug for DeckLinkApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeckLinkApi")
            .field("name", &self.name)
            .field("symbol", &self.symbol)
            .finish()
    }
}

/// 表示モードの一覧の上限。
const MAX_MODES: usize = 256;
/// 装置の数の上限。
const MAX_DEVICES: u32 = 64;

impl DeckLinkApi {
    /// `libDeckLinkAPI.so` を読み込む。無ければ理由を返す（Desktop Video が入っていない）。
    pub fn load(search: &LibrarySearch) -> Result<Arc<DeckLinkApi>, String> {
        let lib = search.open(LIB_NAMES)?;
        for sym in ITERATOR_SYMBOLS {
            if !lib.has(sym) {
                continue;
            }
            // SAFETY: `DeckLinkAPIDispatch.cpp` の `typedef IDeckLinkIterator* (*CreateIteratorFunc)(void)` と同じ型。
            // `_lib` を保持するので関数ポインターはその間有効。
            let f: unsafe extern "C" fn() -> *mut c_void = unsafe { lib.sym(sym)? };
            return Ok(Arc::new(DeckLinkApi {
                name: lib.name.clone(),
                symbol: sym,
                create_iterator: f,
                _lib: lib,
            }));
        }
        Err(format!(
            "{}: none of {} found (unsupported Desktop Video version)",
            lib.name,
            ITERATOR_SYMBOLS.join(", ")
        ))
    }

    /// 装置を順に `f(index, IDeckLink*)` に渡す。`f` が `Some` を返したら止める。
    fn each_device<T>(
        &self,
        mut f: impl FnMut(u32, &ComPtr) -> Option<T>,
    ) -> Result<Option<T>, String> {
        // SAFETY: 引数なしの C 関数。ドライバーが無ければ NULL を返す。
        let it = unsafe { (self.create_iterator)() };
        if it.is_null() {
            return Err("DeckLink driver is not running (iterator unavailable)".into());
        }
        let it = ComPtr(it);
        for index in 0..MAX_DEVICES {
            let mut dl: *mut c_void = std::ptr::null_mut();
            // SAFETY: `it` は IDeckLinkIterator。`dl` は書き込み先。S_OK なら参照を 1 つ持つ IDeckLink が入る。
            let hr = unsafe { (vtbl::<IteratorVtbl>(it.as_ptr()).next)(it.as_ptr(), &mut dl) };
            if hr != S_OK || dl.is_null() {
                break;
            }
            let dl = ComPtr(dl);
            if let Some(v) = f(index, &dl) {
                return Ok(Some(v));
            }
        }
        Ok(None)
    }

    /// 装置の一覧。
    pub fn devices(&self) -> Result<Vec<DeviceInfo>, String> {
        let mut out = Vec::new();
        self.each_device(|index, dl| {
            let (model, display_name) = names(dl);
            let input = query_input(dl).and_then(|i| display_modes(&i));
            out.push(DeviceInfo {
                index,
                model,
                display_name,
                input,
            });
            None::<()>
        })?;
        Ok(out)
    }

    /// `index` 番目の装置の入力を開く。
    pub fn open_input(self: &Arc<Self>, index: u32) -> Result<InputHandle, String> {
        let found = self.each_device(|i, dl| {
            (i == index).then(|| {
                query_input(dl).map(|input| {
                    // SAFETY: `dl` は生きた IDeckLink。InputHandle が持つ参照を 1 つ増やす。
                    unsafe { (vtbl::<UnknownVtbl>(dl.as_ptr()).add_ref)(dl.as_ptr()) };
                    (ComPtr(dl.as_ptr()), input)
                })
            })
        })?;
        let (device, input) = found.ok_or_else(|| format!("no DeckLink device #{index}"))??;
        Ok(InputHandle {
            _api: self.clone(),
            _device: device,
            input,
            callback: std::ptr::null_mut(),
            started: false,
        })
    }
}

fn names(dl: &ComPtr) -> (String, String) {
    let mut model: *const c_char = std::ptr::null();
    let mut display: *const c_char = std::ptr::null();
    // SAFETY: `dl` は生きた IDeckLink。書き込み先は有効。成功すれば呼び出し側が free する文字列が入る。
    unsafe {
        let v = vtbl::<DeckLinkVtbl>(dl.as_ptr());
        if (v.get_model_name)(dl.as_ptr(), &mut model) != S_OK {
            model = std::ptr::null();
        }
        if (v.get_display_name)(dl.as_ptr(), &mut display) != S_OK {
            display = std::ptr::null();
        }
        (take_string(model), take_string(display))
    }
}

fn query_input(dl: &ComPtr) -> Result<ComPtr, String> {
    let mut p: *mut c_void = std::ptr::null_mut();
    // SAFETY: `dl` は生きた IDeckLink（IUnknown で始まる）。IID は値渡し、`p` は書き込み先。
    let hr = unsafe {
        (vtbl::<UnknownVtbl>(dl.as_ptr()).query_interface)(dl.as_ptr(), IID_IDECKLINK_INPUT, &mut p)
    };
    if hr == S_OK && !p.is_null() {
        Ok(ComPtr(p))
    } else if hr == E_NOINTERFACE {
        Err("device has no capture input, or the driver does not support IDeckLinkInput (SDK 11.5-12.x interface)".into())
    } else {
        Err(format!("QueryInterface(IDeckLinkInput) failed: {hr:#x}"))
    }
}

fn mode_info(m: *mut c_void) -> ModeInfo {
    // SAFETY: `m` は生きた IDeckLinkDisplayMode。書き込み先は有効。名前は呼び出し側が free する。
    unsafe {
        let v = vtbl::<DisplayModeVtbl>(m);
        let mut name: *const c_char = std::ptr::null();
        if (v.get_name)(m, &mut name) != S_OK {
            name = std::ptr::null();
        }
        let (mut dur, mut scale) = (0i64, 0i64);
        if (v.get_frame_rate)(m, &mut dur, &mut scale) != S_OK {
            dur = 0;
            scale = 0;
        }
        let field = (v.get_field_dominance)(m);
        ModeInfo {
            id: (v.get_display_mode)(m),
            name: take_string(name),
            width: (v.get_width)(m).clamp(0, u32::MAX as c_long) as u32,
            height: (v.get_height)(m).clamp(0, u32::MAX as c_long) as u32,
            frame_duration: dur,
            time_scale: scale,
            interlaced: field == BMD_LOWER_FIELD_FIRST || field == BMD_UPPER_FIELD_FIRST,
        }
    }
}

fn display_modes(input: &ComPtr) -> Result<Vec<ModeInfo>, String> {
    let mut it: *mut c_void = std::ptr::null_mut();
    // SAFETY: `input` は生きた IDeckLinkInput（この IID の版の配置）。`it` は書き込み先。
    let hr = unsafe {
        (vtbl::<InputVtbl>(input.as_ptr()).get_display_mode_iterator)(input.as_ptr(), &mut it)
    };
    if hr != S_OK || it.is_null() {
        return Err(format!("GetDisplayModeIterator failed: {hr:#x}"));
    }
    let it = ComPtr(it);
    let mut out = Vec::new();
    for _ in 0..MAX_MODES {
        let mut m: *mut c_void = std::ptr::null_mut();
        // SAFETY: `it` は生きた IDeckLinkDisplayModeIterator。`m` は書き込み先。
        let hr =
            unsafe { (vtbl::<DisplayModeIteratorVtbl>(it.as_ptr()).next)(it.as_ptr(), &mut m) };
        if hr != S_OK || m.is_null() {
            break;
        }
        let m = ComPtr(m);
        out.push(mode_info(m.as_ptr()));
    }
    Ok(out)
}

/// コールバックから取り込み側へ渡すもの（SDK のバッファは複製済み）。
#[derive(Debug)]
pub enum Event {
    Video {
        data: Vec<u8>,
        width: u32,
        height: u32,
        row_bytes: usize,
        pixel_format: u32,
        /// 受け取った時刻（`CLOCK_MONOTONIC`）。
        arrival_ns: i64,
        /// ハードウェアの参照時刻（ナノ秒）。
        hardware_ns: Option<i64>,
        /// ストリームの時刻（ナノ秒）と 1 フレームの長さ。
        stream_ns: Option<(i64, i64)>,
    },
    Audio {
        data: Vec<u8>,
        frames: usize,
        /// 標本の通し番号（48 kHz 単位のパケットの時刻）。
        sample_time: Option<i64>,
        arrival_ns: i64,
    },
    /// 入力の形式が変わった（新しい表示モード）。
    FormatChanged(ModeInfo),
    /// 信号が無いフレーム。
    NoSignal,
}

/// コールバックが共有する状態。
struct Shared {
    tx: Mutex<Option<SyncSender<Event>>>,
    /// 形式が変わったときに入力を設定し直すための IDeckLinkInput（参照は InputHandle が持つ）。
    input: AtomicPtr<c_void>,
    audio_channels: AtomicUsize,
    dropped: AtomicU64,
}

impl Shared {
    fn send(&self, e: Event) {
        let guard = self.tx.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(tx) = guard.as_ref() {
            match tx.try_send(e) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }
}

/// `IDeckLinkInputCallback` を実装するオブジェクト（先頭が vtable）。
#[repr(C)]
struct CallbackObject {
    vtbl: *const CallbackVtbl,
    refs: AtomicUsize,
    shared: Arc<Shared>,
}

static CALLBACK_VTBL: CallbackVtbl = CallbackVtbl {
    unk: UnknownVtbl {
        query_interface: cb_query_interface,
        add_ref: cb_add_ref,
        release: cb_release,
    },
    video_input_format_changed: cb_format_changed,
    video_input_frame_arrived: cb_frame_arrived,
    _dtor_complete: cb_dtor,
    _dtor_deleting: cb_dtor,
};

unsafe extern "C" fn cb_query_interface(
    this: *mut c_void,
    iid: RefIid,
    out: *mut *mut c_void,
) -> HResult {
    if out.is_null() {
        return E_FAIL;
    }
    if iid == IID_IUNKNOWN || iid == IID_IDECKLINK_INPUT_CALLBACK {
        // SAFETY: `this` は CallbackObject、`out` は呼び出し側の書き込み先。
        unsafe {
            cb_add_ref(this);
            *out = this;
        }
        S_OK
    } else {
        // SAFETY: `out` は呼び出し側の書き込み先。
        unsafe { *out = std::ptr::null_mut() };
        E_NOINTERFACE
    }
}

unsafe extern "C" fn cb_add_ref(this: *mut c_void) -> c_ulong {
    // SAFETY: `this` は生きた CallbackObject（参照を持つ者だけが呼ぶ）。
    let o = unsafe { &*(this as *const CallbackObject) };
    (o.refs.fetch_add(1, Ordering::AcqRel) + 1) as c_ulong
}

unsafe extern "C" fn cb_release(this: *mut c_void) -> c_ulong {
    // SAFETY: `this` は生きた CallbackObject。
    let o = unsafe { &*(this as *const CallbackObject) };
    let left = o.refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: 最後の参照。Box::into_raw で作ったものを一度だけ戻して解放する。
        drop(unsafe { Box::from_raw(this as *mut CallbackObject) });
    }
    left as c_ulong
}

unsafe extern "C" fn cb_dtor(_this: *mut c_void) {
    // SDK は Release で解放を任せ、デストラクターを直接は呼ばない（保護されたデストラクター）。何もしない。
}

unsafe extern "C" fn cb_format_changed(
    this: *mut c_void,
    events: u32,
    mode: *mut c_void,
    _detected: u32,
) -> HResult {
    let r = std::panic::catch_unwind(|| {
        // SAFETY: `this` は生きた CallbackObject（SDK が参照を持つ間だけ呼ぶ）。
        let o = unsafe { &*(this as *const CallbackObject) };
        if mode.is_null() || events & BMD_VIDEO_INPUT_DISPLAY_MODE_CHANGED == 0 {
            return;
        }
        let info = mode_info(mode);
        let input = o.shared.input.load(Ordering::Acquire);
        if !input.is_null() {
            // SDK の見本と同じ手順: 止める → 新しいモードで有効にし直す → 流れを捨てる → 再開。
            // SAFETY: `input` は InputHandle が参照を持つ IDeckLinkInput（止めるときに NULL にしてから解放する）。
            unsafe {
                let v = vtbl::<InputVtbl>(input);
                (v.pause_streams)(input);
                (v.enable_video_input)(
                    input,
                    info.id,
                    BMD_FORMAT_8BIT_YUV,
                    BMD_VIDEO_INPUT_ENABLE_FORMAT_DETECTION,
                );
                (v.flush_streams)(input);
                (v.start_streams)(input);
            }
        }
        o.shared.send(Event::FormatChanged(info));
    });
    if r.is_ok() { S_OK } else { E_FAIL }
}

unsafe extern "C" fn cb_frame_arrived(
    this: *mut c_void,
    video: *mut c_void,
    audio: *mut c_void,
) -> HResult {
    let arrival_ns = crate::clock::monotonic_ns();
    let r = std::panic::catch_unwind(|| {
        // SAFETY: `this` は生きた CallbackObject。
        let o = unsafe { &*(this as *const CallbackObject) };
        if !video.is_null() {
            // SAFETY: `video` はこの呼び出しの間だけ有効な IDeckLinkVideoInputFrame。中身はここで複製する。
            let ev = unsafe { copy_video(video, arrival_ns) };
            o.shared.send(ev);
        }
        if !audio.is_null() {
            let ch = o.shared.audio_channels.load(Ordering::Relaxed);
            // SAFETY: `audio` はこの呼び出しの間だけ有効な IDeckLinkAudioInputPacket。
            if let Some(ev) = unsafe { copy_audio(audio, ch, arrival_ns) } {
                o.shared.send(ev);
            }
        }
    });
    if r.is_ok() { S_OK } else { E_FAIL }
}

/// 1 フレームの上限（8K の UYVY の余裕を見た値）。
const MAX_FRAME_BYTES: usize = 7680 * 4320 * 4;

/// # Safety
///
/// `f` は生きた IDeckLinkVideoInputFrame。
unsafe fn copy_video(f: *mut c_void, arrival_ns: i64) -> Event {
    // SAFETY: 関数の契約。書き込み先は有効。GetBytes のバッファは row_bytes * height バイト読める（SDK の説明書）。
    unsafe {
        let v = vtbl::<VideoInputFrameVtbl>(f);
        if (v.get_flags)(f) & BMD_FRAME_HAS_NO_INPUT_SOURCE != 0 {
            return Event::NoSignal;
        }
        let width = (v.get_width)(f).clamp(0, 16384) as u32;
        let height = (v.get_height)(f).clamp(0, 16384) as u32;
        let row_bytes = (v.get_row_bytes)(f).max(0) as usize;
        let pixel_format = (v.get_pixel_format)(f);
        let len = row_bytes.saturating_mul(height as usize);
        let mut bytes: *mut c_void = std::ptr::null_mut();
        let data = if len > 0
            && len <= MAX_FRAME_BYTES
            && (v.get_bytes)(f, &mut bytes) == S_OK
            && !bytes.is_null()
        {
            std::slice::from_raw_parts(bytes as *const u8, len).to_vec()
        } else {
            Vec::new()
        };
        let (mut t, mut d) = (0i64, 0i64);
        let stream_ns =
            ((v.get_stream_time)(f, &mut t, &mut d, 1_000_000_000) == S_OK).then_some((t, d));
        let (mut ht, mut hd) = (0i64, 0i64);
        let hardware_ns =
            ((v.get_hardware_reference_timestamp)(f, 1_000_000_000, &mut ht, &mut hd) == S_OK)
                .then_some(ht);
        Event::Video {
            data,
            width,
            height,
            row_bytes,
            pixel_format,
            arrival_ns,
            hardware_ns,
            stream_ns,
        }
    }
}

/// # Safety
///
/// `p` は生きた IDeckLinkAudioInputPacket で、16 bit・`channels` チャンネルで有効にした入力のもの。
unsafe fn copy_audio(p: *mut c_void, channels: usize, arrival_ns: i64) -> Option<Event> {
    // SAFETY: 関数の契約。GetBytes のバッファは frames * channels * 2 バイト読める（16 bit で有効にした）。
    unsafe {
        let v = vtbl::<AudioInputPacketVtbl>(p);
        let frames = (v.get_sample_frame_count)(p).max(0) as usize;
        let len = frames.checked_mul(channels)?.checked_mul(2)?;
        if len == 0 || len > 48000 * 16 * 2 {
            return None;
        }
        let mut bytes: *mut c_void = std::ptr::null_mut();
        if (v.get_bytes)(p, &mut bytes) != S_OK || bytes.is_null() {
            return None;
        }
        let data = std::slice::from_raw_parts(bytes as *const u8, len).to_vec();
        let mut t = 0i64;
        let sample_time =
            ((v.get_packet_time)(p, &mut t, BMD_AUDIO_SAMPLE_RATE_48K as i64) == S_OK).then_some(t);
        Some(Event::Audio {
            data,
            frames,
            sample_time,
            arrival_ns,
        })
    }
}

/// 開いた入力。
pub struct InputHandle {
    _api: Arc<DeckLinkApi>,
    _device: ComPtr,
    input: ComPtr,
    callback: *mut CallbackObject,
    started: bool,
}

// SAFETY: DeckLink の COM オブジェクトはスレッドに縛られない（SDK は自分のスレッドからコールバックを呼ぶ）。
// InputHandle は 1 つのスレッドから `&mut self` で使う。コールバックの状態は `Shared`（Mutex・原子変数）で共有する。
unsafe impl Send for InputHandle {}

impl std::fmt::Debug for InputHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputHandle")
            .field("started", &self.started)
            .finish()
    }
}

impl InputHandle {
    /// 入力を有効にして流れを始める。フレームは `tx` に送る（満杯なら捨てて数える）。
    pub fn start(
        &mut self,
        mode: u32,
        audio_channels: Option<u32>,
        tx: SyncSender<Event>,
    ) -> Result<(), String> {
        if self.started {
            return Err("already started".into());
        }
        let shared = Arc::new(Shared {
            tx: Mutex::new(Some(tx)),
            input: AtomicPtr::new(self.input.as_ptr()),
            audio_channels: AtomicUsize::new(audio_channels.unwrap_or(0) as usize),
            dropped: AtomicU64::new(0),
        });
        let cb = Box::into_raw(Box::new(CallbackObject {
            vtbl: &CALLBACK_VTBL,
            refs: AtomicUsize::new(1),
            shared,
        }));
        self.callback = cb;
        let input = self.input.as_ptr();
        // SAFETY: `input` は生きた IDeckLinkInput（この IID の版の配置）。`cb` は先頭が vtable の生きたオブジェクトで、
        // 自分の参照を 1 つ持つ（SDK は SetCallback で AddRef し、外すときに Release する）。
        let r = unsafe {
            let v = vtbl::<InputVtbl>(input);
            let hr = (v.set_callback)(input, cb as *mut c_void);
            if hr != S_OK {
                Err(format!("SetCallback failed: {hr:#x}"))
            } else {
                let hr = (v.enable_video_input)(
                    input,
                    mode,
                    BMD_FORMAT_8BIT_YUV,
                    BMD_VIDEO_INPUT_ENABLE_FORMAT_DETECTION,
                );
                if hr != S_OK {
                    Err(format!(
                        "EnableVideoInput({}) failed: {hr:#x} (mode not supported, or device in use)",
                        fourcc_str(mode)
                    ))
                } else if let Some(ch) = audio_channels
                    && (v.enable_audio_input)(
                        input,
                        BMD_AUDIO_SAMPLE_RATE_48K,
                        BMD_AUDIO_SAMPLE_TYPE_16BIT,
                        ch,
                    ) != S_OK
                {
                    Err(format!("EnableAudioInput({ch} ch) failed"))
                } else {
                    let hr = (v.start_streams)(input);
                    if hr != S_OK {
                        Err(format!("StartStreams failed: {hr:#x}"))
                    } else {
                        Ok(())
                    }
                }
            }
        };
        self.started = true; // 失敗しても stop で後始末する
        if r.is_err() {
            self.stop();
        }
        r
    }

    /// コールバックが満杯で捨てたフレームの数。
    pub fn dropped(&self) -> u64 {
        if self.callback.is_null() {
            return 0;
        }
        // SAFETY: 自分の参照を持っている間は生きている。
        unsafe { &*self.callback }
            .shared
            .dropped
            .load(Ordering::Relaxed)
    }

    /// 流れを止めて入力を無効にし、コールバックを外す（何度呼んでもよい）。
    pub fn stop(&mut self) {
        if !self.started {
            return;
        }
        self.started = false;
        let input = self.input.as_ptr();
        // SAFETY: `input` は生きた IDeckLinkInput。StopStreams の後はコールバックは呼ばれない（SDK の説明書）。
        unsafe {
            let v = vtbl::<InputVtbl>(input);
            (v.stop_streams)(input);
            (v.disable_audio_input)(input);
            (v.disable_video_input)(input);
            (v.set_callback)(input, std::ptr::null_mut());
        }
        if !self.callback.is_null() {
            // SAFETY: 自分の参照で生きている。送り先を外し、形式変更で使う入力のポインターを消してから自分の参照を返す。
            unsafe {
                let o = &*self.callback;
                o.shared
                    .input
                    .store(std::ptr::null_mut(), Ordering::Release);
                *o.shared.tx.lock().unwrap_or_else(|p| p.into_inner()) = None;
                cb_release(self.callback as *mut c_void);
            }
            self.callback = std::ptr::null_mut();
        }
    }
}

impl Drop for InputHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `BMDDisplayMode` などの 4 文字。
pub fn fourcc_str(v: u32) -> String {
    v.to_be_bytes()
        .iter()
        .map(|b| {
            if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '?'
            }
        })
        .collect()
}

/// 4 文字から `BMDDisplayMode`。
pub fn parse_fourcc(s: &str) -> Option<u32> {
    let b: [u8; 4] = s.as_bytes().try_into().ok()?;
    Some(u32::from_be_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iids_and_constants() {
        assert_eq!(
            IID_IDECKLINK_INPUT.0,
            [
                0xC2, 0x1C, 0xDB, 0x6E, 0xF4, 0x14, 0x46, 0xE4, 0xA6, 0x36, 0x80, 0xA5, 0x66, 0xE0,
                0xED, 0x37
            ]
        );
        assert_eq!(IID_IUNKNOWN.0[8], 0xC0);
        assert_eq!(IID_IUNKNOWN.0[15], 0x46);
        assert_eq!(BMD_FORMAT_8BIT_YUV, 0x3276_7579);
        assert_eq!(parse_fourcc("Hp30"), Some(0x4870_3330));
        assert_eq!(fourcc_str(0x4870_3330), "Hp30");
        assert_eq!(parse_fourcc("toolong"), None);
        assert_eq!(BMD_LOWER_FIELD_FIRST, 0x6C6F_7772);
        // vtable の項目数（関数ポインターの数）
        let p = size_of::<usize>();
        assert_eq!(size_of::<InputVtbl>() / p, 3 + 17);
        assert_eq!(size_of::<VideoInputFrameVtbl>() / p, 3 + 8 + 2 + 2);
        assert_eq!(size_of::<CallbackVtbl>() / p, 3 + 2 + 2);
        assert_eq!(size_of::<DisplayModeVtbl>() / p, 3 + 7);
        assert_eq!(size_of::<RefIid>(), 16);
    }

    /// 自前のコールバックオブジェクトの参照数と QueryInterface（SDK なしで確かめられる部分）。
    #[test]
    fn callback_object_refcount_and_qi() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let shared = Arc::new(Shared {
            tx: Mutex::new(Some(tx)),
            input: AtomicPtr::new(std::ptr::null_mut()),
            audio_channels: AtomicUsize::new(2),
            dropped: AtomicU64::new(0),
        });
        let cb = Box::into_raw(Box::new(CallbackObject {
            vtbl: &CALLBACK_VTBL,
            refs: AtomicUsize::new(1),
            shared: shared.clone(),
        })) as *mut c_void;
        // SAFETY: cb は上で作った生きたオブジェクト。SDK と同じく vtable 経由で呼ぶ。
        unsafe {
            let v = vtbl::<CallbackVtbl>(cb);
            let mut out = std::ptr::null_mut();
            assert_eq!(
                (v.unk.query_interface)(cb, IID_IDECKLINK_INPUT_CALLBACK, &mut out),
                S_OK
            );
            assert_eq!(out, cb);
            assert_eq!(
                (v.unk.query_interface)(cb, IID_IDECKLINK_INPUT, &mut out),
                E_NOINTERFACE
            );
            assert!(out.is_null());
            // 映像・音声なしの到着は何も送らない。モードなしの形式変更も無視する。
            assert_eq!(
                (v.video_input_frame_arrived)(cb, std::ptr::null_mut(), std::ptr::null_mut()),
                S_OK
            );
            assert_eq!(
                (v.video_input_format_changed)(cb, 1, std::ptr::null_mut(), 0),
                S_OK
            );
            assert!(rx.try_recv().is_err());
            shared.send(Event::NoSignal);
            shared.send(Event::NoSignal); // 満杯で捨てる
            assert_eq!(shared.dropped.load(Ordering::Relaxed), 1);
            assert_eq!((v.unk.release)(cb), 1);
            assert_eq!((v.unk.release)(cb), 0); // ここで解放
        }
        assert_eq!(Arc::strong_count(&shared), 1);
    }
}
