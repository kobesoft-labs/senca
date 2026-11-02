//! MPEG-2 TS（ISO/IEC 13818-1）の多重化・分離。I/O を持たない（sans-io）。
//!
//! - [`TsMuxer`]: [`AccessUnit`](senca_core::AccessUnit) を 188 バイトの TS パケット列にする。
//!   PAT/PMT、PES（PTS/DTS）、PCR、連続性カウンター、AUD とパラメーターセットの挿入、
//!   ADTS 化（AAC）、Opus の制御ヘッダーを扱う。
//! - [`TsDemuxer`]: バイト列を入れると [`DemuxEvent`] を返す。同期の検出と再同期、
//!   PAT/PMT（CRC 検査）、PES の組み立て（上限付き）、33 bit 時刻の拡張、
//!   連続性カウンターによる欠落の検出、内部表現（ADR-0003）への変換を行う。
//!
//! 対応する stream_type: H.264 (0x1B)、H.265 (0x24)、AAC ADTS (0x0F)、
//! Opus (0x06 + registration_descriptor "Opus"、opus-codec.org の "Encapsulation of Opus in MPEG-2 TS")。

mod codec;
mod crc;
mod demux;
mod mux;
mod psi;

pub use crc::crc32_mpeg2;
pub use demux::{DemuxEvent, DemuxStats, DemuxerConfig, TsDemuxer};
pub use mux::{MuxError, MuxerConfig, TsMuxer};

/// TS パケットの長さ。
pub const TS_PACKET_SIZE: usize = 188;
/// 同期バイト。
pub const SYNC_BYTE: u8 = 0x47;

/// stream_type の値。
pub mod stream_type {
    pub const AAC_ADTS: u8 = 0x0F;
    pub const H264: u8 = 0x1B;
    pub const H265: u8 = 0x24;
    pub const PRIVATE_PES: u8 = 0x06;
}
