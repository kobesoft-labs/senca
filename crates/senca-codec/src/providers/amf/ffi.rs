//! AMD AMF の C インターフェース（vtable 構造体）を呼ぶ部分（このプロバイダーで `unsafe` を使うのはここだけ）。
//!
//! # 境界
//!
//! - `libamfrt64.so.1` を [`crate::dl`] で読み込み、`AMFQueryVersion`・`AMFInit` を取り出して工場（`AMFFactory`）を得る。
//!   工場は AMF のランタイムが持つプロセスで 1 つのオブジェクトで、解放しない（ランタイムの約束）。
//! - AMF のオブジェクト（context・component・surface・buffer・data・caps）は C では `pVtbl` を先頭に持つ構造体で、
//!   参照カウント（`Acquire`/`Release`）で寿命を管理する。この層の型はそれぞれ 1 つの参照を所有し、`Drop` で
//!   `Release` を 1 回呼ぶ。`Component` は `Context` より先に解放する（[`Session`] のフィールド順）。
//! - プロパティ名・コンポーネント ID は `wchar_t`（Linux では 4 バイト）の NUL 終端文字列。呼ぶ前に NUL 終端を検査する。
//! - 面・バッファの中身は `GetNative` が返すホストメモリのポインターを、`GetHPitch`・`GetWidth`・`GetHeight`・
//!   `GetSize` で範囲を確かめてから読み書きする。
//! - ハードウェアの加速（`AMF_ACCEL_HARDWARE`）のコンポーネントだけを使う（ADR-0004）。
//!
//! GPU の無い開発機では、ライブラリが無いことの検出までしか確かめられない。それ以外の経路は未検証。

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    unnecessary_transmutes,
    unused_imports,
    clippy::all,
    clippy::pedantic
)]
#[path = "bindings/amf.rs"]
pub(super) mod sys;

use crate::dl::{Library, LibrarySearch};
use crate::error::{CodecError, Result};
use crate::frame::{PixelFormat, VideoFrame};
use std::ffi::c_void;
use std::sync::Arc;
use sys::*;

/// 読み込む soname。
pub const LIBRARY_NAMES: &[&str] = &["libamfrt64.so.1"];

/// ヘッダーの版（AMF_FULL_VERSION = AMF_MAKE_FULL_VERSION(1, 5, 3, 0)）。
pub const AMF_FULL_VERSION: u64 = ((AMF_VERSION_MAJOR as u64) << 48)
    | ((AMF_VERSION_MINOR as u64) << 32)
    | ((AMF_VERSION_RELEASE as u64) << 16)
    | AMF_VERSION_BUILD_NUM as u64;

type FnQueryVersion = unsafe extern "C" fn(*mut amf_uint64) -> AMF_RESULT;
type FnInit = unsafe extern "C" fn(amf_uint64, *mut *mut AMFFactory) -> AMF_RESULT;

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> AMFGuid {
    AMFGuid {
        data1: d1,
        data2: d2,
        data3: d3,
        data41: d4[0],
        data42: d4[1],
        data43: d4[2],
        data44: d4[3],
        data45: d4[4],
        data46: d4[5],
        data47: d4[6],
        data48: d4[7],
    }
}

/// core/Context.h: AMF_DECLARE_IID(AMFContext1, 0xd9e9f868, 0x6220, 0x44c6, 0xa2, 0x2f, 0x7c, 0xd6, 0xda, 0xc6, 0x86, 0x46)
pub const IID_AMF_CONTEXT1: AMFGuid = guid(
    0xd9e9f868,
    0x6220,
    0x44c6,
    [0xa2, 0x2f, 0x7c, 0xd6, 0xda, 0xc6, 0x86, 0x46],
);
/// core/Buffer.h: AMF_DECLARE_IID(AMFBuffer, 0xb04b7248, 0xb6f0, 0x4321, 0xb6, 0x91, 0xba, 0xa4, 0x74, 0xf, 0x9f, 0xcb)
pub const IID_AMF_BUFFER: AMFGuid = guid(
    0xb04b7248,
    0xb6f0,
    0x4321,
    [0xb6, 0x91, 0xba, 0xa4, 0x74, 0x0f, 0x9f, 0xcb],
);
/// core/Surface.h: AMF_DECLARE_IID(AMFSurface, 0x3075dbe3, 0x8718, 0x4cfa, 0x86, 0xfb, 0x21, 0x14, 0xc0, 0xa5, 0xa4, 0x51)
pub const IID_AMF_SURFACE: AMFGuid = guid(
    0x3075dbe3,
    0x8718,
    0x4cfa,
    [0x86, 0xfb, 0x21, 0x14, 0xc0, 0xa5, 0xa4, 0x51],
);

