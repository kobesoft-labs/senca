//! libvpx の FFI（`unsafe` はこのモジュールだけ）。
//!
//! 関数は `libvpx.so.12` から dlopen で取り出す。構造体は bindgen の [`bindings`]（libvpx 1.16.0 のヘッダー）。
//! 初期化では `VPX_ENCODER_ABI_VERSION`・`VPX_DECODER_ABI_VERSION` を渡すので、配置の違うライブラリは
//! libvpx 自身が `VPX_CODEC_ABI_MISMATCH` で拒否する。さらに soname をヘッダーと同じ版（.so.12）に限る。

#![allow(clippy::all)]

use crate::dl::{Library, LibrarySearch};
use std::ffi::{CStr, c_char, c_int, c_long, c_uint, c_ulong, c_void};
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
    vpx_codec_ctx_t, vpx_codec_cx_pkt, vpx_codec_dec_cfg, vpx_codec_enc_cfg, vpx_codec_err_t,
    vpx_codec_iface_t, vpx_codec_iter_t, vpx_image_t,
};

pub const LIB_NAMES: &[&str] = &["libvpx.so.12"];

type IfaceFn = unsafe extern "C" fn() -> *mut vpx_codec_iface_t;
type VersionStrFn = unsafe extern "C" fn() -> *const c_char;
type ErrToStringFn = unsafe extern "C" fn(vpx_codec_err_t) -> *const c_char;
type ErrorDetailFn = unsafe extern "C" fn(*const vpx_codec_ctx_t) -> *const c_char;
type DestroyFn = unsafe extern "C" fn(*mut vpx_codec_ctx_t) -> vpx_codec_err_t;
type ControlFn = unsafe extern "C" fn(*mut vpx_codec_ctx_t, c_int, ...) -> vpx_codec_err_t;
type EncConfigDefaultFn =
    unsafe extern "C" fn(*mut vpx_codec_iface_t, *mut vpx_codec_enc_cfg, c_uint) -> vpx_codec_err_t;
type EncInitFn = unsafe extern "C" fn(
    *mut vpx_codec_ctx_t,
    *mut vpx_codec_iface_t,
    *const vpx_codec_enc_cfg,
    c_long,
    c_int,
) -> vpx_codec_err_t;
type EncodeFn = unsafe extern "C" fn(
    *mut vpx_codec_ctx_t,
    *const vpx_image_t,
    i64,
    c_ulong,
    c_long,
    c_ulong,
) -> vpx_codec_err_t;
type GetCxDataFn =
    unsafe extern "C" fn(*mut vpx_codec_ctx_t, *mut vpx_codec_iter_t) -> *const vpx_codec_cx_pkt;
type DecInitFn = unsafe extern "C" fn(
    *mut vpx_codec_ctx_t,
    *mut vpx_codec_iface_t,
    *const vpx_codec_dec_cfg,
    c_long,
    c_int,
) -> vpx_codec_err_t;
type DecodeFn = unsafe extern "C" fn(
    *mut vpx_codec_ctx_t,
    *const u8,
    c_uint,
    *mut c_void,
    c_long,
) -> vpx_codec_err_t;
type GetFrameFn =
    unsafe extern "C" fn(*mut vpx_codec_ctx_t, *mut vpx_codec_iter_t) -> *mut vpx_image_t;

/// VP8 か VP9 か。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Vp8,
    Vp9,
}

pub struct VpxLib {
    lib: Arc<Library>,
    vp8_cx: Option<IfaceFn>,
    vp9_cx: Option<IfaceFn>,
    vp8_dx: Option<IfaceFn>,
    vp9_dx: Option<IfaceFn>,
    version_str: VersionStrFn,
    err_to_string: ErrToStringFn,
    error_detail: ErrorDetailFn,
    destroy: DestroyFn,
    control: ControlFn,
    enc_config_default: EncConfigDefaultFn,
    enc_init: EncInitFn,
    encode: EncodeFn,
    get_cx_data: GetCxDataFn,
    dec_init: DecInitFn,
    decode: DecodeFn,
    get_frame: GetFrameFn,
}

