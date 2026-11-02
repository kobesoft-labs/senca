//! AAC。
//!
//! - mpeg4-generic（RFC 3640）: AU-headers（AU-size と AU-Index/AU-Index-delta）付き。AAC-hbr・AAC-lbr を受け、AAC-hbr で送る。
//!   1 パケットに複数の AU、1 つの AU を複数のパケットに分けたもの（fragment）の両方を扱う。インターリーブは未対応。
//! - MP4A-LATM（RFC 6416）: 受信のみ。cpresent=0（StreamMuxConfig を fmtp の config で渡す）の形。
//!
//! 内部表現は raw_data_block（ADTS なし）、設定は AudioSpecificConfig。

use super::{
    ConfigState, Depacketizer, Frame, FrameQueue, Packetizer, Payload, PayloadFormat, SeqWatch,
};
use crate::error::{Error, Result};
use crate::packet::RtpPacket;
use bytes::Bytes;
use senca_aac::AudioSpecificConfig;
use senca_core::bits::BitReader;
use senca_core::media::{AccessUnit, Codec, CodecConfig};

/// 1 つの AU の上限（13 ビットの AU-size より大きい値も、16 ビットの sizeLength なら来うる）。
const MAX_AU: usize = 1 << 16;
/// 1 パケットの AU-header の数の上限。
const MAX_HEADERS: usize = 512;

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) || s.len() > 2048 {
        return Err(Error::Invalid("config の 16 進"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| Error::Invalid("config の 16 進"))
        })
        .collect()
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn num(fmt: &PayloadFormat, key: &str) -> Result<u32> {
    match fmt.fmtp(key) {
        None => Ok(0),
        Some(v) => v
            .trim()
            .parse::<u32>()
            .map_err(|_| Error::Invalid("fmtp の数値")),
    }
}

/// mpeg4-generic の depacketizer。
pub struct AacDepacketizer {
    size_len: u32,
    index_len: u32,
    index_delta_len: u32,
    samples_per_frame: u32,
    seq: SeqWatch,
    frag: Option<(u32, usize, Vec<u8>)>,
    discontinuity: bool,
    cfg: ConfigState,
    out: FrameQueue,
}

impl AacDepacketizer {
    /// `asc` は AudioSpecificConfig、`size_len` などは fmtp の sizeLength・indexLength・indexDeltaLength。
    pub fn new(asc: &[u8], size_len: u32, index_len: u32, index_delta_len: u32) -> Result<Self> {
        if !(1..=16).contains(&size_len) || index_len > 8 || index_delta_len > 8 {
            return Err(Error::Invalid("sizeLength・indexLength"));
        }
        let parsed =
            AudioSpecificConfig::parse(asc).map_err(|_| Error::Invalid("AudioSpecificConfig"))?;
        let mut cfg = ConfigState::default();
        cfg.update(
            senca_aac::codec_config(asc, 0).map_err(|_| Error::Invalid("AudioSpecificConfig"))?,
        );
        Ok(AacDepacketizer {
            size_len,
            index_len,
            index_delta_len,
            samples_per_frame: parsed.samples_per_frame(),
            seq: SeqWatch::default(),
            frag: None,
            discontinuity: false,
            cfg,
            out: FrameQueue::default(),
        })
    }

    /// SDP の fmtp から作る（mode・sizeLength・indexLength・indexDeltaLength・config）。
    pub fn from_format(fmt: &PayloadFormat) -> Result<Self> {
        if let Some(m) = fmt.fmtp("mode")
            && !m.trim().eq_ignore_ascii_case("AAC-hbr")
            && !m.trim().eq_ignore_ascii_case("AAC-lbr")
        {
            return Err(Error::Unsupported("mpeg4-generic の mode"));
        }
        for k in [
            "ctsdeltalength",
            "dtsdeltalength",
            "randomaccessindication",
            "streamstateindication",
            "auxiliarydatasizelength",
        ] {
            if num(fmt, k)? != 0 {
                return Err(Error::Unsupported("mpeg4-generic の追加の AU-header 項目"));
            }
        }
        let asc = hex_decode(fmt.fmtp("config").ok_or(Error::Invalid("config がない"))?)?;
        Self::new(
            &asc,
            num(fmt, "sizelength")?,
            num(fmt, "indexlength")?,
            num(fmt, "indexdeltalength")?,
        )
    }

    fn emit(&mut self, ts: u32, payload: Bytes) {
        self.out.push(Frame {
            timestamp: ts,
            payload,
            keyframe: true,
            discontinuity: std::mem::take(&mut self.discontinuity),
            config: self.cfg.take_announce(),
            config_generation: self.cfg.generation,
            duration: self.samples_per_frame,
        });
    }

