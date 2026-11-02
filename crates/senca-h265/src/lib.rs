//! H.265（HEVC）のビットストリーム解析。sans-io。符号化・復号はしない（ADR-0004）。
//!
//! - [`annexb`]: Annex B（開始符号）と長さ前置（HVCC）の分割・変換
//! - [`nal`]: NAL ヘッダー（2 バイト）、emulation prevention、IRAP の判定
//! - [`sps`]: VPS・SPS・PPS とスライスセグメントヘッダーの先頭
//! - [`ptl`]: profile_tier_level と codecs 文字列（ISO/IEC 14496-15 附属書 E）
//! - [`record`]: HEVCDecoderConfigurationRecord と `CodecConfig`

pub mod annexb;
mod bits;
pub mod error;
pub mod nal;
pub mod ptl;
pub mod record;
pub mod sps;

pub use annexb::{
    AnnexBNals, LengthPrefixedNals, annexb_nals, annexb_nals_bytes, annexb_to_length_prefixed,
    length_prefixed_nals, length_prefixed_nals_bytes, length_prefixed_to_annexb,
    write_length_prefixed,
};
pub use error::{Error, Result};
pub use nal::{
    NalHeader, NalUnitType, add_emulation_prevention, contains_irap, is_keyframe_annexb,
    is_keyframe_length_prefixed, nal_unit_type, remove_emulation_prevention,
};
pub use ptl::{ProfileTierLevel, parse_codecs_string};
pub use record::{HevcDecoderConfigurationRecord, NalArray, codec_config};
pub use sps::{Pps, SliceSegmentHeaderStart, Sps, Vps, Vui};
