//! libva（VA-API）の呼び出し。`unsafe` はこのモジュールに閉じ込める（mod.rs で `#[allow(unsafe_code)]`）。
//!
//! # 境界
//!
//! - `libva.so.2`・`libva-drm.so.2` を [`crate::dl`] で読み込み、関数ポインターをヘッダー（`va.h`・`va_drm.h`、
//!   libva 2.23.0）の宣言どおりの型で取り出す。構造体は bindgen で生成した [`sys`]（レイアウト検査付き）。
//! - VADisplay・各 ID の寿命は [`Display`]・[`DecodeSession`] が持ち、`Drop` で必ず解放する。
//! - ドライバーが返すポインター（ベンダー文字列、マップしたバッファ）は、長さを確かめてから複製し、外へは出さない。
//! - libva の情報・エラーのメッセージは標準出力・標準エラーに出さない（codec worker の標準出力はプロトコルに使う）。
//!
//! 実機（GPU）での動作は未検証（開発機に /dev/dri が無い）。読み込み・装置なしの扱いだけを試験している。

#![allow(clippy::missing_safety_doc)]

#[path = "bindings.rs"]
#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    unnecessary_transmutes,
    unsafe_op_in_unsafe_fn,
    clippy::all
)]
pub mod sys;

use crate::dl::{Library, LibrarySearch};
use crate::error::{CodecError, Result};
use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Arc;
use sys::*;

type FnGetDisplayDrm = unsafe extern "C" fn(fd: c_int) -> VADisplay;
type MessageCallback = Option<unsafe extern "C" fn(user: *mut c_void, msg: *const c_char)>;
type FnSetCallback =
    unsafe extern "C" fn(VADisplay, MessageCallback, *mut c_void) -> MessageCallback;
type FnInitialize = unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus;
type FnTerminate = unsafe extern "C" fn(VADisplay) -> VAStatus;
type FnQueryVendorString = unsafe extern "C" fn(VADisplay) -> *const c_char;
type FnErrorStr = unsafe extern "C" fn(VAStatus) -> *const c_char;
type FnMaxNum = unsafe extern "C" fn(VADisplay) -> c_int;
type FnQueryConfigProfiles =
    unsafe extern "C" fn(VADisplay, *mut VAProfile, *mut c_int) -> VAStatus;
type FnQueryConfigEntrypoints =
    unsafe extern "C" fn(VADisplay, VAProfile, *mut VAEntrypoint, *mut c_int) -> VAStatus;
type FnGetConfigAttributes = unsafe extern "C" fn(
    VADisplay,
    VAProfile,
    VAEntrypoint,
    *mut VAConfigAttrib,
    c_int,
) -> VAStatus;
type FnCreateConfig = unsafe extern "C" fn(
    VADisplay,
    VAProfile,
    VAEntrypoint,
    *mut VAConfigAttrib,
    c_int,
    *mut VAConfigID,
) -> VAStatus;
type FnDestroyConfig = unsafe extern "C" fn(VADisplay, VAConfigID) -> VAStatus;
type FnCreateSurfaces = unsafe extern "C" fn(
    VADisplay,
    c_uint,
    c_uint,
    c_uint,
    *mut VASurfaceID,
    c_uint,
    *mut c_void, // VASurfaceAttrib *（使わないので NULL）
    c_uint,
) -> VAStatus;
type FnDestroySurfaces = unsafe extern "C" fn(VADisplay, *mut VASurfaceID, c_int) -> VAStatus;
type FnCreateContext = unsafe extern "C" fn(
    VADisplay,
    VAConfigID,
    c_int,
    c_int,
    c_int,
    *mut VASurfaceID,
    c_int,
    *mut VAContextID,
) -> VAStatus;
type FnDestroyContext = unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus;
type FnCreateBuffer = unsafe extern "C" fn(
    VADisplay,
    VAContextID,
    VABufferType,
    c_uint,
    c_uint,
    *mut c_void,
    *mut VABufferID,
) -> VAStatus;
type FnDestroyBuffer = unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus;
type FnMapBuffer = unsafe extern "C" fn(VADisplay, VABufferID, *mut *mut c_void) -> VAStatus;
type FnUnmapBuffer = unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus;
type FnBeginPicture = unsafe extern "C" fn(VADisplay, VAContextID, VASurfaceID) -> VAStatus;
type FnRenderPicture =
    unsafe extern "C" fn(VADisplay, VAContextID, *mut VABufferID, c_int) -> VAStatus;
