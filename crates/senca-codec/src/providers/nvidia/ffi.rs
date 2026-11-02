//! NVIDIA の C API（CUDA ドライバー API、NVENC、NVDEC/cuvid、NVML）の呼び出し。
//!
//! このプロバイダーで `unsafe` を使うのはこのモジュール（と、ここから読み込む生成済みの型定義 `sys`）だけ。
//! 外には安全な型（[`NvLibs`]・[`NvencSession`]・[`CuvidSession`]）だけを見せる。
//!
//! # 前提と境界
//!
//! - ライブラリは [`crate::dl`] で読み込み、`Arc<Library>` を [`NvLibs`] が持つ。セッションは `Arc<NvLibs>` を
//!   持つので、関数ポインターを使う間ライブラリは解放されない。
//! - 関数の型は bindgen がヘッダーの typedef から生成したもの（`tcuInit` など）をそのまま使い、
//!   ヘッダーとの食い違いを避ける。ヘッダーに typedef の無い入口（`NvEncodeAPICreateInstance` など）だけ手で書く。
//! - CUDA の context はセッションごとに作り、呼び出しの間だけ現在のスレッドに push する（[`CtxGuard`]）。
//!   そのため、セッションは別のスレッドへ移してよい（`Send`）が、同時に 2 つのスレッドから使わない（`&mut self`）。
//! - cuvid のコールバックは `cuvidParseVideoData` の中から同じスレッドで同期的に呼ばれるだけ（再入しない）。
//!
//! # 検証の状況
//!
//! 開発機には NVIDIA の GPU・ドライバーが無い。ここで確かめているのは「ライブラリが無い・壊れている」ときの
//! 振る舞いと、型の大きさ（bindgen の配置検査）だけで、GPU 上での符号化・復号は**未検証**。

#![allow(clippy::missing_safety_doc)]

#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    unused_imports,
    unnecessary_transmutes,
    unsafe_op_in_unsafe_fn,
    clippy::all,
    clippy::pedantic
)]
#[path = "bindings/nv.rs"]
mod sys;

use crate::dl::{Library, LibrarySearch};
use crate::error::{CodecError, ErrorKind, Result};
use crate::frame::{PixelFormat, VideoFrame};
use senca_core::Codec;
use std::collections::VecDeque;
use std::ffi::{c_char, c_int, c_uint, c_void};
use std::sync::Arc;

// ---------------------------------------------------------------------------------------------
// GUID（nvEncodeAPI.h の `static const GUID` の値を写したもの）
// ---------------------------------------------------------------------------------------------

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> sys::GUID {
    sys::GUID {
        Data1: d1,
        Data2: d2,
        Data3: d3,
        Data4: d4,
    }
}

