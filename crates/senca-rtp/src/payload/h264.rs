//! H.264（RFC 6184）。packetization-mode 0（single NAL）と 1（non-interleaved: single・STAP-A・FU-A）。
//!
//! 受信: SPS・PPS は AU から抜いて設定（AVCDecoderConfigurationRecord）にする。SDP の sprop-parameter-sets も使う。
//! AUD・filler は捨てる。送信: キーフレームで SPS・PPS が AU に無ければ設定から先頭に足す。

use super::h26x::{Flavor, NalAssembler, NalClass, ParamKind, au_nals, packetize_nals};
use super::{Depacketizer, Frame, Packetizer, Payload, PayloadFormat};
use crate::error::{Error, Result};
use crate::packet::RtpPacket;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use senca_core::media::{AccessUnit, Codec, CodecConfig};
use senca_h264::{AvcDecoderConfigurationRecord, NalUnitType, Pps, Sps};

pub(crate) struct H264;

impl Flavor for H264 {
    const HDR: usize = 1;

    fn classify(nal: &[u8]) -> NalClass {
        let Some(&b) = nal.first() else {
            return NalClass::Skip;
        };
        match NalUnitType::from_u8(b & 0x1f) {
            NalUnitType::Sps => NalClass::Param(
                ParamKind::Sps,
                Sps::parse(nal).map_or(0, |s| s.seq_parameter_set_id),
            ),
            NalUnitType::Pps => NalClass::Param(
                ParamKind::Pps,
                Pps::parse(nal).map_or(0, |p| p.pic_parameter_set_id),
            ),
            NalUnitType::Aud | NalUnitType::FillerData => NalClass::Skip,
            NalUnitType::IdrSlice => NalClass::Vcl { key: true },
            t if t.is_vcl() => NalClass::Vcl { key: false },
            _ => NalClass::Other,
        }
    }

    fn build_config(
        _vps: &[Bytes],
        sps: &[Bytes],
        pps: &[Bytes],
        generation: u32,
    ) -> Result<CodecConfig> {
        let s: Vec<&[u8]> = sps.iter().map(|b| &b[..]).collect();
        let p: Vec<&[u8]> = pps.iter().map(|b| &b[..]).collect();
        AvcDecoderConfigurationRecord::from_parameter_sets(&s, &p)
            .and_then(|r| r.codec_config(generation))
            .map_err(|_| Error::Invalid("SPS・PPS"))
    }

    fn from_config(cfg: &CodecConfig) -> Result<(Vec<Bytes>, u8)> {
        let r = AvcDecoderConfigurationRecord::parse(&cfg.record)
            .map_err(|_| Error::Invalid("AVCDecoderConfigurationRecord"))?;
        let mut v = r.sps.clone();
        v.extend(r.pps.iter().cloned());
        Ok((v, r.length_size))
    }

    fn agg_header(nals: &[Bytes]) -> Vec<u8> {
        let f = nals.iter().fold(0u8, |a, n| a | (n[0] & 0x80));
        let nri = nals.iter().map(|n| n[0] & 0x60).max().unwrap_or(0);
        vec![f | nri | 24]
    }

    fn fu_header(nal: &[u8], start: bool, end: bool) -> Vec<u8> {
        vec![
            (nal[0] & 0xE0) | 28,
            (start as u8) << 7 | (end as u8) << 6 | (nal[0] & 0x1f),
        ]
    }
}

/// packetization-mode。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketizationMode {
    /// 0: single NAL unit mode。
    SingleNal,
    /// 1: non-interleaved mode（STAP-A・FU-A を使う）。
    NonInterleaved,
}

/// sprop-parameter-sets（base64 をカンマで並べたもの）を NAL に戻す。
pub fn parse_sprop_parameter_sets(s: &str) -> Result<Vec<Bytes>> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if out.len() >= 64 {
            return Err(Error::Limit("sprop-parameter-sets の数"));
        }
        let v = STANDARD
            .decode(part)
            .map_err(|_| Error::Invalid("sprop-parameter-sets の base64"))?;
        if v.is_empty() {
            return Err(Error::Invalid("空のパラメーターセット"));
        }
        out.push(Bytes::from(v));
    }
    Ok(out)
}

/// H.264 の depacketizer。
pub struct H264Depacketizer {
    asm: NalAssembler<H264>,
    mode: PacketizationMode,
}

