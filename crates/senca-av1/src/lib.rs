//! AV1 のビットストリーム解析。sans-io。符号化・復号はしない。
//!
//! - [`obu`]: OBU ヘッダー、leb128、OBU 列の分割、Temporal Delimiter の除去（ADR-0003 の内部表現）
//! - [`seq`]: シーケンスヘッダー、フレームヘッダーの先頭、キーフレーム判定、codecs 文字列
//! - [`record`]: AV1CodecConfigurationRecord と `CodecConfig`

pub mod error;
pub mod obu;
pub mod record;
pub mod seq;

pub use error::{Error, Result};
pub use obu::{
    Obu, ObuHeader, ObuType, Obus, obus, read_leb128, strip_temporal_delimiters, write_leb128,
    write_obu,
};
pub use record::{Av1CodecConfigurationRecord, codec_config};
pub use seq::{
    ColorConfig, FrameHeaderStart, FrameType, OperatingPoint, SequenceHeader, TimingInfo,
    is_keyframe,
};
