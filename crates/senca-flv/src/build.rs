//! AccessUnit・CodecConfig から FLV タグ本体（= RTMP の audio/video message の payload）を作る。
//!
//! 書き方の選択:
//! - H.264・AAC・G.711: 従来形式（どのプレーヤーでも読める）。
//! - H.265・AV1・VP9・VP8・Opus: Enhanced RTMP v2（FourCC）。
//!
//! 本体は「数バイトの前置ヘッダー」と「payload（`Bytes` をそのまま共有）」に分けて返す。
//! 送り手（RTMP の chunk 化、FLV ファイルへの書き出し）は両方を順に書けばよく、payload をつなぎ直さない。

use crate::Error;
use crate::tag::{TagType, VideoFrameType, audio_fourcc, video_fourcc};
use bytes::{Bytes, BytesMut};
use senca_core::time::Rescale;
use senca_core::{AccessUnit, Codec, CodecConfig, MediaKind, Timebase};

/// 本体の前置ヘッダー（最大 16 バイト）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TagPrefix {
    buf: [u8; 16],
    len: u8,
}

impl TagPrefix {
    fn new() -> Self {
        TagPrefix {
            buf: [0; 16],
            len: 0,
        }
    }

    fn push(&mut self, b: &[u8]) {
        let l = self.len as usize;
        self.buf[l..l + b.len()].copy_from_slice(b);
        self.len += b.len() as u8;
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }
}

impl std::fmt::Debug for TagPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TagPrefix({:02x?})", self.as_slice())
    }
}

/// タグ本体（前置ヘッダー + payload）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagBody {
    pub prefix: TagPrefix,
    pub payload: Bytes,
}

impl TagBody {
    pub fn len(&self) -> usize {
        self.prefix.as_slice().len() + self.payload.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 1 つの `Bytes` にまとめる（コピーが 1 回起きる。試験や小さなタグ用）。
    pub fn to_bytes(&self) -> Bytes {
        let mut b = BytesMut::with_capacity(self.len());
        b.extend_from_slice(self.prefix.as_slice());
        b.extend_from_slice(&self.payload);
        b.freeze()
    }

    /// 書き出す部品の並び。
    pub fn parts(&self) -> [&[u8]; 2] {
        [self.prefix.as_slice(), &self.payload]
    }
}

/// 作ったタグ（RTMP では message type と timestamp、FLV ファイルではタグヘッダーになる）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutTag {
    pub tag_type: TagType,
    /// ミリ秒（32 bit で折り返す）。
    pub timestamp: u32,
    pub body: TagBody,
}

