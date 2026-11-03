//! 視聴者の数（stream の `limits.max_viewers`）。ノードごとに数える。
//!
//! - 接続を持つ視聴（RTMP・SRT・RTSP の play、WHEP、MoQ）は、購読の間だけ [`ViewerGuard`] を持つ。
//!   guard を手放すと数が減る（購読の終わり・接続の切断・タスクの中断のどれでも）。
//! - HLS は接続を持たないので、プレイリストを取りに来た「最近の別々の客」で近似する。客は送り元の IP と
//!   User-Agent の組で見分け、最後の取得から [`hls_active_ms`] の間は視聴中として数える。
//!   同じ NAT の内側の同じブラウザーは 1 人に見える（多めに通す側に倒れる）。
//! - 上限を超える新しい視聴は断る（既に数えている客・接続はそのまま）。上限が無ければ数えるだけ。
//! - 中継の写し（ADR-0014）・録画・字幕・出力・パイプラインの購読は視聴者に数えない。

use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

/// 1 本の stream で覚えておく HLS の客の数の上限（上限が無い stream でもメモリを使い切らない）。
const MAX_HLS_CLIENTS: usize = 65_536;

/// 接続を持つ視聴の種類。
pub const SESSION_KINDS: [&str; 5] = ["rtmp", "srt", "rtsp", "webrtc", "moq"];

#[derive(Default)]
struct Entry {
    sessions: BTreeMap<&'static str, usize>,
    /// 客の印 → 最後にプレイリストを取った時刻（単調時計の ms）。
    hls: HashMap<u64, u64>,
}

impl Entry {
    fn sessions_total(&self) -> usize {
        self.sessions.values().sum()
    }

    fn prune_hls(&mut self, now: u64, active_ms: u64) {
        self.hls.retain(|_, t| now.saturating_sub(*t) <= active_ms);
    }
}

#[derive(Default)]
pub struct Viewers {
    inner: Mutex<HashMap<String, Entry>>,
}

/// HLS の客を視聴中と見なす長さ（最後のプレイリストの取得から）。セグメントの 3 本分、10 秒以上。
pub fn hls_active_ms(segment_ms: u32) -> u64 {
    (segment_ms as u64 * 3).max(10_000)
}

/// HLS の客の印（送り元の IP と User-Agent）。
pub fn hls_client_key(peer: &str, user_agent: Option<&str>) -> u64 {
    use std::hash::{Hash, Hasher};
    let ip = match peer.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => h.trim_matches(['[', ']']),
        _ => peer,
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ip.hash(&mut h);
    user_agent.unwrap_or("").hash(&mut h);
    h.finish()
}

/// 接続を持つ視聴 1 つ。手放すと数が減る。
pub struct ViewerGuard {
    viewers: Arc<Viewers>,
    stream: String,
    kind: &'static str,
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        let mut g = self.viewers.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = g.get_mut(&self.stream) {
            if let Some(n) = e.sessions.get_mut(self.kind) {
                *n = n.saturating_sub(1);
            }
            e.sessions.retain(|_, n| *n > 0);
            if e.sessions.is_empty() && e.hls.is_empty() {
                g.remove(&self.stream);
            }
        }
    }
}

impl Viewers {
    /// 接続を持つ視聴を 1 つ足す。上限に達していれば None（断る）。
    pub fn try_session(self: &Arc<Self>, stream: &str, kind: &'static str, max: Option<u32>, hls_active: u64) -> Option<ViewerGuard> {
        let now = crate::util::mono_ms();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let e = g.entry(stream.to_string()).or_default();
        e.prune_hls(now, hls_active);
        if let Some(m) = max
            && e.sessions_total() + e.hls.len() >= m as usize
        {
            return None;
        }
        *e.sessions.entry(kind).or_default() += 1;
        Some(ViewerGuard { viewers: self.clone(), stream: stream.to_string(), kind })
    }

    /// 上限の前に確かめるだけ（数えない）。SRT の受け入れの判断で使う（数えるのは接続ができてから）。
    pub fn has_room(&self, stream: &str, max: Option<u32>, hls_active: u64) -> bool {
        let Some(m) = max else { return true };
        let now = crate::util::mono_ms();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match g.get_mut(stream) {
            None => m > 0,
            Some(e) => {
                e.prune_hls(now, hls_active);
                e.sessions_total() + e.hls.len() < m as usize
            }
        }
    }

    /// HLS のプレイリストの取得を記録する。新しい客で上限に達していれば false（断る）。
    pub fn hls_touch(&self, stream: &str, client: u64, max: Option<u32>, hls_active: u64) -> bool {
        let now = crate::util::mono_ms();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let e = g.entry(stream.to_string()).or_default();
        e.prune_hls(now, hls_active);
        if !e.hls.contains_key(&client) {
            if max.is_some_and(|m| e.sessions_total() + e.hls.len() >= m as usize) {
                return false;
            }
            if e.hls.len() >= MAX_HLS_CLIENTS {
                // 覚えきれない: 一番古い客を忘れる
                if let Some(k) = e.hls.iter().min_by_key(|(_, t)| **t).map(|(k, _)| *k) {
                    e.hls.remove(&k);
                }
            }
        }
        e.hls.insert(client, now);
        true
    }

    /// 今の数（API の `live.viewers`）。
    pub fn counts(&self, stream: &str, hls_active: u64) -> Value {
        let now = crate::util::mono_ms();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut v = json!({ "total": 0, "hls": 0 });
        for k in SESSION_KINDS {
            v[k] = json!(0);
        }
        if let Some(e) = g.get_mut(stream) {
            e.prune_hls(now, hls_active);
            for (k, n) in &e.sessions {
                v[*k] = json!(n);
            }
            v["hls"] = json!(e.hls.len());
            v["total"] = json!(e.sessions_total() + e.hls.len());
        }
        v
    }

    /// stream を消したときに忘れる。
    pub fn forget(&self, stream: &str) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).remove(stream);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_and_hls_share_the_limit() {
        let v = Arc::new(Viewers::default());
        let a = v.try_session("s", "rtmp", Some(3), 10_000).unwrap();
        assert!(v.hls_touch("s", 1, Some(3), 10_000));
        // 同じ客はもう一度来ても 1 人
        assert!(v.hls_touch("s", 1, Some(3), 10_000));
        let b = v.try_session("s", "webrtc", Some(3), 10_000).unwrap();
        assert!(v.try_session("s", "moq", Some(3), 10_000).is_none());
        assert!(!v.hls_touch("s", 2, Some(3), 10_000));
        assert!(!v.has_room("s", Some(3), 10_000));
        assert_eq!(v.counts("s", 10_000)["total"], 3);
        drop(a);
        assert!(v.has_room("s", Some(3), 10_000));
        assert!(v.hls_touch("s", 2, Some(3), 10_000));
        let c = v.counts("s", 10_000);
        assert_eq!((c["hls"].as_u64(), c["webrtc"].as_u64(), c["rtmp"].as_u64()), (Some(2), Some(1), Some(0)));
        drop(b);
        // 上限が無ければ数えるだけ
        assert!(v.try_session("t", "srt", None, 10_000).is_some());
    }

    #[test]
    fn client_key_ignores_the_port() {
        assert_eq!(hls_client_key("10.0.0.1:5000", Some("x")), hls_client_key("10.0.0.1:6000", Some("x")));
        assert_eq!(hls_client_key("[::1]:5000", None), hls_client_key("[::1]:7000", None));
        assert_ne!(hls_client_key("10.0.0.1:5000", Some("x")), hls_client_key("10.0.0.1:5000", Some("y")));
    }
}
