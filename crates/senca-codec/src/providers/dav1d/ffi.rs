//! dav1d の FFI（`unsafe` はこのモジュールだけ）。
//!
//! 関数は `libdav1d.so.7` から dlopen で取り出す。構造体は bindgen の [`bindings`]（dav1d 1.5.3 のヘッダー、API 7）。
//! API の主版が 7 でないライブラリは読み込まない（構造体の配置が変わりうるため）。

#![allow(clippy::all)]

use crate::dl::{Library, LibrarySearch};
use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::mem::MaybeUninit;
use std::ptr;
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

use bindings::{Dav1dData, Dav1dPicture, Dav1dSettings};

pub const LIB_NAMES: &[&str] = &["libdav1d.so.7"];

type VersionFn = unsafe extern "C" fn() -> *const c_char;
type VersionApiFn = unsafe extern "C" fn() -> c_uint;
type DefaultSettingsFn = unsafe extern "C" fn(*mut Dav1dSettings);
type OpenFn = unsafe extern "C" fn(*mut *mut c_void, *const Dav1dSettings) -> c_int;
type SendDataFn = unsafe extern "C" fn(*mut c_void, *mut Dav1dData) -> c_int;
type GetPictureFn = unsafe extern "C" fn(*mut c_void, *mut Dav1dPicture) -> c_int;
type CloseFn = unsafe extern "C" fn(*mut *mut c_void);
type DataCreateFn = unsafe extern "C" fn(*mut Dav1dData, usize) -> *mut u8;
type DataUnrefFn = unsafe extern "C" fn(*mut Dav1dData);
type PictureUnrefFn = unsafe extern "C" fn(*mut Dav1dPicture);

pub struct Dav1dLib {
    lib: Arc<Library>,
    version: VersionFn,
    default_settings: DefaultSettingsFn,
    open: OpenFn,
    send_data: SendDataFn,
    get_picture: GetPictureFn,
    close: CloseFn,
    data_create: DataCreateFn,
    data_unref: DataUnrefFn,
    picture_unref: PictureUnrefFn,
}

/// dav1d の `DAV1D_ERR(EAGAIN)`。
pub const EAGAIN: c_int = -libc::EAGAIN;

impl Dav1dLib {
    pub fn load(search: &LibrarySearch) -> Result<Arc<Dav1dLib>, String> {
        let lib = search.open(LIB_NAMES)?;
        // SAFETY: 型は dav1d.h・data.h・picture.h の宣言どおり。`lib` を保持して関数ポインターより長生きさせる。
        let me = unsafe {
            let version_api: VersionApiFn = lib.sym("dav1d_version_api")?;
            let api = version_api();
            if (api >> 16) & 0xff != bindings::DAV1D_API_VERSION_MAJOR {
                return Err(format!(
                    "{}: dav1d API {} is not supported (need 7)",
                    lib.name,
                    (api >> 16) & 0xff
                ));
            }
            Dav1dLib {
                version: lib.sym("dav1d_version")?,
                default_settings: lib.sym("dav1d_default_settings")?,
                open: lib.sym("dav1d_open")?,
                send_data: lib.sym("dav1d_send_data")?,
                get_picture: lib.sym("dav1d_get_picture")?,
                close: lib.sym("dav1d_close")?,
                data_create: lib.sym("dav1d_data_create")?,
                data_unref: lib.sym("dav1d_data_unref")?,
                picture_unref: lib.sym("dav1d_picture_unref")?,
                lib,
            }
        };
        Ok(Arc::new(me))
    }

    pub fn library_name(&self) -> &str {
        &self.lib.name
    }

    pub fn version(&self) -> String {
        // SAFETY: 静的な NUL 終端文字列を返す。
        unsafe {
            let p = (self.version)();
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }
}

/// デコーダーの設定（安全な側から渡す値）。
pub struct Settings {
    pub threads: u32,
    pub max_frame_delay: u32,
    pub frame_size_limit: u32,
}

/// 復号した 1 枚（コピー済み、ライブラリのメモリを指さない）。
pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// 0: I400, 1: I420, 2: I422, 3: I444
    pub layout: u32,
    pub bpc: u32,
    pub planes: [Vec<u8>; 3],
    /// 各面の 1 行のバイト数（詰めた後）。
    pub row_bytes: [usize; 3],
    pub timestamp: i64,
    pub keyframe: bool,
}

pub struct Context {
    lib: Arc<Dav1dLib>,
    ctx: *mut c_void,
}

// SAFETY: Dav1dContext は内部でスレッドを持つが、API の呼び出しは 1 つのスレッドから順に行えばよい（dav1d.h）。
// `&mut self` でだけ使うので同時に呼ばれない。
unsafe impl Send for Context {}

impl Context {
    pub fn open(lib: Arc<Dav1dLib>, s: &Settings) -> Result<Context, c_int> {
        // SAFETY: Dav1dSettings は POD。dav1d_default_settings が全項目を埋め、その後で値を上書きする。
        let mut settings: Dav1dSettings = unsafe { MaybeUninit::zeroed().assume_init() };
        unsafe { (lib.default_settings)(&mut settings) };
        settings.n_threads = s.threads.min(bindings::DAV1D_MAX_THREADS) as c_int;
        settings.max_frame_delay = s.max_frame_delay.min(bindings::DAV1D_MAX_FRAME_DELAY) as c_int;
        settings.frame_size_limit = s.frame_size_limit;
        // ログは出さない（codec worker の stderr を汚さない）。
        settings.logger.callback = None;
        settings.logger.cookie = ptr::null_mut();
        let mut ctx: *mut c_void = ptr::null_mut();
        // SAFETY: settings は有効、ctx は出力先。成功時は dav1d_close で閉じる。
        let r = unsafe { (lib.open)(&mut ctx, &settings) };
        if r < 0 || ctx.is_null() {
            return Err(r);
        }
        Ok(Context { lib, ctx })
    }

