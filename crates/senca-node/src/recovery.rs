//! 起動時の回収と、録画の削除（ADR-0005）。

use crate::app::App;
use senca_control::model::{Recording, RecordingState};
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
                let r = app.control.lock().unwrap().state.recordings.get(&id).cloned();
                if let Some(mut r) = r
                    && r.state == RecordingState::Recording
                {
                    r.state = RecordingState::Interrupted;
                    r.ended_at_ms = Some(ended);
                    r.generation += 1;
                    let _ = app.propose(Command::PutRecording { recording: r });
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
        let _ = app.propose(Command::PutRecording { recording: r });
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

/// 手元の写しを消す: 索引をゴミ箱へ → 手元のどの索引からも参照されないセグメントを消す。消した索引を返す。
/// 名前を変えられなければ Err（何も消さない）。
fn remove_local(app: &App, id: &str) -> Result<Option<senca_record::Index>, ()> {
    let data = &app.cfg.data_dir;
    let dir = data.join("recordings").join(id);
    let trash = data.join("recordings").join(format!(".trash-{id}"));
    let idx = fs::read(dir.join("index.jsonl")).ok().and_then(|b| senca_record::parse(&b).ok());
    if dir.exists() && fs::rename(&dir, &trash).is_err() {
        app.log(&format!("recording {id}: rename failed"));
        return Err(());
    }
    if let Some(idx) = &idx {
        let mut mine: HashSet<String> = idx.objects().into_iter().map(|(p, _)| p.to_string()).collect();
        // 他の索引の参照を集める
        if let Ok(rd) = fs::read_dir(data.join("recordings")) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                if let Ok(b) = fs::read(e.path().join("index.jsonl")) {
                    match senca_record::parse(&b) {
                        Ok(o) => {
                            for (p, _) in o.objects() {
                                mine.remove(p);
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
    Ok(idx)
}

/// 録画を消す: 保存先 → 手元 → 制御状態の順。保存先で失敗したら何も消さずに残し、後でやり直す
/// （制御状態は削除中のまま。`retention_sweep` が拾う）。
pub fn delete_recording(app: &App, id: &str) {
    if !app.deleting.lock().unwrap().insert(id.to_string()) {
        return; // 消している途中
    }
    delete_inner(app, id);
    app.deleting.lock().unwrap().remove(id);
}

fn delete_inner(app: &App, id: &str) {
    let rec = app.control.lock().unwrap().state.recordings.get(id).cloned();
    if let Some(r) = &rec
        && r.storage.is_some()
        && let Err(e) = delete_remote(app, r)
    {
        app.log(&format!("delete recording {id}: storage: {e} (will retry)"));
        return;
    }
    if remove_local(app, id).is_err() {
        return;
    }
    if let Some(r) = rec {
        let _ = app.propose(Command::DeleteRecording { tenant: r.tenant, id: id.into() });
    }
    app.rec_info.lock().unwrap().remove(id);
    if let Some(st) = app.storage.get() {
        st.forget_index(id);
    }
}

/// 保存先の録画を消す: どの録画からも参照されなくなったセグメント → 録画の置き場所の下（サムネイル・書き出し）→ 索引。
fn delete_remote(app: &App, rec: &Recording) -> Result<(), String> {
    let st = app.storage.get().ok_or("このノードには保存先の設定がありません")?;
    let uri = &rec.storage.as_ref().ok_or("no storage")?.uri;
    let loc = senca_s3::Location::parse(uri)?;
    let rt = app.rt.get().ok_or("no runtime")?;
    crate::util::block_on(rt, async {
        let keys = |idx: &senca_record::Index| -> Vec<String> {
            idx.objects().into_iter().map(|(p, k)| k.map(str::to_string).unwrap_or_else(|| loc.key(p))).collect::<Vec<_>>()
        };
        let mut mine: HashSet<String> = match app.index_async(&rec.id).await {
            Some(idx) => keys(&idx).into_iter().collect(),
            None => HashSet::new(), // 前の試みで索引まで消えた
        };
        // 同じ stream で時間の重なる録画は同じセグメントを参照していることがある
        let end = rec.ended_at_ms.unwrap_or(i64::MAX);
        let others: Vec<Recording> = {
            let core = app.control.lock().unwrap();
            core.state
                .recordings
                .values()
                .filter(|o| {
                    o.id != rec.id
                        && o.stream == rec.stream
                        && o.state != RecordingState::Deleting
                        && o.started_at_ms <= end
                        && o.ended_at_ms.unwrap_or(i64::MAX) >= rec.started_at_ms
                })
                .cloned()
                .collect()
        };
        for o in others {
            let local = app.cfg.data_dir.join("recordings").join(&o.id).join("index.jsonl").exists();
            if !local && o.storage.is_none() {
                continue; // 別のノードの録画（セグメントの置き場所が違うので共有しない）
            }
            let idx = app.index_async(&o.id).await.ok_or_else(|| format!("recording {} の索引が読めません", o.id))?;
            for k in keys(&idx) {
                mine.remove(&k);
            }
        }
        for k in &mine {
            st.client.delete_object(&loc.bucket, k).await.map_err(|e| e.to_string())?;
        }
        let mut objs = st.client.list_all(&loc.bucket, &loc.key(&format!("recordings/{}/", rec.id)), 10_000).await.map_err(|e| e.to_string())?;
        objs.sort_by_key(|o| o.key.ends_with("/index.jsonl"));
        for o in &objs {
            st.client.delete_object(&loc.bucket, &o.key).await.map_err(|e| e.to_string())?;
        }
        app.log(&format!("recording {}: deleted {} objects from {uri}", rec.id, mine.len() + objs.len()));
        Ok(())
    })
}

/// 保存先に上げ終わった録画の手元の写しを消す（保存先に無いものは消さない）。
pub fn evict_local(app: &App, id: &str) {
    let ok = app.control.lock().unwrap().state.recordings.get(id).is_some_and(|r| r.storage.is_some() && r.state != RecordingState::Deleting);
    if !ok || app.deleting.lock().unwrap().contains(id) {
        return;
    }
    if remove_local(app, id).is_ok() {
        app.log(&format!("recording {id}: local copy removed (kept in storage)"));
    }
}

/// 手元の写しを残す期間を過ぎたものを消す。
pub fn evict_sweep(app: &App) {
    let Some(st) = app.storage.get() else { return };
    let now = crate::util::now_ms();
    let keep = st.keep_local_secs.saturating_mul(1000) as i64;
    let due: Vec<String> = {
        let core = app.control.lock().unwrap();
        core.state
            .recordings
            .values()
            .filter(|r| owned(app, r) && r.storage.as_ref().is_some_and(|s| s.archived_at_ms.saturating_add(keep) <= now))
            .map(|r| r.id.clone())
            .collect()
    };
    for id in due {
        if app.cfg.data_dir.join("recordings").join(&id).exists() {
            evict_local(app, &id);
        }
    }
}

/// 手元のファイルを持つのがこのノードか（単一ノードなら常に）。
fn owned(app: &App, r: &Recording) -> bool {
    app.cluster.get().is_none() || r.node.as_deref().is_none_or(|n| n == app.node_id)
}

fn is_safe_rel(p: &str) -> bool {
    p.starts_with("segments/") && !Path::new(p).components().any(|c| !matches!(c, std::path::Component::Normal(_)))
}

/// 保持期限を過ぎた録画を消す（法的保全中は消さない）。分散では手元のファイルを持つノードが消す。
/// 削除の途中で止まったもの（保存先の失敗など）もやり直す。
pub fn retention_sweep(app: &App) {
    let now = crate::util::now_ms();
    let (expired, stuck): (Vec<String>, Vec<String>) = {
        let core = app.control.lock().unwrap();
        let expired = core
            .state
            .recordings
            .values()
            .filter(|r| {
                owned(app, r)
                    && !r.legal_hold
                    && matches!(r.state, RecordingState::Finalized | RecordingState::Interrupted)
                    && r.retention_until_ms.is_some_and(|t| t <= now)
            })
            .map(|r| r.id.clone())
            .collect();
        let stuck = core.state.recordings.values().filter(|r| owned(app, r) && r.state == RecordingState::Deleting).map(|r| r.id.clone()).collect();
        (expired, stuck)
    };
    for id in expired {
        let r = app.control.lock().unwrap().state.recordings.get(&id).cloned();
        if let Some(r) = r {
            app.log(&format!("retention: deleting recording {id}"));
            let _ = app.delete_recording(&r);
        }
    }
    for id in stuck {
        if !app.deleting.lock().unwrap().contains(&id) {
            delete_recording(app, &id);
        }
    }
    crate::archive::resume(app);
    evict_sweep(app);
}
