//! SVT-AV1 の FFI（`unsafe` はこのモジュールだけ）。
//!
//! 関数は `libSvtAv1Enc.so.2` から dlopen で取り出す。構造体は bindgen の [`bindings`]（SVT-AV1 2.3.0 のヘッダー）。
//! `EbSvtAv1EncConfiguration` の配置は主版ごとに変わるので、soname をヘッダーと同じ主版（.so.2）に限り、
//! さらに `svt_av1_get_version` の主版が 2 であることを確かめる。

#![allow(clippy::all)]

use crate::dl::{Library, LibrarySearch};
use std::ffi::{CStr, c_char, c_void};
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

use bindings::{
    EbBufferHeaderType, EbComponentType, EbErrorType, EbSvtAv1EncConfiguration, EbSvtIOFormat,
};

pub const LIB_NAMES: &[&str] = &["libSvtAv1Enc.so.2"];

/// SVT-AV1 の初期化・終了はプロセス全体の表（CPU 機能に応じた関数ポインターの表など）を書き換えるため、
/// 同じプロセスで複数のインスタンスを同時に初期化・終了すると落ちることがある（この試験機で再現）。
/// 初期化と終了をこの鍵で直列にする。codec worker は 1 プロセス 1 ジョブなので通常は競合しない。
static LIFECYCLE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lifecycle_lock() -> std::sync::MutexGuard<'static, ()> {
    LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner())
}

type GetVersionFn = unsafe extern "C" fn() -> *const c_char;
type InitHandleFn = unsafe extern "C" fn(
    *mut *mut EbComponentType,
    *mut c_void,
    *mut EbSvtAv1EncConfiguration,
) -> EbErrorType;
type SetParameterFn =
    unsafe extern "C" fn(*mut EbComponentType, *mut EbSvtAv1EncConfiguration) -> EbErrorType;
type InitFn = unsafe extern "C" fn(*mut EbComponentType) -> EbErrorType;
type StreamHeaderFn =
    unsafe extern "C" fn(*mut EbComponentType, *mut *mut EbBufferHeaderType) -> EbErrorType;
type StreamHeaderReleaseFn = unsafe extern "C" fn(*mut EbBufferHeaderType) -> EbErrorType;
type SendPictureFn =
    unsafe extern "C" fn(*mut EbComponentType, *mut EbBufferHeaderType) -> EbErrorType;
type GetPacketFn =
    unsafe extern "C" fn(*mut EbComponentType, *mut *mut EbBufferHeaderType, u8) -> EbErrorType;
type ReleaseOutBufferFn = unsafe extern "C" fn(*mut *mut EbBufferHeaderType);
type DeinitFn = unsafe extern "C" fn(*mut EbComponentType) -> EbErrorType;

pub struct SvtLib {
    lib: Arc<Library>,
    get_version: GetVersionFn,
    init_handle: InitHandleFn,
    set_parameter: SetParameterFn,
    init: InitFn,
    stream_header: StreamHeaderFn,
    stream_header_release: StreamHeaderReleaseFn,
    send_picture: SendPictureFn,
    get_packet: GetPacketFn,
    release_out_buffer: ReleaseOutBufferFn,
    deinit: DeinitFn,
    deinit_handle: DeinitFn,
}

impl SvtLib {
    pub fn load(search: &LibrarySearch) -> Result<Arc<SvtLib>, String> {
        let lib = search.open(LIB_NAMES)?;
        // SAFETY: 型は EbSvtAv1Enc.h の宣言どおり。`lib` を保持する。
        let me = unsafe {
            SvtLib {
                get_version: lib.sym("svt_av1_get_version")?,
                init_handle: lib.sym("svt_av1_enc_init_handle")?,
                set_parameter: lib.sym("svt_av1_enc_set_parameter")?,
                init: lib.sym("svt_av1_enc_init")?,
                stream_header: lib.sym("svt_av1_enc_stream_header")?,
                stream_header_release: lib.sym("svt_av1_enc_stream_header_release")?,
                send_picture: lib.sym("svt_av1_enc_send_picture")?,
                get_packet: lib.sym("svt_av1_enc_get_packet")?,
                release_out_buffer: lib.sym("svt_av1_enc_release_out_buffer")?,
                deinit: lib.sym("svt_av1_enc_deinit")?,
                deinit_handle: lib.sym("svt_av1_enc_deinit_handle")?,
                lib,
            }
        };
        let v = me.version();
        let major = v
            .trim_start_matches('v')
            .split('.')
            .next()
            .and_then(|m| m.parse::<u32>().ok());
        if major != Some(2) {
            return Err(format!(
                "{}: SVT-AV1 {v} is not supported (need 2.x)",
                me.lib.name
            ));
        }
        Ok(Arc::new(me))
    }

