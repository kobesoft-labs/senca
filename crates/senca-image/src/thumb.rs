//! サムネイルの選択（R6-9）。
//!
//! 候補の輝度から「黒・白・平坦」を除き、鮮明さ（ラプラシアンの分散）とコントラスト・情報量で
//! 点数を付ける。似た画（縮小した指紋の平均差が小さいもの）は 1 つにまとめる。
//! 解析は一定の幅（既定 320 画素）に縮めてから行い、解像度やノイズの違いの影響を減らす。

use crate::color::ColorRange;
use crate::error::Result;
use crate::frame::Plane;
use crate::metrics::{ClassifyParams, FrameClass, classify_stats, laplacian_variance, luma_stats};
use crate::scale::{ScaleFilter, scale_plane};

/// 選択のパラメーター。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThumbnailParams {
    pub classify: ClassifyParams,
    /// 解析用に縮める幅。
    pub analysis_width: usize,
    /// 指紋（32x18）の平均絶対差がこれ未満なら「ほぼ同じ画」。
    pub duplicate_threshold: f32,
    /// 直前の候補との差（動き・場面の途中）に掛ける減点。
    pub motion_penalty: f64,
    /// 保持する候補の最大数（超えたら点数の低いものから捨てる）。
    pub max_candidates: usize,
}

impl Default for ThumbnailParams {
    fn default() -> Self {
        Self {
            classify: ClassifyParams::default(),
            analysis_width: 320,
            duplicate_threshold: 6.0,
            motion_penalty: 1.0,
            max_candidates: 256,
        }
    }
}

/// 1 候補の評価。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CandidateScore {
    pub class: FrameClass,
    /// ラプラシアンの分散（解析解像度）。
    pub sharpness: f64,
    /// 輝度の標準偏差（0〜1）。
    pub contrast: f64,
    pub entropy: f64,
    /// 直前の候補との指紋の平均差（0〜255）。
    pub motion: f64,
    /// 総合点（大きいほど良い）。黒・白・平坦は負の無限大。
    pub score: f64,
}

#[derive(Debug, Clone)]
struct Entry {
    id: u64,
    score: CandidateScore,
    sig: Vec<u8>,
}

const SIG_W: usize = 32;
const SIG_H: usize = 18;

fn signature(p: &Plane) -> Result<Vec<u8>> {
    Ok(scale_plane(p, SIG_W, SIG_H, ScaleFilter::Area)?.to_packed())
}

fn sig_diff(a: &[u8], b: &[u8]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(&x, &y)| (x as i32 - y as i32).unsigned_abs() as f64)
        .sum::<f64>()
        / a.len() as f64
}

/// 候補を順に入れて、最良のサムネイルを選ぶ状態つきの選択器。
#[derive(Debug, Clone)]
pub struct ThumbnailSelector {
    params: ThumbnailParams,
    range: ColorRange,
    entries: Vec<Entry>,
    prev_sig: Option<Vec<u8>>,
}

impl ThumbnailSelector {
    pub fn new(params: ThumbnailParams, range: ColorRange) -> Self {
        Self {
            params,
            range,
            entries: Vec::new(),
            prev_sig: None,
        }
    }

