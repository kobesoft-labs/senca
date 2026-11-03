//! 録画を保存先（S3 互換）へ上げる（ADR-0005）。
//!
//! - 書き手が不変のファイル（セグメント・初期化セグメント）を書き終えたら、すぐに上げ始める（[`Archiver::object`]）。
//! - 録画が確定したら（サムネイルを作った後）、残りを確かめて上げ、キーを付けた索引・サムネイルを上げ、
//!   手元の索引もキー付きに書き直し、制御状態に保存先を記録する（[`Archiver::finalize`]）。
//! - 失敗したら間を空けてやり直す。起動時には、まだ上がっていない確定済みの録画を拾う。
//! - 上げ終わってから `keep_local_secs` 経った手元の写しは消す（`recovery::evict_sweep`）。
//!
//! 同じオブジェクトを 2 度上げても中身は同じ（キーと中身が 1 対 1）なので、やり直しは安全。

use crate::app::App;
use bytes::Bytes;
use senca_control::model::{RecordingState, RecordingStorage};
use senca_control::state::Command;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};

/// 同時に上げる数。
const CONCURRENCY: usize = 4;
const QUEUE: usize = 65_536;
/// 上げたことを覚えておく数（多すぎたら忘れる。忘れても HEAD で確かめるだけ）。
const REMEMBER: usize = 200_000;

enum Job {
    Object(String),
    Finalize { id: String, attempt: u32 },
}

pub struct Archiver {
    tx: mpsc::Sender<Job>,
    uploaded: Mutex<HashSet<String>>,
    /// 仕上げを待っている・やり直しを待っている録画（1 つの録画に流れは 1 本だけ）。
    queued: Mutex<HashSet<String>>,
    /// 専用のスレッド（サムネイルの後など）からも仕事を出すため。
    rt: tokio::runtime::Handle,
}

impl Archiver {
    /// 不変のファイルを上げる（待たない。あふれたら確定のときに上げる）。
    pub fn object(&self, rel: String) {
        let _ = self.tx.try_send(Job::Object(rel));
    }

    /// 確定した録画を仕上げる（同じ録画がもう待っていれば何もしない）。
    pub fn finalize(&self, id: &str) {
        if !self.queued.lock().unwrap().insert(id.to_string()) {
            return;
        }
        if self.tx.try_send(Job::Finalize { id: id.to_string(), attempt: 0 }).is_err() {
            let tx = self.tx.clone();
            let id = id.to_string();
            self.rt.spawn(async move {
                let _ = tx.send(Job::Finalize { id, attempt: 0 }).await;
            });
        }
    }

    fn remember(&self, rel: &str) {
        let mut u = self.uploaded.lock().unwrap();
        if u.len() >= REMEMBER {
            u.clear();
        }
        u.insert(rel.to_string());
    }
}

/// 保存先が設定されていれば上げ手を動かす。
pub fn start(app: &Arc<App>) {
    if app.storage.get().is_none() {
        return;
    }
    let (tx, mut rx) = mpsc::channel(QUEUE);
    let a = Arc::new(Archiver {
        tx,
        uploaded: Mutex::new(HashSet::new()),
        queued: Mutex::new(HashSet::new()),
        rt: tokio::runtime::Handle::current(),
    });
    if app.archiver.set(a.clone()).is_err() {
        return;
    }
    let app = app.clone();
    tokio::spawn(async move {
        let sem = Arc::new(Semaphore::new(CONCURRENCY));
        while let Some(job) = rx.recv().await {
            let Ok(permit) = sem.clone().acquire_owned().await else { return };
            let (app, a) = (app.clone(), a.clone());
            tokio::spawn(async move {
                match job {
                    Job::Object(rel) => {
                        if let Err(e) = upload_object(&app, &a, &rel, None).await {
                            // 確定のときにもう一度確かめる
                            app.log(&format!("archive: {rel}: {e}"));
                        }
                    }
                    Job::Finalize { id, attempt } => {
                        let r = finalize(&app, &a, &id).await;
                        if r.is_ok() {
                            a.queued.lock().unwrap().remove(&id);
                        }
                        if let Err(e) = r {
                            let wait = Duration::from_secs((5u64 << attempt.min(6)).min(300));
                            app.log(&format!("archive: recording {id}: {e} (retry in {}s)", wait.as_secs()));
                            let tx = a.tx.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(wait).await;
                                let _ = tx.send(Job::Finalize { id, attempt: attempt + 1 }).await;
                            });
                        }
                    }
                }
                drop(permit);
            });
        }
    });
}

/// 起動時と見回りのたびに: このノードの確定済みで、まだ上がっていない録画を拾う
/// （分散では起動の時点で制御状態がまだ届いていないことがある）。
pub fn resume(app: &App) {
    let Some(a) = app.archiver.get() else { return };
    let ids: Vec<String> = {
        let core = app.control.lock().unwrap();
        core.state
            .recordings
            .values()
            .filter(|r| {
                matches!(r.state, RecordingState::Finalized | RecordingState::Interrupted)
                    && r.storage.is_none()
                    && r.node.as_deref().is_none_or(|n| n == app.node_id)
            })
            .map(|r| r.id.clone())
            .collect()
    };
    for id in ids {
        if app.cfg.data_dir.join("recordings").join(&id).join("index.jsonl").exists() {
            a.finalize(&id);
        }
    }
}

