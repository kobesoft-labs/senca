//! H.264（AVC）のビットストリーム解析。sans-io。符号化・復号はしない（ADR-0004）。
//!
//! - [`annexb`]: Annex B（開始符号）と長さ前置（AVCC）の分割・変換
//! - [`nal`]: NAL ヘッダー、emulation prevention、キーフレーム判定
//! - [`sps`] / [`pps`] / [`slice`]: パラメーターセットとスライスヘッダーの解析
//! - [`record`]: AVCDecoderConfigurationRecord と `CodecConfig`

pub mod annexb;
mod bits;
pub mod error;
pub mod nal;
pub mod pps;
pub mod record;
pub mod slice;
pub mod sps;

pub use annexb::{
    AnnexBNals, LengthPrefixedNals, annexb_nals, annexb_nals_bytes, annexb_to_length_prefixed,
    length_prefixed_nals, length_prefixed_nals_bytes, length_prefixed_to_annexb,
    write_length_prefixed,
};
pub use error::{Error, Result};
pub use nal::{
    NalHeader, NalUnitType, add_emulation_prevention, contains_idr, is_keyframe_annexb,
    is_keyframe_length_prefixed, nal_unit_type, remove_emulation_prevention,
};
pub use pps::Pps;
pub use record::{AvcConfigExt, AvcDecoderConfigurationRecord, codec_config};
pub use slice::{SliceHeader, SliceType};
pub use sps::{Sps, Vui, codecs_string};