/// 結果コードの名前。
pub fn result_name(r: AMF_RESULT) -> String {
    let s = match r {
        AMF_OK => "AMF_OK",
        AMF_FAIL => "AMF_FAIL",
        AMF_UNEXPECTED => "AMF_UNEXPECTED",
        AMF_INVALID_ARG => "AMF_INVALID_ARG",
        AMF_OUT_OF_RANGE => "AMF_OUT_OF_RANGE",
        AMF_OUT_OF_MEMORY => "AMF_OUT_OF_MEMORY",
        AMF_NO_INTERFACE => "AMF_NO_INTERFACE",
        AMF_NOT_IMPLEMENTED => "AMF_NOT_IMPLEMENTED",
        AMF_NOT_SUPPORTED => "AMF_NOT_SUPPORTED",
        AMF_NOT_FOUND => "AMF_NOT_FOUND",
        AMF_NOT_INITIALIZED => "AMF_NOT_INITIALIZED",
        AMF_INVALID_FORMAT => "AMF_INVALID_FORMAT",
        AMF_WRONG_STATE => "AMF_WRONG_STATE",
        AMF_NO_DEVICE => "AMF_NO_DEVICE",
        AMF_EOF => "AMF_EOF",
        AMF_REPEAT => "AMF_REPEAT",
        AMF_INPUT_FULL => "AMF_INPUT_FULL",
        AMF_RESOLUTION_CHANGED => "AMF_RESOLUTION_CHANGED",
        AMF_INVALID_RESOLUTION => "AMF_INVALID_RESOLUTION",
        AMF_CODEC_NOT_SUPPORTED => "AMF_CODEC_NOT_SUPPORTED",
        AMF_SURFACE_FORMAT_NOT_SUPPORTED => "AMF_SURFACE_FORMAT_NOT_SUPPORTED",
        AMF_DECODER_NOT_PRESENT => "AMF_DECODER_NOT_PRESENT",
        AMF_ENCODER_NOT_PRESENT => "AMF_ENCODER_NOT_PRESENT",
        AMF_NEED_MORE_INPUT => "AMF_NEED_MORE_INPUT",
        AMF_VULKAN_FAILED => "AMF_VULKAN_FAILED",
        _ => return format!("AMF_RESULT({r})"),
    };
    s.to_string()
}

fn check(r: AMF_RESULT, what: &str) -> Result<()> {
    if r == AMF_OK {
        return Ok(());
    }
    let msg = format!("{what}: {}", result_name(r));
    Err(match r {
        AMF_NO_DEVICE
        | AMF_VULKAN_FAILED
        | AMF_DECODER_NOT_PRESENT
        | AMF_ENCODER_NOT_PRESENT
        | AMF_OUT_OF_MEMORY
        | AMF_CODEC_NOT_SUPPORTED => CodecError::unavailable(msg),
        AMF_INVALID_ARG
        | AMF_OUT_OF_RANGE
        | AMF_INVALID_RESOLUTION
        | AMF_SURFACE_FORMAT_NOT_SUPPORTED
        | AMF_INVALID_FORMAT => CodecError::invalid_config(msg),
        _ => CodecError::backend(msg),
    })
}

/// NUL 終端の wchar_t 列か確かめてポインターにする。
fn wptr(s: &[i32]) -> Result<*const wchar_t> {
    if s.last() != Some(&0) {
        return Err(CodecError::internal(
            "AMF wide string is not NUL-terminated",
        ));
    }
    Ok(s.as_ptr() as *const wchar_t)
}

/// 読み込んだ AMF ランタイム。
pub struct Amf {
    lib: Arc<Library>,
    factory: *mut AMFFactory,
    version: u64,
}

// SAFETY: AMFFactory はランタイムが持つプロセス全体の単一オブジェクトで、AMF の文書上スレッド安全
// （CreateContext・CreateComponent を別スレッドから呼んでよい）。このポインターは解放しない。
unsafe impl Send for Amf {}
// SAFETY: 同上。`Amf` を通じて呼ぶのは工場の生成関数だけ。
unsafe impl Sync for Amf {}

impl std::fmt::Debug for Amf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Amf")
            .field("lib", &self.lib.name)
            .field("version", &self.version)
            .finish()
    }
}

/// 版の表記（"1.5.3.0"）。
pub fn version_string(v: u64) -> String {
    format!(
        "{}.{}.{}.{}",
        (v >> 48) & 0xffff,
        (v >> 32) & 0xffff,
        (v >> 16) & 0xffff,
        v & 0xffff
    )
}

