//! ライブの HLS 字幕（RFC 8216 3.5 の WebVTT セグメント）。
//!
//! 映像のセグメントが閉じるたびに、同じ media sequence 番号・同じ媒体時刻の範囲で
//! [`SubtitleSegmenter::close_segment`] を呼ぶと、その範囲に重なる Cue を入れた `.vtt` を返す。
//! 字幕のセグメントの境界は映像と同じなので、プレイヤーは番号で対応させられる。
//!
//! - 範囲をまたぐ Cue は両方のセグメントに同じ時刻で入れる（プレイヤーは重複を除く）。
//! - セグメントを閉じた後に届いた Cue（認識が遅れた）は、まだ閉じていない次のセグメントに入れる。
//!   時刻は話した時刻のまま。プレイヤーはライブの端より HOLD-BACK だけ後ろを再生しているので、
//!   遅れが HOLD-BACK より短ければ表示に間に合う（[`latency`](crate::latency)）。
//!   次のセグメントにも重ならないほど古い Cue は `late_cues` に数えてライブでは出さない（録画の
//!   [`VttDocument`](crate::vtt::VttDocument) には入る）。
//! - 各セグメントに `X-TIMESTAMP-MAP` を付ける。

use crate::error::{Error, Result};
use crate::merge::Cue;
use crate::vtt::{TimestampMap, VttCue, write_vtt};
use senca_hls::{MediaPlaylist, MediaSegment, MediaType, MultivariantPlaylist, PlaylistType, Rendition};
use std::collections::VecDeque;

/// 字幕トラックの設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleTrackConfig {
    /// `EXT-X-MEDIA` の NAME（例: `日本語`、`English`）。
    pub name: String,
    /// BCP 47（例: `ja`、`en`）。
    pub language: String,
    /// `GROUP-ID`（同じストリームの全言語で同じ値。例: `subs`）。
    pub group_id: String,
    /// セグメントの URI の前置き（例: `sub_ja_` → `sub_ja_42.vtt`）。
    pub segment_prefix: String,
    /// 字幕の media playlist の URI（multivariant から見た相対 URI。例: `sub_ja.m3u8`）。
    pub playlist_uri: String,
    /// ライブの playlist に載せるセグメントの数（映像と同じにする）。
    pub window_segments: usize,
    /// 映像の playlist と同じ EXT-X-TARGETDURATION（秒）。
    pub target_duration_s: u64,
    /// 媒体時刻 0 に当たる 90 kHz の値（`senca-mpegts` の多重化なら 0）。
    pub mpegts_offset: u64,
    pub default: bool,
    pub autoselect: bool,
    /// 溜める Cue の上限。
    pub max_cues: usize,
}

impl SubtitleTrackConfig {
    pub fn new(language: &str, name: &str) -> Self {
        SubtitleTrackConfig {
            name: name.into(),
            language: language.into(),
            group_id: "subs".into(),
            segment_prefix: format!("sub_{language}_"),
            playlist_uri: format!("sub_{language}.m3u8"),
            window_segments: 10,
            target_duration_s: 6,
            mpegts_offset: 0,
            default: false,
            autoselect: true,
            max_cues: 10_000,
        }
    }

    fn validate(&self) -> Result<()> {
        let ok = |s: &str| !s.is_empty() && s.len() <= 128 && !s.contains(['"', '\n', '\r', ',']);
        if !ok(&self.name) || !ok(&self.group_id) || !ok(&self.playlist_uri) || !ok(&self.language) {
            return Err(Error::InvalidConfig("subtitle track names"));
        }
        if self.segment_prefix.contains(['"', '\n', '\r', '/', '?', '#']) {
            return Err(Error::InvalidConfig("segment prefix"));
        }
        if self.window_segments == 0 || self.target_duration_s == 0 || self.max_cues == 0 {
            return Err(Error::InvalidConfig("subtitle playlist limits"));
        }
        Ok(())
    }
}

/// 出来上がった `.vtt` のセグメント。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VttSegment {
    pub msn: u64,
    pub uri: String,
    pub start_us: i64,
    pub duration_us: u64,
    pub discontinuity: bool,
    pub body: String,
}

#[derive(Debug, Clone)]
struct SegRec {
    msn: u64,
    uri: String,
    duration_us: u64,
    discontinuity: bool,
}

/// 字幕の HLS セグメントと media playlist を作る。
#[derive(Debug)]
pub struct SubtitleSegmenter {
    cfg: SubtitleTrackConfig,
    /// まだセグメントに入れ終わっていない Cue（開始時刻の順）。
    cues: Vec<Cue>,
    segments: VecDeque<SegRec>,
    /// 閉じた最後のセグメントの終わり。
    closed_until: Option<i64>,
    last_msn: Option<u64>,
    discontinuity_sequence: u64,
    ended: bool,
    /// 遅れすぎてライブに出せなかった Cue の数。
    pub late_cues: u64,
    /// 上限で捨てた Cue の数。
    pub dropped_cues: u64,
}

