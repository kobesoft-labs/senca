//! WebVTT（W3C WebVTT）の書き出しと検査。
//!
//! - 時刻は `HH:MM:SS.mmm`（時は 2 桁以上）。負の時刻は書けない。
//! - Cue の本文の `&`・`<`・`>` は文字参照にする（`-->` も現れなくなる）。空行は Cue を終わらせるので除く。
//! - HLS では各セグメントの先頭に `X-TIMESTAMP-MAP=MPEGTS:<90 kHz>,LOCAL:<時刻>` を書き、
//!   WebVTT の時刻（LOCAL）と TS の PTS・fMP4 の 90 kHz に換算した時刻（MPEGTS）を対応させる
//!   （RFC 8216 3.5）。この crate では WebVTT の時刻 = 媒体時刻とし、`MPEGTS = 媒体時刻 × 90 kHz + offset`
//!   （33 bit で折り返す）とする。`senca-mpegts` の多重化は PTS = 媒体時刻なので offset は 0。

use crate::error::{Error, Result};
use crate::merge::Cue;
use std::fmt::Write as _;

/// 1 件の Cue。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VttCue {
    pub id: Option<String>,
    pub start_us: i64,
    pub end_us: i64,
    /// cue settings（例: `line:90% align:center`）。
    pub settings: Option<String>,
    /// 本文（行は `\n`）。エスケープ前の素の文字列。
    pub text: String,
}

impl From<&Cue> for VttCue {
    fn from(c: &Cue) -> Self {
        VttCue { id: Some(c.id.to_string()), start_us: c.start_us, end_us: c.end_us, settings: None, text: c.text.clone() }
    }
}

/// `X-TIMESTAMP-MAP`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimestampMap {
    /// 90 kHz（33 bit）。
    pub mpegts: u64,
    /// WebVTT の時刻（µs、ミリ秒に丸めたもの）。
    pub local_us: i64,
}

impl TimestampMap {
    /// 媒体時刻 `local_us` と、媒体時刻 0 に当たる 90 kHz の値 `mpegts_offset` から作る。
    /// LOCAL はミリ秒に丸め、MPEGTS はその丸めた時刻から計算する（両者を正確に対応させるため）。
    pub fn for_media_time(local_us: i64, mpegts_offset: u64) -> TimestampMap {
        let ms = local_us.max(0) / 1000;
        let ticks = (ms as u128 * 90 + mpegts_offset as u128) % (1u128 << 33);
        TimestampMap { mpegts: ticks as u64, local_us: ms * 1000 }
    }
}

/// `HH:MM:SS.mmm`。
pub fn format_timestamp(us: i64) -> Result<String> {
    if us < 0 {
        return Err(Error::InvalidInput("negative WebVTT timestamp"));
    }
    let ms = us / 1000;
    Ok(format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000))
}

/// `[HH:]MM:SS.mmm` を µs に。
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let (hms, frac) = s.split_once('.')?;
    if frac.len() != 3 || !frac.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let parts: Vec<&str> = hms.split(':').collect();
    let num = |x: &str, min_len: usize| -> Option<i64> {
        (x.len() >= min_len && x.len() <= 10 && x.bytes().all(|c| c.is_ascii_digit())).then(|| x.parse().ok()).flatten()
    };
    let (h, m, sec) = match parts.as_slice() {
        [m, s] if m.len() == 2 && s.len() == 2 => (0, num(m, 2)?, num(s, 2)?),
        [h, m, s] if m.len() == 2 && s.len() == 2 => (num(h, 2)?, num(m, 2)?, num(s, 2)?),
        _ => return None,
    };
    if m > 59 || sec > 59 {
        return None;
    }
    let ms: i64 = frac.parse().ok()?;
    Some(((h * 3600 + m * 60 + sec) * 1000 + ms) * 1000)
}

