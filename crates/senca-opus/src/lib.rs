//! Opus の識別ヘッダー（OpusHead）とパケットの枠組み。sans-io。音声の符号化・復号はしない。
//!
//! - [`head`]: OpusHead（RFC 7845）の解析・組み立てと `CodecConfig`
//! - [`packet`]: TOC とフレーム分割、パケットの長さ（RFC 6716 3.1〜3.4）
//!
//! WebRTC・MP4 の codecs 文字列は `"opus"`。

pub mod error;
pub mod head;
pub mod packet;

pub use error::{Error, Result};
pub use head::{ChannelMapping, OpusHead};
pub use packet::{
    Bandwidth, MAX_FRAME_BYTES, MAX_PACKET_SAMPLES, Mode, Packet, Toc, frame_count, packet_samples,
    parse_packet,
};

/// RFC 6381 の codecs 文字列。
pub const CODECS_STRING: &str = "opus";