impl SubtitleSegmenter {
    pub fn new(cfg: SubtitleTrackConfig) -> Result<Self> {
        cfg.validate()?;
        Ok(SubtitleSegmenter {
            cfg,
            cues: Vec::new(),
            segments: VecDeque::new(),
            closed_until: None,
            last_msn: None,
            discontinuity_sequence: 0,
            ended: false,
            late_cues: 0,
            dropped_cues: 0,
        })
    }

    pub fn config(&self) -> &SubtitleTrackConfig {
        &self.cfg
    }

    /// 確定した Cue を足す（遅れて来てもよい）。
    pub fn push_cue(&mut self, cue: Cue) {
        if let Some(c) = self.closed_until
            && cue.end_us <= c
        {
            // もう閉じたセグメントにしか重ならない
            self.late_cues += 1;
            return;
        }
        if self.cues.len() >= self.cfg.max_cues {
            self.cues.remove(0);
            self.dropped_cues += 1;
        }
        let pos = self.cues.partition_point(|c| (c.start_us, c.id) <= (cue.start_us, cue.id));
        self.cues.insert(pos, cue);
    }

    /// 映像のセグメント `msn`（媒体時刻 `start_us` から `duration_us`）が閉じた。字幕のセグメントを返す。
    pub fn close_segment(&mut self, msn: u64, start_us: i64, duration_us: u64, discontinuity: bool) -> Result<VttSegment> {
        if self.ended {
            return Err(Error::InvalidInput("playlist already ended"));
        }
        if self.last_msn.is_some_and(|m| msn <= m) {
            return Err(Error::InvalidInput("media sequence must increase"));
        }
        if duration_us == 0 || start_us < 0 {
            return Err(Error::InvalidInput("segment range"));
        }
        let end = start_us.saturating_add(duration_us as i64);
        let inside: Vec<VttCue> = self.cues.iter().filter(|c| c.end_us > start_us && c.start_us < end).map(VttCue::from).collect();
        // このセグメントで終わる Cue はもう要らない（次のセグメントにまたがるものは残す）
        self.cues.retain(|c| c.end_us > end);
        let map = TimestampMap::for_media_time(start_us, self.cfg.mpegts_offset);
        let body = write_vtt(Some(map), &inside)?;
        let uri = format!("{}{}.vtt", self.cfg.segment_prefix, msn);
        self.segments.push_back(SegRec { msn, uri: uri.clone(), duration_us, discontinuity });
        while self.segments.len() > self.cfg.window_segments {
            // 窓から外れたセグメントが DISCONTINUITY を持っていれば番号を進める（RFC 8216 6.2.1）
            if self.segments.pop_front().is_some_and(|old| old.discontinuity) {
                self.discontinuity_sequence += 1;
            }
        }
        self.closed_until = Some(end);
        self.last_msn = Some(msn);
        Ok(VttSegment { msn, uri, start_us, duration_us, discontinuity, body })
    }

    /// 終わり（ENDLIST を付ける）。
    pub fn end(&mut self) {
        self.ended = true;
    }

    /// 字幕の media playlist（ライブは窓の分だけ）。
    pub fn playlist(&self) -> Result<String> {
        let mut p = MediaPlaylist::new(self.cfg.target_duration_s);
        p.media_sequence = self.segments.front().map(|s| s.msn).unwrap_or(0);
        p.discontinuity_sequence = self.discontinuity_sequence;
        for s in &self.segments {
            let mut seg = MediaSegment::new(s.uri.clone(), s.duration_us);
            seg.discontinuity = s.discontinuity;
            p.segments.push(seg);
        }
        if self.ended {
            p.end_list = true;
        }
        p.validate()?;
        Ok(p.to_m3u8()?)
    }

    /// multivariant playlist に足す `EXT-X-MEDIA`。
    pub fn rendition(&self) -> Rendition {
        let mut r = Rendition::new(MediaType::Subtitles, self.cfg.group_id.clone(), self.cfg.name.clone());
        r.language = Some(self.cfg.language.clone());
        r.default = self.cfg.default;
        r.autoselect = self.cfg.autoselect;
        r.uri = Some(self.cfg.playlist_uri.clone());
        r
    }

    /// `#EXT-X-MEDIA:TYPE=SUBTITLES,…` の 1 行（multivariant を自分で書く場合用）。
    pub fn ext_x_media_line(&self) -> Result<String> {
        media_lines(&[self.rendition()]).map(|mut v| v.remove(0))
    }
}

/// `EXT-X-MEDIA` の行を書き出す（senca-hls の書き手を使う）。
pub fn media_lines(renditions: &[Rendition]) -> Result<Vec<String>> {
    let mv = MultivariantPlaylist { renditions: renditions.to_vec(), ..MultivariantPlaylist::default() };
    let text = mv.to_m3u8()?;
    let lines: Vec<String> = text.lines().filter(|l| l.starts_with("#EXT-X-MEDIA:")).map(str::to_string).collect();
    if lines.len() != renditions.len() {
        return Err(Error::Container("EXT-X-MEDIA lines".into()));
    }
    Ok(lines)
}

