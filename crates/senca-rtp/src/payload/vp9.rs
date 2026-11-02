//! VP9（RFC 9628）。flexible・non-flexible の両方の descriptor を読む。送信は non-flexible の最小形。
//!
//! descriptor: `I P L F B E V Z`。I: PictureID（7・15 ビット）、L: レイヤー（TID U SID D と、non-flexible なら TL0PICIDX）、
//! F かつ P: P_DIFF（最大 3 つ）、V: scalability structure。B・E はフレーム（レイヤー）の始まり・終わり、marker はピクチャの終わり。
//!
//! 受信: 1 つのピクチャに複数のフレーム（空間レイヤー）があれば superframe（索引付き）にまとめる。
//! 送信: AU（superframe を含む）を 1 つのフレームとして B〜E で送る（FFmpeg と同じ）。

use super::{
    ConfigState, Depacketizer, Frame, FrameQueue, MAX_AU_SIZE, Packetizer, Payload, PayloadFormat,
    SeqWatch,
};
use crate::error::{Error, Result};
use crate::packet::RtpPacket;
use bytes::Bytes;
use senca_core::bits::BitReader;
use senca_core::media::{AccessUnit, Codec, CodecConfig, VideoParams};

/// payload descriptor の解析結果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Vp9Descriptor {
    pub inter_picture: bool,
    pub flexible: bool,
    pub begin: bool,
    pub end: bool,
    pub not_reference_upper: bool,
    pub picture_id: Option<u16>,
    /// (TID, U, SID, D)。
    pub layer: Option<(u8, bool, u8, bool)>,
    pub tl0_pic_idx: Option<u8>,
    pub p_diffs: Vec<u8>,
    /// scalability structure の各空間レイヤーの解像度。
    pub resolutions: Vec<(u16, u16)>,
    pub len: usize,
}

impl Vp9Descriptor {
    pub fn parse(p: &[u8]) -> Result<Vp9Descriptor> {
        let mut i = 0usize;
        let mut next = || -> Result<u8> {
            let b = *p.get(i).ok_or(Error::Truncated)?;
            i += 1;
            Ok(b)
        };
        let b0 = next()?;
        let mut d = Vp9Descriptor {
            inter_picture: b0 & 0x40 != 0,
            flexible: b0 & 0x10 != 0,
            begin: b0 & 0x08 != 0,
            end: b0 & 0x04 != 0,
            not_reference_upper: b0 & 0x01 != 0,
            ..Default::default()
        };
        if b0 & 0x80 != 0 {
            let a = next()?;
            d.picture_id = Some(if a & 0x80 != 0 {
                ((a as u16 & 0x7f) << 8) | next()? as u16
            } else {
                a as u16
            });
        }
        if b0 & 0x20 != 0 {
            let l = next()?;
            d.layer = Some((l >> 5, l & 0x10 != 0, (l >> 1) & 7, l & 1 != 0));
            if !d.flexible {
                d.tl0_pic_idx = Some(next()?);
            }
        }
        if d.flexible && d.inter_picture {
            loop {
                let pd = next()?;
                d.p_diffs.push(pd >> 1);
                if pd & 1 == 0 {
                    break;
                }
                if d.p_diffs.len() >= 3 {
                    return Err(Error::Invalid("P_DIFF が 3 つを超える"));
                }
            }
        }
        if b0 & 0x02 != 0 {
            let ss = next()?;
            let n_s = (ss >> 5) as usize + 1;
            if ss & 0x10 != 0 {
                for _ in 0..n_s {
                    let w = u16::from_be_bytes([next()?, next()?]);
                    let h = u16::from_be_bytes([next()?, next()?]);
                    d.resolutions.push((w, h));
                }
            }
            if ss & 0x08 != 0 {
                let n_g = next()?;
                for _ in 0..n_g {
                    let g = next()?;
                    for _ in 0..((g >> 2) & 3) {
                        next()?;
                    }
                }
            }
        }
        if i >= p.len() {
            return Err(Error::Invalid("VP9 の payload が空"));
        }
        d.len = i;
        Ok(d)
    }
}

/// キーフレームの (幅, 高さ, profile)。
pub type KeyframeInfo = Option<(u32, u32, u8)>;