pub(super) const CODEC_H264: sys::GUID = guid(
    0x6bc82762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
pub(super) const CODEC_HEVC: sys::GUID = guid(
    0x790cdc88,
    0x4522,
    0x4d7b,
    [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
);
pub(super) const CODEC_AV1: sys::GUID = guid(
    0x0a352289,
    0x0aa7,
    0x4759,
    [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54],
);
const PROFILE_AUTOSELECT: sys::GUID = guid(
    0xbfd6f8e7,
    0x233c,
    0x4341,
    [0x8b, 0x3e, 0x48, 0x18, 0x52, 0x38, 0x03, 0xf4],
);
const H264_PROFILE_BASELINE: sys::GUID = guid(
    0x0727bcaa,
    0x78c4,
    0x4c83,
    [0x8c, 0x2f, 0xef, 0x3d, 0xff, 0x26, 0x7c, 0x6a],
);
const H264_PROFILE_MAIN: sys::GUID = guid(
    0x60b5c1d4,
    0x67fe,
    0x4790,
    [0x94, 0xd5, 0xc4, 0x72, 0x6d, 0x7b, 0x6e, 0x6d],
);
const H264_PROFILE_HIGH: sys::GUID = guid(
    0xe7cbc309,
    0x4f7a,
    0x4b89,
    [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
);
const HEVC_PROFILE_MAIN: sys::GUID = guid(
    0xb514c39a,
    0xb55b,
    0x40fa,
    [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec],
);
const HEVC_PROFILE_MAIN10: sys::GUID = guid(
    0xfa4d2b6c,
    0x3a5b,
    0x411a,
    [0x80, 0x18, 0x0a, 0x3f, 0x5e, 0x3c, 0x9b, 0xe5],
);
const AV1_PROFILE_MAIN: sys::GUID = guid(
    0x5f2a39f5,
    0xf14e,
    0x4f95,
    [0x9a, 0x9e, 0xb7, 0x6d, 0x56, 0x8f, 0xcf, 0x97],
);
const PRESET_P1: sys::GUID = guid(
    0xfc0a8d3e,
    0x45f8,
    0x4cf8,
    [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
);
const PRESET_P4: sys::GUID = guid(
    0x90a7b826,
    0xdf06,
    0x4862,
    [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
);

fn guid_eq(a: &sys::GUID, b: &sys::GUID) -> bool {
    a.Data1 == b.Data1 && a.Data2 == b.Data2 && a.Data3 == b.Data3 && a.Data4 == b.Data4
}

/// 構造体の版（`NVENCAPI_STRUCT_VERSION(ver)`）。
const fn struct_ver(v: u32) -> u32 {
    sys::NVENCAPI_VERSION | (v << 16) | (0x7 << 28)
}

const NV_ENC_CAPS_PARAM_VER: u32 = struct_ver(1);
const NV_ENC_CREATE_INPUT_BUFFER_VER: u32 = struct_ver(2);
const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_ver(1);
const NV_ENC_RC_PARAMS_VER: u32 = struct_ver(1);
const NV_ENC_CONFIG_VER: u32 = struct_ver(9) | (1 << 31);
const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_ver(7) | (1 << 31);
const NV_ENC_PRESET_CONFIG_VER: u32 = struct_ver(5) | (1 << 31);
const NV_ENC_PIC_PARAMS_VER: u32 = struct_ver(7) | (1 << 31);
const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_ver(2) | (1 << 31);
const NV_ENC_LOCK_INPUT_BUFFER_VER: u32 = struct_ver(1);
const NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER: u32 = struct_ver(1);
const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_ver(1);
const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_ver(2);

/// `NvEncodeAPIGetMaxSupportedVersion` が返す形（上位 = major、下位 4 bit = minor）の、ヘッダーの版。
pub(super) const HEADER_API_VERSION: u32 =
    (sys::NVENCAPI_MAJOR_VERSION << 4) | sys::NVENCAPI_MINOR_VERSION;

// ---------------------------------------------------------------------------------------------
// ライブラリの読み込み
// ---------------------------------------------------------------------------------------------

/// `Option<fn>` の関数を呼ぶ。読み込み時にすべて `Some` を確かめているが、念のため `None` なら失敗の値を返す。
macro_rules! cu {
    ($f:expr $(, $a:expr)* $(,)?) => {
        match $f {
            Some(f) => f($($a),*),
            None => sys::CUDA_ERROR_UNKNOWN,
        }
    };
}
macro_rules! nv {
    ($f:expr $(, $a:expr)* $(,)?) => {
        match $f {
            Some(f) => f($($a),*),
            None => sys::NV_ENC_ERR_UNIMPLEMENTED,
        }
    };
}

struct Cuda {
    _lib: Arc<Library>,
    init: sys::tcuInit,
    driver_get_version: sys::tcuDriverGetVersion,
    device_get_count: sys::tcuDeviceGetCount,
    device_get: sys::tcuDeviceGet,
    device_get_name: sys::tcuDeviceGetName,
    device_get_attribute: sys::tcuDeviceGetAttribute,
    ctx_create: sys::tcuCtxCreate_v2,
    ctx_push: sys::tcuCtxPushCurrent_v2,
    ctx_pop: sys::tcuCtxPopCurrent_v2,
    ctx_destroy: sys::tcuCtxDestroy_v2,
    memcpy_2d: sys::tcuMemcpy2D_v2,
    get_error_name: sys::tcuGetErrorName,
}

type FnNvEncodeApiCreateInstance =
    unsafe extern "C" fn(function_list: *mut sys::NV_ENCODE_API_FUNCTION_LIST) -> sys::NVENCSTATUS;
type FnNvEncodeApiGetMaxSupportedVersion =
    unsafe extern "C" fn(version: *mut u32) -> sys::NVENCSTATUS;

struct Nvenc {
    _lib: Arc<Library>,
    /// ドライバーが埋めた関数表（関数ポインターだけ）。
    fl: Box<sys::NV_ENCODE_API_FUNCTION_LIST>,
}

struct Cuvid {
    _lib: Arc<Library>,
    get_decoder_caps: sys::tcuvidGetDecoderCaps,
    create_decoder: sys::tcuvidCreateDecoder,
    destroy_decoder: sys::tcuvidDestroyDecoder,
    decode_picture: sys::tcuvidDecodePicture,
    map_video_frame: sys::tcuvidMapVideoFrame64,
    unmap_video_frame: sys::tcuvidUnmapVideoFrame64,
    ctx_lock_create: sys::tcuvidCtxLockCreate,
    ctx_lock_destroy: sys::tcuvidCtxLockDestroy,
    create_video_parser: sys::tcuvidCreateVideoParser,
    parse_video_data: sys::tcuvidParseVideoData,
    destroy_video_parser: sys::tcuvidDestroyVideoParser,
}

/// 読み込んだ NVIDIA のライブラリ群。
pub(super) struct NvLibs {
    cu: Cuda,
    enc: std::result::Result<Nvenc, String>,
    dec: std::result::Result<Cuvid, String>,
    /// ドライバーの版（NVML で分かれば `"570.86.15"` など）と CUDA ドライバー API の版。
    pub(super) driver_version: String,
}

// SAFETY: NvLibs が持つのは、読み込んだライブラリへの Arc、関数ポインター、ドライバーが埋めた関数表
// （関数ポインターと NULL の予約領域）だけで、可変の共有状態は無い。CUDA ドライバー API・NVENC・cuvid の
// 関数はスレッド安全（同じセッション・context を同時に使わないことは、各セッションの `&mut self` で保証する）。
unsafe impl Send for NvLibs {}
// SAFETY: 上と同じ。共有して読むのは関数ポインターだけ。
unsafe impl Sync for NvLibs {}

fn load_cuda(search: &LibrarySearch) -> std::result::Result<Cuda, String> {
    let lib = search.open(&["libcuda.so.1"])?;
    // SAFETY: 各記号の型は、nv-codec-headers の dynlink_cuda.h の typedef（`tcuXxx`）から bindgen で生成した
    // `Option<unsafe extern "C" fn>` で、`_v2` の付く実体名（dynlink_loader.h と同じ）を引く。`Option<fn>` は
    // ヌル許容の関数ポインターと同じ表現。ライブラリの Arc は Cuda が持つ。
    unsafe {
        Ok(Cuda {
            init: lib.sym("cuInit")?,
            driver_get_version: lib.sym("cuDriverGetVersion")?,
            device_get_count: lib.sym("cuDeviceGetCount")?,
            device_get: lib.sym("cuDeviceGet")?,
            device_get_name: lib.sym("cuDeviceGetName")?,
            device_get_attribute: lib.sym("cuDeviceGetAttribute")?,
            ctx_create: lib.sym("cuCtxCreate_v2")?,
            ctx_push: lib.sym("cuCtxPushCurrent_v2")?,
            ctx_pop: lib.sym("cuCtxPopCurrent_v2")?,
            ctx_destroy: lib.sym("cuCtxDestroy_v2")?,
            memcpy_2d: lib.sym("cuMemcpy2D_v2")?,
            get_error_name: lib.sym("cuGetErrorName").unwrap_or(None),
            _lib: lib,
        })
    }
}

fn load_nvenc(search: &LibrarySearch) -> std::result::Result<Nvenc, String> {
    let lib = search.open(&["libnvidia-encode.so.1"])?;
    // SAFETY: 2 つの入口の型は nvEncodeAPI.h の宣言
    // `NVENCSTATUS NvEncodeAPIGetMaxSupportedVersion(uint32_t*)`、
    // `NVENCSTATUS NvEncodeAPICreateInstance(NV_ENCODE_API_FUNCTION_LIST*)` と一致する。
    let (get_max, create): (
        FnNvEncodeApiGetMaxSupportedVersion,
        FnNvEncodeApiCreateInstance,
    ) = unsafe {
        (
            lib.sym("NvEncodeAPIGetMaxSupportedVersion")?,
            lib.sym("NvEncodeAPICreateInstance")?,
        )
    };
    let mut max_version = 0u32;
    // SAFETY: 出力先は有効な u32。
    let st = unsafe { get_max(&mut max_version) };
    if st != sys::NV_ENC_SUCCESS {
        return Err(format!(
            "NvEncodeAPIGetMaxSupportedVersion failed ({})",
            nvenc_status_name(st)
        ));
    }
    if max_version < HEADER_API_VERSION {
        return Err(format!(
            "driver supports NVENC API {}.{}, {}.{} required (driver 610 or newer)",
            max_version >> 4,
            max_version & 0xf,
            sys::NVENCAPI_MAJOR_VERSION,
            sys::NVENCAPI_MINOR_VERSION
        ));
    }
    let mut fl: Box<sys::NV_ENCODE_API_FUNCTION_LIST> = Box::default();
    fl.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    // SAFETY: fl は 0 で埋めた、版を設定した関数表。ドライバーが関数ポインターを書き込む。
    let st = unsafe { create(&mut *fl) };
    if st != sys::NV_ENC_SUCCESS {
        return Err(format!(
            "NvEncodeAPICreateInstance failed ({})",
            nvenc_status_name(st)
        ));
    }
    let required = [
        fl.nvEncOpenEncodeSessionEx.is_some(),
        fl.nvEncGetEncodeGUIDCount.is_some(),
        fl.nvEncGetEncodeGUIDs.is_some(),
        fl.nvEncGetEncodeCaps.is_some(),
        fl.nvEncGetEncodePresetConfigEx.is_some(),
        fl.nvEncInitializeEncoder.is_some(),
        fl.nvEncCreateInputBuffer.is_some(),
        fl.nvEncDestroyInputBuffer.is_some(),
        fl.nvEncCreateBitstreamBuffer.is_some(),
        fl.nvEncDestroyBitstreamBuffer.is_some(),
        fl.nvEncEncodePicture.is_some(),
        fl.nvEncLockBitstream.is_some(),
        fl.nvEncUnlockBitstream.is_some(),
        fl.nvEncLockInputBuffer.is_some(),
        fl.nvEncUnlockInputBuffer.is_some(),
        fl.nvEncGetSequenceParams.is_some(),
        fl.nvEncDestroyEncoder.is_some(),
    ];
    if required.iter().any(|ok| !ok) {
        return Err("NVENC function list is incomplete".into());
    }
    Ok(Nvenc { _lib: lib, fl })
}

fn load_cuvid(search: &LibrarySearch) -> std::result::Result<Cuvid, String> {
    let lib = search.open(&["libnvcuvid.so.1"])?;
    // SAFETY: 各記号の型は dynlink_cuviddec.h / dynlink_nvcuvid.h の typedef から bindgen で生成したもの。
    // 64 bit では Map/Unmap は `...64` の実体を引く（dynlink_loader.h と同じ）。
    unsafe {
        Ok(Cuvid {
            get_decoder_caps: lib.sym("cuvidGetDecoderCaps")?,
            create_decoder: lib.sym("cuvidCreateDecoder")?,
            destroy_decoder: lib.sym("cuvidDestroyDecoder")?,
            decode_picture: lib.sym("cuvidDecodePicture")?,
            map_video_frame: lib.sym("cuvidMapVideoFrame64")?,
            unmap_video_frame: lib.sym("cuvidUnmapVideoFrame64")?,
            ctx_lock_create: lib.sym("cuvidCtxLockCreate")?,
            ctx_lock_destroy: lib.sym("cuvidCtxLockDestroy")?,
            create_video_parser: lib.sym("cuvidCreateVideoParser")?,
            parse_video_data: lib.sym("cuvidParseVideoData")?,
            destroy_video_parser: lib.sym("cuvidDestroyVideoParser")?,
            _lib: lib,
        })
    }
}

type FnNvmlInit = unsafe extern "C" fn() -> c_int;
type FnNvmlGetDriverVersion = unsafe extern "C" fn(version: *mut c_char, length: c_uint) -> c_int;

/// NVML でドライバーの版を得る（無ければ `None`）。
fn nvml_driver_version(search: &LibrarySearch) -> Option<String> {
    let lib = search.open(&["libnvidia-ml.so.1"]).ok()?;
    // SAFETY: dynlink_nvml.h の `tnvmlInit_v2`・`tnvmlSystemGetDriverVersion`・`tnvmlShutdown` と同じ型。
    let (init, get, shutdown): (FnNvmlInit, FnNvmlGetDriverVersion, FnNvmlInit) = unsafe {
        (
            lib.sym("nvmlInit_v2").ok()?,
            lib.sym("nvmlSystemGetDriverVersion").ok()?,
            lib.sym("nvmlShutdown").ok()?,
        )
    };
    // SAFETY: 引数なし。0 = NVML_SUCCESS。
    if unsafe { init() } != 0 {
        return None;
    }
    let mut buf = [0u8; 96];
    // SAFETY: buf の長さを渡し、NVML は NUL 終端で書く。
    let st = unsafe { get(buf.as_mut_ptr().cast(), buf.len() as c_uint) };
    // SAFETY: init が成功したので対になる shutdown を呼ぶ。
    unsafe { shutdown() };
    (st == 0).then(|| super::super::common::c_chars_to_string(&buf))
}

impl NvLibs {
    fn cu_check(&self, r: sys::CUresult, what: &str) -> Result<()> {
        if r == sys::CUDA_SUCCESS {
            return Ok(());
        }
        let mut name: *const c_char = std::ptr::null();
        // SAFETY: cuGetErrorName は静的な文字列へのポインターを書く（または失敗）。
        let ok = unsafe { cu!(self.cu.get_error_name, r, &mut name) } == sys::CUDA_SUCCESS;
        let n = if ok && !name.is_null() {
            // SAFETY: CUDA が返す NUL 終端の静的文字列。
            unsafe { std::ffi::CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned()
        } else {
            format!("CUDA error {r}")
        };
        let kind = if r == sys::CUDA_ERROR_INVALID_VALUE {
            ErrorKind::Backend
        } else {
            cuda_error_kind(r)
        };
        Err(CodecError::new(kind, format!("{what}: {n}")))
    }
}

/// CUDA のエラーの種類（メモリ不足・装置なしは「枠が無い」）。
fn cuda_error_kind(r: sys::CUresult) -> ErrorKind {
    // 2 = CUDA_ERROR_OUT_OF_MEMORY、100 = CUDA_ERROR_NO_DEVICE、101 = CUDA_ERROR_INVALID_DEVICE（cuda.h の値）。
    match r {
        2 | 100 | 101 => ErrorKind::Unavailable,
        _ => ErrorKind::Backend,
    }
}

// ---------------------------------------------------------------------------------------------
// 検出
// ---------------------------------------------------------------------------------------------

/// 符号化の能力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EncodeCap {
    pub codec: Codec,
    pub max_width: u32,
    pub max_height: u32,
    pub ten_bit: bool,
}

/// 復号の能力（ビット深度ごと）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DecodeCap {
    pub codec: Codec,
    pub bit_depth: u8,
    pub max_width: u32,
    pub max_height: u32,
}

#[derive(Debug, Clone)]
pub(super) struct ProbedDevice {
    pub index: u32,
    pub name: String,
    pub compute_capability: (i32, i32),
    pub encode: Vec<EncodeCap>,
    pub decode: Vec<DecodeCap>,
    /// 符号化・復号を調べられなかった理由。
    pub notes: Vec<String>,
}

pub(super) struct Probe {
    pub libs: Arc<NvLibs>,
    pub devices: Vec<ProbedDevice>,
}

/// ライブラリを読み込み、GPU ごとに能力を調べる。ライブラリ・装置が無ければ理由付きの `Err`（落ちない）。
pub(super) fn probe(search: &LibrarySearch) -> std::result::Result<Probe, String> {
    let cu = load_cuda(search)?;
    // SAFETY: cuInit(0) はプロセスで何度呼んでもよい。
    let r = unsafe { cu!(cu.init, 0) };
    if r != sys::CUDA_SUCCESS {
        return Err(format!(
            "cuInit failed (CUDA error {r}; no NVIDIA GPU or driver not loaded)"
        ));
    }
    let mut ver = 0;
    // SAFETY: 出力先は有効な int。
    unsafe { cu!(cu.driver_get_version, &mut ver) };
    let cuda_ver = format!("CUDA {}.{}", ver / 1000, (ver % 1000) / 10);
    let driver_version = match nvml_driver_version(search) {
        Some(d) => format!("{d} ({cuda_ver})"),
        None => cuda_ver,
    };
    let libs = Arc::new(NvLibs {
        cu,
        enc: load_nvenc(search),
        dec: load_cuvid(search),
        driver_version,
    });
    let mut count = 0;
    // SAFETY: 出力先は有効な int。
    libs.cu_check(
        unsafe { cu!(libs.cu.device_get_count, &mut count) },
        "cuDeviceGetCount",
    )
    .map_err(|e| e.message)?;
    let mut devices = Vec::new();
    for i in 0..count.clamp(0, 64) {
        devices.push(probe_device(&libs, i as u32));
    }
    Ok(Probe { libs, devices })
}

fn probe_device(libs: &Arc<NvLibs>, index: u32) -> ProbedDevice {
    let mut d = ProbedDevice {
        index,
        name: format!("NVIDIA GPU {index}"),
        compute_capability: (0, 0),
        encode: Vec::new(),
        decode: Vec::new(),
        notes: Vec::new(),
    };
    let mut dev: sys::CUdevice = 0;
    // SAFETY: 出力先は有効。index は cuDeviceGetCount の範囲内。
    if let Err(e) = libs.cu_check(
        unsafe { cu!(libs.cu.device_get, &mut dev, index as c_int) },
        "cuDeviceGet",
    ) {
        d.notes.push(e.message);
        return d;
    }
    let mut name = [0u8; 256];
    // SAFETY: 長さを渡し、CUDA は NUL 終端で書く。
    if unsafe {
        cu!(
            libs.cu.device_get_name,
            name.as_mut_ptr().cast(),
            name.len() as c_int,
            dev
        )
    } == sys::CUDA_SUCCESS
    {
        d.name = super::super::common::c_chars_to_string(&name);
    }
    let (mut major, mut minor) = (0, 0);
    // SAFETY: 出力先は有効な int。
    unsafe {
        cu!(
            libs.cu.device_get_attribute,
            &mut major,
            sys::CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR,
            dev
        );
        cu!(
            libs.cu.device_get_attribute,
            &mut minor,
            sys::CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR,
            dev
        );
    }
    d.compute_capability = (major, minor);
    let ctx = match Ctx::new(libs.clone(), index) {
        Ok(c) => c,
        Err(e) => {
            d.notes.push(e.message);
            return d;
        }
    };
    let guard = match ctx.push() {
        Ok(g) => g,
        Err(e) => {
            d.notes.push(e.message);
            return d;
        }
    };
    match &libs.enc {
        Ok(enc) => match probe_encode(libs, enc, &ctx) {
            Ok(caps) => d.encode = caps,
            Err(e) => d.notes.push(format!("NVENC: {}", e.message)),
        },
        Err(e) => d.notes.push(format!("NVENC: {e}")),
    }
    match &libs.dec {
        Ok(dec) => d.decode = probe_decode(dec),
        Err(e) => d.notes.push(format!("NVDEC: {e}")),
    }
    drop(guard);
    d
}

/// 符号化セッション（`nvEncOpenEncodeSessionEx` の結果）。Drop で `nvEncDestroyEncoder`。
struct EncHandle<'a> {
    enc: &'a Nvenc,
    ptr: *mut c_void,
}

impl Drop for EncHandle<'_> {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: ptr は open で得た有効なセッションで、この後は使わない。
            unsafe { nv!(self.enc.fl.nvEncDestroyEncoder, self.ptr) };
        }
    }
}

