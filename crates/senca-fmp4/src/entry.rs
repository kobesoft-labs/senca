//! サンプル記述（stsd の中身）の書き出しと読み取り。

use crate::bmff::{begin_box, begin_full_box, end_box, put_u16, put_u32};
use crate::error::Mp4Error;
use bytes::Bytes;
use senca_core::bits::BitReader;
use senca_core::media::{AudioParams, VideoParams};
use senca_core::{Codec, CodecConfig};

const AAC_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// AudioSpecificConfig から (object type, 標本化周波数, チャンネル構成)。
pub(crate) fn parse_asc(b: &[u8]) -> Option<(u8, u32, u8)> {
    let mut r = BitReader::new(b);
    let mut aot = r.read_u8(5)?;
    if aot == 31 {
        aot = 32 + r.read_u8(6)?;
    }
    let idx = r.read_u8(4)?;
    let rate = if idx == 15 { r.read_u32(24)? } else { *AAC_RATES.get(idx as usize)? };
    let ch = r.read_u8(4)?;
    Some((aot, rate, ch))
}

/// OpusHead（RFC 7845、リトルエンディアン）の主な値。
pub(crate) struct OpusHead<'a> {
    pub channels: u8,
    pub pre_skip: u16,
    pub input_rate: u32,
    pub gain: i16,
    pub family: u8,
    /// family ≠ 0 のときの stream_count・coupled_count・mapping。
    pub table: &'a [u8],
}

pub(crate) fn parse_opus_head(b: &[u8]) -> Option<OpusHead<'_>> {
    if b.len() < 19 || &b[0..8] != b"OpusHead" || b[8] >> 4 != 0 || b[9] == 0 {
        return None;
    }
    let channels = b[9];
    let family = b[18];
    let table = if family == 0 { &[][..] } else { b.get(19..21 + channels as usize)? };
    Some(OpusHead {
        channels,
        pre_skip: u16::from_le_bytes([b[10], b[11]]),
        input_rate: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
        gain: i16::from_le_bytes([b[16], b[17]]),
        family,
        table,
    })
}

fn visual_entry(buf: &mut Vec<u8>, fourcc: &[u8; 4], v: &VideoParams) -> Result<usize, Mp4Error> {
    let w = u16::try_from(v.width).map_err(|_| Mp4Error::BadConfig("width"))?;
    let h = u16::try_from(v.height).map_err(|_| Mp4Error::BadConfig("height"))?;
    let p = begin_box(buf, fourcc);
    buf.extend_from_slice(&[0; 6]);
    put_u16(buf, 1); // data_reference_index
    buf.extend_from_slice(&[0; 16]);
    put_u16(buf, w);
    put_u16(buf, h);
    put_u32(buf, 0x0048_0000);
    put_u32(buf, 0x0048_0000);
    put_u32(buf, 0);
    put_u16(buf, 1); // frame_count
    buf.extend_from_slice(&[0; 32]); // compressorname
    put_u16(buf, 0x0018);
    put_u16(buf, 0xFFFF);
    Ok(p)
}

fn audio_entry(buf: &mut Vec<u8>, fourcc: &[u8; 4], channels: u16, rate: u32) -> usize {
    let p = begin_box(buf, fourcc);
    buf.extend_from_slice(&[0; 6]);
    put_u16(buf, 1);
    buf.extend_from_slice(&[0; 8]);
    put_u16(buf, channels);
    put_u16(buf, 16);
    put_u32(buf, 0);
    put_u32(buf, if rate <= 0xFFFF { rate << 16 } else { 0 });
    p
}

fn child(buf: &mut Vec<u8>, fourcc: &[u8; 4], body: &[u8]) {
    let p = begin_box(buf, fourcc);
    buf.extend_from_slice(body);
    end_box(buf, p);
}

