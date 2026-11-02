//! 例の共通部分: トラックごとのサンプルを fMP4 または MP4 にする。

#![allow(dead_code)]

use senca_fmp4::{FragmentWriter, Mp4Writer, Sample, TrackRun, TrackSpec, write_init_segment};
use senca_core::MediaKind;

/// (トラック, サンプル列) をファイルにする。`fragmented` なら映像のキーフレームごとにフラグメントを切る。
pub fn write_output(specs: Vec<TrackSpec>, samples: Vec<Vec<Sample>>, fragmented: bool) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    // 全トラックを DTS（秒）の順に並べる
    let mut order: Vec<(f64, usize, usize)> = Vec::new();
    for (ti, ss) in samples.iter().enumerate() {
        let ts = specs[ti].timescale as f64;
        for (si, s) in ss.iter().enumerate() {
            order.push((s.dts as f64 / ts, ti, si));
        }
    }
    order.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let mut out = Vec::new();
    if fragmented {
        write_init_segment(&specs, &mut out)?;
        let video = specs.iter().position(|s| s.config.codec.kind() == MediaKind::Video);
        let mut fw = FragmentWriter::new();
        let mut cur: Vec<Vec<Sample>> = vec![Vec::new(); specs.len()];
        let flush = |cur: &mut Vec<Vec<Sample>>, fw: &mut FragmentWriter, out: &mut Vec<u8>| -> Result<(), Box<dyn std::error::Error>> {
            if cur.iter().all(|c| c.is_empty()) {
                return Ok(());
            }
            let runs: Vec<TrackRun<'_>> = cur.iter().enumerate().map(|(i, c)| TrackRun { track_id: specs[i].track_id, samples: c }).collect();
            fw.write(&runs, out)?;
            for c in cur.iter_mut() {
                c.clear();
            }
            Ok(())
        };
        let mut last_cut = f64::MIN;
        for (t, ti, si) in order {
            let s = &samples[ti][si];
            let cut = match video {
                Some(v) => ti == v && s.keyframe,
                None => t - last_cut >= 1.0,
            };
            if cut {
                flush(&mut cur, &mut fw, &mut out)?;
                last_cut = t;
            }
            cur[ti].push(s.clone());
        }
        flush(&mut cur, &mut fw, &mut out)?;
    } else {
        let mut w = Mp4Writer::new(specs.clone())?;
        for &(_, ti, si) in &order {
            w.add(specs[ti].track_id, &samples[ti][si])?;
        }
        out = w.finish()?;
        for &(_, ti, si) in &order {
            out.extend_from_slice(&samples[ti][si].data);
        }
    }
    Ok(out)
}