    /// データを 1 つ渡す。中で `EAGAIN` のときは `drain` で出力を取り出してから再送する。
    pub fn send(
        &mut self,
        payload: &[u8],
        timestamp: i64,
        out: &mut Vec<Picture>,
        max_pixels: u64,
    ) -> Result<(), c_int> {
        if payload.is_empty() {
            return Ok(());
        }
        // SAFETY: Dav1dData は POD。dav1d_data_create が sz バイトのバッファを確保して data に入れる。
        let mut data: Dav1dData = unsafe { MaybeUninit::zeroed().assume_init() };
        let buf = unsafe { (self.lib.data_create)(&mut data, payload.len()) };
        if buf.is_null() {
            return Err(-libc::ENOMEM);
        }
        // SAFETY: buf は payload.len() バイト書ける新しい領域。重ならない。
        unsafe { ptr::copy_nonoverlapping(payload.as_ptr(), buf, payload.len()) };
        data.m.timestamp = timestamp;
        let mut spins = 0;
        let result = loop {
            // SAFETY: ctx と data は有効。dav1d は消費した分だけ data を進め、全部消費したら sz = 0 にする。
            let r = unsafe { (self.lib.send_data)(self.ctx, &mut data) };
            if r == 0 && data.sz == 0 {
                break Ok(());
            }
            if r != 0 && r != EAGAIN {
                break Err(r);
            }
            let before = out.len();
            if let Err(e) = self.drain(out, max_pixels) {
                break Err(e);
            }
            spins += 1;
            if out.len() == before && r == EAGAIN && spins > 64 {
                break Err(EAGAIN);
            }
        };
        if data.sz > 0 {
            // SAFETY: 消費されなかったデータの参照を返す（dav1d_data_unref は sz = 0 のとき何もしない）。
            unsafe { (self.lib.data_unref)(&mut data) };
        }
        result
    }

    /// 出力できる絵をすべて取り出す。
    pub fn drain(&mut self, out: &mut Vec<Picture>, max_pixels: u64) -> Result<(), c_int> {
        loop {
            // SAFETY: Dav1dPicture は POD。dav1d_get_picture が成功時に中身を埋め、参照を持たせる。
            let mut pic: Dav1dPicture = unsafe { MaybeUninit::zeroed().assume_init() };
            let r = unsafe { (self.lib.get_picture)(self.ctx, &mut pic) };
            if r == EAGAIN {
                return Ok(());
            }
            if r < 0 {
                return Err(r);
            }
            let copied = copy_picture(&pic, max_pixels);
            // SAFETY: get_picture で得た参照を一度だけ返す。
            unsafe { (self.lib.picture_unref)(&mut pic) };
            out.push(copied.ok_or(-libc::EINVAL)?);
        }
    }
}

/// ライブラリの絵を詰めて複製する。大きさが不正なら `None`。
fn copy_picture(pic: &Dav1dPicture, max_pixels: u64) -> Option<Picture> {
    let w = u32::try_from(pic.p.w).ok().filter(|&v| v > 0)?;
    let h = u32::try_from(pic.p.h).ok().filter(|&v| v > 0)?;
    if w as u64 * h as u64 > max_pixels {
        return None;
    }
    let bpc = u32::try_from(pic.p.bpc).ok()?;
    let bytes = if bpc > 8 { 2 } else { 1 };
    let layout = pic.p.layout as u32;
    let (cw, ch) = match layout {
        0 => (0, 0),
        1 => (w.div_ceil(2), h.div_ceil(2)),
        2 => (w.div_ceil(2), h),
        3 => (w, h),
        _ => return None,
    };
    let dims = [(w, h), (cw, ch), (cw, ch)];
    let mut planes: [Vec<u8>; 3] = Default::default();
    let mut row_bytes = [0usize; 3];
    for i in 0..3 {
        let (pw, ph) = dims[i];
        if pw == 0 || pic.data[i].is_null() {
            continue;
        }
        let stride = pic.stride[if i == 0 { 0 } else { 1 }];
        let row = pw as usize * bytes;
        if stride.unsigned_abs() < row {
            return None;
        }
        let mut v = Vec::with_capacity(row * ph as usize);
        for y in 0..ph as isize {
            // SAFETY: dav1d の絵は data[i] から stride ごとに ph 行、各行 row バイト以上が有効（picture.h の契約）。
            // stride は負でもよいので isize で計算する。
            unsafe {
                let p = (pic.data[i] as *const u8).offset(y * stride);
                v.extend_from_slice(std::slice::from_raw_parts(p, row));
            }
        }
        planes[i] = v;
        row_bytes[i] = row;
    }
    let keyframe = !pic.frame_hdr.is_null()
        // SAFETY: frame_hdr は絵の参照が生きている間有効（unref の前に読む）。
        && unsafe { (*pic.frame_hdr).frame_type } == bindings::Dav1dFrameType_DAV1D_FRAME_TYPE_KEY;
    Some(Picture {
        width: w,
        height: h,
        layout,
        bpc,
        planes,
        row_bytes,
        timestamp: pic.m.timestamp,
        keyframe,
    })
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: dav1d_open で開いた文脈を一度だけ閉じる（関数が ctx を NULL にする）。
        unsafe { (self.lib.close)(&mut self.ctx) }
    }
}
