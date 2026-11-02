//! 板書の変化検出（R6-8: 板書が変わって落ち着いたら静止画を保存する）。
//!
//! 1 フレームごとに（補正後の輝度を）縮小し、局所の背景との差で「字の地図」（2 値）を作る。
//! - 人が前を横切ると大きな塊が出る。外接矩形が大きい塊は「隠れ」とみなし、その範囲は判断しない。
//! - 直近 K フレームの多数決（時間方向の中央値）で地図を安定させる。
//! - 「直前の保存からの変化が一定以上」かつ「N フレーム続けて地図がほぼ変わらない」ときに事象を出す。
//!
//! 1 画素のずれは変化に数えない（3x3 の近傍に同じ値があれば一致とみなす）。

use std::collections::VecDeque;

use senca_image::filter::box_blur_f32;
use senca_image::scale::{ScaleFilter, scale_plane};
use senca_image::{ColorRange, Plane, PlaneF32, Result};

use crate::enhance::BoardKind;

/// 変化検出のパラメーター。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChangeParams {
    pub kind: BoardKind,
    pub range: ColorRange,
    /// 解析の幅。
    pub analysis_width: usize,
    /// 局所の背景との差（0〜1）がこれ以上なら字。
    pub stroke_threshold: f32,
    /// 多数決に使うフレーム数（奇数が望ましい）。
    pub history: usize,
    /// 何フレーム落ち着いたら事象を出すか。
    pub stable_frames: u32,
    /// 保存からの変化（画素の割合）がこれ以上で「変わった」。
    pub change_fraction: f64,
    /// 連続するフレームの差（画素の割合）がこれ以下で「落ち着いた」。
    pub stable_fraction: f64,
    /// 外接矩形の幅または高さが画面のこの割合を超える塊は「人などの隠れ」。
    pub occluder_extent: f64,
    /// 隠れの割合がこれを超える間は事象を出さない。
    pub max_occluded: f64,
}

impl Default for ChangeParams {
    fn default() -> Self {
        Self {
            kind: BoardKind::Whiteboard,
            range: ColorRange::Limited,
            analysis_width: 160,
            stroke_threshold: 0.12,
            history: 5,
            stable_frames: 5,
            change_fraction: 0.004,
            stable_fraction: 0.001,
            occluder_extent: 0.25,
            max_occluded: 0.35,
        }
    }
}

/// 1 フレームの判定。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChangeReport {
    /// 直前の保存からの変化（画素の割合）。
    pub changed_fraction: f64,
    /// 増えた字（割合）。
    pub added_fraction: f64,
    /// 消えた字（割合）。
    pub removed_fraction: f64,
    /// 何フレーム落ち着いているか。
    pub stable_for: u32,
    /// 隠れている割合。
    pub occluded_fraction: f64,
    /// 今のフレームを保存すべき（変わって、落ち着いた）。
    pub snapshot: bool,
}

/// 状態つきの板書の変化検出。
#[derive(Debug, Clone)]
pub struct BoardChangeDetector {
    params: ChangeParams,
    dims: Option<(usize, usize)>,
    /// 0 = 地、1 = 字、2 = 不明（隠れ）。
    history: VecDeque<Vec<u8>>,
    stable_map: Vec<u8>,
    prev_stable: Vec<u8>,
    snapshot_map: Vec<u8>,
    stable_for: u32,
}

const UNKNOWN: u8 = 2;

impl BoardChangeDetector {
    pub fn new(params: ChangeParams) -> Self {
        Self {
            params,
            dims: None,
            history: VecDeque::new(),
            stable_map: Vec::new(),
            prev_stable: Vec::new(),
            snapshot_map: Vec::new(),
            stable_for: 0,
        }
    }

    /// 状態を捨てる（カメラの切り替え・補正の変更のとき）。
    pub fn reset(&mut self) {
        *self = Self::new(self.params);
    }

    /// 解析の解像度の字の地図（0/1/2）と大きさ。試験・表示用。
    pub fn stable_map(&self) -> Option<(usize, usize, &[u8])> {
        self.dims.map(|(w, h)| (w, h, &self.stable_map[..]))
    }

