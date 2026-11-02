//! 重なったチャンクの認識結果を、重複のない 1 本の字幕（[`Cue`]）にまとめる。
//!
//! # 重複の判定
//!
//! 隣り合うチャンクは重なり（既定 1 秒）を持つので、同じ発話が 2 回認識される。次のどれかなら同じ発話とみなす。
//!
//! - 時間の重なりが短い方の 3 割以上で、文字列の類似度（[`similarity`](crate::text::similarity)）がしきい値以上
//! - 時間の重なりが短い方の 8 割以上（文字列が違っても。境界で切れた語の認識違い）
//! - 時間は重ならないが、先頭の差が重なりの長さ以内で、文字列がほぼ同じ（時刻の揺れ）
//!
//! 同じ発話なら、チャンクの端から遠い方（端で切れていない方）を残す。
//!
//! # 確定
//!
//! 区切りの終わりが「最新のチャンクの終わり − 重なり」より前になったら、もう次のチャンクと重ならないので確定し、
//! ID を振って返す。欠落（捨てたチャンク・不連続）があれば、待っている区切りを全部確定する。
//!
//! # 分け方
//!
//! 確定した区切りは、表示幅（既定 1 行 32 全角 = 幅 64）・行数（既定 2 行）・長さ（既定 7 秒）に収まるように分け、
//! 時間は文字の幅に比例して配る。Cue は時刻順で重ならない。

use crate::asr::Transcript;
use crate::chunk::ChunkInfo;
use crate::error::{Error, Result};
use crate::text::{normalize_spaces, similarity, split_pages, text_width};
use std::collections::VecDeque;

/// 字幕の 1 件（媒体時刻、µs）。翻訳した Cue は元と同じ `id`・時刻を持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub id: u64,
    pub start_us: i64,
    pub end_us: i64,
    /// 行は `\n` で区切る。
    pub text: String,
}

/// まとめ方の設定。
#[derive(Debug, Clone, PartialEq)]
pub struct MergeConfig {
    /// チャンクの重なり（[`ChunkerConfig::overlap_us`](crate::chunk::ChunkerConfig::overlap_us) と同じ値）。
    pub overlap_us: u64,
    pub max_cue_duration_us: u64,
    /// 短すぎる Cue はこの長さまで延ばす（次の Cue とは重ねない）。
    pub min_cue_duration_us: u64,
    /// 1 行の幅（半角 1・全角 2）。32 全角なら 64。
    pub max_line_width: u32,
    pub max_lines: usize,
    /// 文字列の類似度のしきい値。
    pub dup_similarity: f32,
    /// 待つ区切りの上限（超えたら古いものから確定する）。
    pub max_pending: usize,
}

impl Default for MergeConfig {
    fn default() -> Self {
        MergeConfig {
            overlap_us: 1_000_000,
            max_cue_duration_us: 7_000_000,
            min_cue_duration_us: 700_000,
            max_line_width: 64,
            max_lines: 2,
            dup_similarity: 0.6,
            max_pending: 256,
        }
    }
}

impl MergeConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_cue_duration_us < 1_000_000 || self.min_cue_duration_us > self.max_cue_duration_us {
            return Err(Error::InvalidConfig("cue duration limits"));
        }
        if !(8..=200).contains(&self.max_line_width) || !(1..=4).contains(&self.max_lines) {
            return Err(Error::InvalidConfig("line limits"));
        }
        if !(0.0..=1.0).contains(&self.dup_similarity) || self.max_pending == 0 {
            return Err(Error::InvalidConfig("merge thresholds"));
        }
        Ok(())
    }
}

/// 数え上げ。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeStats {
    pub segments_in: u64,
    pub duplicates: u64,
    /// 確定済みの範囲より古くて捨てた区切り。
    pub stale: u64,
    pub cues_out: u64,
}

#[derive(Debug, Clone)]
struct Seg {
    start: i64,
    end: i64,
    text: String,
    /// チャンクの端からの距離（大きいほど切れていない）。
    quality: i64,
}

/// 認識結果をまとめる状態機械。結果は**チャンクの順に**入れる。
#[derive(Debug)]
pub struct CueMerger {
    cfg: MergeConfig,
    pending: Vec<Seg>,
    /// 最近確定した区切り（遅れて来た重複を捨てるため）。
    recent: VecDeque<Seg>,
    last_seq: Option<u64>,
    last_chunk_end: Option<i64>,
    /// 出した Cue の終わり（次の Cue はここから後）。
    last_cue_end: i64,
    next_id: u64,
    stats: MergeStats,
}

