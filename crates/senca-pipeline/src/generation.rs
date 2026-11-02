//! 世代（仕様 5.1・11 章 `Pipeline: validate / prepare / commit / rollback`）。
//!
//! - `prepare`: 検証して資源を確保した新しい世代を作る（まだ出力しない）。
//! - `commit`: 新しい世代を動かし始め、安全な境界（出力のトラックごとに、新しい世代の最初のキーフレーム）で
//!   出力を切り替える。すべてのトラックが切り替わったら新しい世代が `Active`、古い世代は `Retired`。
//! - 切り替えの前に新しい世代が失敗した・時間切れになったら `Failed` にし、古い世代をそのまま続ける。
//! - `rollback`: まだ切り替わっていない世代（`Prepared`・`Committing`）を捨てる。
//!
//! [`GenerationManager`] は状態の遷移だけ、[`OutputSwitch`] は AU ごとの切り替えの判断だけを持つ（I/O なし）。

use crate::error::PipelineError;
use senca_core::{AccessUnit, Rescale, Timebase};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// 世代の番号（1 から）。
pub type GenerationId = u64;

/// 世代の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationState {
    Prepared,
    Committing,
    Active,
    Retired,
    Failed,
    RolledBack,
}

/// 世代の状態の管理。
#[derive(Debug, Clone)]
pub struct GenerationManager {
    states: BTreeMap<GenerationId, GenerationState>,
    next: GenerationId,
    active: Option<GenerationId>,
    committing: Option<GenerationId>,
}

/// 覚えておく世代の数（古い終わった世代は忘れる）。
const KEEP: usize = 32;

impl Default for GenerationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GenerationManager {
    pub fn new() -> Self {
        GenerationManager {
            states: BTreeMap::new(),
            next: 1,
            active: None,
            committing: None,
        }
    }

    pub fn state(&self, id: GenerationId) -> Option<GenerationState> {
        self.states.get(&id).copied()
    }

    pub fn active(&self) -> Option<GenerationId> {
        self.active
    }

    pub fn committing(&self) -> Option<GenerationId> {
        self.committing
    }

    /// 準備した世代を登録する。
    pub fn prepare(&mut self) -> GenerationId {
        let id = self.next;
        self.next += 1;
        self.states.insert(id, GenerationState::Prepared);
        self.prune();
        id
    }

    fn expect(&self, id: GenerationId, want: &[GenerationState]) -> Result<(), PipelineError> {
        match self.states.get(&id) {
            None => Err(PipelineError::Generation(format!(
                "unknown generation {id}"
            ))),
            Some(s) if want.contains(s) => Ok(()),
            Some(s) => Err(PipelineError::Generation(format!(
                "generation {id} is {s:?}, expected one of {want:?}"
            ))),
        }
    }

    /// 切り替えを始める。ほかの世代を切り替え中なら拒否する。
    pub fn begin_commit(&mut self, id: GenerationId) -> Result<(), PipelineError> {
        self.expect(id, &[GenerationState::Prepared])?;
        if let Some(c) = self.committing {
            return Err(PipelineError::Generation(format!(
                "generation {c} is still being committed"
            )));
        }
        self.states.insert(id, GenerationState::Committing);
        self.committing = Some(id);
        Ok(())
    }

    /// 切り替えが終わった。前の世代（あれば）を返す。
    pub fn complete_commit(
        &mut self,
        id: GenerationId,
    ) -> Result<Option<GenerationId>, PipelineError> {
        self.expect(id, &[GenerationState::Committing])?;
        let old = self.active.replace(id);
        if let Some(o) = old {
            self.states.insert(o, GenerationState::Retired);
        }
        self.states.insert(id, GenerationState::Active);
        self.committing = None;
        Ok(old)
    }

    /// 切り替えに失敗した（旧世代はそのまま）。
    pub fn fail_commit(&mut self, id: GenerationId) -> Result<(), PipelineError> {
        self.expect(
            id,
            &[GenerationState::Committing, GenerationState::Prepared],
        )?;
        self.states.insert(id, GenerationState::Failed);
        if self.committing == Some(id) {
            self.committing = None;
        }
        Ok(())
    }

    /// 切り替わっていない世代を捨てる。
    pub fn rollback(&mut self, id: GenerationId) -> Result<(), PipelineError> {
        self.expect(
            id,
            &[GenerationState::Prepared, GenerationState::Committing],
        )?;
        self.states.insert(id, GenerationState::RolledBack);
        if self.committing == Some(id) {
            self.committing = None;
        }
        Ok(())
    }

    fn prune(&mut self) {
        while self.states.len() > KEEP {
            let victim = self
                .states
                .iter()
                .find(|(_, s)| {
                    matches!(
                        s,
                        GenerationState::Retired
                            | GenerationState::Failed
                            | GenerationState::RolledBack
                    )
                })
                .map(|(k, _)| *k);
            match victim {
                Some(k) => {
                    self.states.remove(&k);
                }
                None => break,
            }
        }
    }
}

