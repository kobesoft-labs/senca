//! AV1（AV1 RTP Payload Format, AOM v1.0.0）。
//!
//! aggregation header: `Z Y W(2) N RES(3)`。Z: 最初の要素は前のパケットの OBU の続き、Y: 最後の要素は次へ続く、
//! W: 要素の数（0 ならすべての要素に leb128 の長さ。1〜3 なら最後の要素だけ長さなし）、N: 新しい coded video sequence の始まり。
//!
//! RTP 上の OBU は obu_has_size_field = 0 が推奨（1 も受け付ける）。Temporal Delimiter・tile list・padding は送らず、受けても捨てる。
//! 内部表現（ADR-0003）は obu_size 付きの OBU 列なので、受信で付け直し、送信で外す。

use super::{
    ConfigState, Depacketizer, Frame, FrameQueue, MAX_AU_SIZE, Packetizer, Payload, PayloadFormat,
    SeqWatch,
};
use crate::error::{Error, Result};
use crate::packet::RtpPacket;
use bytes::Bytes;
use senca_av1::{
    Av1CodecConfigurationRecord, ObuHeader, ObuType, SequenceHeader, read_leb128, write_leb128,
    write_obu,
};
use senca_core::media::{AccessUnit, Codec, CodecConfig};

/// 1 パケットの OBU 要素の上限。
const MAX_ELEMENTS: usize = 256;

fn leb_len(v: usize) -> usize {
    let mut n = 1;
    let mut v = v >> 7;
    while v > 0 {
        n += 1;
        v >>= 7;
    }
    n
}

/// AV1 の depacketizer。
#[derive(Default)]
pub struct Av1Depacketizer {
    seq: SeqWatch,
    ts: Option<u32>,
    tu: Vec<u8>,
    partial: Option<Vec<u8>>,
    broken: bool,
    waiting_key: bool,
    discontinuity: bool,
    seq_header: Option<SequenceHeader>,
    cfg: ConfigState,
    out: FrameQueue,
}

impl Av1Depacketizer {
    pub fn new() -> Self {
        Av1Depacketizer {
            waiting_key: true,
            ..Default::default()
        }
    }

    fn drop_tu(&mut self) {
        if self.ts.take().is_some() {
            self.discontinuity = true;
        }
        self.tu.clear();
        self.partial = None;
        self.broken = false;
        self.waiting_key = true;
    }

    /// 完成した OBU 要素 1 つを TU に足す。
    fn obu(&mut self, el: &[u8]) -> Result<()> {
        let (h, hlen) = ObuHeader::parse(el).map_err(|_| Error::Invalid("OBU ヘッダー"))?;
        let payload = if h.has_size_field {
            let (size, n) = read_leb128(&el[hlen..]).map_err(|_| Error::Invalid("obu_size"))?;
            let start = hlen + n;
            el.get(start..start + size as usize)
                .ok_or(Error::Truncated)?
        } else {
            &el[hlen..]
        };
        match h.obu_type {
            ObuType::TemporalDelimiter | ObuType::TileList | ObuType::Padding => return Ok(()),
            ObuType::SequenceHeader => {
                let sh = SequenceHeader::parse(payload)
                    .map_err(|_| Error::Invalid("シーケンスヘッダー"))?;
                let mut v = Vec::with_capacity(payload.len() + 4);
                write_obu(ObuType::SequenceHeader, h.extension, payload, &mut v)
                    .map_err(|_| Error::Invalid("シーケンスヘッダー"))?;
                if let Ok(c) = Av1CodecConfigurationRecord::from_sequence_header_obu(&v)
                    .and_then(|r| r.codec_config(0))
                {
                    self.cfg.update(c);
                }
                self.seq_header = Some(sh);
            }
            _ => {}
        }
        if self.tu.len() + payload.len() + 8 > MAX_AU_SIZE {
            return Err(Error::Limit("AV1 の TU の大きさ"));
        }
        write_obu(h.obu_type, h.extension, payload, &mut self.tu).map_err(|_| Error::Invalid("OBU"))
    }