impl VpxLib {
    pub fn load(search: &LibrarySearch) -> Result<Arc<VpxLib>, String> {
        let lib = search.open(LIB_NAMES)?;
        // SAFETY: 型は vpx_codec.h・vpx_encoder.h・vpx_decoder.h・vp8cx.h・vp8dx.h の宣言どおり。
        // インターフェースの取得関数（vpx_codec_vp8_cx など）は、ビルド構成によって無いことがあるので Option にする。
        let me = unsafe {
            VpxLib {
                vp8_cx: lib.sym("vpx_codec_vp8_cx").ok(),
                vp9_cx: lib.sym("vpx_codec_vp9_cx").ok(),
                vp8_dx: lib.sym("vpx_codec_vp8_dx").ok(),
                vp9_dx: lib.sym("vpx_codec_vp9_dx").ok(),
                version_str: lib.sym("vpx_codec_version_str")?,
                err_to_string: lib.sym("vpx_codec_err_to_string")?,
                error_detail: lib.sym("vpx_codec_error_detail")?,
                destroy: lib.sym("vpx_codec_destroy")?,
                control: lib.sym("vpx_codec_control_")?,
                enc_config_default: lib.sym("vpx_codec_enc_config_default")?,
                enc_init: lib.sym("vpx_codec_enc_init_ver")?,
                encode: lib.sym("vpx_codec_encode")?,
                get_cx_data: lib.sym("vpx_codec_get_cx_data")?,
                dec_init: lib.sym("vpx_codec_dec_init_ver")?,
                decode: lib.sym("vpx_codec_decode")?,
                get_frame: lib.sym("vpx_codec_get_frame")?,
                lib,
            }
        };
        Ok(Arc::new(me))
    }

    pub fn library_name(&self) -> &str {
        &self.lib.name
    }

    pub fn version(&self) -> String {
        // SAFETY: 静的な NUL 終端文字列。
        unsafe { cstr((self.version_str)()) }
    }

    pub fn has_encoder(&self, k: Kind) -> bool {
        match k {
            Kind::Vp8 => self.vp8_cx.is_some(),
            Kind::Vp9 => self.vp9_cx.is_some(),
        }
    }

    pub fn has_decoder(&self, k: Kind) -> bool {
        match k {
            Kind::Vp8 => self.vp8_dx.is_some(),
            Kind::Vp9 => self.vp9_dx.is_some(),
        }
    }

    fn iface(&self, f: Option<IfaceFn>) -> Result<*mut vpx_codec_iface_t, String> {
        let f = f.ok_or("interface not available in this libvpx build")?;
        // SAFETY: 引数なしで静的なインターフェース表を返す。
        let p = unsafe { f() };
        if p.is_null() {
            Err("null codec interface".into())
        } else {
            Ok(p)
        }
    }

    fn err(&self, code: vpx_codec_err_t, ctx: Option<&vpx_codec_ctx_t>) -> String {
        // SAFETY: err_to_string は静的な文字列、error_detail は ctx が生きている間有効な文字列（NULL のことがある）を返す。
        unsafe {
            let mut s = cstr((self.err_to_string)(code));
            if let Some(c) = ctx {
                let d = cstr((self.error_detail)(c));
                if !d.is_empty() {
                    s.push_str(": ");
                    s.push_str(&d);
                }
            }
            s
        }
    }
}