impl Amf {
    pub fn load(search: &LibrarySearch) -> std::result::Result<Arc<Amf>, String> {
        let lib = search.open(LIBRARY_NAMES)?;
        // SAFETY: 型は core/Factory.h の AMFQueryVersion_Fn・AMFInit_Fn（C の呼び出し規約）と同じ。
        let (qv, init): (FnQueryVersion, FnInit) =
            unsafe { (lib.sym("AMFQueryVersion")?, lib.sym("AMFInit")?) };
        let mut version: amf_uint64 = 0;
        // SAFETY: 書き込み先の u64 を渡す。
        let r = unsafe { qv(&mut version) };
        if r != AMF_OK {
            return Err(format!("AMFQueryVersion failed: {}", result_name(r)));
        }
        let mut factory: *mut AMFFactory = std::ptr::null_mut();
        // SAFETY: ヘッダーの版と、書き込み先のポインターを渡す。
        let r = unsafe { init(AMF_FULL_VERSION, &mut factory) };
        if r != AMF_OK || factory.is_null() {
            return Err(format!(
                "AMFInit failed: {} (runtime {})",
                result_name(r),
                version_string(version)
            ));
        }
        Ok(Arc::new(Amf {
            lib,
            factory,
            version,
        }))
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn library_name(&self) -> &str {
        &self.lib.name
    }

    /// Vulkan で初期化した context を作る（Linux の AMF は Vulkan の装置を使う）。
    pub fn create_context(self: &Arc<Self>) -> Result<Context> {
        let mut ctx: *mut AMFContext = std::ptr::null_mut();
        // SAFETY: factory は有効（AMFInit が返した）。vtbl の関数は C の宣言どおり。
        let r = unsafe {
            let f = (*self.factory).pVtbl;
            match (*f).CreateContext {
                Some(cc) => cc(self.factory, &mut ctx),
                None => AMF_NOT_IMPLEMENTED,
            }
        };
        check(r, "AMFFactory::CreateContext")?;
        if ctx.is_null() {
            return Err(CodecError::backend(
                "AMFFactory::CreateContext returned NULL",
            ));
        }
        let mut c = Context {
            amf: self.clone(),
            ctx,
            ctx1: std::ptr::null_mut(),
        };
        let mut p: *mut c_void = std::ptr::null_mut();
        // SAFETY: ctx は有効。QueryInterface は参照を 1 つ増やして ctx1 を返す（Drop で Release）。
        let r = unsafe {
            match (*(*ctx).pVtbl).QueryInterface {
                Some(q) => q(ctx, &IID_AMF_CONTEXT1, &mut p),
                None => AMF_NOT_IMPLEMENTED,
            }
        };
        check(r, "AMFContext::QueryInterface(AMFContext1)")?;
        c.ctx1 = p as *mut AMFContext1;
        if c.ctx1.is_null() {
            return Err(CodecError::unavailable("AMFContext1 not available"));
        }
        // SAFETY: ctx1 は有効。NULL を渡すとランタイムが既定の Vulkan 装置を作る。
        let r = unsafe {
            match (*(*c.ctx1).pVtbl).InitVulkan {
                Some(iv) => iv(c.ctx1, std::ptr::null_mut()),
                None => AMF_NOT_IMPLEMENTED,
            }
        };
        check(r, "AMFContext1::InitVulkan").map_err(|e| CodecError::unavailable(e.message))?;
        Ok(c)
    }
}

/// AMF の context（Vulkan で初期化済み）。
pub struct Context {
    amf: Arc<Amf>,
    ctx: *mut AMFContext,
    ctx1: *mut AMFContext1,
}

// SAFETY: context は 1 つのスレッドから順に使う（`Sync` にしない）。AMF のオブジェクトはスレッドに
// 結び付いた状態を持たないので、別のスレッドへ移してよい。
unsafe impl Send for Context {}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: ctx・ctx1 はこの値が 1 つずつ参照を持つ。Terminate してから参照を返す。
        unsafe {
            if let Some(t) = (*(*self.ctx).pVtbl).Terminate {
                t(self.ctx);
            }
            if !self.ctx1.is_null()
                && let Some(rel) = (*(*self.ctx1).pVtbl).Release
            {
                rel(self.ctx1);
            }
            if let Some(rel) = (*(*self.ctx).pVtbl).Release {
                rel(self.ctx);
            }
        }
    }
}

impl Context {
    /// コンポーネント（エンコーダー・デコーダー）を作る。
    pub fn create_component(&self, id: &[i32]) -> Result<Component> {
        let id = wptr(id)?;
        let mut comp: *mut AMFComponent = std::ptr::null_mut();
        // SAFETY: factory・ctx は有効、id は NUL 終端の wchar_t 列。
        let r = unsafe {
            match (*(*self.amf.factory).pVtbl).CreateComponent {
                Some(cc) => cc(self.amf.factory, self.ctx, id, &mut comp),
                None => AMF_NOT_IMPLEMENTED,
            }
        };
        check(r, "AMFFactory::CreateComponent")?;
        if comp.is_null() {
            return Err(CodecError::unavailable(
                "AMFFactory::CreateComponent returned NULL",
            ));
        }
        Ok(Component {
            ptr: comp,
            initialized: false,
        })
    }