fn open_session<'a>(enc: &'a Nvenc, ctx: &Ctx) -> Result<EncHandle<'a>> {
    let mut p = sys::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
        version: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
        deviceType: sys::NV_ENC_DEVICE_TYPE_CUDA,
        device: ctx.ctx.cast(),
        apiVersion: sys::NVENCAPI_VERSION,
        ..Default::default()
    };
    let mut ptr: *mut c_void = std::ptr::null_mut();
    // SAFETY: p は版を設定した構造体で、device は有効な CUDA context。出力先 ptr は有効。
    let st = unsafe { nv!(enc.fl.nvEncOpenEncodeSessionEx, &mut p, &mut ptr) };
    let h = EncHandle { enc, ptr };
    nvenc_check(st, "nvEncOpenEncodeSessionEx")?;
    if h.ptr.is_null() {
        return Err(CodecError::backend(
            "nvEncOpenEncodeSessionEx returned no session",
        ));
    }
    Ok(h)
}

fn session_guids(h: &EncHandle) -> Result<Vec<sys::GUID>> {
    let mut n = 0u32;
    // SAFETY: 有効なセッションと出力先。
    nvenc_check(
        unsafe { nv!(h.enc.fl.nvEncGetEncodeGUIDCount, h.ptr, &mut n) },
        "nvEncGetEncodeGUIDCount",
    )?;
    let n = n.min(64);
    let mut v = vec![sys::GUID::default(); n as usize];
    let mut got = 0u32;
    // SAFETY: v は n 個の GUID を書ける長さ。
    nvenc_check(
        unsafe {
            nv!(
                h.enc.fl.nvEncGetEncodeGUIDs,
                h.ptr,
                v.as_mut_ptr(),
                n,
                &mut got
            )
        },
        "nvEncGetEncodeGUIDs",
    )?;
    v.truncate(got.min(n) as usize);
    Ok(v)
}

fn encode_cap(h: &EncHandle, codec: sys::GUID, cap: sys::NV_ENC_CAPS) -> i32 {
    let mut p = sys::NV_ENC_CAPS_PARAM {
        version: NV_ENC_CAPS_PARAM_VER,
        capsToQuery: cap,
        ..Default::default()
    };
    let mut v: c_int = 0;
    // SAFETY: 有効なセッション、版を設定した構造体、出力先。
    let st = unsafe { nv!(h.enc.fl.nvEncGetEncodeCaps, h.ptr, codec, &mut p, &mut v) };
    if st == sys::NV_ENC_SUCCESS { v } else { 0 }
}

fn probe_encode(_libs: &NvLibs, enc: &Nvenc, ctx: &Ctx) -> Result<Vec<EncodeCap>> {
    let h = open_session(enc, ctx)?;
    let guids = session_guids(&h)?;
    let mut caps = Vec::new();
    for (codec, g) in [
        (Codec::H264, CODEC_H264),
        (Codec::H265, CODEC_HEVC),
        (Codec::Av1, CODEC_AV1),
    ] {
        if !guids.iter().any(|x| guid_eq(x, &g)) {
            continue;
        }
        let w = encode_cap(&h, g, sys::NV_ENC_CAPS_WIDTH_MAX);
        let hh = encode_cap(&h, g, sys::NV_ENC_CAPS_HEIGHT_MAX);
        if w <= 0 || hh <= 0 {
            continue;
        }
        caps.push(EncodeCap {
            codec,
            max_width: w as u32,
            max_height: hh as u32,
            ten_bit: encode_cap(&h, g, sys::NV_ENC_CAPS_SUPPORT_10BIT_ENCODE) != 0,
        });
    }
    Ok(caps)
}

fn cuvid_codec(codec: Codec) -> Option<sys::cudaVideoCodec> {
    Some(match codec {
        Codec::H264 => sys::cudaVideoCodec_H264,
        Codec::H265 => sys::cudaVideoCodec_HEVC,
        Codec::Av1 => sys::cudaVideoCodec_AV1,
        Codec::Vp9 => sys::cudaVideoCodec_VP9,
        _ => return None,
    })
}