    fn parse(&mut self, p: &[u8]) -> Result<()> {
        let agg = *p.first().ok_or(Error::Truncated)?;
        let z = agg & 0x80 != 0;
        let y = agg & 0x40 != 0;
        let w = ((agg >> 4) & 3) as usize;
        let mut pos = 1usize;
        let mut k = 0usize;
        while pos < p.len() {
            if k >= MAX_ELEMENTS {
                return Err(Error::Limit("OBU 要素の数"));
            }
            let last_by_w = w != 0 && k + 1 == w;
            let len = if last_by_w {
                p.len() - pos
            } else {
                let (l, n) =
                    read_leb128(&p[pos..]).map_err(|_| Error::Invalid("OBU 要素の長さ"))?;
                pos += n;
                l as usize
            };
            let end = pos.checked_add(len).ok_or(Error::Truncated)?;
            if end > p.len() {
                return Err(Error::Truncated);
            }
            let el = &p[pos..end];
            pos = end;
            let is_last = pos >= p.len();
            if w != 0 && k + 1 > w {
                return Err(Error::Invalid("W より多い OBU 要素"));
            }
            let mut buf = if k == 0 && z {
                match self.partial.take() {
                    Some(b) => b,
                    None => {
                        // 続きの始まりを受け取っていない
                        self.broken = true;
                        k += 1;
                        continue;
                    }
                }
            } else {
                if self.partial.take().is_some() {
                    self.broken = true;
                }
                Vec::new()
            };
            if buf.len() + el.len() > MAX_AU_SIZE {
                return Err(Error::Limit("OBU の大きさ"));
            }
            buf.extend_from_slice(el);
            if is_last && y {
                self.partial = Some(buf);
            } else if !buf.is_empty() {
                self.obu(&buf)?;
            }
            k += 1;
        }
        if z && k == 0 {
            return Err(Error::Invalid("空の aggregation"));
        }
        Ok(())
    }

    fn finish(&mut self) {
        let ts = self.ts.take();
        let tu = std::mem::take(&mut self.tu);
        let broken = std::mem::take(&mut self.broken) || self.partial.take().is_some();
        let Some(ts) = ts else { return };
        if broken {
            self.discontinuity = true;
            self.waiting_key = true;
            return;
        }
        if tu.is_empty() {
            return;
        }
        let key = senca_av1::is_keyframe(&tu, self.seq_header.as_ref()).unwrap_or(false);
        if self.waiting_key && !key || self.cfg.current.is_none() {
            self.discontinuity = true;
            return;
        }
        self.waiting_key = false;
        self.out.push(Frame {
            timestamp: ts,
            payload: Bytes::from(tu),
            keyframe: key,
            discontinuity: std::mem::take(&mut self.discontinuity),
            config: self.cfg.take_announce(),
            config_generation: self.cfg.generation,
            duration: 0,
        });
    }
}

impl Depacketizer for Av1Depacketizer {
    fn codec(&self) -> Codec {
        Codec::Av1
    }

    fn clock_rate(&self) -> u32 {
        90000
    }

    fn push(&mut self, pkt: &RtpPacket) -> Result<()> {
        if self.seq.gap(pkt.header.sequence) {
            self.drop_tu();
            self.discontinuity = true;
        }
        let ts = pkt.header.timestamp;
        if self.ts.is_some_and(|t| t != ts) {
            self.drop_tu();
        }
        self.ts = Some(ts);
        let r = self.parse(&pkt.payload);
        if r.is_err() {
            self.broken = true;
            self.partial = None;
        }
        if pkt.header.marker {
            self.finish();
        }
        r
    }

    fn pop(&mut self) -> Option<Frame> {
        self.out.pop()
    }

    fn mark_loss(&mut self) {
        self.drop_tu();
        self.discontinuity = true;
    }

    fn config(&self) -> Option<&CodecConfig> {
        self.cfg.current.as_ref()
    }
}

/// AV1 の packetizer（W=0: すべての要素に長さを付ける）。
pub struct Av1Packetizer {
    mtu: usize,
}

