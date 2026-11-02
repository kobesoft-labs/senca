//! 機器の抽象（`CaptureDevice`・`CaptureBackend`）と、検出をまとめる [`DeviceRegistry`]。
//!
//! # 契約
//!
//! - **検出は落ちない。** ライブラリ・装置が無いときは [`BackendStatus::available`] を `false` にして理由を返す。
//! - **流れ。** `open`（機器を確保）→ [`CaptureDevice::start`]（形式を取り決めて取り込み開始）→
//!   [`CaptureDevice::read`] を繰り返す → [`CaptureDevice::stop`]。`drop` でも止まる。
//! - **スレッド。** `CaptureDevice` は `Send`（読み取り用のスレッドに移して使う）。1 つのスレッドから順に呼ぶ。
//! - **フレーム。** `read` は共通メディアモデルに変換したもの（I420・i16 PCM・H.264 の Annex B）を返す。
//!   時刻は [`Timestamps`](crate::types::Timestamps)。
//! - **時間切れ。** `read` は `timeout` だけ待って何も無ければ `Ok(None)`。入力が終わったら
//!   [`ErrorKind::EndOfStream`](crate::ErrorKind)。

use crate::error::{CaptureError, Result};
use crate::types::{
    Backend, BackendStatus, CaptureConfig, CapturedFrame, DeviceDescriptor, Negotiated,
};
use senca_codec::dl::LibrarySearch;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// 取り込み機器（開いたもの）。
pub trait CaptureDevice: Send {
    fn descriptor(&self) -> &DeviceDescriptor;

    /// 形式を取り決めて取り込みを始める。要求の `video`・`audio` のうち機器に無いものは `Unsupported`。
    fn start(&mut self, config: &CaptureConfig) -> Result<Negotiated>;

    /// フレームを 1 つ読む。`timeout` の間に無ければ `Ok(None)`。
    fn read(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>>;

    /// 取り込みを止める（何度呼んでもよい）。
    fn stop(&mut self) -> Result<()>;
}

/// 機器の種類ごとの検出と生成。
pub trait CaptureBackend: Send + Sync {
    fn backend(&self) -> Backend;

    /// 検出の結果（使えないときも理由を返す）。
    fn status(&self) -> BackendStatus;

    /// 機器の一覧（その時点で調べ直す。USB の抜き差しに追従する）。
    fn enumerate(&self) -> Vec<DeviceDescriptor>;

    /// 機器を開く。`id` は [`DeviceDescriptor::id`]。
    fn open(&self, id: &str) -> Result<Box<dyn CaptureDevice>>;
}

/// 検出の設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOptions {
    /// V4L2 の装置ファイルを探すディレクトリ（既定 `/dev`）。
    pub dev_dir: PathBuf,
    /// ALSA の情報（既定 `/proc/asound`）。
    pub proc_asound: PathBuf,
    /// `libasound.so.2`・`libDeckLinkAPI.so` を探す場所。
    pub libs: LibrarySearch,
    /// 試験用の機器（`synthetic:0`）を出す。
    pub synthetic: bool,
    /// 試験用の機器の設定。
    pub synthetic_options: crate::synthetic::SyntheticOptions,
    /// 検出しないバックエンド。
    pub disabled: Vec<Backend>,
}

/// 環境変数 `SENCA_CAPTURE_SYNTHETIC`（`1` で試験用の機器を出す）。
pub const SYNTHETIC_ENV: &str = "SENCA_CAPTURE_SYNTHETIC";
/// 環境変数 `SENCA_CAPTURE_DISABLE`（`,` 区切りのバックエンド名）。
pub const DISABLE_ENV: &str = "SENCA_CAPTURE_DISABLE";
/// 環境変数 `SENCA_CAPTURE_LIB_DIR`（`:` 区切り。DeckLink・ALSA のライブラリを探す場所）。
pub const LIB_DIR_ENV: &str = "SENCA_CAPTURE_LIB_DIR";

impl Default for ProbeOptions {
    fn default() -> Self {
        ProbeOptions {
            dev_dir: PathBuf::from("/dev"),
            proc_asound: PathBuf::from("/proc/asound"),
            libs: LibrarySearch::default(),
            synthetic: false,
            synthetic_options: Default::default(),
            disabled: Vec::new(),
        }
    }
}

impl ProbeOptions {
    /// 環境変数から作る。
    pub fn from_env() -> Self {
        let mut o = ProbeOptions::default();
        if let Some(v) = std::env::var_os(LIB_DIR_ENV) {
            o.libs.dirs = std::env::split_paths(&v)
                .filter(|p| !p.as_os_str().is_empty())
                .collect();
        }
        o.synthetic = std::env::var(SYNTHETIC_ENV).is_ok_and(|v| v == "1" || v == "true");
        if let Ok(v) = std::env::var(DISABLE_ENV) {
            o.disabled = v
                .split(',')
                .filter_map(|s| Backend::parse(s.trim()))
                .collect();
        }
        o
    }

    pub fn is_enabled(&self, b: Backend) -> bool {
        !self.disabled.contains(&b) && (b != Backend::Synthetic || self.synthetic)
    }
}

/// バックエンドの一覧。
#[derive(Clone)]
pub struct DeviceRegistry {
    backends: Vec<Arc<dyn CaptureBackend>>,
    disabled: Vec<BackendStatus>,
}

impl std::fmt::Debug for DeviceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.backends.iter().map(|b| b.backend()))
            .finish()
    }
}

