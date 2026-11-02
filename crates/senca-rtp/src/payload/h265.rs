//! H.265（RFC 7798）。single NAL・AP（48）・FU（49）。DONL は使わない（sprop-max-don-diff = 0）。PACI は未対応。
//!
//! 受信: VPS・SPS・PPS は AU から抜いて設定（HEVCDecoderConfigurationRecord）にする。SDP の sprop-vps/sps/pps も使う。

use super::h26x::{Flavor, NalAssembler, NalClass, ParamKind, au_nals, packetize_nals};
use super::{Depacketizer, Frame, Packetizer, Payload, PayloadFormat};
use crate::error::{Error, Result};
use crate::packet::RtpPacket;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use senca_core::media::{AccessUnit, Codec, CodecConfig};
use senca_h265::{HevcDecoderConfigurationRecord, Pps, Sps, Vps};

const AP: u8 = 48;
const FU: u8 = 49;
const PACI: u8 = 50;

pub(crate) struct H265;

fn nal_type(nal: &[u8]) -> u8 {
    (nal[0] >> 1) & 0x3f
}

impl Flavor for H265 {
    const HDR: usize = 2;

    fn classify(nal: &[u8]) -> NalClass {
        if nal.len() < 2 {
            return NalClass::Skip;
        }
        match nal_type(nal) {
            32 => NalClass::Param(
                ParamKind::Vps,
                Vps::parse(nal).map_or(0, |v| v.vps_video_parameter_set_id),
            ),
            33 => NalClass::Param(
                ParamKind::Sps,
                Sps::parse(nal).map_or(0, |s| s.sps_seq_parameter_set_id),
            ),
            34 => NalClass::Param(
                ParamKind::Pps,
                Pps::parse(nal).map_or(0, |p| p.pps_pic_parameter_set_id),
            ),
            35 | 38 => NalClass::Skip,
            t @ 0..=31 => NalClass::Vcl {
                key: (16..=23).contains(&t),
            },
            _ => NalClass::Other,
        }
    }

    fn build_config(
        vps: &[Bytes],
        sps: &[Bytes],
        pps: &[Bytes],
        generation: u32,
    ) -> Result<CodecConfig> {
        if vps.is_empty() {
            return Err(Error::Invalid("VPS がない"));
        }
        let v: Vec<&[u8]> = vps.iter().map(|b| &b[..]).collect();
        let s: Vec<&[u8]> = sps.iter().map(|b| &b[..]).collect();
        let p: Vec<&[u8]> = pps.iter().map(|b| &b[..]).collect();
        HevcDecoderConfigurationRecord::from_parameter_sets(&v, &s, &p, &[])
            .and_then(|r| r.codec_config(generation))
            .map_err(|_| Error::Invalid("VPS・SPS・PPS"))
    }

    fn from_config(cfg: &CodecConfig) -> Result<(Vec<Bytes>, u8)> {
        let r = HevcDecoderConfigurationRecord::parse(&cfg.record)
            .map_err(|_| Error::Invalid("HEVCDecoderConfigurationRecord"))?;
        let mut v = Vec::new();
        for ty in [32u8, 33, 34] {
            for a in &r.arrays {
                if a.nal_unit_type.as_u8() == ty {
                    v.extend(a.nalus.iter().cloned());
                }
            }
        }
        Ok((v, r.length_size))
    }

    fn agg_header(nals: &[Bytes]) -> Vec<u8> {
        let f = nals.iter().fold(0u8, |a, n| a | (n[0] & 0x80));
        let layer = nals
            .iter()
            .map(|n| ((n[0] & 1) << 5) | (n[1] >> 3))
            .min()
            .unwrap_or(0);
        let tid = nals.iter().map(|n| n[1] & 7).min().unwrap_or(1).max(1);
        vec![f | (AP << 1) | (layer >> 5), (layer << 3) | tid]
    }

    fn fu_header(nal: &[u8], start: bool, end: bool) -> Vec<u8> {
        vec![
            (nal[0] & 0x81) | (FU << 1),
            nal[1],
            (start as u8) << 7 | (end as u8) << 6 | nal_type(nal),
        ]
    }
}

/// H.265 の depacketizer。
pub struct H265Depacketizer {
    asm: NalAssembler<H265>,
}

impl Default for H265Depacketizer {
    fn default() -> Self {
        H265Depacketizer {
            asm: NalAssembler::new(),
        }
    }
}

fn parse_sprop(s: &str, out: &mut Vec<Bytes>) -> Result<()> {
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if out.len() >= 64 {
            return Err(Error::Limit("sprop の数"));
        }
        let v = STANDARD
            .decode(part)
            .map_err(|_| Error::Invalid("sprop の base64"))?;
        out.push(Bytes::from(v));
    }
    Ok(())
}

