//! ISO BMFF（ISO/IEC 14496-12）の書き出しと最小限の読み取り。I/O を持たない。
//!
//! - [`write_init_segment`]: 初期化セグメント（ftyp + moov、mvex/trex 付き）。
//! - [`FragmentWriter`]: メディアフラグメント（moof + mdat、trun は version 1 で符号付き composition offset）。
//! - [`SampleQueue`]: [`AccessUnit`](senca_core::AccessUnit) を次の AU の DTS から長さを決めて [`Sample`] にする。
//! - [`Mp4Writer`]: 断片化しない MP4（moov を先頭に置く faststart。録画の書き出し用）。
//! - [`Mp4Reader`] / [`read_file`]: 初期化セグメントとフラグメント（と断片化しない MP4 の stbl）を読む。
//!
//! サンプル記述: avc1(avcC)、hvc1(hvcC)、av01(av1C)、vp09(vpcC)、mp4a(esds)、Opus(dOps)。
//! CMAF への適合は主張しない（仕様 5.4）。

mod bmff;
mod entry;
mod error;
mod fragment;
mod init;
mod mp4;
mod read;

pub use error::Mp4Error;
pub use fragment::{FragmentWriter, Sample, SampleQueue, TrackRun};
pub use init::{TrackSpec, write_init_segment};
pub use mp4::{Mp4Writer, SampleInfo};
pub use read::{InitSegment, Mp4Reader, Mp4Track, ReadSample, parse_init, read_file};