impl DeviceRegistry {
    /// 組み込みのバックエンドを検出する（落ちない）。
    pub fn probe(o: &ProbeOptions) -> Self {
        let mut backends: Vec<Arc<dyn CaptureBackend>> = Vec::new();
        let mut disabled = Vec::new();
        for b in Backend::ALL {
            if !o.is_enabled(b) {
                if b != Backend::Synthetic {
                    disabled.push(BackendStatus {
                        backend: b,
                        available: false,
                        detail: "disabled by configuration".into(),
                        verification: b.verification(),
                    });
                }
                continue;
            }
            backends.push(match b {
                Backend::V4l2 => Arc::new(crate::v4l2::V4l2Backend::new(o.dev_dir.clone())),
                Backend::Alsa => Arc::new(crate::alsa::AlsaBackend::probe(
                    &o.libs,
                    o.proc_asound.clone(),
                )),
                Backend::Decklink => Arc::new(crate::decklink::DeckLinkBackend::probe(&o.libs)),
                Backend::Synthetic => Arc::new(crate::synthetic::SyntheticBackend::new(
                    o.synthetic_options.clone(),
                )),
            });
        }
        DeviceRegistry { backends, disabled }
    }

    /// 与えたバックエンドで作る（試験用）。
    pub fn with_backends(backends: Vec<Arc<dyn CaptureBackend>>) -> Self {
        DeviceRegistry {
            backends,
            disabled: Vec::new(),
        }
    }

    /// 試験用の機器の設定を差し替えた一覧（試験用の機器が有効なときだけ。無効なら同じもの）。
    pub fn with_synthetic(&self, options: crate::synthetic::SyntheticOptions) -> Self {
        let mut r = self.clone();
        for b in r.backends.iter_mut() {
            if b.backend() == Backend::Synthetic {
                *b = Arc::new(crate::synthetic::SyntheticBackend::new(options.clone()));
            }
        }
        r
    }

    pub fn backends(&self) -> &[Arc<dyn CaptureBackend>] {
        &self.backends
    }

    pub fn statuses(&self) -> Vec<BackendStatus> {
        let mut v: Vec<BackendStatus> = self.backends.iter().map(|b| b.status()).collect();
        v.extend(self.disabled.iter().cloned());
        v
    }

    /// 全バックエンドの機器の一覧。
    pub fn devices(&self) -> Vec<DeviceDescriptor> {
        self.backends
            .iter()
            .filter(|b| b.status().available)
            .flat_map(|b| b.enumerate())
            .collect()
    }

    /// 機器を開く。`id` の前置き（`v4l2:` など）でバックエンドを選ぶ。
    pub fn open(&self, id: &str) -> Result<Box<dyn CaptureDevice>> {
        let (prefix, _) = id
            .split_once(':')
            .ok_or_else(|| CaptureError::invalid_config(format!("bad device id {id:?}")))?;
        let backend = Backend::parse(prefix)
            .ok_or_else(|| CaptureError::not_found(format!("unknown backend {prefix:?}")))?;
        let b = self
            .backends
            .iter()
            .find(|b| b.backend() == backend)
            .ok_or_else(|| {
                CaptureError::unavailable(format!("backend {backend} is not enabled on this node"))
            })?;
        let st = b.status();
        if !st.available {
            return Err(CaptureError::unavailable(format!(
                "backend {backend} is not available: {}",
                st.detail
            )));
        }
        b.open(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nowhere() -> ProbeOptions {
        ProbeOptions {
            dev_dir: "/nonexistent-senca/dev".into(),
            proc_asound: "/nonexistent-senca/asound".into(),
            libs: LibrarySearch {
                dirs: vec!["/nonexistent-senca/lib".into()],
                dirs_only: true,
            },
            synthetic: false,
            synthetic_options: Default::default(),
            disabled: vec![],
        }
    }

    #[test]
    fn probe_without_anything_never_fails() {
        let r = DeviceRegistry::probe(&nowhere());
        assert!(r.devices().is_empty());
        let st = r.statuses();
        assert_eq!(st.len(), 3);
        let dl = st.iter().find(|s| s.backend == Backend::Decklink).unwrap();
        assert!(!dl.available);
        assert!(dl.detail.contains("libDeckLinkAPI"), "{}", dl.detail);
        let e = r.open("decklink:0").err().unwrap();
        assert_eq!(e.kind, crate::ErrorKind::Unavailable);
        assert_eq!(
            r.open("synthetic:0").err().unwrap().kind,
            crate::ErrorKind::Unavailable
        );
        assert_eq!(
            r.open("bogus:0").err().unwrap().kind,
            crate::ErrorKind::NotFound
        );
        assert_eq!(
            r.open("nocolon").err().unwrap().kind,
            crate::ErrorKind::InvalidConfig
        );
        assert_eq!(
            r.open("v4l2:/dev/video99").err().unwrap().kind,
            crate::ErrorKind::NotFound
        );
    }

    #[test]
    fn synthetic_and_disable() {
        let mut o = nowhere();
        o.synthetic = true;
        o.disabled = vec![Backend::Decklink];
        let r = DeviceRegistry::probe(&o);
        let d = r.devices();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].id, "synthetic:0");
        let st = r.statuses();
        assert!(
            st.iter()
                .any(|s| s.backend == Backend::Decklink && s.detail.contains("disabled"))
        );
        assert!(r.open("synthetic:0").is_ok());
        assert_eq!(
            r.open("synthetic:7").err().unwrap().kind,
            crate::ErrorKind::NotFound
        );
    }
}