    fn parse(&mut self, pkt: &RtpPacket) -> Result<()> {
        let p = &pkt.payload;
        if p.len() < 2 {
            return Err(Error::Truncated);
        }
        let bits = u16::from_be_bytes([p[0], p[1]]) as usize;
        let hbytes = bits.div_ceil(8);
        let data_start = 2 + hbytes;
        if data_start > p.len() {
            return Err(Error::Truncated);
        }
        let mut r = BitReader::new(&p[2..data_start]);
        let mut sizes = Vec::new();
        let mut used = 0usize;
        while used < bits {
            let il = if sizes.is_empty() {
                self.index_len
            } else {
                self.index_delta_len
            };
            if used + (self.size_len + il) as usize > bits {
                return Err(Error::Invalid("AU-header の長さ"));
            }
            let size = r.read_bits(self.size_len).ok_or(Error::Truncated)? as usize;
            let idx = if il > 0 {
                r.read_bits(il).ok_or(Error::Truncated)?
            } else {
                0
            };
            if idx != 0 {
                return Err(Error::Unsupported("インターリーブ（AU-Index ≠ 0）"));
            }
            used += (self.size_len + il) as usize;
            if sizes.len() >= MAX_HEADERS {
                return Err(Error::Limit("AU-header の数"));
            }
            sizes.push(size);
        }
        if sizes.is_empty() {
            return Err(Error::Invalid("AU-header がない"));
        }
        let data = pkt.payload.slice(data_start..);
        let ts = pkt.header.timestamp;
        // fragment（AU 1 つが payload より大きい、または続きを待っている）
        if sizes.len() == 1 && (sizes[0] > data.len() || self.frag.is_some()) {
            let size = sizes[0];
            if size > MAX_AU {
                return Err(Error::Limit("AU の大きさ"));
            }
            let mut f = match self.frag.take() {
                Some((t, s, buf)) if t == ts && s == size => (t, s, buf),
                Some(_) => {
                    self.discontinuity = true;
                    (ts, size, Vec::with_capacity(size))
                }
                None => (ts, size, Vec::with_capacity(size)),
            };
            if f.2.len() + data.len() > size {
                self.discontinuity = true;
                return Err(Error::Invalid("fragment の合計が AU-size を超える"));
            }
            f.2.extend_from_slice(&data);
            if f.2.len() == size {
                // 最後の fragment には marker が立つはずだが、無くても受ける
                self.emit(ts, Bytes::from(f.2));
            } else if pkt.header.marker {
                self.discontinuity = true;
                return Err(Error::Invalid("fragment が足りないまま marker"));
            } else {
                self.frag = Some(f);
            }
            return Ok(());
        }
        let total: usize = sizes.iter().sum();
        if total > data.len() {
            return Err(Error::Truncated);
        }
        let mut off = 0usize;
        for (i, s) in sizes.iter().enumerate() {
            let t = ts.wrapping_add(self.samples_per_frame.wrapping_mul(i as u32));
            self.emit(t, data.slice(off..off + s));
            off += s;
        }
        Ok(())
    }
}

impl Depacketizer for AacDepacketizer {
    fn codec(&self) -> Codec {
        Codec::Aac
    }

    fn clock_rate(&self) -> u32 {
        self.cfg
            .current
            .as_ref()
            .and_then(|c| c.audio)
            .map_or(48000, |a| a.sample_rate)
    }

    fn push(&mut self, pkt: &RtpPacket) -> Result<()> {
        if self.seq.gap(pkt.header.sequence) {
            self.discontinuity = true;
            self.frag = None;
        }
        let r = self.parse(pkt);
        if r.is_err() {
            self.frag = None;
            self.discontinuity = true;
        }
        r
    }

    fn pop(&mut self) -> Option<Frame> {
        self.out.pop()
    }

    fn mark_loss(&mut self) {
        self.frag = None;
        self.discontinuity = true;
    }

    fn config(&self) -> Option<&CodecConfig> {
        self.cfg.current.as_ref()
    }
}

/// mpeg4-generic（AAC-hbr）の packetizer。AU 1 つを 1 パケット、大きければ fragment に分ける。
pub struct AacPacketizer {
    mtu: usize,
    clock_rate: u32,
}

impl AacPacketizer {
    pub fn new(mtu: usize, config: &CodecConfig) -> Result<Self> {
        let asc = AudioSpecificConfig::parse(&config.record)
            .map_err(|_| Error::Invalid("AudioSpecificConfig"))?;
        Ok(AacPacketizer {
            mtu: mtu.max(16),
            clock_rate: asc.sampling_frequency,
        })
    }
}

