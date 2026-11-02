//! 映像の合成と音声のミキシング（REQ-006 R6-7、REQ-008）。仕様 2.1 により自前で実装する。
//!
//! - [`scene`] — キャンバス・レイヤー・シーンと、よく使う配置（PinP・左右・格子）
//! - [`render`] — 合成器（ソースの保持・スレート・トランジション・帯ごとの並列描画）
//! - [`mixer`] — 音声ミキサー（再標本化・音量・パン・ミュート・ソロ・遅延・リミッター・メーター）
//! - [`resample`] — 多相の窓付き sinc による再標本化

pub mod error;
pub mod mixer;
pub mod render;
pub mod resample;
pub mod scene;

pub use error::{ComposeError, Result};
pub use mixer::{
    AudioMixer, InputConfig, InputStats, LimiterConfig, Meter, MixerConfig, MixerMeters,
    OUTPUT_RATE,
};
pub use render::{Compositor, OverlayImage, Slate, SourcePolicy, Transition, TransitionKind};
pub use resample::Resampler;
pub use scene::{
    Border, Canvas, Corner, Crop, DestRect, FitMode, ImageId, Layer, LayerContent, Scene, SourceId,
    layout,
};
