//! 録画のサムネイル（R6-9）。録画の確定後に、セグメントからキーフレームを最大 12 枚等間隔に選んで復号し、
//! 黒・白・平坦・ぶれを避けていちばん良いものを `thumbnail.jpg`（＋`thumbnail.json`）にする。
//! 復号できる装置が無ければ作らない（H.264 はハードウェアだけ）。

use crate::app::App;
use senca_core::{AccessUnit, CodecConfig, MediaKind, TrackId};
use std::collections::BTreeMap;

const CANDIDATES: usize = 12;

/// fMP4 の読み手と、トラック番号 → (時間軸, 設定)。
type Fmp4Source = (senca_fmp4::Mp4Reader, BTreeMap<u32, (u32, CodecConfig)>);

pub fn generate(app: &App, recording: &str) {
    let Some(codec) = app.codec.as_ref() else { return };
    let Some(idx) = app.read_index(recording) else { return };
    if idx.segments.is_empty() {
        return;
    }
    // fMP4 は初期化セグメントからトラックの設定を読む
    let mut fmp4: Option<Fmp4Source> = None;
    if idx.container == "fmp4" {
        let Some((p, _)) = &idx.init else { return };
        let Ok(b) = std::fs::read(app.cfg.data_dir.join(p)) else { return };
        let Ok((init, _)) = senca_fmp4::read_file(&bytes::Bytes::from(b)) else { return };
        let tracks = init.tracks.iter().filter_map(|t| Some((t.track_id, (t.timescale, t.config.clone()?)))).collect();
        fmp4 = Some((senca_fmp4::Mp4Reader::new(init), tracks));
    }
    let (tenant, e2ee) = {
        let core = app.control.lock().unwrap();
        let r = core.state.recordings.get(recording);
        let e2ee = r.and_then(|r| core.state.streams.get(&r.stream)).is_some_and(|s| crate::api::streams::stream_is_e2ee(&core.state, s));
        (r.map(|r| r.tenant.clone()), e2ee)
    };
    let Some(tenant) = tenant else { return };
    // E2EE の部屋から書き出した録画は復号できない（サムネイルを作らない）
    if e2ee {
        return;
    }
    let n = idx.segments.len();
    let step = (n as f64 / CANDIDATES as f64).max(1.0);
    let mut picks: Vec<usize> = (0..CANDIDATES.min(n)).map(|i| ((i as f64 + 0.5) * step) as usize).filter(|i| *i < n).collect();
    picks.dedup();
    let mut sel = senca_image::thumb::ThumbnailSelector::new(Default::default(), senca_image::ColorRange::Limited);
    let mut frames = Vec::new();
    for (k, i) in picks.into_iter().enumerate() {
        let Ok(b) = std::fs::read(app.cfg.data_dir.join(&idx.segments[i].path)) else { continue };
        let mut configs: BTreeMap<TrackId, CodecConfig> = BTreeMap::new();
        let mut key: Option<AccessUnit> = None;
        if let Some((reader, tracks)) = fmp4.as_mut() {
            let Ok(samples) = reader.read_fragments(&bytes::Bytes::from(b)) else { continue };
            for smp in samples {
                let Some((ts, c)) = tracks.get(&smp.track_id) else { continue };
                if c.codec.kind() != MediaKind::Video || !smp.keyframe {
                    continue;
                }
                let t = TrackId(smp.track_id);
                configs.insert(t, c.clone());
                key = Some(AccessUnit {
                    track: t,
                    codec: c.codec,
                    epoch: 0,
                    sequence: 0,
                    config_generation: c.generation,
                    pts: smp.dts + smp.cts_offset as i64,
                    dts: smp.dts,
                    duration: smp.duration as u64,
                    timebase: senca_core::Timebase::new(1, *ts),
                    keyframe: true,
                    discontinuity: false,
                    capture_time_ns: None,
                    payload: smp.data,
                });
                break;
            }
        } else {
            let mut d = senca_mpegts::TsDemuxer::new(Default::default());
            d.push(&b);
            d.flush();
            while let Some(ev) = d.poll() {
                match ev {
                    senca_mpegts::DemuxEvent::Config { track, config } => {
                        configs.insert(track, config);
                    }
                    senca_mpegts::DemuxEvent::AccessUnit(a) if a.codec.kind() == MediaKind::Video && a.keyframe && key.is_none() => key = Some(a),
                    _ => {}
                }
            }
        }
        let Some(au) = key else { continue };
        let Some(cfg) = configs.get(&au.track) else { continue };
        match codec.decode_image(&tenant, cfg, vec![au], false) {
            Ok(img) => {
                if sel.push(k as u64, img.y()).is_ok() {
                    frames.push((k as u64, img));
                }
            }
            Err(e) => {
                app.log(&format!("thumbnail {recording}: {} {}", e.title, e.details));
                return;
            }
        }
    }
    let Some(best) = sel.best() else { return };
    let Some((_, img)) = frames.iter().find(|(k, _)| *k == best) else { return };
    let Some(jpg) = crate::codec::jpeg(img, 85) else { return };
    let dir = app.cfg.data_dir.join("recordings").join(recording);
    if !dir.exists() {
        return; // 削除された
    }
    if crate::store::write_atomic(&dir.join("thumbnail.jpg"), &jpg).is_ok() {
        let meta = serde_json::json!({ "width": img.width(), "height": img.height(), "candidate": best });
        let _ = crate::store::write_atomic(&dir.join("thumbnail.json"), meta.to_string().as_bytes());
        if let Some(i) = app.rec_info.lock().unwrap().get_mut(recording) {
            i.has_thumbnail = true;
        }
    }
}
