//! VP8（RFC 7741）。
//!
//! payload descriptor: `X R N S R PID(3)`、X があれば `I L T K`、I があれば PictureID（7 か 15 ビット）、
//! L があれば TL0PICIDX、T か K があれば `TID Y KEYIDX`。フレームの始まりは S=1 かつ PID=0、終わりは marker。

use super::{
    ConfigState, Depacketizer, Frame, FrameQueue, MAX_AU_SIZE, Packetizer, Payload, PayloadFormat,
    SeqWatch,
};
use crate::error::{Error, Result};
use crate::packet::RtpPacket;
use bytes::Bytes;
use senca_core::media::{AccessUnit, Codec, CodecConfig, VideoParams};

/// payload descriptor の解析結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Vp8Descriptor {
    pub non_reference: bool,
    pub start: bool,
    pub partition_id: u8,
    pub picture_id: Option<u16>,
    pub tl0_pic_idx: Option<u8>,
    pub tid: Option<(u8, bool)>,
    pub key_idx: Option<u8>,
    /// descriptor の長さ。
    pub len: usize,
}

impl Vp8Descriptor {
    pub fn parse(p: &[u8]) -> Result<Vp8Descriptor> {
        let b0 = *p.first().ok_or(Error::Truncated)?;
        let mut d = Vp8Descriptor {
            non_reference: b0 & 0x20 != 0,
            start: b0 & 0x10 != 0,
            partition_id: b0 & 0x07,
            ..Default::default()
        };
        let mut i = 1usize;
        if b0 & 0x80 != 0 {
            let x = *p.get(i).ok_or(Error::Truncated)?;
            i += 1;
            if x & 0x80 != 0 {
                let a = *p.get(i).ok_or(Error::Truncated)?;
                i += 1;
                if a & 0x80 != 0 {
                    let b = *p.get(i).ok_or(Error::Truncated)?;
                    i += 1;
                    d.picture_id = Some(((a as u16 & 0x7f) << 8) | b as u16);
                } else {
                    d.picture_id = Some(a as u16);
                }
            }
            if x & 0x40 != 0 {
                d.tl0_pic_idx = Some(*p.get(i).ok_or(Error::Truncated)?);
                i += 1;
            }
            if x & 0x30 != 0 {
                let t = *p.get(i).ok_or(Error::Truncated)?;
                i += 1;
                if x & 0x20 != 0 {
                    d.tid = Some((t >> 6, t & 0x20 != 0));
                }
                if x & 0x10 != 0 {
                    d.key_idx = Some(t & 0x1f);
                }
            }
        }
        if i >= p.len() {
            return Err(Error::Invalid("VP8 の payload が空"));
        }
        d.len = i;
        Ok(d)
    }
}

/// VP8 のフレームの先頭（payload header）から、キーフレームなら（幅, 高さ）を取り出す。
fn keyframe_size(frame: &[u8]) -> Option<(u32, u32)> {
    if frame.len() < 10 || frame[0] & 1 != 0 {
        return None;
    }
    if frame[3..6] != [0x9d, 0x01, 0x2a] {
        return None;
    }
    let w = u16::from_le_bytes([frame[6], frame[7]]) & 0x3fff;
    let h = u16::from_le_bytes([frame[8], frame[9]]) & 0x3fff;
    Some((w as u32, h as u32))
}

/// VP8 のフレームがキーフレームか（frame tag の P ビットが 0）。
pub fn is_keyframe(frame: &[u8]) -> bool {
    frame.first().is_some_and(|b| b & 1 == 0)
}

/// VP8 の depacketizer。
#[derive(Default)]
pub struct Vp8Depacketizer {
    seq: SeqWatch,
    buf: Option<(u32, Vec<u8>)>,
    waiting_key: bool,
    discontinuity: bool,
    cfg: ConfigState,
    frames: FrameQueue,
}

impl Vp8Depacketizer {
    pub fn new() -> Self {
        Vp8Depacketizer {
            waiting_key: true,
            ..Default::default()
        }
    }

    fn drop_current(&mut self) {
        if self.buf.take().is_some() {
            self.discontinuity = true;
        }
        self.waiting_key = true;
    }

