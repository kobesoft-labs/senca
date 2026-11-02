//! SENCA のキャプチャ機器（REQ-004「キャプチャボード」）。
//!
//! 機器の SDK・ドライバーはこのクレートの入力アダプターの中に閉じ込め、得た映像・音声は共通メディアモデル
//! （`senca_codec::VideoFrame` の I420、`senca_codec::AudioFrame` のインターリーブ PCM）に変換する。
//! 未圧縮の映像は [`session::CaptureSession`] が `senca_codec::Encoder` で符号化し、
//! `senca_core::AccessUnit` と `CodecConfig` にして返す（stream に publish できる形）。
//!
//! # 機器の種類（バックエンド）
//!
//! | バックエンド | 対象 | 読み込み方 | 検証 |
//! |---|---|---|---|
//! | [`v4l2`] | UVC の Web カメラ、HDMI の USB キャプチャ | `ioctl`（`libc`） | 未検証（実機・v4l2loopback なし。模擬で試験） |
//! | [`alsa`] | USB マイク、キャプチャ機器の音声 | `libasound.so.2` を dlopen | 未検証（実機なし） |
//! | [`decklink`] | Blackmagic DeckLink（Desktop Video） | `libDeckLinkAPI.so` を dlopen、COM の vtable を手で呼ぶ | 未検証（実機・ドライバーなし） |
//! | [`synthetic`] | 試験用の決定的な試験パターンと音 | なし | 検証済み（e2e 試験） |
//!
//! # 隔離
//!
//! 機器の SDK（特に DeckLink）はノードのプロセスに読み込まない。ノードは別プロセスの
//! `senca-capture-worker`（[`worker`]）を起動し、[`client`] で符号化済みの AU を受け取る。
//! worker の上のプロトコルは codec worker（`senca_codec_worker::protocol`）のフレーム形式とメッセージをそのまま使う。
//!
//! # `unsafe` について
//!
//! ワークスペースは `unsafe_code = "deny"`。`ioctl`・`mmap`・dlopen した C/C++ の関数を呼ぶ
//! 各バックエンドの `ffi` モジュール（[`v4l2::ffi`]・[`alsa::ffi`]・[`decklink::ffi`]）と、時計を読む [`clock`]
//! にだけ `#[allow(unsafe_code)]` を付ける。各 `unsafe` には前提（SAFETY）をコメントで書く。
//!
//! # モジュール
//!
//! - [`error`] — エラー型（制御 API のエラーコードへの対応付き）
//! - [`types`] — 機器の説明・能力・要求・取り込んだフレーム
//! - [`device`] — `CaptureDevice`・`CaptureBackend` の trait と、検出をまとめる [`device::DeviceRegistry`]
//! - [`negotiate`] — 形式・解像度・フレームレートの取り決め（純粋な関数）
//! - [`convert`] — YUYV・UYVY・NV12・MJPEG → I420 の変換（自前、仕様 2.1）
//! - [`h264`] — UVC カメラの H.264 の素通し（Annex B → 長さ前置、avcC の生成。復号はしない）
//! - [`session`] — 機器から読み、符号化して AU にする
//! - [`spec`] — worker に渡す取り込みの指定（JSON）
//! - [`worker`] — `senca-capture-worker` の本体
//! - [`client`] — 親（ノード）側で worker を起動して使う

pub mod alsa;
pub mod client;
#[allow(unsafe_code)]
pub mod clock;
pub mod convert;
pub mod decklink;
pub mod device;
pub mod error;
pub mod h264;
pub mod negotiate;
pub mod session;
pub mod spec;
pub mod synthetic;
pub mod types;
pub mod v4l2;
pub mod worker;

pub use device::{CaptureBackend, CaptureDevice, DeviceRegistry, ProbeOptions};
pub use error::{CaptureError, ErrorKind, Result};
pub use session::{CaptureSession, EncodedOutput, SessionStats};
pub use spec::CaptureSpec;
pub use types::{
    AudioCapability, AudioMode, AudioRequest, Backend, BackendStatus, CaptureConfig, CapturedFrame,
    DeviceDescriptor, Fps, IntervalRange, Negotiated, RawFormat, SizeRange, Timestamps,
    Verification, VideoCapability, VideoMode, VideoRequest,
};
