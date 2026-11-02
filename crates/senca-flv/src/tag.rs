//! FLV の音声・映像タグ本体（RTMP の audio/video message の payload と同じ形）の解釈。
//!
//! - 従来形式: 映像は AVC（CodecID 7）と、広く使われている非公式の HEVC（CodecID 12）。
//!   音声は AAC（SoundFormat 10）と G.711（7 = A-law、8 = μ-law）。
//! - Enhanced RTMP v2（Veovera, enhanced-rtmp-v2.md）: IsExHeader ビット、FourCC
//!   （映像 avc1・hvc1・av01・vp09・vp08、音声 Opus・mp4a）、PacketType
//!   SequenceStart・CodedFrames・CodedFramesX・SequenceEnd・Metadata・MPEG2TSSequenceStart、
//!   ModEx（中身は読み飛ばす）。Multitrack はまだ扱わず [`Error::MultitrackUnsupported`] を返す。
//!
//! payload はすべて入力 `Bytes` の切り出し（コピーしない）。

use crate::Error;
use bytes::Bytes;
use senca_core::Codec;

/// FLV のタグ種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TagType {
    Audio,
    Video,
    Script,
}

impl TagType {
    pub fn from_u8(v: u8) -> Option<TagType> {
        match v {
            8 => Some(TagType::Audio),
            9 => Some(TagType::Video),
            18 => Some(TagType::Script),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            TagType::Audio => 8,
            TagType::Video => 9,
            TagType::Script => 18,
        }
    }
}

/// 映像のフレーム種別（FLV の FrameType）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoFrameType {
    Key,
    Inter,
    DisposableInter,
    GeneratedKey,
    /// 映像情報・コマンドフレーム。
    Command,
}

impl VideoFrameType {
    fn from_bits(v: u8) -> Result<Self, Error> {
        Ok(match v {
            1 => VideoFrameType::Key,
            2 => VideoFrameType::Inter,
            3 => VideoFrameType::DisposableInter,
            4 => VideoFrameType::GeneratedKey,
            5 => VideoFrameType::Command,
            _ => return Err(Error::Malformed("invalid video frame type")),
        })
    }

    pub fn as_bits(self) -> u8 {
        match self {
            VideoFrameType::Key => 1,
            VideoFrameType::Inter => 2,
            VideoFrameType::DisposableInter => 3,
            VideoFrameType::GeneratedKey => 4,
            VideoFrameType::Command => 5,
        }
    }
}

/// 映像タグの中身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoPacket {
    /// 設定（AVCDecoderConfigurationRecord・HEVCDecoderConfigurationRecord・av1C・vpcC）。
    SequenceStart(Bytes),
    /// 符号化フレーム。`composition_time` はミリ秒（PTS − DTS）。CodedFramesX と AV1/VP9 では 0。
    CodedFrames {
        composition_time: i32,
        data: Bytes,
    },
    SequenceEnd,
    /// Enhanced RTMP の Metadata（AMF の colorInfo など）。
    Metadata(Bytes),
    /// Enhanced RTMP の MPEG2TSSequenceStart（AV1 の video descriptor）。
    Mpeg2TsSequenceStart(Bytes),
    /// コマンドフレーム（0 = シーク開始、1 = シーク終了）。
    Command(u8),
}

/// 解釈した映像タグ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTag {
    pub frame_type: VideoFrameType,
    /// Enhanced RTMP のコマンドフレームでは FourCC がないので `None`。それ以外は必ず `Some`。
    pub codec: Option<Codec>,
    /// Enhanced RTMP（IsExHeader = 1）で書かれていた。
    pub enhanced: bool,
    pub packet: VideoPacket,
}

impl VideoTag {
    pub fn is_keyframe(&self) -> bool {
        self.frame_type == VideoFrameType::Key
    }
}