impl Packetizer for AacPacketizer {
    fn codec(&self) -> Codec {
        Codec::Aac
    }

    fn clock_rate(&self) -> u32 {
        self.clock_rate
    }

    fn packetize(
        &mut self,
        au: &AccessUnit,
        _config: Option<&CodecConfig>,
        out: &mut Vec<Payload>,
    ) -> Result<()> {
        let len = au.payload.len();
        if len == 0 || len > 8191 {
            return Err(Error::Invalid("AAC の AU の大きさ（1〜8191）"));
        }
        let hdr = [0x00u8, 0x10, (len >> 5) as u8, ((len & 0x1f) << 3) as u8];
        let chunk = self.mtu - 4;
        let n = len.div_ceil(chunk);
        for (i, part) in au.payload.chunks(chunk).enumerate() {
            let mut v = Vec::with_capacity(4 + part.len());
            v.extend_from_slice(&hdr);
            v.extend_from_slice(part);
            out.push(Payload::new(Bytes::from(v), i + 1 == n));
        }
        Ok(())
    }
}

/// SDP に書く payload format（`mpeg4-generic/<rate>/<ch>`、AAC-hbr）。
pub fn payload_format(config: &CodecConfig) -> Result<PayloadFormat> {
    let asc = AudioSpecificConfig::parse(&config.record)
        .map_err(|_| Error::Invalid("AudioSpecificConfig"))?;
    Ok(PayloadFormat {
        encoding: "mpeg4-generic".into(),
        clock_rate: asc.sampling_frequency,
        channels: Some(asc.channels.max(1) as u16),
        fmtp: vec![
            ("profile-level-id".into(), "1".into()),
            ("mode".into(), "AAC-hbr".into()),
            ("sizelength".into(), "13".into()),
            ("indexlength".into(), "3".into()),
            ("indexdeltalength".into(), "3".into()),
            ("config".into(), hex_encode(&config.record)),
        ],
    })
}

/// StreamMuxConfig（ISO/IEC 14496-3 1.7.3）の必要な部分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamMuxConfig {
    pub num_sub_frames: u8,
    /// AudioSpecificConfig（バイト境界に揃えたもの）。
    pub asc: Vec<u8>,
}

fn latm_get_value(r: &mut BitReader) -> Option<u64> {
    let n = r.read_bits(2)? as u32 + 1;
    r.read_bits(8 * n)
}

impl StreamMuxConfig {
    /// 1 プログラム・1 レイヤーのものだけ受け付ける。
    pub fn parse(data: &[u8]) -> Result<StreamMuxConfig> {
        let mut r = BitReader::new(data);
        let t = Error::Truncated;
        let version = r.read_bit().ok_or(t)?;
        if version {
            if r.read_bit().ok_or(t)? {
                return Err(Error::Unsupported("audioMuxVersionA = 1"));
            }
            latm_get_value(&mut r).ok_or(t)?; // taraBufferFullness
        }
        if !r.read_bit().ok_or(t)? {
            return Err(Error::Unsupported("allStreamsSameTimeFraming = 0"));
        }
        let num_sub_frames = r.read_bits(6).ok_or(t)? as u8;
        if r.read_bits(4).ok_or(t)? != 0 || r.read_bits(3).ok_or(t)? != 0 {
            return Err(Error::Unsupported("複数のプログラム・レイヤー"));
        }
        if version {
            latm_get_value(&mut r).ok_or(t)?; // ascLen
        }
        // ASC はバイト境界に揃っていないので、ずらして取り出す
        let mut asc = Vec::new();
        while r.bits_left() >= 8 {
            asc.push(r.read_bits(8).ok_or(t)? as u8);
        }
        let left = r.bits_left() as u32;
        if left > 0 {
            asc.push((r.read_bits(left).ok_or(t)? as u8) << (8 - left));
        }
        let parsed =
            AudioSpecificConfig::parse(&asc).map_err(|_| Error::Invalid("AudioSpecificConfig"))?;
        let asc = parsed
            .to_bytes()
            .map_err(|_| Error::Invalid("AudioSpecificConfig"))?;
        Ok(StreamMuxConfig {
            num_sub_frames,
            asc,
        })
    }
}

/// MP4A-LATM（RFC 6416、cpresent=0）の depacketizer。
pub struct LatmDepacketizer {
    num_sub_frames: u8,
    samples_per_frame: u32,
    clock_rate: u32,
    seq: SeqWatch,
    buf: Option<(u32, Vec<u8>)>,
    discontinuity: bool,
    cfg: ConfigState,
    out: FrameQueue,
}