impl Default for H264Depacketizer {
    fn default() -> Self {
        Self::new(PacketizationMode::NonInterleaved)
    }
}

impl H264Depacketizer {
    pub fn new(mode: PacketizationMode) -> Self {
        H264Depacketizer {
            asm: NalAssembler::new(),
            mode,
        }
    }

    /// SDP の fmtp（packetization-mode・sprop-parameter-sets）から作る。
    pub fn from_format(fmt: &PayloadFormat) -> Result<Self> {
        let mode = match fmt.fmtp("packetization-mode").unwrap_or("0").trim() {
            "0" => PacketizationMode::SingleNal,
            "1" => PacketizationMode::NonInterleaved,
            _ => return Err(Error::Unsupported("packetization-mode 2（interleaved）")),
        };
        let mut d = Self::new(mode);
        if let Some(s) = fmt.fmtp("sprop-parameter-sets") {
            // 壊れた sprop は無視して帯域内のものを待つ
            if let Ok(nals) = parse_sprop_parameter_sets(s) {
                d.set_parameter_sets(&nals);
            }
        }
        Ok(d)
    }

    /// 帯域外のパラメーターセット（SPS・PPS の NAL）を入れる。
    pub fn set_parameter_sets(&mut self, nals: &[Bytes]) {
        self.asm.add_out_of_band(nals);
    }

    fn parse(&mut self, p: &Bytes) -> Result<()> {
        let b0 = *p.first().ok_or(Error::Truncated)?;
        let ty = b0 & 0x1f;
        match ty {
            1..=23 => self.asm.add_nal(p.clone()),
            24 => {
                if self.mode == PacketizationMode::SingleNal {
                    return Err(Error::Invalid("packetization-mode 0 での STAP-A"));
                }
                let mut i = 1usize;
                let mut n = 0;
                while i < p.len() {
                    let len = u16::from_be_bytes([
                        *p.get(i).ok_or(Error::Truncated)?,
                        *p.get(i + 1).ok_or(Error::Truncated)?,
                    ]) as usize;
                    i += 2;
                    if len == 0 || i + len > p.len() {
                        return Err(Error::Invalid("STAP-A の NAL 長"));
                    }
                    self.asm.add_nal(p.slice(i..i + len))?;
                    i += len;
                    n += 1;
                }
                if n == 0 {
                    return Err(Error::Invalid("空の STAP-A"));
                }
                Ok(())
            }
            28 => {
                if self.mode == PacketizationMode::SingleNal {
                    return Err(Error::Invalid("packetization-mode 0 での FU-A"));
                }
                let h = *p.get(1).ok_or(Error::Truncated)?;
                let (s, e) = (h & 0x80 != 0, h & 0x40 != 0);
                if s && e {
                    return Err(Error::Invalid("FU-A の S と E が両方立っている"));
                }
                let data = &p[2..];
                if s {
                    self.asm.fu_start(&[(b0 & 0xE0) | (h & 0x1f)], data)
                } else {
                    self.asm.fu_continue(data, e)
                }
            }
            25 | 26 | 27 | 29 => Err(Error::Unsupported("STAP-B・MTAP・FU-B（interleaved mode）")),
            _ => Err(Error::Invalid("NAL type 0・30・31")),
        }
    }
}

impl Depacketizer for H264Depacketizer {
    fn codec(&self) -> Codec {
        Codec::H264
    }

    fn clock_rate(&self) -> u32 {
        90000
    }

    fn push(&mut self, pkt: &RtpPacket) -> Result<()> {
        self.asm.begin(pkt);
        let r = self.parse(&pkt.payload);
        if r.is_err() {
            self.asm.set_broken();
        }
        self.asm.end(pkt.header.marker);
        r
    }

    fn pop(&mut self) -> Option<Frame> {
        self.asm.pop()
    }

    fn mark_loss(&mut self) {
        self.asm.loss();
    }

    fn config(&self) -> Option<&CodecConfig> {
        self.asm.cfg.current.as_ref()
    }
}

/// H.264 の packetizer。
pub struct H264Packetizer {
    mtu: usize,
    mode: PacketizationMode,
}

impl H264Packetizer {
    pub fn new(mtu: usize, mode: PacketizationMode) -> Self {
        H264Packetizer {
            mtu: mtu.max(16),
            mode,
        }
    }
}

