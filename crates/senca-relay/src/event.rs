//! チャネルで運ぶもの（ノードの hub の `HubEvent` と 1 対 1 に写せる）と、トラックの選び方。

use senca_core::{AccessUnit, CodecConfig, TrackId};

/// 上流から届く・下流へ渡すもの。`Lost` 以外はノードの `HubEvent` と同じ意味。
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Config(TrackId, CodecConfig),
    Au(AccessUnit),
    /// 送り手が切れた・入れ替わった（epoch が変わる）。
    Discontinuity,
    /// 配信が終わった。
    End,
    /// 元のノードの HLS のセグメントが閉じた。
    Segment {
        seq: u64,
        start_us: i64,
        duration_us: u64,
        discontinuity: bool,
    },
    /// 上流を失った（接続が切れた・相手が退く・購読を断られた）。統合側は別のノードから購読し直す（ADR-0013）。
    Lost(LostReason),
}

/// 上流を失った理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LostReason {
    /// 接続が切れた（時間切れ・I/O の誤り・規約違反）。
    ConnectionLost(String),
    /// 相手が GoAway を送った。流れは続くが、早めに別のノードへ移る（make-before-break）。
    GoingAway,
    /// 購読を断られた（stream が無い・所有権が移った・過負荷）。
    Rejected { code: u32, reason: String },
}

impl LostReason {
    /// 流れがまだ続いているか（GoAway の間だけ true）。
    pub fn is_graceful(&self) -> bool {
        matches!(self, LostReason::GoingAway)
    }
}

/// どのトラック（サイマルキャストの層を含む）を受けるか。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TrackSel {
    #[default]
    All,
    /// 昇順・重複なし。
    Only(Vec<TrackId>),
}

/// 1 つの選択に並べられるトラックの上限。
pub const MAX_SEL_TRACKS: usize = 64;

impl TrackSel {
    pub fn only(tracks: impl IntoIterator<Item = TrackId>) -> TrackSel {
        let mut v: Vec<TrackId> = tracks.into_iter().collect();
        v.sort();
        v.dedup();
        TrackSel::Only(v)
    }

    pub fn contains(&self, t: TrackId) -> bool {
        match self {
            TrackSel::All => true,
            TrackSel::Only(v) => v.binary_search(&t).is_ok(),
        }
    }

    /// 和。
    pub fn union(&self, other: &TrackSel) -> TrackSel {
        match (self, other) {
            (TrackSel::All, _) | (_, TrackSel::All) => TrackSel::All,
            (TrackSel::Only(a), TrackSel::Only(b)) => {
                TrackSel::only(a.iter().chain(b.iter()).copied())
            }
        }
    }

    /// `self` にあって `prev` に無いトラック（`All` が絡むときは判定できないので None）。
    pub fn added_since(&self, prev: &TrackSel) -> Option<Vec<TrackId>> {
        match (self, prev) {
            (TrackSel::Only(a), TrackSel::Only(b)) => Some(
                a.iter()
                    .copied()
                    .filter(|t| b.binary_search(t).is_err())
                    .collect(),
            ),
            (TrackSel::Only(_), TrackSel::All) => Some(Vec::new()),
            _ => None,
        }
    }
}
