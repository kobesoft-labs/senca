//! SENCA のコーデック抽象（仕様 11 章の `CodecProvider`）と、実行時に読み込むプロバイダー群。
//!
//! # 方針（ADR-0004）
//!
//! - H.264 / H.265 の符号化・復号は GPU ベンダーのハードウェア API（NVENC/NVDEC、oneVPL、AMF、VA-API）だけで行う。
//!   ソフトウェアの H.264/H.265 コーデックは読み込まない。ハードウェアが無ければ [`ErrorKind::Unavailable`]
//!   （制御 API の `CAPACITY_UNAVAILABLE`）を返し、黙って劣化させない。
//! - AAC は符号化・復号しない（[`ErrorKind::Unsupported`]）。
//! - ロイヤリティフリーとして提供されているソフトウェアコーデック（dav1d、SVT-AV1、libvpx、libopus）は使ってよい。
//! - どのライブラリも `libloading`（dlopen）で実行時に読み込む。ビルドに SDK は要らず、GPU やライブラリが
//!   無い環境でも検出は「使えない」を返すだけで落ちない。
//! - このクレートは codec worker（`senca-codec-worker`）だけが使う。ノード本体は外部コーデックを読み込まない（仕様 3.1・9.4）。
//!
//! # `unsafe` について
//!
//! ワークスペースは `unsafe_code = "deny"`。C ABI のライブラリを呼ぶにはどうしても `unsafe` が要るので、
//! このクレートだけを例外にする（DEVELOPMENT.md「FFI の crate だけ例外」）。ただし例外の範囲は
//! クレート全体ではなく、各プロバイダーの `ffi` モジュール（と共通の [`dl`]）に `#[allow(unsafe_code)]` を付けて
//! 閉じ込める。それ以外のモジュールでは引き続き `unsafe` はコンパイルエラーになる。
//! `ffi` モジュールの `unsafe` ブロックには、それぞれ前提（SAFETY）をコメントで書く。
//!
//! # モジュール
//!
//! - [`error`] — エラー型（制御 API のエラーコードへの対応付き）
//! - [`frame`] — 生のフレーム（I420・NV12・P010、PCM f32/i16）
//! - [`config`] — エンコーダー・デコーダーの設定
//! - [`caps`] — 能力・装置の情報（`GET /devices`）
//! - [`provider`] — `CodecProvider`・`Encoder`・`Decoder` の trait
//! - [`registry`] — プロバイダーの検出・選択（ハードウェア優先、H.264/H.265 はハードウェアのみ、ロイヤリティフリー限定）
//! - [`bitstream`] — 内部形式（ADR-0003）との変換（Annex B ⇔ 長さ前置、avcC/hvcC/av1C、OpusHead）
//! - [`providers`] — 各プロバイダーの実装

pub mod bitstream;
pub mod caps;
pub mod config;
#[allow(unsafe_code)]
pub mod dl;
pub mod error;
pub mod frame;
pub mod provider;
pub mod providers;
pub mod registry;

pub use caps::{Capability, DeviceInfo, Direction, ProviderId, ProviderStatus};
pub use config::{
    AudioEncoderConfig, DecoderConfig, EncoderConfig, OpusApplication, RateControl,
    VideoEncoderConfig, VideoProfile,
};
pub use error::{CodecError, ErrorKind, Result};
pub use frame::{AudioFrame, Frame, PixelFormat, Plane, SampleFormat, VideoFrame};
pub use provider::{CodecProvider, Decoder, Encoder};
pub use registry::{Policy, ProbeOptions, ProviderRegistry};