    /// ホストメモリの面を作り、フレーム（NV12 または P010）を写す。
    pub fn upload(&self, f: &VideoFrame, pts: i64) -> Result<Surface> {
        let fmt = match f.format {
            PixelFormat::Nv12 => AMF_SURFACE_NV12,
            PixelFormat::P010 => AMF_SURFACE_P010,
            PixelFormat::I420 => return Err(CodecError::internal("AMF upload needs NV12/P010")),
        };
        f.validate()?;
        let mut s: *mut AMFSurface = std::ptr::null_mut();
        // SAFETY: ctx は有効。書き込み先を渡す。
        let r = unsafe {
            match (*(*self.ctx).pVtbl).AllocSurface {
                Some(a) => a(
                    self.ctx,
                    AMF_MEMORY_HOST,
                    fmt,
                    f.width as i32,
                    f.height as i32,
                    &mut s,
                ),
                None => AMF_NOT_IMPLEMENTED,
            }
        };
        check(r, "AMFContext::AllocSurface")?;
        if s.is_null() {
            return Err(CodecError::backend("AllocSurface returned NULL"));
        }
        let surface = Surface { ptr: s };
        for (idx, pt) in [(0usize, AMF_PLANE_Y), (1usize, AMF_PLANE_UV)] {
            // SAFETY: s は有効な面。GetPlane は面が持つ平面を（参照を増やさずに）返す。
            let plane = unsafe {
                match (*(*s).pVtbl).GetPlane {
                    Some(g) => g(s, pt),
                    None => std::ptr::null_mut(),
                }
            };
            // SAFETY: plane は s が生きている間有効。
            let (ptr, pitch, rows) = unsafe { plane_info(plane)? };
            let (row, nrows) = f
                .format
                .plane_size(f.width, f.height, idx)
                .unwrap_or((0, 0));
            if row > pitch || nrows > rows {
                return Err(CodecError::backend("AMF surface plane smaller than frame"));
            }
            for y in 0..nrows {
                // SAFETY: 行 y の書き込み先は ptr + y*pitch から row バイトで、pitch・rows の範囲内（上で検査）。
                unsafe {
                    std::ptr::copy_nonoverlapping(f.row(idx, y).as_ptr(), ptr.add(y * pitch), row)
                };
            }
        }
        // SAFETY: s は有効。
        unsafe {
            if let Some(sp) = (*(*s).pVtbl).SetPts {
                sp(s, pts);
            }
        }
        Ok(surface)
    }

    /// ホストメモリのバッファを作り、`data` を写す。
    pub fn upload_buffer(&self, data: &[u8], pts: i64) -> Result<Buffer> {
        let mut b: *mut AMFBuffer = std::ptr::null_mut();
        // SAFETY: ctx は有効。
        let r = unsafe {
            match (*(*self.ctx).pVtbl).AllocBuffer {
                Some(a) => a(self.ctx, AMF_MEMORY_HOST, data.len(), &mut b),
                None => AMF_NOT_IMPLEMENTED,
            }
        };
        check(r, "AMFContext::AllocBuffer")?;
        if b.is_null() {
            return Err(CodecError::backend("AllocBuffer returned NULL"));
        }
        let buf = Buffer { ptr: b };
        // SAFETY: b は有効なホストのバッファ。GetSize 以上は書かない。
        unsafe {
            let v = (*b).pVtbl;
            let size = (*v).GetSize.map_or(0, |g| g(b));
            let dst = (*v).GetNative.map_or(std::ptr::null_mut(), |g| g(b)) as *mut u8;
            if dst.is_null() || size < data.len() {
                return Err(CodecError::backend("AMF buffer too small"));
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
            if let Some(sp) = (*v).SetPts {
                sp(b, pts);
            }
        }
        Ok(buf)
    }
}

/// # Safety
///
/// `plane` は NULL か、生きている面の有効な平面。
unsafe fn plane_info(plane: *mut AMFPlane) -> Result<(*mut u8, usize, usize)> {
    if plane.is_null() {
        return Err(CodecError::backend("AMF surface has no such plane"));
    }
    // SAFETY: 関数の前提。
    unsafe {
        let v = (*plane).pVtbl;
        let ptr = (*v).GetNative.map_or(std::ptr::null_mut(), |g| g(plane)) as *mut u8;
        let pitch = (*v).GetHPitch.map_or(0, |g| g(plane));
        let h = (*v).GetHeight.map_or(0, |g| g(plane));
        let oy = (*v).GetOffsetY.map_or(0, |g| g(plane));
        let ox = (*v).GetOffsetX.map_or(0, |g| g(plane));
        let px = (*v).GetPixelSizeInBytes.map_or(1, |g| g(plane));
        if ptr.is_null() || pitch <= 0 || h <= 0 || ox < 0 || oy < 0 || px <= 0 {
            return Err(CodecError::backend("AMF plane is not host-accessible"));
        }
        let start = oy as usize * pitch as usize + ox as usize * px as usize;
        Ok((ptr.add(start), pitch as usize, h as usize))
    }
}

/// 送った結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submit {
    Accepted,
    /// 入力がいっぱい。出力を取り出してからもう一度送る。
    InputFull,
}

