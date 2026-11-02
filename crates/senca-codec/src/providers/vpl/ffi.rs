//! oneVPL の C API を呼ぶ部分（このプロバイダーで `unsafe` を使うのはここだけ）。
//!
//! # 境界
//!
//! - `libvpl.so.2`（dispatcher）を [`crate::dl`] で読み込み、関数ポインターを [`Vpl`] に持つ。
//!   [`Vpl`] は `Arc<Library>` を保持するので、関数ポインターはライブラリが生きている間だけ使われる。
//! - セッション（`mfxSession`）とローダー（`mfxLoader`）は [`Session`] が所有し、`Drop` で
//!   `MFXClose` → `MFXUnload` の順に閉じる。1 つのセッションは 1 つのスレッドからだけ使う（`Send` だが `Sync` でない）。
//! - 面（surface）は VPL 2.x の内部割り当て（`MFXMemory_GetSurfaceForEncode`、復号の `surface_work = NULL`）を使い、
//!   `FrameInterface->Map/Unmap/Release` で読み書きする。行の長さ・行数は `Info.CropW/CropH` と `Pitch` で検査してから写す。
//! - ハードウェアの実装（`MFX_IMPL_TYPE_HARDWARE`）だけを選ぶ。ソフトウェア実装は列挙の段階で捨てる（ADR-0004）。
//!   初期化で `MFX_WRN_PARTIAL_ACCELERATION`（一部をソフトウェアで処理）が返ったら使わない。
//!
//! GPU の無い開発機では読み込みと「実装が 0 個」までしか確かめられない。符号化・復号の経路は未検証。

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
#[path = "bindings/vpl.rs"]
pub(super) mod sys;

use crate::dl::{Library, LibrarySearch};
use crate::error::{CodecError, Result};
use crate::frame::{PixelFormat, VideoFrame};
use std::sync::Arc;
use sys::*;

/// 読み込む soname。
pub const LIBRARY_NAMES: &[&str] = &["libvpl.so.2"];

type FnLoad = unsafe extern "C" fn() -> mfxLoader;
type FnUnload = unsafe extern "C" fn(mfxLoader);
type FnCreateConfig = unsafe extern "C" fn(mfxLoader) -> mfxConfig;
type FnSetConfigFilterProperty =
    unsafe extern "C" fn(mfxConfig, *const mfxU8, mfxVariant) -> mfxStatus;
type FnEnumImplementations =
    unsafe extern "C" fn(mfxLoader, mfxU32, mfxImplCapsDeliveryFormat, *mut mfxHDL) -> mfxStatus;
type FnCreateSession = unsafe extern "C" fn(mfxLoader, mfxU32, *mut mfxSession) -> mfxStatus;
type FnDispReleaseImplDescription = unsafe extern "C" fn(mfxLoader, mfxHDL) -> mfxStatus;
type FnSessionOnly = unsafe extern "C" fn(mfxSession) -> mfxStatus;
type FnSync = unsafe extern "C" fn(mfxSession, mfxSyncPoint, mfxU32) -> mfxStatus;
type FnParam = unsafe extern "C" fn(mfxSession, *mut mfxVideoParam) -> mfxStatus;
type FnGetSurface = unsafe extern "C" fn(mfxSession, *mut *mut mfxFrameSurface1) -> mfxStatus;
type FnEncodeFrameAsync = unsafe extern "C" fn(
    mfxSession,
    *mut mfxEncodeCtrl,
    *mut mfxFrameSurface1,
    *mut mfxBitstream,
    *mut mfxSyncPoint,
) -> mfxStatus;
type FnDecodeHeader =
    unsafe extern "C" fn(mfxSession, *mut mfxBitstream, *mut mfxVideoParam) -> mfxStatus;
type FnDecodeFrameAsync = unsafe extern "C" fn(
    mfxSession,
    *mut mfxBitstream,
    *mut mfxFrameSurface1,
    *mut *mut mfxFrameSurface1,
    *mut mfxSyncPoint,
) -> mfxStatus;

/// 読み込んだ libvpl と関数ポインター。
pub struct Vpl {
    lib: Arc<Library>,
    load: FnLoad,
    unload: FnUnload,
    create_config: FnCreateConfig,
    set_filter: FnSetConfigFilterProperty,
    enum_impls: FnEnumImplementations,
    create_session: FnCreateSession,
    release_desc: FnDispReleaseImplDescription,
    close: FnSessionOnly,
    sync: FnSync,
    enc_init: FnParam,
    enc_get_param: FnParam,
    enc_close: FnSessionOnly,
    enc_frame: FnEncodeFrameAsync,
    get_surface_enc: FnGetSurface,
    dec_header: FnDecodeHeader,
    dec_init: FnParam,
    dec_close: FnSessionOnly,
    dec_frame: FnDecodeFrameAsync,
}

impl std::fmt::Debug for Vpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vpl").field("lib", &self.lib.name).finish()
    }
}