/// 出力のトラック（sink と、その中のトラック番号）。
pub type TrackKey = (String, u32);

/// AU をどうするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 出す。`switched` はこの AU で世代が切り替わった（新しい設定を一緒に出す）。
    Emit { switched: bool },
    /// 捨てる。
    Drop,
    /// 捨てて、その世代の符号化器にキーフレームを求める（時刻が前の出力より前だった）。
    DropRequestKeyframe,
}

#[derive(Debug, Clone)]
struct TrackSwitch {
    owner: GenerationId,
    last_us: Option<i64>,
    /// 次のキーフレームまで出さない（始まり・切り替えの取りやめの後）。
    need_key: bool,
}

/// 出力の切り替え。
#[derive(Debug, Clone, Default)]
pub struct OutputSwitch {
    active: Option<GenerationId>,
    pending: Option<(GenerationId, BTreeSet<TrackKey>)>,
    tracks: HashMap<TrackKey, TrackSwitch>,
}

fn pts_us(au: &AccessUnit) -> i64 {
    au.pts
        .rescale(au.timebase, Timebase::MICROS)
        .unwrap_or(au.pts)
}

impl OutputSwitch {
    pub fn new(active: GenerationId) -> Self {
        OutputSwitch {
            active: Some(active),
            pending: None,
            tracks: HashMap::new(),
        }
    }

    pub fn active(&self) -> Option<GenerationId> {
        self.active
    }

    pub fn pending(&self) -> Option<GenerationId> {
        self.pending.as_ref().map(|(g, _)| *g)
    }

    /// 新しい世代への切り替えを始める。`tracks` は新しい世代が出すトラック。
    pub fn begin(&mut self, gen_id: GenerationId, tracks: BTreeSet<TrackKey>) {
        if self.active.is_none() {
            self.active = Some(gen_id);
            return;
        }
        self.pending = Some((gen_id, tracks));
    }

    /// 切り替えをやめる（新しい世代の失敗・rollback）。すでに切り替えたトラックは古い世代に戻す
    /// （古い世代の次のキーフレームまでは出ない）。
    pub fn abort(&mut self) -> Option<GenerationId> {
        let (g, _) = self.pending.take()?;
        let active = self.active;
        for t in self.tracks.values_mut() {
            if t.owner == g
                && let Some(a) = active
            {
                t.owner = a;
                t.need_key = true;
            }
        }
        Some(g)
    }

    /// すべてのトラックが切り替わったか。切り替わったら新しい世代を active にして `true`。
    pub fn try_complete(&mut self) -> bool {
        let Some((g, want)) = &self.pending else {
            return false;
        };
        let done = want
            .iter()
            .all(|k| self.tracks.get(k).is_some_and(|t| t.owner == *g));
        if done {
            self.active = Some(*g);
            self.pending = None;
        }
        done
    }