/// 現在の context で cuvidGetDecoderCaps を呼ぶ。
fn decoder_caps(dec: &Cuvid, codec: Codec, bit_depth: u8) -> Option<sys::CUVIDDECODECAPS> {
    let mut c = sys::CUVIDDECODECAPS {
        eCodecType: cuvid_codec(codec)?,
        eChromaFormat: sys::cudaVideoChromaFormat_420,
        nBitDepthMinus8: (bit_depth - 8) as c_uint,
        ..Default::default()
    };
    // SAFETY: 入力欄を設定した構造体。呼び出し側が context を push している。
    let r = unsafe { cu!(dec.get_decoder_caps, &mut c) };
    (r == sys::CUDA_SUCCESS && c.bIsSupported != 0).then_some(c)
}

fn probe_decode(dec: &Cuvid) -> Vec<DecodeCap> {
    let mut out = Vec::new();
    for codec in [Codec::H264, Codec::H265, Codec::Av1, Codec::Vp9] {
        let depths: &[u8] = if codec == Codec::H264 { &[8] } else { &[8, 10] };
        for &bd in depths {
            if let Some(c) = decoder_caps(dec, codec, bd) {
                out.push(DecodeCap {
                    codec,
                    bit_depth: bd,
                    max_width: c.nMaxWidth,
                    max_height: c.nMaxHeight,
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// CUDA context
// ---------------------------------------------------------------------------------------------

/// セッション専用の CUDA context。Drop で破棄する。
struct Ctx {
    libs: Arc<NvLibs>,
    ctx: sys::CUcontext,
}

impl Ctx {
    fn new(libs: Arc<NvLibs>, index: u32) -> Result<Ctx> {
        let mut dev: sys::CUdevice = 0;
        // SAFETY: 出力先は有効。
        libs.cu_check(
            unsafe { cu!(libs.cu.device_get, &mut dev, index as c_int) },
            "cuDeviceGet",
        )?;
        let mut ctx: sys::CUcontext = std::ptr::null_mut();
        // SAFETY: 出力先は有効。作った context は現在のスレッドに積まれる。
        libs.cu_check(
            unsafe {
                cu!(
                    libs.cu.ctx_create,
                    &mut ctx,
                    sys::CU_CTX_SCHED_BLOCKING_SYNC,
                    dev
                )
            },
            "cuCtxCreate",
        )?;
        let mut popped: sys::CUcontext = std::ptr::null_mut();
        // SAFETY: cuCtxCreate が積んだ context を外す（以後は使うときだけ push する）。
        unsafe { cu!(libs.cu.ctx_pop, &mut popped) };
        Ok(Ctx { libs, ctx })
    }

    fn push(&self) -> Result<CtxGuard> {
        // SAFETY: self.ctx は有効な context。対になる pop は CtxGuard の Drop。
        self.libs.cu_check(
            unsafe { cu!(self.libs.cu.ctx_push, self.ctx) },
            "cuCtxPushCurrent",
        )?;
        Ok(CtxGuard {
            libs: self.libs.clone(),
        })
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            // SAFETY: この context を使うセッションの資源は先に解放済み（各 Drop の順序）。
            unsafe { cu!(self.libs.cu.ctx_destroy, self.ctx) };
        }
    }
}

/// context を現在のスレッドに積んでいる間の印。Drop で外す。
struct CtxGuard {
    libs: Arc<NvLibs>,
}

impl Drop for CtxGuard {
    fn drop(&mut self) {
        let mut popped: sys::CUcontext = std::ptr::null_mut();
        // SAFETY: push と対になる pop（同じスレッドで、push した context が一番上にある）。
        unsafe { cu!(self.libs.cu.ctx_pop, &mut popped) };
    }
}

// ---------------------------------------------------------------------------------------------
// NVENC
// ---------------------------------------------------------------------------------------------

/// NVENC の状態の名前。
pub(super) fn nvenc_status_name(st: sys::NVENCSTATUS) -> &'static str {
    match st {
        sys::NV_ENC_SUCCESS => "NV_ENC_SUCCESS",
        sys::NV_ENC_ERR_NO_ENCODE_DEVICE => "NV_ENC_ERR_NO_ENCODE_DEVICE",
        sys::NV_ENC_ERR_UNSUPPORTED_DEVICE => "NV_ENC_ERR_UNSUPPORTED_DEVICE",
        sys::NV_ENC_ERR_INVALID_DEVICE => "NV_ENC_ERR_INVALID_DEVICE",
        sys::NV_ENC_ERR_INVALID_PTR => "NV_ENC_ERR_INVALID_PTR",
        sys::NV_ENC_ERR_INVALID_PARAM => "NV_ENC_ERR_INVALID_PARAM",
        sys::NV_ENC_ERR_INVALID_CALL => "NV_ENC_ERR_INVALID_CALL",
        sys::NV_ENC_ERR_OUT_OF_MEMORY => "NV_ENC_ERR_OUT_OF_MEMORY",
        sys::NV_ENC_ERR_UNSUPPORTED_PARAM => "NV_ENC_ERR_UNSUPPORTED_PARAM",
        sys::NV_ENC_ERR_INVALID_VERSION => "NV_ENC_ERR_INVALID_VERSION",
        sys::NV_ENC_ERR_NEED_MORE_INPUT => "NV_ENC_ERR_NEED_MORE_INPUT",
        sys::NV_ENC_ERR_ENCODER_BUSY => "NV_ENC_ERR_ENCODER_BUSY",
        sys::NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY => "NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY",
        sys::NV_ENC_ERR_UNIMPLEMENTED => "NV_ENC_ERR_UNIMPLEMENTED",
        sys::NV_ENC_ERR_GENERIC => "NV_ENC_ERR_GENERIC",
        _ => "NV_ENC_ERR (other)",
    }
}

/// NVENC の失敗の種類。セッション上限（消費者向け GPU）・メモリ不足・装置なしは「枠が無い」。
pub(super) fn nvenc_error_kind(st: sys::NVENCSTATUS) -> ErrorKind {
    match st {
        sys::NV_ENC_ERR_OUT_OF_MEMORY
        | sys::NV_ENC_ERR_NO_ENCODE_DEVICE
        | sys::NV_ENC_ERR_UNSUPPORTED_DEVICE
        | sys::NV_ENC_ERR_ENCODER_BUSY
        | sys::NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY
        | sys::NV_ENC_ERR_INVALID_VERSION => ErrorKind::Unavailable,
        sys::NV_ENC_ERR_UNSUPPORTED_PARAM | sys::NV_ENC_ERR_INVALID_PARAM => {
            ErrorKind::InvalidConfig
        }
        _ => ErrorKind::Backend,
    }
}

fn nvenc_check(st: sys::NVENCSTATUS, what: &str) -> Result<()> {
    if st == sys::NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(CodecError::new(
            nvenc_error_kind(st),
            format!("{what}: {}", nvenc_status_name(st)),
        ))
    }
}

/// 符号量の制御。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RcMode {
    Cbr,
    Vbr,
    ConstQp(u8),
}

/// プロファイルの選び方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProfileSel {
    Auto,
    H264Baseline,
    H264Main,
    H264High,
    HevcMain,
    HevcMain10,
    Av1Main,
}

/// NVENC の設定（[`crate::VideoEncoderConfig`] から mod.rs で作る）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EncodeParams {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub bitrate: u32,
    pub max_bitrate: u32,
    /// VBV のバッファ（ビット、0 はプリセットの既定）。
    pub vbv_size: u32,
    /// キーフレームの間隔（0 = 無限）。
    pub gop: u32,
    pub rc: RcMode,
    pub profile: ProfileSel,
    pub low_latency: bool,
    pub b_frames: u32,
    pub ten_bit: bool,
}

fn codec_guid(codec: Codec) -> Option<sys::GUID> {
    match codec {
        Codec::H264 => Some(CODEC_H264),
        Codec::H265 => Some(CODEC_HEVC),
        Codec::Av1 => Some(CODEC_AV1),
        _ => None,
    }
}

fn profile_guid(p: ProfileSel) -> sys::GUID {
    match p {
        ProfileSel::Auto => PROFILE_AUTOSELECT,
        ProfileSel::H264Baseline => H264_PROFILE_BASELINE,
        ProfileSel::H264Main => H264_PROFILE_MAIN,
        ProfileSel::H264High => H264_PROFILE_HIGH,
        ProfileSel::HevcMain => HEVC_PROFILE_MAIN,
        ProfileSel::HevcMain10 => HEVC_PROFILE_MAIN10,
        ProfileSel::Av1Main => AV1_PROFILE_MAIN,
    }
}

/// プリセットとチューニング（低遅延なら P1 + ULTRA_LOW_LATENCY、そうでなければ P4 + LOW_LATENCY）。
fn preset_for(low_latency: bool) -> (sys::GUID, sys::NV_ENC_TUNING_INFO) {
    if low_latency {
        (PRESET_P1, sys::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY)
    } else {
        (PRESET_P4, sys::NV_ENC_TUNING_INFO_LOW_LATENCY)
    }
}

/// 出力されたビットストリーム 1 つ（Annex B または OBU 列のまま）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EncodedPacket {
    pub data: Vec<u8>,
    pub pts: i64,
    pub keyframe: bool,
}

struct IoBuf {
    input: sys::NV_ENC_INPUT_PTR,
    output: sys::NV_ENC_OUTPUT_PTR,
}

/// NVENC の符号化セッション（入力はシステムメモリの NV12 / P010、同期モード）。
pub(super) struct NvencSession {
    // Drop の順序: 先にバッファとエンコーダーを壊し、最後に context（ctx）を壊す。
    encoder: *mut c_void,
    bufs: Vec<IoBuf>,
    free: VecDeque<usize>,
    /// 符号化を依頼して、まだ出力を取り出していないバッファ（依頼順）。
    pending: VecDeque<usize>,
    width: u32,
    height: u32,
    format: sys::NV_ENC_BUFFER_FORMAT,
    ten_bit: bool,
    frame_idx: u32,
    ctx: Ctx,
}

