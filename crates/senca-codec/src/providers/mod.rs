//! 各プロバイダーの実装。どれも実行時にライブラリを読み込み、無ければ「使えない」を返す。
//!
//! | モジュール | プロバイダー | 区分 |
//! |---|---|---|
//! | [`nvidia`] | NVENC / NVDEC | ハードウェア |
//! | [`vpl`] | Intel oneVPL | ハードウェア |
//! | [`amf`] | AMD AMF | ハードウェア |
//! | [`vaapi`] | VA-API | ハードウェア |
//! | [`dav1d`] | dav1d（AV1 復号） | ソフトウェア（ロイヤリティフリー） |
//! | [`libvpx`] | libvpx（VP8/VP9） | ソフトウェア（ロイヤリティフリー） |
//! | [`svtav1`] | SVT-AV1（AV1 符号化） | ソフトウェア（ロイヤリティフリー） |
//! | [`opus`] | libopus | ソフトウェア（ロイヤリティフリー） |

use crate::caps::ProviderId;
use crate::provider::CodecProvider;
use crate::registry::ProbeOptions;
use std::sync::Arc;

pub mod amf;
pub mod common;
pub mod dav1d;
pub mod libvpx;
pub mod nvidia;
pub mod opus;
pub mod svtav1;
pub mod vaapi;
pub mod vpl;

/// すべての組み込みプロバイダーを検出する（順はハードウェア、ソフトウェア）。
/// 無効にしたものも「使えない」として一覧に残す（`GET /devices` で理由を出すため）。
pub fn builtin(options: &ProbeOptions) -> Vec<Arc<dyn CodecProvider>> {
    ProviderId::ALL
        .iter()
        .map(|&id| {
            if !options.is_enabled(id) {
                return common::UnavailableProvider::arc(id, "disabled by configuration");
            }
            probe_one(id, options)
        })
        .collect()
}

/// 1 つのプロバイダーを検出する。
pub fn probe_one(id: ProviderId, options: &ProbeOptions) -> Arc<dyn CodecProvider> {
    match id {
        ProviderId::Nvidia => nvidia::probe(options),
        ProviderId::IntelVpl => vpl::probe(options),
        ProviderId::Amf => amf::probe(options),
        ProviderId::Vaapi => vaapi::probe(options),
        ProviderId::Dav1d => dav1d::probe(options),
        ProviderId::Libvpx => libvpx::probe(options),
        ProviderId::SvtAv1 => svtav1::probe(options),
        ProviderId::Opus => opus::probe(options),
    }
}