/// 1 つのファイルを上げる。`size` が分かっていて、保存先に同じ大きさのものがあれば上げない。
async fn upload_object(app: &App, a: &Archiver, rel: &str, key: Option<&str>) -> Result<(), String> {
    let Some(st) = app.storage.get() else { return Ok(()) };
    if !crate::storage::safe_rel(rel) || a.uploaded.lock().unwrap().contains(rel) {
        return Ok(());
    }
    let path = app.cfg.data_dir.join(rel);
    let body = match tokio::fs::read(&path).await {
        Ok(b) => Bytes::from(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("local file is missing".into()),
        Err(e) => return Err(e.to_string()),
    };
    let key = key.map(str::to_string).unwrap_or_else(|| st.location.key(rel));
    // 前に上げた分（再起動の前など）は大きさが同じなら上げ直さない
    if st.client.head_object(&st.location.bucket, &key).await.map_err(|e| e.to_string())? == Some(body.len() as u64) {
        a.remember(rel);
        return Ok(());
    }
    st.put(&st.location, &key, body, crate::storage::content_type(rel)).await?;
    a.remember(rel);
    Ok(())
}

async fn finalize(app: &Arc<App>, a: &Archiver, id: &str) -> Result<(), String> {
    let Some(st) = app.storage.get() else { return Ok(()) };
    let rec = app.control.lock().unwrap().state.recordings.get(id).cloned();
    let Some(rec) = rec else { return Ok(()) }; // 消された
    if !matches!(rec.state, RecordingState::Finalized | RecordingState::Interrupted) || rec.storage.is_some() {
        return Ok(());
    }
    let dir = app.cfg.data_dir.join("recordings").join(id);
    let data = match tokio::fs::read(dir.join("index.jsonl")).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    let loc = st.location.clone();
    let keyed = senca_record::with_keys(&data, |p| loc.key(p)).map_err(|e| e.to_string())?;
    let idx = senca_record::parse(&keyed).map_err(|e| e.to_string())?;
    let mut missing = 0usize;
    for (rel, key) in idx.objects() {
        match upload_object(app, a, rel, key).await {
            Ok(()) => {}
            // 手元に無いもの（壊れて隔離されたなど）は飛ばす。再生では欠けとして扱われる
            Err(e) if e == "local file is missing" => missing += 1,
            Err(e) => return Err(format!("{rel}: {e}")),
        }
    }
    if missing > 0 {
        app.log(&format!("archive: recording {id}: {missing} files were missing locally"));
    }
    let mut thumbnail = false;
    for name in ["thumbnail.jpg", "thumbnail.json"] {
        if let Ok(b) = tokio::fs::read(dir.join(name)).await {
            let rel = format!("recordings/{id}/{name}");
            st.put(&loc, &loc.key(&rel), Bytes::from(b), crate::storage::content_type(&rel)).await?;
            thumbnail |= name == "thumbnail.jpg";
        }
    }
    // 索引は最後に上げる（索引が見えれば、参照するものは揃っている）
    let rel = format!("recordings/{id}/index.jsonl");
    st.put(&loc, &loc.key(&rel), Bytes::from(keyed.clone()), "application/x-ndjson").await?;
    // 手元の索引もキー付きにする（確定した索引は変わらないので置き換えてよい）
    if keyed != data {
        let path = dir.join("index.jsonl");
        tokio::task::spawn_blocking(move || crate::store::write_atomic(&path, &keyed)).await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    }
    let storage = RecordingStorage { uri: loc.uri(), archived_at_ms: crate::util::now_ms(), thumbnail };
    let me = app.clone();
    let rid = id.to_string();
    tokio::task::spawn_blocking(move || me.propose(Command::SetRecordingStorage { id: rid, storage: Some(storage) }))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    app.log(&format!("archive: recording {id} stored in {} ({} objects)", loc.uri(), idx.objects().len()));
    if st.keep_local_secs == 0 {
        let me = app.clone();
        let rid = id.to_string();
        let _ = tokio::task::spawn_blocking(move || crate::recovery::evict_local(&me, &rid)).await;
    }
    Ok(())
}

/// 書き出し（MP4）を保存先に置く（次からはそれを返す）。
pub fn store_export(app: &Arc<App>, id: &str, body: Bytes) {
    let Some((_, loc)) = app.remote_of(id) else { return };
    let Some(rt) = app.rt.get() else { return };
    let me = app.clone();
    let id = id.to_string();
    rt.spawn(async move {
        let Some(st) = me.storage.get() else { return };
        let key = loc.key(&format!("recordings/{id}/export.mp4"));
        match st.put(&loc, &key, body, "video/mp4").await {
            Ok(()) => me.log(&format!("archive: export of {id} stored")),
            Err(e) => me.log(&format!("archive: export of {id}: {e}")),
        }
    });
}