/// # Safety
/// `p` は NULL か NUL 終端の文字列。
unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        // SAFETY: 関数の契約。
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// エンコーダーの設定（安全な側から渡す値）。
pub struct EncParams {
    pub kind: Kind,
    pub width: u32,
    pub height: u32,
    pub tb_num: i32,
    pub tb_den: i32,
    pub threads: u32,
    pub bitrate_kbps: u32,
    /// `bindings::vpx_rc_mode_*`
    pub end_usage: u32,
    pub min_q: u32,
    pub max_q: u32,
    pub cq_level: Option<u32>,
    pub kf_max_dist: u32,
    pub lag_in_frames: u32,
    pub realtime: bool,
    pub cpu_used: i32,
}

/// 入力の 1 枚（I420、各面の stride 付き）。
pub struct Image<'a> {
    pub width: u32,
    pub height: u32,
    pub planes: [&'a [u8]; 3],
    pub strides: [usize; 3],
}

/// 出力の 1 パケット。
pub struct Packet {
    pub data: Vec<u8>,
    pub pts: i64,
    pub duration: u64,
    pub keyframe: bool,
}

pub struct RawEncoder {
    lib: Arc<VpxLib>,
    /// 初期化後に移動しないよう Box に置く。
    ctx: Box<vpx_codec_ctx_t>,
    deadline: c_ulong,
    width: u32,
    height: u32,
}

// SAFETY: vpx_codec_ctx はスレッドに結び付いた状態を持たない。`&mut self` でだけ使う。
unsafe impl Send for RawEncoder {}

impl RawEncoder {
    pub fn new(lib: Arc<VpxLib>, p: &EncParams) -> Result<RawEncoder, String> {
        let iface = lib.iface(match p.kind {
            Kind::Vp8 => lib.vp8_cx,
            Kind::Vp9 => lib.vp9_cx,
        })?;
        // SAFETY: vpx_codec_enc_cfg は POD。enc_config_default が全項目を既定値で埋める。
        let mut cfg: vpx_codec_enc_cfg = unsafe { MaybeUninit::zeroed().assume_init() };
        let r = unsafe { (lib.enc_config_default)(iface, &mut cfg, 0) };
        if r != 0 {
            return Err(lib.err(r, None));
        }
        cfg.g_w = p.width;
        cfg.g_h = p.height;
        cfg.g_timebase.num = p.tb_num;
        cfg.g_timebase.den = p.tb_den;
        cfg.g_threads = p.threads;
        cfg.g_lag_in_frames = p.lag_in_frames;
        cfg.g_pass = bindings::vpx_enc_pass_VPX_RC_ONE_PASS;
        cfg.rc_end_usage = p.end_usage;
        cfg.rc_target_bitrate = p.bitrate_kbps;
        cfg.rc_min_quantizer = p.min_q;
        cfg.rc_max_quantizer = p.max_q;
        cfg.kf_mode = bindings::vpx_kf_mode_VPX_KF_AUTO;
        cfg.kf_min_dist = 0;
        cfg.kf_max_dist = p.kf_max_dist;
        if p.realtime {
            cfg.rc_dropframe_thresh = 0;
            cfg.rc_undershoot_pct = 50;
            cfg.rc_overshoot_pct = 50;
            cfg.rc_buf_initial_sz = 500;
            cfg.rc_buf_optimal_sz = 600;
            cfg.rc_buf_sz = 1000;
        }
        // SAFETY: vpx_codec_ctx は POD。enc_init が初期化する。Box で場所を固定する。
        let mut ctx: Box<vpx_codec_ctx_t> =
            Box::new(unsafe { MaybeUninit::zeroed().assume_init() });
        let r = unsafe {
            (lib.enc_init)(
                &mut *ctx,
                iface,
                &cfg,
                0,
                bindings::VPX_ENCODER_ABI_VERSION as c_int,
            )
        };
        if r != 0 {
            // 失敗時の ctx は初期化されていない（vpx_codec_enc_init_ver は失敗時に priv を NULL のままにする）。
            return Err(lib.err(r, Some(&ctx)));
        }
        let mut enc = RawEncoder {
            lib,
            ctx,
            deadline: if p.realtime {
                bindings::VPX_DL_REALTIME as c_ulong
            } else {
                bindings::VPX_DL_GOOD_QUALITY as c_ulong
            },
            width: p.width,
            height: p.height,
        };
        enc.control(
            bindings::vp8e_enc_control_id_VP8E_SET_CPUUSED as c_int,
            p.cpu_used,
        )?;
        if let Some(q) = p.cq_level {
            enc.control(
                bindings::vp8e_enc_control_id_VP8E_SET_CQ_LEVEL as c_int,
                q as c_int,
            )?;
        }
        match p.kind {
            Kind::Vp8 => {
                enc.control(
                    bindings::vp8e_enc_control_id_VP8E_SET_ENABLEAUTOALTREF as c_int,
                    0,
                )?;
            }
            Kind::Vp9 => {
                enc.control(bindings::vp8e_enc_control_id_VP9E_SET_ROW_MT as c_int, 1)?;
                if p.realtime {
                    enc.control(bindings::vp8e_enc_control_id_VP9E_SET_AQ_MODE as c_int, 3)?;
                }
                // 幅に合わせてタイル列（log2）。
                let tiles = match p.width {
                    0..=703 => 0,
                    704..=1407 => 1,
                    1408..=2815 => 2,
                    _ => 3,
                };
                enc.control(
                    bindings::vp8e_enc_control_id_VP9E_SET_TILE_COLUMNS as c_int,
                    tiles,
                )?;
            }
        }
        Ok(enc)
    }