    fn finish(&mut self) {
        let Some((ts, data)) = self.buf.take() else {
            return;
        };
        let key = is_keyframe(&data);
        if let Some((w, h)) = keyframe_size(&data) {
            self.cfg.update(CodecConfig {
                codec: Codec::Vp8,
                generation: 0,
                record: Bytes::new(),
                video: Some(VideoParams {
                    width: w,
                    height: h,
                    ..Default::default()
                }),
                audio: None,
            });
        }
        if self.waiting_key && !key {
            self.discontinuity = true;
            return;
        }
        self.waiting_key = false;
        self.frames.push(Frame {
            timestamp: ts,
            payload: Bytes::from(data),
            keyframe: key,
            discontinuity: std::mem::take(&mut self.discontinuity),
            config: self.cfg.take_announce(),
            config_generation: self.cfg.generation,
            duration: 0,
        });
    }
}

impl Depacketizer for Vp8Depacketizer {
    fn codec(&self) -> Codec {
        Codec::Vp8
    }

    fn clock_rate(&self) -> u32 {
        90000
    }

    fn push(&mut self, pkt: &RtpPacket) -> Result<()> {
        if self.seq.gap(pkt.header.sequence) {
            self.drop_current();
            self.discontinuity = true;
        }
        let ts = pkt.header.timestamp;
        if self.buf.as_ref().is_some_and(|(t, _)| *t != ts) {
            // marker が来ないまま次のフレーム
            self.drop_current();
        }
        let d = match Vp8Descriptor::parse(&pkt.payload) {
            Ok(d) => d,
            Err(e) => {
                self.drop_current();
                return Err(e);
            }
        };
        let data = &pkt.payload[d.len..];
        if d.start && d.partition_id == 0 {
            if self.buf.is_some() {
                self.drop_current();
            }
            self.buf = Some((ts, Vec::with_capacity(data.len() * 4)));
        }
        let Some((_, buf)) = self.buf.as_mut() else {
            // 始まりを受け取っていない
            self.waiting_key = true;
            self.discontinuity = true;
            return Ok(());
        };
        if buf.len() + data.len() > MAX_AU_SIZE {
            self.drop_current();
            return Err(Error::Limit("VP8 のフレームの大きさ"));
        }
        buf.extend_from_slice(data);
        if pkt.header.marker {
            self.finish();
        }
        Ok(())
    }

    fn pop(&mut self) -> Option<Frame> {
        self.frames.pop()
    }

    fn mark_loss(&mut self) {
        self.drop_current();
        self.discontinuity = true;
    }

    fn config(&self) -> Option<&CodecConfig> {
        self.cfg.current.as_ref()
    }
}

/// VP8 の packetizer（X=1・I=1 の 15 ビット PictureID を付ける）。
pub struct Vp8Packetizer {
    mtu: usize,
    picture_id: u16,
}

impl Vp8Packetizer {
    pub fn new(mtu: usize) -> Self {
        Vp8Packetizer {
            mtu: mtu.max(16),
            picture_id: 0,
        }
    }
}

impl Packetizer for Vp8Packetizer {
    fn codec(&self) -> Codec {
        Codec::Vp8
    }

    fn clock_rate(&self) -> u32 {
        90000
    }

    fn packetize(
        &mut self,
        au: &AccessUnit,
        _config: Option<&CodecConfig>,
        out: &mut Vec<Payload>,
    ) -> Result<()> {
        if au.payload.is_empty() {
            return Err(Error::Invalid("空の VP8 フレーム"));
        }
        let pid = self.picture_id;
        self.picture_id = (self.picture_id + 1) & 0x7fff;
        let chunk = self.mtu - 4;
        let n = au.payload.len().div_ceil(chunk);
        for (i, part) in au.payload.chunks(chunk).enumerate() {
            let mut v = Vec::with_capacity(4 + part.len());
            v.push(0x80 | if i == 0 { 0x10 } else { 0 });
            v.push(0x80);
            v.push(0x80 | (pid >> 8) as u8);
            v.push(pid as u8);
            v.extend_from_slice(part);
            out.push(Payload::new(Bytes::from(v), i + 1 == n));
        }
        Ok(())
    }
}