// SAFETY: encoder・バッファのハンドルは、このセッションの CUDA context に属し、使うときは必ず
// その context を push する（CtxGuard）。NVENC はセッションを別スレッドから使うことを許す（同時でなければよい）。
// `&mut self` で同時の使用は起きない。
unsafe impl Send for NvencSession {}

impl NvencSession {
    pub(super) fn open(libs: &Arc<NvLibs>, device: u32, p: &EncodeParams) -> Result<NvencSession> {
        let enc = libs
            .enc
            .as_ref()
            .map_err(|e| CodecError::unavailable(format!("NVENC: {e}")))?;
        let codec = codec_guid(p.codec)
            .ok_or_else(|| CodecError::unsupported(format!("NVENC does not encode {}", p.codec)))?;
        let ctx = Ctx::new(libs.clone(), device)?;
        let mut s = NvencSession {
            encoder: std::ptr::null_mut(),
            bufs: Vec::new(),
            free: VecDeque::new(),
            pending: VecDeque::new(),
            width: p.width,
            height: p.height,
            format: if p.ten_bit {
                sys::NV_ENC_BUFFER_FORMAT_YUV420_10BIT
            } else {
                sys::NV_ENC_BUFFER_FORMAT_NV12
            },
            ten_bit: p.ten_bit,
            frame_idx: 0,
            ctx,
        };
        {
            let _g = s.ctx.push()?;
            let h = open_session(enc, &s.ctx)?;
            if !session_guids(&h)?.iter().any(|g| guid_eq(g, &codec)) {
                return Err(CodecError::unsupported(format!(
                    "this GPU's NVENC does not encode {}",
                    p.codec
                )));
            }
            // 以後は s.encoder が持つ（NvencSession の Drop で破棄）。h の Drop では壊さない。
            s.encoder = h.ptr;
            std::mem::forget(h);
        }
        s.init(enc, codec, p)?;
        Ok(s)
    }

    fn init(&mut self, enc: &Nvenc, codec: sys::GUID, p: &EncodeParams) -> Result<()> {
        let _g = self.ctx.push()?;
        let (preset, tuning) = preset_for(p.low_latency);
        let mut pc = sys::NV_ENC_PRESET_CONFIG {
            version: NV_ENC_PRESET_CONFIG_VER,
            ..Default::default()
        };
        pc.presetCfg.version = NV_ENC_CONFIG_VER;
        // SAFETY: 有効なセッション、版を設定した構造体。
        nvenc_check(
            unsafe {
                nv!(
                    enc.fl.nvEncGetEncodePresetConfigEx,
                    self.encoder,
                    codec,
                    preset,
                    tuning,
                    &mut pc
                )
            },
            "nvEncGetEncodePresetConfigEx",
        )?;
        let mut cfg = pc.presetCfg;
        cfg.version = NV_ENC_CONFIG_VER;
        cfg.profileGUID = profile_guid(p.profile);
        cfg.gopLength = if p.gop == 0 {
            sys::NVENC_INFINITE_GOPLENGTH
        } else {
            p.gop
        };
        cfg.frameIntervalP = 1 + p.b_frames.min(4) as i32;
        cfg.rcParams.version = NV_ENC_RC_PARAMS_VER;
        match p.rc {
            RcMode::Cbr => {
                cfg.rcParams.rateControlMode = sys::NV_ENC_PARAMS_RC_CBR;
                cfg.rcParams.averageBitRate = p.bitrate;
                cfg.rcParams.maxBitRate = p.bitrate;
            }
            RcMode::Vbr => {
                cfg.rcParams.rateControlMode = sys::NV_ENC_PARAMS_RC_VBR;
                cfg.rcParams.averageBitRate = p.bitrate;
                cfg.rcParams.maxBitRate = p.max_bitrate;
            }
            RcMode::ConstQp(q) => {
                cfg.rcParams.rateControlMode = sys::NV_ENC_PARAMS_RC_CONSTQP;
                let q = q as u32;
                cfg.rcParams.constQP = sys::NV_ENC_QP {
                    qpInterP: q,
                    qpInterB: q,
                    qpIntra: q,
                };
            }
        }
        if p.vbv_size > 0 {
            cfg.rcParams.vbvBufferSize = p.vbv_size;
            cfg.rcParams.vbvInitialDelay = p.vbv_size;
        }
        let idr = cfg.gopLength;
        let depth = if p.ten_bit {
            sys::NV_ENC_BIT_DEPTH_10
        } else {
            sys::NV_ENC_BIT_DEPTH_8
        };
        // SAFETY: encodeCodecConfig は共用体で、codec に対応する欄だけを書き換える（プリセットが同じ codec で埋めた）。
        unsafe {
            match p.codec {
                Codec::H264 => {
                    let h = &mut cfg.encodeCodecConfig.h264Config;
                    h.idrPeriod = idr;
                    h.set_repeatSPSPPS(0);
                    h.set_outputAUD(0);
                    h.set_disableSPSPPS(0);
                    h.chromaFormatIDC = 1;
                    if p.b_frames > 0 {
                        h.useBFramesAsRef = sys::NV_ENC_BFRAME_REF_MODE_DISABLED;
                    }
                }
                Codec::H265 => {
                    let h = &mut cfg.encodeCodecConfig.hevcConfig;
                    h.idrPeriod = idr;
                    h.set_repeatSPSPPS(0);
                    h.set_outputAUD(0);
                    h.set_disableSPSPPS(0);
                    h.set_chromaFormatIDC(1);
                    h.inputBitDepth = depth;
                    h.outputBitDepth = depth;
                    if p.b_frames > 0 {
                        h.useBFramesAsRef = sys::NV_ENC_BFRAME_REF_MODE_DISABLED;
                    }
                }
                Codec::Av1 => {
                    let a = &mut cfg.encodeCodecConfig.av1Config;
                    a.idrPeriod = idr;
                    a.set_outputAnnexBFormat(0);
                    a.set_disableSeqHdr(0);
                    a.set_repeatSeqHdr(1);
                    a.set_chromaFormatIDC(1);
                    a.inputBitDepth = depth;
                    a.outputBitDepth = depth;
                    if p.b_frames > 0 {
                        a.useBFramesAsRef = sys::NV_ENC_BFRAME_REF_MODE_DISABLED;
                    }
                }
                _ => return Err(CodecError::unsupported("codec")),
            }
        }
        let mut ip = sys::NV_ENC_INITIALIZE_PARAMS {
            version: NV_ENC_INITIALIZE_PARAMS_VER,
            encodeGUID: codec,
            presetGUID: preset,
            encodeWidth: p.width,
            encodeHeight: p.height,
            darWidth: p.width,
            darHeight: p.height,
            frameRateNum: p.fps_num,
            frameRateDen: p.fps_den,
            enableEncodeAsync: 0,
            enablePTD: 1,
            encodeConfig: &mut cfg,
            maxEncodeWidth: p.width,
            maxEncodeHeight: p.height,
            tuningInfo: tuning,
            bufferFormat: self.format,
            ..Default::default()
        };
        // SAFETY: ip と ip.encodeConfig（cfg）は呼び出しの間有効。
        nvenc_check(
            unsafe { nv!(enc.fl.nvEncInitializeEncoder, self.encoder, &mut ip) },
            "nvEncInitializeEncoder",
        )?;
        // 入出力バッファ: B フレームで出力が遅れる分 + 余裕。
        let n = p.b_frames.min(4) as usize + 3;
        for i in 0..n {
            let mut ib = sys::NV_ENC_CREATE_INPUT_BUFFER {
                version: NV_ENC_CREATE_INPUT_BUFFER_VER,
                width: p.width,
                height: p.height,
                memoryHeap: sys::NV_ENC_MEMORY_HEAP_SYSMEM_CACHED,
                bufferFmt: self.format,
                ..Default::default()
            };
            // SAFETY: 版を設定した構造体。作ったバッファは Drop で壊す。
            nvenc_check(
                unsafe { nv!(enc.fl.nvEncCreateInputBuffer, self.encoder, &mut ib) },
                "nvEncCreateInputBuffer",
            )?;
            let mut ob = sys::NV_ENC_CREATE_BITSTREAM_BUFFER {
                version: NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
                ..Default::default()
            };
            // SAFETY: 同上。
            let st = unsafe { nv!(enc.fl.nvEncCreateBitstreamBuffer, self.encoder, &mut ob) };
            self.bufs.push(IoBuf {
                input: ib.inputBuffer,
                output: if st == sys::NV_ENC_SUCCESS {
                    ob.bitstreamBuffer
                } else {
                    std::ptr::null_mut()
                },
            });
            nvenc_check(st, "nvEncCreateBitstreamBuffer")?;
            self.free.push_back(i);
        }
        Ok(())
    }

    /// パラメーターセット（H.264: SPS+PPS、HEVC: VPS+SPS+PPS の Annex B、AV1: シーケンスヘッダー OBU）。
    pub(super) fn sequence_params(&mut self) -> Result<Vec<u8>> {
        let _g = self.ctx.push()?;
        let mut buf = vec![0u8; 1024];
        let mut size = 0u32;
        let mut sp = sys::NV_ENC_SEQUENCE_PARAM_PAYLOAD {
            version: NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER,
            inBufferSize: buf.len() as u32,
            spsppsBuffer: buf.as_mut_ptr().cast(),
            outSPSPPSPayloadSize: &mut size,
            ..Default::default()
        };
        let libs = self.ctx.libs.clone();
        let fl = &libs
            .enc
            .as_ref()
            .map_err(|e| CodecError::internal(e.clone()))?
            .fl;
        // SAFETY: spsppsBuffer は inBufferSize バイト書ける。outSPSPPSPayloadSize は有効な u32。
        nvenc_check(
            unsafe { nv!(fl.nvEncGetSequenceParams, self.encoder, &mut sp) },
            "nvEncGetSequenceParams",
        )?;
        buf.truncate((size as usize).min(1024));
        Ok(buf)
    }