    /// 候補（輝度）を評価して記録する。`id` は呼び出し側の識別子（時刻など）。
    pub fn push(&mut self, id: u64, luma: &Plane) -> Result<CandidateScore> {
        let aw = self.params.analysis_width.clamp(16, 4096).min(luma.width());
        let ah = ((luma.height() * aw) / luma.width()).max(8);
        let small = scale_plane(luma, aw, ah, ScaleFilter::Area)?;
        let stats = luma_stats(&small);
        let class = classify_stats(&stats, self.range, &self.params.classify);
        let sharpness = laplacian_variance(&small);
        let span = (self.range.white() - self.range.black()) as f64;
        let contrast = stats.std / span;
        let sig = signature(&small)?;
        let motion = self.prev_sig.as_ref().map_or(0.0, |p| sig_diff(p, &sig));
        self.prev_sig = Some(sig.clone());
        let score = if class == FrameClass::Normal {
            (1.0 + sharpness).ln() + 4.0 * contrast + 0.3 * stats.entropy
                - self.params.motion_penalty * (motion / 32.0).min(2.0)
        } else {
            f64::NEG_INFINITY
        };
        let s = CandidateScore {
            class,
            sharpness,
            contrast,
            entropy: stats.entropy,
            motion,
            score,
        };
        self.entries.push(Entry { id, score: s, sig });
        if self.entries.len() > self.params.max_candidates.max(1) {
            // 最も低い点の候補を捨てる。
            if let Some((i, _)) = self
                .entries
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.score.score.total_cmp(&b.1.score.score))
            {
                self.entries.remove(i);
            }
        }
        Ok(s)
    }

    /// 候補の数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 最良の候補の id。黒・白・平坦しかなければ None。
    pub fn best(&self) -> Option<u64> {
        self.pick_diverse(1).first().copied()
    }

    /// 似た画を避けて、点の高い順に最大 `k` 個の id を返す（黒・白・平坦は含めない）。
    pub fn pick_diverse(&self, k: usize) -> Vec<u64> {
        let mut order: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| e.score.score.is_finite())
            .collect();
        order.sort_by(|a, b| b.score.score.total_cmp(&a.score.score));
        let mut picked: Vec<&Entry> = Vec::new();
        for e in order {
            if picked.len() >= k {
                break;
            }
            if picked
                .iter()
                .all(|p| sig_diff(&p.sig, &e.sig) >= self.params.duplicate_threshold as f64)
            {
                picked.push(e);
            }
        }
        picked.into_iter().map(|e| e.id).collect()
    }
}

/// 輝度の候補の中から最良の 1 枚の添字を選ぶ（黒・白・平坦しかなければ None）。
pub fn select_thumbnail(frames: &[&Plane], range: ColorRange) -> Result<Option<usize>> {
    let mut s = ThumbnailSelector::new(
        ThumbnailParams {
            motion_penalty: 0.0,
            ..Default::default()
        },
        range,
    );
    for (i, f) in frames.iter().enumerate() {
        s.push(i as u64, f)?;
    }
    Ok(s.best().map(|i| i as usize))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::box_blur;

    fn scene(w: usize, h: usize, seed: u32) -> Plane {
        let mut s = seed;
        let mut p = Plane::from_fn(w, h, |x, y| (60 + (x * 100 / w) + (y * 50 / h)) as u8).unwrap();
        // ランダムな矩形と線を描く
        for _ in 0..30 {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            let x0 = (s >> 8) as usize % w;
            let y0 = (s >> 20) as usize % h;
            let v = (s >> 3) as u8 | 16;
            for y in y0..(y0 + 10).min(h) {
                for x in x0..(x0 + 25).min(w) {
                    p.set(x, y, v.clamp(16, 235));
                }
            }
        }
        p
    }

    #[test]
    fn rejects_black_white_flat_and_prefers_sharp() {
        let black = Plane::filled(320, 180, 16).unwrap();
        let white = Plane::filled(320, 180, 235).unwrap();
        let flat = Plane::filled(320, 180, 128).unwrap();
        let sharp = scene(320, 180, 7);
        let blurry = box_blur(&sharp, 3);
        let frames = [&black, &blurry, &white, &sharp, &flat];
        assert_eq!(
            select_thumbnail(&frames, ColorRange::Limited).unwrap(),
            Some(3)
        );
        assert_eq!(
            select_thumbnail(&[&black, &white], ColorRange::Limited).unwrap(),
            None
        );
    }

    #[test]
    fn diverse_skips_near_duplicates() {
        let a = scene(320, 180, 1);
        let mut a2 = a.clone();
        a2.map_in_place(|v| v.saturating_add(1));
        let b = scene(320, 180, 99);
        let mut s = ThumbnailSelector::new(
            ThumbnailParams {
                motion_penalty: 0.0,
                ..Default::default()
            },
            ColorRange::Limited,
        );
        s.push(10, &a).unwrap();
        s.push(11, &a2).unwrap();
        s.push(12, &b).unwrap();
        let picks = s.pick_diverse(3);
        assert_eq!(picks.len(), 2);
        assert!(picks.contains(&12));
    }
}
