//! 内部形式（ADR-0003）とコーデックライブラリの形式の変換。I/O を持たない。
//!
//! - [`nal`] — Annex B ⇔ 長さ前置、エミュレーション防止バイトの除去
//! - [`h264`] — SPS の解析、AVCDecoderConfigurationRecord
//! - [`h265`] — SPS の解析、HEVCDecoderConfigurationRecord
//! - [`av1`] — OBU の解析、Temporal Delimiter の除去、av1C
//! - [`opus`] — OpusHead
//!
//! H.264/H.265 の解析は形式変換のためだけで、符号化・復号ではない（ADR-0004 の 2）。

pub mod av1;
pub mod h264;
pub mod h265;
pub mod nal;
pub mod opus;