/// 取り出した結果。
pub enum Output {
    /// 今は出力が無い。
    None,
    /// 出し切りが終わった。
    Eof,
    Data(Data),
}

/// 能力（`AMFCaps`）から読んだ値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapsInfo {
    pub hardware: bool,
    pub max_width: u32,
    pub max_height: u32,
    pub num_streams: Option<u32>,
}

/// AMF のコンポーネント。
pub struct Component {
    ptr: *mut AMFComponent,
    initialized: bool,
}

// SAFETY: Context と同じ（1 つのスレッドから順に使う）。
unsafe impl Send for Component {}

impl Drop for Component {
    fn drop(&mut self) {
        // SAFETY: ptr はこの値が持つ 1 つの参照。初期化済みなら Terminate してから返す。
        unsafe {
            let v = (*self.ptr).pVtbl;
            if self.initialized
                && let Some(t) = (*v).Terminate
            {
                t(self.ptr);
            }
            if let Some(rel) = (*v).Release {
                rel(self.ptr);
            }
        }
    }
}

impl Component {
    fn set(&mut self, name: &[i32], value: AMFVariantStruct) -> Result<()> {
        let n = wptr(name)?;
        // SAFETY: ptr は有効、name は NUL 終端、value は値渡しのスカラー（文字列・インターフェースは渡さない）。
        let r = unsafe {
            match (*(*self.ptr).pVtbl).SetProperty {
                Some(s) => s(self.ptr, n, value),
                None => AMF_NOT_IMPLEMENTED,
            }
        };
        check(r, &format!("SetProperty({})", wide_to_string(name)))
    }

    pub fn set_i64(&mut self, name: &[i32], v: i64) -> Result<()> {
        // SAFETY: AMFVariantStruct は POD。型と値の組を正しく入れる。
        let mut var: AMFVariantStruct = unsafe { std::mem::zeroed() };
        var.type_ = AMF_VARIANT_INT64;
        var.__bindgen_anon_1.int64Value = v;
        self.set(name, var)
    }

    pub fn set_bool(&mut self, name: &[i32], v: bool) -> Result<()> {
        // SAFETY: 同上。
        let mut var: AMFVariantStruct = unsafe { std::mem::zeroed() };
        var.type_ = AMF_VARIANT_BOOL;
        var.__bindgen_anon_1.boolValue = v as amf_bool;
        self.set(name, var)
    }

    pub fn set_size(&mut self, name: &[i32], w: u32, h: u32) -> Result<()> {
        // SAFETY: 同上。
        let mut var: AMFVariantStruct = unsafe { std::mem::zeroed() };
        var.type_ = AMF_VARIANT_SIZE;
        var.__bindgen_anon_1.sizeValue = AMFSize {
            width: w as i32,
            height: h as i32,
        };
        self.set(name, var)
    }

    pub fn set_rate(&mut self, name: &[i32], num: u32, den: u32) -> Result<()> {
        // SAFETY: 同上。
        let mut var: AMFVariantStruct = unsafe { std::mem::zeroed() };
        var.type_ = AMF_VARIANT_RATE;
        var.__bindgen_anon_1.rateValue = AMFRate { num, den };
        self.set(name, var)
    }

    pub fn init(&mut self, format: PixelFormat, w: u32, h: u32) -> Result<()> {
        let fmt = match format {
            PixelFormat::P010 => AMF_SURFACE_P010,
            _ => AMF_SURFACE_NV12,
        };
        // SAFETY: ptr は有効。
        let r = unsafe {
            match (*(*self.ptr).pVtbl).Init {
                Some(i) => i(self.ptr, fmt, w as i32, h as i32),
                None => AMF_NOT_IMPLEMENTED,
            }
        };
        check(r, "AMFComponent::Init")?;
        self.initialized = true;
        Ok(())
    }