    /// 補正後の輝度を 1 フレーム入れる。
    pub fn push(&mut self, luma: &Plane) -> Result<ChangeReport> {
        let p = &self.params;
        let aw = p.analysis_width.clamp(16, 1024).min(luma.width());
        let ah = ((luma.height() * aw) / luma.width()).max(4);
        if self.dims != Some((aw, ah)) {
            let keep = self.params;
            *self = Self::new(keep);
            self.dims = Some((aw, ah));
            self.stable_map = vec![0; aw * ah];
            self.prev_stable = vec![0; aw * ah];
            self.snapshot_map = vec![0; aw * ah];
        }
        let p = self.params;
        let map = stroke_map(luma, aw, ah, &p)?;
        self.history.push_back(map);
        while self.history.len() > p.history.max(1) {
            self.history.pop_front();
        }
        // 多数決
        let n = aw * ah;
        let mut occluded = 0usize;
        let mut cur = vec![0u8; n];
        for (i, c) in cur.iter_mut().enumerate() {
            let (mut ones, mut known) = (0, 0);
            for m in &self.history {
                if m[i] != UNKNOWN {
                    known += 1;
                    ones += m[i] as usize;
                }
            }
            let latest_unknown = self.history.back().is_some_and(|m| m[i] == UNKNOWN);
            if latest_unknown {
                occluded += 1;
            }
            *c = if known == 0 {
                self.stable_map[i]
            } else if 2 * ones > known {
                1
            } else if 2 * ones == known {
                self.stable_map[i]
            } else {
                0
            };
        }
        self.prev_stable = std::mem::replace(&mut self.stable_map, cur);
        let latest = self.history.back().cloned().unwrap_or_default();
        let unknown = |i: usize| latest.get(i) == Some(&UNKNOWN);
        let step = diff_count(&self.prev_stable, &self.stable_map, aw, ah, &unknown).0;
        if (step as f64) <= p.stable_fraction * n as f64 {
            self.stable_for = self.stable_for.saturating_add(1);
        } else {
            self.stable_for = 0;
        }
        let (added, removed) = diff_count(&self.snapshot_map, &self.stable_map, aw, ah, &unknown);
        let changed = (added + removed) as f64 / n as f64;
        let occ = occluded as f64 / n as f64;
        let snapshot = changed >= p.change_fraction
            && self.stable_for >= p.stable_frames
            && occ <= p.max_occluded
            && self.history.len() >= p.history.max(1);
        if snapshot {
            self.snapshot_map = self.stable_map.clone();
        }
        Ok(ChangeReport {
            changed_fraction: changed,
            added_fraction: added as f64 / n as f64,
            removed_fraction: removed as f64 / n as f64,
            stable_for: self.stable_for,
            occluded_fraction: occ,
            snapshot,
        })
    }
}

/// a → b の (増えた, 消えた) 画素数。1 画素のずれは数えない。`skip` の画素は数えない。
fn diff_count(
    a: &[u8],
    b: &[u8],
    w: usize,
    h: usize,
    skip: &dyn Fn(usize) -> bool,
) -> (usize, usize) {
    let near = |m: &[u8], x: usize, y: usize, v: u8| -> bool {
        for dy in -1..=1isize {
            for dx in -1..=1isize {
                let (xx, yy) = (x as isize + dx, y as isize + dy);
                if xx >= 0
                    && yy >= 0
                    && (xx as usize) < w
                    && (yy as usize) < h
                    && m[yy as usize * w + xx as usize] == v
                {
                    return true;
                }
            }
        }
        false
    };
    let (mut add, mut rem) = (0, 0);
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if skip(i) || a[i] == b[i] {
                continue;
            }
            if b[i] == 1 && !near(a, x, y, 1) {
                add += 1;
            } else if b[i] == 0 && a[i] == 1 && !near(b, x, y, 1) {
                rem += 1;
            }
        }
    }
    (add, rem)
}

/// 字の地図（0 地、1 字、2 隠れ）を作る。
fn stroke_map(luma: &Plane, aw: usize, ah: usize, p: &ChangeParams) -> Result<Vec<u8>> {
    let small = scale_plane(luma, aw, ah, ScaleFilter::Area)?;
    let mut f = PlaneF32::from_plane(&small);
    for v in f.data.iter_mut() {
        *v = p.range.normalize_luma(*v);
    }
    let r = (aw / 20).max(3);
    let bg = box_blur_f32(&f, r);
    let mut map: Vec<u8> = f
        .data
        .iter()
        .zip(&bg.data)
        .map(|(&v, &b)| {
            let d = match p.kind {
                BoardKind::Whiteboard => b - v,
                BoardKind::Blackboard => v - b,
            };
            // 地から大きく外れた暗い／明るい面（人の体）も字の候補に入れて塊として扱う。
            let far = match p.kind {
                BoardKind::Whiteboard => v < 0.45,
                BoardKind::Blackboard => v > 0.6,
            };
            u8::from(d >= p.stroke_threshold || far)
        })
        .collect();
    // 連結成分（8 連結）。外接矩形が大きいものは隠れ。
    let mut label = vec![u32::MAX; aw * ah];
    let mut stack = Vec::new();
    let ext_w = (p.occluder_extent * aw as f64) as usize;
    let ext_h = (p.occluder_extent * ah as f64) as usize;
    let mut boxes: Vec<(usize, usize, usize, usize)> = Vec::new();
    for start in 0..aw * ah {
        if map[start] != 1 || label[start] != u32::MAX {
            continue;
        }
        let id = boxes.len() as u32;
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        label[start] = id;
        stack.push(start);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % aw, i / aw);
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
            for dy in -1..=1isize {
                for dx in -1..=1isize {
                    let (xx, yy) = (x as isize + dx, y as isize + dy);
                    if xx < 0 || yy < 0 || xx as usize >= aw || yy as usize >= ah {
                        continue;
                    }
                    let j = yy as usize * aw + xx as usize;
                    if map[j] == 1 && label[j] == u32::MAX {
                        label[j] = id;
                        stack.push(j);
                    }
                }
            }
        }
        boxes.push((x0, y0, x1, y1));
    }
    let margin = r;
    for &(x0, y0, x1, y1) in &boxes {
        if x1 - x0 + 1 > ext_w || y1 - y0 + 1 > ext_h {
            let (xa, ya) = (x0.saturating_sub(margin), y0.saturating_sub(margin));
            let (xb, yb) = ((x1 + margin).min(aw - 1), (y1 + margin).min(ah - 1));
            for y in ya..=yb {
                for v in &mut map[y * aw + xa..=y * aw + xb] {
                    *v = UNKNOWN;
                }
            }
        }
    }
    Ok(map)
}