fn put_desc(buf: &mut Vec<u8>, tag: u8, body: &[u8]) -> Result<(), Mp4Error> {
    if body.len() >= 1 << 28 {
        return Err(Mp4Error::TooLarge("descriptor"));
    }
    buf.push(tag);
    let n = body.len() as u32;
    // 4 バイトの可変長表現（多くの実装と同じ）
    buf.extend_from_slice(&[
        0x80 | ((n >> 21) as u8 & 0x7F),
        0x80 | ((n >> 14) as u8 & 0x7F),
        0x80 | ((n >> 7) as u8 & 0x7F),
        n as u8 & 0x7F,
    ]);
    buf.extend_from_slice(body);
    Ok(())
}

fn require_video(c: &CodecConfig) -> Result<VideoParams, Mp4Error> {
    c.video.ok_or(Mp4Error::BadConfig("video params (width/height) required"))
}

/// stsd の中に置くサンプル記述を 1 つ書く。
pub(crate) fn write_sample_entry(buf: &mut Vec<u8>, c: &CodecConfig, track_id: u32) -> Result<(), Mp4Error> {
    match c.codec {
        Codec::H264 => {
            let v = require_video(c)?;
            if c.record.len() < 7 || c.record[0] != 1 {
                return Err(Mp4Error::BadConfig("AVCDecoderConfigurationRecord"));
            }
            let p = visual_entry(buf, b"avc1", &v)?;
            child(buf, b"avcC", &c.record);
            end_box(buf, p);
        }
        Codec::H265 => {
            let v = require_video(c)?;
            if c.record.len() < 23 || c.record[0] != 1 {
                return Err(Mp4Error::BadConfig("HEVCDecoderConfigurationRecord"));
            }
            let p = visual_entry(buf, b"hvc1", &v)?;
            child(buf, b"hvcC", &c.record);
            end_box(buf, p);
        }
        Codec::Av1 => {
            let v = require_video(c)?;
            if c.record.len() < 4 || c.record[0] != 0x81 {
                return Err(Mp4Error::BadConfig("AV1CodecConfigurationRecord"));
            }
            let p = visual_entry(buf, b"av01", &v)?;
            child(buf, b"av1C", &c.record);
            end_box(buf, p);
        }
        Codec::Vp9 => {
            let v = require_video(c)?;
            let p = visual_entry(buf, b"vp09", &v)?;
            let r = &c.record[..];
            // record は vpcC の中身。version/flags（01 00 00 00）付きでも無しでもよい。
            let body: Vec<u8> = if r.len() >= 12 && r[0..4] == [1, 0, 0, 0] {
                r.to_vec()
            } else if r.len() >= 8 {
                let mut b = vec![1, 0, 0, 0];
                b.extend_from_slice(r);
                b
            } else if r.is_empty() {
                // 既定: profile, level, 8 bit・4:2:0（colocated なし）、BT.709
                vec![1, 0, 0, 0, v.profile, v.level, 0x82, 1, 1, 1, 0, 0]
            } else {
                return Err(Mp4Error::BadConfig("vpcC"));
            };
            child(buf, b"vpcC", &body);
            end_box(buf, p);
        }
        Codec::Aac => {
            let (aot, rate, ch) = parse_asc(&c.record).ok_or(Mp4Error::BadConfig("AudioSpecificConfig"))?;
            let _ = aot;
            let a = c.audio.unwrap_or(AudioParams { sample_rate: rate, channels: ch, object_type: aot });
            let channels = if a.channels != 0 { a.channels } else { ch.max(1) };
            let rate = if a.sample_rate != 0 { a.sample_rate } else { rate };
            let p = audio_entry(buf, b"mp4a", channels as u16, rate);
            let e = begin_full_box(buf, b"esds", 0, 0);
            let mut dsi = Vec::new();
            put_desc(&mut dsi, 0x05, &c.record)?;
            let mut dcd = vec![0x40, 0x15, 0, 0, 0];
            dcd.extend_from_slice(&[0; 8]); // maxBitrate, avgBitrate（不明）
            dcd.extend_from_slice(&dsi);
            let mut es = Vec::new();
            put_u16(&mut es, (track_id & 0xFFFF) as u16);
            es.push(0);
            put_desc(&mut es, 0x04, &dcd)?;
            put_desc(&mut es, 0x06, &[0x02])?;
            put_desc(buf, 0x03, &es)?;
            end_box(buf, e);
            end_box(buf, p);
        }
        Codec::Opus => {
            let h = parse_opus_head(&c.record).ok_or(Mp4Error::BadConfig("OpusHead"))?;
            let p = audio_entry(buf, b"Opus", h.channels as u16, 48000);
            let d = begin_box(buf, b"dOps");
            buf.push(0);
            buf.push(h.channels);
            put_u16(buf, h.pre_skip);
            put_u32(buf, h.input_rate);
            put_u16(buf, h.gain as u16);
            buf.push(h.family);
            buf.extend_from_slice(h.table);
            end_box(buf, d);
            end_box(buf, p);
        }
        c => return Err(Mp4Error::UnsupportedCodec(c)),
    }
    Ok(())
}