impl Av1Packetizer {
    pub fn new(mtu: usize) -> Self {
        Av1Packetizer { mtu: mtu.max(16) }
    }
}

impl Packetizer for Av1Packetizer {
    fn codec(&self) -> Codec {
        Codec::Av1
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
        // OBU を obu_size なしの要素にする
        let mut elements: Vec<Vec<u8>> = Vec::new();
        let mut has_seq = false;
        for obu in senca_av1::obus(&au.payload) {
            let obu = obu.map_err(|_| Error::Invalid("OBU 列"))?;
            match obu.header.obu_type {
                ObuType::TemporalDelimiter | ObuType::TileList | ObuType::Padding => continue,
                ObuType::SequenceHeader => has_seq = true,
                _ => {}
            }
            let mut e = Vec::with_capacity(obu.payload.len() + 2);
            ObuHeader {
                has_size_field: false,
                ..obu.header
            }
            .write(&mut e);
            e.extend_from_slice(obu.payload);
            elements.push(e);
        }
        if elements.is_empty() {
            return Err(Error::Invalid("空の TU"));
        }
        let new_cvs =
            has_seq && (au.keyframe || senca_av1::is_keyframe(&au.payload, None).unwrap_or(false));
        let start = out.len();
        let room_total = self.mtu - 1;
        let mut cur: Vec<u8> = Vec::new();
        let mut cur_z = false;
        let flush = |cur: &mut Vec<u8>, z: bool, y: bool, out: &mut Vec<Payload>| {
            let first = out.len() == start;
            let agg = (z as u8) << 7 | (y as u8) << 6 | ((first && new_cvs) as u8) << 3;
            let mut v = Vec::with_capacity(1 + cur.len());
            v.push(agg);
            v.append(cur);
            out.push(Payload::new(Bytes::from(v), false));
        };
        for el in &elements {
            let mut off = 0usize;
            while off < el.len() {
                let room = room_total - cur.len();
                let remaining = el.len() - off;
                if leb_len(remaining) + remaining <= room {
                    write_leb128(remaining as u32, &mut cur);
                    cur.extend_from_slice(&el[off..]);
                    break;
                }
                if room < 4 {
                    // 余白が小さい: 次のパケットから始める
                    flush(&mut cur, cur_z, false, out);
                    cur_z = false;
                    continue;
                }
                let frag = room - leb_len(room);
                write_leb128(frag as u32, &mut cur);
                cur.extend_from_slice(&el[off..off + frag]);
                off += frag;
                flush(&mut cur, cur_z, true, out);
                cur_z = true;
            }
        }
        if !cur.is_empty() {
            flush(&mut cur, cur_z, false, out);
        }
        if let Some(l) = out[start..].last_mut() {
            l.marker = true;
        }
        Ok(())
    }
}