/// 従来形式の音声ヘッダーの値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyAudioHeader {
    /// 0 = 5.5 kHz、1 = 11 kHz、2 = 22 kHz、3 = 44 kHz（AAC では意味を持たない）。
    pub rate_code: u8,
    pub sample_16bit: bool,
    pub stereo: bool,
}

/// 音声タグの中身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioPacket {
    /// 設定（AAC: AudioSpecificConfig、Opus: OpusHead）。
    SequenceStart(Bytes),
    CodedFrames(Bytes),
    SequenceEnd,
    /// Enhanced RTMP の MultichannelConfig（中身はそのまま）。
    MultichannelConfig(Bytes),
}

/// 解釈した音声タグ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTag {
    pub codec: Codec,
    pub enhanced: bool,
    /// 従来形式のときだけ。
    pub legacy: Option<LegacyAudioHeader>,
    pub packet: AudioPacket,
}

/// FourCC。
pub mod fourcc {
    pub const AVC1: [u8; 4] = *b"avc1";
    pub const HVC1: [u8; 4] = *b"hvc1";
    pub const AV01: [u8; 4] = *b"av01";
    pub const VP09: [u8; 4] = *b"vp09";
    pub const VP08: [u8; 4] = *b"vp08";
    pub const OPUS: [u8; 4] = *b"Opus";
    pub const MP4A: [u8; 4] = *b"mp4a";
}

/// コーデックに対応する映像の FourCC。
pub fn video_fourcc(codec: Codec) -> Option<[u8; 4]> {
    Some(match codec {
        Codec::H264 => fourcc::AVC1,
        Codec::H265 => fourcc::HVC1,
        Codec::Av1 => fourcc::AV01,
        Codec::Vp9 => fourcc::VP09,
        Codec::Vp8 => fourcc::VP08,
        _ => return None,
    })
}

/// コーデックに対応する音声の FourCC。
pub fn audio_fourcc(codec: Codec) -> Option<[u8; 4]> {
    Some(match codec {
        Codec::Opus => fourcc::OPUS,
        Codec::Aac => fourcc::MP4A,
        _ => return None,
    })
}

fn video_codec_from_fourcc(f: [u8; 4]) -> Result<Codec, Error> {
    Ok(match &f {
        b"avc1" => Codec::H264,
        b"hvc1" => Codec::H265,
        b"av01" => Codec::Av1,
        b"vp09" => Codec::Vp9,
        b"vp08" => Codec::Vp8,
        _ => return Err(Error::UnsupportedFourCc(f)),
    })
}

fn audio_codec_from_fourcc(f: [u8; 4]) -> Result<Codec, Error> {
    Ok(match &f {
        b"Opus" => Codec::Opus,
        b"mp4a" => Codec::Aac,
        _ => return Err(Error::UnsupportedFourCc(f)),
    })
}