const RECENT: usize = 16;

impl CueMerger {
    pub fn new(cfg: MergeConfig) -> Result<Self> {
        cfg.validate()?;
        Ok(CueMerger {
            cfg,
            pending: Vec::new(),
            recent: VecDeque::new(),
            last_seq: None,
            last_chunk_end: None,
            last_cue_end: i64::MIN,
            next_id: 1,
            stats: MergeStats::default(),
        })
    }

    pub fn stats(&self) -> &MergeStats {
        &self.stats
    }

    /// 確定済みの範囲の終わり（媒体時刻）。ここまでの字幕はもう変わらない。
    pub fn finalized_until(&self) -> Option<i64> {
        (self.last_cue_end != i64::MIN).then_some(self.last_cue_end)
    }

    /// 1 チャンクの結果を入れ、確定した Cue を返す。
    pub fn push(&mut self, info: &ChunkInfo, t: &Transcript) -> Result<Vec<Cue>> {
        if self.last_seq.is_some_and(|s| info.seq <= s) {
            return Err(Error::InvalidInput("chunk results must be pushed in sequence order"));
        }
        let mut out = Vec::new();
        let broken = info.dropped_before > 0 || info.discontinuity || self.last_chunk_end.is_some_and(|e| info.start_us >= e);
        if broken {
            self.finalize_all(&mut out);
        }
        let chunk_end = info.end_us();
        for s in &t.segments {
            self.stats.segments_in += 1;
            let start = info.start_us.saturating_add(s.start_us);
            let end = info.start_us.saturating_add(s.end_us);
            let text = normalize_spaces(&s.text);
            if text.is_empty() {
                continue;
            }
            let cand = Seg { start, end, quality: (start - info.start_us).min(chunk_end - end), text };
            if self.recent.iter().any(|r| self.is_dup(r, &cand)) {
                self.stats.duplicates += 1;
                continue;
            }
            if cand.end <= self.last_cue_end {
                self.stats.stale += 1;
                continue;
            }
            if let Some(i) = self.pending.iter().position(|p| self.is_dup(p, &cand)) {
                self.stats.duplicates += 1;
                if cand.quality > self.pending[i].quality {
                    self.pending[i] = cand;
                }
                continue;
            }
            self.pending.push(cand);
        }
        self.pending.sort_by_key(|s| (s.start, s.end));
        let horizon = chunk_end - self.cfg.overlap_us as i64;
        // 先頭から順に、地平より前に終わるものを確定する（順序を保つため、途中で止まる）
        let n = self.pending.iter().take_while(|s| s.end <= horizon).count();
        let excess = self.pending.len().saturating_sub(self.cfg.max_pending);
        let take: Vec<Seg> = self.pending.drain(..n.max(excess)).collect();
        for s in take {
            self.finalize(s, &mut out);
        }
        self.last_seq = Some(info.seq);
        self.last_chunk_end = Some(chunk_end);
        Ok(out)
    }

    /// 終わり: 待っている区切りを全部確定する。
    pub fn flush(&mut self) -> Vec<Cue> {
        let mut out = Vec::new();
        self.finalize_all(&mut out);
        out
    }

    fn finalize_all(&mut self, out: &mut Vec<Cue>) {
        let all = std::mem::take(&mut self.pending);
        for s in all {
            self.finalize(s, out);
        }
    }

    fn is_dup(&self, a: &Seg, b: &Seg) -> bool {
        let ov = (a.end.min(b.end) - a.start.max(b.start)).max(0);
        let min_dur = (a.end - a.start).min(b.end - b.start).max(1);
        let r = ov as f64 / min_dur as f64;
        let sim = similarity(&a.text, &b.text);
        (r >= 0.3 && sim >= self.cfg.dup_similarity)
            || r >= 0.8
            || (ov == 0 && (a.start - b.start).unsigned_abs() <= self.cfg.overlap_us && sim >= 0.85)
    }

