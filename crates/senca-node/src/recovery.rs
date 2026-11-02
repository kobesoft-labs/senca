//! 起動時の回収と、録画の削除（ADR-0005）。

use crate::app::App;
use senca_control::model::RecordingState;
use senca_control::state::Command;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// 起動時: 終わっていない索引を閉じ、制御状態の録画をそろえ、参照されないセグメントを隔離する。
pub fn recover(app: &App) {
    let data = &app.cfg.data_dir;
    let recs_dir = data.join("recordings");
    let _ = fs::create_dir_all(&recs_dir);
    // 削除の途中だったもの
    let deleting: Vec<String> = {
        let core = app.control.lock().unwrap();
        core.state.recordings.values().filter(|r| r.state == RecordingState::Deleting).map(|r| r.id.clone()).collect()
    };
    for id in deleting {
        delete_recording(app, &id);
    }
    let mut referenced: HashSet<String> = HashSet::new();
    let mut unreadable = false;
    if let Ok(rd) = fs::read_dir(&recs_dir) {
        for e in rd.flatten() {
            let id = e.file_name().to_string_lossy().to_string();
            if id.starts_with('.') {
                continue;
            }
            let path = e.path().join("index.jsonl");
            let Ok(bytes) = fs::read(&path) else { continue };
            let idx = match senca_record::parse(&bytes) {
                Ok(i) => i,
                Err(err) => {
                    // 理解できない索引は触らない。セグメントの隔離もしない（参照を失うので）
                    app.log(&format!("recovery: recording {id}: {err}; left untouched"));
                    unreadable = true;
                    continue;
                }
            };
            for s in &idx.segments {
                referenced.insert(s.path.clone());
            }
            if let Some((p, _)) = &idx.init {
                referenced.insert(p.clone());
            }
            if app.cfg.verify_recordings {
                for s in &idx.segments {
                    let ok = fs::read(data.join(&s.path))
                        .map(|b| senca_control::secret::hex(&<sha2::Sha256 as sha2::Digest>::digest(&b)) == s.sha256)
                        .unwrap_or(false);
                    if !ok {
                        app.log(&format!("recovery: recording {id}: segment {} failed verification", s.path));
                    }
                }
            }
            if idx.end.is_none() {
                // 書きかけを切り詰めて interrupted で閉じる
                if idx.truncated_tail
                    && let Ok(f) = fs::OpenOptions::new().write(true).open(&path)
                {
                    let _ = f.set_len(idx.valid_len as u64);
                }
                let ended = idx.segments.last().map(|s| s.wall_ms + s.duration_ms as i64).unwrap_or(idx.started_at_ms);
                let line = senca_record::encode(&senca_record::Entry::End {
                    ended_at_ms: ended,
                    state: "interrupted".into(),
                    not_after_ms: None,
                });
                if let Ok(mut f) = fs::OpenOptions::new().append(true).open(&path) {
                    use std::io::Write;
                    let _ = f.write_all(&line).and_then(|_| f.sync_all());
                }
                app.log(&format!("recovery: recording {id} closed as interrupted ({} segments)", idx.segments.len()));
                let mut core = app.control.lock().unwrap();
                if let Some(mut r) = core.state.recordings.get(&id).cloned()
                    && r.state == RecordingState::Recording
                {
                    r.state = RecordingState::Interrupted;
                    r.ended_at_ms = Some(ended);
                    r.generation += 1;
                    let _ = core.propose(Command::PutRecording { recording: r });
                }
            }
        }
    }
    // 制御状態で Recording のままで索引が無いもの
    let stale: Vec<_> = {
        let core = app.control.lock().unwrap();
        core.state
            .recordings
            .values()
            .filter(|r| r.state == RecordingState::Recording && r.node.as_deref().is_none_or(|n| n == app.node_id))
            .cloned()
            .collect()
    };
    for mut r in stale {
        r.state = RecordingState::Interrupted;
        r.ended_at_ms.get_or_insert(r.started_at_ms);
        r.generation += 1;
        let _ = app.control.lock().unwrap().propose(Command::PutRecording { recording: r });
    }
    if !unreadable {
        quarantine_orphans(app, &referenced);
    }
}

