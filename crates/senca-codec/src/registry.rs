//! プロバイダーの検出と選択。
//!
//! # 選び方（ADR-0004・REQ-004・REQ-007）
//!
//! 1. AAC は常に `Unsupported`（符号化・復号しない）。
//! 2. `royalty_free_only` のとき、特許プールのあるコーデック（H.264・H.265・AAC）は `PolicyRejected`。
//! 3. H.264・H.265 はハードウェアのプロバイダーだけを候補にする。候補が無ければ `Unavailable`（503）。
//! 4. それ以外は、ハードウェアを先に、次にソフトウェア（`allow_software` のとき）。同じ区分の中では `prefer` の順。
//! 5. 候補を順に試し、`Unavailable`・`Backend`（セッション上限・ドライバーの一時的な失敗）なら次を試す。

use crate::caps::{Capability, DeviceInfo, Direction, ProviderId, ProviderStatus};
use crate::config::{DecoderConfig, EncoderConfig};
use crate::dl::LibrarySearch;
use crate::error::{CodecError, ErrorKind, Result};
use crate::provider::{CodecProvider, Decoder, Encoder};
use senca_core::Codec;
use std::path::PathBuf;
use std::sync::Arc;

/// 環境変数 `SENCA_CODEC_DISABLE`（`,` 区切りのプロバイダー名、`hardware`・`software` も可）。
pub const DISABLE_ENV: &str = "SENCA_CODEC_DISABLE";

/// 検出の設定。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeOptions {
    pub libs: LibrarySearch,
    /// 検出しないプロバイダー。
    pub disabled: Vec<ProviderId>,
    /// VA-API で調べる DRM の render node。`None` なら `/dev/dri/renderD*` を探す。
    pub drm_nodes: Option<Vec<PathBuf>>,
}

impl ProbeOptions {
    /// 環境変数（`SENCA_CODEC_LIB_DIR`・`SENCA_CODEC_DISABLE`）から作る。
    pub fn from_env() -> Self {
        let mut o = ProbeOptions {
            libs: LibrarySearch::from_env(),
            ..Default::default()
        };
        if let Ok(v) = std::env::var(DISABLE_ENV) {
            for name in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                match name {
                    "hardware" => o
                        .disabled
                        .extend(ProviderId::ALL.iter().filter(|p| p.is_hardware())),
                    "software" => o
                        .disabled
                        .extend(ProviderId::ALL.iter().filter(|p| !p.is_hardware())),
                    _ => {
                        if let Some(p) = ProviderId::parse(name) {
                            o.disabled.push(p);
                        }
                    }
                }
            }
        }
        o
    }

    pub fn is_enabled(&self, id: ProviderId) -> bool {
        !self.disabled.contains(&id)
    }
}

/// 選択の方針（テナント・stream ごと、REQ-007）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// ロイヤリティフリーとして提供されている技術だけを使う。
    pub royalty_free_only: bool,
    /// ソフトウェアのプロバイダーを使ってよい（H.264/H.265 には関係しない。そもそも使わない）。
    pub allow_software: bool,
    /// 同じ区分（ハードウェア／ソフトウェア）の中で優先する順。
    pub prefer: Vec<ProviderId>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            royalty_free_only: false,
            allow_software: true,
            prefer: Vec::new(),
        }
    }
}

impl Policy {
    pub fn royalty_free() -> Self {
        Policy {
            royalty_free_only: true,
            ..Default::default()
        }
    }
}

/// プロバイダーの一覧。
#[derive(Clone)]
pub struct ProviderRegistry {
    providers: Vec<Arc<dyn CodecProvider>>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.providers.iter().map(|p| p.id()))
            .finish()
    }
}

impl ProviderRegistry {
    /// 組み込みのプロバイダーをすべて検出する。ライブラリ・装置が無いものは「使えない」として残す（落ちない）。
    pub fn probe(options: &ProbeOptions) -> Self {
        ProviderRegistry {
            providers: crate::providers::builtin(options),
        }
    }

    /// 与えたプロバイダーで作る（試験用）。
    pub fn with_providers(providers: Vec<Arc<dyn CodecProvider>>) -> Self {
        ProviderRegistry { providers }
    }

    pub fn providers(&self) -> &[Arc<dyn CodecProvider>] {
        &self.providers
    }

    pub fn get(&self, id: ProviderId) -> Option<&Arc<dyn CodecProvider>> {
        self.providers.iter().find(|p| p.id() == id)
    }

