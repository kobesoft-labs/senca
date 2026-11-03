//! codec worker との窓口（ADR-0004、M4）。外部コーデックはノードに読み込まず、worker プロセス（プール）で動かす。
//! H.264/H.265 はハードウェアだけ。装置が無ければ 503（CAPACITY_UNAVAILABLE）を理由付きで返す。

use bytes::Bytes;
use senca_codec::caps::Direction;
use senca_codec::registry::Policy;
use senca_codec::{DecoderConfig, PixelFormat, VideoFrame};
use senca_codec_worker::pool::{PoolConfig, WorkerPool};
use senca_codec_worker::protocol::{JobKind, JobOutput};
use senca_control::{Problem, ProblemCode};
use senca_core::{AccessUnit, Codec, CodecConfig};
use senca_image::I420Frame;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

pub struct CodecService {
    pool: WorkerPool,
    /// 常駐の処理（パイプライン）のための worker（1 セッション = 1 プロセス）。
    pub sessions: senca_codec_worker::session_pool::SessionPool,
    /// プールを動かしている実行環境（tokio の外のスレッドからも依頼できるように）。
    rt: tokio::runtime::Handle,
}

fn worker_binary(cfg: &crate::config::Config) -> Option<PathBuf> {
    if let Some(p) = &cfg.codec_worker_bin {
        return Some(p.clone());
    }
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.join("senca-codec-worker");
    p.exists().then_some(p)
}

impl CodecService {
    pub async fn start(cfg: &crate::config::Config) -> Result<CodecService, String> {
        let bin = worker_binary(cfg).ok_or("senca-codec-worker が見つかりません（--codec-worker-bin）")?;
        let mut pc = PoolConfig::new(bin);
        if cfg.codec_workers > 0 {
            pc.size = cfg.codec_workers;
        }
        if let Some(d) = &cfg.codec_lib_dir {
            pc.env.retain(|(k, _)| k != "SENCA_CODEC_LIB_DIR");
            pc.env.push(("SENCA_CODEC_LIB_DIR".into(), d.clone().into_os_string()));
        }
        pc.job_timeout = Duration::from_secs(30);
        // --log-json: worker の標準エラーの行も JSON のログにまとめる
        pc.stderr_log = crate::util::worker_log_sink();
        let mut sc = senca_codec_worker::session_pool::SessionPoolConfig::new(pc.program.clone());
        sc.worker.env = pc.env.clone();
        sc.worker.env_clear = pc.env_clear;
        let sessions = senca_codec_worker::session_pool::SessionPool::new(sc);
        let pool = WorkerPool::start(pc).await.map_err(|e| e.to_string())?;
        Ok(CodecService { pool, sessions, rt: tokio::runtime::Handle::current() })
    }

    fn caps(&self) -> Vec<senca_codec::Capability> {
        self.pool.capabilities()
    }

    pub fn capabilities(&self) -> Value {
        let v: Vec<Value> = self
            .caps()
            .iter()
            .map(|c| {
                json!({
                    "provider": c.provider.as_str(),
                    "codec": c.codec.as_str(),
                    "direction": if c.direction == Direction::Encode { "encode" } else { "decode" },
                    "hardware": c.hardware,
                    "max_width": c.max_width,
                    "max_height": c.max_height,
                    "max_sessions": c.max_sessions,
                    "device": c.device_name,
                })
            })
            .collect();
        Value::Array(v)
    }

    pub fn devices(&self) -> Value {
        let info = self.pool.worker_info();
        let statuses: Vec<Value> = info
            .as_ref()
            .map(|i| {
                i.statuses
                    .iter()
                    .map(|s| json!({ "provider": s.provider.as_str(), "available": s.available, "detail": s.detail }))
                    .collect()
            })
            .unwrap_or_default();
        json!({ "providers": statuses, "sandbox": info.map(|i| i.sandbox) })
    }

    pub fn capability_list(&self) -> Vec<senca_codec::Capability> {
        self.caps()
    }

    /// 新しい worker で装置を調べ直す（ブロックする。tokio の外のスレッドから呼ぶ）。起動できなければ None。
    pub fn probe(&self) -> Option<senca_codec_worker::pool::WorkerInfo> {
        self.rt.block_on(self.pool.probe()).ok()
    }

    pub fn can_transcode_hw(&self) -> bool {
        self.caps().iter().any(|c| c.hardware && c.direction == Direction::Encode)
    }

