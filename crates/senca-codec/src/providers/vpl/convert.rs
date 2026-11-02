//! ハードウェアのコーデック API と内部形式（ADR-0003）の変換。`unsafe` を含まない。
//!
//! oneVPL と AMF の両方が使う（どちらも Annex B / OBU の生のビットストリームを入出力する）。
//!
//! 入力（デコーダーへ）: 長さ前置の NAL 列 → Annex B。キーフレームの前には設定（avcC・hvcC）の
//! パラメーターセットを付ける（AU に入っていなくても復号できるように）。AV1 は Temporal Delimiter を付け、
//! キーフレームでシーケンスヘッダーが無ければ av1C の configOBUs を前に付ける。VP9 はそのまま。
//!
//! 出力（エンコーダーから）: Annex B → 長さ前置（SPS/PPS/VPS と AUD は AU から外して設定に入れる）。
//! AV1 は Temporal Delimiter を外し、シーケンスヘッダーから av1C を作る。

use crate::bitstream::{av1, h264, h265, nal};
use crate::error::{CodecError, Result};
use crate::providers::common::AuStamper;
use bytes::Bytes;
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase, TrackId};

/// デコーダーに渡す 1 AU の上限（64 MiB）。
pub const MAX_INPUT_AU: usize = 64 << 20;

/// 内部形式の AU を、ハードウェアのデコーダーが受け取る形にする。
#[derive(Debug, Clone)]
pub struct InputConverter {
    codec: Codec,
    length_size: usize,
    /// キーフレームの前に付けるもの（Annex B のパラメーターセット、AV1 のシーケンスヘッダー OBU）。
    prefix: Vec<u8>,
}

impl InputConverter {
    pub fn new(codec: Codec, config: Option<&CodecConfig>) -> Result<Self> {
        if !matches!(
            codec,
            Codec::H264 | Codec::H265 | Codec::Av1 | Codec::Vp9 | Codec::Vp8
        ) {
            return Err(CodecError::unsupported(format!(
                "{codec} is not a video codec handled here"
            )));
        }
        let mut c = InputConverter {
            codec,
            length_size: 4,
            prefix: Vec::new(),
        };
        if let Some(cfg) = config {
            c.set_config(cfg)?;
        }
        Ok(c)
    }

    pub fn set_config(&mut self, cfg: &CodecConfig) -> Result<()> {
        if cfg.codec != self.codec {
            return Err(CodecError::invalid_config(
                "codec config does not match decoder",
            ));
        }
        if cfg.record.is_empty() {
            self.prefix.clear();
            return Ok(());
        }
        match self.codec {
            Codec::H264 => {
                let r = h264::parse_avcc(&cfg.record)?;
                self.length_size = r.length_size;
                self.prefix =
                    nal::nals_to_annexb(r.sps.iter().chain(r.pps.iter()).map(|b| b.as_ref()));
            }
            Codec::H265 => {
                let r = h265::parse_hvcc(&cfg.record)?;
                self.length_size = r.length_size;
                self.prefix = nal::nals_to_annexb(
                    r.vps
                        .iter()
                        .chain(r.sps.iter())
                        .chain(r.pps.iter())
                        .map(|b| b.as_ref()),
                );
            }
            Codec::Av1 => {
                let obus = av1::parse_av1c(&cfg.record)?;
                self.prefix = av1::to_internal(obus)?;
            }
            _ => self.prefix.clear(),
        }
        Ok(())
    }

    /// 設定から分かる解像度（H.264/H.265 の SPS、AV1 のシーケンスヘッダーの最大値）。
    pub fn config_resolution(cfg: &CodecConfig) -> Option<(u32, u32)> {
        if let Some(v) = cfg.video
            && v.width > 0
            && v.height > 0
        {
            return Some((v.width, v.height));
        }
        match cfg.codec {
            Codec::H264 => {
                let r = h264::parse_avcc(&cfg.record).ok()?;
                let i = h264::parse_sps(r.sps.first()?).ok()?;
                Some((i.width, i.height))
            }
            Codec::H265 => {
                let r = h265::parse_hvcc(&cfg.record).ok()?;
                let i = h265::parse_sps(r.sps.first()?).ok()?;
                Some((i.width, i.height))
            }
            _ => None,
        }
    }