    fn submit(&mut self, data: *mut AMFData) -> Result<Submit> {
        // SAFETY: ptr は有効。data は生きている面またはバッファ（AMFData の派生で、vtbl の先頭が同じ）。
        // コンポーネントは必要なら自分で参照を増やす。
        let r = unsafe {
            match (*(*self.ptr).pVtbl).SubmitInput {
                Some(s) => s(self.ptr, data),
                None => AMF_NOT_IMPLEMENTED,
            }
        };
        match r {
            AMF_INPUT_FULL | AMF_DECODER_NO_FREE_SURFACES => Ok(Submit::InputFull),
            // 解像度の変化は受理として扱う（以後の出力が新しい大きさになる）。
            AMF_RESOLUTION_CHANGED | AMF_RESOLUTION_UPDATED | AMF_NEED_MORE_INPUT => {
                Ok(Submit::Accepted)
            }
            _ => check(r, "AMFComponent::SubmitInput").map(|_| Submit::Accepted),
        }
    }

    pub fn submit_surface(&mut self, s: &Surface) -> Result<Submit> {
        self.submit(s.ptr as *mut AMFData)
    }

    pub fn submit_buffer(&mut self, b: &Buffer) -> Result<Submit> {
        self.submit(b.ptr as *mut AMFData)
    }

    /// 出力を 1 つ取り出す（待たない）。
    pub fn query_output(&mut self) -> Result<Output> {
        let mut d: *mut AMFData = std::ptr::null_mut();
        // SAFETY: ptr は有効。出力があれば参照 1 つ付きで d に入る（Data の Drop で返す）。
        let r = unsafe {
            match (*(*self.ptr).pVtbl).QueryOutput {
                Some(q) => q(self.ptr, &mut d),
                None => AMF_NOT_IMPLEMENTED,
            }
        };
        let data = (!d.is_null()).then_some(Data { ptr: d });
        match r {
            AMF_OK | AMF_REPEAT | AMF_NEED_MORE_INPUT => {
                Ok(data.map_or(Output::None, Output::Data))
            }
            AMF_EOF => Ok(data.map_or(Output::Eof, Output::Data)),
            _ => {
                drop(data);
                check(r, "AMFComponent::QueryOutput").map(|_| Output::None)
            }
        }
    }

    pub fn drain(&mut self) -> Result<()> {
        // SAFETY: ptr は有効。
        let r = unsafe {
            match (*(*self.ptr).pVtbl).Drain {
                Some(d) => d(self.ptr),
                None => AMF_NOT_IMPLEMENTED,
            }
        };
        if r == AMF_INPUT_FULL {
            return Err(CodecError::backend("AMFComponent::Drain: input full"));
        }
        check(r, "AMFComponent::Drain")
    }

    /// 能力を読む。`num_streams_prop` は同時ストリーム数のプロパティ名（コーデックごとに違う）。
    pub fn caps(&self, num_streams_prop: Option<&[i32]>) -> Result<CapsInfo> {
        let mut caps: *mut AMFCaps = std::ptr::null_mut();
        // SAFETY: ptr は有効。caps は参照 1 つ付きで返る（下で Release）。
        let r = unsafe {
            match (*(*self.ptr).pVtbl).GetCaps {
                Some(g) => g(self.ptr, &mut caps),
                None => AMF_NOT_IMPLEMENTED,
            }
        };
        check(r, "AMFComponent::GetCaps")?;
        if caps.is_null() {
            return Err(CodecError::backend("GetCaps returned NULL"));
        }
        // SAFETY: caps は有効。入力の能力（io）も参照付きで返るので Release する。
        unsafe {
            let cv = (*caps).pVtbl;
            let hardware = (*cv)
                .GetAccelerationType
                .is_some_and(|g| g(caps) == AMF_ACCEL_HARDWARE);
            let (mut max_w, mut max_h) = (0i32, 0i32);
            let mut io: *mut AMFIOCaps = std::ptr::null_mut();
            if let Some(gi) = (*cv).GetInputCaps
                && gi(caps, &mut io) == AMF_OK
                && !io.is_null()
            {
                let iv = (*io).pVtbl;
                let mut min = 0i32;
                if let Some(w) = (*iv).GetWidthRange {
                    w(io, &mut min, &mut max_w);
                }
                if let Some(h) = (*iv).GetHeightRange {
                    h(io, &mut min, &mut max_h);
                }
                if let Some(rel) = (*iv).Release {
                    rel(io);
                }
            }
            let mut num_streams = None;
            if let Some(name) = num_streams_prop
                && let Ok(n) = wptr(name)
                && let Some(gp) = (*cv).GetProperty
            {
                let mut var: AMFVariantStruct = std::mem::zeroed();
                if gp(caps, n, &mut var) == AMF_OK {
                    if var.type_ == AMF_VARIANT_INT64 {
                        num_streams = u32::try_from(var.__bindgen_anon_1.int64Value).ok();
                    } else {
                        clear_variant(&mut var);
                    }
                }
            }
            if let Some(rel) = (*cv).Release {
                rel(caps);
            }
            Ok(CapsInfo {
                hardware,
                max_width: max_w.max(0) as u32,
                max_height: max_h.max(0) as u32,
                num_streams,
            })
        }
    }
}