/// SDP に書く payload format。
pub fn payload_format() -> PayloadFormat {
    PayloadFormat {
        encoding: "VP8".into(),
        clock_rate: 90000,
        channels: None,
        fmtp: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::testutil::{fuzz, run};
    use senca_core::{Timebase, TrackId};

    pub(crate) fn au(codec: Codec, payload: Bytes, key: bool) -> AccessUnit {
        AccessUnit {
            track: TrackId(0),
            codec,
            epoch: 0,
            sequence: 0,
            config_generation: 0,
            pts: 0,
            dts: 0,
            duration: 0,
            timebase: Timebase::MPEG,
            keyframe: key,
            discontinuity: false,
            capture_time_ns: None,
            payload,
        }
    }

    fn keyframe(len: usize) -> Vec<u8> {
        let mut v = vec![0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a, 0x40, 0x01, 0xF0, 0x00];
        v.extend((0..len).map(|i| i as u8));
        v
    }

    #[test]
    fn rfc7741_descriptor_examples() {
        // X=1 S=1、I=1 M=1 PictureID=0x1234、L=1 TL0PICIDX=5、T=1 K=1
        let d = Vp8Descriptor::parse(&[0x90, 0xF0, 0x92, 0x34, 5, 0b1010_0011, 0xAA]).unwrap();
        assert!(d.start);
        assert_eq!(d.picture_id, Some(0x1234));
        assert_eq!(d.tl0_pic_idx, Some(5));
        assert_eq!(d.tid, Some((2, true)));
        assert_eq!(d.key_idx, Some(3));
        assert_eq!(d.len, 6);
        // 7 ビットの PictureID
        let d = Vp8Descriptor::parse(&[0x90, 0x80, 0x11, 0xAA]).unwrap();
        assert_eq!(d.picture_id, Some(0x11));
        assert!(Vp8Descriptor::parse(&[0x90, 0x80, 0x91]).is_err());
        assert!(Vp8Descriptor::parse(&[0x10]).is_err());
    }

    #[test]
    fn roundtrip_and_loss() {
        let mut p = Vp8Packetizer::new(300);
        let mut d = Vp8Depacketizer::new();
        let k = keyframe(1000);
        let mut out = Vec::new();
        p.packetize(
            &au(Codec::Vp8, Bytes::from(k.clone()), true),
            None,
            &mut out,
        )
        .unwrap();
        assert_eq!(out.len(), 4);
        let mut seq = 0;
        let f = run(&mut d, &out, 0, &mut seq);
        assert_eq!(f.len(), 1);
        assert_eq!(&f[0].payload[..], &k[..]);
        assert!(f[0].keyframe);
        let v = f[0].config.as_ref().unwrap().video.unwrap();
        assert_eq!((v.width, v.height), (320, 240));
        // インターフレームの途中を落とす → 捨ててキーフレームを待つ
        let mut inter = keyframe(800);
        inter[0] |= 1;
        let mut out = Vec::new();
        p.packetize(
            &au(Codec::Vp8, Bytes::from(inter.clone()), false),
            None,
            &mut out,
        )
        .unwrap();
        out.remove(1);
        assert!(run(&mut d, &out[..1], 3000, &mut seq).is_empty());
        seq = seq.wrapping_add(1);
        assert!(run(&mut d, &out[1..], 3000, &mut seq).is_empty());
        let mut out = Vec::new();
        p.packetize(&au(Codec::Vp8, Bytes::from(inter), false), None, &mut out)
            .unwrap();
        assert!(run(&mut d, &out, 6000, &mut seq).is_empty());
        let mut out = Vec::new();
        p.packetize(&au(Codec::Vp8, Bytes::from(k), true), None, &mut out)
            .unwrap();
        let f = run(&mut d, &out, 9000, &mut seq);
        assert!(f[0].discontinuity && f[0].keyframe);
        assert!(f[0].config.is_none());
    }

    #[test]
    fn fuzz_vp8() {
        let mut p = Vp8Packetizer::new(100);
        let mut seeds = Vec::new();
        p.packetize(
            &au(Codec::Vp8, Bytes::from(keyframe(300)), true),
            None,
            &mut seeds,
        )
        .unwrap();
        fuzz(&mut Vp8Depacketizer::new(), &seeds, 20000);
    }
}
