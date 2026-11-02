//! VA-API で H.264 を復号するための自前のビットストリーム前処理（`unsafe` なし）。
//!
//! 復号そのもの（エントロピー復号・逆変換・動き補償）は GPU がする。ここでするのは、ハードウェアに渡す値を
//! 作るためのパラメーターセット・スライスヘッダーの解析と、参照ピクチャーの管理（ADR-0004 の 2 の範囲）。
//!
//! - [`params`] — SPS・PPS
//! - [`slice`] — スライスヘッダー
//! - [`dpb`] — POC・参照の印付け・参照リスト・表示順の出力

pub mod dpb;
pub mod params;
pub mod slice;

pub use dpb::{Action, H264Frontend, OutputPic, PictureJob, RefPic, SliceJob, StreamInfo};

#[cfg(test)]
mod tests {
    use super::dpb::*;
    use super::params::*;
    use super::slice::*;
    use crate::bitstream::nal::split_annexb;
    use bytes::Bytes;

    const HIGH_BF: &[u8] = include_bytes!("../testdata/high_bf.h264");
    const HIGH_BF_FRAMES: &str = include_str!("../testdata/high_bf.frames");
    const BASELINE: &[u8] = include_bytes!("../testdata/baseline.h264");
    const BASELINE_FRAMES: &str = include_str!("../testdata/baseline.frames");
    const OPENGOP: &[u8] = include_bytes!("../testdata/opengop.h264");
    const OPENGOP_FRAMES: &str = include_str!("../testdata/opengop.frames");

    /// AUD で区切った AU と、その先頭（開始符号）のファイル内の位置。
    fn split_aus(data: &[u8]) -> Vec<(usize, Vec<Bytes>)> {
        let all = Bytes::copy_from_slice(data);
        let mut aus: Vec<(usize, Vec<Bytes>)> = Vec::new();
        for nal in split_annexb(data) {
            let off = nal.as_ptr() as usize - data.as_ptr() as usize;
            let b = all.slice(off..off + nal.len());
            if nal[0] & 0x1f == 9 {
                // 開始符号（4 バイト）の位置が ffprobe の pkt_pos
                aus.push((off - 4, Vec::new()));
            } else if let Some(last) = aus.last_mut() {
                last.1.push(b);
            }
        }
        aus
    }