/// 想定外の型で返ったプロパティの後始末（インターフェースなら参照を返す）。
///
/// # Safety
///
/// `var` は GetProperty が埋めた値。
unsafe fn clear_variant(var: &mut AMFVariantStruct) {
    if var.type_ == AMF_VARIANT_INTERFACE {
        // SAFETY: インターフェース型なら pInterface は参照 1 つ付きの有効なポインター（または NULL）。
        unsafe {
            let p = var.__bindgen_anon_1.pInterface;
            if !p.is_null()
                && let Some(rel) = (*(*p).pVtbl).Release
            {
                rel(p);
            }
        }
    }
    // 文字列型はランタイムの割り当て関数で確保されており、ここからは安全に解放できない（読まないプロパティなので起きない）。
    var.type_ = AMF_VARIANT_EMPTY;
}

/// ホストメモリの面（入力用）。
pub struct Surface {
    ptr: *mut AMFSurface,
}

// SAFETY: Context と同じ。
unsafe impl Send for Surface {}

impl Surface {
    pub fn set_i64(&mut self, name: &[i32], v: i64) -> Result<()> {
        let n = wptr(name)?;
        // SAFETY: AMFVariantStruct は POD。ptr は有効、name は NUL 終端。
        let r = unsafe {
            let mut var: AMFVariantStruct = std::mem::zeroed();
            var.type_ = AMF_VARIANT_INT64;
            var.__bindgen_anon_1.int64Value = v;
            match (*(*self.ptr).pVtbl).SetProperty {
                Some(s) => s(self.ptr, n, var),
                None => AMF_NOT_IMPLEMENTED,
            }
        };
        check(r, "AMFSurface::SetProperty")
    }

    pub fn set_bool(&mut self, name: &[i32], v: bool) -> Result<()> {
        let n = wptr(name)?;
        // SAFETY: 同上。
        let r = unsafe {
            let mut var: AMFVariantStruct = std::mem::zeroed();
            var.type_ = AMF_VARIANT_BOOL;
            var.__bindgen_anon_1.boolValue = v as amf_bool;
            match (*(*self.ptr).pVtbl).SetProperty {
                Some(s) => s(self.ptr, n, var),
                None => AMF_NOT_IMPLEMENTED,
            }
        };
        check(r, "AMFSurface::SetProperty")
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: この値が持つ 1 つの参照を返す。
        unsafe {
            if let Some(rel) = (*(*self.ptr).pVtbl).Release {
                rel(self.ptr);
            }
        }
    }
}

/// ホストメモリのバッファ（入力用）。
pub struct Buffer {
    ptr: *mut AMFBuffer,
}

// SAFETY: Context と同じ。
unsafe impl Send for Buffer {}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: この値が持つ 1 つの参照を返す。
        unsafe {
            if let Some(rel) = (*(*self.ptr).pVtbl).Release {
                rel(self.ptr);
            }
        }
    }
}

/// コンポーネントの出力。
pub struct Data {
    ptr: *mut AMFData,
}

impl Drop for Data {
    fn drop(&mut self) {
        // SAFETY: QueryOutput が付けた参照を 1 回だけ返す。
        unsafe {
            if let Some(rel) = (*(*self.ptr).pVtbl).Release {
                rel(self.ptr);
            }
        }
    }
}

/// 復号した 1 フレーム（詰めた NV12 / P010）。
#[derive(Debug, Clone)]
pub struct DecodedSurface {
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub pts: i64,
}

/// 出力 1 つの大きさの上限。
const MAX_OUTPUT: usize = 256 << 20;

impl Data {
    pub fn pts(&self) -> i64 {
        // SAFETY: ptr は有効。
        unsafe { (*(*self.ptr).pVtbl).GetPts.map_or(0, |g| g(self.ptr)) }
    }

    pub fn get_i64(&self, name: &[i32]) -> Option<i64> {
        let n = wptr(name).ok()?;
        // SAFETY: ptr は有効、name は NUL 終端。返った値は型を見てから読む。
        unsafe {
            let gp = (*(*self.ptr).pVtbl).GetProperty?;
            let mut var: AMFVariantStruct = std::mem::zeroed();
            if gp(self.ptr, n, &mut var) != AMF_OK {
                return None;
            }
            if var.type_ == AMF_VARIANT_INT64 {
                Some(var.__bindgen_anon_1.int64Value)
            } else {
                clear_variant(&mut var);
                None
            }
        }
    }

