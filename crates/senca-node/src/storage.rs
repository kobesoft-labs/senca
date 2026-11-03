//! 録画の置き場所（ADR-0005）。2 段に分ける:
//!
//! 1. **手元**（データ置き場、今までどおり）: セグメント・初期化セグメント・索引・サムネイルをまず書く（障害に強い順番）。
//! 2. **保存先**（S3 互換のオブジェクトストレージ、設定したときだけ）: 確定した録画を上げる（`archive.rs`）。
//!    キーは「接頭辞 + データ置き場からの相対パス」（`{prefix}/segments/...`・`{prefix}/recordings/{id}/...`）。
//!
//! 読むときは手元を先に見て、無ければ保存先から読む。どの録画が保存先にあるかは制御状態の
//! `Recording.storage` にあるので、同じ保存先を設定したノードならどこからでも出せる。

use crate::app::App;
use crate::s3::S3Client;
use bytes::Bytes;
use senca_s3::Location;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 保存先から読む索引の大きさの上限。
const MAX_INDEX: usize = 64 << 20;
/// 保存先から読むセグメント 1 本の上限。
pub const MAX_OBJECT: usize = 512 << 20;
/// 書き出し（MP4）の上限（`export.rs` と同じ）。
pub const MAX_EXPORT: usize = 4 << 30;
/// 保存先の索引の写しを覚えておく数。
const INDEX_CACHE: usize = 512;

pub struct Storage {
    pub client: S3Client,
    /// 新しく上げる場所（このノードの設定）。
    pub location: Location,
    pub keep_local_secs: u64,
    pub part_size: u64,
    /// 確定した録画の索引（保存先から読んだもの。変わらないので覚えておける）。
    index_cache: Mutex<HashMap<String, Arc<senca_record::Index>>>,
}

impl Storage {
    pub fn from_config(cfg: &crate::config::RecordingStorageConfig) -> Result<Option<Storage>, String> {
        let Some((location, endpoint)) = cfg.remote()? else { return Ok(None) };
        let creds = crate::s3::load_credentials(cfg.credentials_file.as_deref())?;
        Ok(Some(Storage {
            client: S3Client::new(endpoint, creds),
            location,
            keep_local_secs: cfg.keep_local_secs,
            part_size: cfg.part_size,
            index_cache: Mutex::new(HashMap::new()),
        }))
    }

    pub fn uri(&self) -> String {
        self.location.uri()
    }

    pub fn forget_index(&self, id: &str) {
        self.index_cache.lock().unwrap().remove(id);
    }

    /// 上げる（大きければ分割）。
    pub async fn put(&self, loc: &Location, key: &str, body: Bytes, content_type: &str) -> Result<(), String> {
        self.client.put_auto(&loc.bucket, key, body, content_type, self.part_size).await.map_err(|e| e.to_string())
    }
}

/// 録画の中の相対パスが手元で読んでよい形か（保存先の索引が書き換えられていてもデータ置き場の外を読まない）。
pub fn safe_rel(p: &str) -> bool {
    (p.starts_with("segments/") || p.starts_with("recordings/")) && senca_s3::request::valid_rel(p)
}

pub fn content_type(rel: &str) -> &'static str {
    match rel.rsplit('.').next().unwrap_or("") {
        "ts" => "video/mp2t",
        "m4s" => "video/iso.segment",
        "mp4" => "video/mp4",
        "jpg" => "image/jpeg",
        "json" => "application/json",
        "jsonl" => "application/x-ndjson",
        _ => "application/octet-stream",
    }
}

/// 範囲つきで読んだ結果。
pub enum Ranged {
    /// 全体（手元にあった、または保存先が範囲を無視した）。範囲は呼び出し側で切る。
    Whole(Bytes),
    /// 保存先が範囲で返した（先頭, 末尾, 全体）。
    Part(Bytes, u64, u64, u64),
    /// 範囲が合わない。
    Unsatisfiable,
}

impl App {
    /// この録画が保存先にあり、このノードから読めるなら、その場所。
    pub fn remote_of(&self, id: &str) -> Option<(&Storage, Location)> {
        let st = self.storage.get()?;
        let uri = self.control.lock().unwrap().state.recordings.get(id)?.storage.as_ref()?.uri.clone();
        Some((st, Location::parse(&uri).ok()?))
    }