    fn finalize(&mut self, s: Seg, out: &mut Vec<Cue>) {
        let start = if self.last_cue_end == i64::MIN { s.start } else { s.start.max(self.last_cue_end) };
        let mut end = s.end.max(start);
        if ((end - start) as u64) < self.cfg.min_cue_duration_us {
            end = start + self.cfg.min_cue_duration_us as i64;
        }
        let pieces = self.split(&s.text, (end - start) as u64);
        let total: u64 = pieces.iter().map(|p| text_width(p).max(1) as u64).sum::<u64>().max(1);
        let dur = (end - start) as u64;
        let mut t = start;
        let mut acc = 0u64;
        let count = pieces.len();
        for (i, p) in pieces.into_iter().enumerate() {
            acc += text_width(&p).max(1) as u64;
            let piece_end = if i + 1 == count { end } else { start + (dur as u128 * acc as u128 / total as u128) as i64 };
            let shown_end = piece_end.min(t + self.cfg.max_cue_duration_us as i64);
            if shown_end > t {
                out.push(Cue { id: self.next_id, start_us: t, end_us: shown_end, text: p });
                self.next_id += 1;
                self.stats.cues_out += 1;
            }
            t = piece_end;
        }
        self.last_cue_end = end;
        self.recent.push_back(s);
        if self.recent.len() > RECENT {
            self.recent.pop_front();
        }
    }