impl H265Depacketizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// SDP の fmtp（sprop-vps・sprop-sps・sprop-pps・sprop-max-don-diff）から作る。
    pub fn from_format(fmt: &PayloadFormat) -> Result<Self> {
        if fmt
            .fmtp("sprop-max-don-diff")
            .is_some_and(|v| v.trim() != "0")
        {
            return Err(Error::Unsupported("DONL（sprop-max-don-diff > 0）"));
        }
        let mut d = Self::new();
        let mut nals = Vec::new();
        for k in ["sprop-vps", "sprop-sps", "sprop-pps"] {
            if let Some(v) = fmt.fmtp(k) {
                // 壊れた sprop は無視して帯域内のものを待つ
                let _ = parse_sprop(v, &mut nals);
            }
        }
        d.set_parameter_sets(&nals);
        Ok(d)
    }

    /// 帯域外のパラメーターセット（VPS・SPS・PPS の NAL）を入れる。
    pub fn set_parameter_sets(&mut self, nals: &[Bytes]) {
        self.asm.add_out_of_band(nals);
    }

    fn parse(&mut self, p: &Bytes) -> Result<()> {
        if p.len() < 2 {
            return Err(Error::Truncated);
        }
        match nal_type(p) {
            AP => {
                let mut i = 2usize;
                let mut n = 0;
                while i < p.len() {
                    let len = u16::from_be_bytes([
                        *p.get(i).ok_or(Error::Truncated)?,
                        *p.get(i + 1).ok_or(Error::Truncated)?,
                    ]) as usize;
                    i += 2;
                    if len < 2 || i + len > p.len() {
                        return Err(Error::Invalid("AP の NAL 長"));
                    }
                    self.asm.add_nal(p.slice(i..i + len))?;
                    i += len;
                    n += 1;
                }
                if n < 1 {
                    return Err(Error::Invalid("空の AP"));
                }
                Ok(())
            }
            FU => {
                let h = *p.get(2).ok_or(Error::Truncated)?;
                let (s, e) = (h & 0x80 != 0, h & 0x40 != 0);
                if s && e {
                    return Err(Error::Invalid("FU の S と E が両方立っている"));
                }
                let ty = h & 0x3f;
                if matches!(ty, AP | FU | PACI) {
                    return Err(Error::Invalid("FU の中の NAL type"));
                }
                let data = &p[3..];
                if s {
                    let hdr = [(p[0] & 0x81) | (ty << 1), p[1]];
                    self.asm.fu_start(&hdr, data)
                } else {
                    self.asm.fu_continue(data, e)
                }
            }
            PACI => Err(Error::Unsupported("PACI")),
            51..=63 => Err(Error::Invalid("未定義の NAL type")),
            _ => self.asm.add_nal(p.clone()),
        }
    }
}

