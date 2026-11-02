//! 共通メディアモデル（仕様 4.2）。
//!
//! 内部表現（ADR-0003）:
//! - H.264 / H.265: NAL ユニットを 4 バイトの長さ前置で並べたもの（AVCC/HVCC 形式）。
//!   パラメーターセットは [`CodecConfig`] に持ち、AU には原則入れない（入っていても良い）。
//! - AAC: raw_data_block（ADTS ヘッダーなし）。設定は AudioSpecificConfig。
//! - Opus: Opus パケット 1 つ。
//! - AV1: Low Overhead Bitstream Format の OBU 列（Temporal Delimiter なし）。
//! - VP8 / VP9: フレーム 1 つ（VP9 はスーパーフレームを含みうる）。
//! - G.711: 8 bit サンプル列。

use crate::time::Timebase;
use bytes::Bytes;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Video,
    Audio,
    Subtitle,
    Data,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    H265,
    Av1,
    Vp8,
    Vp9,
    Aac,
    Opus,
    Pcma,
    Pcmu,
    /// WebVTT の字幕断片。
    WebVtt,
}

impl Codec {
    pub fn kind(self) -> MediaKind {
        match self {
            Codec::H264 | Codec::H265 | Codec::Av1 | Codec::Vp8 | Codec::Vp9 => MediaKind::Video,
            Codec::Aac | Codec::Opus | Codec::Pcma | Codec::Pcmu => MediaKind::Audio,
            Codec::WebVtt => MediaKind::Subtitle,
        }
    }

    /// 特許プールが権利を主張しているコーデック（REQ-007 のロイヤリティフリー・プロファイルで拒否する）。
    pub fn is_patent_pooled(self) -> bool {
        matches!(self, Codec::H264 | Codec::H265 | Codec::Aac)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
            Codec::Av1 => "av1",
            Codec::Vp8 => "vp8",
            Codec::Vp9 => "vp9",
            Codec::Aac => "aac",
            Codec::Opus => "opus",
            Codec::Pcma => "pcma",
            Codec::Pcmu => "pcmu",
            Codec::WebVtt => "webvtt",
        }
    }

    pub fn parse(s: &str) -> Option<Codec> {
        Some(match s {
            "h264" => Codec::H264,
            "h265" => Codec::H265,
            "av1" => Codec::Av1,
            "vp8" => Codec::Vp8,
            "vp9" => Codec::Vp9,
            "aac" => Codec::Aac,
            "opus" => Codec::Opus,
            "pcma" => Codec::Pcma,
            "pcmu" => Codec::Pcmu,
            "webvtt" => Codec::WebVtt,
            _ => return None,
        })
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// stream の中のトラック番号（0 から）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackId(pub u32);

/// コーデックの設定。世代（`generation`）は同じトラックで設定が変わるたびに増える。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecConfig {
    pub codec: Codec,
    pub generation: u32,
    /// H.264: AVCDecoderConfigurationRecord、H.265: HEVCDecoderConfigurationRecord、
    /// AAC: AudioSpecificConfig、AV1: AV1CodecConfigurationRecord、Opus: OpusHead（RFC 7845 の識別ヘッダー）、
    /// VP8/VP9・G.711: 空でもよい（VP9 は vpcC があれば入れる）。
    pub record: Bytes,
    pub video: Option<VideoParams>,
    pub audio: Option<AudioParams>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoParams {
    pub width: u32,
    pub height: u32,
    /// RFC 6381 の codecs 文字列を作るための値（H.264 なら profile_idc・constraint・level_idc）。
    pub profile: u8,
    pub compat: u8,
    pub level: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioParams {
    pub sample_rate: u32,
    pub channels: u8,
    /// AAC の audio object type（2 = LC）。他は 0。
    pub object_type: u8,
}

/// 符号化済みのアクセスユニット（仕様 4.2 の EncodedAccessUnit）。payload は参照カウントで共有する（REQ-010 R10-7）。
#[derive(Debug, Clone)]
pub struct AccessUnit {
    pub track: TrackId,
    pub codec: Codec,
    /// 入力の再起動や不連続で増える世代。
    pub epoch: u64,
    /// トラック内の通し番号。
    pub sequence: u64,
    /// 適用する [`CodecConfig::generation`]。
    pub config_generation: u32,
    pub pts: i64,
    pub dts: i64,
    /// 長さ（不明なら 0）。
    pub duration: u64,
    pub timebase: Timebase,
    pub keyframe: bool,
    /// この AU の直前に欠落がある。
    pub discontinuity: bool,
    /// 撮影時刻（UNIX ns）。時刻源が分かるときだけ。
    pub capture_time_ns: Option<i64>,
    pub payload: Bytes,
}

impl AccessUnit {
    pub fn kind(&self) -> MediaKind {
        self.codec.kind()
    }
}
