//! ファイルによる永続化（このクレートで I/O を行う唯一のモジュール）。
//!
//! 配置:
//! - `hardstate`: `"SRHS" | crc32:u32be | 本体`（一時名 → fsync → rename → ディレクトリ fsync）。
//! - `snap-<index:020>-<term:020>.snap`: `"SRSN" | version:u8 | meta_len:u32be | meta | data_len:u64be | data | crc32:u32be`
//!   （同じく原子的に書く。新しいものを書いたら古いものを消す）。
//! - `log/<first_index:020>.seg`: レコードの列。1 レコード = `len:u32be | crc32:u32be | entry`。
//!   `segment_bytes` を超えたら fsync して次のファイルへ移る。
//!
//! 起動時の検査: 最後のセグメントの末尾の壊れたレコード（fsync 前の書きかけ）は切り捨てる。
//! それ以外の破損・index の飛び・重なりは誤り（黙って捨てない）。snapshot と食い違うログ
//! （snapshot を入れた直後にログを消す前に落ちた場合）は捨てる。

use crate::codec::{self, Reader};
use crate::storage::{InitialState, Storage};
use crate::types::{Entry, HardState, Snapshot, SnapshotMeta};
use bytes::Bytes;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const HS_MAGIC: &[u8; 4] = b"SRHS";
const SNAP_MAGIC: &[u8; 4] = b"SRSN";
const MAX_RECORD: usize = codec::MAX_ENTRY_DATA + 64;
/// 既定のセグメントの大きさ。
pub const DEFAULT_SEGMENT_BYTES: u64 = 64 << 20;

fn corrupt(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// 一時名 → fsync → rename → ディレクトリ fsync。
fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("x");
    let tmp = dir.join(format!(".{name}.tmp"));
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    fsync_dir(dir)
}

#[derive(Debug)]
struct Segment {
    first: u64,
    path: PathBuf,
    /// レコードごとの (term, ファイル内の位置)。
    recs: Vec<(u64, u64)>,
    size: u64,
}

impl Segment {
    fn end(&self) -> u64 {
        self.first + self.recs.len() as u64
    }
}

/// ファイルによる [`Storage`]。
#[derive(Debug)]
pub struct FileStorage {
    dir: PathBuf,
    log_dir: PathBuf,
    segment_bytes: u64,
    snap: Option<SnapshotMeta>,
    segs: Vec<Segment>,
    active: Option<File>,
    dirty: bool,
    dir_dirty: bool,
    loaded: Option<InitialState>,
}

impl FileStorage {
    pub fn open(dir: &Path) -> io::Result<FileStorage> {
        FileStorage::open_with(dir, DEFAULT_SEGMENT_BYTES)
    }