impl Vpl {
    /// ライブラリを読み込み、使う関数をすべて取り出す。1 つでも無ければ `Err`（古すぎる dispatcher）。
    pub fn load(search: &LibrarySearch) -> std::result::Result<Arc<Vpl>, String> {
        let lib = search.open(LIBRARY_NAMES)?;
        // SAFETY: 各型は oneVPL 2.16 のヘッダー（mfxdispatcher.h・mfxsession.h・mfxvideo.h・mfxmemory.h）の
        // 宣言と同じ引数・戻り値の `extern "C"` 関数ポインター。`lib` は `Vpl` が保持する。
        unsafe {
            Ok(Arc::new(Vpl {
                load: lib.sym("MFXLoad")?,
                unload: lib.sym("MFXUnload")?,
                create_config: lib.sym("MFXCreateConfig")?,
                set_filter: lib.sym("MFXSetConfigFilterProperty")?,
                enum_impls: lib.sym("MFXEnumImplementations")?,
                create_session: lib.sym("MFXCreateSession")?,
                release_desc: lib.sym("MFXDispReleaseImplDescription")?,
                close: lib.sym("MFXClose")?,
                sync: lib.sym("MFXVideoCORE_SyncOperation")?,
                enc_init: lib.sym("MFXVideoENCODE_Init")?,
                enc_get_param: lib.sym("MFXVideoENCODE_GetVideoParam")?,
                enc_close: lib.sym("MFXVideoENCODE_Close")?,
                enc_frame: lib.sym("MFXVideoENCODE_EncodeFrameAsync")?,
                get_surface_enc: lib.sym("MFXMemory_GetSurfaceForEncode")?,
                dec_header: lib.sym("MFXVideoDECODE_DecodeHeader")?,
                dec_init: lib.sym("MFXVideoDECODE_Init")?,
                dec_close: lib.sym("MFXVideoDECODE_Close")?,
                dec_frame: lib.sym("MFXVideoDECODE_DecodeFrameAsync")?,
                lib,
            }))
        }
    }

    pub fn library_name(&self) -> &str {
        &self.lib.name
    }

    /// ハードウェアの絞り込みを付けたローダーを作る。
    fn hardware_loader(&self) -> std::result::Result<mfxLoader, String> {
        // SAFETY: MFXLoad は引数を取らず、失敗時は NULL を返す。
        let loader = unsafe { (self.load)() };
        if loader.is_null() {
            return Err("MFXLoad failed".into());
        }
        // SAFETY: loader は直前に作った有効なハンドル。
        let cfg = unsafe { (self.create_config)(loader) };
        if cfg.is_null() {
            // SAFETY: 有効な loader を 1 回だけ解放する。
            unsafe { (self.unload)(loader) };
            return Err("MFXCreateConfig failed".into());
        }
        // SAFETY: mfxVariant は POD（全 0 が有効な値）。
        let mut v: mfxVariant = unsafe { std::mem::zeroed() };
        v.Version.Version = 256 + 1; // MFX_VARIANT_VERSION = MFX_STRUCT_VERSION(1, 1)
        v.Type = MFX_VARIANT_TYPE_U32;
        v.Data.U32 = MFX_IMPL_TYPE_HARDWARE;
        // SAFETY: 名前は NUL 終端の静的文字列、cfg は有効、v は値渡し。
        let st = unsafe { (self.set_filter)(cfg, c"mfxImplDescription.Impl".as_ptr().cast(), v) };
        if st != MFX_ERR_NONE {
            // SAFETY: 有効な loader を 1 回だけ解放する（cfg は loader と一緒に解放される）。
            unsafe { (self.unload)(loader) };
            return Err(format!(
                "MFXSetConfigFilterProperty failed: {}",
                status_name(st)
            ));
        }
        Ok(loader)
    }

    /// ハードウェアの実装を列挙する（GPU が無ければ空）。
    pub fn enumerate_hardware(&self) -> std::result::Result<Vec<ImplInfo>, String> {
        let loader = self.hardware_loader()?;
        let mut out = Vec::new();
        for i in 0..16u32 {
            let mut hdl: mfxHDL = std::ptr::null_mut();
            // SAFETY: loader は有効。hdl は書き込み先。
            let st =
                unsafe { (self.enum_impls)(loader, i, MFX_IMPLCAPS_IMPLDESCSTRUCTURE, &mut hdl) };
            if st != MFX_ERR_NONE || hdl.is_null() {
                break;
            }
            // SAFETY: MFX_IMPLCAPS_IMPLDESCSTRUCTURE を指定したので hdl は mfxImplDescription を指し、
            // MFXDispReleaseImplDescription を呼ぶまで有効。中の配列は Num* の個数だけ有効（read_desc で検査）。
            let info = unsafe { read_desc(i, &*(hdl as *const mfxImplDescription)) };
            // SAFETY: EnumImplementations が返したハンドルを 1 回だけ返す。
            unsafe { (self.release_desc)(loader, hdl) };
            if let Some(info) = info {
                out.push(info);
            }
        }
        // SAFETY: 有効な loader を 1 回だけ解放する。
        unsafe { (self.unload)(loader) };
        Ok(out)
    }
}

/// 列挙した実装の情報（Rust の値に写したもの）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplInfo {
    /// MFXEnumImplementations の番号（MFXCreateSession に渡す）。
    pub index: u32,
    pub name: String,
    pub api_version: (u16, u16),
    pub vendor_id: u32,
    pub device_id: String,
    pub decoders: Vec<CodecRange>,
    pub encoders: Vec<CodecRange>,
}