    pub fn library_name(&self) -> &str {
        &self.lib.name
    }

    pub fn version(&self) -> String {
        // SAFETY: 静的な NUL 終端文字列。
        unsafe {
            let p = (self.get_version)();
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }
}

/// 設定（安全な側から渡す値）。
pub struct Params {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub preset: i8,
    pub intra_period: i32,
    pub low_delay: bool,
    /// 0 = CQP/CRF、1 = VBR、2 = CBR
    pub rc_mode: u32,
    pub qp: u32,
    pub target_bitrate: u32,
    pub threads: u32,
}

/// 出力パケット（コピー済み）。
pub struct Packet {
    pub data: Vec<u8>,
    pub pts: i64,
    pub keyframe: bool,
    pub eos: bool,
}

pub struct RawEncoder {
    lib: Arc<SvtLib>,
    handle: *mut EbComponentType,
    initialized: bool,
    width: u32,
    height: u32,
}

// SAFETY: SVT-AV1 の handle は内部でスレッドを持つが、API は 1 つのスレッドから順に呼べばよい。`&mut self` でだけ使う。
unsafe impl Send for RawEncoder {}

pub fn err_name(e: EbErrorType) -> String {
    match e {
        bindings::EbErrorType_EB_ErrorInsufficientResources => "insufficient resources".into(),
        bindings::EbErrorType_EB_ErrorBadParameter => "bad parameter".into(),
        bindings::EbErrorType_EB_ErrorUndefined => "undefined error".into(),
        bindings::EbErrorType_EB_ErrorInvalidComponent => "invalid component".into(),
        other => format!("error 0x{:x}", other as u32),
    }
}

impl RawEncoder {
    pub fn new(lib: Arc<SvtLib>, p: &Params) -> Result<RawEncoder, String> {
        let guard = lifecycle_lock();
        let r = Self::new_locked(lib, p);
        drop(guard);
        // 途中まで作ったものは鍵を放してから破棄する（Drop が同じ鍵を取るため）。
        r.map_err(|(e, partial)| {
            drop(partial);
            e
        })
    }