/// 非圧縮ヘッダーの先頭から（キーフレームか, キーフレームなら (幅, 高さ, profile)）。
pub fn parse_frame_header(frame: &[u8]) -> Option<(bool, KeyframeInfo)> {
    let mut r = BitReader::new(frame);
    if r.read_bits(2)? != 2 {
        return None;
    }
    let lo = r.read_bit()? as u8;
    let hi = r.read_bit()? as u8;
    let profile = (hi << 1) | lo;
    if profile == 3 {
        r.read_bit()?;
    }
    if r.read_bit()? {
        // show_existing_frame
        return Some((false, None));
    }
    let key = !r.read_bit()?;
    if !key {
        return Some((false, None));
    }
    r.read_bit()?; // show_frame
    r.read_bit()?; // error_resilient_mode
    if r.read_bits(24)? != 0x49_83_42 {
        return Some((true, None));
    }
    if profile >= 2 {
        r.read_bit()?; // ten_or_twelve_bit
    }
    let cs = r.read_bits(3)?;
    if cs != 7 {
        r.read_bit()?; // color_range
        if profile == 1 || profile == 3 {
            r.read_bits(3)?;
        }
    } else if profile == 1 || profile == 3 {
        r.read_bit()?;
    }
    let w = r.read_bits(16)? as u32 + 1;
    let h = r.read_bits(16)? as u32 + 1;
    Some((true, Some((w, h, profile))))
}

/// superframe の索引があればフレームに分ける（無ければ全体を 1 つ）。
pub fn split_superframe(data: &[u8]) -> Vec<&[u8]> {
    let Some(&last) = data.last() else {
        return vec![data];
    };
    if last & 0xe0 != 0xc0 {
        return vec![data];
    }
    let frames = (last & 7) as usize + 1;
    let mag = ((last >> 3) & 3) as usize + 1;
    let index = 2 + mag * frames;
    if data.len() < index || data[data.len() - index] != last {
        return vec![data];
    }
    let mut out = Vec::with_capacity(frames);
    let mut pos = 0usize;
    let idx = &data[data.len() - index + 1..];
    for f in 0..frames {
        let mut size = 0usize;
        for b in 0..mag {
            size |= (idx[f * mag + b] as usize) << (8 * b);
        }
        if pos + size > data.len() - index {
            return vec![data];
        }
        out.push(&data[pos..pos + size]);
        pos += size;
    }
    out
}

/// フレームを superframe にまとめる。
pub fn build_superframe(frames: &[Vec<u8>]) -> Result<Vec<u8>> {
    if frames.len() == 1 {
        return Ok(frames[0].clone());
    }
    if frames.is_empty() || frames.len() > 8 {
        return Err(Error::Invalid("superframe のフレーム数"));
    }
    let max = frames.iter().map(|f| f.len()).max().unwrap_or(0);
    let mag = match max {
        0..=0xff => 1,
        0x100..=0xffff => 2,
        0x1_0000..=0xff_ffff => 3,
        _ => 4,
    };
    let marker = 0xc0 | ((mag as u8 - 1) << 3) | (frames.len() as u8 - 1);
    let mut out =
        Vec::with_capacity(frames.iter().map(|f| f.len()).sum::<usize>() + 2 + mag * frames.len());
    for f in frames {
        out.extend_from_slice(f);
    }
    out.push(marker);
    for f in frames {
        out.extend_from_slice(&(f.len() as u32).to_le_bytes()[..mag]);
    }
    out.push(marker);
    Ok(out)
}

/// VP9 の depacketizer。
#[derive(Default)]
pub struct Vp9Depacketizer {
    seq: SeqWatch,
    ts: Option<u32>,
    frames_in_pic: Vec<Vec<u8>>,
    cur: Option<Vec<u8>>,
    size: usize,
    waiting_key: bool,
    discontinuity: bool,
    cfg: ConfigState,
    out: FrameQueue,
}

impl Vp9Depacketizer {
    pub fn new() -> Self {
        Vp9Depacketizer {
            waiting_key: true,
            ..Default::default()
        }
    }

    fn drop_picture(&mut self) {
        if self.ts.take().is_some() {
            self.discontinuity = true;
        }
        self.frames_in_pic.clear();
        self.cur = None;
        self.size = 0;
        self.waiting_key = true;
    }

    fn finish(&mut self) {
        let ts = self.ts.take();
        let mut frames = std::mem::take(&mut self.frames_in_pic);
        self.size = 0;
        if let Some(c) = self.cur.take() {
            // E が来ないまま終わったフレームも、marker で終わったものとして入れる
            frames.push(c);
        }
        let Some(ts) = ts else { return };
        if frames.is_empty() {
            return;
        }
        let header = parse_frame_header(&frames[0]);
        let key = header.is_some_and(|h| h.0);
        if let Some((_, Some((w, h, profile)))) = header {
            self.cfg.update(CodecConfig {
                codec: Codec::Vp9,
                generation: 0,
                record: Bytes::new(),
                video: Some(VideoParams {
                    width: w,
                    height: h,
                    profile,
                    ..Default::default()
                }),
                audio: None,
            });
        }
        if self.waiting_key && !key {
            self.discontinuity = true;
            return;
        }
        let Ok(payload) = build_superframe(&frames) else {
            self.discontinuity = true;
            self.waiting_key = true;
            return;
        };
        self.waiting_key = false;
        self.out.push(Frame {
            timestamp: ts,
            payload: Bytes::from(payload),
            keyframe: key,
            discontinuity: std::mem::take(&mut self.discontinuity),
            config: self.cfg.take_announce(),
            config_generation: self.cfg.generation,
            duration: 0,
        });
    }
}