/// 1 つのコーデックの範囲。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecRange {
    pub codec_id: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub color_formats: Vec<u32>,
}

/// 列挙の上限（壊れた記述で巨大なループにしない）。
const MAX_DESC_ITEMS: usize = 256;

/// # Safety
///
/// `d` は dispatcher が返した有効な mfxImplDescription で、中のポインターは `Num*` の個数の要素を指す。
unsafe fn read_desc(index: u32, d: &mfxImplDescription) -> Option<ImplInfo> {
    if d.Impl != MFX_IMPL_TYPE_HARDWARE {
        return None;
    }
    let name = chars(&d.ImplName);
    let device_id = chars(&d.Dev.DeviceID);
    // SAFETY: mfxVersion は {Minor, Major} の構造体と u32 の共用体。どちらで読んでも有効。
    let ver = unsafe { d.ApiVersion.__bindgen_anon_1 };
    let mut decoders = Vec::new();
    // SAFETY: Codecs は NumCodecs 個（関数の前提）。各 Profiles・MemDesc・ColorFormats も同様。
    unsafe {
        for c in slice(d.Dec.Codecs, d.Dec.NumCodecs) {
            let mut r = CodecRange {
                codec_id: c.CodecID,
                max_width: 0,
                max_height: 0,
                color_formats: vec![],
            };
            for p in slice(c.Profiles, c.NumProfiles) {
                for m in slice(p.MemDesc, p.NumMemTypes) {
                    r.max_width = r.max_width.max(m.Width.Max);
                    r.max_height = r.max_height.max(m.Height.Max);
                    for f in slice(m.ColorFormats, m.NumColorFormats) {
                        if !r.color_formats.contains(f) {
                            r.color_formats.push(*f);
                        }
                    }
                }
            }
            decoders.push(r);
        }
    }
    let mut encoders = Vec::new();
    // SAFETY: 同上（Enc の記述）。
    unsafe {
        for c in slice(d.Enc.Codecs, d.Enc.NumCodecs) {
            let mut r = CodecRange {
                codec_id: c.CodecID,
                max_width: 0,
                max_height: 0,
                color_formats: vec![],
            };
            for p in slice(c.Profiles, c.NumProfiles) {
                for m in slice(p.MemDesc, p.NumMemTypes) {
                    r.max_width = r.max_width.max(m.Width.Max);
                    r.max_height = r.max_height.max(m.Height.Max);
                    for f in slice(m.ColorFormats, m.NumColorFormats) {
                        if !r.color_formats.contains(f) {
                            r.color_formats.push(*f);
                        }
                    }
                }
            }
            encoders.push(r);
        }
    }
    Some(ImplInfo {
        index,
        name,
        api_version: (ver.Major, ver.Minor),
        vendor_id: d.VendorID,
        device_id,
        decoders,
        encoders,
    })
}

/// # Safety
///
/// `p` が NULL でなければ `n` 個の `T` を指す。
unsafe fn slice<'a, T>(p: *const T, n: u16) -> &'a [T] {
    if p.is_null() || n == 0 {
        &[]
    } else {
        // SAFETY: 関数の前提。個数は上限で切る（壊れた記述への備え。上限を超える分は読まない）。
        unsafe { std::slice::from_raw_parts(p, (n as usize).min(MAX_DESC_ITEMS)) }
    }
}

fn chars(c: &[mfxChar]) -> String {
    let bytes: Vec<u8> = c.iter().map(|&b| b as u8).collect();
    crate::providers::common::c_chars_to_string(&bytes)
}

/// 状態コードの名前（ログ・エラー用）。
pub fn status_name(st: mfxStatus) -> String {
    let s = match st {
        MFX_ERR_NONE => "MFX_ERR_NONE",
        MFX_ERR_UNKNOWN => "MFX_ERR_UNKNOWN",
        MFX_ERR_NULL_PTR => "MFX_ERR_NULL_PTR",
        MFX_ERR_UNSUPPORTED => "MFX_ERR_UNSUPPORTED",
        MFX_ERR_MEMORY_ALLOC => "MFX_ERR_MEMORY_ALLOC",
        MFX_ERR_NOT_ENOUGH_BUFFER => "MFX_ERR_NOT_ENOUGH_BUFFER",
        MFX_ERR_INVALID_HANDLE => "MFX_ERR_INVALID_HANDLE",
        MFX_ERR_NOT_INITIALIZED => "MFX_ERR_NOT_INITIALIZED",
        MFX_ERR_NOT_FOUND => "MFX_ERR_NOT_FOUND",
        MFX_ERR_MORE_DATA => "MFX_ERR_MORE_DATA",
        MFX_ERR_MORE_SURFACE => "MFX_ERR_MORE_SURFACE",
        MFX_ERR_DEVICE_LOST => "MFX_ERR_DEVICE_LOST",
        MFX_ERR_INCOMPATIBLE_VIDEO_PARAM => "MFX_ERR_INCOMPATIBLE_VIDEO_PARAM",
        MFX_ERR_INVALID_VIDEO_PARAM => "MFX_ERR_INVALID_VIDEO_PARAM",
        MFX_ERR_DEVICE_FAILED => "MFX_ERR_DEVICE_FAILED",
        MFX_ERR_GPU_HANG => "MFX_ERR_GPU_HANG",
        MFX_ERR_NOT_IMPLEMENTED => "MFX_ERR_NOT_IMPLEMENTED",
        MFX_WRN_DEVICE_BUSY => "MFX_WRN_DEVICE_BUSY",
        MFX_WRN_VIDEO_PARAM_CHANGED => "MFX_WRN_VIDEO_PARAM_CHANGED",
        MFX_WRN_PARTIAL_ACCELERATION => "MFX_WRN_PARTIAL_ACCELERATION",
        MFX_WRN_INCOMPATIBLE_VIDEO_PARAM => "MFX_WRN_INCOMPATIBLE_VIDEO_PARAM",
        _ => return format!("mfxStatus({st})"),
    };
    s.to_string()
}