    /// 世代 `gen_id` の AU を出すか決める。
    pub fn offer(&mut self, gen_id: GenerationId, key: &TrackKey, au: &AccessUnit) -> Decision {
        let t = pts_us(au);
        let pending = self.pending.as_ref().map(|(g, _)| *g);
        let Some(active) = self.active else {
            return Decision::Drop;
        };
        let tr = self.tracks.entry(key.clone()).or_insert(TrackSwitch {
            owner: active,
            last_us: None,
            need_key: true,
        });
        let wants_track = self
            .pending
            .as_ref()
            .is_some_and(|(_, keys)| keys.contains(key));
        if gen_id == tr.owner {
            // キーフレームから始める（始まりと、切り替えを取りやめた後）。
            if tr.need_key {
                if !au.keyframe {
                    return if tr.last_us.is_some() {
                        Decision::DropRequestKeyframe
                    } else {
                        Decision::Drop
                    };
                }
                tr.need_key = false;
            }
            if tr.last_us.is_some_and(|l| t <= l) && au.kind() == senca_core::MediaKind::Audio {
                return Decision::Drop;
            }
            tr.last_us = Some(tr.last_us.map_or(t, |l| l.max(t)));
            return Decision::Emit { switched: false };
        }
        if Some(gen_id) == pending && wants_track {
            if !au.keyframe {
                return Decision::Drop;
            }
            if tr.last_us.is_some_and(|l| t <= l) {
                return Decision::DropRequestKeyframe;
            }
            tr.owner = gen_id;
            tr.last_us = Some(t);
            tr.need_key = false;
            return Decision::Emit { switched: true };
        }
        Decision::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use senca_core::{Codec, TrackId};

    #[test]
    fn manager_transitions() {
        let mut m = GenerationManager::new();
        let g1 = m.prepare();
        assert_eq!(g1, 1);
        m.begin_commit(g1).unwrap();
        assert_eq!(m.complete_commit(g1).unwrap(), None);
        assert_eq!(m.active(), Some(1));
        // 2 を準備して commit 中に 3 は commit できない
        let g2 = m.prepare();
        let g3 = m.prepare();
        m.begin_commit(g2).unwrap();
        assert!(m.begin_commit(g3).is_err());
        // 2 が失敗: 1 が active のまま
        m.fail_commit(g2).unwrap();
        assert_eq!(m.active(), Some(1));
        assert_eq!(m.state(g2), Some(GenerationState::Failed));
        assert!(m.complete_commit(g2).is_err());
        // 3 を commit して完了: 1 は retired
        m.begin_commit(g3).unwrap();
        assert_eq!(m.complete_commit(g3).unwrap(), Some(1));
        assert_eq!(m.state(1), Some(GenerationState::Retired));
        assert_eq!(m.active(), Some(3));
        // active は rollback できない、知らない世代も
        assert!(m.rollback(3).is_err());
        assert!(m.rollback(99).is_err());
        let g4 = m.prepare();
        m.rollback(g4).unwrap();
        assert_eq!(m.state(g4), Some(GenerationState::RolledBack));
        assert!(m.begin_commit(g4).is_err());
        // たくさん作っても覚えておく数は有限
        for _ in 0..100 {
            let g = m.prepare();
            m.rollback(g).unwrap();
        }
        assert!(m.states.len() <= KEEP + 1);
        assert_eq!(m.active(), Some(3));
    }

    fn au(pts_ms: i64, key: bool, codec: Codec) -> AccessUnit {
        AccessUnit {
            track: TrackId(0),
            codec,
            epoch: 0,
            sequence: 0,
            config_generation: 0,
            pts: pts_ms,
            dts: pts_ms,
            duration: 0,
            timebase: Timebase::MILLIS,
            keyframe: key,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::new(),
        }
    }

    #[test]
    fn switch_per_track_at_keyframes() {
        let v: TrackKey = ("out".into(), 0);
        let a: TrackKey = ("out".into(), 1);
        let mut s = OutputSwitch::new(1);
        // 最初の映像はキーフレームから
        assert_eq!(s.offer(1, &v, &au(0, false, Codec::Vp9)), Decision::Drop);
        assert_eq!(
            s.offer(1, &v, &au(33, true, Codec::Vp9)),
            Decision::Emit { switched: false }
        );
        assert_eq!(
            s.offer(1, &a, &au(20, true, Codec::Opus)),
            Decision::Emit { switched: false }
        );
        s.begin(2, [v.clone(), a.clone()].into_iter().collect());
        // 新しい世代の非キーフレームは捨てる、古い時刻のキーフレームはキーフレームを求める
        assert_eq!(s.offer(2, &v, &au(66, false, Codec::Vp9)), Decision::Drop);
        assert_eq!(
            s.offer(1, &v, &au(66, false, Codec::Vp9)),
            Decision::Emit { switched: false }
        );
        assert_eq!(
            s.offer(2, &v, &au(66, true, Codec::Vp9)),
            Decision::DropRequestKeyframe
        );
        assert_eq!(
            s.offer(2, &v, &au(100, true, Codec::Vp9)),
            Decision::Emit { switched: true }
        );
        // 映像は切り替わった: 古い世代の映像は捨てる、音声はまだ古い世代
        assert_eq!(s.offer(1, &v, &au(133, false, Codec::Vp9)), Decision::Drop);
        assert!(!s.try_complete());
        assert_eq!(
            s.offer(1, &a, &au(40, true, Codec::Opus)),
            Decision::Emit { switched: false }
        );
        assert_eq!(
            s.offer(2, &a, &au(60, true, Codec::Opus)),
            Decision::Emit { switched: true }
        );
        assert_eq!(s.offer(1, &a, &au(60, true, Codec::Opus)), Decision::Drop);
        assert!(s.try_complete());
        assert_eq!(s.active(), Some(2));
        assert_eq!(
            s.offer(2, &v, &au(133, false, Codec::Vp9)),
            Decision::Emit { switched: false }
        );
        // 終わった世代・知らない世代は捨てる
        assert_eq!(s.offer(1, &v, &au(166, true, Codec::Vp9)), Decision::Drop);
        assert_eq!(s.offer(7, &v, &au(166, true, Codec::Vp9)), Decision::Drop);
    }

    #[test]
    fn abort_returns_tracks_to_old_generation() {
        let v: TrackKey = ("out".into(), 0);
        let mut s = OutputSwitch::new(1);
        s.offer(1, &v, &au(0, true, Codec::Vp9));
        s.begin(2, [v.clone()].into_iter().collect());
        assert_eq!(
            s.offer(2, &v, &au(33, true, Codec::Vp9)),
            Decision::Emit { switched: true }
        );
        assert_eq!(s.abort(), Some(2));
        assert_eq!(s.pending(), None);
        assert_eq!(s.offer(2, &v, &au(66, true, Codec::Vp9)), Decision::Drop);
        assert_eq!(
            s.offer(1, &v, &au(66, true, Codec::Vp9)),
            Decision::Emit { switched: false }
        );
        assert_eq!(s.active(), Some(1));
    }
}