impl Depacketizer for Vp9Depacketizer {
    fn codec(&self) -> Codec {
        Codec::Vp9
    }

    fn clock_rate(&self) -> u32 {
        90000
    }

    fn push(&mut self, pkt: &RtpPacket) -> Result<()> {
        if self.seq.gap(pkt.header.sequence) {
            self.drop_picture();
            self.discontinuity = true;
        }
        let ts = pkt.header.timestamp;
        if self.ts.is_some_and(|t| t != ts) {
            self.drop_picture();
        }
        let d = match Vp9Descriptor::parse(&pkt.payload) {
            Ok(d) => d,
            Err(e) => {
                self.drop_picture();
                return Err(e);
            }
        };
        let data = &pkt.payload[d.len..];
        if d.begin {
            if let Some(c) = self.cur.take() {
                // E のないまま次のフレーム
                self.frames_in_pic.push(c);
            }
            if self.frames_in_pic.len() >= 8 {
                self.drop_picture();
                return Err(Error::Limit("1 ピクチャのフレーム数"));
            }
            self.ts = Some(ts);
            self.cur = Some(Vec::with_capacity(data.len() * 4));
        }
        let Some(cur) = self.cur.as_mut() else {
            // フレームの始まり（B）を受け取っていない: このピクチャは捨てる
            self.drop_picture();
            self.discontinuity = true;
            return Ok(());
        };
        self.size += data.len();
        if self.size > MAX_AU_SIZE {
            self.drop_picture();
            return Err(Error::Limit("VP9 のピクチャの大きさ"));
        }
        cur.extend_from_slice(data);
        if d.end
            && let Some(c) = self.cur.take()
        {
            self.frames_in_pic.push(c);
        }
        if pkt.header.marker {
            self.finish();
        }
        Ok(())
    }

    fn pop(&mut self) -> Option<Frame> {
        self.out.pop()
    }

    fn mark_loss(&mut self) {
        self.drop_picture();
        self.discontinuity = true;
    }

    fn config(&self) -> Option<&CodecConfig> {
        self.cfg.current.as_ref()
    }
}

/// VP9 の packetizer（non-flexible、I=1 の 15 ビット PictureID）。
pub struct Vp9Packetizer {
    mtu: usize,
    picture_id: u16,
}

impl Vp9Packetizer {
    pub fn new(mtu: usize) -> Self {
        Vp9Packetizer {
            mtu: mtu.max(16),
            picture_id: 0,
        }
    }
}