impl LatmDepacketizer {
    pub fn new(smc: &StreamMuxConfig, clock_rate: u32) -> Result<Self> {
        let parsed = AudioSpecificConfig::parse(&smc.asc)
            .map_err(|_| Error::Invalid("AudioSpecificConfig"))?;
        let mut cfg = ConfigState::default();
        cfg.update(
            senca_aac::codec_config(&smc.asc, 0)
                .map_err(|_| Error::Invalid("AudioSpecificConfig"))?,
        );
        Ok(LatmDepacketizer {
            num_sub_frames: smc.num_sub_frames,
            samples_per_frame: parsed.samples_per_frame(),
            clock_rate,
            seq: SeqWatch::default(),
            buf: None,
            discontinuity: false,
            cfg,
            out: FrameQueue::default(),
        })
    }

    /// SDP の fmtp（cpresent=0・config）から作る。
    pub fn from_format(fmt: &PayloadFormat) -> Result<Self> {
        if fmt.fmtp("cpresent").map(str::trim) != Some("0") {
            return Err(Error::Unsupported(
                "MP4A-LATM の cpresent=1（帯域内の StreamMuxConfig）",
            ));
        }
        let smc = StreamMuxConfig::parse(&hex_decode(
            fmt.fmtp("config").ok_or(Error::Invalid("config がない"))?,
        )?)?;
        Self::new(&smc, fmt.clock_rate)
    }

    fn finish(&mut self) -> Result<()> {
        let Some((ts, data)) = self.buf.take() else {
            return Ok(());
        };
        let data = Bytes::from(data);
        let mut pos = 0usize;
        let mut frames = Vec::new();
        for _ in 0..=self.num_sub_frames {
            let mut len = 0usize;
            loop {
                let b = *data.get(pos).ok_or(Error::Truncated)?;
                pos += 1;
                len += b as usize;
                if b != 255 {
                    break;
                }
            }
            let end = pos + len;
            if end > data.len() {
                return Err(Error::Truncated);
            }
            frames.push(data.slice(pos..end));
            pos = end;
        }
        for (i, f) in frames.into_iter().enumerate() {
            self.out.push(Frame {
                timestamp: ts.wrapping_add(self.samples_per_frame.wrapping_mul(i as u32)),
                payload: f,
                keyframe: true,
                discontinuity: std::mem::take(&mut self.discontinuity),
                config: self.cfg.take_announce(),
                config_generation: self.cfg.generation,
                duration: self.samples_per_frame,
            });
        }
        Ok(())
    }
}

impl Depacketizer for LatmDepacketizer {
    fn codec(&self) -> Codec {
        Codec::Aac
    }

    fn clock_rate(&self) -> u32 {
        self.clock_rate
    }

    fn push(&mut self, pkt: &RtpPacket) -> Result<()> {
        if self.seq.gap(pkt.header.sequence) {
            self.discontinuity = true;
            self.buf = None;
        }
        let ts = pkt.header.timestamp;
        if self.buf.as_ref().is_some_and(|(t, _)| *t != ts) {
            self.buf = None;
            self.discontinuity = true;
        }
        let (_, buf) = self.buf.get_or_insert_with(|| (ts, Vec::new()));
        if buf.len() + pkt.payload.len() > MAX_AU * 4 {
            self.buf = None;
            self.discontinuity = true;
            return Err(Error::Limit("AudioMuxElement の大きさ"));
        }
        buf.extend_from_slice(&pkt.payload);
        if pkt.header.marker {
            let r = self.finish();
            if r.is_err() {
                self.discontinuity = true;
            }
            return r;
        }
        Ok(())
    }

    fn pop(&mut self) -> Option<Frame> {
        self.out.pop()
    }

    fn mark_loss(&mut self) {
        self.buf = None;
        self.discontinuity = true;
    }

