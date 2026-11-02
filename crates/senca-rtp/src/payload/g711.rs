//! G.711 PCMA・PCMU（RFC 3551 4.5.14）。1 サンプル = 1 バイト、既定は 8 kHz。

use super::{
    ConfigState, Depacketizer, Frame, FrameQueue, Packetizer, Payload, PayloadFormat, SeqWatch,
};
use crate::error::{Error, Result};
use crate::packet::RtpPacket;
use bytes::Bytes;
use senca_core::media::{AccessUnit, AudioParams, Codec, CodecConfig};

/// G.711 の depacketizer。
pub struct G711Depacketizer {
    codec: Codec,
    clock_rate: u32,
    seq: SeqWatch,
    discontinuity: bool,
    cfg: ConfigState,
    out: FrameQueue,
}

impl G711Depacketizer {
    pub fn new(codec: Codec, clock_rate: u32) -> Self {
        let mut cfg = ConfigState::default();
        cfg.update(config(codec, clock_rate.max(1)));
        G711Depacketizer {
            codec,
            clock_rate: clock_rate.max(1),
            seq: SeqWatch::default(),
            discontinuity: false,
            cfg,
            out: FrameQueue::default(),
        }
    }
}

fn config(codec: Codec, rate: u32) -> CodecConfig {
    CodecConfig {
        codec,
        generation: 0,
        record: Bytes::new(),
        video: None,
        audio: Some(AudioParams {
            sample_rate: rate,
            channels: 1,
            object_type: 0,
        }),
    }
}

impl Depacketizer for G711Depacketizer {
    fn codec(&self) -> Codec {
        self.codec
    }

    fn clock_rate(&self) -> u32 {
        self.clock_rate
    }

    fn push(&mut self, pkt: &RtpPacket) -> Result<()> {
        if self.seq.gap(pkt.header.sequence) {
            self.discontinuity = true;
        }
        if pkt.payload.is_empty() {
            return Err(Error::Invalid("空の G.711"));
        }
        self.out.push(Frame {
            timestamp: pkt.header.timestamp,
            payload: pkt.payload.clone(),
            keyframe: true,
            discontinuity: std::mem::take(&mut self.discontinuity),
            config: self.cfg.take_announce(),
            config_generation: self.cfg.generation,
            duration: pkt.payload.len() as u32,
        });
        Ok(())
    }

    fn pop(&mut self) -> Option<Frame> {
        self.out.pop()
    }

    fn mark_loss(&mut self) {
        self.discontinuity = true;
    }

    fn config(&self) -> Option<&CodecConfig> {
        self.cfg.current.as_ref()
    }
}

/// G.711 の packetizer。MTU を超える AU はサンプル境界で分け、timestamp をずらす。
pub struct G711Packetizer {
    codec: Codec,
    mtu: usize,
}

impl G711Packetizer {
    pub fn new(codec: Codec, mtu: usize) -> Self {
        G711Packetizer {
            codec,
            mtu: mtu.max(16),
        }
    }
}

impl Packetizer for G711Packetizer {
    fn codec(&self) -> Codec {
        self.codec
    }

    fn clock_rate(&self) -> u32 {
        8000
    }

    fn packetize(
        &mut self,
        au: &AccessUnit,
        _config: Option<&CodecConfig>,
        out: &mut Vec<Payload>,
    ) -> Result<()> {
        if au.payload.is_empty() {
            return Err(Error::Invalid("空の G.711"));
        }
        let mut off = 0usize;
        while off < au.payload.len() {
            let end = (off + self.mtu).min(au.payload.len());
            out.push(Payload {
                data: au.payload.slice(off..end),
                marker: false,
                timestamp_offset: off as u32,
            });
            off = end;
        }
        Ok(())
    }
}

/// SDP に書く payload format（`PCMA/8000`・`PCMU/8000`）。
pub fn payload_format(codec: Codec) -> PayloadFormat {
    PayloadFormat {
        encoding: if codec == Codec::Pcma { "PCMA" } else { "PCMU" }.into(),
        clock_rate: 8000,
        channels: Some(1),
        fmtp: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::testutil::run;

    #[test]
    fn split_and_join() {
        let mut p = G711Packetizer::new(Codec::Pcma, 100);
        let mut au = crate::payload::h264::tests::au(Bytes::from(vec![0xD5u8; 250]), true, 0);
        au.codec = Codec::Pcma;
        let mut out = Vec::new();
        p.packetize(&au, None, &mut out).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].timestamp_offset, 200);
        let mut d = G711Depacketizer::new(Codec::Pcma, 8000);
        let mut seq = 0;
        let f = run(&mut d, &out, 1000, &mut seq);
        assert_eq!(f.len(), 3);
        assert_eq!(f[1].timestamp, 1100);
        assert_eq!(f[2].duration, 50);
        assert!(f[0].config.is_some());
    }
}