/// 索引から参照されていないセグメントを orphans/ に移す（消さない）。
fn quarantine_orphans(app: &App, referenced: &HashSet<String>) {
    let seg_root = app.cfg.data_dir.join("segments");
    let Ok(streams) = fs::read_dir(&seg_root) else { return };
    for s in streams.flatten() {
        let Ok(runs) = fs::read_dir(s.path()) else { continue };
        for r in runs.flatten() {
            let run = r.file_name().to_string_lossy().to_string();
            if run == "orphans" {
                continue;
            }
            let Ok(files) = fs::read_dir(r.path()) else { continue };
            for f in files.flatten() {
                let name = f.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    let _ = fs::remove_file(f.path()); // 一時ファイル
                    continue;
                }
                let rel = format!("segments/{}/{run}/{name}", s.file_name().to_string_lossy());
                if !referenced.contains(&rel) {
                    let dst = s.path().join("orphans").join(&run);
                    let _ = fs::create_dir_all(&dst);
                    let _ = fs::rename(f.path(), dst.join(&name));
                }
            }
            let _ = fs::remove_dir(r.path()); // 空なら消える
        }
    }
}

/// 録画を消す: 索引をゴミ箱へ → どこからも参照されないセグメントを消す → 制御状態から消す。
pub fn delete_recording(app: &App, id: &str) {
    let data = &app.cfg.data_dir;
    let dir = data.join("recordings").join(id);
    let trash = data.join("recordings").join(format!(".trash-{id}"));
    let idx = fs::read(dir.join("index.jsonl")).ok().and_then(|b| senca_record::parse(&b).ok());
    if dir.exists() && fs::rename(&dir, &trash).is_err() {
        app.log(&format!("delete recording {id}: rename failed"));
        return;
    }
    if let Some(idx) = idx {
        let mut mine: HashSet<String> = idx.segments.iter().map(|s| s.path.clone()).collect();
        if let Some((p, _)) = idx.init {
            mine.insert(p);
        }
        // 他の索引の参照を集める
        if let Ok(rd) = fs::read_dir(data.join("recordings")) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                if let Ok(b) = fs::read(e.path().join("index.jsonl")) {
                    match senca_record::parse(&b) {
                        Ok(o) => {
                            for s in o.segments {
                                mine.remove(&s.path);
                            }
                            if let Some((p, _)) = o.init {
                                mine.remove(&p);
                            }
                        }
                        Err(_) => {
                            // 読めない索引があれば消さない（安全側）
                            mine.clear();
                            break;
                        }
                    }
                }
            }
        }
        for p in mine {
            if is_safe_rel(&p) {
                let _ = fs::remove_file(data.join(&p));
            }
        }
    }
    let _ = fs::remove_dir_all(&trash);
    let tenant = app.control.lock().unwrap().state.recordings.get(id).map(|r| r.tenant.clone());
    if let Some(t) = tenant {
        let _ = app.control.lock().unwrap().propose(Command::DeleteRecording { tenant: t, id: id.into() });
    }
    app.rec_info.lock().unwrap().remove(id);
}

fn is_safe_rel(p: &str) -> bool {
    p.starts_with("segments/") && !Path::new(p).components().any(|c| !matches!(c, std::path::Component::Normal(_)))
}

/// 保持期限を過ぎた録画を消す（法的保全中は消さない）。
pub fn retention_sweep(app: &App) {
    let now = crate::util::now_ms();
    let expired: Vec<String> = {
        let core = app.control.lock().unwrap();
        core.state
            .recordings
            .values()
            .filter(|r| {
                !r.legal_hold
                    && matches!(r.state, RecordingState::Finalized | RecordingState::Interrupted)
                    && r.retention_until_ms.is_some_and(|t| t <= now)
            })
            .map(|r| r.id.clone())
            .collect()
    };
    for id in expired {
        let r = app.control.lock().unwrap().state.recordings.get(&id).cloned();
        if let Some(r) = r {
            app.log(&format!("retention: deleting recording {id}"));
            let _ = app.delete_recording(&r);
        }
    }
}
