//! 能力と装置の情報（REQ-004「能力の検出」、`GET /devices`）。

use crate::frame::PixelFormat;
use senca_core::Codec;

/// プロバイダーの識別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProviderId {
    /// NVIDIA NVENC / NVDEC。
    Nvidia,
    /// Intel oneVPL（Quick Sync）。
    IntelVpl,
    /// AMD AMF。
    Amf,
    /// VA-API（Intel / AMD、Linux）。
    Vaapi,
    /// dav1d（AV1 復号、ソフトウェア）。
    Dav1d,
    /// libvpx（VP8/VP9、ソフトウェア）。
    Libvpx,
    /// SVT-AV1（AV1 符号化、ソフトウェア）。
    SvtAv1,
    /// libopus（ソフトウェア）。
    Opus,
}

impl ProviderId {
    pub const ALL: [ProviderId; 8] = [
        ProviderId::Nvidia,
        ProviderId::IntelVpl,
        ProviderId::Amf,
        ProviderId::Vaapi,
        ProviderId::Dav1d,
        ProviderId::Libvpx,
        ProviderId::SvtAv1,
        ProviderId::Opus,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProviderId::Nvidia => "nvidia",
            ProviderId::IntelVpl => "intel-vpl",
            ProviderId::Amf => "amd-amf",
            ProviderId::Vaapi => "vaapi",
            ProviderId::Dav1d => "dav1d",
            ProviderId::Libvpx => "libvpx",
            ProviderId::SvtAv1 => "svt-av1",
            ProviderId::Opus => "opus",
        }
    }

    pub fn parse(s: &str) -> Option<ProviderId> {
        ProviderId::ALL.into_iter().find(|p| p.as_str() == s)
    }

    /// ハードウェア（GPU ベンダーの API）か。
    pub fn is_hardware(self) -> bool {
        matches!(
            self,
            ProviderId::Nvidia | ProviderId::IntelVpl | ProviderId::Amf | ProviderId::Vaapi
        )
    }

    pub fn to_u8(self) -> u8 {
        ProviderId::ALL
            .iter()
            .position(|p| *p == self)
            .expect("listed") as u8
    }

    pub fn from_u8(v: u8) -> Option<ProviderId> {
        ProviderId::ALL.get(v as usize).copied()
    }
}

/// 符号化か復号か。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Encode,
    Decode,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Encode => "encode",
            Direction::Decode => "decode",
        }
    }
}

/// 検出した装置（GPU・ライブラリ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub provider: ProviderId,
    /// プロバイダー内の装置番号（GPU の番号、DRM の render node の順）。
    pub index: u32,
    /// 装置名（GPU の型番、ソフトウェアならライブラリ名）。
    pub name: String,
    /// ドライバー・ライブラリの版（分かるとき）。
    pub driver_version: Option<String>,
    /// 装置のパス（`/dev/dri/renderD128` など）。
    pub path: Option<String>,
    pub hardware: bool,
}

/// 1 つのコーデック・方向の能力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub provider: ProviderId,
    pub codec: Codec,
    pub direction: Direction,
    pub hardware: bool,
    /// 最大解像度（分からなければプロバイダーの保守的な値）。
    pub max_width: u32,
    pub max_height: u32,
    /// 同時セッション数の上限（分かるとき。NVENC の消費者向け GPU の制限など）。
    pub max_sessions: Option<u32>,
    /// 入力（符号化）・出力（復号）できる画素形式。
    pub formats: Vec<PixelFormat>,
    /// どの装置の能力か（[`DeviceInfo::index`] と同じ値）。
    pub device_index: u32,
    pub device_name: String,
    pub driver_version: Option<String>,
}

impl Capability {
    /// ロイヤリティフリーとして提供されているコーデックか（REQ-007）。
    pub fn royalty_free(&self) -> bool {
        !self.codec.is_patent_pooled()
    }
}

/// プロバイダーの検出結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderStatus {
    pub provider: ProviderId,
    pub available: bool,
    /// 使えない理由（ライブラリが無い、装置が無い、版が古いなど）。使えるときは読み込んだライブラリの説明。
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        for p in ProviderId::ALL {
            assert_eq!(ProviderId::parse(p.as_str()), Some(p));
            assert_eq!(ProviderId::from_u8(p.to_u8()), Some(p));
        }
        assert!(ProviderId::Nvidia.is_hardware());
        assert!(!ProviderId::Dav1d.is_hardware());
        assert_eq!(ProviderId::from_u8(8), None);
    }
}