    pub fn statuses(&self) -> Vec<ProviderStatus> {
        self.providers.iter().map(|p| p.status()).collect()
    }

    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.providers.iter().flat_map(|p| p.devices()).collect()
    }

    /// 方針に合う能力の一覧（H.264/H.265 のソフトウェア能力は、もしあっても出さない）。
    pub fn capabilities(&self, policy: &Policy) -> Vec<Capability> {
        self.providers
            .iter()
            .flat_map(|p| p.capabilities())
            .filter(|c| capability_allowed(c, policy))
            .collect()
    }

    /// 方針に従って候補のプロバイダーを順に並べる。候補が無ければ理由付きの `Err`。
    pub fn candidates(
        &self,
        codec: Codec,
        direction: Direction,
        resolution: Option<(u32, u32)>,
        policy: &Policy,
    ) -> Result<Vec<Arc<dyn CodecProvider>>> {
        check_policy(codec, policy)?;
        let mut list: Vec<(Arc<dyn CodecProvider>, (u8, usize))> = Vec::new();
        for p in &self.providers {
            let caps = p.capabilities();
            let ok = caps.iter().any(|c| {
                c.codec == codec
                    && c.direction == direction
                    && capability_allowed(c, policy)
                    && resolution.is_none_or(|(w, h)| w <= c.max_width && h <= c.max_height)
            });
            if !ok {
                continue;
            }
            let hw = p.id().is_hardware();
            let class = if hw { 0 } else { 1 };
            let pref = policy
                .prefer
                .iter()
                .position(|x| *x == p.id())
                .unwrap_or(usize::MAX);
            list.push((p.clone(), (class, pref)));
        }
        list.sort_by_key(|(p, key)| (*key, p.id()));
        if list.is_empty() {
            let what = format!("{} {}", codec, direction.as_str());
            return Err(if codec.is_patent_pooled() {
                CodecError::unavailable(format!(
                    "no hardware {what} available (software H.264/H.265 codecs are not used, ADR-0004)"
                ))
            } else if matches!(codec, Codec::Av1 | Codec::Vp8 | Codec::Vp9 | Codec::Opus) {
                CodecError::unavailable(format!("no provider for {what} is available on this node"))
            } else {
                CodecError::unsupported(format!("{what} is not supported"))
            });
        }
        Ok(list.into_iter().map(|(p, _)| p).collect())
    }

    /// 方針に従ってエンコーダーを作る。
    pub fn create_encoder(
        &self,
        config: &EncoderConfig,
        policy: &Policy,
    ) -> Result<(ProviderId, Box<dyn Encoder>)> {
        config.validate()?;
        let cands = self.candidates(
            config.codec(),
            Direction::Encode,
            config.resolution(),
            policy,
        )?;
        try_each(cands, |p| p.create_encoder(config))
    }

    /// 方針に従ってデコーダーを作る。
    pub fn create_decoder(
        &self,
        config: &DecoderConfig,
        policy: &Policy,
    ) -> Result<(ProviderId, Box<dyn Decoder>)> {
        config.validate()?;
        let res = config
            .config
            .as_ref()
            .and_then(|c| c.video)
            .map(|v| (v.width, v.height));
        let cands = self.candidates(config.codec, Direction::Decode, res, policy)?;
        try_each(cands, |p| p.create_decoder(config))
    }
}

