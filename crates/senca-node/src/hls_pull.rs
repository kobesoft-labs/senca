//! HLS pull の入力（仕様 7 章 HLS → 他）。プレイリストを繰り返し読み、新しいセグメントを分離して実時間で送る。
//! TS と fMP4 に対応。入力のセグメントをためる分の遅延は消せない（仕様 7 章の注記）。

use crate::app::App;
use crate::io::IoStatus;
use senca_control::model::Input;
use senca_core::{AccessUnit, Rescale, Timebase, TrackId};
use senca_hls::multivariant::Playlist;
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_PLAYLIST: usize = 1 << 20;

/// fMP4 の読み手と、トラック番号 → (時間軸, 設定)。
type Fmp4Source = (senca_fmp4::Mp4Reader, BTreeMap<u32, (u32, senca_core::CodecConfig)>);
const MAX_SEGMENT: usize = 64 << 20;

pub async fn run(app: &Arc<App>, i: &Input, st: &Mutex<IoStatus>) -> Result<(), String> {
    let allow = app.cfg.egress_allow.clone();
    let get = |url: String, max: usize| {
        let allow = allow.clone();
        async move {
            let r = crate::httpc::request(&allow, senca_http::Method::Get, &url, &[], None, max, Duration::from_secs(15)).await?;
            if r.status != 200 {
                return Err(format!("{url}: HTTP {}", r.status));
            }
            Ok::<_, String>(r.body)
        }
    };
    st.lock().unwrap().state = "connecting".into();
    // 複数の品質があれば帯域の最も大きいものを選ぶ
    let mut url = i.url.clone();
    let body = get(url.clone(), MAX_PLAYLIST).await?;
    if let Ok(Playlist::Multivariant(m)) = Playlist::parse(&body) {
        let best = m.variants.iter().max_by_key(|v| v.bandwidth).ok_or("variant がありません")?;
        url = crate::httpc::resolve(&url, &best.uri);
    }
    let mut seen: HashSet<u64> = HashSet::new();
    let mut publisher: Option<crate::hub::Publisher> = None;
    let mut demux_ts: Option<senca_mpegts::TsDemuxer> = None;
    let mut fmp4: Option<Fmp4Source> = None;
    let mut init_uri: Option<String> = None;
    // 実時間で送るための基準
    let mut clock: Option<(Instant, i64)> = None;
    let mut sent_cfg: HashSet<(TrackId, u32)> = HashSet::new();
    loop {
        let body = get(url.clone(), MAX_PLAYLIST).await?;
        let pl = match Playlist::parse(&body).map_err(|e| format!("playlist: {e:?}"))? {
            Playlist::Media(m) => *m,
            Playlist::Multivariant(_) => return Err("入れ子の multivariant playlist".into()),
        };
        let first_time = seen.is_empty();
        let n = pl.segments.len();
        for (k, s) in pl.segments.iter().enumerate() {
            let seq = pl.media_sequence + k as u64;
            // 最初はライブの端（最後の 3 本）から
            if seen.contains(&seq) || (first_time && !pl.end_list && k + 3 < n) {
                seen.insert(seq);
                continue;
            }
            seen.insert(seq);
            if let Some(m) = &s.map {
                let iu = crate::httpc::resolve(&url, &m.uri);
                if init_uri.as_ref() != Some(&iu) {
                    let b = get(iu.clone(), MAX_SEGMENT).await?;
                    let (init, _) = senca_fmp4::read_file(&b).map_err(|e| format!("init: {e:?}"))?;
                    let tracks = init.tracks.iter().filter_map(|t| Some((t.track_id, (t.timescale, t.config.clone()?)))).collect();
                    fmp4 = Some((senca_fmp4::Mp4Reader::new(init), tracks));
                    init_uri = Some(iu);
                }
            }
            let data = get(crate::httpc::resolve(&url, &s.uri), MAX_SEGMENT).await?;
            st.lock().unwrap().bytes += data.len() as u64;
            if publisher.is_none() {
                publisher = Some(crate::io::attach(app, &i.stream, "hls_pull", &url).await?);
                st.lock().unwrap().connects += 1;
                st.lock().unwrap().state = "active".into();
            }
            let p = publisher.as_ref().unwrap();
            let mut aus: Vec<AccessUnit> = Vec::new();
            let mut cfgs: Vec<(TrackId, senca_core::CodecConfig)> = Vec::new();
            if let Some((r, tracks)) = fmp4.as_mut() {
                for smp in r.read_fragments(&data).map_err(|e| format!("fmp4: {e:?}"))? {
                    let Some((ts, c)) = tracks.get(&smp.track_id) else { continue };
                    let t = TrackId(smp.track_id - 1);
                    if sent_cfg.insert((t, c.generation)) {
                        cfgs.push((t, c.clone()));
                    }
                    aus.push(AccessUnit {
                        track: t,
                        codec: c.codec,
                        epoch: p.epoch(),
                        sequence: 0,
                        config_generation: c.generation,
                        pts: smp.dts + smp.cts_offset as i64,
                        dts: smp.dts,
                        duration: smp.duration as u64,
                        timebase: Timebase::new(1, *ts),
                        keyframe: smp.keyframe,
                        discontinuity: s.discontinuity,
                        capture_time_ns: None,
                        payload: smp.data,
                    });
                }
            } else {
                let d = demux_ts.get_or_insert_with(|| senca_mpegts::TsDemuxer::new(Default::default()));
                d.push(&data);
                // HLS のセグメントは PES の境目で終わるので、最後の PES もここで出す
                d.flush();
                while let Some(ev) = d.poll() {
                    match ev {
                        senca_mpegts::DemuxEvent::Config { track, config } => cfgs.push((track, config)),
                        senca_mpegts::DemuxEvent::AccessUnit(a) => aus.push(a),
                        _ => {}
                    }
                }
            }
            for (t, c) in cfgs {
                p.config(t, c).await.map_err(|e| e.to_string())?;
            }
            aus.sort_by_key(|a| a.dts.rescale(a.timebase, Timebase::MICROS).unwrap_or(0));
            for a in aus {
                let us = a.dts.rescale(a.timebase, Timebase::MICROS).unwrap_or(0);
                let (t0, base) = *clock.get_or_insert((Instant::now(), us));
                let due = Duration::from_micros((us - base).max(0) as u64);
                let el = t0.elapsed();
                if due > el {
                    tokio::time::sleep(due - el).await;
                } else if el > due + Duration::from_secs(10) {
                    clock = Some((Instant::now(), us)); // 大きく遅れたらそろえ直す
                }
                p.access_unit(a).await.map_err(|e| e.to_string())?;
            }
        }
        if seen.len() > 10_000 {
            let min = pl.media_sequence;
            seen.retain(|s| *s >= min);
        }
        if pl.end_list {
            if let Some(p) = publisher.take() {
                p.detach("hls pull ended").await;
            }
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis((pl.target_duration * 1000 / 2).clamp(500, 5000))).await;
    }
}
