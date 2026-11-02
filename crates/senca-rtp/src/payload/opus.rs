//! Opus（RFC 7587）。1 パケット = Opus パケット 1 つ、RTP の時計は常に 48 kHz。
//!
//! 設定（OpusHead）のチャンネル数は fmtp の sprop-stereo、無ければ最初のパケットの TOC の stereo ビットから決める。

use super::{
    ConfigState, Depacketizer, Frame, FrameQueue, Packetizer, Payload, PayloadFormat, SeqWatch,
};
use crate::error::{Error, Result};
use crate::packet::RtpPacket;
use senca_core::media::{AccessUnit, Codec, CodecConfig};
use senca_opus::{OpusHead, Toc};

/// Opus の depacketizer。
#[derive(Default)]
pub struct OpusDepacketizer {
    seq: SeqWatch,
    channels: Option<u8>,
    discontinuity: bool,
    cfg: ConfigState,
    out: FrameQueue,
}

impl OpusDepacketizer {
    /// `channels` は分かっていれば 1 か 2（sprop-stereo）。
    pub fn new(channels: Option<u8>) -> Self {
        let mut d = OpusDepacketizer {
            channels,
            ..Default::default()
        };
        if let Some(c) = channels {
            d.set_channels(c);
        }
        d
    }

    /// fmtp の sprop-stereo から作る。
    pub fn from_format(fmt: &PayloadFormat) -> Result<Self> {
        if fmt.clock_rate != 48000 {
            return Err(Error::Invalid("Opus の RTP の時計は 48000"));
        }
        let ch = match fmt.fmtp("sprop-stereo").map(str::trim) {
            Some("1") => Some(2),
            Some("0") => Some(1),
            _ => None,
        };
        Ok(Self::new(ch))
    }

    fn set_channels(&mut self, c: u8) {
        self.channels = Some(c);
        if let Ok(cfg) = OpusHead::new(c, 0, 48000).and_then(|h| h.codec_config(0)) {
            self.cfg.update(cfg);
        }
    }
}

impl Depacketizer for OpusDepacketizer {
    fn codec(&self) -> Codec {
        Codec::Opus
    }

    fn clock_rate(&self) -> u32 {
        48000
    }

    fn push(&mut self, pkt: &RtpPacket) -> Result<()> {
        if self.seq.gap(pkt.header.sequence) {
            self.discontinuity = true;
        }
        let samples = match senca_opus::packet_samples(&pkt.payload) {
            Ok(s) => s,
            Err(_) => {
                self.discontinuity = true;
                return Err(Error::Invalid("Opus パケット"));
            }
        };
        if self.channels.is_none() {
            self.set_channels(if Toc::from_byte(pkt.payload[0]).stereo {
                2
            } else {
                1
            });
        }
        self.out.push(Frame {
            timestamp: pkt.header.timestamp,
            payload: pkt.payload.clone(),
            keyframe: true,
            discontinuity: std::mem::take(&mut self.discontinuity),
            config: self.cfg.take_announce(),
            config_generation: self.cfg.generation,
            duration: samples,
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

/// Opus の packetizer（AU をそのまま 1 パケットに）。
#[derive(Default)]
pub struct OpusPacketizer;

impl OpusPacketizer {
    pub fn new() -> Self {
        OpusPacketizer
    }
}

impl Packetizer for OpusPacketizer {
    fn codec(&self) -> Codec {
        Codec::Opus
    }

    fn clock_rate(&self) -> u32 {
        48000
    }

    fn packetize(
        &mut self,
        au: &AccessUnit,
        _config: Option<&CodecConfig>,
        out: &mut Vec<Payload>,
    ) -> Result<()> {
        senca_opus::parse_packet(&au.payload).map_err(|_| Error::Invalid("Opus パケット"))?;
        out.push(Payload::new(au.payload.clone(), false));
        Ok(())
    }
}

/// SDP に書く payload format（`opus/48000/2`、ステレオなら sprop-stereo=1）。
pub fn payload_format(config: Option<&CodecConfig>) -> PayloadFormat {
    let stereo = config
        .and_then(|c| c.audio)
        .is_some_and(|a| a.channels >= 2);
    let mut fmtp = vec![
        ("minptime".to_string(), "10".to_string()),
        ("useinbandfec".into(), "1".into()),
    ];
    if stereo {
        fmtp.push(("stereo".into(), "1".into()));
        fmtp.push(("sprop-stereo".into(), "1".into()));
    }
    PayloadFormat {
        encoding: "opus".into(),
        clock_rate: 48000,
        channels: Some(2),
        fmtp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::testutil::{fuzz, run};
    use bytes::Bytes;

    #[test]
    fn roundtrip_and_channels() {
        let fmt = PayloadFormat {
            encoding: "opus".into(),
            clock_rate: 48000,
            channels: Some(2),
            fmtp: vec![],
        };
        let mut d = OpusDepacketizer::from_format(&fmt).unwrap();
        assert!(d.config().is_none());
        // config 31（CELT FB 20 ms）、stereo、code 0
        let pkt = Bytes::from_static(&[0xFC, 1, 2, 3]);
        let mut seq = 0;
        let f = run(&mut d, &[Payload::new(pkt.clone(), false)], 960, &mut seq);
        assert_eq!(f[0].duration, 960);
        assert_eq!(f[0].payload, pkt);
        let head = OpusHead::parse(&f[0].config.as_ref().unwrap().record).unwrap();
        assert_eq!(head.channels, 2);
        let mut p = OpusPacketizer::new();
        let mut out = Vec::new();
        let mut au = crate::payload::h264::tests::au(pkt, true, 0);
        au.codec = Codec::Opus;
        p.packetize(&au, None, &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert!(
            d.push(&RtpPacket::new(111, 9, 0, 1, false, Bytes::new()))
                .is_err()
        );
        let f = run(&mut d, &out, 1920, &mut seq);
        assert!(f[0].discontinuity);
        let bad = PayloadFormat {
            clock_rate: 8000,
            ..fmt
        };
        assert!(OpusDepacketizer::from_format(&bad).is_err());
    }

    #[test]
    fn fuzz_opus() {
        fuzz(
            &mut OpusDepacketizer::new(Some(2)),
            &[Payload::new(
                Bytes::from_static(&[0xFF, 0x03, 1, 2, 3, 4]),
                false,
            )],
            10000,
        );
    }
}
