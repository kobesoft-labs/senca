//! 今の画（`GET /streams/{id}/snapshot`）と、保存する静止画。復号は codec worker（M4）。

use crate::api::streams::ImageFormat;
use crate::app::App;
use crate::hub::HubEvent;
use bytes::Bytes;
use senca_control::{Problem, ProblemCode};
use senca_core::{CodecConfig, TrackId};
use serde_json::Value;
use std::collections::BTreeMap;

pub fn render(app: &App, _configs: &BTreeMap<TrackId, CodecConfig>, gop: &[HubEvent], _fmt: ImageFormat) -> Result<Bytes, Problem> {
    if gop.is_empty() {
        return Err(Problem::new(ProblemCode::Conflict, "まだ画がありません（キーフレーム待ち）"));
    }
    let _ = app;
    Err(Problem::new(ProblemCode::CapacityUnavailable, "映像を復号できる装置がありません").with_retryable(false))
}

fn dir(app: &App, stream: &str) -> std::path::PathBuf {
    app.cfg.data_dir.join("snapshots").join(stream)
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
    v.sort_by(|a, b| a["taken_at"].as_str().cmp(&b["taken_at"].as_str()));
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