fn cts_bytes(cts: i32) -> Result<[u8; 3], Error> {
    if !(-(1 << 23)..(1 << 23)).contains(&cts) {
        return Err(Error::TimestampOutOfRange);
    }
    let v = cts as u32;
    Ok([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

fn uses_enhanced_video(codec: Codec) -> bool {
    codec != Codec::H264
}

/// 映像の設定（SequenceStart）タグ本体。
pub fn video_sequence_start(codec: Codec, record: Bytes) -> Result<TagBody, Error> {
    let mut p = TagPrefix::new();
    if uses_enhanced_video(codec) {
        let fcc = video_fourcc(codec).ok_or(Error::UnsupportedCodec(codec))?;
        p.push(&[0x80 | (VideoFrameType::Key.as_bits() << 4)]);
        p.push(&fcc);
    } else {
        p.push(&[0x17, 0x00, 0, 0, 0]);
    }
    Ok(TagBody {
        prefix: p,
        payload: record,
    })
}

/// 映像の符号化フレームのタグ本体。`composition_time` はミリ秒（PTS − DTS）。
pub fn video_frame(
    codec: Codec,
    keyframe: bool,
    composition_time: i32,
    data: Bytes,
) -> Result<TagBody, Error> {
    let ft = if keyframe {
        VideoFrameType::Key
    } else {
        VideoFrameType::Inter
    };
    let mut p = TagPrefix::new();
    if uses_enhanced_video(codec) {
        let fcc = video_fourcc(codec).ok_or(Error::UnsupportedCodec(codec))?;
        let with_cts = composition_time != 0 && matches!(codec, Codec::H265);
        if !with_cts && composition_time != 0 {
            // AV1・VP8・VP9 は CTS を運べない（並べ替えがない前提のコーデック）。
            return Err(Error::TimestampOutOfRange);
        }
        let packet_type = if with_cts { 1 } else { 3 };
        p.push(&[0x80 | (ft.as_bits() << 4) | packet_type]);
        p.push(&fcc);
        if with_cts {
            p.push(&cts_bytes(composition_time)?);
        }
    } else {
        p.push(&[(ft.as_bits() << 4) | 7, 0x01]);
        p.push(&cts_bytes(composition_time)?);
    }
    Ok(TagBody {
        prefix: p,
        payload: data,
    })
}

/// 映像の終わり（SequenceEnd）タグ本体。
pub fn video_sequence_end(codec: Codec) -> Result<TagBody, Error> {
    let mut p = TagPrefix::new();
    if uses_enhanced_video(codec) {
        let fcc = video_fourcc(codec).ok_or(Error::UnsupportedCodec(codec))?;
        p.push(&[0x80 | (VideoFrameType::Key.as_bits() << 4) | 2]);
        p.push(&fcc);
    } else {
        p.push(&[0x17, 0x02, 0, 0, 0]);
    }
    Ok(TagBody {
        prefix: p,
        payload: Bytes::new(),
    })
}

/// 従来形式の音声ヘッダー 1 バイト。
fn legacy_audio_byte(codec: Codec, config: Option<&CodecConfig>) -> Result<u8, Error> {
    let stereo = config.and_then(|c| c.audio).is_some_and(|a| a.channels > 1);
    Ok(match codec {
        // AAC は常に 44 kHz・16 bit・stereo を書く決まり。
        Codec::Aac => 0xAF,
        Codec::Pcma => (7 << 4) | 0x02 | stereo as u8,
        Codec::Pcmu => (8 << 4) | 0x02 | stereo as u8,
        c => return Err(Error::UnsupportedCodec(c)),
    })
}

/// 音声の設定（SequenceStart）タグ本体。G.711 は設定タグを持たないので `Ok(None)`。
pub fn audio_sequence_start(config: &CodecConfig) -> Result<Option<TagBody>, Error> {
    let mut p = TagPrefix::new();
    match config.codec {
        Codec::Aac => p.push(&[0xAF, 0x00]),
        Codec::Pcma | Codec::Pcmu => return Ok(None),
        c => {
            let fcc = audio_fourcc(c).ok_or(Error::UnsupportedCodec(c))?;
            p.push(&[0x90]);
            p.push(&fcc);
        }
    }
    Ok(Some(TagBody {
        prefix: p,
        payload: config.record.clone(),
    }))
}

/// 音声の符号化フレームのタグ本体。
pub fn audio_frame(
    codec: Codec,
    config: Option<&CodecConfig>,
    data: Bytes,
) -> Result<TagBody, Error> {
    let mut p = TagPrefix::new();
    match codec {
        Codec::Aac => p.push(&[0xAF, 0x01]),
        Codec::Pcma | Codec::Pcmu => p.push(&[legacy_audio_byte(codec, config)?]),
        c => {
            let fcc = audio_fourcc(c).ok_or(Error::UnsupportedCodec(c))?;
            p.push(&[0x91]);
            p.push(&fcc);
        }
    }
    Ok(TagBody {
        prefix: p,
        payload: data,
    })
}

/// AccessUnit の列を FLV タグの列にする。設定の世代が変わったら設定タグを先に出す。
///
/// 時刻は AU の時間軸からミリ秒に換算し（最も近い値へ丸め）、32 bit で折り返す。
/// 負の DTS も 32 bit の 2 の補数として書く（受け手が折り返しとして扱う）。
#[derive(Debug, Default)]
pub struct FlvMuxer {
    video: Option<CodecConfig>,
    audio: Option<CodecConfig>,
    video_sent: Option<u32>,
    audio_sent: Option<u32>,
}

impl FlvMuxer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 設定を登録する（次にその世代を参照する AU の前に設定タグを出す）。
    pub fn set_config(&mut self, config: CodecConfig) -> Result<(), Error> {
        match config.codec.kind() {
            MediaKind::Video => self.video = Some(config),
            MediaKind::Audio => self.audio = Some(config),
            _ => return Err(Error::UnsupportedCodec(config.codec)),
        }
        Ok(())
    }

    /// 映像・音声の設定。
    pub fn video_config(&self) -> Option<&CodecConfig> {
        self.video.as_ref()
    }

    pub fn audio_config(&self) -> Option<&CodecConfig> {
        self.audio.as_ref()
    }

    /// AU を 1 つタグにする（必要なら設定タグを先に `out` へ積む）。
    pub fn access_unit(&mut self, au: &AccessUnit, out: &mut Vec<OutTag>) -> Result<(), Error> {
        let dts = au
            .dts
            .rescale(au.timebase, Timebase::MILLIS)
            .ok_or(Error::TimestampOutOfRange)?;
        let pts = au
            .pts
            .rescale(au.timebase, Timebase::MILLIS)
            .ok_or(Error::TimestampOutOfRange)?;
        let cts = i32::try_from(pts - dts).map_err(|_| Error::TimestampOutOfRange)?;
        let timestamp = dts as u32;
        match au.codec.kind() {
            MediaKind::Video => {
                let cfg = self.video.as_ref();
                if self.video_sent != Some(au.config_generation) {
                    match cfg {
                        Some(c) if c.generation == au.config_generation && c.codec == au.codec => {
                            if !c.record.is_empty() {
                                out.push(OutTag {
                                    tag_type: TagType::Video,
                                    timestamp,
                                    body: video_sequence_start(c.codec, c.record.clone())?,
                                });
                            }
                            self.video_sent = Some(c.generation);
                        }
                        _ if needs_config(au.codec) => return Err(Error::MissingConfig),
                        _ => self.video_sent = Some(au.config_generation),
                    }
                }
                out.push(OutTag {
                    tag_type: TagType::Video,
                    timestamp,
                    body: video_frame(au.codec, au.keyframe, cts, au.payload.clone())?,
                });
            }
            MediaKind::Audio => {
                if self.audio_sent != Some(au.config_generation) {
                    match self.audio.as_ref() {
                        Some(c) if c.generation == au.config_generation && c.codec == au.codec => {
                            if !c.record.is_empty()
                                && let Some(body) = audio_sequence_start(c)?
                            {
                                out.push(OutTag {
                                    tag_type: TagType::Audio,
                                    timestamp,
                                    body,
                                });
                            }
                            self.audio_sent = Some(c.generation);
                        }
                        _ if needs_config(au.codec) => return Err(Error::MissingConfig),
                        _ => self.audio_sent = Some(au.config_generation),
                    }
                }
                out.push(OutTag {
                    tag_type: TagType::Audio,
                    timestamp,
                    body: audio_frame(au.codec, self.audio.as_ref(), au.payload.clone())?,
                });
            }
            _ => return Err(Error::UnsupportedCodec(au.codec)),
        }
        Ok(())
    }

    /// 次の AU の前に設定タグを出し直させる（途中から再生を始める相手に送るときなど）。
    pub fn resend_configs(&mut self) {
        self.video_sent = None;
        self.audio_sent = None;
    }
}

/// 設定なしでは復号できないコーデック。
pub(crate) fn needs_config(codec: Codec) -> bool {
    matches!(codec, Codec::H264 | Codec::H265 | Codec::Av1 | Codec::Aac)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tag::{AudioPacket, VideoPacket, parse_audio, parse_video};
    use senca_core::TrackId;

    fn au(codec: Codec, dts: i64, pts: i64, key: bool, payload: &'static [u8]) -> AccessUnit {
        AccessUnit {
            track: TrackId(0),
            codec,
            epoch: 0,
            sequence: 0,
            config_generation: 1,
            pts,
            dts,
            duration: 0,
            timebase: Timebase::MPEG,
            keyframe: key,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from_static(payload),
        }
    }

    fn cfg(codec: Codec, record: &'static [u8]) -> CodecConfig {
        CodecConfig {
            codec,
            generation: 1,
            record: Bytes::from_static(record),
            video: None,
            audio: None,
        }
    }

    #[test]
    fn video_roundtrip_legacy_and_enhanced() {
        for codec in [Codec::H264, Codec::H265] {
            let b = video_frame(codec, true, -40, Bytes::from_static(&[0, 0, 0, 1, 5]))
                .unwrap()
                .to_bytes();
            let t = parse_video(&b).unwrap();
            assert_eq!(t.codec, Some(codec));
            assert!(t.is_keyframe());
            assert_eq!(
                t.packet,
                VideoPacket::CodedFrames {
                    composition_time: -40,
                    data: Bytes::from_static(&[0, 0, 0, 1, 5])
                }
            );
        }
        let b = video_frame(Codec::Av1, false, 0, Bytes::from_static(&[1]))
            .unwrap()
            .to_bytes();
        assert_eq!(parse_video(&b).unwrap().codec, Some(Codec::Av1));
        assert!(video_frame(Codec::Av1, false, 5, Bytes::new()).is_err());
        let b = video_sequence_end(Codec::Vp9).unwrap().to_bytes();
        assert_eq!(parse_video(&b).unwrap().packet, VideoPacket::SequenceEnd);
    }

    #[test]
    fn muxer_emits_config_first() {
        let mut m = FlvMuxer::new();
        let mut out = Vec::new();
        assert_eq!(
            m.access_unit(&au(Codec::H264, 0, 0, true, &[0, 0, 0, 1, 5]), &mut out),
            Err(Error::MissingConfig)
        );
        m.set_config(cfg(Codec::H264, &[1, 0x64, 0, 0x1f, 0xff]))
            .unwrap();
        m.set_config(cfg(Codec::Aac, &[0x12, 0x10])).unwrap();
        m.access_unit(
            &au(Codec::H264, 9000, 12000, true, &[0, 0, 0, 1, 5]),
            &mut out,
        )
        .unwrap();
        m.access_unit(
            &au(Codec::H264, 12000, 12000, false, &[0, 0, 0, 1, 1]),
            &mut out,
        )
        .unwrap();
        m.access_unit(&au(Codec::Aac, 9000, 9000, true, &[0x21]), &mut out)
            .unwrap();
        assert_eq!(out.len(), 5);
        assert_eq!(out[0].timestamp, 100);
        assert!(matches!(
            parse_video(&out[0].body.to_bytes()).unwrap().packet,
            VideoPacket::SequenceStart(_)
        ));
        assert!(matches!(
            parse_video(&out[1].body.to_bytes()).unwrap().packet,
            VideoPacket::CodedFrames {
                composition_time: 33,
                ..
            }
        ));
        assert_eq!(out[2].timestamp, 133);
        assert!(matches!(
            parse_audio(&out[3].body.to_bytes()).unwrap().packet,
            AudioPacket::SequenceStart(_)
        ));
        assert_eq!(out[4].tag_type, TagType::Audio);
    }
}