    pub fn convert(&self, au: &AccessUnit) -> Result<Vec<u8>> {
        if au.codec != self.codec {
            return Err(CodecError::invalid_input(
                "access unit codec does not match decoder",
            ));
        }
        if au.payload.len() > MAX_INPUT_AU {
            return Err(CodecError::invalid_input("access unit too large"));
        }
        match self.codec {
            Codec::H264 | Codec::H265 => {
                let nals = nal::split_length_prefixed(&au.payload, self.length_size)?;
                let mut out =
                    Vec::with_capacity(au.payload.len() + self.prefix.len() + nals.len() * 4);
                let has_ps = nals.iter().any(|n| match self.codec {
                    Codec::H264 => h264::nal_type(n) == h264::NAL_SPS,
                    _ => h265::nal_type(n) == h265::NAL_SPS,
                });
                if au.keyframe && !has_ps {
                    out.extend_from_slice(&self.prefix);
                }
                for n in nals {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(n);
                }
                Ok(out)
            }
            Codec::Av1 => {
                let obus = av1::parse_obus(&au.payload)?;
                let has_seq = obus.iter().any(|o| o.obu_type == av1::OBU_SEQUENCE_HEADER);
                let mut out = Vec::with_capacity(au.payload.len() + self.prefix.len() + 2);
                out.extend_from_slice(&[0x12, 0x00]);
                if au.keyframe && !has_seq {
                    out.extend_from_slice(&self.prefix);
                }
                for o in obus {
                    if o.obu_type != av1::OBU_TEMPORAL_DELIMITER {
                        out.extend_from_slice(o.raw);
                    }
                }
                Ok(out)
            }
            _ => Ok(au.payload.to_vec()),
        }
    }
}

/// ハードウェアのエンコーダーの出力（Annex B / OBU）を内部形式の AU にする。
#[derive(Debug, Clone)]
pub struct OutputConverter {
    codec: Codec,
    stamper: AuStamper,
}

impl OutputConverter {
    pub fn new(codec: Codec, track: TrackId, timebase: Timebase) -> Result<Self> {
        if !matches!(codec, Codec::H264 | Codec::H265 | Codec::Av1) {
            return Err(CodecError::unsupported(format!(
                "{codec} encode output is not handled here"
            )));
        }
        Ok(OutputConverter {
            codec,
            stamper: AuStamper::new(codec, track, timebase),
        })
    }

    pub fn config(&self) -> Option<CodecConfig> {
        self.stamper.config()
    }