/// 先頭から順に読む小さな読み手。
struct Cursor<'a> {
    body: &'a Bytes,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(body: &'a Bytes) -> Self {
        Cursor { body, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8, Error> {
        let v = *self.body.get(self.pos).ok_or(Error::Truncated)?;
        self.pos += 1;
        Ok(v)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::Truncated)?;
        let s = self.body.get(self.pos..end).ok_or(Error::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    fn fourcc(&mut self) -> Result<[u8; 4], Error> {
        let s = self.take(4)?;
        Ok([s[0], s[1], s[2], s[3]])
    }

    fn i24(&mut self) -> Result<i32, Error> {
        let s = self.take(3)?;
        let v = ((s[0] as i32) << 16) | ((s[1] as i32) << 8) | s[2] as i32;
        // 符号拡張
        Ok((v << 8) >> 8)
    }

    fn u16(&mut self) -> Result<u16, Error> {
        let s = self.take(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }

    fn rest(&self) -> Bytes {
        self.body.slice(self.pos.min(self.body.len())..)
    }

    /// Enhanced RTMP の ModEx を読み飛ばし、最終的な PacketType を返す。
    fn skip_mod_ex(&mut self, mut packet_type: u8) -> Result<u8, Error> {
        const MOD_EX: u8 = 7;
        let mut rounds = 0;
        while packet_type == MOD_EX {
            rounds += 1;
            if rounds > 8 {
                return Err(Error::Malformed("too many ModEx headers"));
            }
            let mut size = self.u8()? as usize + 1;
            if size == 256 {
                size = self.u16()? as usize + 1;
            }
            self.take(size)?;
            let b = self.u8()?;
            // 上位 4 bit は ModExType（0 = TimestampOffsetNano）。ここでは使わない。
            packet_type = b & 0x0F;
        }
        Ok(packet_type)
    }
}

/// 映像タグ本体を解釈する。payload は `body` の切り出し。
pub fn parse_video(body: &Bytes) -> Result<VideoTag, Error> {
    let mut c = Cursor::new(body);
    let b0 = c.u8()?;
    if b0 & 0x80 != 0 {
        parse_video_enhanced(&mut c, b0)
    } else {
        parse_video_legacy(&mut c, b0)
    }
}

fn parse_video_legacy(c: &mut Cursor<'_>, b0: u8) -> Result<VideoTag, Error> {
    let frame_type = VideoFrameType::from_bits(b0 >> 4)?;
    let codec = match b0 & 0x0F {
        7 => Codec::H264,
        12 => Codec::H265,
        id => return Err(Error::UnsupportedCodecId(id)),
    };
    let packet_type = c.u8()?;
    let cts = c.i24()?;
    let packet = if frame_type == VideoFrameType::Command {
        VideoPacket::Command(c.u8()?)
    } else {
        match packet_type {
            0 => VideoPacket::SequenceStart(c.rest()),
            1 => VideoPacket::CodedFrames {
                composition_time: cts,
                data: c.rest(),
            },
            2 => VideoPacket::SequenceEnd,
            _ => return Err(Error::Malformed("invalid AVCPacketType")),
        }
    };
    Ok(VideoTag {
        frame_type,
        codec: Some(codec),
        enhanced: false,
        packet,
    })
}

fn parse_video_enhanced(c: &mut Cursor<'_>, b0: u8) -> Result<VideoTag, Error> {
    const METADATA: u8 = 4;
    const MULTITRACK: u8 = 6;
    let frame_type = VideoFrameType::from_bits((b0 >> 4) & 0x07)?;
    let packet_type = c.skip_mod_ex(b0 & 0x0F)?;
    if packet_type != METADATA && frame_type == VideoFrameType::Command {
        return Ok(VideoTag {
            frame_type,
            codec: None,
            enhanced: true,
            packet: VideoPacket::Command(c.u8()?),
        });
    }
    if packet_type == MULTITRACK {
        return Err(Error::MultitrackUnsupported);
    }
    let codec = video_codec_from_fourcc(c.fourcc()?)?;
    let packet = match packet_type {
        0 => VideoPacket::SequenceStart(c.rest()),
        1 => {
            let composition_time = if matches!(codec, Codec::H264 | Codec::H265) {
                c.i24()?
            } else {
                0
            };
            VideoPacket::CodedFrames {
                composition_time,
                data: c.rest(),
            }
        }
        2 => VideoPacket::SequenceEnd,
        3 => VideoPacket::CodedFrames {
            composition_time: 0,
            data: c.rest(),
        },
        METADATA => VideoPacket::Metadata(c.rest()),
        5 => VideoPacket::Mpeg2TsSequenceStart(c.rest()),
        _ => return Err(Error::Malformed("invalid VideoPacketType")),
    };
    Ok(VideoTag {
        frame_type,
        codec: Some(codec),
        enhanced: true,
        packet,
    })
}

/// 音声タグ本体を解釈する。payload は `body` の切り出し。
pub fn parse_audio(body: &Bytes) -> Result<AudioTag, Error> {
    let mut c = Cursor::new(body);
    let b0 = c.u8()?;
    let format = b0 >> 4;
    if format == 9 {
        return parse_audio_enhanced(&mut c, b0);
    }
    let legacy = LegacyAudioHeader {
        rate_code: (b0 >> 2) & 0x03,
        sample_16bit: b0 & 0x02 != 0,
        stereo: b0 & 0x01 != 0,
    };
    let (codec, packet) = match format {
        10 => {
            let packet = match c.u8()? {
                0 => AudioPacket::SequenceStart(c.rest()),
                1 => AudioPacket::CodedFrames(c.rest()),
                _ => return Err(Error::Malformed("invalid AACPacketType")),
            };
            (Codec::Aac, packet)
        }
        7 => (Codec::Pcma, AudioPacket::CodedFrames(c.rest())),
        8 => (Codec::Pcmu, AudioPacket::CodedFrames(c.rest())),
        f => return Err(Error::UnsupportedSoundFormat(f)),
    };
    Ok(AudioTag {
        codec,
        enhanced: false,
        legacy: Some(legacy),
        packet,
    })
}

fn parse_audio_enhanced(c: &mut Cursor<'_>, b0: u8) -> Result<AudioTag, Error> {
    const MULTITRACK: u8 = 5;
    let packet_type = c.skip_mod_ex(b0 & 0x0F)?;
    if packet_type == MULTITRACK {
        return Err(Error::MultitrackUnsupported);
    }
    let codec = audio_codec_from_fourcc(c.fourcc()?)?;
    let packet = match packet_type {
        0 => AudioPacket::SequenceStart(c.rest()),
        1 => AudioPacket::CodedFrames(c.rest()),
        2 => AudioPacket::SequenceEnd,
        4 => AudioPacket::MultichannelConfig(c.rest()),
        _ => return Err(Error::Malformed("invalid AudioPacketType")),
    };
    Ok(AudioTag {
        codec,
        enhanced: true,
        legacy: None,
        packet,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_avc() {
        let body = Bytes::from_static(&[0x17, 0x01, 0xFF, 0xFF, 0xFE, 0, 0, 0, 1, 0x65]);
        let t = parse_video(&body).unwrap();
        assert_eq!(t.codec, Some(Codec::H264));
        assert!(t.is_keyframe());
        assert!(!t.enhanced);
        match t.packet {
            VideoPacket::CodedFrames {
                composition_time,
                data,
            } => {
                assert_eq!(composition_time, -2);
                assert_eq!(&data[..], &[0, 0, 0, 1, 0x65]);
                // 切り出しであること
                assert_eq!(data.as_ptr(), body[5..].as_ptr());
            }
            p => panic!("{p:?}"),
        }
        let seq = Bytes::from_static(&[0x17, 0x00, 0, 0, 0, 1, 0x64]);
        assert_eq!(
            parse_video(&seq).unwrap().packet,
            VideoPacket::SequenceStart(Bytes::from_static(&[1, 0x64]))
        );
        let eos = Bytes::from_static(&[0x17, 0x02, 0, 0, 0]);
        assert_eq!(parse_video(&eos).unwrap().packet, VideoPacket::SequenceEnd);
    }

    #[test]
    fn enhanced_video() {
        // hvc1 CodedFrames、cts = 33
        let body = Bytes::from_static(&[0x91, b'h', b'v', b'c', b'1', 0, 0, 33, 9, 9]);
        let t = parse_video(&body).unwrap();
        assert_eq!(t.codec, Some(Codec::H265));
        assert!(t.enhanced && t.is_keyframe());
        assert_eq!(
            t.packet,
            VideoPacket::CodedFrames {
                composition_time: 33,
                data: Bytes::from_static(&[9, 9])
            }
        );
        // av01 CodedFramesX（cts なし）
        let body = Bytes::from_static(&[0xA3, b'a', b'v', b'0', b'1', 7]);
        let t = parse_video(&body).unwrap();
        assert_eq!(t.frame_type, VideoFrameType::Inter);
        assert_eq!(
            t.packet,
            VideoPacket::CodedFrames {
                composition_time: 0,
                data: Bytes::from_static(&[7])
            }
        );
        // vp09 SequenceStart
        let body = Bytes::from_static(&[0x90, b'v', b'p', b'0', b'9', 1, 2]);
        assert_eq!(
            parse_video(&body).unwrap().packet,
            VideoPacket::SequenceStart(Bytes::from_static(&[1, 2]))
        );
        // ModEx（2 バイトのデータ）のあと CodedFramesX
        let body = Bytes::from_static(&[0x97, 1, 0xAA, 0xBB, 0x03, b'a', b'v', b'c', b'1', 5]);
        assert_eq!(
            parse_video(&body).unwrap().packet,
            VideoPacket::CodedFrames {
                composition_time: 0,
                data: Bytes::from_static(&[5])
            }
        );
        // コマンドフレーム
        let body = Bytes::from_static(&[0xD1, 0x00]);
        let t = parse_video(&body).unwrap();
        assert_eq!(t.codec, None);
        assert_eq!(t.packet, VideoPacket::Command(0));
        // マルチトラック
        let body = Bytes::from_static(&[0x96, 0, b'a', b'v', b'c', b'1']);
        assert_eq!(parse_video(&body), Err(Error::MultitrackUnsupported));
        // 未知の FourCC
        let body = Bytes::from_static(&[0x91, b'x', b'x', b'x', b'x']);
        assert_eq!(parse_video(&body), Err(Error::UnsupportedFourCc(*b"xxxx")));
    }

    #[test]
    fn audio() {
        let body = Bytes::from_static(&[0xAF, 0x00, 0x12, 0x10]);
        let t = parse_audio(&body).unwrap();
        assert_eq!(t.codec, Codec::Aac);
        assert_eq!(
            t.packet,
            AudioPacket::SequenceStart(Bytes::from_static(&[0x12, 0x10]))
        );
        assert!(t.legacy.unwrap().stereo);
        let body = Bytes::from_static(&[0xAF, 0x01, 0x21]);
        assert_eq!(
            parse_audio(&body).unwrap().packet,
            AudioPacket::CodedFrames(Bytes::from_static(&[0x21]))
        );
        let body = Bytes::from_static(&[0x91, b'O', b'p', b'u', b's', 0xFC]);
        let t = parse_audio(&body).unwrap();
        assert_eq!(t.codec, Codec::Opus);
        assert!(t.enhanced);
        let body = Bytes::from_static(&[0x95, 0, b'O', b'p', b'u', b's']);
        assert_eq!(parse_audio(&body), Err(Error::MultitrackUnsupported));
        let body = Bytes::from_static(&[0x2F, 0x00]);
        assert_eq!(parse_audio(&body), Err(Error::UnsupportedSoundFormat(2)));
    }

    #[test]
    fn malformed_never_panics() {
        let samples: &[&[u8]] = &[
            &[],
            &[0x17],
            &[0x17, 0x01, 0x00],
            &[0x07, 0x01, 0, 0, 0],
            &[0x97],
            &[0x97, 0xFF],
            &[0x97, 0xFF, 0xFF],
            &[0x91, b'a'],
            &[0xAF],
            &[0xAF, 0x05],
            &[0x9F],
            &[
                0x97, 0, 0, 0x07, 0, 0, 0x07, 0, 0, 0x07, 0, 0, 0x07, 0, 0, 0x07, 0, 0, 0x07, 0, 0,
                0x07, 0, 0, 0x07, 0, 0, 0x07,
            ],
        ];
        for s in samples {
            let b = Bytes::copy_from_slice(s);
            let _ = parse_video(&b);
            let _ = parse_audio(&b);
        }
        let mut x: u32 = 99;
        for _ in 0..5000 {
            let mut v = Vec::new();
            for _ in 0..(x % 16) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                v.push(x as u8);
            }
            x = x.wrapping_add(1);
            let b = Bytes::from(v);
            let _ = parse_video(&b);
            let _ = parse_audio(&b);
        }
    }
}