/// 本文のエスケープ。行ごとに整え、空行を除く。
pub fn escape_text(s: &str) -> String {
    let mut lines = Vec::new();
    for line in s.split('\n') {
        let mut o = String::with_capacity(line.len());
        for c in line.chars() {
            match c {
                '&' => o.push_str("&amp;"),
                '<' => o.push_str("&lt;"),
                '>' => o.push_str("&gt;"),
                '\r' => {}
                c if c.is_control() => {}
                c => o.push(c),
            }
        }
        let t = o.trim();
        if !t.is_empty() {
            lines.push(t.to_string());
        }
    }
    lines.join("\n")
}

fn check_line(s: &str, what: &'static str) -> Result<()> {
    if s.contains(['\n', '\r']) || s.contains("-->") || s.chars().any(char::is_control) {
        return Err(Error::InvalidInput(what));
    }
    Ok(())
}

/// WebVTT のファイルを書く。Cue は開始時刻の順（同じなら終わりの順）に並べ替える。本文が空の Cue は書かない。
pub fn write_vtt(map: Option<TimestampMap>, cues: &[VttCue]) -> Result<String> {
    let mut o = String::with_capacity(32 + cues.len() * 64);
    o.push_str("WEBVTT\n");
    if let Some(m) = map {
        let _ = writeln!(o, "X-TIMESTAMP-MAP=MPEGTS:{},LOCAL:{}", m.mpegts, format_timestamp(m.local_us)?);
    }
    let mut sorted: Vec<&VttCue> = cues.iter().collect();
    sorted.sort_by_key(|c| (c.start_us, c.end_us));
    for c in sorted {
        if c.end_us < c.start_us {
            return Err(Error::InvalidInput("cue ends before it starts"));
        }
        let text = escape_text(&c.text);
        if text.is_empty() {
            continue;
        }
        o.push('\n');
        if let Some(id) = &c.id {
            check_line(id, "cue id")?;
            if !id.is_empty() {
                o.push_str(id);
                o.push('\n');
            }
        }
        o.push_str(&format_timestamp(c.start_us)?);
        o.push_str(" --> ");
        o.push_str(&format_timestamp(c.end_us)?);
        if let Some(s) = &c.settings {
            check_line(s, "cue settings")?;
            if !s.is_empty() {
                o.push(' ');
                o.push_str(s);
            }
        }
        o.push('\n');
        o.push_str(&text);
        o.push('\n');
    }
    Ok(o)
}

/// 検査の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VttSummary {
    pub cues: Vec<(Option<String>, i64, i64, String)>,
    pub timestamp_map: Option<TimestampMap>,
}

