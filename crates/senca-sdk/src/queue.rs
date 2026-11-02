//! 送信キュー（上限付き、ライブ優先で捨てる）。
//!
//! 回線が遅い・切れているときは古いものから捨て、今に追いつく（仕様 5.3）。
//!
//! - 上限は「量（バイト）」と「長さ（先頭と末尾の DTS の差）」の 2 つ。設定（[`Item::Config`]）は捨てない。
//! - 上限を超えたら、まずキューの中で最後のキーフレームより前の映像を捨てる。キーフレームが無ければ
//!   映像を全部捨て、次のキーフレームまで映像を受け付けない（参照の切れた P フレームを送らない）。
//!   このとき符号化器へキーフレームを頼む（[`LiveQueue::take_keyframe_request`]）。
//! - それでも超えるなら古い音声から捨てる。

use crate::frame::Item;
use senca_core::MediaKind;
use std::collections::VecDeque;

/// キューの上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueLimits {
    pub max_bytes: usize,
    /// マイクロ秒。
    pub max_duration_us: i64,
}

impl Default for QueueLimits {
    fn default() -> Self {
        QueueLimits {
            max_bytes: 8 << 20,
            max_duration_us: 2_000_000,
        }
    }
}

/// 送信キュー。
#[derive(Debug)]
pub struct LiveQueue {
    limits: QueueLimits,
    items: VecDeque<Item>,
    bytes: usize,
    waiting_key: bool,
    key_request: bool,
    pub dropped_video: u64,
    pub dropped_audio: u64,
}

fn size_of(i: &Item) -> usize {
    match i {
        Item::Config(c) => c.record.len() + 64,
        Item::Au(a) => a.payload.len() + 64,
    }
}

fn is_video_au(i: &Item) -> bool {
    matches!(i, Item::Au(a) if a.kind() == MediaKind::Video)
}