    /// 行・長さの上限に収まるページに分ける。長い発話は、時間の上限に合わせてページを細かくする。
    fn split(&self, text: &str, dur: u64) -> Vec<String> {
        let mut width = self.cfg.max_line_width;
        let need = dur.div_ceil(self.cfg.max_cue_duration_us).max(1) as usize;
        let mut pages = split_pages(text, width, self.cfg.max_lines);
        let total = text_width(text);
        while pages.len() < need && width > 8 {
            let target = (total / (need as u32 * self.cfg.max_lines as u32)).max(8);
            width = if target < width { target } else { width - 2 };
            pages = split_pages(text, width, self.cfg.max_lines);
        }
        pages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asr::AsrSegment;

    fn info(seq: u64, start_s: f64) -> ChunkInfo {
        ChunkInfo { seq, start_us: (start_s * 1e6) as i64, duration_us: 5_000_000, dropped_before: 0, discontinuity: false }
    }

    fn tr(segs: &[(f64, f64, &str)]) -> Transcript {
        Transcript {
            language: None,
            segments: segs
                .iter()
                .map(|&(s, e, t)| AsrSegment {
                    start_us: (s * 1e6) as i64,
                    end_us: (e * 1e6) as i64,
                    text: t.into(),
                    no_speech_prob: None,
                    avg_logprob: None,
                })
                .collect(),
        }
    }

    fn texts(c: &[Cue]) -> Vec<String> {
        c.iter().map(|c| c.text.replace('\n', "")).collect()
    }

    fn check_monotonic(c: &[Cue]) {
        for w in c.windows(2) {
            assert!(w[0].end_us <= w[1].start_us, "{w:?}");
            assert!(w[0].id < w[1].id);
        }
        for x in c {
            assert!(x.end_us > x.start_us);
        }
    }

    #[test]
    fn overlapping_chunks_dedup() {
        let mut m = CueMerger::new(MergeConfig::default()).unwrap();
        let mut all = Vec::new();
        // チャンク 0: 0〜5 秒。最後の発話は境界で切れている
        all.extend(m.push(&info(0, 0.0), &tr(&[(0.2, 2.0, "皆さんこんにちは。"), (2.2, 3.6, "今日は微分の話をします。"), (4.3, 5.0, "まず最初に")])).unwrap());
        // チャンク 1: 4〜9 秒（重なり 1 秒）。同じ発話がチャンクの中ほどで完全に認識される
        all.extend(m.push(&info(1, 4.0), &tr(&[(0.3, 1.9, "まず最初に定義を確認します。"), (2.0, 4.5, "関数の変化の割合を考えます。")])).unwrap());
        // チャンク 2: 8〜13 秒。前の発話の後半がまた出る
        all.extend(m.push(&info(2, 8.0), &tr(&[(0.0, 0.5, "割合を考えます。"), (1.0, 3.0, "これが微分係数です。")])).unwrap());
        all.extend(m.flush());
        assert_eq!(
            texts(&all),
            vec!["皆さんこんにちは。", "今日は微分の話をします。", "まず最初に定義を確認します。", "関数の変化の割合を考えます。", "これが微分係数です。"]
        );
        check_monotonic(&all);
        assert!(m.stats().duplicates >= 2);
        // 媒体時刻に直っている
        assert_eq!(all[2].start_us, 4_300_000);
        assert_eq!(all[4].start_us, 9_000_000);
    }

    #[test]
    fn finalizes_only_older_than_overlap() {
        let mut m = CueMerger::new(MergeConfig::default()).unwrap();
        let out = m.push(&info(0, 0.0), &tr(&[(0.0, 3.0, "あ"), (3.5, 4.5, "い")])).unwrap();
        // 4.5 秒に終わる区切りは、次のチャンク（4 秒から）と重なるのでまだ確定しない
        assert_eq!(texts(&out), vec!["あ"]);
        assert_eq!(m.finalized_until(), Some(3_000_000));
        let out = m.push(&info(1, 4.0), &tr(&[])).unwrap();
        assert_eq!(texts(&out), vec!["い"]);
    }

    #[test]
    fn gap_finalizes_everything() {
        let mut m = CueMerger::new(MergeConfig::default()).unwrap();
        assert!(m.push(&info(0, 0.0), &tr(&[(4.2, 5.0, "途中")])).unwrap().is_empty());
        let mut i = info(5, 20.0);
        i.dropped_before = 4;
        let out = m.push(&i, &tr(&[(0.5, 1.0, "後")])).unwrap();
        assert_eq!(texts(&out), vec!["途中", "後"]);
        // 順序の違反
        assert!(m.push(&info(5, 24.0), &tr(&[])).is_err());
    }

    #[test]
    fn long_segment_split_by_lines_and_duration() {
        let mut m = CueMerger::new(MergeConfig::default()).unwrap();
        let long = "これはとても長い発話の例で、講義の中で先生が一息に話し続けた内容をそのまま書き起こしたものです。字幕は二行までに収め、七秒を超えないように分けなければなりません。";
        let out = m.push(&info(0, 0.0), &tr(&[(0.0, 2.0, "短い")])).unwrap();
        assert_eq!(out.len(), 1);
        let mut all = out;
        all.extend(m.push(&info(1, 4.0), &tr(&[(0.0, 0.0, "")])).unwrap());
        // 20 秒の発話を不連続の後に入れる
        let mut i = info(2, 10.0);
        i.duration_us = 30_000_000;
        i.discontinuity = true;
        let out = m.push(&i, &tr(&[(0.0, 20.0, long)])).unwrap();
        all.extend(out);
        all.extend(m.flush());
        check_monotonic(&all);
        let pieces: Vec<&Cue> = all.iter().filter(|c| c.start_us >= 10_000_000).collect();
        assert!(pieces.len() >= 3, "{pieces:?}");
        for c in &pieces {
            assert!(c.end_us - c.start_us <= 7_000_000);
            assert!(c.text.lines().count() <= 2);
            assert!(c.text.lines().all(|l| text_width(l) <= 64), "{}", c.text);
        }
        assert_eq!(pieces.iter().map(|c| c.text.replace('\n', "")).collect::<String>(), long);
        assert_eq!(pieces.last().unwrap().end_us, 30_000_000);
    }

    #[test]
    fn min_duration_and_no_overlap() {
        let mut m = CueMerger::new(MergeConfig::default()).unwrap();
        let mut all = m.push(&info(0, 0.0), &tr(&[(0.0, 0.1, "はい"), (0.2, 1.5, "では始めます"), (1.0, 1.4, "えー")])).unwrap();
        all.extend(m.flush());
        check_monotonic(&all);
        assert_eq!(all[0].end_us - all[0].start_us, 700_000);
    }

    #[test]
    fn time_jitter_duplicate() {
        let mut m = CueMerger::new(MergeConfig::default()).unwrap();
        let mut all = m.push(&info(0, 0.0), &tr(&[(3.9, 4.6, "ありがとうございます")])).unwrap();
        // 次のチャンクでは時刻が少しずれて、重ならない位置に出た
        all.extend(m.push(&info(1, 4.0), &tr(&[(0.7, 1.2, "ありがとうございます。")])).unwrap());
        all.extend(m.flush());
        assert_eq!(all.len(), 1);
    }
}
