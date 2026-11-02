//! UVC カメラが符号化した H.264 の素通し（復号・符号化はしない、ADR-0004 の 2）。
//!
//! UVC の H.264（V4L2 の `V4L2_PIX_FMT_H264`）は 1 バッファに Annex B の 1 アクセスユニットが入る。
//! これを内部形式（ADR-0003: 4 バイトの長さ前置）にし、SPS/PPS は `CodecConfig`（avcC）に移す。
//!
//! - SPS/PPS が変わると設定の世代（`generation`）が増える。
//! - 最初の SPS/PPS と IDR が揃うまでの AU は捨てる（復号を始められないため）。
//! - B フレームは無い前提で `dts = pts`（UVC カメラの H.264 は Baseline/Main の低遅延が一般的）。
//!   B フレームを使うカメラでは DTS が崩れる。実機で確かめるまで未検証。

use crate::error::{CaptureError, Result};
use bytes::Bytes;
use senca_codec::bitstream::h264::{build_avcc, nal_type, split_parameter_sets, video_params};
use senca_codec::bitstream::nal::to_length_prefixed;
use senca_codec::providers::common::AuStamper;
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase, TrackId};

const NAL_IDR: u8 = 5;

/// Annex B の AU を内部形式の AU にする。
#[derive(Debug, Clone)]
pub struct H264Passthrough {
    stamper: AuStamper,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    waiting_for_idr: bool,
    /// 捨てた AU の数（SPS/PPS・IDR 待ち）。
    pub dropped: u64,
}

impl H264Passthrough {
    pub fn new(track: TrackId, timebase: Timebase) -> Self {
        H264Passthrough {
            stamper: AuStamper::new(Codec::H264, track, timebase),
            sps: None,
            pps: None,
            waiting_for_idr: true,
            dropped: 0,
        }
    }

    pub fn codec_config(&self) -> Option<CodecConfig> {
        self.stamper.config()
    }

    /// Annex B の 1 AU を入れる。出せる AU なら返す（待ちの間は `None`）。
    pub fn push(&mut self, annexb: &[u8], pts: i64, duration: u64) -> Result<Option<AccessUnit>> {
        let (sps, pps, rest) = split_parameter_sets(annexb);
        if let Some(s) = sps.last() {
            self.sps = Some(s.to_vec());
        }
        if let Some(p) = pps.last() {
            self.pps = Some(p.to_vec());
        }
        if (!sps.is_empty() || !pps.is_empty())
            && let (Some(s), Some(p)) = (&self.sps, &self.pps)
        {
            let rec = build_avcc(&[s.as_slice()], &[p.as_slice()])
                .map_err(|e| CaptureError::invalid_input(format!("h264 avcC: {}", e.message)))?;
            let vp = video_params(s)
                .map_err(|e| CaptureError::invalid_input(format!("h264 SPS: {}", e.message)))?;
            self.stamper.set_config(rec, Some(vp), None);
        }
        let slices: Vec<&[u8]> = rest.into_iter().filter(|n| !n.is_empty()).collect();
        if slices.is_empty() {
            return Ok(None);
        }
        let keyframe = slices.iter().any(|n| nal_type(n) == NAL_IDR);
        if self.stamper.config().is_none() || (self.waiting_for_idr && !keyframe) {
            self.dropped += 1;
            return Ok(None);
        }
        self.waiting_for_idr = false;
        let payload = Bytes::from(to_length_prefixed(slices));
        Ok(Some(
            self.stamper.stamp(payload, pts, pts, duration, keyframe),
        ))
    }

    /// 次の IDR まで捨てる（欠落の後など）。
    pub fn wait_for_keyframe(&mut self) {
        self.waiting_for_idr = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// x264 の 320x240 High の SPS と PPS（senca-codec の試験と同じ既知の値）。
    const SPS: &[u8] = &[
        0x67, 0x64, 0x00, 0x0d, 0xac, 0xd9, 0x41, 0x41, 0xfb, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00,
        0x10, 0x00, 0x00, 0x03, 0x03, 0xc0, 0xf1, 0x42, 0x99, 0x60,
    ];
    const PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0];

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&[0, 0, 0, 1]);
            v.extend_from_slice(n);
        }
        v
    }

    #[test]
    fn converts_and_waits_for_idr() {
        let mut p = H264Passthrough::new(TrackId(0), Timebase::MPEG);
        let aud: &[u8] = &[0x09, 0xf0];
        let idr: &[u8] = &[0x65, 0x88, 0x84, 0x00, 0x33];
        let non_idr: &[u8] = &[0x41, 0x9a, 0x02];
        // SPS/PPS の前の P は捨てる
        assert!(p.push(&annexb(&[non_idr]), 0, 3000).unwrap().is_none());
        // SPS/PPS だけでは出さないが、設定はできる
        assert!(p.push(&annexb(&[SPS, PPS]), 0, 3000).unwrap().is_none());
        let c = p.codec_config().unwrap();
        assert_eq!(c.generation, 0);
        assert_eq!(c.video.unwrap().width, 320);
        // IDR より前の P は捨てる
        assert!(p.push(&annexb(&[non_idr]), 3000, 3000).unwrap().is_none());
        assert_eq!(p.dropped, 2);
        let au = p
            .push(&annexb(&[aud, SPS, PPS, idr]), 6000, 3000)
            .unwrap()
            .unwrap();
        assert!(au.keyframe);
        assert_eq!(au.codec, Codec::H264);
        assert_eq!((au.pts, au.dts, au.sequence), (6000, 6000, 0));
        // AUD・SPS・PPS は AU に入れない
        let mut want = (idr.len() as u32).to_be_bytes().to_vec();
        want.extend_from_slice(idr);
        assert_eq!(&au.payload[..], &want[..]);
        let au = p.push(&annexb(&[non_idr]), 9000, 3000).unwrap().unwrap();
        assert!(!au.keyframe);
        assert_eq!(au.sequence, 1);
        // 同じ SPS/PPS なら世代は変わらない
        p.push(&annexb(&[SPS, PPS, idr]), 12000, 3000).unwrap();
        assert_eq!(p.codec_config().unwrap().generation, 0);
        // 壊れた SPS は誤り
        let mut q = H264Passthrough::new(TrackId(0), Timebase::MPEG);
        assert!(q.push(&annexb(&[&[0x67, 0x64], PPS, idr]), 0, 0).is_err());
    }
}