    /// 1 フレーム分の出力を変換する。中身が空（パラメーターセットだけ等）なら `None`。
    /// `keyframe_hint` は API が返したフレーム種別（分かるとき）。AV1 だけで使い、H.264/H.265 は NAL 種別で決める。
    pub fn push(
        &mut self,
        data: &[u8],
        pts: i64,
        dts: i64,
        duration: u64,
        keyframe_hint: Option<bool>,
    ) -> Result<Option<AccessUnit>> {
        let (payload, keyframe) = match self.codec {
            Codec::H264 => {
                let (sps, pps, rest) = h264::split_parameter_sets(data);
                if !sps.is_empty() && !pps.is_empty() {
                    let rec = h264::build_avcc(&sps, &pps)?;
                    let vp = h264::video_params(sps[0])?;
                    self.stamper.set_config(rec, Some(vp), None);
                }
                let key = rest.iter().any(|n| h264::nal_type(n) == h264::NAL_IDR);
                if rest.is_empty() {
                    return Ok(None);
                }
                (nal::to_length_prefixed(rest), key)
            }
            Codec::H265 => {
                let ps = h265::split_parameter_sets(data);
                if !ps.vps.is_empty() && !ps.sps.is_empty() && !ps.pps.is_empty() {
                    let rec = h265::build_hvcc(&ps.vps, &ps.sps, &ps.pps)?;
                    let vp = h265::video_params(ps.sps[0])?;
                    self.stamper.set_config(rec, Some(vp), None);
                }
                let key = ps.rest.iter().any(|n| h265::is_irap(n));
                if ps.rest.is_empty() {
                    return Ok(None);
                }
                (nal::to_length_prefixed(ps.rest), key)
            }
            _ => {
                let internal = av1::to_internal(data)?;
                let mut has_seq = false;
                if let Some(seq) = av1::find_sequence_header(&internal)? {
                    has_seq = true;
                    let (rec, sh) = av1::build_av1c(seq)?;
                    self.stamper
                        .set_config(rec, Some(av1::video_params(&sh)), None);
                }
                if internal.is_empty() {
                    return Ok(None);
                }
                (internal, keyframe_hint.unwrap_or(has_seq))
            }
        };
        Ok(Some(self.stamper.stamp(
            Bytes::from(payload),
            pts,
            dts,
            duration,
            keyframe,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitstream::h264::tests::{PPS, SPS_320X240_HIGH};

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        nal::nals_to_annexb(nals.iter().copied())
    }

    #[test]
    fn h264_output_and_input_roundtrip() {
        let mut oc = OutputConverter::new(Codec::H264, TrackId(3), Timebase::MPEG).unwrap();
        let idr: &[u8] = &[0x65, 0x88, 0x84, 0x21];
        let aud: &[u8] = &[0x09, 0xf0];
        let raw = annexb(&[aud, SPS_320X240_HIGH, PPS, idr]);
        let au = oc.push(&raw, 0, 0, 3000, None).unwrap().unwrap();
        assert!(au.keyframe);
        assert_eq!(au.track, TrackId(3));
        assert_eq!(au.payload.as_ref(), &[0, 0, 0, 4, 0x65, 0x88, 0x84, 0x21]);
        let cfg = oc.config().unwrap();
        assert_eq!(cfg.video.unwrap().width, 320);
        assert_eq!(InputConverter::config_resolution(&cfg), Some((320, 240)));
        // P フレーム
        let p = oc
            .push(&annexb(&[&[0x41, 0x9a, 0x02]]), 3000, 3000, 3000, None)
            .unwrap()
            .unwrap();
        assert!(!p.keyframe);
        assert_eq!(p.config_generation, au.config_generation);
        // パラメーターセットだけの出力は AU にしない
        assert!(
            oc.push(&annexb(&[SPS_320X240_HIGH, PPS]), 0, 0, 0, None)
                .unwrap()
                .is_none()
        );

        let ic = InputConverter::new(Codec::H264, Some(&cfg)).unwrap();
        let back = ic.convert(&au).unwrap();
        let nals = nal::split_annexb(&back);
        assert_eq!(nals, vec![SPS_320X240_HIGH, PPS, idr]);
        let back = ic.convert(&p).unwrap();
        assert_eq!(nal::split_annexb(&back), vec![&[0x41, 0x9a, 0x02][..]]);
    }

    #[test]
    fn input_rejects_bad() {
        let ic = InputConverter::new(Codec::H264, None).unwrap();
        let mut au = AccessUnit {
            track: TrackId(0),
            codec: Codec::H264,
            epoch: 0,
            sequence: 0,
            config_generation: 0,
            pts: 0,
            dts: 0,
            duration: 0,
            timebase: Timebase::MPEG,
            keyframe: false,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from_static(&[0, 0, 0, 9, 1]),
        };
        assert!(ic.convert(&au).is_err());
        au.codec = Codec::H265;
        assert!(ic.convert(&au).is_err());
        assert!(InputConverter::new(Codec::Opus, None).is_err());
        assert!(OutputConverter::new(Codec::Vp8, TrackId(0), Timebase::MPEG).is_err());
    }

    #[test]
    fn av1_input_adds_td() {
        let ic = InputConverter::new(Codec::Av1, None).unwrap();
        let au = AccessUnit {
            track: TrackId(0),
            codec: Codec::Av1,
            epoch: 0,
            sequence: 0,
            config_generation: 0,
            pts: 0,
            dts: 0,
            duration: 0,
            timebase: Timebase::MPEG,
            keyframe: false,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from_static(&[0x32, 0x01, 0xaa]),
        };
        assert_eq!(ic.convert(&au).unwrap(), vec![0x12, 0x00, 0x32, 0x01, 0xaa]);
    }
}