    /// フレームを 1 つ符号化し、出てきたビットストリームを返す（B フレームのときは遅れて出る）。
    /// `frame` は NV12（8 bit）または P010（10 bit）で、大きさはセッションと同じであること（mod.rs で確かめる）。
    pub(super) fn encode(
        &mut self,
        frame: &VideoFrame,
        pts: i64,
        force_idr: bool,
    ) -> Result<Vec<EncodedPacket>> {
        let want = if self.ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        if frame.format != want || frame.width != self.width || frame.height != self.height {
            return Err(CodecError::invalid_input(
                "frame does not match NVENC session",
            ));
        }
        let mut out = Vec::new();
        if self.free.is_empty() {
            // 出力の遅れがバッファ数を超えた（起こらないはずだが、念のため取り出してから続ける）。
            return Err(CodecError::internal("NVENC buffers exhausted"));
        }
        let idx = self.free.pop_front().expect("checked non-empty");
        let r = self.encode_into(idx, frame, pts, force_idr, &mut out);
        if r.is_err() && !self.pending.contains(&idx) && !self.free.contains(&idx) {
            self.free.push_back(idx);
        }
        r.map(|_| out)
    }

    fn encode_into(
        &mut self,
        idx: usize,
        frame: &VideoFrame,
        pts: i64,
        force_idr: bool,
        out: &mut Vec<EncodedPacket>,
    ) -> Result<()> {
        let _g = self.ctx.push()?;
        let libs = self.ctx.libs.clone();
        let fl = &libs
            .enc
            .as_ref()
            .map_err(|e| CodecError::internal(e.clone()))?
            .fl;
        let input = self.bufs[idx].input;
        let mut lk = sys::NV_ENC_LOCK_INPUT_BUFFER {
            version: NV_ENC_LOCK_INPUT_BUFFER_VER,
            inputBuffer: input,
            ..Default::default()
        };
        // SAFETY: input はこのセッションの入力バッファ。
        nvenc_check(
            unsafe { nv!(fl.nvEncLockInputBuffer, self.encoder, &mut lk) },
            "nvEncLockInputBuffer",
        )?;
        let pitch = lk.pitch as usize;
        let len = super::input_buffer_len(frame.format, pitch, self.height);
        let copy = match len {
            Some(len) if !lk.bufferDataPtr.is_null() => {
                // SAFETY: NVENC は width x height の NV12/P010 を pitch で並べた領域（輝度 pitch*height、
                // 続いて色差 pitch*ceil(height/2)）をロックして返す。len はその大きさで、unlock までの間だけ使う。
                let dst =
                    unsafe { std::slice::from_raw_parts_mut(lk.bufferDataPtr.cast::<u8>(), len) };
                super::copy_frame_to_pitched(frame, dst, pitch, self.height as usize)
            }
            _ => Err(CodecError::backend("NVENC input buffer pitch is invalid")),
        };
        // SAFETY: lock と対になる unlock。
        let st = unsafe { nv!(fl.nvEncUnlockInputBuffer, self.encoder, input) };
        copy?;
        nvenc_check(st, "nvEncUnlockInputBuffer")?;
        let mut pp = sys::NV_ENC_PIC_PARAMS {
            version: NV_ENC_PIC_PARAMS_VER,
            inputWidth: self.width,
            inputHeight: self.height,
            inputPitch: pitch as u32,
            encodePicFlags: if force_idr {
                sys::NV_ENC_PIC_FLAG_FORCEIDR
            } else {
                0
            },
            frameIdx: self.frame_idx,
            inputTimeStamp: pts as u64,
            inputBuffer: input,
            outputBitstream: self.bufs[idx].output,
            bufferFmt: self.format,
            pictureStruct: sys::NV_ENC_PIC_STRUCT_FRAME,
            ..Default::default()
        };
        self.frame_idx = self.frame_idx.wrapping_add(1);
        // SAFETY: pp は版を設定した構造体で、入出力バッファはこのセッションのもの。同期モード。
        let st = unsafe { nv!(fl.nvEncEncodePicture, self.encoder, &mut pp) };
        match st {
            sys::NV_ENC_SUCCESS => {
                self.pending.push_back(idx);
                self.drain(out)
            }
            // B フレームなどで出力が後になる。バッファは依頼済みとして残す。
            sys::NV_ENC_ERR_NEED_MORE_INPUT => {
                self.pending.push_back(idx);
                Ok(())
            }
            _ => nvenc_check(st, "nvEncEncodePicture"),
        }
    }

    /// 依頼済みの出力をすべて取り出す（呼び出し側が context を push していること）。
    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<()> {
        let libs = self.ctx.libs.clone();
        let fl = &libs
            .enc
            .as_ref()
            .map_err(|e| CodecError::internal(e.clone()))?
            .fl;
        while let Some(idx) = self.pending.pop_front() {
            let output = self.bufs[idx].output;
            let mut lb = sys::NV_ENC_LOCK_BITSTREAM {
                version: NV_ENC_LOCK_BITSTREAM_VER,
                outputBitstream: output,
                ..Default::default()
            };
            // SAFETY: output はこのセッションの出力バッファで、符号化を依頼済み。doNotWait=0 で完了を待つ。
            let st = unsafe { nv!(fl.nvEncLockBitstream, self.encoder, &mut lb) };
            if st != sys::NV_ENC_SUCCESS {
                self.free.push_back(idx);
                return nvenc_check(st, "nvEncLockBitstream");
            }
            let n = lb.bitstreamSizeInBytes as usize;
            let data = if lb.bitstreamBufferPtr.is_null() || n > super::MAX_PACKET_BYTES {
                None
            } else {
                // SAFETY: ロック中の出力は bitstreamBufferPtr から bitstreamSizeInBytes バイト。unlock 前に複製する。
                Some(
                    unsafe { std::slice::from_raw_parts(lb.bitstreamBufferPtr.cast::<u8>(), n) }
                        .to_vec(),
                )
            };
            // SAFETY: lock と対になる unlock。
            unsafe { nv!(fl.nvEncUnlockBitstream, self.encoder, output) };
            self.free.push_back(idx);
            let data =
                data.ok_or_else(|| CodecError::backend("NVENC returned an invalid bitstream"))?;
            out.push(EncodedPacket {
                data,
                pts: lb.outputTimeStamp as i64,
                keyframe: lb.pictureType == sys::NV_ENC_PIC_TYPE_IDR
                    || lb.pictureType == sys::NV_ENC_PIC_TYPE_I,
            });
        }
        Ok(())
    }

    /// 入力の終わり（EOS）を送り、残りの出力をすべて返す。
    pub(super) fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        let _g = self.ctx.push()?;
        let libs = self.ctx.libs.clone();
        let fl = &libs
            .enc
            .as_ref()
            .map_err(|e| CodecError::internal(e.clone()))?
            .fl;
        let mut pp = sys::NV_ENC_PIC_PARAMS {
            version: NV_ENC_PIC_PARAMS_VER,
            encodePicFlags: sys::NV_ENC_PIC_FLAG_EOS,
            ..Default::default()
        };
        // SAFETY: EOS の通知（入出力バッファなし）。
        let st = unsafe { nv!(fl.nvEncEncodePicture, self.encoder, &mut pp) };
        let mut out = Vec::new();
        self.drain(&mut out)?;
        nvenc_check(st, "nvEncEncodePicture(EOS)")?;
        Ok(out)
    }
}

impl Drop for NvencSession {
    fn drop(&mut self) {
        let Ok(enc) = self.ctx.libs.enc.as_ref() else {
            return;
        };
        let Ok(_g) = self.ctx.push() else { return };
        for b in &self.bufs {
            // SAFETY: このセッションで作ったバッファ。以後は使わない。
            unsafe {
                if !b.input.is_null() {
                    nv!(enc.fl.nvEncDestroyInputBuffer, self.encoder, b.input);
                }
                if !b.output.is_null() {
                    nv!(enc.fl.nvEncDestroyBitstreamBuffer, self.encoder, b.output);
                }
            }
        }
        if !self.encoder.is_null() {
            // SAFETY: open で得たセッション。
            unsafe { nv!(enc.fl.nvEncDestroyEncoder, self.encoder) };
        }
    }
}

// ---------------------------------------------------------------------------------------------
// NVDEC（cuvid）
// ---------------------------------------------------------------------------------------------

/// 復号の設定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DecodeParams {
    pub codec: Codec,
    pub max_width: u32,
    pub max_height: u32,
}

/// 復号した 1 枚（NV12 または P010 を詰めて並べたもの）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DecodedPicture {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Vec<u8>,
    pub pts: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CurrentFormat {
    coded_width: u32,
    coded_height: u32,
    width: u32,
    height: u32,
    bit_depth: u8,
    surfaces: u32,
    progressive: bool,
}

/// コールバックから触る状態（Box で固定し、そのポインターを pUserData に渡す）。
struct DecState {
    libs: Arc<NvLibs>,
    params: DecodeParams,
    lock: sys::CUvideoctxlock,
    decoder: sys::CUvideodecoder,
    format: Option<CurrentFormat>,
    out: VecDeque<DecodedPicture>,
    error: Option<CodecError>,
}

