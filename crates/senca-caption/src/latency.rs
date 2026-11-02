//! 字幕の遅延の見積もり（REQ-006 R6-4「遅延の予算を明示する」）。
//!
//! 2 つの方針から選ぶ。
//!
//! - [`LatencyMode::CaptionsMayLag`] — 映像はそのまま出し、字幕は確定したものから出す。字幕は話した時刻
//!   （媒体時刻）で付くので、プレイヤーの再生位置がライブの端より `HOLD-BACK` だけ後ろなら、
//!   字幕の遅れ（[`LatencyReport::caption_latency_us`]）が HOLD-BACK より短い限り間に合う。超えた分は表示されない。
//! - [`LatencyMode::DelayVideo`] — 映像（と全 rendition）の playlist の公開を
//!   [`LatencyReport::required_video_delay_us`] だけ遅らせ、字幕がそろってから見せる。遅延は増えるが字幕は欠けない。
//!
//! ある発話（終わりの時刻 t）の字幕が確定するまでの最悪の時間:
//!
//! 1. 音声が溜まる: t を含むチャンクの終わりまで最大で窓の長さ。さらに確定には「チャンクの終わり − 重なり ≥ t」
//!    が要るので、重なりの中に入った発話は次のチャンクを待つ。合わせて **窓の長さ**（= 重なり + ずらす量）。
//! 2. 認識の待ち: 認識が前のチャンクを処理中なら、その残りを待つ（最大で認識の時間）。
//! 3. 認識の時間（p95 など、見積もりを入れる）。
//! 4. 翻訳する言語は、まとめ送りの待ち＋翻訳の時間を足す。
//! 5. 字幕のセグメントは映像のセグメントの境界で出るので、最大でセグメント 1 つ分遅れる。
//!
//! 認識の時間が「ずらす量」（窓 − 重なり）より長いと実時間に追いつかず、チャンクを捨て続ける
//! （[`LatencyReport::realtime_ok`] が偽）。

/// 遅延の方針。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencyMode {
    CaptionsMayLag,
    DelayVideo,
}

/// 見積もりの入力（µs）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencyInputs {
    pub chunk_window_us: u64,
    pub chunk_overlap_us: u64,
    /// 1 チャンクの認識にかかる時間（p95 などの見積もり。送受信を含む）。
    pub asr_expected_us: u64,
    /// 翻訳にかかる時間（翻訳しないなら `None`）。
    pub translation_expected_us: Option<u64>,
    /// 翻訳のまとめ送りの最大の待ち（[`BatcherConfig::max_wait_us`](crate::translate::BatcherConfig::max_wait_us)）。
    pub translation_batch_wait_us: u64,
    /// HLS のセグメントの長さ。
    pub segment_duration_us: u64,
    /// 余裕。
    pub safety_margin_us: u64,
}

impl Default for LatencyInputs {
    fn default() -> Self {
        LatencyInputs {
            chunk_window_us: 5_000_000,
            chunk_overlap_us: 1_000_000,
            asr_expected_us: 1_500_000,
            translation_expected_us: Some(2_000_000),
            translation_batch_wait_us: 1_500_000,
            segment_duration_us: 2_000_000,
            safety_margin_us: 500_000,
        }
    }
}

/// 見積もりの結果（µs）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencyReport {
    pub mode: LatencyMode,
    /// 話してから原語の字幕が確定するまで（最悪）。
    pub caption_latency_us: u64,
    /// 話してから訳が届くまで（最悪。翻訳しないなら原語と同じ）。
    pub translation_latency_us: u64,
    /// 字幕のセグメントに載るまで（最も遅い言語 + セグメント 1 つ + 余裕）。
    pub publish_latency_us: u64,
    /// `DelayVideo` で映像の公開を遅らせる量（セグメントの長さの倍数に切り上げ）。`CaptionsMayLag` では 0。
    pub required_video_delay_us: u64,
    /// 認識が実時間に追いつく（認識の時間 < 窓 − 重なり）。
    pub realtime_ok: bool,
}

impl LatencyReport {
    /// `CaptionsMayLag` で、プレイヤーの HOLD-BACK（µs）の内に字幕が間に合うか。
    pub fn fits_hold_back(&self, hold_back_us: u64) -> bool {
        self.publish_latency_us <= hold_back_us
    }
}

/// 見積もる（純粋関数）。
pub fn plan(mode: LatencyMode, i: &LatencyInputs) -> LatencyReport {
    let step = i.chunk_window_us.saturating_sub(i.chunk_overlap_us);
    let realtime_ok = i.asr_expected_us < step;
    // 窓 + （前のチャンクの認識の残り）+ 認識
    let queue_wait = i.asr_expected_us.min(step);
    let caption = i.chunk_window_us.saturating_add(queue_wait).saturating_add(i.asr_expected_us);
    let translation = match i.translation_expected_us {
        Some(t) => caption.saturating_add(i.translation_batch_wait_us).saturating_add(t),
        None => caption,
    };
    let publish = translation.max(caption).saturating_add(i.segment_duration_us).saturating_add(i.safety_margin_us);
    let required = match mode {
        LatencyMode::CaptionsMayLag => 0,
        LatencyMode::DelayVideo if i.segment_duration_us > 0 => publish.div_ceil(i.segment_duration_us) * i.segment_duration_us,
        LatencyMode::DelayVideo => publish,
    };
    LatencyReport {
        mode,
        caption_latency_us: caption,
        translation_latency_us: translation,
        publish_latency_us: publish,
        required_video_delay_us: required,
        realtime_ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_budget() {
        let i = LatencyInputs::default();
        let lag = plan(LatencyMode::CaptionsMayLag, &i);
        // 5 + 1.5 + 1.5 = 8 秒、訳は + 1.5 + 2 = 11.5 秒、公開は + 2 + 0.5 = 14 秒
        assert_eq!(lag.caption_latency_us, 8_000_000);
        assert_eq!(lag.translation_latency_us, 11_500_000);
        assert_eq!(lag.publish_latency_us, 14_000_000);
        assert_eq!(lag.required_video_delay_us, 0);
        assert!(lag.realtime_ok);
        assert!(!lag.fits_hold_back(6_000_000));
        let delay = plan(LatencyMode::DelayVideo, &i);
        assert_eq!(delay.required_video_delay_us, 14_000_000);
        let no_tr = plan(LatencyMode::DelayVideo, &LatencyInputs { translation_expected_us: None, ..i });
        assert_eq!(no_tr.publish_latency_us, 10_500_000);
        assert_eq!(no_tr.required_video_delay_us, 12_000_000);
    }

    #[test]
    fn slow_asr_is_flagged() {
        let i = LatencyInputs { asr_expected_us: 4_500_000, ..LatencyInputs::default() };
        let r = plan(LatencyMode::CaptionsMayLag, &i);
        assert!(!r.realtime_ok);
        assert_eq!(r.caption_latency_us, 5_000_000 + 4_000_000 + 4_500_000);
    }
}
