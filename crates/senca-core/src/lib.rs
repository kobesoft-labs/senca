//! SENCA の共通部品。I/O を持たない。
//!
//! - `id`: 推測困難な 128 bit ID
//! - `time`: 時間軸の換算（オーバーフロー検査付き）と折り返しの拡張
//! - `media`: 共通メディアモデル（仕様 4.2 の EncodedAccessUnit）
//! - `bits`: ビット単位の読み書き（Exp-Golomb を含む）
//! - `rfc3339`: UTC の RFC 3339 表記

pub mod bits;
pub mod id;
pub mod media;
pub mod rfc3339;
pub mod time;

pub use id::Id128;
pub use media::{AccessUnit, Codec, CodecConfig, MediaKind, TrackId};
pub use time::{Rescale, Timebase};