/// 既定の時間軸（映像 90 kHz、音声は標本化周波数）。
pub(crate) fn default_timescale(c: &CodecConfig) -> u32 {
    match c.codec {
        Codec::Opus => 48000,
        Codec::Aac => c
            .audio
            .map(|a| a.sample_rate)
            .filter(|&r| r != 0)
            .or_else(|| parse_asc(&c.record).map(|x| x.1))
            .unwrap_or(48000),
        Codec::Pcma | Codec::Pcmu => 8000,
        _ => 90000,
    }
}

// ---------------------------------------------------------------------------
// 読み取り

/// 箱の列を (fourcc, 中身) で列挙する。壊れていたら `Err`。
pub(crate) fn boxes(mut d: &[u8]) -> impl Iterator<Item = Result<([u8; 4], &[u8]), Mp4Error>> {
    std::iter::from_fn(move || {
        if d.is_empty() {
            return None;
        }
        let r = split_box(d);
        match r {
            Ok((ty, body, rest)) => {
                d = rest;
                Some(Ok((ty, body)))
            }
            Err(e) => {
                d = &[];
                Some(Err(e))
            }
        }
    })
}

/// 切り出した箱: (fourcc, 中身, 残り)。
pub(crate) type SplitBox<'a> = ([u8; 4], &'a [u8], &'a [u8]);

/// 先頭の箱を切り出す。
pub(crate) fn split_box(d: &[u8]) -> Result<SplitBox<'_>, Mp4Error> {
    if d.len() < 8 {
        return Err(Mp4Error::Malformed("truncated box header"));
    }
    let size32 = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as u64;
    let ty = [d[4], d[5], d[6], d[7]];
    let (size, hdr) = match size32 {
        0 => (d.len() as u64, 8usize),
        1 => {
            if d.len() < 16 {
                return Err(Mp4Error::Malformed("truncated largesize"));
            }
            (u64::from_be_bytes(d[8..16].try_into().expect("8 bytes")), 16)
        }
        s => (s, 8),
    };
    if size < hdr as u64 || size > d.len() as u64 {
        return Err(Mp4Error::Malformed("box size"));
    }
    let size = size as usize;
    Ok((ty, &d[hdr..size], &d[size..]))
}

pub(crate) fn find_box<'a>(d: &'a [u8], ty: &[u8; 4]) -> Result<Option<&'a [u8]>, Mp4Error> {
    for b in boxes(d) {
        let (t, body) = b?;
        if &t == ty {
            return Ok(Some(body));
        }
    }
    Ok(None)
}

fn read_desc(d: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *d.first()?;
    let mut len = 0usize;
    let mut p = 1;
    for _ in 0..4 {
        let b = *d.get(p)?;
        p += 1;
        len = (len << 7) | (b & 0x7F) as usize;
        if b & 0x80 == 0 {
            break;
        }
    }
    let body = d.get(p..p + len)?;
    Some((tag, body, &d[p + len..]))
}