    pub fn can_decode(&self, codec: Codec) -> bool {
        self.caps().iter().any(|c| c.codec == codec && c.direction == Direction::Decode)
    }

    /// AU を復号して 1 枚の画にする（静止画・サムネイル）。`aus` はキーフレームから始めること。
    pub fn decode_image(&self, tenant: &str, config: &CodecConfig, aus: Vec<AccessUnit>, royalty_free: bool) -> Result<I420Frame, Problem> {
        if !self.can_decode(config.codec) {
            return Err(Problem::new(
                ProblemCode::CapacityUnavailable,
                format!("{} を復号できる装置がありません（H.264/H.265 はハードウェアのデコーダーだけを使います）", config.codec),
            )
            .with_retryable(false));
        }
        let mut dc = DecoderConfig::new(config.codec);
        dc.config = Some(config.clone());
        dc.output_format = Some(PixelFormat::I420);
        let policy = Policy { royalty_free_only: royalty_free, ..Default::default() };
        let kind = JobKind::DecodeToImage { decoder: dc, aus };
        let res = self.rt.block_on(self.pool.submit(tenant, policy, kind));
        match res {
            Ok(JobOutput::Image(f)) => to_i420(&f).ok_or_else(Problem::internal),
            Ok(_) => Err(Problem::internal()),
            Err(e) => {
                let code = match e.kind.api_code() {
                    "UNSUPPORTED_CODEC" => ProblemCode::UnsupportedCodec,
                    "INVALID_CONFIG" => ProblemCode::InvalidConfig,
                    "CAPACITY_UNAVAILABLE" => ProblemCode::CapacityUnavailable,
                    _ => ProblemCode::InternalError,
                };
                Err(Problem::new(code, "復号に失敗しました").with_details(json!({ "reason": e.message })))
            }
        }
    }

    pub async fn shutdown(&self) {
        self.pool.shutdown().await;
    }
}

/// worker の出力（I420 / NV12、stride あり）を詰めた I420 にする。
fn to_i420(f: &VideoFrame) -> Option<I420Frame> {
    let (w, h) = (f.width as usize, f.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut out = Vec::with_capacity(w * h + 2 * cw * ch);
    let copy = |out: &mut Vec<u8>, p: &senca_codec::Plane, rw: usize, rh: usize| -> Option<()> {
        for y in 0..rh {
            out.extend_from_slice(p.data.get(y * p.stride..y * p.stride + rw)?);
        }
        Some(())
    };
    match f.format {
        PixelFormat::I420 => {
            copy(&mut out, f.planes.first()?, w, h)?;
            copy(&mut out, f.planes.get(1)?, cw, ch)?;
            copy(&mut out, f.planes.get(2)?, cw, ch)?;
        }
        PixelFormat::Nv12 => {
            copy(&mut out, f.planes.first()?, w, h)?;
            let uv = f.planes.get(1)?;
            let mut u = Vec::with_capacity(cw * ch);
            let mut v = Vec::with_capacity(cw * ch);
            for y in 0..ch {
                let row = uv.data.get(y * uv.stride..y * uv.stride + 2 * cw)?;
                for x in 0..cw {
                    u.push(row[2 * x]);
                    v.push(row[2 * x + 1]);
                }
            }
            out.extend_from_slice(&u);
            out.extend_from_slice(&v);
        }
        _ => return None,
    }
    I420Frame::from_packed(w, h, &out).ok()
}

/// JPEG（BT.709 限定範囲を仮定。SD は BT.601）。
pub fn jpeg(f: &I420Frame, quality: u8) -> Option<Bytes> {
    let spec = spec_for(f);
    senca_image::jpeg::encode_jpeg_i420(f, spec, quality).ok().map(Bytes::from)
}

pub fn png(f: &I420Frame) -> Option<Bytes> {
    let rgb = senca_image::color::i420_to_rgb(f, spec_for(f));
    senca_image::png::encode_png_rgb(&rgb, 6).ok().map(Bytes::from)
}

fn spec_for(f: &I420Frame) -> senca_image::YuvSpec {
    use senca_image::{ColorMatrix, ColorRange, YuvSpec};
    let m = if f.height() >= 720 { ColorMatrix::Bt709 } else { ColorMatrix::Bt601 };
    YuvSpec { matrix: m, range: ColorRange::Limited }
}