type FnEndPicture = unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus;
type FnSyncSurface = unsafe extern "C" fn(VADisplay, VASurfaceID) -> VAStatus;
type FnDeriveImage = unsafe extern "C" fn(VADisplay, VASurfaceID, *mut VAImage) -> VAStatus;
type FnCreateImage =
    unsafe extern "C" fn(VADisplay, *mut VAImageFormat, c_int, c_int, *mut VAImage) -> VAStatus;
type FnGetImage = unsafe extern "C" fn(
    VADisplay,
    VASurfaceID,
    c_int,
    c_int,
    c_uint,
    c_uint,
    VAImageID,
) -> VAStatus;
type FnDestroyImage = unsafe extern "C" fn(VADisplay, VAImageID) -> VAStatus;

/// 読み込んだ libva の関数表。
pub struct VaLib {
    _va: Arc<Library>,
    _drm: Arc<Library>,
    /// 読み込んだライブラリの説明。
    pub description: String,
    get_display_drm: FnGetDisplayDrm,
    set_error_callback: Option<FnSetCallback>,
    set_info_callback: Option<FnSetCallback>,
    initialize: FnInitialize,
    terminate: FnTerminate,
    query_vendor_string: FnQueryVendorString,
    error_str: FnErrorStr,
    max_num_profiles: FnMaxNum,
    max_num_entrypoints: FnMaxNum,
    query_config_profiles: FnQueryConfigProfiles,
    query_config_entrypoints: FnQueryConfigEntrypoints,
    get_config_attributes: FnGetConfigAttributes,
    create_config: FnCreateConfig,
    destroy_config: FnDestroyConfig,
    create_surfaces: FnCreateSurfaces,
    destroy_surfaces: FnDestroySurfaces,
    create_context: FnCreateContext,
    destroy_context: FnDestroyContext,
    create_buffer: FnCreateBuffer,
    destroy_buffer: FnDestroyBuffer,
    map_buffer: FnMapBuffer,
    unmap_buffer: FnUnmapBuffer,
    begin_picture: FnBeginPicture,
    render_picture: FnRenderPicture,
    end_picture: FnEndPicture,
    sync_surface: FnSyncSurface,
    derive_image: FnDeriveImage,
    create_image: FnCreateImage,
    get_image: FnGetImage,
    destroy_image: FnDestroyImage,
}

impl std::fmt::Debug for VaLib {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaLib")
            .field("description", &self.description)
            .finish()
    }
}