fn try_each<T>(
    cands: Vec<Arc<dyn CodecProvider>>,
    mut f: impl FnMut(&dyn CodecProvider) -> Result<T>,
) -> Result<(ProviderId, T)> {
    let mut last = None;
    for p in cands {
        match f(p.as_ref()) {
            Ok(v) => return Ok((p.id(), v)),
            Err(e) if matches!(e.kind, ErrorKind::Unavailable | ErrorKind::Backend) => {
                last = Some(CodecError::new(
                    e.kind,
                    format!("{}: {}", p.id().as_str(), e.message),
                ));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| CodecError::unavailable("no provider")))
}

/// コーデックそのものが方針で使えるか。
pub fn check_policy(codec: Codec, policy: &Policy) -> Result<()> {
    if codec == Codec::Aac {
        return Err(CodecError::unsupported(
            "AAC is passed through only; AAC encode/decode is not provided (ADR-0004)",
        ));
    }
    if policy.royalty_free_only && codec.is_patent_pooled() {
        return Err(CodecError::policy(format!(
            "{codec} is rejected by the royalty-free-only profile (REQ-007); use AV1/VP9/VP8/Opus"
        )));
    }
    Ok(())
}

fn capability_allowed(c: &Capability, policy: &Policy) -> bool {
    if c.codec == Codec::Aac {
        return false;
    }
    if c.codec.is_patent_pooled() && (!c.hardware || !c.provider.is_hardware()) {
        return false;
    }
    if policy.royalty_free_only && c.codec.is_patent_pooled() {
        return false;
    }
    if !policy.allow_software && !c.provider.is_hardware() {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::PixelFormat;
    use crate::{AudioEncoderConfig, Frame, VideoEncoderConfig};
    use senca_core::{AccessUnit, CodecConfig};

    struct Fake {
        id: ProviderId,
        caps: Vec<(Codec, Direction)>,
        fail: Option<ErrorKind>,
    }

    struct FakeEnc;
    impl Encoder for FakeEnc {
        fn codec(&self) -> Codec {
            Codec::H264
        }
        fn codec_config(&self) -> Option<CodecConfig> {
            None
        }
        fn send(&mut self, _: &Frame) -> Result<()> {
            Ok(())
        }
        fn receive(&mut self) -> Result<Option<AccessUnit>> {
            Ok(None)
        }
        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
        fn request_keyframe(&mut self) {}
    }

    impl CodecProvider for Fake {
        fn id(&self) -> ProviderId {
            self.id
        }
        fn status(&self) -> ProviderStatus {
            ProviderStatus {
                provider: self.id,
                available: true,
                detail: String::new(),
            }
        }
        fn devices(&self) -> Vec<DeviceInfo> {
            Vec::new()
        }
        fn capabilities(&self) -> Vec<Capability> {
            self.caps
                .iter()
                .map(|(codec, direction)| Capability {
                    provider: self.id,
                    codec: *codec,
                    direction: *direction,
                    hardware: self.id.is_hardware(),
                    max_width: 4096,
                    max_height: 4096,
                    max_sessions: None,
                    formats: vec![PixelFormat::I420],
                    device_index: 0,
                    device_name: "fake".into(),
                    driver_version: None,
                })
                .collect()
        }
        fn create_encoder(&self, _: &EncoderConfig) -> Result<Box<dyn Encoder>> {
            match self.fail {
                Some(k) => Err(CodecError::new(k, "fail")),
                None => Ok(Box::new(FakeEnc)),
            }
        }
        fn create_decoder(&self, _: &DecoderConfig) -> Result<Box<dyn Decoder>> {
            Err(CodecError::unsupported("x"))
        }
    }

    fn fake(id: ProviderId, caps: &[(Codec, Direction)]) -> Arc<dyn CodecProvider> {
        Arc::new(Fake {
            id,
            caps: caps.to_vec(),
            fail: None,
        })
    }

    #[test]
    fn h264_requires_hardware() {
        // ソフトウェアのプロバイダーが H.264 を名乗っても使わない。
        let r = ProviderRegistry::with_providers(vec![fake(
            ProviderId::Libvpx,
            &[(Codec::H264, Direction::Encode)],
        )]);
        let cfg = EncoderConfig::Video(VideoEncoderConfig::new(
            Codec::H264,
            640,
            480,
            30,
            1_000_000,
        ));
        let e = r.create_encoder(&cfg, &Policy::default()).err().unwrap();
        assert_eq!(e.kind, ErrorKind::Unavailable);
        assert_eq!(e.kind.api_code(), "CAPACITY_UNAVAILABLE");
        assert!(r.capabilities(&Policy::default()).is_empty());

        let r = ProviderRegistry::with_providers(vec![
            fake(ProviderId::Libvpx, &[(Codec::H264, Direction::Encode)]),
            fake(ProviderId::Vaapi, &[(Codec::H264, Direction::Encode)]),
        ]);
        let (id, _) = r.create_encoder(&cfg, &Policy::default()).unwrap();
        assert_eq!(id, ProviderId::Vaapi);
    }

    #[test]
    fn royalty_free_only_rejects() {
        let r = ProviderRegistry::with_providers(vec![fake(
            ProviderId::Nvidia,
            &[(Codec::H264, Direction::Encode)],
        )]);
        let cfg = EncoderConfig::Video(VideoEncoderConfig::new(
            Codec::H264,
            640,
            480,
            30,
            1_000_000,
        ));
        let e = r
            .create_encoder(&cfg, &Policy::royalty_free())
            .err()
            .unwrap();
        assert_eq!(e.kind, ErrorKind::PolicyRejected);
        assert!(r.capabilities(&Policy::royalty_free()).is_empty());
        assert_eq!(r.capabilities(&Policy::default()).len(), 1);
    }

    #[test]
    fn aac_unsupported() {
        let r = ProviderRegistry::with_providers(vec![]);
        let mut a = AudioEncoderConfig::opus(48000, 2, 64000);
        a.codec = Codec::Aac;
        let e = r
            .create_encoder(&EncoderConfig::Audio(a), &Policy::default())
            .err()
            .unwrap();
        assert_eq!(e.kind, ErrorKind::Unsupported);
    }

    #[test]
    fn hardware_first_and_fallback() {
        let caps = [(Codec::Av1, Direction::Encode)];
        let busy: Arc<dyn CodecProvider> = Arc::new(Fake {
            id: ProviderId::Nvidia,
            caps: caps.to_vec(),
            fail: Some(ErrorKind::Unavailable),
        });
        let r = ProviderRegistry::with_providers(vec![
            fake(ProviderId::SvtAv1, &caps),
            busy,
            fake(ProviderId::Amf, &caps),
        ]);
        let c = r
            .candidates(Codec::Av1, Direction::Encode, None, &Policy::default())
            .unwrap();
        let ids: Vec<_> = c.iter().map(|p| p.id()).collect();
        assert_eq!(
            ids,
            vec![ProviderId::Nvidia, ProviderId::Amf, ProviderId::SvtAv1]
        );
        let cfg =
            EncoderConfig::Video(VideoEncoderConfig::new(Codec::Av1, 640, 480, 30, 1_000_000));
        // NVIDIA は枠が無いので AMF に落ちる。
        assert_eq!(
            r.create_encoder(&cfg, &Policy::default()).unwrap().0,
            ProviderId::Amf
        );
        // 優先順の指定
        let p = Policy {
            prefer: vec![ProviderId::Amf],
            ..Default::default()
        };
        let ids: Vec<_> = r
            .candidates(Codec::Av1, Direction::Encode, None, &p)
            .unwrap()
            .iter()
            .map(|p| p.id())
            .collect();
        assert_eq!(ids[0], ProviderId::Amf);
        // ソフトウェア禁止
        let p = Policy {
            allow_software: false,
            ..Default::default()
        };
        assert_eq!(
            r.candidates(Codec::Av1, Direction::Encode, None, &p)
                .unwrap()
                .len(),
            2
        );
        // 解像度が大きすぎる
        assert!(
            r.candidates(
                Codec::Av1,
                Direction::Encode,
                Some((8192, 8192)),
                &Policy::default()
            )
            .is_err()
        );
    }

    #[test]
    fn missing_rf_codec_is_unavailable() {
        let r = ProviderRegistry::with_providers(vec![]);
        let e = r
            .candidates(Codec::Vp9, Direction::Decode, None, &Policy::default())
            .err()
            .unwrap();
        assert_eq!(e.kind, ErrorKind::Unavailable);
        let e = r
            .candidates(Codec::WebVtt, Direction::Decode, None, &Policy::default())
            .err()
            .unwrap();
        assert_eq!(e.kind, ErrorKind::Unsupported);
    }

    #[test]
    fn probe_never_panics() {
        // ライブラリの無いディレクトリだけを見る。
        let o = ProbeOptions {
            libs: LibrarySearch {
                dirs: vec!["/nonexistent".into()],
                dirs_only: true,
            },
            disabled: vec![],
            drm_nodes: Some(vec!["/nonexistent/renderD128".into()]),
        };
        let r = ProviderRegistry::probe(&o);
        assert_eq!(r.providers().len(), ProviderId::ALL.len());
        for s in r.statuses() {
            assert!(!s.available, "{:?}", s);
            assert!(!s.detail.is_empty());
        }
        assert!(r.capabilities(&Policy::default()).is_empty());
        let cfg = EncoderConfig::Video(VideoEncoderConfig::new(
            Codec::H264,
            640,
            480,
            30,
            1_000_000,
        ));
        assert_eq!(
            r.create_encoder(&cfg, &Policy::default())
                .err()
                .unwrap()
                .kind,
            ErrorKind::Unavailable
        );
    }

    #[test]
    fn disable_env_parsing() {
        let mut o = ProbeOptions::default();
        o.disabled.push(ProviderId::Nvidia);
        assert!(!o.is_enabled(ProviderId::Nvidia));
        assert!(o.is_enabled(ProviderId::Dav1d));
    }
}