fn check(st: mfxStatus, what: &str) -> Result<()> {
    if st < MFX_ERR_NONE {
        let msg = format!("{what}: {}", status_name(st));
        return Err(match st {
            MFX_ERR_UNSUPPORTED
            | MFX_ERR_INVALID_VIDEO_PARAM
            | MFX_ERR_INCOMPATIBLE_VIDEO_PARAM => CodecError::invalid_config(msg),
            MFX_ERR_MEMORY_ALLOC
            | MFX_ERR_DEVICE_LOST
            | MFX_ERR_DEVICE_FAILED
            | MFX_ERR_GPU_HANG => CodecError::unavailable(msg),
            _ => CodecError::backend(msg),
        });
    }
    if st == MFX_WRN_PARTIAL_ACCELERATION {
        return Err(CodecError::unavailable(format!(
            "{what}: implementation reported partial (software) acceleration; refused by policy"
        )));
    }
    Ok(())
}

/// 「装置が忙しい」を待つ回数（1 ms ずつ、合計約 10 秒）。
const BUSY_RETRIES: u32 = 10_000;
/// SyncOperation の待ち時間（ms）。
const SYNC_TIMEOUT_MS: u32 = 60_000;

/// ローダーとセッション。
struct Session {
    vpl: Arc<Vpl>,
    loader: mfxLoader,
    session: mfxSession,
}

// SAFETY: mfxSession は 1 つのスレッドから順に使う限り、別のスレッドへ移してよい（VPL はセッション単位で
// スレッド安全ではないが、スレッドに結び付いた状態は持たない）。`Session` は `Sync` にしない。
unsafe impl Send for Session {}

impl Session {
    fn open(vpl: Arc<Vpl>, impl_index: u32) -> Result<Session> {
        let loader = vpl.hardware_loader().map_err(CodecError::unavailable)?;
        let mut session: mfxSession = std::ptr::null_mut();
        // SAFETY: loader は有効、session は書き込み先。
        let st = unsafe { (vpl.create_session)(loader, impl_index, &mut session) };
        if st != MFX_ERR_NONE || session.is_null() {
            // SAFETY: 有効な loader を 1 回だけ解放する。
            unsafe { (vpl.unload)(loader) };
            return Err(CodecError::unavailable(format!(
                "MFXCreateSession failed: {}",
                status_name(st)
            )));
        }
        Ok(Session {
            vpl,
            loader,
            session,
        })
    }

    fn sync(&self, sp: mfxSyncPoint) -> Result<()> {
        // SAFETY: session は有効、sp は同じセッションの非同期処理が返したもの。
        let st = unsafe { (self.vpl.sync)(self.session, sp, SYNC_TIMEOUT_MS) };
        check(st, "MFXVideoCORE_SyncOperation")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: 有効な session と loader を、この順で 1 回だけ閉じる。
        unsafe {
            (self.vpl.close)(self.session);
            (self.vpl.unload)(self.loader);
        }
    }
}

/// 符号化の設定（`mfxVideoParam` に写す値。計算は mod.rs で行い試験する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncParams {
    pub codec_id: u32,
    pub profile: u16,
    pub target_usage: u16,
    pub rate_control: u16,
    pub brc_multiplier: u16,
    pub target_kbps: u16,
    pub max_kbps: u16,
    pub qp: u16,
    pub gop_pic_size: u16,
    pub gop_ref_dist: u16,
    pub idr_interval: u16,
    pub width: u16,
    pub height: u16,
    pub crop_w: u16,
    pub crop_h: u16,
    pub fps_n: u32,
    pub fps_d: u32,
    pub ten_bit: bool,
}

/// 符号化の出力 1 つ。
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    /// 90 kHz。
    pub pts90: i64,
    pub dts90: i64,
    pub frame_type: u16,
}

/// 符号化のセッション。
pub struct EncodeSession {
    s: Session,
    bs_buf: Vec<u8>,
    params: EncParams,
}