    fn control(&mut self, id: c_int, v: c_int) -> Result<(), String> {
        // SAFETY: これらの制御 ID は int を 1 つ受け取る（vp8cx.h の VPX_CTRL_USE_TYPE）。ctx は初期化済み。
        let r = unsafe { (self.lib.control)(&mut *self.ctx, id, v) };
        if r != 0 {
            Err(self.lib.err(r, Some(&self.ctx)))
        } else {
            Ok(())
        }
    }

    /// 1 枚符号化して（`None` なら出し切り）、出たパケットを返す。
    pub fn encode(
        &mut self,
        img: Option<&Image<'_>>,
        pts: i64,
        duration: u64,
        force_key: bool,
    ) -> Result<Vec<Packet>, String> {
        let r = match img {
            Some(i) => {
                if i.width != self.width || i.height != self.height {
                    return Err("image size mismatch".into());
                }
                let (w, h) = (i.width as usize, i.height as usize);
                let dims = [
                    (w, h),
                    (w.div_ceil(2), h.div_ceil(2)),
                    (w.div_ceil(2), h.div_ceil(2)),
                ];
                for k in 0..3 {
                    let (pw, ph) = dims[k];
                    if i.strides[k] < pw
                        || i.strides[k] > i32::MAX as usize
                        || i.planes[k].len() < i.strides[k] * (ph - 1) + pw
                    {
                        return Err("image plane too small".into());
                    }
                }
                // SAFETY: vpx_image_t は POD。読み取り専用の入力として、面のポインター・stride・大きさだけを埋める。
                // 面の大きさは上で確かめた。エンコーダーは入力を読むだけで書かない（vpx_codec_encode の const img）。
                let mut im: vpx_image_t = unsafe { MaybeUninit::zeroed().assume_init() };
                im.fmt = bindings::vpx_img_fmt_VPX_IMG_FMT_I420;
                im.cs = bindings::vpx_color_space_VPX_CS_BT_601;
                im.range = bindings::vpx_color_range_VPX_CR_STUDIO_RANGE;
                im.w = i.width;
                im.h = i.height;
                im.d_w = i.width;
                im.d_h = i.height;
                im.r_w = i.width;
                im.r_h = i.height;
                im.bit_depth = 8;
                im.x_chroma_shift = 1;
                im.y_chroma_shift = 1;
                im.bps = 12;
                for k in 0..3 {
                    im.planes[k] = i.planes[k].as_ptr() as *mut u8;
                    im.stride[k] = i.strides[k] as c_int;
                }
                let flags = if force_key {
                    bindings::VPX_EFLAG_FORCE_KF as c_long
                } else {
                    0
                };
                // SAFETY: ctx は初期化済み、im は呼び出しの間有効（面は借用中のスライス）。
                unsafe {
                    (self.lib.encode)(
                        &mut *self.ctx,
                        &im,
                        pts,
                        duration as c_ulong,
                        flags,
                        self.deadline,
                    )
                }
            }
            // SAFETY: img = NULL は出し切りの要求（vpx_encoder.h）。
            None => unsafe {
                (self.lib.encode)(&mut *self.ctx, ptr::null(), 0, 0, 0, self.deadline)
            },
        };
        if r != 0 {
            return Err(self.lib.err(r, Some(&self.ctx)));
        }
        let mut out = Vec::new();
        let mut iter: vpx_codec_iter_t = ptr::null();
        loop {
            // SAFETY: iter は NULL から始め、ライブラリが更新する。返るパケットは次の encode 呼び出しまで有効。
            let pkt = unsafe { (self.lib.get_cx_data)(&mut *self.ctx, &mut iter) };
            if pkt.is_null() {
                break;
            }
            // SAFETY: pkt は有効。kind が FRAME のときだけ data.frame を読む（union の該当する腕）。
            unsafe {
                if (*pkt).kind != bindings::vpx_codec_cx_pkt_kind_VPX_CODEC_CX_FRAME_PKT {
                    continue;
                }
                let f = &(*pkt).data.frame;
                if f.buf.is_null() || f.sz == 0 {
                    continue;
                }
                let data = std::slice::from_raw_parts(f.buf as *const u8, f.sz).to_vec();
                out.push(Packet {
                    data,
                    pts: f.pts,
                    duration: f.duration as u64,
                    keyframe: f.flags & bindings::VPX_FRAME_IS_KEY != 0,
                });
            }
        }
        Ok(out)
    }
}

