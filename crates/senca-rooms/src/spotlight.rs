//! 話者の検出とスポットライト（注目する N 人）。
//!
//! - 音量（RFC 6464 の -dBov、0 が最大・127 が無音）を参加者ごとに受け、`speaking_level` より大きい音が
//!   `activate_after` 続けば「話している」。最後の声から `release_after` 過ぎれば話していない
//!   （途中の `gap` までの無音は続きとみなす）。
//! - 注目の組 F（最大 N 人、新しく注目した順）: 話し始めた人は空きがあれば入る。空きが無ければ、
//!   話しておらず、注目してから `min_hold` 以上たった人のうち最も長く黙っている人と入れ替える（揺れを防ぐ）。
//!   話していない人も入れ替えられるまで残る（直前の話者を見続ける）。
//! - 候補（映像の送り手）が N 人に満たなければ、参加の順で埋める。
//!
//! 音量の拡張（RFC 6464）が無いときは Opus の packet の大きさから見積もる（[`level_from_opus_size`]）。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// 設定。
#[derive(Debug, Clone)]
pub struct DetectorConfig {
    /// これより大きい音（-dBov が小さい）を声とみなす。
    pub speaking_level: u8,
    pub activate_after: Duration,
    pub release_after: Duration,
    /// 声の途切れをこの長さまでは続きとみなす。
    pub gap: Duration,
    /// 注目してから外せるまでの最短の時間。
    pub min_hold: Duration,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        DetectorConfig {
            speaking_level: 50,
            activate_after: Duration::from_millis(200),
            release_after: Duration::from_millis(1500),
            gap: Duration::from_millis(400),
            min_hold: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Speaker {
    speaking_since: Option<Instant>,
    last_voice: Option<Instant>,
}

/// 話者の検出。参加者は文字列の ID（connection_id）で区別する。
#[derive(Debug, Clone)]
pub struct SpeakerDetector {
    cfg: DetectorConfig,
    speakers: BTreeMap<String, Speaker>,
    focus: Vec<String>,
    focused_at: BTreeMap<String, Instant>,
    /// 主な話者（[`SpeakerDetector::dominant`]）と、その人にした時刻。
    dominant: Option<(String, Instant)>,
}

/// Opus の packet の大きさから音量を見積もる（20 ms の packet で、無音・DTX は 10 byte 前後、声は 40 byte 以上）。
pub fn level_from_opus_size(payload_len: usize, duration_ms: u32) -> u8 {
    let per20 = payload_len as f64 * 20.0 / duration_ms.max(1) as f64;
    if per20 <= 12.0 {
        127
    } else if per20 >= 45.0 {
        30
    } else {
        (127.0 - (per20 - 12.0) / 33.0 * 97.0) as u8
    }
}

impl SpeakerDetector {
    pub fn new(cfg: DetectorConfig) -> SpeakerDetector {
        SpeakerDetector {
            cfg,
            speakers: BTreeMap::new(),
            focus: Vec::new(),
            focused_at: BTreeMap::new(),
            dominant: None,
        }
    }

    /// 主な話者（1 人。合成の話者の大写しに使う）。揺れを防ぐ: 今の話者が話している間と、替えてから
    /// `min_hold` の間は替えない。替えるときは、話している人のうち話し始めの早い人、いなければ `release_after` の間に
    /// 声のあった人のうち最後に声のあった人。誰も声を出していなければ今の話者のまま（いなければ候補の先頭）。
    pub fn dominant(&mut self, now: Instant, candidates: &[String]) -> Option<String> {
        if self.dominant.as_ref().is_some_and(|(d, _)| !candidates.contains(d)) {
            self.dominant = None;
        }
        if let Some((d, since)) = &self.dominant
            && (self.is_speaking(d, now) || now.saturating_duration_since(*since) < self.cfg.min_hold)
        {
            return Some(d.clone());
        }
        let speaking = candidates
            .iter()
            .filter_map(|c| {
                let s = self.speakers.get(c)?;
                self.speaking(s, now).then(|| (s.speaking_since, c.clone()))
            })
            .min()
            .map(|x| x.1);
        let recent = || {
            candidates
                .iter()
                .filter_map(|c| {
                    let t = self.speakers.get(c)?.last_voice?;
                    (now.saturating_duration_since(t) < self.cfg.release_after).then(|| (t, c.clone()))
                })
                .max()
                .map(|x| x.1)
        };
        let next = speaking.or_else(recent);
        match (next, &self.dominant) {
            (Some(n), Some((d, _))) if &n == d => {}
            (Some(n), _) => self.dominant = Some((n, now)),
            (None, Some(_)) => {}
            (None, None) => self.dominant = candidates.first().map(|c| (c.clone(), now)),
        }
        self.dominant.as_ref().map(|(d, _)| d.clone())
    }

    /// 音量（-dBov）を 1 つ入れる。
    pub fn observe(&mut self, id: &str, level: u8, now: Instant) {
        let s = self.speakers.entry(id.to_string()).or_default();
        if level <= self.cfg.speaking_level {
            if s
                .last_voice
                .is_none_or(|t| now.saturating_duration_since(t) > self.cfg.gap)
            {
                s.speaking_since = Some(now);
            }
            s.last_voice = Some(now);
        }
    }

    /// 話しているか。
    pub fn is_speaking(&self, id: &str, now: Instant) -> bool {
        self.speakers.get(id).is_some_and(|s| self.speaking(s, now))
    }

    fn speaking(&self, s: &Speaker, now: Instant) -> bool {
        match (s.speaking_since, s.last_voice) {
            (Some(since), Some(last)) => {
                now.saturating_duration_since(since) >= self.cfg.activate_after
                    && now.saturating_duration_since(last) < self.cfg.release_after
            }
            _ => false,
        }
    }

    /// 参加者を忘れる。
    pub fn remove(&mut self, id: &str) {
        self.speakers.remove(id);
        self.focused_at.remove(id);
        self.focus.retain(|x| x != id);
    }

    /// 注目の組（新しく注目した順）。
    pub fn focus(&self) -> &[String] {
        &self.focus
    }

    /// 注目の組を更新する。`candidates` は映像の送り手（参加の順）、`n` は人数。変われば真。
    pub fn update(&mut self, now: Instant, candidates: &[String], n: usize) -> bool {
        let before = self.focus.clone();
        self.focus.retain(|x| candidates.contains(x));
        while self.focus.len() > n {
            if let Some(x) = self.focus.pop() {
                self.focused_at.remove(&x);
            }
        }
        // 話している人を、話し始めの早い順に
        let mut active: Vec<(Instant, String)> = candidates
            .iter()
            .filter_map(|c| {
                let s = self.speakers.get(c)?;
                if self.speaking(s, now) {
                    Some((s.speaking_since?, c.clone()))
                } else {
                    None
                }
            })
            .collect();
        active.sort();
        for (_, id) in active {
            if self.focus.contains(&id) {
                continue;
            }
            if self.focus.len() < n {
                self.focus.insert(0, id.clone());
                self.focused_at.insert(id, now);
                continue;
            }
            // 入れ替える相手: 話しておらず、min_hold を過ぎた人のうち最も長く黙っている人
            let victim = self
                .focus
                .iter()
                .filter(|f| !self.is_speaking(f, now))
                .filter(|f| {
                    self.focused_at
                        .get(*f)
                        .is_none_or(|t| now.saturating_duration_since(*t) >= self.cfg.min_hold)
                })
                .min_by_key(|f| self.speakers.get(*f).and_then(|s| s.last_voice))
                .cloned();
            if let Some(v) = victim {
                self.focus.retain(|x| *x != v);
                self.focused_at.remove(&v);
                self.focus.insert(0, id.clone());
                self.focused_at.insert(id, now);
            }
        }
        // 空きは参加の順で埋める（まだ誰も話していないとき）
        for c in candidates {
            if self.focus.len() >= n {
                break;
            }
            if !self.focus.contains(c) {
                self.focus.push(c.clone());
                self.focused_at.insert(c.clone(), now);
            }
        }
        self.focus != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dominant_speaker_with_hysteresis() {
        let mut d = SpeakerDetector::new(DetectorConfig::default());
        let cands = ids(&["a", "b"]);
        let mut t = Instant::now();
        // 誰も話していない: 候補の先頭
        assert_eq!(d.dominant(t, &cands).as_deref(), Some("a"));
        // b が話し続ける: min_hold の後に b へ
        let mut seen = Vec::new();
        for _ in 0..150 {
            d.observe("b", 20, t);
            d.observe("a", 127, t);
            seen.push(d.dominant(t, &cands).unwrap());
            t += Duration::from_millis(20);
        }
        assert_eq!(seen.last().map(String::as_str), Some("b"));
        let first_b = seen.iter().position(|x| x == "b").unwrap();
        assert!(first_b as u64 * 20 >= 2000, "switched too early at {} ms", first_b * 20);
        // a が短く割り込む（話している判定に届かない 100 ms）: b のまま
        for _ in 0..5 {
            d.observe("a", 20, t);
            d.observe("b", 20, t);
            assert_eq!(d.dominant(t, &cands).as_deref(), Some("b"));
            t += Duration::from_millis(20);
        }
        // b が黙り、a が話す: b の声が途切れて release_after の後に a へ
        let mut switched = None;
        for i in 0..200 {
            d.observe("a", 20, t);
            d.observe("b", 127, t);
            if d.dominant(t, &cands).as_deref() == Some("a") {
                switched = Some(i);
                break;
            }
            t += Duration::from_millis(20);
        }
        let i = switched.expect("switched to a");
        assert!(i * 20 >= 1400, "switched after {} ms", i * 20);
        // 候補から外れた話者は外す
        assert_eq!(d.dominant(t, &ids(&["b"])).as_deref(), Some("b"));
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// `who` が `ms` の間 20 ms ごとに声を出す（他は無音）。
    fn talk(d: &mut SpeakerDetector, t: &mut Instant, who: &str, ms: u64, cands: &[String], n: usize) -> usize {
        let mut changes = 0;
        for _ in 0..ms / 20 {
            for c in cands {
                d.observe(c, if c == who { 20 } else { 127 }, *t);
            }
            if d.update(*t, cands, n) {
                changes += 1;
            }
            *t += Duration::from_millis(20);
        }
        changes
    }

    #[test]
    fn fills_in_join_order_then_follows_speakers() {
        let mut d = SpeakerDetector::new(DetectorConfig::default());
        let c = ids(&["a", "b", "c", "d"]);
        let mut t = Instant::now();
        assert!(d.update(t, &c, 2));
        assert_eq!(d.focus(), &ids(&["a", "b"])[..]);
        // c が話す: min_hold（2 秒）が過ぎるまで入れ替えない
        talk(&mut d, &mut t, "c", 1000, &c, 2);
        assert_eq!(d.focus(), &ids(&["a", "b"])[..]);
        talk(&mut d, &mut t, "c", 1200, &c, 2);
        assert_eq!(d.focus()[0], "c");
        assert_eq!(d.focus().len(), 2);
        // d が話すと、c ではなく残りの黙っている人と入れ替わる
        talk(&mut d, &mut t, "d", 2500, &c, 2);
        assert!(d.focus().contains(&"d".to_string()));
        assert!(d.focus().contains(&"c".to_string()), "{:?}", d.focus());
    }

    #[test]
    fn hysteresis_ignores_short_noises_and_gaps() {
        let cfg = DetectorConfig::default();
        let mut d = SpeakerDetector::new(cfg.clone());
        let c = ids(&["a", "b", "c"]);
        let mut t = Instant::now();
        d.update(t, &c, 1);
        t += Duration::from_secs(5);
        // 100 ms の咳払い（activate_after 未満）では注目しない
        let n = talk(&mut d, &mut t, "b", 100, &c, 1);
        assert_eq!(n, 0);
        assert_eq!(d.focus(), &ids(&["a"])[..]);
        // 300 ms の無音を挟んで話し続けるのは 1 つの発話（gap 400 ms）
        let mut t2 = t + Duration::from_secs(3);
        talk(&mut d, &mut t2, "c", 300, &c, 1);
        t2 += Duration::from_millis(300);
        let n = talk(&mut d, &mut t2, "c", 300, &c, 1);
        assert!(n <= 1);
        assert_eq!(d.focus(), &ids(&["c"])[..]);
        assert!(d.is_speaking("c", t2));
        // 黙ると release_after の後に話していない扱い（でも注目には残る）
        let t3 = t2 + cfg.release_after + Duration::from_millis(1);
        assert!(!d.is_speaking("c", t3));
        d.update(t3, &c, 1);
        assert_eq!(d.focus(), &ids(&["c"])[..]);
    }

    #[test]
    fn alternating_speakers_do_not_flap() {
        let mut d = SpeakerDetector::new(DetectorConfig::default());
        let c = ids(&["a", "b"]);
        let mut t = Instant::now();
        d.update(t, &c, 1);
        let mut changes = 0;
        // 300 ms ごとに交互に話す（10 秒）: 入れ替えは min_hold ごとに高々 1 回
        for i in 0..33 {
            changes += talk(&mut d, &mut t, if i % 2 == 0 { "b" } else { "a" }, 300, &c, 1);
        }
        assert!(changes <= 5, "{changes} focus changes in 10 s");
    }

    #[test]
    fn leaving_and_shrinking() {
        let mut d = SpeakerDetector::new(DetectorConfig::default());
        let t = Instant::now();
        d.update(t, &ids(&["a", "b", "c"]), 3);
        d.remove("b");
        assert_eq!(d.focus(), &ids(&["a", "c"])[..]);
        assert!(d.update(t, &ids(&["a"]), 3));
        assert_eq!(d.focus(), &ids(&["a"])[..]);
        d.update(t, &ids(&["a", "b", "c"]), 3);
        assert!(d.update(t, &ids(&["a", "b", "c"]), 1));
        assert_eq!(d.focus().len(), 1);
    }

    #[test]
    fn opus_size_levels() {
        assert_eq!(level_from_opus_size(3, 20), 127);
        assert_eq!(level_from_opus_size(80, 20), 30);
        let mid = level_from_opus_size(28, 20);
        assert!(mid > 30 && mid < 127);
        // 60 ms の packet は 3 倍まで同じ扱い
        assert_eq!(level_from_opus_size(30, 60), 127);
    }
}