impl VaLib {
    /// `libva.so.2` と `libva-drm.so.2` を読み込む。無ければ理由を返す。
    pub fn load(search: &LibrarySearch) -> std::result::Result<Arc<VaLib>, String> {
        // libva-drm は libva に依存するので先に libva を読み込む（同じ soname の読み込み済みのものが使われる）。
        let va = search.open(&["libva.so.2"])?;
        let drm = search.open(&["libva-drm.so.2"])?;
        // SAFETY: 各型は va.h / va_drm.h（libva 2.23.0）の宣言を写したもの。ライブラリは `_va`・`_drm` で保持する。
        unsafe {
            Ok(Arc::new(VaLib {
                description: format!("{}, {}", va.name, drm.name),
                get_display_drm: drm.sym("vaGetDisplayDRM")?,
                // 1.0 より前の libva には無い（無ければ既定の出力のまま）。
                set_error_callback: va.sym("vaSetErrorCallback").ok(),
                set_info_callback: va.sym("vaSetInfoCallback").ok(),
                initialize: va.sym("vaInitialize")?,
                terminate: va.sym("vaTerminate")?,
                query_vendor_string: va.sym("vaQueryVendorString")?,
                error_str: va.sym("vaErrorStr")?,
                max_num_profiles: va.sym("vaMaxNumProfiles")?,
                max_num_entrypoints: va.sym("vaMaxNumEntrypoints")?,
                query_config_profiles: va.sym("vaQueryConfigProfiles")?,
                query_config_entrypoints: va.sym("vaQueryConfigEntrypoints")?,
                get_config_attributes: va.sym("vaGetConfigAttributes")?,
                create_config: va.sym("vaCreateConfig")?,
                destroy_config: va.sym("vaDestroyConfig")?,
                create_surfaces: va.sym("vaCreateSurfaces")?,
                destroy_surfaces: va.sym("vaDestroySurfaces")?,
                create_context: va.sym("vaCreateContext")?,
                destroy_context: va.sym("vaDestroyContext")?,
                create_buffer: va.sym("vaCreateBuffer")?,
                destroy_buffer: va.sym("vaDestroyBuffer")?,
                map_buffer: va.sym("vaMapBuffer")?,
                unmap_buffer: va.sym("vaUnmapBuffer")?,
                begin_picture: va.sym("vaBeginPicture")?,
                render_picture: va.sym("vaRenderPicture")?,
                end_picture: va.sym("vaEndPicture")?,
                sync_surface: va.sym("vaSyncSurface")?,
                derive_image: va.sym("vaDeriveImage")?,
                create_image: va.sym("vaCreateImage")?,
                get_image: va.sym("vaGetImage")?,
                destroy_image: va.sym("vaDestroyImage")?,
                _va: va,
                _drm: drm,
            }))
        }
    }

    fn status_message(&self, status: VAStatus) -> String {
        // SAFETY: vaErrorStr は静的な NUL 終端文字列（未知の値でも "unknown libva error"）を返す。
        let p = unsafe { (self.error_str)(status) };
        if p.is_null() {
            return format!("VA status {status}");
        }
        // SAFETY: 上のとおり NUL 終端の静的文字列。
        let s = unsafe { CStr::from_ptr(p) };
        format!("{} ({status})", s.to_string_lossy())
    }

    fn check(&self, status: VAStatus, what: &str) -> Result<()> {
        if status == VA_STATUS_SUCCESS as VAStatus {
            Ok(())
        } else {
            Err(CodecError::backend(format!(
                "{what}: {}",
                self.status_message(status)
            )))
        }
    }
}

/// libva のメッセージを捨てる（標準出力を汚さない）。
unsafe extern "C" fn quiet(_user: *mut c_void, _msg: *const c_char) {}

/// DRM の render node で開いた VADisplay。
pub struct Display {
    lib: Arc<VaLib>,
    dpy: VADisplay,
    // VADisplay より後に閉じる（Drop で vaTerminate の後）。
    _file: std::fs::File,
    pub major: i32,
    pub minor: i32,
}

// SAFETY: VADisplay は作ったスレッドに縛られない（libva はスレッドローカルな状態を持たない）。
// `Display` は `Sync` ではないので、同時に複数のスレッドから使われることはない。
unsafe impl Send for Display {}

impl std::fmt::Debug for Display {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Display")
            .field("major", &self.major)
            .field("minor", &self.minor)
            .finish()
    }
}