    fn config(&self) -> Option<&CodecConfig> {
        self.cfg.current.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::testutil::{fuzz, run};

    fn cfg() -> CodecConfig {
        senca_aac::codec_config(&[0x11, 0x90], 0).unwrap()
    }

    fn au(n: usize) -> AccessUnit {
        let mut a = crate::payload::h264::tests::au(
            Bytes::from((0..n).map(|i| i as u8).collect::<Vec<_>>()),
            true,
            0,
        );
        a.codec = Codec::Aac;
        a
    }

    #[test]
    fn rfc3640_hbr_roundtrip_and_fragment() {
        let fmt = payload_format(&cfg()).unwrap();
        assert_eq!(fmt.clock_rate, 48000);
        assert_eq!(fmt.fmtp("config"), Some("1190"));
        let mut d = AacDepacketizer::from_format(&fmt).unwrap();
        let mut p = AacPacketizer::new(200, &cfg()).unwrap();
        let mut seq = 0;
        let mut out = Vec::new();
        p.packetize(&au(150), None, &mut out).unwrap();
        assert_eq!(out.len(), 1);
        let f = run(&mut d, &out, 0, &mut seq);
        assert_eq!(f[0].payload, au(150).payload);
        assert_eq!(f[0].duration, 1024);
        assert!(f[0].config.is_some());
        let mut out = Vec::new();
        p.packetize(&au(700), None, &mut out).unwrap();
        assert_eq!(out.len(), 4);
        let f = run(&mut d, &out, 1024, &mut seq);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].payload, au(700).payload);
        // 途中の fragment を落とす → 捨てて discontinuity
        let mut out = Vec::new();
        p.packetize(&au(700), None, &mut out).unwrap();
        assert!(run(&mut d, &out[..1], 2048, &mut seq).is_empty());
        seq += 1;
        let _ = d.push(&RtpPacket::new(
            96,
            seq,
            2048,
            1,
            false,
            out[2].data.clone(),
        ));
        seq += 1;
        let _ = d.push(&RtpPacket::new(96, seq, 2048, 1, true, out[3].data.clone()));
        seq += 1;
        assert!(d.pop().is_none());
        let mut out = Vec::new();
        p.packetize(&au(10), None, &mut out).unwrap();
        let f = run(&mut d, &out, 4096, &mut seq);
        assert!(f[0].discontinuity);
    }

    #[test]
    fn multiple_aus_per_packet() {
        // RFC 3640 3.3.6 の形: AU-headers-length 32 ビット、2 つの AU（3 バイトと 2 バイト）
        let mut d = AacDepacketizer::new(&[0x11, 0x90], 13, 3, 3).unwrap();
        let pkt = [0x00, 0x20, 0x00, 0x18, 0x00, 0x10, 1, 2, 3, 4, 5];
        d.push(&RtpPacket::new(
            96,
            0,
            100,
            1,
            true,
            Bytes::copy_from_slice(&pkt),
        ))
        .unwrap();
        let a = d.pop().unwrap();
        let b = d.pop().unwrap();
        assert_eq!(&a.payload[..], &[1, 2, 3]);
        assert_eq!(&b.payload[..], &[4, 5]);
        assert_eq!(b.timestamp, 100 + 1024);
        // インターリーブは未対応
        let bad = [0x00, 0x10, 0x00, 0x19, 1, 2, 3];
        assert!(
            d.push(&RtpPacket::new(
                96,
                1,
                0,
                1,
                true,
                Bytes::copy_from_slice(&bad)
            ))
            .is_err()
        );
        let mut p = AacPacketizer::new(100, &cfg()).unwrap();
        let mut seeds = Vec::new();
        p.packetize(&au(300), None, &mut seeds).unwrap();
        seeds.push(Payload::new(Bytes::copy_from_slice(&pkt), true));
        fuzz(&mut d, &seeds, 20000);
    }

    #[test]
    fn latm() {
        // FFmpeg の rtpenc_latm が出す config（AAC-LC 48 kHz ステレオ）: 400023203fc0
        let smc = StreamMuxConfig::parse(&hex_decode("400023203fc0").unwrap()).unwrap();
        assert_eq!(smc.num_sub_frames, 0);
        assert_eq!(smc.asc, vec![0x11, 0x90]);
        let fmt = PayloadFormat {
            encoding: "MP4A-LATM".into(),
            clock_rate: 48000,
            channels: Some(2),
            fmtp: vec![
                ("cpresent".into(), "0".into()),
                ("config".into(), "400023203fc0".into()),
            ],
        };
        let mut d = LatmDepacketizer::from_format(&fmt).unwrap();
        let mut data = vec![255u8, 45];
        data.extend((0..300).map(|i| i as u8));
        let mut seq = 0;
        let f = run(
            &mut d,
            &[
                Payload::new(Bytes::copy_from_slice(&data[..100]), false),
                Payload::new(Bytes::copy_from_slice(&data[100..]), true),
            ],
            0,
            &mut seq,
        );
        assert_eq!(f.len(), 1);
        assert_eq!(&f[0].payload[..], &data[2..]);
        assert!(
            LatmDepacketizer::from_format(&PayloadFormat {
                fmtp: vec![],
                ..fmt.clone()
            })
            .is_err()
        );
        fuzz(&mut d, &[Payload::new(Bytes::from(data), true)], 10000);
    }
}