/// WebVTT を厳しめに読む（書いたものの検査と試験用）。header・Cue の時刻の形・順序・本文の禁止文字を確かめる。
pub fn validate(input: &str) -> Result<VttSummary> {
    if input.len() > 16 << 20 {
        return Err(Error::TooLarge("vtt"));
    }
    let s = input.strip_prefix('\u{FEFF}').unwrap_or(input);
    if s.contains('\0') {
        return Err(Error::Malformed("NUL in vtt".into()));
    }
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut blocks = s.split("\n\n").filter(|b| !b.trim().is_empty());
    let header = blocks.next().ok_or(Error::Malformed("empty".into()))?;
    let mut hl = header.lines();
    let first = hl.next().unwrap_or("");
    if !(first == "WEBVTT" || first.starts_with("WEBVTT ") || first.starts_with("WEBVTT\t")) {
        return Err(Error::Malformed("missing WEBVTT signature".into()));
    }
    let mut summary = VttSummary { cues: Vec::new(), timestamp_map: None };
    for l in hl {
        if l.contains("-->") {
            return Err(Error::Malformed("cue inside the header block".into()));
        }
        if let Some(v) = l.strip_prefix("X-TIMESTAMP-MAP=") {
            let mut mpegts = None;
            let mut local = None;
            for kv in v.split(',') {
                if let Some(x) = kv.strip_prefix("MPEGTS:") {
                    mpegts = x.parse::<u64>().ok();
                } else if let Some(x) = kv.strip_prefix("LOCAL:") {
                    local = parse_timestamp(x);
                }
            }
            match (mpegts, local) {
                (Some(m), Some(l)) if m < 1 << 33 => summary.timestamp_map = Some(TimestampMap { mpegts: m, local_us: l }),
                _ => return Err(Error::Malformed("bad X-TIMESTAMP-MAP".into())),
            }
        }
    }
    let mut last_start = i64::MIN;
    for b in blocks {
        let b = b.trim_start_matches('\n');
        if b.starts_with("NOTE") || b.starts_with("STYLE") || b.starts_with("REGION") {
            continue;
        }
        let mut lines = b.lines();
        let mut l = lines.next().unwrap_or("");
        let mut id = None;
        if !l.contains("-->") {
            id = Some(l.to_string());
            l = lines.next().ok_or(Error::Malformed("cue id without timing".into()))?;
        }
        let (a, rest) = l.split_once(" --> ").ok_or(Error::Malformed("cue timing line".into()))?;
        let bpart = rest.split([' ', '\t']).next().unwrap_or("");
        let start = parse_timestamp(a.trim()).ok_or(Error::Malformed("cue start".into()))?;
        let end = parse_timestamp(bpart).ok_or(Error::Malformed("cue end".into()))?;
        if end < start {
            return Err(Error::Malformed("cue ends before it starts".into()));
        }
        if start < last_start {
            return Err(Error::Malformed("cues are not ordered by start time".into()));
        }
        last_start = start;
        let text: Vec<&str> = lines.collect();
        if text.iter().any(|t| t.contains("-->")) {
            return Err(Error::Malformed("--> in cue text".into()));
        }
        if summary.cues.len() >= 1_000_000 {
            return Err(Error::TooLarge("vtt cues"));
        }
        summary.cues.push((id, start, end, text.join("\n")));
    }
    Ok(summary)
}

/// 録画用の WebVTT 全体（VOD）。Cue を ID で重複なく溜める（上限付き）。
#[derive(Debug, Clone)]
pub struct VttDocument {
    cues: Vec<VttCue>,
    max_cues: usize,
    /// 上限で捨てた数。
    pub dropped: u64,
    /// 時刻の原点（録画の先頭の媒体時刻）。書き出す時刻はここからの差。
    pub origin_us: i64,
}

impl VttDocument {
    pub fn new(origin_us: i64) -> Self {
        VttDocument { cues: Vec::new(), max_cues: 500_000, dropped: 0, origin_us }
    }

    pub fn with_limit(origin_us: i64, max_cues: usize) -> Self {
        VttDocument { max_cues, ..VttDocument::new(origin_us) }
    }

    pub fn push(&mut self, c: &Cue) {
        if self.cues.len() >= self.max_cues {
            self.dropped += 1;
            return;
        }
        let id = c.id.to_string();
        if self.cues.iter().rev().take(64).any(|x| x.id.as_deref() == Some(&id)) {
            return;
        }
        let start = (c.start_us - self.origin_us).max(0);
        let end = (c.end_us - self.origin_us).max(start);
        self.cues.push(VttCue { id: Some(id), start_us: start, end_us: end, settings: None, text: c.text.clone() });
    }