impl Display {
    /// render node を開いて初期化する。装置で無いファイル・ドライバーが無いときは `Err`（落ちない）。
    pub fn open(lib: &Arc<VaLib>, path: &Path) -> std::result::Result<Display, String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        // SAFETY: 有効な fd を渡す。fd は `file` が `Display` の寿命の間保持する。失敗なら NULL が返る。
        let dpy = unsafe { (lib.get_display_drm)(file.as_raw_fd()) };
        if dpy.is_null() {
            return Err(format!(
                "{}: vaGetDisplayDRM failed (not a DRM device?)",
                path.display()
            ));
        }
        // SAFETY: dpy は有効。コールバックは何もしない関数で、ユーザー文脈は使わない。
        unsafe {
            if let Some(f) = lib.set_error_callback {
                f(dpy, Some(quiet), std::ptr::null_mut());
            }
            if let Some(f) = lib.set_info_callback {
                f(dpy, Some(quiet), std::ptr::null_mut());
            }
        }
        let (mut major, mut minor) = (0, 0);
        // SAFETY: dpy は有効、出力先はこの関数のローカル変数。
        let st = unsafe { (lib.initialize)(dpy, &mut major, &mut minor) };
        if st != VA_STATUS_SUCCESS as VAStatus {
            let msg = lib.status_message(st);
            // SAFETY: vaGetDisplayDRM で得た dpy は、初期化に失敗しても vaTerminate で解放する。
            unsafe { (lib.terminate)(dpy) };
            return Err(format!("{}: vaInitialize failed: {msg}", path.display()));
        }
        Ok(Display {
            lib: lib.clone(),
            dpy,
            _file: file,
            major,
            minor,
        })
    }

    /// ドライバーのベンダー文字列（例: "Intel iHD driver for Intel(R) Gen Graphics - 24.1.0 ()"）。
    pub fn vendor(&self) -> String {
        // SAFETY: 初期化済みの dpy。返る文字列は dpy が生きている間有効な NUL 終端文字列（NULL もありうる）。
        let p = unsafe { (self.lib.query_vendor_string)(self.dpy) };
        if p.is_null() {
            return String::new();
        }
        // SAFETY: 上のとおり。すぐ複製する。長さは 1 KiB で打ち切る。
        let s = unsafe { CStr::from_ptr(p) }.to_bytes();
        String::from_utf8_lossy(&s[..s.len().min(1024)]).into_owned()
    }

    pub fn profiles(&self) -> Vec<VAProfile> {
        // SAFETY: 初期化済みの dpy。
        let max = unsafe { (self.lib.max_num_profiles)(self.dpy) }.clamp(0, 1024) as usize;
        let mut v: Vec<VAProfile> = vec![0; max];
        let mut n: c_int = 0;
        // SAFETY: 配列は vaMaxNumProfiles 個ぶん確保してある（仕様上それを超えて書かない）。
        let st = unsafe { (self.lib.query_config_profiles)(self.dpy, v.as_mut_ptr(), &mut n) };
        if st != VA_STATUS_SUCCESS as VAStatus {
            return Vec::new();
        }
        v.truncate((n.max(0) as usize).min(max));
        v
    }

    pub fn entrypoints(&self, profile: VAProfile) -> Vec<VAEntrypoint> {
        // SAFETY: 初期化済みの dpy。
        let max = unsafe { (self.lib.max_num_entrypoints)(self.dpy) }.clamp(0, 1024) as usize;
        let mut v: Vec<VAEntrypoint> = vec![0; max];
        let mut n: c_int = 0;
        // SAFETY: 配列は vaMaxNumEntrypoints 個ぶん確保してある。
        let st = unsafe {
            (self.lib.query_config_entrypoints)(self.dpy, profile, v.as_mut_ptr(), &mut n)
        };
        if st != VA_STATUS_SUCCESS as VAStatus {
            return Vec::new();
        }
        v.truncate((n.max(0) as usize).min(max));
        v
    }

    /// 設定の属性 1 つ。対応しなければ `None`。
    pub fn config_attribute(
        &self,
        profile: VAProfile,
        entrypoint: VAEntrypoint,
        attr: VAConfigAttribType,
    ) -> Option<u32> {
        let mut a = VAConfigAttrib {
            type_: attr,
            value: 0,
        };
        // SAFETY: 初期化済みの dpy、属性 1 つの配列。
        let st =
            unsafe { (self.lib.get_config_attributes)(self.dpy, profile, entrypoint, &mut a, 1) };
        (st == VA_STATUS_SUCCESS as VAStatus && a.value != VA_ATTRIB_NOT_SUPPORTED)
            .then_some(a.value)
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: 初期化済みの dpy を 1 度だけ解放する。この後 `_file` が fd を閉じる。
        unsafe { (self.lib.terminate)(self.dpy) };
    }
}