impl LiveQueue {
    pub fn new(limits: QueueLimits) -> Self {
        LiveQueue {
            limits,
            items: VecDeque::new(),
            bytes: 0,
            waiting_key: false,
            key_request: false,
            dropped_video: 0,
            dropped_audio: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// 先頭と末尾の AU の DTS の差（マイクロ秒）。
    pub fn duration_us(&self) -> i64 {
        let first = self.items.iter().find_map(|i| match i {
            Item::Au(a) => Some(a.dts),
            _ => None,
        });
        let last = self.items.iter().rev().find_map(|i| match i {
            Item::Au(a) => Some(a.dts),
            _ => None,
        });
        match (first, last) {
            (Some(f), Some(l)) => l.saturating_sub(f).max(0),
            _ => 0,
        }
    }

    fn over(&self) -> bool {
        self.bytes > self.limits.max_bytes || self.duration_us() > self.limits.max_duration_us
    }

    /// 入れる。捨てたら false。
    pub fn push(&mut self, item: Item) -> bool {
        if let Item::Au(a) = &item
            && a.kind() == MediaKind::Video
        {
            if self.waiting_key && !a.keyframe {
                self.dropped_video += 1;
                return false;
            }
            if a.keyframe {
                self.waiting_key = false;
            }
        }
        self.bytes += size_of(&item);
        self.items.push_back(item);
        if self.over() {
            self.shed();
        }
        true
    }

    fn remove_where(&mut self, mut pred: impl FnMut(usize, &Item) -> bool) {
        let mut kept = VecDeque::with_capacity(self.items.len());
        for (idx, it) in self.items.drain(..).enumerate() {
            if pred(idx, &it) {
                self.bytes -= size_of(&it);
                match &it {
                    Item::Au(a) if a.kind() == MediaKind::Video => self.dropped_video += 1,
                    Item::Au(_) => self.dropped_audio += 1,
                    Item::Config(_) => {}
                }
            } else {
                kept.push_back(it);
            }
        }
        self.items = kept;
    }

    /// 上限まで減らす。
    fn shed(&mut self) {
        let last_key = self
            .items
            .iter()
            .rposition(|i| matches!(i, Item::Au(a) if a.kind() == MediaKind::Video && a.keyframe));
        let first_video = self.items.iter().position(is_video_au);
        match (last_key, first_video) {
            (Some(k), Some(f)) if k > f => self.remove_where(|i, it| i < k && is_video_au(it)),
            (Some(_), _) => {}
            (None, _) => self.drop_all_video(),
        }
        while self.over() {
            let Some(pos) = self.items.iter().position(|i| matches!(i, Item::Au(_))) else {
                break;
            };
            if is_video_au(&self.items[pos]) {
                // 残りがキーフレームから始まる映像だけでも超えるなら、映像を全部捨てて待つ
                self.drop_all_video();
                continue;
            }
            self.remove_where(|i, _| i == pos);
        }
    }

    fn drop_all_video(&mut self) {
        self.remove_where(|_, it| is_video_au(it));
        self.waiting_key = true;
        self.key_request = true;
    }

    /// 新しい接続の始まり: 映像はキーフレームから送る。キューにキーフレームが無ければ次まで待つ。
    pub fn require_keyframe(&mut self) {
        match self
            .items
            .iter()
            .position(|i| matches!(i, Item::Au(a) if a.kind() == MediaKind::Video && a.keyframe))
        {
            Some(k) => self.remove_where(|i, it| i < k && is_video_au(it)),
            None => self.drop_all_video(),
        }
    }

    /// 種類 `kind` の最初の設定を取り出す（その種類の AU がまだ無いときに、前倒しで使う）。
    pub fn take_config(&mut self, kind: MediaKind) -> Option<senca_core::CodecConfig> {
        let i = self
            .items
            .iter()
            .position(|it| matches!(it, Item::Config(c) if c.codec.kind() == kind))?;
        match self.items.remove(i)? {
            Item::Config(c) => {
                self.bytes -= c.record.len() + 64;
                Some(c)
            }
            other => {
                self.items.insert(i, other);
                None
            }
        }
    }

    pub fn pop(&mut self) -> Option<Item> {
        let it = self.items.pop_front()?;
        self.bytes -= size_of(&it);
        Some(it)
    }

    /// キーフレームを頼む必要があれば true（1 回だけ）。
    pub fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.key_request)
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use senca_core::{AccessUnit, Codec, Timebase, TrackId};

    fn au(video: bool, key: bool, dts_ms: i64, size: usize) -> Item {
        Item::Au(AccessUnit {
            track: TrackId(u32::from(!video)),
            codec: if video { Codec::H264 } else { Codec::Aac },
            epoch: 0,
            sequence: 0,
            config_generation: 1,
            pts: dts_ms * 1000,
            dts: dts_ms * 1000,
            duration: 0,
            timebase: Timebase::MICROS,
            keyframe: key,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from(vec![0; size]),
        })
    }

    fn kinds(q: &LiveQueue) -> Vec<(bool, bool, i64)> {
        q.items
            .iter()
            .filter_map(|i| match i {
                Item::Au(a) => Some((a.kind() == MediaKind::Video, a.keyframe, a.dts / 1000)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn drops_old_gop_when_newer_keyframe_queued() {
        let mut q = LiveQueue::new(QueueLimits {
            max_bytes: 1 << 20,
            max_duration_us: 1_000_000,
        });
        q.push(au(true, true, 0, 10));
        for t in 1..10 {
            q.push(au(true, false, t * 100, 10));
            q.push(au(false, true, t * 100, 10));
        }
        q.push(au(true, true, 1000, 10));
        assert_eq!(q.dropped_video, 0, "exactly 1 s is within the limit");
        q.push(au(true, false, 1100, 10));
        // 0〜900 の映像は捨て、キーフレーム（1000）から残る
        let k = kinds(&q);
        assert!(k.iter().filter(|x| x.0).all(|x| x.2 >= 1000), "{k:?}");
        assert_eq!(q.dropped_video, 10);
        assert!(!q.take_keyframe_request());
    }

    #[test]
    fn waits_for_keyframe_after_dropping_all_video() {
        let mut q = LiveQueue::new(QueueLimits {
            max_bytes: 400,
            max_duration_us: 10_000_000,
        });
        q.push(au(true, true, 0, 100));
        assert!(q.push(au(true, false, 33, 100)));
        // 3 つ目で超える（キーフレームは先頭だけ）→ 映像を全部捨てて待つ
        q.push(au(true, false, 66, 100));
        assert!(kinds(&q).is_empty());
        assert!(q.take_keyframe_request());
        assert!(!q.push(au(true, false, 99, 10)));
        assert!(q.push(au(false, true, 99, 10)));
        assert!(q.push(au(true, true, 132, 10)));
        assert!(q.push(au(true, false, 165, 10)));
        assert_eq!(
            kinds(&q),
            vec![(false, true, 99), (true, true, 132), (true, false, 165)]
        );
    }

    #[test]
    fn require_keyframe_on_reconnect_and_configs_kept() {
        let mut q = LiveQueue::new(QueueLimits::default());
        q.push(au(true, false, 0, 10));
        q.push(Item::Config(senca_core::CodecConfig {
            codec: Codec::H264,
            generation: 1,
            record: Bytes::new(),
            video: None,
            audio: None,
        }));
        q.push(au(false, true, 0, 10));
        q.push(au(true, true, 33, 10));
        q.require_keyframe();
        assert!(matches!(q.pop(), Some(Item::Config(_))));
        assert_eq!(kinds(&q), vec![(false, true, 0), (true, true, 33)]);
        q.clear();
        q.push(au(true, false, 0, 10));
        q.require_keyframe();
        assert!(q.is_empty() && q.take_keyframe_request());
        assert!(!q.push(au(true, false, 10, 10)));
    }
}