impl EncodeSession {
    pub fn new(vpl: Arc<Vpl>, impl_index: u32, p: &EncParams) -> Result<EncodeSession> {
        let s = Session::open(vpl, impl_index)?;
        // SAFETY: mfxVideoParam は POD（全 0 は「既定値」の意味）。共用体は mfx（符号化・復号）側を書く。
        let mut par: mfxVideoParam = unsafe { std::mem::zeroed() };
        par.AsyncDepth = 1;
        par.IOPattern = MFX_IOPATTERN_IN_SYSTEM_MEMORY as u16;
        // SAFETY: 共用体のフィールドへの書き込み（読み出しではない）。
        unsafe {
            let m = &mut par.__bindgen_anon_1.mfx;
            m.CodecId = p.codec_id;
            m.CodecProfile = p.profile;
            m.BRCParamMultiplier = p.brc_multiplier;
            let e = &mut m.__bindgen_anon_1.__bindgen_anon_1;
            e.TargetUsage = p.target_usage;
            e.GopPicSize = p.gop_pic_size;
            e.GopRefDist = p.gop_ref_dist;
            e.IdrInterval = p.idr_interval;
            e.RateControlMethod = p.rate_control;
            if p.rate_control == MFX_RATECONTROL_CQP as u16 {
                e.__bindgen_anon_1.QPI = p.qp;
                e.__bindgen_anon_2.QPP = p.qp;
                e.__bindgen_anon_3.QPB = p.qp;
            } else {
                e.__bindgen_anon_1.InitialDelayInKB = 0;
                e.__bindgen_anon_2.TargetKbps = p.target_kbps;
                e.__bindgen_anon_3.MaxKbps = p.max_kbps;
            }
            fill_frame_info(&mut m.FrameInfo, p);
        }
        // SAFETY: session は有効、par は初期化済み。
        let st = unsafe { (s.vpl.enc_init)(s.session, &mut par) };
        check(st, "MFXVideoENCODE_Init")?;
        // SAFETY: 同上。
        let st = unsafe { (s.vpl.enc_get_param)(s.session, &mut par) };
        check(st, "MFXVideoENCODE_GetVideoParam")?;
        // SAFETY: GetVideoParam が mfx 側を埋めた。
        let (kb, mult) = unsafe {
            let m = &par.__bindgen_anon_1.mfx;
            (
                m.__bindgen_anon_1.__bindgen_anon_1.BufferSizeInKB as usize,
                m.BRCParamMultiplier.max(1) as usize,
            )
        };
        let fallback = p.width as usize * p.height as usize * 3;
        let size = (kb * 1000 * mult).max(fallback).min(256 << 20);
        Ok(EncodeSession {
            s,
            bs_buf: vec![0; size],
            params: *p,
        })
    }