    pub fn open_with(dir: &Path, segment_bytes: u64) -> io::Result<FileStorage> {
        let mut s = FileStorage {
            dir: dir.to_path_buf(),
            log_dir: dir.join("log"),
            segment_bytes: segment_bytes.max(1),
            snap: None,
            segs: Vec::new(),
            active: None,
            dirty: false,
            dir_dirty: false,
            loaded: None,
        };
        let st = s.scan()?;
        s.loaded = Some(st);
        Ok(s)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// セグメントのディレクトリ（試験で書きかけを作るのに使う）。
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    fn snap_index(&self) -> u64 {
        self.snap.as_ref().map_or(0, |m| m.index)
    }

    fn last_index(&self) -> u64 {
        let seg_last = self.segs.last().map_or(0, |s| s.end().saturating_sub(1));
        seg_last.max(self.snap_index())
    }

    fn term_of(&self, i: u64) -> Option<u64> {
        if let Some(m) = &self.snap
            && m.index == i
        {
            return Some(m.term);
        }
        let seg = self.segs.iter().rev().find(|s| s.first <= i)?;
        seg.recs.get((i - seg.first) as usize).map(|r| r.0)
    }

    fn remove_all_segments(&mut self) -> io::Result<()> {
        self.active = None;
        for s in self.segs.drain(..) {
            fs::remove_file(&s.path)?;
        }
        fsync_dir(&self.log_dir)
    }

    fn open_active(&mut self) -> io::Result<()> {
        self.active = match self.segs.last() {
            Some(s) => Some(OpenOptions::new().append(true).open(&s.path)?),
            None => None,
        };
        Ok(())
    }

    fn scan(&mut self) -> io::Result<InitialState> {
        fs::create_dir_all(&self.log_dir)?;
        // 一時ファイルの掃除
        for e in fs::read_dir(&self.dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && name.ends_with(".tmp") {
                fs::remove_file(e.path())?;
            }
        }
        // hard state
        let hard_state = match fs::read(self.dir.join("hardstate")) {
            Ok(b) => {
                if b.len() < 8 || &b[0..4] != HS_MAGIC {
                    return Err(corrupt("hardstate: bad header".into()));
                }
                let crc = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
                if codec::crc32(&b[8..]) != crc {
                    return Err(corrupt("hardstate: checksum".into()));
                }
                codec::decode_hard_state(Bytes::copy_from_slice(&b[8..]))
                    .map_err(|e| corrupt(format!("hardstate: {e}")))?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => HardState::default(),
            Err(e) => return Err(e),
        };
        // snapshot（最新だけ使う）
        let mut snaps: Vec<(u64, PathBuf)> = Vec::new();
        for e in fs::read_dir(&self.dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(rest) = name
                .strip_prefix("snap-")
                .and_then(|r| r.strip_suffix(".snap"))
                && let Some((i, _t)) = rest.split_once('-')
                && let Ok(i) = i.parse::<u64>()
            {
                snaps.push((i, e.path()));
            }
        }
        snaps.sort();
        let snapshot = match snaps.last() {
            Some((_, p)) => Some(read_snapshot_file(p)?),
            None => None,
        };
        for (_, p) in snaps.iter().rev().skip(1) {
            fs::remove_file(p)?;
        }
        self.snap = snapshot.as_ref().map(|s| s.meta.clone());
        // セグメント
        let mut names: Vec<(u64, PathBuf)> = Vec::new();
        for e in fs::read_dir(&self.log_dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(i) = name
                .strip_suffix(".seg")
                .and_then(|n| n.parse::<u64>().ok())
            {
                names.push((i, e.path()));
            }
        }
        names.sort();
        self.segs.clear();
        let mut entries: Vec<Entry> = Vec::new();
        let n = names.len();
        for (k, (first, path)) in names.into_iter().enumerate() {
            let is_last = k + 1 == n;
            let buf = Bytes::from(fs::read(&path)?);
            let mut seg = Segment {
                first,
                path: path.clone(),
                recs: Vec::new(),
                size: 0,
            };
            if let Some(prev) = self.segs.last()
                && prev.end() != first
            {
                return Err(corrupt(format!(
                    "log: segment {} does not follow {}",
                    first,
                    prev.end() - 1
                )));
            }
            let mut pos = 0usize;
            loop {
                if pos == buf.len() {
                    break;
                }
                let rec = parse_record(&buf, pos);
                match rec {
                    Some((e, next)) if e.index == seg.end() => {
                        seg.recs.push((e.term, pos as u64));
                        entries.push(e);
                        pos = next;
                    }
                    Some((e, _)) => {
                        return Err(corrupt(format!(
                            "log: {}: index {} where {} expected",
                            path.display(),
                            e.index,
                            seg.end()
                        )));
                    }
                    None if is_last => {
                        // 書きかけの末尾を切り捨てる
                        let f = OpenOptions::new().write(true).open(&path)?;
                        f.set_len(pos as u64)?;
                        f.sync_all()?;
                        break;
                    }
                    None => {
                        return Err(corrupt(format!(
                            "log: {}: corrupt record at {pos}",
                            path.display()
                        )));
                    }
                }
            }
            seg.size = pos as u64;
            if seg.recs.is_empty() && !is_last {
                return Err(corrupt(format!("log: empty segment {}", path.display())));
            }
            self.segs.push(seg);
        }
        // snapshot との整合
        let si = self.snap_index();
        if let Some(lf) = entries.first().map(|e| e.index) {
            let ll = entries.last().map_or(0, |e| e.index);
            let st = self.snap.as_ref().map_or(0, |m| m.term);
            let keep = if ll < si {
                false
            } else if lf > si + 1 {
                return Err(corrupt(format!(
                    "log: gap between snapshot {si} and first entry {lf}"
                )));
            } else if si >= lf {
                entries[(si - lf) as usize].term == st
            } else {
                true
            };
            if keep {
                entries.retain(|e| e.index > si);
            } else {
                entries.clear();
                self.remove_all_segments()?;
            }
        }
        self.open_active()?;
        Ok(InitialState {
            hard_state,
            snapshot,
            entries,
        })
    }

    fn truncate_from(&mut self, idx: u64) -> io::Result<()> {
        let mut removed = false;
        while self.segs.last().is_some_and(|s| s.first >= idx) {
            if let Some(s) = self.segs.pop() {
                self.active = None;
                fs::remove_file(&s.path)?;
                removed = true;
            }
        }
        if removed {
            fsync_dir(&self.log_dir)?;
        }
        if let Some(seg) = self.segs.last_mut()
            && idx < seg.end()
        {
            let off = seg.recs[(idx - seg.first) as usize].1;
            let f = OpenOptions::new().write(true).open(&seg.path)?;
            f.set_len(off)?;
            f.sync_all()?;
            seg.recs.truncate((idx - seg.first) as usize);
            seg.size = off;
        }
        self.open_active()
    }

    fn write_record(&mut self, e: &Entry) -> io::Result<()> {
        let need_new = self
            .segs
            .last()
            .is_none_or(|s| s.size >= self.segment_bytes)
            || self.active.is_none();
        if need_new {
            if let Some(f) = self.active.as_mut()
                && self.dirty
            {
                f.sync_data()?;
            }
            let reuse = self
                .segs
                .last()
                .is_some_and(|s| s.size < self.segment_bytes);
            if !reuse {
                let path = self.log_dir.join(format!("{:020}.seg", e.index));
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                self.segs.push(Segment {
                    first: e.index,
                    path,
                    recs: Vec::new(),
                    size: 0,
                });
                self.dir_dirty = true;
            }
            self.open_active()?;
        }
        let payload = codec::encode_entry(e);
        let mut rec = Vec::with_capacity(payload.len() + 8);
        rec.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        rec.extend_from_slice(&codec::crc32(&payload).to_be_bytes());
        rec.extend_from_slice(&payload);
        let Some(f) = self.active.as_mut() else {
            return Err(io::Error::other("no active segment"));
        };
        f.write_all(&rec)?;
        let Some(seg) = self.segs.last_mut() else {
            return Err(io::Error::other("no active segment"));
        };
        seg.recs.push((e.term, seg.size));
        seg.size += rec.len() as u64;
        self.dirty = true;
        Ok(())
    }
}

fn parse_record(buf: &Bytes, pos: usize) -> Option<(Entry, usize)> {
    if buf.len() - pos < 8 {
        return None;
    }
    let len = u32::from_be_bytes(buf[pos..pos + 4].try_into().ok()?) as usize;
    let crc = u32::from_be_bytes(buf[pos + 4..pos + 8].try_into().ok()?);
    if len > MAX_RECORD || buf.len() - pos - 8 < len {
        return None;
    }
    let body = buf.slice(pos + 8..pos + 8 + len);
    if codec::crc32(&body) != crc {
        return None;
    }
    let e = codec::decode_entry(body).ok()?;
    Some((e, pos + 8 + len))
}

fn read_snapshot_file(p: &Path) -> io::Result<Snapshot> {
    let b = Bytes::from(fs::read(p)?);
    let bad = |w: &str| corrupt(format!("snapshot {}: {w}", p.display()));
    if b.len() < 4 + 1 + 4 + 8 + 4 || &b[0..4] != SNAP_MAGIC {
        return Err(bad("bad header"));
    }
    let body = b.slice(..b.len() - 4);
    let crc = u32::from_be_bytes(b[b.len() - 4..].try_into().map_err(|_| bad("crc"))?);
    if codec::crc32(&body[4..]) != crc {
        return Err(bad("checksum"));
    }
    let mut r = Reader::new(body.slice(4..));
    let ver = r.u8().map_err(|_| bad("truncated"))?;
    if ver != codec::VERSION {
        return Err(bad("version"));
    }
    let ml = r.u32().map_err(|_| bad("truncated"))? as usize;
    let meta = codec::decode_snapshot_meta(r.bytes(ml).map_err(|_| bad("truncated"))?)
        .map_err(|_| bad("meta"))?;
    let dl = r.u64().map_err(|_| bad("truncated"))?;
    if dl as usize != r.remaining() {
        return Err(bad("length"));
    }
    let data = r.bytes(dl as usize).map_err(|_| bad("truncated"))?;
    Ok(Snapshot { meta, data })
}

impl Storage for FileStorage {
    fn load(&mut self) -> io::Result<InitialState> {
        match self.loaded.take() {
            Some(s) => Ok(s),
            None => self.scan(),
        }
    }

    fn append(&mut self, entries: &[Entry]) -> io::Result<()> {
        self.loaded = None;
        let Some(first) = entries.first() else {
            return Ok(());
        };
        let si = self.snap_index();
        let last = self.last_index();
        if first.index <= si || first.index > last + 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "append at {} out of range (snapshot {si}, last {last})",
                    first.index
                ),
            ));
        }
        if first.index <= last {
            self.truncate_from(first.index)?;
        } else if self.segs.last().is_some_and(|s| s.end() != first.index) {
            // 圧縮で最後のセグメントが snapshot より前で終わっている → 新しいセグメントから始める。
            self.active = None;
            let path = self.log_dir.join(format!("{:020}.seg", first.index));
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            self.segs.push(Segment {
                first: first.index,
                path,
                recs: Vec::new(),
                size: 0,
            });
            self.dir_dirty = true;
            self.open_active()?;
        }
        for e in entries {
            self.write_record(e)?;
        }
        Ok(())
    }

    fn set_hard_state(&mut self, hs: &HardState) -> io::Result<()> {
        self.loaded = None;
        let payload = codec::encode_hard_state(hs);
        let mut b = Vec::with_capacity(payload.len() + 8);
        b.extend_from_slice(HS_MAGIC);
        b.extend_from_slice(&codec::crc32(&payload).to_be_bytes());
        b.extend_from_slice(&payload);
        write_atomic(&self.dir.join("hardstate"), &b)
    }

    fn save_snapshot(&mut self, snap: &Snapshot) -> io::Result<()> {
        self.loaded = None;
        let idx = snap.meta.index;
        if self.snap.is_some() && idx <= self.snap_index() {
            return Ok(());
        }
        let meta = codec::encode_snapshot_meta(&snap.meta);
        let mut b = Vec::with_capacity(meta.len() + snap.data.len() + 21);
        b.extend_from_slice(SNAP_MAGIC);
        b.push(codec::VERSION);
        b.extend_from_slice(&(meta.len() as u32).to_be_bytes());
        b.extend_from_slice(&meta);
        b.extend_from_slice(&(snap.data.len() as u64).to_be_bytes());
        b.extend_from_slice(&snap.data);
        let crc = codec::crc32(&b[4..]);
        b.extend_from_slice(&crc.to_be_bytes());
        let name = format!("snap-{:020}-{:020}.snap", idx, snap.meta.term);
        write_atomic(&self.dir.join(&name), &b)?;
        // ログを詰める
        if self.term_of(idx) == Some(snap.meta.term) {
            let mut removed = false;
            while self.segs.first().is_some_and(|s| s.end() <= idx + 1) {
                let s = self.segs.remove(0);
                if self.segs.is_empty() {
                    self.active = None;
                }
                fs::remove_file(&s.path)?;
                removed = true;
            }
            if removed {
                fsync_dir(&self.log_dir)?;
            }
        } else {
            self.remove_all_segments()?;
        }
        // 古い snapshot を消す
        for e in fs::read_dir(&self.dir)? {
            let e = e?;
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("snap-") && n.ends_with(".snap") && n != name {
                fs::remove_file(e.path())?;
            }
        }
        fsync_dir(&self.dir)?;
        self.snap = Some(snap.meta.clone());
        self.open_active()
    }

    fn sync(&mut self) -> io::Result<()> {
        if self.dirty {
            if let Some(f) = self.active.as_mut() {
                f.sync_data()?;
            }
            self.dirty = false;
        }
        if self.dir_dirty {
            fsync_dir(&self.log_dir)?;
            self.dir_dirty = false;
        }
        Ok(())
    }
}
