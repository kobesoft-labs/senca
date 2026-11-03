//! 録画の書き手（ADR-0005）。stream ごとに 1 つのタスク。
//!
//! stream タスクが作ったセグメント（ライブと同じバイト列）を受け取り、
//! 1. 誰かが録画していれば `segments/{stream}/{run}/{name}` に不変ファイルとして書き（一時名 → fsync → rename）、
//! 2. つながっている録画それぞれの `recordings/{id}/index.jsonl` に 1 行追記して fsync する。
//!
//! 順番の約束: セグメント → 索引。索引にあるセグメントは常に完全。
//! 録画の開始・停止もセグメントと同じキューで届くので、境目はセグメント単位で決まる。
//! キューはあふれたら落とす（ライブを止めない。仕様 5.3）。落とした数は次のセグメントの前に欠落として記録する。

use bytes::Bytes;
use senca_record::{Entry, SegmentRef};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub enum RecMsg {
    /// 完成したセグメント。
    Segment(SegmentData),
    /// fMP4 の初期化セグメントが変わった。
    Init { bytes: Bytes, name: String },
    Start {
        id: String,
        container: String,
        not_before_ms: Option<i64>,
        started_at_ms: i64,
        reply: Option<oneshot::Sender<Result<(), String>>>,
    },
    Stop { id: String, ended_at_ms: i64, not_after_ms: Option<i64>, state: &'static str, reply: Option<oneshot::Sender<()>> },
    /// 欠落（入力の切断など）。
    /// `mark_next`: 次のセグメントに不連続の印を付ける（パッケージャーが新しい区切りで始め直したとき。
    /// 猶予内の再接続ではパッケージャーが自分で印を付けるので false）。
    Gap { at_ms: i64, reason: String, mark_next: bool },
    /// すべて止めて終わる（ノードの終了）。
    Shutdown { reply: oneshot::Sender<()> },
}

#[derive(Debug, Clone)]
pub struct SegmentData {
    pub name: String,
    pub epoch: u64,
    pub sequence: u64,
    pub duration_ms: u32,
    pub wall_ms: i64,
    pub discontinuity: bool,
    pub bytes: Bytes,
}

/// 録画の書き手に送る口。あふれたら落として数える。
#[derive(Clone)]
pub struct RecorderTx {
    tx: mpsc::Sender<RecMsg>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl RecorderTx {
    /// セグメントを送る。満杯なら落として数える（待たない）。
    pub fn segment(&self, s: SegmentData) {
        use std::sync::atomic::Ordering;
        if self.tx.try_send(RecMsg::Segment(s)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 制御のメッセージは待って送る（落とさない）。
    pub async fn control(&self, m: RecMsg) -> bool {
        self.tx.send(m).await.is_ok()
    }

    pub fn try_control(&self, m: RecMsg) -> bool {
        self.tx.try_send(m).is_ok()
    }
}

struct Active {
    file: File,
    segments: u64,
    /// 欠落の後: 次のセグメントに不連続の印を付ける。
    disc_next: bool,
}

pub struct Recorder {
    data_dir: PathBuf,
    stream: String,
    run: String,
    run_dir_ready: bool,
    active: BTreeMap<String, Active>,
    init: Option<(String, String)>,
    pending_init: Option<(Bytes, String)>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    on_event: Box<dyn Fn(RecorderEvent) + Send>,
}

/// 書き手からの通知（録画の情報の更新に使う）。
#[derive(Debug, Clone)]
pub enum RecorderEvent {
    Appended { recording: String, segment: SegmentRef },
    /// 欠落を索引に書いた（録画中の `gaps` に出す）。
    Gap { recording: String, at_ms: i64, reason: String, dropped: u64 },
    Stopped { recording: String, ended_at_ms: i64, state: &'static str },
    Failed { recording: String, error: String },
}

pub fn spawn(
    data_dir: PathBuf,
    stream: String,
    queue: usize,
    on_event: Box<dyn Fn(RecorderEvent) + Send>,
) -> RecorderTx {
    let (tx, mut rx) = mpsc::channel(queue);
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let run = senca_core::Id128::random().map(|i| i.to_string()).unwrap_or_else(|_| format!("{}", crate::util::now_ms()));
    let mut r = Recorder {
        data_dir,
        stream,
        run,
        run_dir_ready: false,
        active: BTreeMap::new(),
        init: None,
        pending_init: None,
        dropped: dropped.clone(),
        on_event,
    };
    // ファイルの書き込みはブロックするので、専用のスレッドで回す（tokio のワーカーを止めない）。
    std::thread::Builder::new()
        .name("senca-recorder".into())
        .spawn(move || {
            while let Some(m) = rx.blocking_recv() {
                if let RecMsg::Shutdown { reply } = m {
                    r.stop_all(crate::util::now_ms(), "finalized");
                    let _ = reply.send(());
                    return;
                }
                r.handle(m);
            }
            r.stop_all(crate::util::now_ms(), "finalized");
        })
        .expect("spawn recorder thread");
    RecorderTx { tx, dropped }
}

fn sha256_hex(b: &[u8]) -> String {
    senca_control::secret::hex(&Sha256::digest(b))
}

pub fn write_file_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    crate::store::write_atomic(path, data)
}

impl Recorder {
    fn run_dir(&self) -> PathBuf {
        self.data_dir.join("segments").join(&self.stream).join(&self.run)
    }

    fn rel(&self, name: &str) -> String {
        format!("segments/{}/{}/{}", self.stream, self.run, name)
    }

    fn handle(&mut self, m: RecMsg) {
        use std::sync::atomic::Ordering;
        match m {
            RecMsg::Segment(s) => {
                let dropped = self.dropped.swap(0, Ordering::Relaxed);
                if dropped > 0 {
                    self.append_all(&Entry::Gap { at_ms: s.wall_ms, reason: "queue_overflow".into(), dropped }, true);
                }
                if self.active.is_empty() {
                    return; // 誰も欲しがらないセグメントは書かない
                }
                if let Err(e) = self.ensure_init() {
                    self.fail_all(&format!("init: {e}"));
                    return;
                }
                let path = self.run_dir().join(&s.name);
                if let Err(e) = write_file_atomic(&path, &s.bytes) {
                    self.fail_all(&format!("segment write: {e}"));
                    return;
                }
                let seg = SegmentRef {
                    path: self.rel(&s.name),
                    epoch: s.epoch,
                    sequence: s.sequence,
                    duration_ms: s.duration_ms,
                    bytes: s.bytes.len() as u64,
                    sha256: sha256_hex(&s.bytes),
                    wall_ms: s.wall_ms,
                    discontinuity: s.discontinuity,
                };
                let e = Entry::Segment(seg.clone());
                let ids: Vec<String> = self.active.keys().cloned().collect();
                for id in ids {
                    let ok = {
                        let a = self.active.get_mut(&id).unwrap();
                        // 最初のセグメントは不連続の印を持たない。欠落の後のセグメントには必ず付ける
                        // （配信が終わってから送り手が戻ると、パッケージャーは新しい区切りで始めるので印が無い）
                        let entry = if a.segments == 0 || (a.disc_next && !seg.discontinuity) {
                            let mut s2 = seg.clone();
                            s2.discontinuity = a.segments > 0;
                            Entry::Segment(s2)
                        } else {
                            e.clone()
                        };
                        a.segments += 1;
                        a.disc_next = false;
                        append(&mut a.file, &entry)
                    };
                    match ok {
                        Ok(()) => (self.on_event)(RecorderEvent::Appended { recording: id, segment: seg.clone() }),
                        Err(err) => self.fail(&id, &format!("index append: {err}")),
                    }
                }
            }
            RecMsg::Init { bytes, name } => {
                self.pending_init = Some((bytes, name));
                if !self.active.is_empty()
                    && let Err(e) = self.ensure_init()
                {
                    self.fail_all(&format!("init: {e}"));
                }
            }
            RecMsg::Start { id, container, not_before_ms, started_at_ms, reply } => {
                let res = self.start(&id, &container, not_before_ms, started_at_ms);
                if let Err(e) = &res {
                    (self.on_event)(RecorderEvent::Failed { recording: id.clone(), error: e.clone() });
                }
                if let Some(r) = reply {
                    let _ = r.send(res);
                }
            }
            RecMsg::Stop { id, ended_at_ms, not_after_ms, state, reply } => {
                if let Some(mut a) = self.active.remove(&id) {
                    let _ = append(&mut a.file, &Entry::End { ended_at_ms, state: state.into(), not_after_ms });
                    (self.on_event)(RecorderEvent::Stopped { recording: id, ended_at_ms, state });
                }
                if let Some(r) = reply {
                    let _ = r.send(());
                }
            }
            RecMsg::Gap { at_ms, reason, mark_next } => self.append_all(&Entry::Gap { at_ms, reason, dropped: 0 }, mark_next),
            RecMsg::Shutdown { .. } => unreachable!(),
        }
    }

    fn ensure_init(&mut self) -> std::io::Result<()> {
        if !self.run_dir_ready {
            fs::create_dir_all(self.run_dir())?;
            crate::store::fsync_dir(&self.data_dir.join("segments").join(&self.stream))?;
            self.run_dir_ready = true;
        }
        if let Some((bytes, name)) = self.pending_init.take() {
            write_file_atomic(&self.run_dir().join(&name), &bytes)?;
            let init = (self.rel(&name), sha256_hex(&bytes));
            self.init = Some(init.clone());
            self.append_all(&Entry::Init { path: init.0, sha256: init.1 }, false);
        }
        Ok(())
    }

    fn start(&mut self, id: &str, container: &str, not_before_ms: Option<i64>, started_at_ms: i64) -> Result<(), String> {
        if self.active.contains_key(id) {
            return Ok(());
        }
        let dir = self.data_dir.join("recordings").join(id);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join("index.jsonl");
        let mut f = OpenOptions::new().create_new(true).append(true).open(&path).map_err(|e| e.to_string())?;
        append(
            &mut f,
            &Entry::Begin {
                schema: senca_record::SCHEMA,
                recording: id.into(),
                stream: self.stream.clone(),
                container: container.into(),
                started_at_ms,
                not_before_ms,
            },
        )
        .map_err(|e| e.to_string())?;
        if let Some((p, h)) = &self.init {
            append(&mut f, &Entry::Init { path: p.clone(), sha256: h.clone() }).map_err(|e| e.to_string())?;
        }
        crate::store::fsync_dir(&dir).map_err(|e| e.to_string())?;
        self.active.insert(id.to_string(), Active { file: f, segments: 0, disc_next: false });
        Ok(())
    }

    /// `mark_next`: 欠落なら、次のセグメントに不連続の印を付ける。
    fn append_all(&mut self, e: &Entry, mark_next: bool) {
        let ids: Vec<String> = self.active.keys().cloned().collect();
        for id in ids {
            let a = self.active.get_mut(&id).unwrap();
            let r = append(&mut a.file, e);
            match r {
                Err(err) => self.fail(&id, &err.to_string()),
                Ok(()) => {
                    if let Entry::Gap { at_ms, reason, dropped } = e {
                        a.disc_next |= mark_next;
                        (self.on_event)(RecorderEvent::Gap { recording: id, at_ms: *at_ms, reason: reason.clone(), dropped: *dropped });
                    }
                }
            }
        }
    }

    fn fail(&mut self, id: &str, err: &str) {
        crate::util::log(&format!("recorder: recording {id}: {err}"));
        if let Some(mut a) = self.active.remove(id) {
            let now = crate::util::now_ms();
            let _ = append(&mut a.file, &Entry::End { ended_at_ms: now, state: "interrupted".into(), not_after_ms: None });
        }
        (self.on_event)(RecorderEvent::Failed { recording: id.into(), error: err.into() });
    }

    fn fail_all(&mut self, err: &str) {
        let ids: Vec<String> = self.active.keys().cloned().collect();
        for id in ids {
            self.fail(&id, err);
        }
    }

    fn stop_all(&mut self, now: i64, state: &'static str) {
        let ids: Vec<String> = self.active.keys().cloned().collect();
        for id in ids {
            if let Some(mut a) = self.active.remove(&id) {
                let _ = append(&mut a.file, &Entry::End { ended_at_ms: now, state: state.into(), not_after_ms: None });
                (self.on_event)(RecorderEvent::Stopped { recording: id, ended_at_ms: now, state });
            }
        }
    }
}

fn append(f: &mut File, e: &Entry) -> std::io::Result<()> {
    f.write_all(&senca_record::encode(e))?;
    f.sync_data()
}
