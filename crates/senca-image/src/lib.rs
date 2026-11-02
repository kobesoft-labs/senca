//! SENCA の画像処理の基礎（仕様 2.1: リサイズ・色変換・合成の部品は自前）。
//!
//! - [`frame`] — 平面・I420・NV12・RGB/RGBA・f32 の平面
//! - [`color`] — YUV ↔ RGB（BT.601 / BT.709、限定 / 全範囲）
//! - [`scale`] — 拡大縮小（双線形・双三次・Lanczos3・面積平均）、切り抜きつき
//! - [`geom`] — 切り抜き・余白・回転・反転
//! - [`filter`] — ぼかし・モルフォロジー・Sobel
//! - [`png`]・[`jpeg`]: 符号化と復号（[`deflate`] は自前の DEFLATE/zlib）
//! - [`metrics`] — PSNR・SSIM・鮮明さ・黒／白／平坦の判定
//! - [`thumb`] — サムネイルの選択（R6-9）
//!
//! 外部の画像ライブラリは使わない。`unsafe` も使わない。

pub mod checksum;
pub mod color;
pub mod deflate;
pub mod error;
pub mod filter;
pub mod frame;
pub mod geom;
pub mod jpeg;
pub mod metrics;
pub mod png;
pub mod scale;
pub mod thumb;

pub use color::{ColorMatrix, ColorRange, YuvSpec};
pub use error::{ImageError, Result};
pub use frame::{
    GrayImage, I420Frame, Limits, Nv12Frame, Plane, PlaneF32, Rect, RectF, RgbImage, RgbaImage,
    YuvColor,
};
pub use scale::ScaleFilter;
