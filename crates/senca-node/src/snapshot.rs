//! 今の画（`GET /streams/{id}/snapshot`）と、保存する静止画（R6-8）。復号は codec worker。

use crate::api::streams::ImageFormat;
use crate::app::App;
use crate::hub::HubEvent;
use bytes::Bytes;
use senca_control::model::{CodecProfile, Stream};
use senca_control::{Problem, ProblemCode};
use senca_core::{AccessUnit, CodecConfig, MediaKind, TrackId};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// GOP の最初のキーフレームから最後の AU までを復号して、最後の画を返す。
pub fn decode_latest(
    app: &App,
    tenant: &str,
    royalty_free: bool,
    configs: &BTreeMap<TrackId, CodecConfig>,
    gop: &[HubEvent],
) -> Result<senca_image::I420Frame, Problem> {
    let Some((track, cfg)) = configs.iter().find(|(_, c)| c.codec.kind() == MediaKind::Video) else {
        return Err(Problem::new(ProblemCode::Conflict, "映像のトラックがありません"));
    };
    let aus: Vec<AccessUnit> = gop
        .iter()
        .filter_map(|e| match e {
            HubEvent::Au(a) if a.track == *track => Some(a.clone()),
            _ => None,
        })
        .skip_while(|a| !a.keyframe)
        .take(600)
        .collect();
    if aus.is_empty() {
        return Err(Problem::new(ProblemCode::Conflict, "まだ画がありません（キーフレーム待ち）"));
    }
    let codec = app.codec.as_ref().ok_or_else(|| {
        Problem::new(ProblemCode::CapacityUnavailable, "codec worker が動いていません").with_retryable(false)
    })?;
    // 最後の画が要るので全部渡す（DecodeToImage は最後の出力を返す）
    codec.decode_image(tenant, cfg, aus, royalty_free)
}

pub fn render(app: &App, s: &Stream, configs: &BTreeMap<TrackId, CodecConfig>, gop: &[HubEvent], fmt: ImageFormat) -> Result<Bytes, Problem> {
    let f = decode_latest(app, &s.tenant, s.codec_profile == CodecProfile::RoyaltyFree, configs, gop)?;
    encode(&f, fmt).ok_or_else(Problem::internal)
}

pub fn encode(f: &senca_image::I420Frame, fmt: ImageFormat) -> Option<Bytes> {
    match fmt {
        ImageFormat::Jpeg => crate::codec::jpeg(f, 90),
        ImageFormat::Png => crate::codec::png(f),
    }
}

fn dir(app: &App, stream: &str) -> std::path::PathBuf {
    app.cfg.data_dir.join("snapshots").join(stream)
}

/// 保存する（`reason`: interval / schedule / board_change）。古いものは `keep` を超えたら消す。
pub fn store(app: &App, stream: &str, img: &senca_image::I420Frame, reason: &str, keep: u32) -> Result<String, String> {
    let d = dir(app, stream);
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    let id = senca_core::Id128::random().map_err(|e| e.to_string())?.to_string();
    let jpg = encode(img, ImageFormat::Jpeg).ok_or("jpeg")?;
    crate::store::write_atomic(&d.join(format!("{id}.jpg")), &jpg).map_err(|e| e.to_string())?;
    let meta = json!({ "taken_at": crate::util::rfc3339(crate::util::now_ms()), "reason": reason, "width": img.width(), "height": img.height(), "bytes": jpg.len() });
    crate::store::write_atomic(&d.join(format!("{id}.json")), meta.to_string().as_bytes()).map_err(|e| e.to_string())?;
    // 古いものから消す
    let mut items = list(app, stream);
    while items.len() > keep as usize {
        if let Some(id) = items.first().and_then(|v| v["id"].as_str()).map(str::to_string) {
            delete(app, stream, &id);
        }
        items.remove(0);
    }
    Ok(id)
}

pub fn list(app: &App, stream: &str) -> Vec<Value> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir(app, stream)) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if let Some(id) = n.strip_suffix(".json")
                && let Ok(b) = std::fs::read(e.path())
                && let Ok(mut j) = serde_json::from_slice::<Value>(&b)
            {
                j["id"] = id.into();
                v.push(j);
            }
        }
    }
    v.sort_by(|a, b| a["taken_at"].as_str().cmp(&b["taken_at"].as_str()).then(a["id"].as_str().cmp(&b["id"].as_str())));
    v
}

fn valid_id(id: &str) -> bool {
    id.parse::<senca_core::Id128>().is_ok()
}

pub fn get(app: &App, stream: &str, id: &str) -> Option<(&'static str, Bytes)> {
    if !valid_id(id) {
        return None;
    }
    for (ext, ct) in [("jpg", "image/jpeg"), ("png", "image/png")] {
        if let Ok(b) = std::fs::read(dir(app, stream).join(format!("{id}.{ext}"))) {
            return Some((ct, Bytes::from(b)));
        }
    }
    None
}

pub fn delete(app: &App, stream: &str, id: &str) -> bool {
    if !valid_id(id) {
        return false;
    }
    let d = dir(app, stream);
    let mut found = false;
    for ext in ["jpg", "png", "json"] {
        found |= std::fs::remove_file(d.join(format!("{id}.{ext}"))).is_ok();
    }
    found
}

/// 一定間隔の保存（stream の `snapshots` 方針と、予約の snapshot）。1 秒ごとに見る。
pub async fn run_snapshotter(app: std::sync::Arc<App>) {
    let mut last: BTreeMap<String, i64> = BTreeMap::new();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        if app.shutting_down() {
            return;
        }
        let now = crate::util::now_ms();
        let mut due: Vec<(Stream, u32, &'static str)> = Vec::new();
        {
            let core = app.control.lock().unwrap();
            for s in core.state.streams.values() {
                if let Some(p) = &s.snapshots
                    && now - last.get(&s.id).copied().unwrap_or(0) >= p.interval_secs as i64 * 1000
                {
                    due.push((s.clone(), p.keep, "interval"));
                }
            }
            for (sid, (stream, interval)) in app.snapshot_schedules.lock().unwrap().iter() {
                let key = format!("sched:{sid}");
                if now - last.get(&key).copied().unwrap_or(0) >= *interval as i64 * 1000
                    && let Some(s) = core.state.streams.get(stream)
                {
                    last.insert(key, now);
                    due.push((s.clone(), 10_000, "schedule"));
                }
            }
        }
        for (s, keep, reason) in due {
            let Some(hub) = app.hubs.lock().unwrap().get(&s.id).cloned() else { continue };
            if !hub.is_live() {
                continue;
            }
            if reason == "interval" {
                last.insert(s.id.clone(), now);
            }
            let gop = hub.shared.gop.lock().unwrap().clone();
            let configs = hub.shared.configs.lock().unwrap().clone();
            let a = app.clone();
            let _ = tokio::task::spawn_blocking(move || {
                match decode_latest(&a, &s.tenant, s.codec_profile == CodecProfile::RoyaltyFree, &configs, &gop) {
                    Ok(img) => {
                        if let Err(e) = store(&a, &s.id, &img, reason, keep) {
                            a.log(&format!("snapshot store {}: {e}", s.id));
                        }
                    }
                    Err(e) => a.log(&format!("snapshot {}: {}", s.id, e.title)),
                }
            })
            .await;
        }
    }
}