    fn local_index(&self, id: &str) -> Option<senca_record::Index> {
        if id.parse::<senca_core::Id128>().is_err() {
            return None;
        }
        let b = std::fs::read(self.cfg.data_dir.join("recordings").join(id).join("index.jsonl")).ok()?;
        senca_record::parse(&b).ok()
    }

    /// 録画の索引。手元 → 保存先の順に探す。
    pub async fn index_async(&self, id: &str) -> Option<Arc<senca_record::Index>> {
        if let Some(i) = self.local_index(id) {
            return Some(Arc::new(i));
        }
        let (st, loc) = self.remote_of(id)?;
        if let Some(i) = st.index_cache.lock().unwrap().get(id) {
            return Some(i.clone());
        }
        let key = loc.key(&format!("recordings/{id}/index.jsonl"));
        let got = match st.client.get_object(&loc.bucket, &key, None, MAX_INDEX).await {
            Ok(Some(g)) => g,
            Ok(None) => return None,
            Err(e) => {
                self.log(&format!("storage: index of {id}: {e}"));
                return None;
            }
        };
        let idx = Arc::new(senca_record::parse(&got.body).ok()?);
        if idx.end.is_some() {
            let mut c = st.index_cache.lock().unwrap();
            if c.len() >= INDEX_CACHE {
                c.clear();
            }
            c.insert(id.to_string(), idx.clone());
        }
        Some(idx)
    }

    /// 同期の文脈から（API のスレッド・書き出し・サムネイル）。
    pub fn read_index(&self, id: &str) -> Option<senca_record::Index> {
        if let Some(i) = self.local_index(id) {
            return Some(i);
        }
        self.remote_of(id)?;
        let rt = self.rt.get()?;
        crate::util::block_on(rt, self.index_async(id)).map(|i| (*i).clone())
    }

    /// 録画の中のオブジェクト（セグメント・初期化セグメント・サムネイルなど）。手元 → 保存先。
    /// `key` は索引にある保存先のキー（無ければ場所と相対パスから作る）。
    pub async fn rec_object(&self, id: &str, rel: &str, key: Option<&str>, max: usize) -> Option<Bytes> {
        if !safe_rel(rel) {
            return None;
        }
        if let Ok(b) = std::fs::read(self.cfg.data_dir.join(rel)) {
            return Some(Bytes::from(b));
        }
        let (st, loc) = self.remote_of(id)?;
        let key = key.map(str::to_string).unwrap_or_else(|| loc.key(rel));
        match st.client.get_object(&loc.bucket, &key, None, max).await {
            Ok(Some(g)) => Some(g.body),
            Ok(None) => None,
            Err(e) => {
                self.log(&format!("storage: {id} {rel}: {e}"));
                None
            }
        }
    }

    pub fn rec_object_blocking(&self, id: &str, rel: &str, key: Option<&str>, max: usize) -> Option<Bytes> {
        if !safe_rel(rel) {
            return None;
        }
        if let Ok(b) = std::fs::read(self.cfg.data_dir.join(rel)) {
            return Some(Bytes::from(b));
        }
        self.remote_of(id)?;
        let rt = self.rt.get()?;
        crate::util::block_on(rt, self.rec_object(id, rel, key, max))
    }

    /// 範囲つきで読む（VOD の Range）。手元にあれば全体を返し、保存先なら範囲をそのまま渡す。
    pub async fn rec_object_range(&self, id: &str, rel: &str, key: Option<&str>, range: &str) -> Option<Ranged> {
        if !safe_rel(rel) {
            return None;
        }
        if let Ok(b) = std::fs::read(self.cfg.data_dir.join(rel)) {
            return Some(Ranged::Whole(Bytes::from(b)));
        }
        let (st, loc) = self.remote_of(id)?;
        let key = key.map(str::to_string).unwrap_or_else(|| loc.key(rel));
        match st.client.get_object(&loc.bucket, &key, Some(range), MAX_OBJECT).await {
            Ok(Some(g)) => match g.range {
                Some((a, b, total)) => Some(Ranged::Part(g.body, a, b, total.unwrap_or(b + 1))),
                None => Some(Ranged::Whole(g.body)),
            },
            Ok(None) => None,
            Err(e) if e.status == 416 => Some(Ranged::Unsatisfiable),
            Err(e) => {
                self.log(&format!("storage: {id} {rel}: {e}"));
                None
            }
        }
    }
}