    /// フレームを 1 つ入れる（`None` は出し切り）。出力があれば返す。出し切りで出力が無ければ `Ok(None)`。
    pub fn encode(
        &mut self,
        frame: Option<(&VideoFrame, i64)>,
        force_key: bool,
    ) -> Result<Option<EncodedFrame>> {
        let mut surf: *mut mfxFrameSurface1 = std::ptr::null_mut();
        if let Some((f, pts90)) = frame {
            // SAFETY: session は有効、surf は書き込み先。
            let st = unsafe { (self.s.vpl.get_surface_enc)(self.s.session, &mut surf) };
            check(st, "MFXMemory_GetSurfaceForEncode")?;
            if surf.is_null() {
                return Err(CodecError::backend(
                    "MFXMemory_GetSurfaceForEncode returned NULL",
                ));
            }
            // SAFETY: surf は VPL が割り当てた有効な面。書き込みの前に Map、後で Unmap する。
            let r = unsafe { write_surface(surf, f, &self.params, pts90) };
            if let Err(e) = r {
                // SAFETY: GetSurfaceForEncode で得た参照を 1 回だけ返す。
                unsafe { release_surface(surf) };
                return Err(e);
            }
        }
        // SAFETY: mfxEncodeCtrl・mfxBitstream は POD。
        let mut ctrl: mfxEncodeCtrl = unsafe { std::mem::zeroed() };
        ctrl.FrameType = (MFX_FRAMETYPE_I | MFX_FRAMETYPE_IDR | MFX_FRAMETYPE_REF) as u16;
        let ctrl_ptr: *mut mfxEncodeCtrl = if force_key && !surf.is_null() {
            &mut ctrl
        } else {
            std::ptr::null_mut()
        };
        let mut result = Ok(None);
        let mut tries = 0;
        loop {
            // SAFETY: 同上。
            let mut bs: mfxBitstream = unsafe { std::mem::zeroed() };
            bs.Data = self.bs_buf.as_mut_ptr();
            bs.MaxLength = self.bs_buf.len() as u32;
            let mut sp: mfxSyncPoint = std::ptr::null_mut();
            // SAFETY: session・surf（または NULL で出し切り）・bs（bs_buf を指す、MaxLength 以内）・sp は有効。
            // bs_buf は SyncOperation が終わるまで動かさない。
            let st =
                unsafe { (self.s.vpl.enc_frame)(self.s.session, ctrl_ptr, surf, &mut bs, &mut sp) };
            if st == MFX_WRN_DEVICE_BUSY {
                tries += 1;
                if tries > BUSY_RETRIES {
                    result = Err(CodecError::unavailable("encoder device busy"));
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            if st == MFX_ERR_NOT_ENOUGH_BUFFER && self.bs_buf.len() < (256 << 20) {
                let n = self.bs_buf.len() * 2;
                self.bs_buf.resize(n, 0);
                continue;
            }
            if st == MFX_ERR_MORE_DATA {
                break;
            }
            if let Err(e) = check(st, "MFXVideoENCODE_EncodeFrameAsync") {
                result = Err(e);
                break;
            }
            if !sp.is_null() {
                if let Err(e) = self.s.sync(sp) {
                    result = Err(e);
                    break;
                }
                let off = bs.DataOffset as usize;
                let len = bs.DataLength as usize;
                if off
                    .checked_add(len)
                    .is_none_or(|end| end > self.bs_buf.len())
                {
                    result = Err(CodecError::backend(
                        "encoder returned out-of-range bitstream",
                    ));
                    break;
                }
                result = Ok(Some(EncodedFrame {
                    data: self.bs_buf[off..off + len].to_vec(),
                    pts90: bs.TimeStamp as i64,
                    dts90: bs.DecodeTimeStamp,
                    frame_type: bs.FrameType,
                }));
            }
            break;
        }
        if !surf.is_null() {
            // SAFETY: エンコーダーは必要なら自分で参照を取っている。ここで得た参照を 1 回だけ返す。
            unsafe { release_surface(surf) };
        }
        result
    }
}

impl Drop for EncodeSession {
    fn drop(&mut self) {
        // SAFETY: 初期化済みのエンコーダーを閉じる（その後 Session の Drop が MFXClose する）。
        unsafe { (self.s.vpl.enc_close)(self.s.session) };
    }
}

/// # Safety
///
/// 共用体 `FrameInfo` の幅・高さ側に書き込む。
unsafe fn fill_frame_info(fi: &mut mfxFrameInfo, p: &EncParams) {
    fi.FourCC = if p.ten_bit {
        MFX_FOURCC_P010
    } else {
        MFX_FOURCC_NV12
    };
    fi.ChromaFormat = MFX_CHROMAFORMAT_YUV420 as u16;
    fi.PicStruct = MFX_PICSTRUCT_PROGRESSIVE as u16;
    fi.BitDepthLuma = if p.ten_bit { 10 } else { 8 };
    fi.BitDepthChroma = fi.BitDepthLuma;
    fi.Shift = if p.ten_bit { 1 } else { 0 };
    fi.FrameRateExtN = p.fps_n;
    fi.FrameRateExtD = p.fps_d;
    // SAFETY: 共用体への書き込み（関数の前提）。
    let r = unsafe { &mut fi.__bindgen_anon_1.__bindgen_anon_1 };
    r.Width = p.width;
    r.Height = p.height;
    r.CropX = 0;
    r.CropY = 0;
    r.CropW = p.crop_w;
    r.CropH = p.crop_h;
}

/// # Safety
///
/// `surf` は VPL が返した有効な面で、FrameInterface を持つ。
unsafe fn release_surface(surf: *mut mfxFrameSurface1) {
    // SAFETY: 関数の前提。
    unsafe {
        let fi = (*surf).__bindgen_anon_1.FrameInterface;
        if !fi.is_null()
            && let Some(rel) = (*fi).Release
        {
            rel(surf);
        }
    }
}

fn pitch(d: &mfxFrameData) -> usize {
    // SAFETY: Pitch と PitchLow は同じ u16 の別名（共用体）。
    let low = unsafe { d.__bindgen_anon_2.PitchLow } as usize;
    ((d.PitchHigh as usize) << 16) | low
}

/// # Safety
///
/// `surf` は VPL が返した有効な面（NV12 または P010）。
unsafe fn write_surface(
    surf: *mut mfxFrameSurface1,
    f: &VideoFrame,
    p: &EncParams,
    pts90: i64,
) -> Result<()> {
    let want = if p.ten_bit {
        PixelFormat::P010
    } else {
        PixelFormat::Nv12
    };
    if f.format != want {
        return Err(CodecError::internal(
            "frame format does not match encoder surface",
        ));
    }
    // SAFETY: 関数の前提。FrameInterface の関数で Map してから Data のポインターを使う。
    unsafe {
        let fi = (*surf).__bindgen_anon_1.FrameInterface;
        if fi.is_null() {
            return Err(CodecError::backend("surface without FrameInterface"));
        }
        let (Some(map), Some(unmap)) = ((*fi).Map, (*fi).Unmap) else {
            return Err(CodecError::backend("surface without Map/Unmap"));
        };
        check(map(surf, MFX_MAP_WRITE), "surface Map")?;
        let d = &mut (*surf).Data;
        let pitch = pitch(d);
        let y = d.__bindgen_anon_3.Y;
        let uv = d.__bindgen_anon_4.UV;
        let rows_y = f.height as usize;
        let rows_uv = (f.height as usize).div_ceil(2);
        let (row_y, _) = f.format.plane_size(f.width, f.height, 0).unwrap_or((0, 0));
        let (row_uv, _) = f.format.plane_size(f.width, f.height, 1).unwrap_or((0, 0));
        // 面は (Width, Height)（16 の倍数に揃えた大きさ）以上ある。行の長さが pitch を超えないことを確かめる。
        let ok = !y.is_null()
            && !uv.is_null()
            && row_y <= pitch
            && row_uv <= pitch
            && rows_y <= p.height as usize
            && f.width <= p.width as u32;
        if ok {
            for r in 0..rows_y {
                std::ptr::copy_nonoverlapping(f.row(0, r).as_ptr(), y.add(r * pitch), row_y);
            }
            for r in 0..rows_uv {
                std::ptr::copy_nonoverlapping(f.row(1, r).as_ptr(), uv.add(r * pitch), row_uv);
            }
        }
        d.TimeStamp = pts90 as u64;
        d.FrameOrder = 0;
        unmap(surf);
        if !ok {
            return Err(CodecError::backend(
                "encoder surface layout is smaller than the frame",
            ));
        }
    }
    Ok(())
}

/// 復号した 1 フレーム（詰めた NV12 / P010）。
#[derive(Debug, Clone)]
pub struct DecodedSurface {
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub pts90: i64,
}

/// 入力をためるバッファの上限。
const MAX_PENDING: usize = 80 << 20;

/// 復号のセッション。
pub struct DecodeSession {
    s: Session,
    codec_id: u32,
    buf: Vec<u8>,
    initialized: bool,
    max_w: u32,
    max_h: u32,
}

impl DecodeSession {
    pub fn new(
        vpl: Arc<Vpl>,
        impl_index: u32,
        codec_id: u32,
        max_w: u32,
        max_h: u32,
    ) -> Result<DecodeSession> {
        let s = Session::open(vpl, impl_index)?;
        Ok(DecodeSession {
            s,
            codec_id,
            buf: Vec::new(),
            initialized: false,
            max_w,
            max_h,
        })
    }

    fn bitstream(&mut self, pts90: i64, eos: bool) -> mfxBitstream {
        // SAFETY: mfxBitstream は POD。
        let mut bs: mfxBitstream = unsafe { std::mem::zeroed() };
        bs.Data = self.buf.as_mut_ptr();
        bs.DataLength = self.buf.len() as u32;
        bs.MaxLength = self.buf.len() as u32;
        bs.TimeStamp = pts90 as u64;
        bs.DataFlag =
            (MFX_BITSTREAM_COMPLETE_FRAME | if eos { MFX_BITSTREAM_EOS } else { 0 }) as u16;
        bs
    }

    /// 消費した分を buf から取り除く。
    fn consume(&mut self, bs: &mfxBitstream) -> Result<()> {
        let off = bs.DataOffset as usize;
        let len = bs.DataLength as usize;
        if off.checked_add(len).is_none_or(|e| e > self.buf.len()) {
            return Err(CodecError::backend(
                "decoder returned out-of-range bitstream offsets",
            ));
        }
        self.buf.drain(..off);
        self.buf.truncate(len);
        Ok(())
    }

    fn init(&mut self, pts90: i64) -> Result<bool> {
        // SAFETY: mfxVideoParam は POD。共用体は mfx 側を使う。
        let mut par: mfxVideoParam = unsafe { std::mem::zeroed() };
        // 共用体への書き込み（Copy な型への代入は安全）。
        par.__bindgen_anon_1.mfx.CodecId = self.codec_id;
        let mut bs = self.bitstream(pts90, false);
        // SAFETY: session・bs（buf を指す）・par は有効。
        let st = unsafe { (self.s.vpl.dec_header)(self.s.session, &mut bs, &mut par) };
        self.consume(&bs)?;
        if st == MFX_ERR_MORE_DATA {
            return Ok(false);
        }
        check(st, "MFXVideoDECODE_DecodeHeader")?;
        // SAFETY: DecodeHeader が mfx 側を埋めた。
        let (cw, ch) = unsafe {
            let r = &par
                .__bindgen_anon_1
                .mfx
                .FrameInfo
                .__bindgen_anon_1
                .__bindgen_anon_1;
            (r.CropW as u32, r.CropH as u32)
        };
        if cw == 0 || ch == 0 || cw > self.max_w || ch > self.max_h {
            return Err(CodecError::invalid_input(format!(
                "stream resolution {cw}x{ch} outside allowed range"
            )));
        }
        par.IOPattern = MFX_IOPATTERN_OUT_SYSTEM_MEMORY as u16;
        par.AsyncDepth = 1;
        // SAFETY: session・par は有効。
        let st = unsafe { (self.s.vpl.dec_init)(self.s.session, &mut par) };
        check(st, "MFXVideoDECODE_Init")?;
        self.initialized = true;
        Ok(true)
    }

    /// AU（Annex B / OBU / VP9 フレーム）を 1 つ入れ、出たフレームを返す。`data = None` は出し切り。
    pub fn decode(&mut self, data: Option<&[u8]>, pts90: i64) -> Result<Vec<DecodedSurface>> {
        let eos = data.is_none();
        if let Some(d) = data {
            if self.buf.len() + d.len() > MAX_PENDING {
                return Err(CodecError::invalid_input("decoder input backlog too large"));
            }
            self.buf.extend_from_slice(d);
        }
        let mut out = Vec::new();
        if !self.initialized && !self.init(pts90)? {
            return Ok(out);
        }
        let mut tries = 0;
        let mut reinit = 0;
        let mut iterations = 0u32;
        loop {
            iterations += 1;
            if iterations > BUSY_RETRIES {
                return Err(CodecError::backend("decoder made no progress"));
            }
            let mut bs = self.bitstream(pts90, eos);
            let bs_ptr: *mut mfxBitstream = if eos && self.buf.is_empty() {
                std::ptr::null_mut()
            } else {
                &mut bs
            };
            let mut surf_out: *mut mfxFrameSurface1 = std::ptr::null_mut();
            let mut sp: mfxSyncPoint = std::ptr::null_mut();
            // SAFETY: session は有効。bs は buf を指す（または NULL で出し切り）。surface_work は NULL（VPL 2.x の内部割り当て）。
            let st = unsafe {
                (self.s.vpl.dec_frame)(
                    self.s.session,
                    bs_ptr,
                    std::ptr::null_mut(),
                    &mut surf_out,
                    &mut sp,
                )
            };
            if !bs_ptr.is_null() {
                self.consume(&bs)?;
            }
            match st {
                MFX_WRN_DEVICE_BUSY => {
                    tries += 1;
                    if tries > BUSY_RETRIES {
                        return Err(CodecError::unavailable("decoder device busy"));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                MFX_ERR_MORE_DATA => break,
                MFX_WRN_VIDEO_PARAM_CHANGED | MFX_ERR_MORE_SURFACE if sp.is_null() => continue,
                MFX_ERR_INCOMPATIBLE_VIDEO_PARAM => {
                    reinit += 1;
                    if reinit > 2 {
                        return Err(CodecError::backend("decoder re-initialization loop"));
                    }
                    // 新しい系列: 閉じてヘッダーから読み直す（残りの出力は捨てる）。
                    // SAFETY: 初期化済みのデコーダーを閉じる。
                    unsafe { (self.s.vpl.dec_close)(self.s.session) };
                    self.initialized = false;
                    if !self.init(pts90)? {
                        break;
                    }
                    continue;
                }
                _ => {}
            }
            check(st, "MFXVideoDECODE_DecodeFrameAsync")?;
            if !sp.is_null() && !surf_out.is_null() {
                let r = self.s.sync(sp).and_then(|_| {
                    // SAFETY: surf_out は同期の済んだ出力面。読み終えたら参照を返す。
                    unsafe { read_surface(surf_out, self.max_w, self.max_h) }
                });
                // SAFETY: DecodeFrameAsync が渡した参照を 1 回だけ返す。
                unsafe { release_surface(surf_out) };
                out.push(r?);
                if out.len() > 64 {
                    return Err(CodecError::backend(
                        "decoder produced too many frames for one input",
                    ));
                }
            }
            if !eos && self.buf.is_empty() {
                break;
            }
        }
        Ok(out)
    }
}

impl Drop for DecodeSession {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: 初期化済みのデコーダーを閉じる。
            unsafe { (self.s.vpl.dec_close)(self.s.session) };
        }
    }
}

/// # Safety
///
/// `surf` は同期済みの有効な出力面。
unsafe fn read_surface(
    surf: *mut mfxFrameSurface1,
    max_w: u32,
    max_h: u32,
) -> Result<DecodedSurface> {
    // SAFETY: 関数の前提。Map してから Data のポインターを読む。
    unsafe {
        let fi = (*surf).__bindgen_anon_1.FrameInterface;
        if fi.is_null() {
            return Err(CodecError::backend("surface without FrameInterface"));
        }
        let (Some(map), Some(unmap)) = ((*fi).Map, (*fi).Unmap) else {
            return Err(CodecError::backend("surface without Map/Unmap"));
        };
        check(map(surf, MFX_MAP_READ), "surface Map")?;
        let info = &(*surf).Info;
        let r = &info.__bindgen_anon_1.__bindgen_anon_1;
        let (fmt, bpp) = match info.FourCC {
            MFX_FOURCC_NV12 => (PixelFormat::Nv12, 1usize),
            MFX_FOURCC_P010 => (PixelFormat::P010, 2usize),
            other => {
                unmap(surf);
                return Err(CodecError::unsupported(format!(
                    "decoder output fourcc {other:#x}"
                )));
            }
        };
        let (w, h) = (r.CropW as u32, r.CropH as u32);
        let (cx, cy) = (r.CropX as usize & !1, r.CropY as usize & !1);
        let d = &(*surf).Data;
        let pitch = pitch(d);
        let y = d.__bindgen_anon_3.Y;
        let uv = d.__bindgen_anon_4.UV;
        let row_y = w as usize * bpp;
        let row_uv = (w as usize).div_ceil(2) * 2 * bpp;
        let rows_uv = (h as usize).div_ceil(2);
        let ok = w > 0
            && h > 0
            && w <= max_w
            && h <= max_h
            && !y.is_null()
            && !uv.is_null()
            && cx * bpp + row_y <= pitch
            && cx * bpp + row_uv <= pitch
            && cy + h as usize <= r.Height as usize;
        let mut data = Vec::new();
        if ok {
            data.reserve(fmt.packed_size(w, h));
            for row in 0..h as usize {
                let p = y.add((cy + row) * pitch + cx * bpp);
                data.extend_from_slice(std::slice::from_raw_parts(p, row_y));
            }
            for row in 0..rows_uv {
                let p = uv.add((cy / 2 + row) * pitch + cx * bpp);
                data.extend_from_slice(std::slice::from_raw_parts(p, row_uv));
            }
        }
        let pts90 = d.TimeStamp as i64;
        unmap(surf);
        if !ok {
            return Err(CodecError::invalid_input(format!(
                "decoded surface {w}x{h} rejected (layout or size limit)"
            )));
        }
        Ok(DecodedSurface {
            format: fmt,
            width: w,
            height: h,
            data,
            pts90,
        })
    }
}