    fn expected(frames: &str) -> Vec<(usize, char)> {
        frames
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| {
                let (p, t) = l.split_once(',').unwrap();
                (p.parse().unwrap(), t.chars().next().unwrap())
            })
            .collect()
    }

    /// 前処理だけを動かし、表示順の AU の位置と、各 AU のスライス種別を返す。
    fn run(data: &[u8]) -> (Vec<usize>, Vec<(usize, char)>, Vec<Action>) {
        let aus = split_aus(data);
        let mut fe = H264Frontend::new(1920, 1080);
        let mut out = Vec::new();
        let mut types = Vec::new();
        let mut all = Vec::new();
        for (i, (pos, nals)) in aus.iter().enumerate() {
            let acts = fe.decode_au(nals, i as i64, false).unwrap();
            for a in &acts {
                match a {
                    Action::Submit(p) => {
                        // 復号先のサーフェスは、参照中・表示待ちのものと重ならない
                        assert!(!p.refs.iter().any(|r| r.surface == Some(p.surface)));
                        let t = match p.slices[0].header.slice_type {
                            SLICE_I => 'I',
                            SLICE_P => 'P',
                            _ => 'B',
                        };
                        types.push((*pos, t));
                        for s in &p.slices {
                            assert_eq!(
                                s.list0.len(),
                                if s.header.slice_type == SLICE_I {
                                    0
                                } else {
                                    s.header.num_ref_idx_l0_active_minus1 as usize + 1
                                }
                            );
                            for r in s.list0.iter().chain(s.list1.iter()) {
                                let r = r.unwrap_or_else(|| panic!("unresolved ref: pos {} type {} n0 {} n1 {} refs {:?} l0 {:?} l1 {:?} mods {:?}", pos, s.header.slice_type, s.header.num_ref_idx_l0_active_minus1, s.header.num_ref_idx_l1_active_minus1, p.refs, s.list0, s.list1, s.header.ref_list_mods));
                                assert!(p.refs.contains(&r));
                            }
                            assert!(s.header.header_bits > 8);
                        }
                    }
                    Action::Output(o) => out.push(aus[o.pts as usize].0),
                    Action::Reinit(_) => {}
                }
            }
            all.extend(acts);
        }
        for a in fe.flush() {
            if let Action::Output(o) = &a {
                out.push(aus[o.pts as usize].0);
            }
            all.push(a);
        }
        (out, types, all)
    }

    fn check(data: &[u8], frames: &str) {
        let exp = expected(frames);
        let (out, types, _) = run(data);
        let exp_pos: Vec<usize> = exp.iter().map(|e| e.0).collect();
        assert_eq!(out, exp_pos, "display order differs from ffprobe");
        let mut t = types.clone();
        t.sort();
        let mut e = exp.clone();
        e.sort();
        assert_eq!(t, e, "slice types differ from ffprobe");
    }

    #[test]
    fn high_profile_b_pyramid_order_matches_ffprobe() {
        check(HIGH_BF, HIGH_BF_FRAMES);
    }

    #[test]
    fn baseline_order_matches_ffprobe() {
        check(BASELINE, BASELINE_FRAMES);
    }

    #[test]
    fn open_gop_cavlc_order_matches_ffprobe() {
        check(OPENGOP, OPENGOP_FRAMES);
    }

    #[test]
    fn sps_pps_fields() {
        let aus = split_aus(HIGH_BF);
        let sps = parse_sps(&aus[0].1.iter().find(|n| n[0] & 0x1f == 7).unwrap()[..]).unwrap();
        assert_eq!(sps.profile_idc, 100);
        assert_eq!((sps.pic_width_in_mbs, sps.frame_height_in_mbs()), (11, 7));
        assert_eq!(sps.display_size(), (170, 100));
        assert_eq!(sps.crop, [0, 6, 0, 12]);
        assert_eq!(sps.poc_type, 0);
        assert_eq!(sps.max_num_ref_frames, 4);
        assert!(sps.max_num_reorder_frames.is_some());
        assert!(sps.frame_mbs_only);
        let pps_nal = aus[0].1.iter().find(|n| n[0] & 0x1f == 8).unwrap();
        let pps = parse_pps(pps_nal, |_| Some(sps.clone())).unwrap();
        assert!(pps.entropy_coding_mode);
        assert!(pps.transform_8x8_mode);
        assert!(pps.weighted_pred);
        assert_eq!(pps.weighted_bipred_idc, 2); // x264 の既定（weightb、暗黙の重み）
        assert_eq!(pps.scaling, ScalingLists::flat());

        let aus = split_aus(BASELINE);
        let sps = parse_sps(&aus[0].1.iter().find(|n| n[0] & 0x1f == 7).unwrap()[..]).unwrap();
        assert_eq!(sps.profile_idc, 66);
        assert_eq!(sps.poc_type, 2);
        assert_eq!(sps.reorder_depth(), 0);
        assert_eq!(sps.display_size(), (176, 144));
    }

    #[test]
    fn baseline_outputs_immediately() {
        // B の無いストリームは、入れた AU がすぐ出る（低遅延）。
        let aus = split_aus(BASELINE);
        let mut fe = H264Frontend::new(1920, 1080);
        for (i, (_, nals)) in aus.iter().enumerate() {
            let acts = fe.decode_au(nals, i as i64, false).unwrap();
            let outs: Vec<i64> = acts
                .iter()
                .filter_map(|a| {
                    if let Action::Output(o) = a {
                        Some(o.pts)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(outs, vec![i as i64]);
        }
    }

    #[test]
    fn slice_header_lengths_match_ffmpeg_trace() {
        let exp: Vec<u32> = include_str!("../testdata/high_bf.hdrbits")
            .split_whitespace()
            .map(|v| v.parse().unwrap())
            .collect();
        let (_, _, acts) = run(HIGH_BF);
        let got: Vec<u32> = acts
            .iter()
            .filter_map(|a| {
                if let Action::Submit(p) = a {
                    Some(p.slices[0].header.header_bits)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(got, exp);
    }

    #[test]
    fn weighted_prediction_parsed() {
        // weightp=2 の P スライスには pred_weight_table がある
        let aus = split_aus(HIGH_BF);
        let mut fe = H264Frontend::new(1920, 1080);
        let mut seen = false;
        for (i, (_, nals)) in aus.iter().enumerate() {
            for a in fe.decode_au(nals, i as i64, false).unwrap() {
                if let Action::Submit(p) = a {
                    for s in &p.slices {
                        if s.header.slice_type == SLICE_P {
                            seen |= s.header.pred_weight.is_some();
                        }
                    }
                }
            }
        }
        assert!(seen);
    }

    #[test]
    fn starts_at_keyframe_and_rejects_garbage() {
        let aus = split_aus(HIGH_BF);
        let mut fe = H264Frontend::new(1920, 1080);
        // パラメーターセットだけ入れて、P から始めると捨てられる
        let ps: Vec<Bytes> = aus[0]
            .1
            .iter()
            .filter(|n| matches!(n[0] & 0x1f, 7 | 8))
            .cloned()
            .collect();
        fe.decode_au(&ps, 0, false).unwrap();
        let acts = fe.decode_au(&aus[2].1, 2, false).unwrap();
        assert!(!acts.iter().any(|a| matches!(a, Action::Submit(_))));
        // 壊れたスライス
        assert!(
            fe.decode_au(&[Bytes::from_static(&[0x65, 0xff])], 0, false)
                .is_err()
        );
        assert!(
            fe.decode_au(&[Bytes::from_static(&[0x65])], 0, false)
                .is_err()
        );
        // 知らない PPS
        let mut fe2 = H264Frontend::new(1920, 1080);
        assert!(
            fe2.decode_au(&aus[0].1[aus[0].1.len() - 1..], 0, false)
                .is_err()
        );
        // 解像度の上限
        let mut fe3 = H264Frontend::new(64, 64);
        assert!(fe3.decode_au(&aus[0].1, 0, false).is_err());
    }

    #[test]
    fn truncation_never_panics() {
        let aus = split_aus(HIGH_BF);
        let mut fe = H264Frontend::new(1920, 1080);
        for (i, (_, nals)) in aus.iter().enumerate() {
            for n in nals {
                for cut in [1, 2, 3, 5, 8, n.len() / 2] {
                    if cut < n.len() {
                        let _ = fe.decode_au(&[n.slice(..cut)], i as i64, false);
                    }
                }
                // ビットを反転したもの
                let mut v = n.to_vec();
                for b in v.iter_mut().skip(1).step_by(3) {
                    *b ^= 0x5a;
                }
                let _ = fe.decode_au(&[Bytes::from(v)], i as i64, false);
            }
        }
        let _ = fe.flush();
    }

    #[test]
    fn interlaced_rejected() {
        // frame_mbs_only_flag = 0 の SPS を作る（Main, 16x16 MB, POC 型 2）
        let mut w = senca_core::bits::BitWriter::new();
        w.write_bits(77, 8);
        w.write_bits(0, 8);
        w.write_bits(30, 8);
        w.write_ue(0); // sps_id
        w.write_ue(0); // log2_max_frame_num - 4
        w.write_ue(2); // poc type
        w.write_ue(1); // max_num_ref_frames
        w.write_bit(false);
        w.write_ue(0); // width mbs - 1
        w.write_ue(0); // height map units - 1
        w.write_bit(false); // frame_mbs_only
        w.write_bit(false); // mbaff
        w.write_bit(true);
        w.write_bit(false); // cropping
        w.write_bit(false); // vui
        w.write_bit(true); // stop bit
        let mut sps = vec![0x67];
        sps.extend(w.finish());
        let s = parse_sps(&sps).unwrap();
        assert!(!s.frame_mbs_only);
        let mut w = senca_core::bits::BitWriter::new();
        w.write_ue(0);
        w.write_ue(0);
        w.write_bits(0, 2);
        w.write_ue(0);
        w.write_ue(0);
        w.write_ue(0);
        w.write_bits(0, 3);
        w.write_se(0);
        w.write_se(0);
        w.write_se(0);
        w.write_bits(0, 3);
        w.write_bit(true);
        let mut pps = vec![0x68];
        pps.extend(w.finish());
        let mut w = senca_core::bits::BitWriter::new();
        w.write_ue(0);
        w.write_ue(7);
        w.write_ue(0);
        w.write_bits(0, 8);
        let mut slice = vec![0x65];
        slice.extend(w.finish());
        let mut fe = H264Frontend::new(1920, 1080);
        let e = fe
            .decode_au(
                &[Bytes::from(sps), Bytes::from(pps), Bytes::from(slice)],
                0,
                false,
            )
            .unwrap_err();
        assert_eq!(e.kind, crate::ErrorKind::Unsupported);
    }
}