impl Drop for RawEncoder {
    fn drop(&mut self) {
        // SAFETY: 初期化に成功した ctx を一度だけ破棄する。
        unsafe { (self.lib.destroy)(&mut *self.ctx) };
    }
}

/// 復号した 1 枚（コピー済み）。
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u32,
    /// 4:2:0 なら Some(true)、4:0:0 は無いので Some(false) は返さない。他の形式は None。
    pub layout_420: Option<bool>,
    pub planes: [Vec<u8>; 3],
    pub row_bytes: [usize; 3],
    pub pts: i64,
}

pub struct RawDecoder {
    lib: Arc<VpxLib>,
    ctx: Box<vpx_codec_ctx_t>,
}

// SAFETY: RawEncoder と同じ。
unsafe impl Send for RawDecoder {}

impl RawDecoder {
    pub fn new(lib: Arc<VpxLib>, kind: Kind, threads: u32) -> Result<RawDecoder, String> {
        let iface = lib.iface(match kind {
            Kind::Vp8 => lib.vp8_dx,
            Kind::Vp9 => lib.vp9_dx,
        })?;
        let cfg = vpx_codec_dec_cfg {
            threads,
            w: 0,
            h: 0,
        };
        // SAFETY: vpx_codec_ctx は POD。dec_init が初期化する。
        let mut ctx: Box<vpx_codec_ctx_t> =
            Box::new(unsafe { MaybeUninit::zeroed().assume_init() });
        let r = unsafe {
            (lib.dec_init)(
                &mut *ctx,
                iface,
                &cfg,
                0,
                bindings::VPX_DECODER_ABI_VERSION as c_int,
            )
        };
        if r != 0 {
            return Err(lib.err(r, Some(&ctx)));
        }
        Ok(RawDecoder { lib, ctx })
    }