    pub fn len(&self) -> usize {
        self.cues.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    /// ファイル全体。録画の時刻 0 = `origin_us`。
    pub fn to_vtt(&self) -> Result<String> {
        write_vtt(None, &self.cues)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(format_timestamp(0).unwrap(), "00:00:00.000");
        assert_eq!(format_timestamp(3_723_456_789).unwrap(), "01:02:03.456");
        assert_eq!(format_timestamp(100 * 3_600_000_000).unwrap(), "100:00:00.000");
        assert!(format_timestamp(-1).is_err());
        assert_eq!(parse_timestamp("01:02:03.456"), Some(3_723_456_000));
        assert_eq!(parse_timestamp("02:03.456"), Some(123_456_000));
        for bad in ["1:02:03.456", "00:60:00.000", "00:00:00.45", "00:00:00", "aa:bb:cc.ddd", "00:00:00.-01"] {
            assert_eq!(parse_timestamp(bad), None, "{bad}");
        }
    }

    #[test]
    fn escaping() {
        assert_eq!(escape_text("a<b>&c --> d"), "a&lt;b&gt;&amp;c --&gt; d");
        assert_eq!(escape_text("一行目\n\n  \n二行目\r"), "一行目\n二行目");
    }

    #[test]
    fn write_and_validate() {
        let cues = vec![
            VttCue { id: Some("2".into()), start_us: 5_000_000, end_us: 6_000_000, settings: Some("line:90%".into()), text: "後 <b>".into() },
            VttCue { id: Some("1".into()), start_us: 1_000_000, end_us: 4_500_000, settings: None, text: "前の行\n次の行".into() },
            VttCue { id: None, start_us: 7_000_000, end_us: 8_000_000, settings: None, text: "\n".into() },
        ];
        let map = TimestampMap::for_media_time(10_000_400, 0);
        let s = write_vtt(Some(map), &cues).unwrap();
        assert!(s.starts_with("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:10.000\n\n1\n00:00:01.000 --> 00:00:04.500\n前の行\n次の行\n"));
        let v = validate(&s).unwrap();
        assert_eq!(v.cues.len(), 2); // 空の Cue は書かない
        assert_eq!(v.cues[1], (Some("2".into()), 5_000_000, 6_000_000, "後 &lt;b&gt;".into()));
        assert_eq!(v.timestamp_map, Some(TimestampMap { mpegts: 900_000, local_us: 10_000_000 }));
        let bad_id = VttCue { id: Some("a-->b".into()), start_us: 0, end_us: 1, settings: None, text: "x".into() };
        assert!(write_vtt(None, &[bad_id]).is_err());
        let backwards = VttCue { id: None, start_us: 2, end_us: 1, settings: None, text: "x".into() };
        assert!(write_vtt(None, &[backwards]).is_err());
    }

    #[test]
    fn timestamp_map_wraps() {
        // 33 bit（約 26.5 時間）で折り返す
        let m = TimestampMap::for_media_time(27 * 3600 * 1_000_000, 0);
        assert_eq!(m.mpegts, (27u64 * 3600 * 90_000) % (1 << 33));
        let m = TimestampMap::for_media_time(0, 126_000);
        assert_eq!(m.mpegts, 126_000);
    }

    #[test]
    fn validate_rejects() {
        for bad in [
            "",
            "WEBVT\n\n00:00.000 --> 00:01.000\nx\n",
            "WEBVTT\n\n00:02.000 --> 00:01.000\nx\n",
            "WEBVTT\n\n00:02.000 --> 00:03.000\nx\n\n00:01.000 --> 00:04.000\ny\n",
            "WEBVTT\n\nid\n00:02.000 -> 00:03.000\nx\n",
            "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:abc,LOCAL:00:00:00.000\n\n00:01.000 --> 00:02.000\nx\n",
            "WEBVTT\n\nonly-id\n",
        ] {
            assert!(validate(bad).is_err(), "{bad:?}");
        }
        assert!(validate("\u{FEFF}WEBVTT - title\r\n\r\nNOTE hi\r\n\r\n00:01.000 --> 00:02.000 align:start\r\nx\r\n").is_ok());
    }

    #[test]
    fn document_dedups_and_limits() {
        let mut d = VttDocument::with_limit(10_000_000, 2);
        let c = |id, s| Cue { id, start_us: s, end_us: s + 1_000_000, text: format!("c{id}") };
        d.push(&c(1, 10_500_000));
        d.push(&c(1, 10_500_000));
        d.push(&c(2, 12_000_000));
        d.push(&c(3, 13_000_000));
        assert_eq!(d.len(), 2);
        assert_eq!(d.dropped, 1);
        let v = validate(&d.to_vtt().unwrap()).unwrap();
        assert_eq!(v.cues[0].1, 500_000);
    }
}
