//! ライブ HLS のメモリキャッシュ（ADR-0006）。HTTP はここから返す。ディスクを通らない。
//! 窓から外れたセグメントは少し猶予を置いて消す（再生中のプレイヤーが取りに来るため）。

use crate::packager::{LivePart, LiveSegment};
use bytes::Bytes;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use tokio::sync::watch;

/// 窓から外れても残すセグメント数。
const KEEP_EXTRA: usize = 4;

#[derive(Default)]
struct Inner {
    segments: VecDeque<(u64, Bytes, String)>,
    parts: BTreeMap<(u64, u32), Bytes>,
    inits: BTreeMap<String, Bytes>,
    playlist: Option<Bytes>,
    playlist_skip: Option<Bytes>,
    window: usize,
    /// (最後に完成したセグメント, 作りかけのセグメントの最後の part)
    last_complete: Option<u64>,
    last_part: Option<(u64, u32)>,
    ended: bool,
    ext: String,
}

pub struct LiveCache {
    inner: Mutex<Inner>,
    /// 更新のたびに増える（ブロッキング・リロードの待ち合わせ）。
    version: watch::Sender<u64>,
}

impl Default for LiveCache {
    fn default() -> Self {
        Self::new()
    }
}

pub enum Ready {
    Now,
    Wait,
    Bad,
}

impl LiveCache {
    pub fn new() -> Self {
        let (version, _) = watch::channel(0);
        LiveCache { inner: Mutex::new(Inner { window: 6, ..Default::default() }), version }
    }

    fn bump(&self) {
        self.version.send_modify(|v| *v += 1);
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    pub fn push_segment(&self, s: &LiveSegment) {
        let mut g = self.inner.lock().unwrap();
        g.ended = false;
        g.ext = s.name.rsplit('.').next().unwrap_or("ts").to_string();
        g.segments.push_back((s.sequence, s.bytes.clone(), s.name.clone()));
        g.parts.retain(|(seq, _), _| *seq > s.sequence);
        let removed = s.removed.len();
        g.window = g.window.max(1);
        // 窓から外れた数 + 猶予を超えたら古いものから消す
        let in_window = g.segments.len().saturating_sub(removed);
        let _ = in_window;
        let limit = 64usize.max(g.window + KEEP_EXTRA);
        while g.segments.len() > limit {
            g.segments.pop_front();
        }
        for seq in &s.removed {
            // 猶予: 窓から外れてから KEEP_EXTRA セグメント分は残す
            let cutoff = seq.saturating_sub(KEEP_EXTRA as u64);
            while g.segments.front().is_some_and(|(q, _, _)| *q < cutoff) {
                g.segments.pop_front();
            }
        }
        g.last_complete = Some(s.sequence);
        if !s.playlist.is_empty() {
            g.playlist = Some(Bytes::from(s.playlist.clone()));
            g.playlist_skip = s.playlist_skip.clone().map(Bytes::from);
        }
        drop(g);
        self.bump();
    }

    pub fn push_part(&self, p: &LivePart) {
        let mut g = self.inner.lock().unwrap();
        g.parts.insert((p.sequence, p.part), p.bytes.clone());
        g.last_part = Some((p.sequence, p.part));
        if !p.playlist.is_empty() {
            g.playlist = Some(Bytes::from(p.playlist.clone()));
            g.playlist_skip = p.playlist_skip.clone().map(Bytes::from);
        }
        drop(g);
        self.bump();
    }

    pub fn set_init(&self, b: Bytes, name: &str) {
        let mut g = self.inner.lock().unwrap();
        g.inits.insert(name.to_string(), b);
        while g.inits.len() > 8 {
            let k = g.inits.keys().next().cloned().unwrap();
            g.inits.remove(&k);
        }
    }

    /// 配信の終わり。プレイリストに ENDLIST を付ける。
    pub fn end(&self) {
        let mut g = self.inner.lock().unwrap();
        g.ended = true;
        if let Some(p) = g.playlist.clone() {
            let s = String::from_utf8_lossy(&p);
            if !s.contains("#EXT-X-ENDLIST") {
                // 作りかけの part・preload hint を除いて ENDLIST
                let mut out = String::new();
                for line in s.lines() {
                    if line.starts_with("#EXT-X-PRELOAD-HINT") || line.starts_with("#EXT-X-PART:") {
                        continue;
                    }
                    out.push_str(line);
                    out.push('\n');
                }
                out.push_str("#EXT-X-ENDLIST\n");
                g.playlist = Some(Bytes::from(out));
            }
        }
        g.playlist_skip = None;
        drop(g);
        self.bump();
    }

    pub fn playlist(&self, skip: bool) -> Option<Bytes> {
        let g = self.inner.lock().unwrap();
        if skip { g.playlist_skip.clone().or_else(|| g.playlist.clone()) } else { g.playlist.clone() }
    }

    /// `name` は `{seq}.ts`・`{seq}.{part}.m4s`・`init{n}.mp4` など。
    pub fn object(&self, name: &str) -> Option<Bytes> {
        let g = self.inner.lock().unwrap();
        if let Some(b) = g.inits.get(name) {
            return Some(b.clone());
        }
        let stem = name.strip_suffix(&format!(".{}", g.ext))?;
        let mut it = stem.split('.');
        let seq: u64 = it.next()?.parse().ok()?;
        match it.next() {
            None => g.segments.iter().find(|(q, _, _)| *q == seq).map(|(_, b, _)| b.clone()),
            Some(p) => {
                let part: u32 = p.parse().ok()?;
                if it.next().is_some() {
                    return None;
                }
                g.parts.get(&(seq, part)).cloned()
            }
        }
    }

    /// ブロッキング・リロード（LL-HLS）: `_HLS_msn`・`_HLS_part` の内容が揃っているか。
    pub fn ready(&self, msn: u64, part: Option<u32>) -> Ready {
        let g = self.inner.lock().unwrap();
        if g.ended {
            return Ready::Now;
        }
        let done = g.last_complete;
        let open = done.map(|d| d + 1).unwrap_or(0);
        match part {
            None => {
                if done.is_some_and(|d| msn <= d) {
                    Ready::Now
                } else if msn <= open + 1 {
                    Ready::Wait
                } else {
                    Ready::Bad
                }
            }
            Some(p) => {
                if done.is_some_and(|d| msn <= d) || g.last_part.is_some_and(|(s, lp)| s == msn && lp >= p) {
                    Ready::Now
                } else if msn <= open + 1 && p < 100 {
                    Ready::Wait
                } else {
                    Ready::Bad
                }
            }
        }
    }
}