/// SDP に書く payload format。
pub fn payload_format() -> PayloadFormat {
    PayloadFormat {
        encoding: "AV1".into(),
        clock_rate: 90000,
        channels: None,
        fmtp: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::testutil::{fuzz, run};

    /// 実際の libaom のシーケンスヘッダー OBU（obu_size 付き、320x240）。
    pub(crate) const SEQ: &[u8] = &[
        0x0A, 0x0B, 0x00, 0x00, 0x00, 0x04, 0x3C, 0xFF, 0xBC, 0xDA, 0xF9, 0x00, 0x40,
    ];

    fn frame_obu(len: usize, key: bool) -> Vec<u8> {
        // OBU_FRAME: show_existing_frame=0, frame_type(2)=KEY(0)/INTER(1), show_frame=1
        let mut payload = vec![if key { 0b0001_0000 } else { 0b0011_0000 }];
        payload.extend((0..len).map(|i| (i * 7) as u8));
        let mut v = Vec::new();
        write_obu(ObuType::Frame, None, &payload, &mut v).unwrap();
        v
    }

    fn tu(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    fn au(payload: Vec<u8>, key: bool) -> AccessUnit {
        let mut a = crate::payload::h264::tests::au(Bytes::from(payload), key, 0);
        a.codec = Codec::Av1;
        a
    }

    #[test]
    fn roundtrip_fragmented() {
        let mut td = Vec::new();
        write_obu(ObuType::TemporalDelimiter, None, &[], &mut td).unwrap();
        let key = tu(&[&td, SEQ, &frame_obu(3000, true)]);
        let mut p = Av1Packetizer::new(500);
        let mut out = Vec::new();
        p.packetize(&au(key.clone(), true), None, &mut out).unwrap();
        assert!(out.len() >= 7);
        assert_eq!(out[0].data[0] & 0x08, 0x08, "N");
        assert_eq!(out[1].data[0] & 0xC0, 0xC0, "Z・Y");
        assert!(out.iter().all(|p| p.data.len() <= 500));
        let mut d = Av1Depacketizer::new();
        let mut seq = 0;
        let f = run(&mut d, &out, 0, &mut seq);
        assert_eq!(f.len(), 1);
        // TD は除かれる
        assert_eq!(&f[0].payload[..], &key[td.len()..]);
        assert!(f[0].keyframe);
        assert!(f[0].config.is_some());
        let inter = frame_obu(100, false);
        let mut out = Vec::new();
        p.packetize(&au(inter.clone(), false), None, &mut out)
            .unwrap();
        assert_eq!(out.len(), 1);
        let f = run(&mut d, &out, 3000, &mut seq);
        assert_eq!(&f[0].payload[..], &inter[..]);
        assert!(!f[0].keyframe);
    }

    #[test]
    fn w_field_and_size_field_obus() {
        // W=2: 1 つ目は長さ付き、2 つ目は長さなし。2 つ目の OBU は obu_size 付き（許される）
        let mut d = Av1Depacketizer::new();
        let seq_nosize = {
            let mut v = vec![0x08];
            v.extend_from_slice(&SEQ[2..]);
            v
        };
        let fr = frame_obu(10, true);
        let mut p = vec![0x20 | 0x08];
        p.push(seq_nosize.len() as u8);
        p.extend_from_slice(&seq_nosize);
        p.extend_from_slice(&fr);
        d.push(&RtpPacket::new(96, 0, 0, 1, true, Bytes::from(p)))
            .unwrap();
        let f = d.pop().unwrap();
        assert_eq!(&f.payload[..], &tu(&[SEQ, &fr])[..]);
    }

    #[test]
    fn loss_of_middle_fragment() {
        let key = tu(&[SEQ, &frame_obu(2000, true)]);
        let mut p = Av1Packetizer::new(300);
        let mut out = Vec::new();
        p.packetize(&au(key.clone(), true), None, &mut out).unwrap();
        let mut d = Av1Depacketizer::new();
        let mut seq = 0;
        out.remove(2);
        assert!(run(&mut d, &out[..2], 0, &mut seq).is_empty());
        seq += 1;
        assert!(run(&mut d, &out[2..], 0, &mut seq).is_empty());
        let mut out = Vec::new();
        p.packetize(&au(frame_obu(50, false), false), None, &mut out)
            .unwrap();
        assert!(run(&mut d, &out, 3000, &mut seq).is_empty());
        let mut out = Vec::new();
        p.packetize(&au(key, true), None, &mut out).unwrap();
        let f = run(&mut d, &out, 6000, &mut seq);
        assert!(f[0].keyframe && f[0].discontinuity);
    }

    #[test]
    fn malformed() {
        let mut d = Av1Depacketizer::new();
        for bad in [
            &[0x00u8, 0x05, 1][..],
            &[0x80],
            &[0x10, 0x80],
            &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
        ] {
            assert!(
                d.push(&RtpPacket::new(
                    96,
                    0,
                    0,
                    1,
                    true,
                    Bytes::copy_from_slice(bad)
                ))
                .is_err(),
                "{bad:?}"
            );
        }
        let mut p = Av1Packetizer::new(120);
        let mut seeds = Vec::new();
        p.packetize(
            &au(tu(&[SEQ, &frame_obu(500, true)]), true),
            None,
            &mut seeds,
        )
        .unwrap();
        fuzz(&mut d, &seeds, 20000);
    }
}