/// 録画（VOD）の字幕 playlist: 1 つの `.vtt`（[`VttDocument`](crate::vtt::VttDocument)）を全体の長さで載せる。
pub fn vod_single_file_playlist(vtt_uri: &str, duration_us: u64) -> Result<String> {
    let target = duration_us.div_ceil(1_000_000).max(1);
    let mut p = MediaPlaylist::new(target);
    p.playlist_type = Some(PlaylistType::Vod);
    p.segments.push(MediaSegment::new(vtt_uri, duration_us.max(1)));
    p.end_list = true;
    p.validate()?;
    Ok(p.to_m3u8()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vtt::validate;

    fn cue(id: u64, s: f64, e: f64, t: &str) -> Cue {
        Cue { id, start_us: (s * 1e6) as i64, end_us: (e * 1e6) as i64, text: t.into() }
    }

    #[test]
    fn segments_follow_video_boundaries() {
        let mut cfg = SubtitleTrackConfig::new("ja", "日本語");
        cfg.window_segments = 3;
        cfg.target_duration_s = 4;
        cfg.default = true;
        let mut s = SubtitleSegmenter::new(cfg).unwrap();
        s.push_cue(cue(1, 0.5, 2.0, "一"));
        s.push_cue(cue(2, 3.5, 5.0, "二")); // 境界をまたぐ
        let a = s.close_segment(100, 0, 4_000_000, false).unwrap();
        assert_eq!(a.uri, "sub_ja_100.vtt");
        let va = validate(&a.body).unwrap();
        assert_eq!(va.cues.len(), 2);
        assert_eq!(va.timestamp_map, Some(TimestampMap { mpegts: 0, local_us: 0 }));
        // 遅れて来た Cue: 閉じたセグメントにしか重ならないものは捨て、次に重なるものは次へ
        s.push_cue(cue(3, 1.0, 3.0, "遅すぎ"));
        s.push_cue(cue(4, 3.8, 4.5, "少し遅い"));
        assert_eq!(s.late_cues, 1);
        let b = s.close_segment(101, 4_000_000, 4_000_000, false).unwrap();
        let vb = validate(&b.body).unwrap();
        assert_eq!(vb.cues.iter().map(|c| c.3.as_str()).collect::<Vec<_>>(), vec!["二", "少し遅い"]);
        assert_eq!(vb.timestamp_map.unwrap().mpegts, 360_000);
        // 空のセグメントも作る（ヘッダーだけ）
        let c = s.close_segment(102, 8_000_000, 4_000_000, false).unwrap();
        assert!(validate(&c.body).unwrap().cues.is_empty());
        s.close_segment(103, 12_000_000, 3_500_000, true).unwrap();
        let pl = s.playlist().unwrap();
        assert!(pl.contains("#EXT-X-MEDIA-SEQUENCE:101\n"), "{pl}");
        assert!(pl.contains("#EXT-X-TARGETDURATION:4\n"));
        assert!(pl.contains("#EXT-X-DISCONTINUITY\n"));
        assert!(!pl.contains("sub_ja_100.vtt"));
        assert!(!pl.contains("ENDLIST"));
        let parsed = MediaPlaylist::parse(pl.as_bytes()).unwrap();
        assert_eq!(parsed.segments.len(), 3);
        assert!(s.close_segment(103, 0, 1, false).is_err());
        s.end();
        assert!(s.playlist().unwrap().contains("#EXT-X-ENDLIST"));
        assert!(s.close_segment(104, 16_000_000, 1, false).is_err());
    }

    #[test]
    fn media_line_and_rendition() {
        let mut cfg = SubtitleTrackConfig::new("en", "English");
        cfg.default = true;
        let s = SubtitleSegmenter::new(cfg).unwrap();
        let line = s.ext_x_media_line().unwrap();
        assert_eq!(
            line,
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,URI=\"sub_en.m3u8\""
        );
        let mut bad = SubtitleTrackConfig::new("en", "Eng\"lish");
        assert!(SubtitleSegmenter::new(bad.clone()).is_err());
        bad.name = "x".into();
        bad.window_segments = 0;
        assert!(SubtitleSegmenter::new(bad).is_err());
    }

    #[test]
    fn vod_playlist() {
        let p = vod_single_file_playlist("captions_ja.vtt", 3_600_500_000).unwrap();
        assert!(p.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(p.contains("#EXT-X-TARGETDURATION:3601"));
        assert!(p.contains("captions_ja.vtt\n#EXT-X-ENDLIST"));
    }

    #[test]
    fn cue_limit() {
        let mut cfg = SubtitleTrackConfig::new("ja", "日本語");
        cfg.max_cues = 2;
        let mut s = SubtitleSegmenter::new(cfg).unwrap();
        for i in 0..5 {
            s.push_cue(cue(i, i as f64, i as f64 + 0.5, "x"));
        }
        assert_eq!(s.dropped_cues, 3);
    }
}