impl Packetizer for H264Packetizer {
    fn codec(&self) -> Codec {
        Codec::H264
    }

    fn clock_rate(&self) -> u32 {
        90000
    }

    fn packetize(
        &mut self,
        au: &AccessUnit,
        config: Option<&CodecConfig>,
        out: &mut Vec<Payload>,
    ) -> Result<()> {
        let nals = au_nals::<H264>(&au.payload, au.keyframe, config, |b, l| {
            senca_h264::length_prefixed_nals_bytes(b, l).map_err(|_| ())
        })?;
        packetize_nals::<H264>(
            &nals,
            self.mtu,
            self.mode == PacketizationMode::NonInterleaved,
            out,
        )
    }
}

/// SDP に書く payload format（`H264/90000`、profile-level-id・packetization-mode・sprop-parameter-sets）。
pub fn payload_format(
    config: Option<&CodecConfig>,
    mode: PacketizationMode,
) -> Result<PayloadFormat> {
    let mut fmtp = vec![(
        "packetization-mode".to_string(),
        match mode {
            PacketizationMode::SingleNal => "0",
            PacketizationMode::NonInterleaved => "1",
        }
        .to_string(),
    )];
    if let Some(c) = config {
        let r = AvcDecoderConfigurationRecord::parse(&c.record)
            .map_err(|_| Error::Invalid("AVCDecoderConfigurationRecord"))?;
        fmtp.push((
            "profile-level-id".into(),
            format!(
                "{:02X}{:02X}{:02X}",
                r.profile_indication, r.profile_compatibility, r.level_indication
            ),
        ));
        let sets: Vec<String> = r
            .sps
            .iter()
            .chain(r.pps.iter())
            .map(|n| STANDARD.encode(n))
            .collect();
        fmtp.push(("sprop-parameter-sets".into(), sets.join(",")));
    }
    Ok(PayloadFormat {
        encoding: "H264".into(),
        clock_rate: 90000,
        channels: None,
        fmtp,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::payload::testutil::{Rng, fuzz, run};
    use senca_core::{Timebase, TrackId};

    /// 実際の x264 の SPS・PPS（Baseline 320x240）。
    pub(crate) const SPS: &[u8] = &[
        0x67, 0x42, 0xC0, 0x0D, 0xD9, 0x01, 0x41, 0xFB, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10,
        0x00, 0x00, 0x03, 0x03, 0xC0, 0xF1, 0x42, 0xA4, 0x80,
    ];
    pub(crate) const PPS: &[u8] = &[0x68, 0xCB, 0x83, 0xCB, 0x20];

    pub(crate) fn config() -> CodecConfig {
        senca_h264::codec_config(SPS, PPS, 0).unwrap()
    }

    pub(crate) fn avcc(nals: &[&[u8]]) -> Bytes {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&(n.len() as u32).to_be_bytes());
            v.extend_from_slice(n);
        }
        Bytes::from(v)
    }

    pub(crate) fn au(payload: Bytes, key: bool, pts: i64) -> AccessUnit {
        AccessUnit {
            track: TrackId(0),
            codec: Codec::H264,
            epoch: 0,
            sequence: 0,
            config_generation: 0,
            pts,
            dts: pts,
            duration: 0,
            timebase: Timebase::MPEG,
            keyframe: key,
            discontinuity: false,
            capture_time_ns: None,
            payload,
        }
    }

    fn idr(len: usize, seed: u32) -> Vec<u8> {
        let mut r = Rng(seed | 1);
        let mut v = vec![0x65];
        v.extend((0..len).map(|_| r.next() as u8 | 1));
        v
    }

    fn slice(len: usize, seed: u32) -> Vec<u8> {
        let mut v = idr(len, seed);
        v[0] = 0x41;
        v
    }

    #[test]
    fn roundtrip_mode1_inserts_params() {
        let cfg = config();
        let mut p = H264Packetizer::new(500, PacketizationMode::NonInterleaved);
        let mut d = H264Depacketizer::default();
        let sei = [0x06u8, 5, 1, 0xff, 0x80];
        let big = idr(3000, 3);
        let mut out = Vec::new();
        p.packetize(&au(avcc(&[&sei, &big]), true, 0), Some(&cfg), &mut out)
            .unwrap();
        // STAP-A（SPS+PPS+SEI）+ FU-A × 7
        assert_eq!(out[0].data[0] & 0x1f, 24);
        assert_eq!(out.iter().filter(|p| p.data[0] & 0x1f == 28).count(), 7);
        assert!(out.iter().all(|p| p.data.len() <= 500));
        assert!(out.last().unwrap().marker);
        assert_eq!(out.iter().filter(|p| p.marker).count(), 1);
        let mut seq = 65530;
        let frames = run(&mut d, &out, 1000, &mut seq);
        assert_eq!(frames.len(), 1);
        let f = &frames[0];
        assert!(f.keyframe && !f.discontinuity);
        assert_eq!(f.payload, avcc(&[&sei, &big]));
        assert_eq!(f.config.as_ref().unwrap().record, cfg.record);
        // 2 枚目: P スライス 3 つ（小さい）は STAP-A 1 つ
        let s1 = slice(100, 5);
        let s2 = slice(100, 6);
        let mut out = Vec::new();
        p.packetize(&au(avcc(&[&s1, &s2]), false, 3000), Some(&cfg), &mut out)
            .unwrap();
        assert_eq!(out.len(), 1);
        let frames = run(&mut d, &out, 4000, &mut seq);
        assert_eq!(frames[0].payload, avcc(&[&s1, &s2]));
        assert!(frames[0].config.is_none());
    }

    #[test]
    fn mode0_single_nal_only() {
        let cfg = config();
        let mut p = H264Packetizer::new(1200, PacketizationMode::SingleNal);
        let mut out = Vec::new();
        p.packetize(&au(avcc(&[&idr(100, 1)]), true, 0), Some(&cfg), &mut out)
            .unwrap();
        assert_eq!(out.len(), 3);
        assert!(
            p.packetize(&au(avcc(&[&idr(2000, 1)]), true, 0), Some(&cfg), &mut out)
                .is_err()
        );
        let fmt = PayloadFormat {
            encoding: "H264".into(),
            clock_rate: 90000,
            channels: None,
            fmtp: vec![],
        };
        let mut d = H264Depacketizer::from_format(&fmt).unwrap();
        let stap = Payload::new(Bytes::from_static(&[24, 0, 1, 0x09]), true);
        assert!(
            d.push(&RtpPacket::new(96, 0, 0, 1, true, stap.data))
                .is_err()
        );
    }

    #[test]
    fn sprop_parameter_sets_give_config_before_inband() {
        let fmt = payload_format(Some(&config()), PacketizationMode::NonInterleaved).unwrap();
        assert_eq!(fmt.fmtp("profile-level-id"), Some("42C00D"));
        let mut d = H264Depacketizer::from_format(&fmt).unwrap();
        assert_eq!(d.config().unwrap().record, config().record);
        // パラメーターセット無しの IDR だけでも出る
        let x = idr(50, 9);
        let mut seq = 0;
        let frames = run(
            &mut d,
            &[Payload::new(Bytes::from(x.clone()), true)],
            0,
            &mut seq,
        );
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].config_generation, 0);
        assert_eq!(
            parse_sprop_parameter_sets("Z0IAH5WoFAFuQA==,aM48gA==")
                .unwrap()
                .len(),
            2
        );
        assert!(parse_sprop_parameter_sets("!!").is_err());
    }

    #[test]
    fn missing_fragment_drops_au_and_waits_for_keyframe() {
        let cfg = config();
        let mut p = H264Packetizer::new(300, PacketizationMode::NonInterleaved);
        let mut d = H264Depacketizer::default();
        let mut seq = 100u16;
        // 1: IDR（そのまま）
        let mut out = Vec::new();
        p.packetize(&au(avcc(&[&idr(1000, 1)]), true, 0), Some(&cfg), &mut out)
            .unwrap();
        assert_eq!(run(&mut d, &out, 0, &mut seq).len(), 1);
        // 2: P、FU の途中を 1 つ落とす
        let mut out = Vec::new();
        p.packetize(
            &au(avcc(&[&slice(1000, 2)]), false, 3000),
            Some(&cfg),
            &mut out,
        )
        .unwrap();
        let mut lossy = out.clone();
        lossy.remove(2);
        let f = run(&mut d, &lossy[..2], 3000, &mut seq);
        seq = seq.wrapping_add(1);
        let f2 = run(&mut d, &lossy[2..], 3000, &mut seq);
        assert!(f.is_empty() && f2.is_empty());
        // 3: P（完全）でもキーフレームまで捨てる
        let mut out = Vec::new();
        p.packetize(
            &au(avcc(&[&slice(100, 3)]), false, 6000),
            Some(&cfg),
            &mut out,
        )
        .unwrap();
        assert!(run(&mut d, &out, 6000, &mut seq).is_empty());
        // 4: IDR で再開、discontinuity が立つ
        let mut out = Vec::new();
        p.packetize(&au(avcc(&[&idr(500, 4)]), true, 9000), Some(&cfg), &mut out)
            .unwrap();
        let f = run(&mut d, &out, 9000, &mut seq);
        assert_eq!(f.len(), 1);
        assert!(f[0].keyframe && f[0].discontinuity);
        // 5: 最後のパケット（marker）を落とし、次の AU の timestamp で区切られる
        let mut out = Vec::new();
        p.packetize(
            &au(avcc(&[&slice(1000, 5)]), false, 12000),
            Some(&cfg),
            &mut out,
        )
        .unwrap();
        out.pop();
        assert!(run(&mut d, &out, 12000, &mut seq).is_empty());
        seq = seq.wrapping_add(1);
        let mut out = Vec::new();
        p.packetize(
            &au(avcc(&[&idr(100, 6)]), true, 15000),
            Some(&cfg),
            &mut out,
        )
        .unwrap();
        let f = run(&mut d, &out, 15000, &mut seq);
        assert_eq!(f.len(), 1);
        assert!(f[0].keyframe && f[0].discontinuity);
    }

    #[test]
    fn marker_less_au_boundary_by_timestamp() {
        let cfg = config();
        let mut d = H264Depacketizer::default();
        d.set_parameter_sets(&[Bytes::from_static(SPS), Bytes::from_static(PPS)]);
        let a = idr(10, 1);
        let b = slice(10, 2);
        let mut seq = 0;
        let f = run(
            &mut d,
            &[Payload::new(Bytes::from(a.clone()), false)],
            0,
            &mut seq,
        );
        assert!(f.is_empty());
        let f = run(
            &mut d,
            &[Payload::new(Bytes::from(b), true)],
            3000,
            &mut seq,
        );
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].payload, avcc(&[&a]));
        assert_eq!(f[0].config.as_ref().unwrap().record, cfg.record);
    }

    #[test]
    fn parameter_set_change_bumps_generation() {
        let mut d = H264Depacketizer::default();
        let mut seq = 0;
        let stap = |sps: &[u8], pps: &[u8], x: &[u8]| {
            let mut v = vec![24u8];
            for n in [sps, pps, x] {
                v.extend_from_slice(&(n.len() as u16).to_be_bytes());
                v.extend_from_slice(n);
            }
            Payload::new(Bytes::from(v), true)
        };
        let x = idr(20, 1);
        let f = run(&mut d, &[stap(SPS, PPS, &x)], 0, &mut seq);
        assert_eq!(f[0].config_generation, 0);
        let f = run(&mut d, &[stap(SPS, PPS, &x)], 3000, &mut seq);
        assert!(f[0].config.is_none());
        // level を変えた SPS
        let mut sps2 = SPS.to_vec();
        sps2[3] = 0x1E;
        let f = run(&mut d, &[stap(&sps2, PPS, &x)], 6000, &mut seq);
        assert_eq!(f[0].config_generation, 1);
        assert_eq!(f[0].config.as_ref().unwrap().video.unwrap().level, 0x1E);
    }

    #[test]
    fn malformed_rejected_without_panic() {
        let mut d = H264Depacketizer::default();
        for bad in [
            &[24u8, 0, 9, 1][..],
            &[24, 0, 0],
            &[28],
            &[28, 0xC5, 1],
            &[0],
            &[31, 1],
            &[25, 1],
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
                .is_err()
            );
        }
        let cfg = config();
        let mut p = H264Packetizer::new(200, PacketizationMode::NonInterleaved);
        let mut seeds = Vec::new();
        p.packetize(
            &au(avcc(&[&idr(1000, 1), &slice(50, 2)]), true, 0),
            Some(&cfg),
            &mut seeds,
        )
        .unwrap();
        fuzz(&mut d, &seeds, 20000);
    }
}