/// 出力を溜められる枚数（受け取らずに送り続けたら止める）。
const MAX_QUEUED_PICTURES: usize = 32;

impl DecState {
    fn cuvid(&self) -> Result<&Cuvid> {
        self.libs
            .dec
            .as_ref()
            .map_err(|e| CodecError::unavailable(format!("NVDEC: {e}")))
    }

    fn on_sequence(&mut self, f: &sys::CUVIDEOFORMAT) -> Result<c_int> {
        let codec =
            cuvid_codec(self.params.codec).ok_or_else(|| CodecError::unsupported("codec"))?;
        if f.codec != codec {
            return Err(CodecError::invalid_input(
                "stream codec does not match decoder",
            ));
        }
        if f.chroma_format != sys::cudaVideoChromaFormat_420 {
            return Err(CodecError::unsupported("NVDEC: only 4:2:0 is supported"));
        }
        let bit_depth = 8 + f.bit_depth_luma_minus8;
        if !matches!(bit_depth, 8 | 10) || f.bit_depth_chroma_minus8 != f.bit_depth_luma_minus8 {
            return Err(CodecError::unsupported(
                "NVDEC: only 8 and 10 bit are supported",
            ));
        }
        let da = f.display_area;
        let (w, h) = super::display_size(da.left, da.top, da.right, da.bottom)
            .ok_or_else(|| CodecError::invalid_input("bad display area"))?;
        if w > self.params.max_width || h > self.params.max_height {
            return Err(CodecError::invalid_input(format!(
                "stream resolution {w}x{h} outside allowed range"
            )));
        }
        let caps = decoder_caps(self.cuvid()?, self.params.codec, bit_depth).ok_or_else(|| {
            CodecError::unsupported(format!(
                "NVDEC does not decode {} {bit_depth} bit",
                self.params.codec
            ))
        })?;
        if f.coded_width > caps.nMaxWidth || f.coded_height > caps.nMaxHeight {
            return Err(CodecError::unsupported("resolution exceeds NVDEC limit"));
        }
        let surfaces = (f.min_num_decode_surfaces as u32).max(1) + 3;
        let fmt = CurrentFormat {
            coded_width: f.coded_width,
            coded_height: f.coded_height,
            width: w,
            height: h,
            bit_depth,
            surfaces,
            progressive: f.progressive_sequence != 0,
        };
        if self.format == Some(fmt) && !self.decoder.is_null() {
            return Ok(surfaces as c_int);
        }
        self.destroy_decoder();
        let mut ci = sys::CUVIDDECODECREATEINFO {
            ulWidth: f.coded_width as sys::tcu_ulong,
            ulHeight: f.coded_height as sys::tcu_ulong,
            ulNumDecodeSurfaces: surfaces as sys::tcu_ulong,
            CodecType: codec,
            ChromaFormat: sys::cudaVideoChromaFormat_420,
            ulCreationFlags: sys::cudaVideoCreate_PreferCUVID as sys::tcu_ulong,
            bitDepthMinus8: (bit_depth - 8) as sys::tcu_ulong,
            ulMaxWidth: f.coded_width as sys::tcu_ulong,
            ulMaxHeight: f.coded_height as sys::tcu_ulong,
            OutputFormat: if bit_depth > 8 {
                sys::cudaVideoSurfaceFormat_P016
            } else {
                sys::cudaVideoSurfaceFormat_NV12
            },
            DeinterlaceMode: if fmt.progressive {
                sys::cudaVideoDeinterlaceMode_Weave
            } else {
                sys::cudaVideoDeinterlaceMode_Adaptive
            },
            ulTargetWidth: w as sys::tcu_ulong,
            ulTargetHeight: h as sys::tcu_ulong,
            ulNumOutputSurfaces: 2,
            vidLock: self.lock,
            ..Default::default()
        };
        ci.display_area.left = da.left as i16;
        ci.display_area.top = da.top as i16;
        ci.display_area.right = da.right as i16;
        ci.display_area.bottom = da.bottom as i16;
        let mut dec: sys::CUvideodecoder = std::ptr::null_mut();
        // SAFETY: ci は入力欄を設定した構造体。呼び出しは parse の中（context は push 済み）。
        let r = unsafe { cu!(self.cuvid()?.create_decoder, &mut dec, &mut ci) };
        self.libs.cu_check(r, "cuvidCreateDecoder")?;
        self.decoder = dec;
        self.format = Some(fmt);
        Ok(surfaces as c_int)
    }

    fn on_decode(&mut self, pic: *mut sys::CUVIDPICPARAMS) -> Result<()> {
        if self.decoder.is_null() {
            return Err(CodecError::invalid_input("picture before sequence header"));
        }
        // SAFETY: pic はパーサーが渡した有効な構造体。decoder は on_sequence で作ったもの。
        let r = unsafe { cu!(self.cuvid()?.decode_picture, self.decoder, pic) };
        self.libs.cu_check(r, "cuvidDecodePicture")
    }

    fn on_display(&mut self, disp: &sys::CUVIDPARSERDISPINFO) -> Result<()> {
        let fmt = self
            .format
            .ok_or_else(|| CodecError::internal("display before sequence"))?;
        if self.out.len() >= MAX_QUEUED_PICTURES {
            return Err(CodecError::invalid_input("too many undelivered pictures"));
        }
        let mut pp = sys::CUVIDPROCPARAMS {
            progressive_frame: disp.progressive_frame,
            second_field: 0,
            top_field_first: disp.top_field_first,
            unpaired_field: (disp.repeat_first_field < 0) as c_int,
            ..Default::default()
        };
        let mut dptr: u64 = 0;
        let mut pitch: c_uint = 0;
        let cuvid = self.cuvid()?;
        // SAFETY: picture_index はパーサーが表示を求めた面。出力先は有効。
        let r = unsafe {
            cu!(
                cuvid.map_video_frame,
                self.decoder,
                disp.picture_index,
                &mut dptr,
                &mut pitch,
                &mut pp
            )
        };
        self.libs.cu_check(r, "cuvidMapVideoFrame")?;
        let format = if fmt.bit_depth > 8 {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        let res = self.copy_out(dptr, pitch as usize, fmt, format);
        // SAFETY: map と対になる unmap。
        unsafe { cu!(cuvid.unmap_video_frame, self.decoder, dptr) };
        let data = res?;
        self.out.push_back(DecodedPicture {
            width: fmt.width,
            height: fmt.height,
            format,
            data,
            pts: disp.timestamp,
        });
        Ok(())
    }

    fn copy_out(
        &self,
        dptr: u64,
        pitch: usize,
        fmt: CurrentFormat,
        format: PixelFormat,
    ) -> Result<Vec<u8>> {
        let (yrow, yrows) = format
            .plane_size(fmt.width, fmt.height, 0)
            .expect("plane 0");
        let (crow, crows) = format
            .plane_size(fmt.width, fmt.height, 1)
            .expect("plane 1");
        if pitch < yrow || pitch < crow {
            return Err(CodecError::backend("NVDEC pitch too small"));
        }
        let mut buf = vec![0u8; yrow * yrows + crow * crows];
        // 出力面の色差は、輝度（ulTargetHeight を偶数に切り上げた行数）の後ろにある。
        let chroma_off = pitch as u64 * ((fmt.height as u64 + 1) & !1);
        let (luma, chroma) = buf.split_at_mut(yrow * yrows);
        for (src, dst, row, rows) in [
            (dptr, luma, yrow, yrows),
            (dptr + chroma_off, chroma, crow, crows),
        ] {
            let c = sys::CUDA_MEMCPY2D {
                srcMemoryType: sys::CU_MEMORYTYPE_DEVICE,
                srcDevice: src,
                srcPitch: pitch,
                dstMemoryType: sys::CU_MEMORYTYPE_HOST,
                dstHost: dst.as_mut_ptr().cast(),
                dstPitch: row,
                WidthInBytes: row,
                Height: rows,
                ..Default::default()
            };
            // SAFETY: 転送元は map した面（pitch × 行数の範囲内）、転送先は row × rows バイトの dst。context は push 済み。
            let r = unsafe { cu!(self.libs.cu.memcpy_2d, &c) };
            self.libs.cu_check(r, "cuMemcpy2D")?;
        }
        Ok(buf)
    }

    fn destroy_decoder(&mut self) {
        if !self.decoder.is_null() {
            if let Ok(c) = self.cuvid() {
                // SAFETY: on_sequence で作った decoder。以後は使わない。
                unsafe { cu!(c.destroy_decoder, self.decoder) };
            }
            self.decoder = std::ptr::null_mut();
            self.format = None;
        }
    }
}

/// コールバックの共通部分。pUserData から状態を取り出し、パニックを C の側へ漏らさない。
fn with_state<F: FnOnce(&mut DecState) -> Result<c_int>>(user: *mut c_void, f: F) -> c_int {
    if user.is_null() {
        return 0;
    }
    // SAFETY: user は CuvidSession が持つ Box<DecState> のポインターで、パーサーはこれを
    // cuvidParseVideoData の中から同じスレッドで同期的に渡すだけ（再入しない）。その間 CuvidSession 側は
    // state を触らない（parse は state をこのポインター経由でしか使わない）ので、可変参照は 1 つだけ。
    let st = unsafe { &mut *(user.cast::<DecState>()) };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(st))) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            st.error.get_or_insert(e);
            0
        }
        Err(_) => {
            st.error
                .get_or_insert(CodecError::internal("panic in NVDEC callback"));
            0
        }
    }
}