    /// 1 フレーム渡して（`None` なら出し切り）、出た絵を返す。`max_pixels` を超える絵はエラー。
    pub fn decode(
        &mut self,
        data: Option<&[u8]>,
        pts: i64,
        max_pixels: u64,
    ) -> Result<Vec<DecodedImage>, String> {
        let r = match data {
            Some(d) => {
                if d.is_empty() || d.len() > u32::MAX as usize {
                    return Err("bad frame size".into());
                }
                // SAFETY: d は有効なスライス。user_priv には pts の値をポインターの大きさの整数として入れ、
                // 出力の vpx_image_t.user_priv で受け取る（ポインターとしては使わない）。
                unsafe {
                    (self.lib.decode)(
                        &mut *self.ctx,
                        d.as_ptr(),
                        d.len() as c_uint,
                        pts as isize as *mut c_void,
                        0,
                    )
                }
            }
            // SAFETY: data = NULL, size = 0 は出し切りの要求（vpx_decoder.h）。
            None => unsafe {
                (self.lib.decode)(&mut *self.ctx, ptr::null(), 0, ptr::null_mut(), 0)
            },
        };
        if r != 0 {
            return Err(self.lib.err(r, Some(&self.ctx)));
        }
        let mut out = Vec::new();
        let mut iter: vpx_codec_iter_t = ptr::null();
        loop {
            // SAFETY: iter は NULL から始める。返る絵は次の decode 呼び出しまで有効。
            let img = unsafe { (self.lib.get_frame)(&mut *self.ctx, &mut iter) };
            if img.is_null() {
                break;
            }
            // SAFETY: img は有効。
            let im = unsafe { &*img };
            out.push(copy_image(im, max_pixels)?);
        }
        Ok(out)
    }
}

fn copy_image(im: &vpx_image_t, max_pixels: u64) -> Result<DecodedImage, String> {
    let (w, h) = (im.d_w, im.d_h);
    if w == 0 || h == 0 || w as u64 * h as u64 > max_pixels {
        return Err(format!("decoded image {w}x{h} out of range"));
    }
    let high = im.fmt & bindings::VPX_IMG_FMT_HIGHBITDEPTH != 0;
    let bps = if high { 2usize } else { 1 };
    let layout_420 = (im.x_chroma_shift == 1 && im.y_chroma_shift == 1).then_some(true);
    let (cw, ch) = ((w as usize).div_ceil(2), (h as usize).div_ceil(2));
    let dims = [(w as usize, h as usize), (cw, ch), (cw, ch)];
    let mut planes: [Vec<u8>; 3] = Default::default();
    let mut row_bytes = [0usize; 3];
    if layout_420.is_some() {
        for k in 0..3 {
            let (pw, ph) = dims[k];
            let row = pw * bps;
            let stride = im.stride[k] as isize;
            if im.planes[k].is_null() || stride.unsigned_abs() < row {
                return Err("decoder returned an invalid plane".into());
            }
            let mut v = Vec::with_capacity(row * ph);
            for y in 0..ph as isize {
                // SAFETY: libvpx の絵は planes[k] から stride ごとに少なくとも ph 行、各行 row バイトが有効
                // （d_w・d_h と chroma_shift から求めた大きさ。vpx_image.h の契約）。
                unsafe {
                    let p = im.planes[k].offset(y * stride);
                    v.extend_from_slice(std::slice::from_raw_parts(p, row));
                }
            }
            planes[k] = v;
            row_bytes[k] = row;
        }
    }
    Ok(DecodedImage {
        width: w,
        height: h,
        bit_depth: if high { im.bit_depth } else { 8 },
        layout_420,
        planes,
        row_bytes,
        pts: im.user_priv as isize as i64,
    })
}

impl Drop for RawDecoder {
    fn drop(&mut self) {
        // SAFETY: 初期化に成功した ctx を一度だけ破棄する。
        unsafe { (self.lib.destroy)(&mut *self.ctx) };
    }
}