/// すべて 0 の構造体（VA-API のパラメーター構造体は整数と整数の共用体だけなので、0 は有効な値）。
pub fn zeroed_pic_params() -> VAPictureParameterBufferH264 {
    // SAFETY: 整数・整数の配列・整数のビットフィールドの共用体だけからなる POD。
    unsafe { std::mem::zeroed() }
}

pub fn zeroed_iq_matrix() -> VAIQMatrixBufferH264 {
    // SAFETY: u8 の配列と u32 だけの POD。
    unsafe { std::mem::zeroed() }
}

pub fn zeroed_slice_params() -> VASliceParameterBufferH264 {
    // SAFETY: 整数と整数の配列（VAPictureH264 も整数だけ）の POD。
    unsafe { std::mem::zeroed() }
}

/// 復号のセッション（設定・サーフェス・コンテキスト）。
pub struct DecodeSession {
    display: Display,
    config: Option<VAConfigID>,
    context: Option<VAContextID>,
    surfaces: Vec<VASurfaceID>,
    width: u32,
    height: u32,
}

impl std::fmt::Debug for DecodeSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodeSession")
            .field("surfaces", &self.surfaces.len())
            .finish()
    }
}

impl DecodeSession {
    pub fn new(display: Display) -> Self {
        DecodeSession {
            display,
            config: None,
            context: None,
            surfaces: Vec::new(),
            width: 0,
            height: 0,
        }
    }

    fn teardown(&mut self) {
        let lib = self.display.lib.clone();
        let dpy = self.display.dpy;
        // SAFETY: 自分で作った ID を 1 度だけ解放する（コンテキスト → サーフェス → 設定の順）。
        unsafe {
            if let Some(c) = self.context.take() {
                (lib.destroy_context)(dpy, c);
            }
            if !self.surfaces.is_empty() {
                (lib.destroy_surfaces)(
                    dpy,
                    self.surfaces.as_mut_ptr(),
                    self.surfaces.len() as c_int,
                );
                self.surfaces.clear();
            }
            if let Some(c) = self.config.take() {
                (lib.destroy_config)(dpy, c);
            }
        }
    }

