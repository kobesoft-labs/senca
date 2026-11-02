//! FLV のタグ本体（RTMP の audio/video/data message と共通）と FLV ファイルの扱い。I/O を持たない。
//!
//! - [`tag`]: 映像・音声タグ本体の解釈（従来形式と Enhanced RTMP v2）
//! - [`build`]: AccessUnit・CodecConfig からタグ本体を作る（[`build::FlvMuxer`]）
//! - [`ingest`]: タグから AccessUnit・CodecConfig を作る（[`ingest::FlvIngest`]）
//! - [`file`]: FLV ファイルのヘッダー・タグの書き出しと逐次読み込み
//! - [`script`]: script data（onMetaData など）

pub mod build;
pub mod file;
pub mod ingest;
pub mod script;
pub mod tag;

pub use build::{FlvMuxer, OutTag, TagBody};
pub use file::{FlvHeader, FlvReader, FlvTag};
pub use ingest::{FlvIngest, IngestEvent, IngestOptions};
pub use tag::{AudioPacket, AudioTag, TagType, VideoFrameType, VideoPacket, VideoTag};

use senca_core::Codec;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// 本体が途中で終わっている。
    Truncated,
    /// 形式に合わない（理由は固定文字列）。
    Malformed(&'static str),
    /// 従来形式の未対応 CodecID。
    UnsupportedCodecId(u8),
    /// 従来形式の未対応 SoundFormat。
    UnsupportedSoundFormat(u8),
    /// Enhanced RTMP の未対応 FourCC。
    UnsupportedFourCc([u8; 4]),
    /// FLV で運べない（またはまだ対応していない）コーデック。
    UnsupportedCodec(Codec),
    /// Enhanced RTMP のマルチトラックはまだ扱わない。
    MultitrackUnsupported,
    /// 同じトラックの途中でコーデックが変わった。
    CodecChanged,
    /// AU が参照する設定が登録されていない。
    MissingConfig,
    /// 時刻が FLV で表せない（CTS が 24 bit を超える、換算で溢れるなど）。
    TimestampOutOfRange,
    /// タグが 16 MiB − 1 を超える。
    TooLarge,
    Amf(senca_amf0::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => f.write_str("flv: truncated tag"),
            Error::Malformed(m) => write!(f, "flv: {m}"),
            Error::UnsupportedCodecId(id) => write!(f, "flv: unsupported video codec id {id}"),
            Error::UnsupportedSoundFormat(id) => write!(f, "flv: unsupported sound format {id}"),
            Error::UnsupportedFourCc(c) => {
                write!(
                    f,
                    "flv: unsupported fourcc {}",
                    String::from_utf8_lossy(c).escape_debug()
                )
            }
            Error::UnsupportedCodec(c) => write!(f, "flv: codec {c} not supported"),
            Error::MultitrackUnsupported => {
                f.write_str("flv: enhanced rtmp multitrack is not supported")
            }
            Error::CodecChanged => f.write_str("flv: codec changed within a track"),
            Error::MissingConfig => f.write_str("flv: missing codec configuration"),
            Error::TimestampOutOfRange => f.write_str("flv: timestamp out of range"),
            Error::TooLarge => f.write_str("flv: tag too large"),
            Error::Amf(e) => write!(f, "flv: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<senca_amf0::Error> for Error {
    fn from(e: senca_amf0::Error) -> Self {
        Error::Amf(e)
    }
}