impl Depacketizer for H265Depacketizer {
    fn codec(&self) -> Codec {
        Codec::H265
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

/// H.265 の packetizer（single・AP・FU）。
pub struct H265Packetizer {
    mtu: usize,
}

impl H265Packetizer {
    pub fn new(mtu: usize) -> Self {
        H265Packetizer { mtu: mtu.max(16) }
    }
}

impl Packetizer for H265Packetizer {
    fn codec(&self) -> Codec {
        Codec::H265
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
        let nals = au_nals::<H265>(&au.payload, au.keyframe, config, |b, l| {
            senca_h265::length_prefixed_nals_bytes(b, l).map_err(|_| ())
        })?;
        packetize_nals::<H265>(&nals, self.mtu, true, out)
    }
}

/// SDP に書く payload format（`H265/90000`、sprop-vps/sps/pps）。
pub fn payload_format(config: Option<&CodecConfig>) -> Result<PayloadFormat> {
    let mut fmtp = Vec::new();
    if let Some(c) = config {
        let r = HevcDecoderConfigurationRecord::parse(&c.record)
            .map_err(|_| Error::Invalid("HEVCDecoderConfigurationRecord"))?;
        for (ty, key) in [(32u8, "sprop-vps"), (33, "sprop-sps"), (34, "sprop-pps")] {
            let sets: Vec<String> = r
                .arrays
                .iter()
                .filter(|a| a.nal_unit_type.as_u8() == ty)
                .flat_map(|a| a.nalus.iter().map(|n| STANDARD.encode(n)))
                .collect();
            if !sets.is_empty() {
                fmtp.push((key.to_string(), sets.join(",")));
            }
        }
    }
    Ok(PayloadFormat {
        encoding: "H265".into(),
        clock_rate: 90000,
        channels: None,
        fmtp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::testutil::{Rng, fuzz, run};
    use senca_core::{Timebase, TrackId};

    const VPS: &[u8] = &[
        0x40, 0x01, 0x0C, 0x01, 0xFF, 0xFF, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00,
        0x03, 0x00, 0x00, 0x03, 0x00, 0x3C, 0x95, 0x98, 0x09,
    ];
    const SPS: &[u8] = &[
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x3C, 0xA0, 0x0A, 0x08, 0x0F, 0x16, 0x59, 0x59, 0xA4, 0x93, 0x2B, 0xC0, 0x5A,
        0x02, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0x3C, 0x10,
    ];
    const PPS: &[u8] = &[0x44, 0x01, 0xC1, 0x72, 0xB4, 0x62, 0x40];

    fn config() -> CodecConfig {
        senca_h265::codec_config(VPS, SPS, PPS, 0).unwrap()
    }

    fn nal(ty: u8, len: usize, seed: u32) -> Vec<u8> {
        let mut r = Rng(seed | 1);
        let mut v = vec![ty << 1, 1];
        v.extend((0..len).map(|_| r.next() as u8 | 1));
        v
    }

    fn avcc(nals: &[&[u8]]) -> Bytes {
        crate::payload::h264::tests::avcc(nals)
    }

    fn au(payload: Bytes, key: bool) -> AccessUnit {
        AccessUnit {
            track: TrackId(0),
            codec: Codec::H265,
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

    #[test]
    fn roundtrip_ap_fu() {
        let cfg = config();
        let mut p = H265Packetizer::new(400);
        let mut d = H265Depacketizer::new();
        let idr = nal(19, 2000, 1);
        let mut out = Vec::new();
        p.packetize(&au(avcc(&[&idr]), true), Some(&cfg), &mut out)
            .unwrap();
        assert_eq!(nal_type(&out[0].data), AP);
        assert!(out[1..].iter().all(|p| nal_type(&p.data) == FU));
        assert!(out.iter().all(|p| p.data.len() <= 400));
        let mut seq = 0;
        let f = run(&mut d, &out, 0, &mut seq);
        assert_eq!(f.len(), 1);
        assert!(f[0].keyframe);
        assert_eq!(f[0].payload, avcc(&[&idr]));
        assert_eq!(f[0].config.as_ref().unwrap().record, cfg.record);
        let trail = nal(1, 50, 2);
        let mut out = Vec::new();
        p.packetize(&au(avcc(&[&trail]), false), Some(&cfg), &mut out)
            .unwrap();
        assert_eq!(out.len(), 1);
        let f = run(&mut d, &out, 3000, &mut seq);
        assert_eq!(f[0].payload, avcc(&[&trail]));
    }

    #[test]
    fn sprop_and_loss() {
        let fmt = payload_format(Some(&config())).unwrap();
        assert!(fmt.fmtp("sprop-vps").is_some());
        let mut d = H265Depacketizer::from_format(&fmt).unwrap();
        assert!(d.config().is_some());
        let mut p = H265Packetizer::new(300);
        let mut out = Vec::new();
        p.packetize(&au(avcc(&[&nal(1, 1000, 3)]), false), None, &mut out)
            .unwrap();
        let mut seq = 0;
        // 最初はキーフレームを待つ
        assert!(run(&mut d, &out, 0, &mut seq).is_empty());
        let mut out = Vec::new();
        p.packetize(&au(avcc(&[&nal(20, 1000, 4)]), true), None, &mut out)
            .unwrap();
        out.remove(1);
        assert!(run(&mut d, &out[..1], 3000, &mut seq).is_empty());
        seq = seq.wrapping_add(1);
        assert!(run(&mut d, &out[1..], 3000, &mut seq).is_empty());
        let bad = PayloadFormat {
            fmtp: vec![("sprop-max-don-diff".into(), "2".into())],
            ..fmt
        };
        assert!(H265Depacketizer::from_format(&bad).is_err());
    }

    #[test]
    fn malformed() {
        let mut d = H265Depacketizer::new();
        for bad in [
            &[0x60u8, 1, 0, 9][..],
            &[0x62, 1],
            &[0x62, 1, 0xC1],
            &[0x64, 1],
            &[0x7E, 1],
            &[0x62, 1, 0x80 | 49],
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
        let mut p = H265Packetizer::new(200);
        let mut seeds = Vec::new();
        p.packetize(
            &au(avcc(&[&nal(19, 800, 1), &nal(39, 10, 2)]), true),
            Some(&config()),
            &mut seeds,
        )
        .unwrap();
        fuzz(&mut d, &seeds, 20000);
    }
}