    /// 作り直す（ストリームの大きさが変わったとき）。
    pub fn configure(
        &mut self,
        profile: VAProfile,
        width: u32,
        height: u32,
        surfaces: usize,
    ) -> Result<()> {
        self.teardown();
        if surfaces == 0
            || surfaces > 64
            || width == 0
            || height == 0
            || width > 16384
            || height > 16384
        {
            return Err(CodecError::invalid_config(
                "bad VA-API surface configuration",
            ));
        }
        let lib = self.display.lib.clone();
        let dpy = self.display.dpy;
        let mut attr = VAConfigAttrib {
            type_: VAConfigAttribType_VAConfigAttribRTFormat,
            value: VA_RT_FORMAT_YUV420,
        };
        let mut config: VAConfigID = 0;
        // SAFETY: 初期化済みの dpy、属性 1 つ、出力はローカル変数。
        let st = unsafe {
            (lib.create_config)(
                dpy,
                profile,
                VAEntrypoint_VAEntrypointVLD,
                &mut attr,
                1,
                &mut config,
            )
        };
        lib.check(st, "vaCreateConfig")?;
        self.config = Some(config);
        let mut ids: Vec<VASurfaceID> = vec![VA_INVALID_SURFACE; surfaces];
        // SAFETY: ids は surfaces 個ぶん確保。属性は渡さない（NULL, 0）。
        let st = unsafe {
            (lib.create_surfaces)(
                dpy,
                VA_RT_FORMAT_YUV420,
                width,
                height,
                ids.as_mut_ptr(),
                surfaces as c_uint,
                std::ptr::null_mut(),
                0,
            )
        };
        if let Err(e) = lib.check(st, "vaCreateSurfaces") {
            self.teardown();
            return Err(e);
        }
        self.surfaces = ids;
        let mut ctx: VAContextID = 0;
        // SAFETY: 上で作った設定とサーフェス。サーフェスの配列は呼び出しの間有効。
        let st = unsafe {
            (lib.create_context)(
                dpy,
                config,
                width as c_int,
                height as c_int,
                VA_PROGRESSIVE as c_int,
                self.surfaces.as_mut_ptr(),
                self.surfaces.len() as c_int,
                &mut ctx,
            )
        };
        if let Err(e) = lib.check(st, "vaCreateContext") {
            self.teardown();
            return Err(e);
        }
        self.context = Some(ctx);
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// サーフェス番号から VA の ID。範囲外は `VA_INVALID_SURFACE`。
    pub fn surface_id(&self, idx: usize) -> VASurfaceID {
        self.surfaces
            .get(idx)
            .copied()
            .unwrap_or(VA_INVALID_SURFACE)
    }

    fn create_buffer<T>(&self, ty: VABufferType, value: &T) -> Result<VABufferID> {
        let ctx = self
            .context
            .ok_or_else(|| CodecError::internal("VA-API session is not configured"))?;
        let mut id: VABufferID = VA_INVALID_ID;
        // SAFETY: `value` は T の大きさのメモリ。libva はその内容を複製して保持する（呼び出し後は参照しない）。
        let st = unsafe {
            (self.display.lib.create_buffer)(
                self.display.dpy,
                ctx,
                ty,
                std::mem::size_of::<T>() as c_uint,
                1,
                value as *const T as *mut c_void,
                &mut id,
            )
        };
        self.display.lib.check(st, "vaCreateBuffer")?;
        Ok(id)
    }

    fn create_data_buffer(&self, data: &[u8]) -> Result<VABufferID> {
        let ctx = self
            .context
            .ok_or_else(|| CodecError::internal("VA-API session is not configured"))?;
        let len = c_uint::try_from(data.len())
            .map_err(|_| CodecError::invalid_input("slice too large"))?;
        let mut id: VABufferID = VA_INVALID_ID;
        // SAFETY: data は len バイト。libva は内容を複製する。
        let st = unsafe {
            (self.display.lib.create_buffer)(
                self.display.dpy,
                ctx,
                VABufferType_VASliceDataBufferType,
                len,
                1,
                data.as_ptr() as *mut c_void,
                &mut id,
            )
        };
        self.display.lib.check(st, "vaCreateBuffer(slice data)")?;
        Ok(id)
    }

    /// 1 ピクチャーを復号して完了を待つ。
    pub fn decode_h264(
        &mut self,
        target: usize,
        pic: &VAPictureParameterBufferH264,
        iq: &VAIQMatrixBufferH264,
        slices: &[(VASliceParameterBufferH264, &[u8])],
    ) -> Result<()> {
        let ctx = self
            .context
            .ok_or_else(|| CodecError::internal("VA-API session is not configured"))?;
        let surface = self.surface_id(target);
        if surface == VA_INVALID_SURFACE {
            return Err(CodecError::internal("VA-API surface index out of range"));
        }
        let lib = self.display.lib.clone();
        let dpy = self.display.dpy;
        let mut buffers: Vec<VABufferID> = Vec::with_capacity(2 + 2 * slices.len());
        let result = (|| -> Result<()> {
            buffers.push(self.create_buffer(VABufferType_VAPictureParameterBufferType, pic)?);
            buffers.push(self.create_buffer(VABufferType_VAIQMatrixBufferType, iq)?);
            // SAFETY: ctx と surface はこのセッションで作ったもの。
            lib.check(
                unsafe { (lib.begin_picture)(dpy, ctx, surface) },
                "vaBeginPicture",
            )?;
            let mut r = (|| -> Result<()> {
                // SAFETY: buffers はこのコンテキストで作ったバッファ ID の配列。
                lib.check(
                    unsafe { (lib.render_picture)(dpy, ctx, buffers.as_mut_ptr(), 2) },
                    "vaRenderPicture(picture)",
                )?;
                for (sp, data) in slices {
                    let mut pair = [
                        self.create_buffer(VABufferType_VASliceParameterBufferType, sp)?,
                        VA_INVALID_ID,
                    ];
                    buffers.push(pair[0]);
                    pair[1] = self.create_data_buffer(data)?;
                    buffers.push(pair[1]);
                    // SAFETY: 上で作った 2 つのバッファ。
                    lib.check(
                        unsafe { (lib.render_picture)(dpy, ctx, pair.as_mut_ptr(), 2) },
                        "vaRenderPicture(slice)",
                    )?;
                }
                Ok(())
            })();
            // vaBeginPicture の後は失敗しても必ず vaEndPicture で閉じる。
            // SAFETY: 開始したピクチャーを閉じる。
            let end = lib.check(unsafe { (lib.end_picture)(dpy, ctx) }, "vaEndPicture");
            if r.is_ok() {
                r = end;
            }
            r?;
            // SAFETY: このセッションのサーフェス。
            lib.check(unsafe { (lib.sync_surface)(dpy, surface) }, "vaSyncSurface")
        })();
        for b in buffers {
            // SAFETY: 自分で作ったバッファを 1 度だけ解放する（libva 2.x では vaRenderPicture は解放しない）。
            unsafe { (lib.destroy_buffer)(dpy, b) };
        }
        result
    }

    /// サーフェスを NV12 として読み出し、切り抜いて詰めたバッファ（Y の後に UV）を返す。
    pub fn read_nv12(&mut self, idx: usize, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let surface = self.surface_id(idx);
        if surface == VA_INVALID_SURFACE {
            return Err(CodecError::internal("VA-API surface index out of range"));
        }
        if !x.is_multiple_of(2)
            || !y.is_multiple_of(2)
            || x + w > self.width
            || y + h > self.height
            || w == 0
            || h == 0
        {
            return Err(CodecError::internal("bad crop rectangle"));
        }
        let lib = self.display.lib.clone();
        let dpy = self.display.dpy;
        // SAFETY: 0 で埋めた VAImage は有効な初期値（出力用）。
        let mut image: VAImage = unsafe { std::mem::zeroed() };
        image.image_id = VA_INVALID_ID;
        // まず vaDeriveImage（コピー無し）を試し、だめなら vaCreateImage + vaGetImage。
        // SAFETY: このセッションのサーフェス、出力はローカル変数。
        let derived = unsafe { (lib.derive_image)(dpy, surface, &mut image) }
            == VA_STATUS_SUCCESS as VAStatus
            && image.format.fourcc == VA_FOURCC_NV12;
        if !derived {
            if image.image_id != VA_INVALID_ID {
                // SAFETY: 上で得た（NV12 でない）画像を解放する。
                unsafe { (lib.destroy_image)(dpy, image.image_id) };
            }
            // SAFETY: 0 で埋めた VAImageFormat に NV12 を指定する。
            let mut fmt: VAImageFormat = unsafe { std::mem::zeroed() };
            fmt.fourcc = VA_FOURCC_NV12;
            fmt.byte_order = 1; // VA_LSB_FIRST
            fmt.bits_per_pixel = 12;
            // SAFETY: 初期化済みの dpy、出力はローカル変数。
            let st = unsafe {
                (lib.create_image)(
                    dpy,
                    &mut fmt,
                    self.width as c_int,
                    self.height as c_int,
                    &mut image,
                )
            };
            lib.check(st, "vaCreateImage")?;
            // SAFETY: 作った画像にサーフェスの全体を写す。
            let st = unsafe {
                (lib.get_image)(dpy, surface, 0, 0, self.width, self.height, image.image_id)
            };
            if let Err(e) = lib.check(st, "vaGetImage") {
                // SAFETY: 作った画像を解放する。
                unsafe { (lib.destroy_image)(dpy, image.image_id) };
                return Err(e);
            }
        }
        let result = (|| -> Result<Vec<u8>> {
            let mut ptr: *mut c_void = std::ptr::null_mut();
            // SAFETY: 画像のバッファをマップする。
            lib.check(
                unsafe { (lib.map_buffer)(dpy, image.buf, &mut ptr) },
                "vaMapBuffer",
            )?;
            if ptr.is_null() {
                // SAFETY: マップに成功したバッファを戻す。
                unsafe { (lib.unmap_buffer)(dpy, image.buf) };
                return Err(CodecError::backend("vaMapBuffer returned NULL"));
            }
            // SAFETY: ドライバーは data_size バイトをマップしている（VAImage の契約）。読み取りだけ。
            let data =
                unsafe { std::slice::from_raw_parts(ptr as *const u8, image.data_size as usize) };
            let out = copy_nv12(data, &image, x, y, w, h);
            // SAFETY: マップしたバッファを戻す（以後 data は使わない）。
            unsafe { (lib.unmap_buffer)(dpy, image.buf) };
            out
        })();
        // SAFETY: 得た画像を 1 度だけ解放する。
        unsafe { (lib.destroy_image)(dpy, image.image_id) };
        result
    }
}

/// マップした NV12 の画像から、切り抜きを詰めて複製する（範囲はすべて検査する）。
fn copy_nv12(data: &[u8], image: &VAImage, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
    if image.num_planes < 2 {
        return Err(CodecError::backend("NV12 image with fewer than 2 planes"));
    }
    let (x, y, w, h) = (x as usize, y as usize, w as usize, h as usize);
    let cw = w.div_ceil(2) * 2;
    let ch = h.div_ceil(2);
    let mut out = Vec::with_capacity(w * h + cw * ch);
    let planes = [(0usize, x, y, w, h), (1usize, x, y / 2, cw, ch)];
    for (p, px, py, row, rows) in planes {
        let pitch = image.pitches[p] as usize;
        let off = image.offsets[p] as usize;
        if pitch < px + row {
            return Err(CodecError::backend("VA image pitch too small"));
        }
        for r in 0..rows {
            let start = off
                .checked_add(
                    (py + r)
                        .checked_mul(pitch)
                        .ok_or_else(|| CodecError::backend("overflow"))?,
                )
                .and_then(|v| v.checked_add(px))
                .ok_or_else(|| CodecError::backend("overflow"))?;
            let line = data
                .get(start..start + row)
                .ok_or_else(|| CodecError::backend("VA image smaller than expected"))?;
            out.extend_from_slice(line);
        }
    }
    Ok(out)
}

impl Drop for DecodeSession {
    fn drop(&mut self) {
        self.teardown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_nv12_crops_and_checks_bounds() {
        // 4x4、pitch 8
        let mut data = vec![0u8; 8 * 4 + 8 * 2];
        for (i, b) in data.iter_mut().enumerate() {
            *b = i as u8;
        }
        // SAFETY: 試験用に 0 で埋めた POD。
        let mut img: VAImage = unsafe { std::mem::zeroed() };
        img.num_planes = 2;
        img.pitches = [8, 8, 0];
        img.offsets = [0, 32, 0];
        let out = copy_nv12(&data, &img, 2, 2, 2, 2).unwrap();
        assert_eq!(out, vec![18, 19, 26, 27, 42, 43]);
        img.offsets[1] = 60;
        assert!(copy_nv12(&data, &img, 0, 0, 4, 4).is_err());
        img.pitches[0] = 2;
        assert!(copy_nv12(&data, &img, 0, 0, 4, 4).is_err());
    }
}