    #[allow(clippy::result_large_err)]
    fn new_locked(
        lib: Arc<SvtLib>,
        p: &Params,
    ) -> Result<RawEncoder, (String, Option<RawEncoder>)> {
        // SAFETY: 設定は POD。init_handle が既定値で埋める。
        let mut cfg: EbSvtAv1EncConfiguration = unsafe { MaybeUninit::zeroed().assume_init() };
        let mut handle: *mut EbComponentType = ptr::null_mut();
        let r = unsafe { (lib.init_handle)(&mut handle, ptr::null_mut(), &mut cfg) };
        if r != bindings::EbErrorType_EB_ErrorNone || handle.is_null() {
            return Err((format!("init_handle: {}", err_name(r)), None));
        }
        let mut enc = RawEncoder {
            lib,
            handle,
            initialized: false,
            width: p.width,
            height: p.height,
        };
        cfg.source_width = p.width;
        cfg.source_height = p.height;
        cfg.frame_rate_numerator = p.fps_num;
        cfg.frame_rate_denominator = p.fps_den;
        cfg.encoder_bit_depth = 8;
        cfg.encoder_color_format = bindings::EbColorFormat_EB_YUV420;
        cfg.enc_mode = p.preset;
        cfg.intra_period_length = p.intra_period;
        cfg.pred_structure = if p.low_delay {
            bindings::SvtAv1PredStructure_SVT_AV1_PRED_LOW_DELAY_B as u8
        } else {
            bindings::SvtAv1PredStructure_SVT_AV1_PRED_RANDOM_ACCESS as u8
        };
        cfg.rate_control_mode = p.rc_mode;
        if p.rc_mode == bindings::SvtAv1RcMode_SVT_AV1_RC_MODE_CQP_OR_CRF {
            cfg.qp = p.qp;
            cfg.enable_adaptive_quantization = 0; // CQP
        } else {
            cfg.target_bit_rate = p.target_bitrate;
        }
        // low delay では指定しなくても強制キーフレームが使える（指定すると警告が出る）。
        // random access では CQP のときだけ使える（VBR では SVT-AV1 が設定を拒否する）。
        cfg.force_key_frames =
            (!p.low_delay && p.rc_mode == bindings::SvtAv1RcMode_SVT_AV1_RC_MODE_CQP_OR_CRF) as u8;
        if p.low_delay {
            cfg.look_ahead_distance = 0;
        }
        if p.threads > 0 {
            cfg.logical_processors = p.threads;
        }
        // SAFETY: handle は init_handle で得たもの。cfg は呼び出しの間有効（ライブラリが中身を複製する）。
        let r = unsafe { (enc.lib.set_parameter)(enc.handle, &mut cfg) };
        if r != bindings::EbErrorType_EB_ErrorNone {
            return Err((format!("set_parameter: {}", err_name(r)), Some(enc)));
        }
        // SAFETY: 設定済みの handle を初期化する。
        let r = unsafe { (enc.lib.init)(enc.handle) };
        if r != bindings::EbErrorType_EB_ErrorNone {
            return Err((format!("init: {}", err_name(r)), Some(enc)));
        }
        enc.initialized = true;
        Ok(enc)
    }

    /// ストリームのヘッダー（Temporal Delimiter とシーケンスヘッダーの OBU）。
    pub fn stream_header(&mut self) -> Result<Vec<u8>, String> {
        let mut buf: *mut EbBufferHeaderType = ptr::null_mut();
        // SAFETY: 初期化済みの handle。返るバッファは stream_header_release で返す。
        let r = unsafe { (self.lib.stream_header)(self.handle, &mut buf) };
        if r != bindings::EbErrorType_EB_ErrorNone || buf.is_null() {
            return Err(format!("stream_header: {}", err_name(r)));
        }
        // SAFETY: buf は有効で、p_buffer は n_filled_len バイト読める。
        let data = unsafe {
            let b = &*buf;
            if b.p_buffer.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(b.p_buffer, b.n_filled_len as usize).to_vec()
            }
        };
        // SAFETY: 得たバッファを一度だけ返す。
        unsafe { (self.lib.stream_header_release)(buf) };
        Ok(data)
    }

