//! relay の object の cache（仕様 6.5、draft-22 7.1）。
//!
//! - 同じ object（Full Track Name・Group ID・Object ID）には同じ中身だけを対応させる。後から違う
//!   payload・subgroup・優先度・status・運ばれ方の object が来たら「malformed」として拒否し、cache を
//!   書き換えない（12.1 の 6・7）。
//! - 期限（MAX_CACHE_DURATION か relay の既定）と容量の上限で古いものから捨てる。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::Location;
use crate::session::{Delivery, Object, ObjectStatus};

/// cache の一つの object。
#[derive(Clone, Debug)]
pub struct CachedObject {
    pub object: Object,
    pub delivery: Delivery,
    pub received: Instant,
    /// payload の SHA-256（同一性の確認と記録用）。
    pub digest: [u8; 32],
}

/// 入れた結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insert {
    /// 新しい object。
    New,
    /// 既にある同じ中身の object（転送しない）。
    Duplicate,
    /// 既にある object と中身が違う（track は malformed）。
    Mismatch,
}

/// 一つの track の cache。
#[derive(Debug)]
pub struct TrackCache {
    objects: BTreeMap<Location, CachedObject>,
    bytes: usize,
    max_bytes: usize,
    max_age: Duration,
    largest: Option<Location>,
    /// End of Track の位置。
    end_of_track: Option<Location>,
    malformed: bool,
    mismatches: u64,
}

fn digest(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

impl TrackCache {
    pub fn new(max_bytes: usize, max_age: Duration) -> Self {
        Self {
            objects: BTreeMap::new(),
            bytes: 0,
            max_bytes,
            max_age,
            largest: None,
            end_of_track: None,
            malformed: false,
            mismatches: 0,
        }
    }

    /// MAX_CACHE_DURATION（Track Property）で期限を短くする。
    pub fn limit_age(&mut self, ms: u64) {
        self.max_age = self.max_age.min(Duration::from_millis(ms));
    }

    pub fn largest(&self) -> Option<Location> {
        self.largest
    }

    pub fn end_of_track(&self) -> Option<Location> {
        self.end_of_track
    }

    pub fn is_malformed(&self) -> bool {
        self.malformed
    }

    pub fn mismatches(&self) -> u64 {
        self.mismatches
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    pub fn first(&self) -> Option<Location> {
        self.objects.keys().next().copied()
    }

    /// 最も大きい位置を更新する（Largest Object、3.1.4）。
    pub fn note_largest(&mut self, l: Location) {
        if self.largest.is_none_or(|x| l > x) {
            self.largest = Some(l);
        }
    }

    /// object を入れる。
    pub fn insert(&mut self, o: &Object, delivery: Delivery, now: Instant) -> Insert {
        let loc = o.location();
        let d = digest(&o.payload);
        if let Some(c) = self.objects.get(&loc) {
            // fetch で届いたものは運ばれ方を比べない（FETCH は Delivery Mode を持たない、3.2.1）
            let delivery_differs = delivery != Delivery::Fetch && c.delivery != Delivery::Fetch && c.delivery != delivery;
            let subgroup_differs = delivery != Delivery::Fetch && c.delivery != Delivery::Fetch && c.object.subgroup != o.subgroup;
            if c.digest != d
                || c.object.payload.len() != o.payload.len()
                || c.object.status != o.status
                || c.object.priority != o.priority && delivery != Delivery::Fetch
                || subgroup_differs
                || delivery_differs
            {
                self.malformed = true;
                self.mismatches += 1;
                return Insert::Mismatch;
            }
            return Insert::Duplicate;
        }
        // End of Group / End of Track の後ろの object は malformed（12.1 の 4・5）
        if let Some(e) = self.end_of_track
            && loc > e
        {
            self.malformed = true;
            self.mismatches += 1;
            return Insert::Mismatch;
        }
        if let Some((eg, _)) = self
            .objects
            .range(Location::new(o.group, 0)..=Location::new(o.group, u64::MAX))
            .find(|(_, c)| c.object.status == ObjectStatus::EndOfGroup)
            && loc > *eg
        {
            self.malformed = true;
            self.mismatches += 1;
            return Insert::Mismatch;
        }
        if o.status == ObjectStatus::EndOfTrack {
            self.end_of_track = Some(loc);
        }
        // 状態だけの object（End of Group・End of Track）は Largest Object に数えない
        if o.status == ObjectStatus::Normal {
            self.note_largest(loc);
        }
        self.bytes += o.payload.len();
        self.objects.insert(
            loc,
            CachedObject {
                object: o.clone(),
                delivery,
                received: now,
                digest: d,
            },
        );
        self.evict(now);
        Insert::New
    }

    /// 期限・容量で捨てる（最新の group は残す）。
    pub fn evict(&mut self, now: Instant) {
        let newest_group = self.largest.map_or(0, |l| l.group);
        while let Some((&loc, c)) = self.objects.iter().next() {
            let too_old = now.saturating_duration_since(c.received) > self.max_age;
            let too_big = self.bytes > self.max_bytes;
            if (too_old || too_big) && loc.group < newest_group {
                self.bytes -= c.object.payload.len();
                self.objects.remove(&loc);
            } else {
                break;
            }
        }
    }

    /// `[start, end]` の object（位置の昇順）。
    pub fn range(&self, start: Location, end: Location) -> impl DoubleEndedIterator<Item = &CachedObject> {
        let end = if end < start { start } else { end };
        self.objects.range(start..=end).map(|(_, c)| c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn obj(g: u64, o: u64, p: &'static [u8]) -> Object {
        Object {
            group: g,
            subgroup: Some(0),
            id: o,
            priority: 128,
            status: ObjectStatus::Normal,
            properties: Bytes::new(),
            payload: Bytes::from_static(p),
        }
    }

    #[test]
    fn immutable_and_eviction() {
        let now = Instant::now();
        let mut c = TrackCache::new(10, Duration::from_secs(60));
        assert_eq!(c.insert(&obj(0, 0, b"aaaa"), Delivery::Subgroup, now), Insert::New);
        assert_eq!(c.insert(&obj(0, 0, b"aaaa"), Delivery::Subgroup, now), Insert::Duplicate);
        assert_eq!(c.insert(&obj(0, 0, b"aaab"), Delivery::Subgroup, now), Insert::Mismatch);
        assert!(c.is_malformed());
        // 書き換えられていない
        assert_eq!(&c.range(Location::new(0, 0), Location::new(0, 0)).next().unwrap().object.payload[..], b"aaaa");
        // 運ばれ方の違い
        let mut c2 = TrackCache::new(100, Duration::from_secs(60));
        c2.insert(&obj(0, 0, b"x"), Delivery::Subgroup, now);
        assert_eq!(c2.insert(&obj(0, 0, b"x"), Delivery::Datagram, now), Insert::Mismatch);
        assert_eq!(c2.insert(&obj(0, 0, b"x"), Delivery::Fetch, now), Insert::Duplicate);
        // 容量: 古い group から捨てる
        c.insert(&obj(1, 0, b"bbbb"), Delivery::Subgroup, now);
        c.insert(&obj(2, 0, b"cccc"), Delivery::Subgroup, now);
        assert!(c.first().unwrap().group >= 1);
        // End of Group の後ろ
        let mut c3 = TrackCache::new(100, Duration::from_secs(60));
        let mut eog = obj(5, 2, b"");
        eog.status = ObjectStatus::EndOfGroup;
        c3.insert(&eog, Delivery::Subgroup, now);
        assert_eq!(c3.insert(&obj(5, 3, b"late"), Delivery::Subgroup, now), Insert::Mismatch);
    }
}