unsafe extern "C" fn on_sequence_cb(user: *mut c_void, fmt: *mut sys::CUVIDEOFORMAT) -> c_int {
    with_state(user, |st| {
        // SAFETY: パーサーが渡す有効な CUVIDEOFORMAT（呼び出しの間だけ）。
        let f = unsafe { fmt.as_ref() }.ok_or_else(|| CodecError::backend("null video format"))?;
        st.on_sequence(f)
    })
}

unsafe extern "C" fn on_decode_cb(user: *mut c_void, pic: *mut sys::CUVIDPICPARAMS) -> c_int {
    with_state(user, |st| st.on_decode(pic).map(|_| 1))
}

unsafe extern "C" fn on_display_cb(
    user: *mut c_void,
    disp: *mut sys::CUVIDPARSERDISPINFO,
) -> c_int {
    with_state(user, |st| {
        // SAFETY: NULL は終わりの合図。そうでなければ呼び出しの間有効な構造体。
        match unsafe { disp.as_ref() } {
            None => Ok(1),
            Some(d) => st.on_display(d).map(|_| 1),
        }
    })
}

/// NVDEC の復号セッション（cuvid のパーサー + デコーダー）。
pub(super) struct CuvidSession {
    parser: sys::CUvideoparser,
    /// `Box::into_raw` で作った状態。パーサーの pUserData と同じポインターで、Drop で `Box::from_raw` して解放する。
    /// Box のまま持つと、コールバック用に渡した生ポインターとの別名の規則（Box は一意）を破るので生ポインターで持つ。
    state: std::ptr::NonNull<DecState>,
    ctx: Ctx,
}

// SAFETY: パーサー・デコーダー・ロックはこのセッションの context に属し、使うときは必ず push する。
// コールバックは parse の中から同じスレッドでだけ呼ばれる。`&mut self` で同時の使用は起きない。
unsafe impl Send for CuvidSession {}

impl CuvidSession {
    pub(super) fn open(
        libs: &Arc<NvLibs>,
        device: u32,
        params: DecodeParams,
    ) -> Result<CuvidSession> {
        let cuvid = libs
            .dec
            .as_ref()
            .map_err(|e| CodecError::unavailable(format!("NVDEC: {e}")))?;
        let codec = cuvid_codec(params.codec).ok_or_else(|| {
            CodecError::unsupported(format!("NVDEC does not decode {}", params.codec))
        })?;
        let ctx = Ctx::new(libs.clone(), device)?;
        let mut lock: sys::CUvideoctxlock = std::ptr::null_mut();
        // SAFETY: 出力先は有効、ctx は有効な context。
        libs.cu_check(
            unsafe { cu!(cuvid.ctx_lock_create, &mut lock, ctx.ctx) },
            "cuvidCtxLockCreate",
        )?;
        let state = Box::new(DecState {
            libs: libs.clone(),
            params,
            lock,
            decoder: std::ptr::null_mut(),
            format: None,
            out: VecDeque::new(),
            error: None,
        });
        let mut s = CuvidSession {
            parser: std::ptr::null_mut(),
            state: std::ptr::NonNull::from(Box::leak(state)),
            ctx,
        };
        let mut pp = sys::CUVIDPARSERPARAMS {
            CodecType: codec,
            ulMaxNumDecodeSurfaces: 1,
            ulMaxDisplayDelay: 0,
            pUserData: s.state.as_ptr().cast(),
            pfnSequenceCallback: Some(on_sequence_cb),
            pfnDecodePicture: Some(on_decode_cb),
            pfnDisplayPicture: Some(on_display_cb),
            ..Default::default()
        };
        let _g = s.ctx.push()?;
        let mut parser: sys::CUvideoparser = std::ptr::null_mut();
        // SAFETY: pp は呼び出しの間有効。pUserData は s.state（Box で固定、パーサーより長生き: Drop で先にパーサーを壊す）。
        let r = unsafe { cu!(cuvid.create_video_parser, &mut parser, &mut pp) };
        libs.cu_check(r, "cuvidCreateVideoParser")?;
        s.parser = parser;
        drop(_g);
        Ok(s)
    }

    /// データを渡す（`eos` なら終わりの合図）。復号できた分は [`Self::pop`] で取り出す。
    pub(super) fn parse(&mut self, data: &[u8], pts: i64, eos: bool) -> Result<()> {
        let cuvid = self
            .ctx
            .libs
            .dec
            .as_ref()
            .map_err(|e| CodecError::unavailable(format!("NVDEC: {e}")))?;
        let mut pkt = sys::CUVIDSOURCEDATAPACKET {
            flags: if eos {
                sys::CUVID_PKT_ENDOFSTREAM as sys::tcu_ulong
            } else {
                sys::CUVID_PKT_TIMESTAMP as sys::tcu_ulong
            },
            payload_size: if eos { 0 } else { data.len() as sys::tcu_ulong },
            payload: if eos { std::ptr::null() } else { data.as_ptr() },
            timestamp: pts,
        };
        let _g = self.ctx.push()?;
        // SAFETY: payload は呼び出しの間有効な data。コールバックは state を pUserData 経由で使う（with_state）。
        // この間、self.state への参照は作らない。
        let r = unsafe { cu!(cuvid.parse_video_data, self.parser, &mut pkt) };
        if let Some(e) = self.state_mut().error.take() {
            return Err(e);
        }
        self.ctx.libs.cu_check(r, "cuvidParseVideoData")
    }

    pub(super) fn pop(&mut self) -> Option<DecodedPicture> {
        self.state_mut().out.pop_front()
    }

    fn state_mut(&mut self) -> &mut DecState {
        // SAFETY: state は open で Box から作った有効なポインターで、Drop まで解放しない。コールバックが
        // 使うのは parse の中だけなので、parse の外（&mut self の間）ではこれが唯一の参照。
        unsafe { self.state.as_mut() }
    }
}

impl Drop for CuvidSession {
    fn drop(&mut self) {
        self.release();
        // SAFETY: state は open で Box::leak したもので、パーサーは壊した（または作れなかった）ので
        // 以後コールバックから使われない。ここで一度だけ Box に戻して解放する。
        drop(unsafe { Box::from_raw(self.state.as_ptr()) });
    }
}

impl CuvidSession {
    fn release(&mut self) {
        let libs = self.ctx.libs.clone();
        let Ok(cuvid) = libs.dec.as_ref() else { return };
        let Ok(g) = self.ctx.push() else { return };
        if !self.parser.is_null() {
            // SAFETY: open で作ったパーサー。state より先に壊す（以後コールバックは呼ばれない）。
            unsafe { cu!(cuvid.destroy_video_parser, self.parser) };
            self.parser = std::ptr::null_mut();
        }
        self.state_mut().destroy_decoder();
        drop(g);
        let lock = self.state_mut().lock;
        if !lock.is_null() {
            // SAFETY: open で作ったロック。デコーダーは壊した後。
            unsafe { cu!(cuvid.ctx_lock_destroy, lock) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_match_header() {
        assert_eq!(HEADER_API_VERSION, (13 << 4) | 1);
        // NVENCAPI_STRUCT_VERSION(1) = 13 | (1<<24) | (1<<16) | (7<<28)
        assert_eq!(
            NV_ENC_CAPS_PARAM_VER,
            13 | (1 << 24) | (1 << 16) | (7 << 28)
        );
        assert_eq!(NV_ENC_CONFIG_VER & (1 << 31), 1 << 31);
    }

    #[test]
    fn guids() {
        assert!(guid_eq(&codec_guid(Codec::H264).unwrap(), &CODEC_H264));
        assert!(!guid_eq(&CODEC_H264, &CODEC_HEVC));
        assert!(codec_guid(Codec::Vp9).is_none());
        assert!(guid_eq(
            &profile_guid(ProfileSel::HevcMain10),
            &HEVC_PROFILE_MAIN10
        ));
        let (p, t) = preset_for(true);
        assert!(guid_eq(&p, &PRESET_P1));
        assert_eq!(t, sys::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY);
        assert_eq!(cuvid_codec(Codec::Vp8), None);
    }

    #[test]
    fn error_kinds() {
        assert_eq!(
            nvenc_error_kind(sys::NV_ENC_ERR_OUT_OF_MEMORY),
            ErrorKind::Unavailable
        );
        assert_eq!(
            nvenc_error_kind(sys::NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY),
            ErrorKind::Unavailable
        );
        assert_eq!(
            nvenc_error_kind(sys::NV_ENC_ERR_INVALID_PARAM),
            ErrorKind::InvalidConfig
        );
        assert_eq!(
            nvenc_error_kind(sys::NV_ENC_ERR_GENERIC),
            ErrorKind::Backend
        );
        assert!(nvenc_check(sys::NV_ENC_SUCCESS, "x").is_ok());
        assert!(
            nvenc_check(sys::NV_ENC_ERR_GENERIC, "x")
                .unwrap_err()
                .message
                .contains("GENERIC")
        );
        assert_eq!(cuda_error_kind(100), ErrorKind::Unavailable);
        assert_eq!(cuda_error_kind(999), ErrorKind::Backend);
    }

    #[test]
    fn struct_sizes() {
        // bindgen の配置検査に加え、よく使う構造体の大きさを固定しておく（x86_64）。
        assert_eq!(std::mem::size_of::<sys::GUID>(), 16);
        assert_eq!(std::mem::size_of::<sys::CUVIDSOURCEDATAPACKET>(), 32);
        assert_eq!(std::mem::size_of::<sys::CUDA_MEMCPY2D>(), 128);
    }
}