    /// 8 bit I420 の 1 枚を送る。`planes`・`strides` は Y・U・V（stride はバイト = 画素）。
    pub fn send(
        &mut self,
        planes: [&[u8]; 3],
        strides: [usize; 3],
        pts: i64,
        force_key: bool,
    ) -> Result<(), String> {
        let (w, h) = (self.width as usize, self.height as usize);
        let dims = [
            (w, h),
            (w.div_ceil(2), h.div_ceil(2)),
            (w.div_ceil(2), h.div_ceil(2)),
        ];
        for k in 0..3 {
            let (pw, ph) = dims[k];
            if strides[k] < pw
                || strides[k] > u32::MAX as usize
                || planes[k].len() < strides[k] * (ph - 1) + pw
            {
                return Err("image plane too small".into());
            }
        }
        // SAFETY: EbSvtIOFormat・EbBufferHeaderType は POD。ライブラリは send_picture の中で入力を自分のバッファへ
        // 複製する（EbSvtAv1Enc.h: 呼び出しから戻れば入力バッファを再利用してよい）ので、借用中のスライスを指してよい。
        let mut io: EbSvtIOFormat = unsafe { MaybeUninit::zeroed().assume_init() };
        io.luma = planes[0].as_ptr() as *mut u8;
        io.cb = planes[1].as_ptr() as *mut u8;
        io.cr = planes[2].as_ptr() as *mut u8;
        io.y_stride = strides[0] as u32;
        io.cb_stride = strides[1] as u32;
        io.cr_stride = strides[2] as u32;
        io.width = self.width;
        io.height = self.height;
        io.color_fmt = bindings::EbColorFormat_EB_YUV420;
        io.bit_depth = bindings::EbBitDepth_EB_EIGHT_BIT;
        let mut hdr: EbBufferHeaderType = unsafe { MaybeUninit::zeroed().assume_init() };
        hdr.size = std::mem::size_of::<EbBufferHeaderType>() as u32;
        hdr.p_buffer = &mut io as *mut EbSvtIOFormat as *mut u8;
        hdr.n_filled_len =
            (planes[0].len() + planes[1].len() + planes[2].len()).min(u32::MAX as usize) as u32;
        hdr.n_alloc_len = hdr.n_filled_len;
        hdr.pts = pts;
        hdr.pic_type = if force_key {
            bindings::EbAv1PictureType_EB_AV1_KEY_PICTURE
        } else {
            bindings::EbAv1PictureType_EB_AV1_INVALID_PICTURE
        };
        // SAFETY: handle は初期化済み。hdr・io は呼び出しの間有効。
        let r = unsafe { (self.lib.send_picture)(self.handle, &mut hdr) };
        if r != bindings::EbErrorType_EB_ErrorNone {
            return Err(format!("send_picture: {}", err_name(r)));
        }
        Ok(())
    }

    /// 入力の終わりを送る。
    pub fn send_eos(&mut self) -> Result<(), String> {
        // SAFETY: POD。p_buffer = NULL と EOS フラグで終わりを知らせる（SVT-AV1 の約束）。
        let mut hdr: EbBufferHeaderType = unsafe { MaybeUninit::zeroed().assume_init() };
        hdr.size = std::mem::size_of::<EbBufferHeaderType>() as u32;
        hdr.flags = bindings::EB_BUFFERFLAG_EOS;
        hdr.pic_type = bindings::EbAv1PictureType_EB_AV1_INVALID_PICTURE;
        let r = unsafe { (self.lib.send_picture)(self.handle, &mut hdr) };
        if r != bindings::EbErrorType_EB_ErrorNone {
            return Err(format!("send eos: {}", err_name(r)));
        }
        Ok(())
    }

    /// パケットを 1 つ取り出す。`done` が真（EOS 送信後）ならパケットが出るまで待つ。無ければ `None`。
    pub fn get_packet(&mut self, done: bool) -> Result<Option<Packet>, String> {
        let mut buf: *mut EbBufferHeaderType = ptr::null_mut();
        // SAFETY: 初期化済みの handle。返るバッファは release_out_buffer で返す。
        let r = unsafe { (self.lib.get_packet)(self.handle, &mut buf, done as u8) };
        if r == bindings::EbErrorType_EB_NoErrorEmptyQueue {
            return Ok(None);
        }
        if r != bindings::EbErrorType_EB_ErrorNone || buf.is_null() {
            return Err(format!("get_packet: {}", err_name(r)));
        }
        // SAFETY: buf は有効。p_buffer は n_filled_len バイト読める。読んだ後で一度だけ返す。
        let pkt = unsafe {
            let b = &*buf;
            let data = if b.p_buffer.is_null() || b.n_filled_len == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(b.p_buffer, b.n_filled_len as usize).to_vec()
            };
            Packet {
                data,
                pts: b.pts,
                keyframe: b.pic_type == bindings::EbAv1PictureType_EB_AV1_KEY_PICTURE,
                eos: b.flags & bindings::EB_BUFFERFLAG_EOS != 0,
            }
        };
        unsafe { (self.lib.release_out_buffer)(&mut buf) };
        Ok(Some(pkt))
    }
}

impl Drop for RawEncoder {
    fn drop(&mut self) {
        let _guard = lifecycle_lock();
        // SAFETY: init に成功していれば deinit、handle は必ず deinit_handle で一度だけ解放する。
        unsafe {
            if self.initialized {
                (self.lib.deinit)(self.handle);
            }
            (self.lib.deinit_handle)(self.handle);
        }
    }
}