impl Packetizer for Vp9Packetizer {
    fn codec(&self) -> Codec {
        Codec::Vp9
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
            return Err(Error::Invalid("空の VP9 フレーム"));
        }
        let key = parse_frame_header(&au.payload).is_some_and(|h| h.0) || au.keyframe;
        let pid = self.picture_id;
        self.picture_id = (self.picture_id + 1) & 0x7fff;
        let chunk = self.mtu - 3;
        let n = au.payload.len().div_ceil(chunk);
        for (i, part) in au.payload.chunks(chunk).enumerate() {
            let mut b0 = 0x80u8;
            if !key {
                b0 |= 0x40;
            }
            if i == 0 {
                b0 |= 0x08;
            }
            if i + 1 == n {
                b0 |= 0x04;
            }
            let mut v = Vec::with_capacity(3 + part.len());
            v.push(b0);
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
        encoding: "VP9".into(),
        clock_rate: 90000,
        channels: None,
        fmtp: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::testutil::{fuzz, run};

    fn au(payload: Vec<u8>) -> AccessUnit {
        let mut a = crate::payload::h264::tests::au(Bytes::from(payload), false, 0);
        a.codec = Codec::Vp9;
        a
    }

    /// profile 0 のキーフレームのヘッダー（幅 320・高さ 240）。
    fn keyframe(len: usize) -> Vec<u8> {
        // 10 0 0 | 0 show_existing | 0 key | 1 show | 0 err | sync 0x498342 | cs(3)=1 range 0 | w-1 16 | h-1 16
        let mut w = senca_core::bits::BitWriter::new();
        w.write_bits(2, 2);
        w.write_bits(0, 2);
        w.write_bit(false);
        w.write_bit(false);
        w.write_bit(true);
        w.write_bit(false);
        w.write_bits(0x49_83_42, 24);
        w.write_bits(1, 3);
        w.write_bit(false);
        w.write_bits(319, 16);
        w.write_bits(239, 16);
        w.write_bits(0, 4);
        let mut v = w.finish();
        v.extend((0..len).map(|i| i as u8));
        v
    }

    #[test]
    fn descriptor_flexible_and_ss() {
        // I=1(15bit) P=1 L=1 F=1 B=1 E=0 V=0 → P_DIFF 2 つ
        let d = Vp9Descriptor::parse(&[0xF8, 0x81, 0x02, 0x22, 0x03, 0x04, 0xAA]).unwrap();
        assert!(d.flexible && d.begin && !d.end && d.inter_picture);
        assert_eq!(d.picture_id, Some(0x102));
        assert_eq!(d.layer, Some((1, false, 1, false)));
        assert_eq!(d.p_diffs, vec![1, 2]);
        assert_eq!(d.len, 6);
        // non-flexible、V=1: N_S=0 Y=1 G=1 N_G=1 (R=1 → P_DIFF 1)
        let d = Vp9Descriptor::parse(&[0x0E, 0x18, 0x01, 0x40, 0x00, 0xF0, 0x01, 0x04, 0x01, 0xAA])
            .unwrap();
        assert_eq!(d.resolutions, vec![(320, 240)]);
        assert_eq!(d.len, 9);
        assert!(Vp9Descriptor::parse(&[0x0E, 0x18, 0x01]).is_err());
    }

    #[test]
    fn frame_header() {
        assert_eq!(
            parse_frame_header(&keyframe(0)),
            Some((true, Some((320, 240, 0))))
        );
        let mut inter = keyframe(0);
        inter[0] |= 0x04; // frame_type = 1
        assert_eq!(parse_frame_header(&inter), Some((false, None)));
    }

    #[test]
    fn superframe_roundtrip() {
        let a = vec![1u8; 300];
        let b = vec![2u8; 10];
        let s = build_superframe(&[a.clone(), b.clone()]).unwrap();
        let parts = split_superframe(&s);
        assert_eq!(parts, vec![&a[..], &b[..]]);
        assert_eq!(split_superframe(&a), vec![&a[..]]);
    }

    #[test]
    fn roundtrip_and_layers() {
        let mut p = Vp9Packetizer::new(200);
        let mut d = Vp9Depacketizer::new();
        let k = keyframe(1000);
        let mut out = Vec::new();
        p.packetize(&au(k.clone()), None, &mut out).unwrap();
        let mut seq = 0;
        let f = run(&mut d, &out, 0, &mut seq);
        assert_eq!(f.len(), 1);
        assert_eq!(&f[0].payload[..], &k[..]);
        assert!(f[0].keyframe);
        assert_eq!(f[0].config.as_ref().unwrap().video.unwrap().width, 320);
        // 2 つの空間レイヤー（B..E を 2 回、最後に marker）→ superframe
        let l0 = keyframe(10);
        let l1 = vec![0x86u8, 1, 2, 3];
        let pk = |b0: u8, data: &[u8], m: bool| {
            let mut v = vec![b0 | 0x20, 0x00, 0];
            v.extend_from_slice(data);
            Payload::new(Bytes::from(v), m)
        };
        let pkts = [pk(0x0C, &l0, false), pk(0x0C, &l1, true)];
        let f = run(&mut d, &pkts, 3000, &mut seq);
        assert_eq!(split_superframe(&f[0].payload), vec![&l0[..], &l1[..]]);
    }

    #[test]
    fn loss_waits_for_key() {
        let mut p = Vp9Packetizer::new(200);
        let mut d = Vp9Depacketizer::new();
        let mut inter = keyframe(600);
        inter[0] |= 0x04;
        let mut out = Vec::new();
        p.packetize(&au(inter), None, &mut out).unwrap();
        let mut seq = 0;
        assert!(run(&mut d, &out, 0, &mut seq).is_empty());
        let mut out = Vec::new();
        p.packetize(&au(keyframe(600)), None, &mut out).unwrap();
        out.remove(1);
        assert!(run(&mut d, &out[..1], 3000, &mut seq).is_empty());
        seq += 1;
        assert!(run(&mut d, &out[1..], 3000, &mut seq).is_empty());
        let mut out = Vec::new();
        p.packetize(&au(keyframe(600)), None, &mut out).unwrap();
        let f = run(&mut d, &out, 6000, &mut seq);
        assert!(f[0].keyframe && f[0].discontinuity);
        let mut seeds = Vec::new();
        p.packetize(&au(keyframe(300)), None, &mut seeds).unwrap();
        fuzz(&mut d, &seeds, 20000);
    }
}