    /// バッファとして中身を写す（エンコーダーの出力）。
    pub fn buffer_bytes(&self) -> Result<Vec<u8>> {
        let mut p: *mut c_void = std::ptr::null_mut();
        // SAFETY: ptr は有効。QueryInterface は参照 1 つ付きで AMFBuffer を返す（下で Release）。
        unsafe {
            let r = match (*(*self.ptr).pVtbl).QueryInterface {
                Some(q) => q(self.ptr, &IID_AMF_BUFFER, &mut p),
                None => AMF_NOT_IMPLEMENTED,
            };
            check(r, "AMFData::QueryInterface(AMFBuffer)")?;
            let b = p as *mut AMFBuffer;
            if b.is_null() {
                return Err(CodecError::backend("AMFBuffer is NULL"));
            }
            let _guard = Buffer { ptr: b };
            let v = (*b).pVtbl;
            let size = (*v).GetSize.map_or(0, |g| g(b));
            let src = (*v).GetNative.map_or(std::ptr::null_mut(), |g| g(b)) as *const u8;
            if size > MAX_OUTPUT || (src.is_null() && size > 0) {
                return Err(CodecError::backend("AMF output buffer invalid"));
            }
            if size == 0 {
                return Ok(Vec::new());
            }
            Ok(std::slice::from_raw_parts(src, size).to_vec())
        }
    }

    /// 面として中身を写す（デコーダーの出力）。ホストメモリに変換してから読む。
    pub fn surface(&self, max_w: u32, max_h: u32) -> Result<DecodedSurface> {
        let mut p: *mut c_void = std::ptr::null_mut();
        // SAFETY: ptr は有効。QueryInterface は参照 1 つ付きで AMFSurface を返す（Surface の Drop で返す）。
        unsafe {
            let r = match (*(*self.ptr).pVtbl).QueryInterface {
                Some(q) => q(self.ptr, &IID_AMF_SURFACE, &mut p),
                None => AMF_NOT_IMPLEMENTED,
            };
            check(r, "AMFData::QueryInterface(AMFSurface)")?;
            let s = p as *mut AMFSurface;
            if s.is_null() {
                return Err(CodecError::backend("AMFSurface is NULL"));
            }
            let guard = Surface { ptr: s };
            let v = (*s).pVtbl;
            if let Some(c) = (*v).Convert {
                check(c(s, AMF_MEMORY_HOST), "AMFSurface::Convert(HOST)")?;
            }
            let (format, bpp) = match (*v).GetFormat.map_or(AMF_SURFACE_UNKNOWN, |g| g(s)) {
                AMF_SURFACE_NV12 => (PixelFormat::Nv12, 1usize),
                AMF_SURFACE_P010 => (PixelFormat::P010, 2usize),
                other => {
                    return Err(CodecError::unsupported(format!(
                        "AMF output surface format {other}"
                    )));
                }
            };
            let y = (*v)
                .GetPlane
                .map_or(std::ptr::null_mut(), |g| g(s, AMF_PLANE_Y));
            let uv = (*v)
                .GetPlane
                .map_or(std::ptr::null_mut(), |g| g(s, AMF_PLANE_UV));
            let (yp, ypitch, yrows) = plane_info(y)?;
            let (uvp, uvpitch, uvrows) = plane_info(uv)?;
            let w = (*(*y).pVtbl).GetWidth.map_or(0, |g| g(y));
            let h = (*(*y).pVtbl).GetHeight.map_or(0, |g| g(y));
            if w <= 0 || h <= 0 || w as u32 > max_w || h as u32 > max_h {
                return Err(CodecError::invalid_input(format!(
                    "decoded surface {w}x{h} outside allowed range"
                )));
            }
            let (w, h) = (w as u32, h as u32);
            let row_y = w as usize * bpp;
            let row_uv = (w as usize).div_ceil(2) * 2 * bpp;
            let rows_uv = (h as usize).div_ceil(2);
            if row_y > ypitch || row_uv > uvpitch || h as usize > yrows || rows_uv > uvrows {
                return Err(CodecError::backend("AMF plane layout smaller than surface"));
            }
            let mut data = Vec::with_capacity(format.packed_size(w, h));
            for r in 0..h as usize {
                data.extend_from_slice(std::slice::from_raw_parts(yp.add(r * ypitch), row_y));
            }
            for r in 0..rows_uv {
                data.extend_from_slice(std::slice::from_raw_parts(uvp.add(r * uvpitch), row_uv));
            }
            let pts = (*v).GetPts.map_or(0, |g| g(s));
            drop(guard);
            Ok(DecodedSurface {
                format,
                width: w,
                height: h,
                data,
                pts,
            })
        }
    }
}

/// wchar_t 列を表示用の文字列にする（エラー文用）。
pub fn wide_to_string(w: &[i32]) -> String {
    w.iter()
        .take_while(|&&c| c != 0)
        .filter_map(|&c| char::from_u32(c as u32))
        .collect()
}