/// esds の中身（FullBox ヘッダーの後ろ）から DecoderSpecificInfo を取り出す。
fn esds_dsi(d: &[u8]) -> Option<&[u8]> {
    let (tag, es, _) = read_desc(d)?;
    if tag != 0x03 || es.len() < 3 {
        return None;
    }
    let flags = es[2];
    let mut p = 3;
    if flags & 0x80 != 0 {
        p += 2;
    }
    if flags & 0x40 != 0 {
        p += 1 + *es.get(p)? as usize;
    }
    if flags & 0x20 != 0 {
        p += 2;
    }
    let (tag, dcd, _) = read_desc(es.get(p..)?)?;
    if tag != 0x04 || dcd.len() < 13 || dcd[0] != 0x40 {
        return None;
    }
    let (tag, dsi, _) = read_desc(&dcd[13..])?;
    (tag == 0x05).then_some(dsi)
}

/// サンプル記述を読んで [`CodecConfig`] にする。知らない種類は `Ok(None)`。
pub(crate) fn parse_sample_entry(ty: [u8; 4], body: &[u8]) -> Result<Option<CodecConfig>, Mp4Error> {
    let bad = Mp4Error::Malformed("sample entry");
    match &ty {
        b"avc1" | b"avc3" | b"hvc1" | b"hev1" | b"av01" | b"vp09" | b"vp08" => {
            if body.len() < 78 {
                return Err(bad);
            }
            let width = u16::from_be_bytes([body[24], body[25]]) as u32;
            let height = u16::from_be_bytes([body[26], body[27]]) as u32;
            let children = &body[78..];
            let (codec, cfg_ty): (Codec, Option<&[u8; 4]>) = match &ty {
                b"avc1" | b"avc3" => (Codec::H264, Some(b"avcC")),
                b"hvc1" | b"hev1" => (Codec::H265, Some(b"hvcC")),
                b"av01" => (Codec::Av1, Some(b"av1C")),
                b"vp09" => (Codec::Vp9, Some(b"vpcC")),
                _ => (Codec::Vp8, None),
            };
            let record: &[u8] = match cfg_ty {
                Some(t) => find_box(children, t)?.ok_or(Mp4Error::Malformed("missing codec configuration box"))?,
                None => &[],
            };
            let (profile, compat, level) = match codec {
                Codec::H264 if record.len() >= 4 => (record[1], record[2], record[3]),
                Codec::H265 if record.len() >= 13 => (record[1] & 0x1F, (record[1] >> 5) & 1, record[12]),
                Codec::Av1 if record.len() >= 3 => (record[1] >> 5, record[2] >> 7, record[1] & 0x1F),
                // ffmpeg の empty_moov は空の av1C を書くことがある。読めるようにしておく（書くときは拒否する）
                Codec::Av1 => (0, 0, 0),
                Codec::Vp9 if record.len() >= 6 => (record[4], 0, record[5]),
                Codec::Vp8 => (0, 0, 0),
                _ => return Err(Mp4Error::Malformed("codec configuration")),
            };
            Ok(Some(CodecConfig {
                codec,
                generation: 0,
                record: Bytes::copy_from_slice(record),
                video: Some(VideoParams { width, height, profile, compat, level }),
                audio: None,
            }))
        }
        b"mp4a" | b"Opus" => {
            if body.len() < 28 {
                return Err(bad);
            }
            let version = u16::from_be_bytes([body[8], body[9]]);
            let channels = u16::from_be_bytes([body[16], body[17]]);
            let rate = u32::from_be_bytes([body[24], body[25], body[26], body[27]]) >> 16;
            let skip = match version {
                0 => 28,
                1 => 44,
                2 => 64,
                _ => return Err(bad),
            };
            let children = body.get(skip..).ok_or(Mp4Error::Malformed("audio sample entry"))?;
            if &ty == b"mp4a" {
                let esds = find_box(children, b"esds")?.ok_or(Mp4Error::Malformed("missing esds"))?;
                let dsi = esds.get(4..).and_then(esds_dsi).ok_or(Mp4Error::Malformed("esds"))?;
                let (aot, asc_rate, ch) = parse_asc(dsi).ok_or(Mp4Error::Malformed("AudioSpecificConfig"))?;
                Ok(Some(CodecConfig {
                    codec: Codec::Aac,
                    generation: 0,
                    record: Bytes::copy_from_slice(dsi),
                    video: None,
                    audio: Some(AudioParams {
                        sample_rate: if asc_rate != 0 { asc_rate } else { rate },
                        channels: if ch == 7 { 8 } else if ch != 0 { ch } else { channels as u8 },
                        object_type: aot,
                    }),
                }))
            } else {
                let d = find_box(children, b"dOps")?.ok_or(Mp4Error::Malformed("missing dOps"))?;
                if d.len() < 11 || d[0] != 0 {
                    return Err(Mp4Error::Malformed("dOps"));
                }
                let ch = d[1];
                let family = d[10];
                let mut head = Vec::with_capacity(21 + ch as usize);
                head.extend_from_slice(b"OpusHead");
                head.push(1);
                head.push(ch);
                head.extend_from_slice(&u16::from_be_bytes([d[2], d[3]]).to_le_bytes());
                head.extend_from_slice(&u32::from_be_bytes([d[4], d[5], d[6], d[7]]).to_le_bytes());
                head.extend_from_slice(&i16::from_be_bytes([d[8], d[9]]).to_le_bytes());
                head.push(family);
                if family != 0 {
                    head.extend_from_slice(d.get(11..13 + ch as usize).ok_or(Mp4Error::Malformed("dOps mapping"))?);
                }
                Ok(Some(CodecConfig {
                    codec: Codec::Opus,
                    generation: 0,
                    record: Bytes::from(head),
                    video: None,
                    audio: Some(AudioParams { sample_rate: 48000, channels: ch, object_type: 0 }),
                }))
            }
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_entry_roundtrip() {
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 0]);
        let c = CodecConfig {
            codec: Codec::Opus,
            generation: 0,
            record: Bytes::from(head.clone()),
            video: None,
            audio: Some(AudioParams { sample_rate: 48000, channels: 2, object_type: 0 }),
        };
        let mut buf = Vec::new();
        write_sample_entry(&mut buf, &c, 1).unwrap();
        let (ty, body, rest) = split_box(&buf).unwrap();
        assert!(rest.is_empty());
        let back = parse_sample_entry(ty, body).unwrap().unwrap();
        assert_eq!(back.record, Bytes::from(head));
    }

    #[test]
    fn aac_entry_roundtrip() {
        let c = CodecConfig {
            codec: Codec::Aac,
            generation: 0,
            record: Bytes::from_static(&[0x11, 0x90]),
            video: None,
            audio: None,
        };
        let mut buf = Vec::new();
        write_sample_entry(&mut buf, &c, 2).unwrap();
        let (ty, body, _) = split_box(&buf).unwrap();
        let back = parse_sample_entry(ty, body).unwrap().unwrap();
        assert_eq!(&back.record[..], &[0x11, 0x90]);
        assert_eq!(back.audio.unwrap().sample_rate, 48000);
        assert_eq!(back.audio.unwrap().channels, 2);
        // 切り詰めてもパニックしない
        for i in 0..buf.len() {
            if let Ok((ty, body, _)) = split_box(&buf[..i]) {
                let _ = parse_sample_entry(ty, body);
            }
            let _ = parse_sample_entry(*b"mp4a", &buf[8..i.max(8)]);
        }
    }

    #[test]
    fn video_needs_params() {
        let c = CodecConfig { codec: Codec::Av1, generation: 0, record: Bytes::from_static(&[0x81, 0, 0x0C, 0]), video: None, audio: None };
        assert!(matches!(write_sample_entry(&mut Vec::new(), &c, 1), Err(Mp4Error::BadConfig(_))));
    }
}
