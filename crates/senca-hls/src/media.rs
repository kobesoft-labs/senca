//! media playlist（RFC 8216 4.3.2〜4.3.3 と LL-HLS の tag）。
//!
//! 時間はすべてマイクロ秒（`*_us`）で持つ。書き出した文字列を読み直すと同じ値になる。

use crate::attr::{AttrList, AttrWriter, format_micros, parse_integer, parse_micros};
use crate::error::HlsError;
use crate::lines::{Line, ParseLimits, split_lines};
use senca_core::rfc3339;
use std::fmt::Write as _;

/// `EXT-X-BYTERANGE` / `BYTERANGE=`（`length[@offset]`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub length: u64,
    /// 省略時は同じ資源の直前の範囲の続き。
    pub offset: Option<u64>,
}

impl ByteRange {
    fn parse(s: &str) -> Result<ByteRange, HlsError> {
        let (l, o) = match s.split_once('@') {
            Some((l, o)) => (l, Some(o)),
            None => (s, None),
        };
        let length = parse_integer(l).ok_or(HlsError::Syntax("byte range"))?;
        let offset = o.map(|o| parse_integer(o).ok_or(HlsError::Syntax("byte range"))).transpose()?;
        Ok(ByteRange { length, offset })
    }

    fn to_text(self) -> String {
        match self.offset {
            Some(o) => format!("{}@{o}", self.length),
            None => self.length.to_string(),
        }
    }
}

/// `EXT-X-MAP`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Map {
    pub uri: String,
    pub byte_range: Option<ByteRange>,
}

/// `EXT-X-PART`（LL-HLS の partial segment）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub uri: String,
    pub duration_us: u64,
    pub independent: bool,
    pub byte_range: Option<ByteRange>,
    pub gap: bool,
}

/// 完成した media segment。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaSegment {
    pub uri: String,
    pub duration_us: u64,
    /// EXTINF の title（通常は空）。
    pub title: String,
    pub byte_range: Option<ByteRange>,
    /// 直前に `EXT-X-DISCONTINUITY`。
    pub discontinuity: bool,
    /// `EXT-X-PROGRAM-DATE-TIME`（UNIX ミリ秒）。
    pub program_date_time_ms: Option<i64>,
    /// 直前に `EXT-X-MAP` がある（以降の segment にも効く）。
    pub map: Option<Map>,
    pub gap: bool,
    /// この segment を構成する partial segment（古い segment では省く）。
    pub parts: Vec<Part>,
}

impl MediaSegment {
    pub fn new(uri: impl Into<String>, duration_us: u64) -> Self {
        MediaSegment {
            uri: uri.into(),
            duration_us,
            title: String::new(),
            byte_range: None,
            discontinuity: false,
            program_date_time_ms: None,
            map: None,
            gap: false,
            parts: Vec::new(),
        }
    }
}

/// まだ完成していない最後の segment（LL-HLS で part だけが公開されているもの）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenSegment {
    pub discontinuity: bool,
    pub program_date_time_ms: Option<i64>,
    pub map: Option<Map>,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistType {
    Event,
    Vod,
}

/// `EXT-X-SERVER-CONTROL`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerControl {
    pub can_block_reload: bool,
    pub can_skip_until_us: Option<u64>,
    pub can_skip_dateranges: bool,
    pub hold_back_us: Option<u64>,
    pub part_hold_back_us: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreloadHintType {
    Part,
    Map,
}

/// `EXT-X-PRELOAD-HINT`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreloadHint {
    pub hint_type: PreloadHintType,
    pub uri: String,
    pub byte_range_start: Option<u64>,
    pub byte_range_length: Option<u64>,
}

/// `EXT-X-RENDITION-REPORT`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenditionReport {
    pub uri: String,
    pub last_msn: Option<u64>,
    pub last_part: Option<u64>,
}

/// media playlist。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPlaylist {
    /// 明示する版。書き出しでは使っている tag が要求する版と大きい方を書く。
    pub version: Option<u32>,
    /// `EXT-X-TARGETDURATION`（秒）。
    pub target_duration: u64,
    pub media_sequence: u64,
    pub discontinuity_sequence: u64,
    pub playlist_type: Option<PlaylistType>,
    pub independent_segments: bool,
    pub server_control: Option<ServerControl>,
    /// `EXT-X-PART-INF:PART-TARGET`。
    pub part_target_us: Option<u64>,
    /// `EXT-X-SKIP:SKIPPED-SEGMENTS`（delta update）。
    pub skipped_segments: Option<u64>,
    pub segments: Vec<MediaSegment>,
    pub open_segment: Option<OpenSegment>,
    pub preload_hints: Vec<PreloadHint>,
    pub rendition_reports: Vec<RenditionReport>,
    pub end_list: bool,
}

impl MediaPlaylist {
    pub fn new(target_duration: u64) -> Self {
        MediaPlaylist {
            version: None,
            target_duration,
            media_sequence: 0,
            discontinuity_sequence: 0,
            playlist_type: None,
            independent_segments: false,
            server_control: None,
            part_target_us: None,
            skipped_segments: None,
            segments: Vec::new(),
            open_segment: None,
            preload_hints: Vec::new(),
            rendition_reports: Vec::new(),
            end_list: false,
        }
    }

    /// 使っている tag が要求する最小の版（RFC 8216 7、LL-HLS は Apple の仕様）。
    pub fn required_version(&self) -> u32 {
        let mut v = 3; // 小数の EXTINF
        let segs = || self.segments.iter();
        if segs().any(|s| s.byte_range.is_some()) {
            v = v.max(4);
        }
        let open_map = self.open_segment.as_ref().is_some_and(|o| o.map.is_some());
        if segs().any(|s| s.map.is_some()) || open_map {
            v = v.max(6);
        }
        if self.skipped_segments.is_some() {
            v = v.max(9);
        }
        if self.server_control.is_some_and(|c| c.can_skip_dateranges) {
            v = v.max(10);
        }
        v
    }

    /// 規則に合うかを確かめる（書き出しの前に使う）。
    pub fn validate(&self) -> Result<(), HlsError> {
        for s in &self.segments {
            // RFC 8216 4.3.3.1: 四捨五入した EXTINF が TARGETDURATION 以下
            if s.duration_us.saturating_add(500_000) / 1_000_000 > self.target_duration {
                return Err(HlsError::InvalidValue("segment longer than target duration"));
            }
        }
        if let Some(pt) = self.part_target_us {
            let all_parts = self.segments.iter().flat_map(|s| s.parts.iter()).chain(self.open_segment.iter().flat_map(|o| o.parts.iter()));
            for p in all_parts {
                if p.duration_us > pt {
                    return Err(HlsError::InvalidValue("part longer than part target"));
                }
            }
        } else if self.segments.iter().any(|s| !s.parts.is_empty()) || self.open_segment.is_some() {
            return Err(HlsError::InvalidValue("parts without EXT-X-PART-INF"));
        }
        if self.playlist_type == Some(PlaylistType::Vod) && !self.end_list {
            return Err(HlsError::InvalidValue("VOD playlist without ENDLIST"));
        }
        Ok(())
    }

    /// m3u8 の文字列にする。URI や引用符付きの値に `"`・改行があれば `Err`。
    pub fn to_m3u8(&self) -> Result<String, HlsError> {
        let mut o = String::with_capacity(256 + self.segments.len() * 64);
        let version = self.version.unwrap_or(0).max(self.required_version());
        let _ = writeln!(o, "#EXTM3U\n#EXT-X-VERSION:{version}\n#EXT-X-TARGETDURATION:{}", self.target_duration);
        if let Some(sc) = &self.server_control {
            let mut a = AttrWriter::default();
            if sc.can_block_reload {
                a.yes("CAN-BLOCK-RELOAD");
            }
            if let Some(v) = sc.can_skip_until_us {
                a.micros("CAN-SKIP-UNTIL", v);
            }
            if sc.can_skip_dateranges {
                a.yes("CAN-SKIP-DATERANGES");
            }
            if let Some(v) = sc.hold_back_us {
                a.micros("HOLD-BACK", v);
            }
            if let Some(v) = sc.part_hold_back_us {
                a.micros("PART-HOLD-BACK", v);
            }
            let _ = writeln!(o, "#EXT-X-SERVER-CONTROL:{}", a.finish());
        }
        if let Some(pt) = self.part_target_us {
            let _ = writeln!(o, "#EXT-X-PART-INF:PART-TARGET={}", format_micros(pt));
        }
        let _ = writeln!(o, "#EXT-X-MEDIA-SEQUENCE:{}", self.media_sequence);
        if self.discontinuity_sequence != 0 {
            let _ = writeln!(o, "#EXT-X-DISCONTINUITY-SEQUENCE:{}", self.discontinuity_sequence);
        }
        match self.playlist_type {
            Some(PlaylistType::Vod) => o.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n"),
            Some(PlaylistType::Event) => o.push_str("#EXT-X-PLAYLIST-TYPE:EVENT\n"),
            None => {}
        }
        if self.independent_segments {
            o.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
        }
        if let Some(n) = self.skipped_segments {
            let _ = writeln!(o, "#EXT-X-SKIP:SKIPPED-SEGMENTS={n}");
        }
        for s in &self.segments {
            write_segment_tags(&mut o, s.discontinuity, s.map.as_ref(), s.program_date_time_ms, &s.parts)?;
            if s.gap {
                o.push_str("#EXT-X-GAP\n");
            }
            if s.title.contains(['\r', '\n']) {
                return Err(HlsError::InvalidValue("title"));
            }
            let _ = writeln!(o, "#EXTINF:{},{}", format_micros(s.duration_us), s.title);
            if let Some(br) = s.byte_range {
                let _ = writeln!(o, "#EXT-X-BYTERANGE:{}", br.to_text());
            }
            check_uri(&s.uri)?;
            o.push_str(&s.uri);
            o.push('\n');
        }
        if let Some(os) = &self.open_segment {
            write_segment_tags(&mut o, os.discontinuity, os.map.as_ref(), os.program_date_time_ms, &os.parts)?;
        }
        for h in &self.preload_hints {
            let mut a = AttrWriter::default();
            a.enumerated("TYPE", if h.hint_type == PreloadHintType::Part { "PART" } else { "MAP" })?;
            a.quoted("URI", &h.uri)?;
            if let Some(v) = h.byte_range_start {
                a.integer("BYTERANGE-START", v);
            }
            if let Some(v) = h.byte_range_length {
                a.integer("BYTERANGE-LENGTH", v);
            }
            let _ = writeln!(o, "#EXT-X-PRELOAD-HINT:{}", a.finish());
        }
        for r in &self.rendition_reports {
            let mut a = AttrWriter::default();
            a.quoted("URI", &r.uri)?;
            if let Some(v) = r.last_msn {
                a.integer("LAST-MSN", v);
            }
            if let Some(v) = r.last_part {
                a.integer("LAST-PART", v);
            }
            let _ = writeln!(o, "#EXT-X-RENDITION-REPORT:{}", a.finish());
        }
        if self.end_list {
            o.push_str("#EXT-X-ENDLIST\n");
        }
        Ok(o)
    }

    pub fn parse(input: &[u8]) -> Result<MediaPlaylist, HlsError> {
        Self::parse_with_limits(input, &ParseLimits::DEFAULT)
    }

    pub fn parse_with_limits(input: &[u8], limits: &ParseLimits) -> Result<MediaPlaylist, HlsError> {
        let lines = split_lines(input, limits)?;
        parse_media(&lines)
    }
}

fn check_uri(u: &str) -> Result<(), HlsError> {
    if u.is_empty() || u.starts_with('#') || u.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(HlsError::InvalidValue("URI"));
    }
    Ok(())
}

fn write_segment_tags(o: &mut String, disc: bool, map: Option<&Map>, pdt: Option<i64>, parts: &[Part]) -> Result<(), HlsError> {
    if disc {
        o.push_str("#EXT-X-DISCONTINUITY\n");
    }
    if let Some(m) = map {
        let mut a = AttrWriter::default();
        a.quoted("URI", &m.uri)?;
        if let Some(br) = m.byte_range {
            a.quoted("BYTERANGE", &br.to_text())?;
        }
        let _ = writeln!(o, "#EXT-X-MAP:{}", a.finish());
    }
    if let Some(ms) = pdt {
        let _ = writeln!(o, "#EXT-X-PROGRAM-DATE-TIME:{}", rfc3339::format_millis(ms));
    }
    for p in parts {
        let mut a = AttrWriter::default();
        a.micros("DURATION", p.duration_us);
        a.quoted("URI", &p.uri)?;
        if p.independent {
            a.yes("INDEPENDENT");
        }
        if let Some(br) = p.byte_range {
            a.quoted("BYTERANGE", &br.to_text())?;
        }
        if p.gap {
            a.yes("GAP");
        }
        let _ = writeln!(o, "#EXT-X-PART:{}", a.finish());
    }
    Ok(())
}

/// ISO 8601 の日時。RFC 3339 に加えて、FFmpeg などが書く `+0900` 形式の時差も受け付ける。
pub(crate) fn parse_date_time(s: &str) -> Option<i64> {
    if let Some(v) = rfc3339::parse_millis(s) {
        return Some(v);
    }
    let b = s.as_bytes();
    if b.len() > 5 {
        let tail = &b[b.len() - 5..];
        if matches!(tail[0], b'+' | b'-') && tail[1..].iter().all(u8::is_ascii_digit) {
            let fixed = format!("{}:{}", &s[..s.len() - 2], &s[s.len() - 2..]);
            return rfc3339::parse_millis(&fixed);
        }
    }
    None
}

fn parse_part(v: &str) -> Result<Part, HlsError> {
    let a = AttrList::parse(v)?;
    Ok(Part {
        uri: AttrList::require(a.quoted("URI")?, "URI")?.to_owned(),
        duration_us: AttrList::require(a.micros("DURATION")?, "DURATION")?,
        independent: a.yes_no("INDEPENDENT")?.unwrap_or(false),
        byte_range: a.quoted("BYTERANGE")?.map(ByteRange::parse).transpose()?,
        gap: a.yes_no("GAP")?.unwrap_or(false),
    })
}

#[derive(Default)]
struct Pending {
    discontinuity: bool,
    pdt: Option<i64>,
    map: Option<Map>,
    gap: bool,
    parts: Vec<Part>,
    extinf: Option<(u64, String)>,
    byte_range: Option<ByteRange>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        !self.discontinuity && self.pdt.is_none() && self.map.is_none() && !self.gap && self.parts.is_empty() && self.extinf.is_none() && self.byte_range.is_none()
    }
}

fn once<T>(slot: &mut Option<T>, v: T, tag: &'static str) -> Result<(), HlsError> {
    if slot.is_some() {
        return Err(HlsError::Tag(tag));
    }
    *slot = Some(v);
    Ok(())
}

fn parse_media(lines: &[Line<'_>]) -> Result<MediaPlaylist, HlsError> {
    let mut version = None;
    let mut target = None;
    let mut msn = None;
    let mut dsn = None;
    let mut ptype = None;
    let mut sc = None;
    let mut part_inf = None;
    let mut skip = None;
    let mut independent = false;
    let mut end_list = false;
    let mut segments = Vec::new();
    let mut hints = Vec::new();
    let mut reports = Vec::new();
    let mut p = Pending::default();
    for line in lines {
        let (name, value) = match *line {
            Line::Uri(u) => {
                let Some((duration_us, title)) = p.extinf.take() else {
                    return Err(HlsError::Tag("URI without EXTINF"));
                };
                if end_list {
                    return Err(HlsError::Tag("segment after EXT-X-ENDLIST"));
                }
                let pend = std::mem::take(&mut p);
                segments.push(MediaSegment {
                    uri: u.to_owned(),
                    duration_us,
                    title,
                    byte_range: pend.byte_range,
                    discontinuity: pend.discontinuity,
                    program_date_time_ms: pend.pdt,
                    map: pend.map,
                    gap: pend.gap,
                    parts: pend.parts,
                });
                continue;
            }
            Line::Tag(n, v) => (n, v),
        };
        let val = || value.ok_or(HlsError::Tag("tag value missing"));
        let no_val = |tag: &'static str| if value.is_some() { Err(HlsError::Tag(tag)) } else { Ok(()) };
        let header_tag = |tag: &'static str| {
            if !segments.is_empty() || !p.is_empty() {
                Err(HlsError::Tag(tag))
            } else {
                Ok(())
            }
        };
        match name {
            "EXTM3U" => return Err(HlsError::Tag("EXTM3U")),
            "EXT-X-VERSION" => {
                let v = parse_integer(val()?).and_then(|v| u32::try_from(v).ok()).ok_or(HlsError::Tag("EXT-X-VERSION"))?;
                once(&mut version, v, "EXT-X-VERSION")?;
            }
            "EXT-X-TARGETDURATION" => {
                let v = parse_integer(val()?).ok_or(HlsError::Tag("EXT-X-TARGETDURATION"))?;
                once(&mut target, v, "EXT-X-TARGETDURATION")?;
            }
            "EXT-X-MEDIA-SEQUENCE" => {
                header_tag("EXT-X-MEDIA-SEQUENCE")?;
                let v = parse_integer(val()?).ok_or(HlsError::Tag("EXT-X-MEDIA-SEQUENCE"))?;
                once(&mut msn, v, "EXT-X-MEDIA-SEQUENCE")?;
            }
            "EXT-X-DISCONTINUITY-SEQUENCE" => {
                header_tag("EXT-X-DISCONTINUITY-SEQUENCE")?;
                let v = parse_integer(val()?).ok_or(HlsError::Tag("EXT-X-DISCONTINUITY-SEQUENCE"))?;
                once(&mut dsn, v, "EXT-X-DISCONTINUITY-SEQUENCE")?;
            }
            "EXT-X-PLAYLIST-TYPE" => {
                let t = match val()? {
                    "VOD" => PlaylistType::Vod,
                    "EVENT" => PlaylistType::Event,
                    _ => return Err(HlsError::Tag("EXT-X-PLAYLIST-TYPE")),
                };
                once(&mut ptype, t, "EXT-X-PLAYLIST-TYPE")?;
            }
            "EXT-X-INDEPENDENT-SEGMENTS" => {
                no_val("EXT-X-INDEPENDENT-SEGMENTS")?;
                independent = true;
            }
            "EXT-X-ENDLIST" => {
                no_val("EXT-X-ENDLIST")?;
                if end_list {
                    return Err(HlsError::Tag("EXT-X-ENDLIST"));
                }
                end_list = true;
            }
            "EXT-X-SERVER-CONTROL" => {
                let a = AttrList::parse(val()?)?;
                let c = ServerControl {
                    can_block_reload: a.yes_no("CAN-BLOCK-RELOAD")?.unwrap_or(false),
                    can_skip_until_us: a.micros("CAN-SKIP-UNTIL")?,
                    can_skip_dateranges: a.yes_no("CAN-SKIP-DATERANGES")?.unwrap_or(false),
                    hold_back_us: a.micros("HOLD-BACK")?,
                    part_hold_back_us: a.micros("PART-HOLD-BACK")?,
                };
                once(&mut sc, c, "EXT-X-SERVER-CONTROL")?;
            }
            "EXT-X-PART-INF" => {
                let a = AttrList::parse(val()?)?;
                let pt = AttrList::require(a.micros("PART-TARGET")?, "PART-TARGET")?;
                once(&mut part_inf, pt, "EXT-X-PART-INF")?;
            }
            "EXT-X-SKIP" => {
                header_tag("EXT-X-SKIP")?;
                let a = AttrList::parse(val()?)?;
                let n = AttrList::require(a.integer("SKIPPED-SEGMENTS")?, "SKIPPED-SEGMENTS")?;
                once(&mut skip, n, "EXT-X-SKIP")?;
            }
            "EXTINF" => {
                let v = val()?;
                let (d, title) = v.split_once(',').ok_or(HlsError::Tag("EXTINF"))?;
                let d = parse_micros(d).ok_or(HlsError::Tag("EXTINF"))?;
                if p.extinf.is_some() {
                    return Err(HlsError::Tag("EXTINF"));
                }
                p.extinf = Some((d, title.to_owned()));
            }
            "EXT-X-BYTERANGE" => {
                if p.byte_range.is_some() {
                    return Err(HlsError::Tag("EXT-X-BYTERANGE"));
                }
                p.byte_range = Some(ByteRange::parse(val()?)?);
            }
            "EXT-X-DISCONTINUITY" => {
                no_val("EXT-X-DISCONTINUITY")?;
                p.discontinuity = true;
            }
            "EXT-X-GAP" => {
                no_val("EXT-X-GAP")?;
                p.gap = true;
            }
            "EXT-X-PROGRAM-DATE-TIME" => {
                let ms = parse_date_time(val()?).ok_or(HlsError::Tag("EXT-X-PROGRAM-DATE-TIME"))?;
                p.pdt = Some(ms);
            }
            "EXT-X-MAP" => {
                let a = AttrList::parse(val()?)?;
                let uri = AttrList::require(a.quoted("URI")?, "URI")?.to_owned();
                let byte_range = a.quoted("BYTERANGE")?.map(ByteRange::parse).transpose()?;
                p.map = Some(Map { uri, byte_range });
            }
            "EXT-X-PART" => {
                if p.extinf.is_some() {
                    return Err(HlsError::Tag("EXT-X-PART after EXTINF"));
                }
                if p.parts.len() >= 1000 {
                    return Err(HlsError::TooLarge);
                }
                p.parts.push(parse_part(val()?)?);
            }
            "EXT-X-PRELOAD-HINT" => {
                let a = AttrList::parse(val()?)?;
                let hint_type = match AttrList::require(a.enumerated("TYPE")?, "TYPE")? {
                    "PART" => PreloadHintType::Part,
                    "MAP" => PreloadHintType::Map,
                    _ => return Err(HlsError::Attribute("TYPE".into())),
                };
                hints.push(PreloadHint {
                    hint_type,
                    uri: AttrList::require(a.quoted("URI")?, "URI")?.to_owned(),
                    byte_range_start: a.integer("BYTERANGE-START")?,
                    byte_range_length: a.integer("BYTERANGE-LENGTH")?,
                });
            }
            "EXT-X-RENDITION-REPORT" => {
                let a = AttrList::parse(val()?)?;
                reports.push(RenditionReport {
                    uri: AttrList::require(a.quoted("URI")?, "URI")?.to_owned(),
                    last_msn: a.integer("LAST-MSN")?,
                    last_part: a.integer("LAST-PART")?,
                });
            }
            "EXT-X-KEY" => {
                let a = AttrList::parse(val()?)?;
                if a.enumerated("METHOD")? != Some("NONE") {
                    return Err(HlsError::Unsupported("encrypted segments (EXT-X-KEY)"));
                }
            }
            "EXT-X-I-FRAMES-ONLY" => return Err(HlsError::Unsupported("I-frame playlist")),
            "EXT-X-STREAM-INF" | "EXT-X-MEDIA" | "EXT-X-I-FRAME-STREAM-INF" | "EXT-X-SESSION-DATA" | "EXT-X-SESSION-KEY" => {
                return Err(HlsError::Tag("multivariant tag in media playlist"));
            }
            // RFC 8216 6.3.1: 知らない tag は無視する（EXT-X-DATERANGE・EXT-X-BITRATE・EXT-X-START など）
            _ => {}
        }
    }
    if p.extinf.is_some() || p.byte_range.is_some() {
        return Err(HlsError::Tag("EXTINF without URI"));
    }
    let open_segment = if p.is_empty() {
        None
    } else {
        if p.parts.is_empty() && p.map.is_none() && !p.discontinuity && p.pdt.is_none() {
            return Err(HlsError::Tag("dangling segment tags"));
        }
        Some(OpenSegment { discontinuity: p.discontinuity, program_date_time_ms: p.pdt, map: p.map, parts: p.parts })
    };
    let pl = MediaPlaylist {
        version,
        target_duration: target.ok_or(HlsError::Tag("EXT-X-TARGETDURATION missing"))?,
        media_sequence: msn.unwrap_or(0),
        discontinuity_sequence: dsn.unwrap_or(0),
        playlist_type: ptype,
        independent_segments: independent,
        server_control: sc,
        part_target_us: part_inf,
        skipped_segments: skip,
        segments,
        open_segment,
        preload_hints: hints,
        rendition_reports: reports,
        end_list,
    };
    if pl.part_target_us.is_none() && (pl.segments.iter().any(|s| !s.parts.is_empty()) || pl.open_segment.as_ref().is_some_and(|o| !o.parts.is_empty())) {
        return Err(HlsError::Tag("EXT-X-PART without EXT-X-PART-INF"));
    }
    if let Some(v) = pl.version
        && v < 3
        && pl.segments.iter().any(|s| s.duration_us % 1_000_000 != 0)
    {
        return Err(HlsError::Tag("decimal EXTINF requires version 3"));
    }
    if let Some(v) = pl.version
        && v < pl.required_version()
    {
        return Err(HlsError::Tag("EXT-X-VERSION too low for tags used"));
    }
    Ok(pl)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RFC_LIVE: &str = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:8\n#EXT-X-MEDIA-SEQUENCE:2680\n\n#EXTINF:7.975,\nhttps://priv.example.com/fileSequence2680.ts\n#EXTINF:7.941,\nhttps://priv.example.com/fileSequence2681.ts\n#EXTINF:7.975,\nhttps://priv.example.com/fileSequence2682.ts\n";

    // Apple の LL-HLS 仕様（draft-pantos-hls-rfc8216bis）の例に近い形
    const LL: &str = "#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-VERSION:6
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.0,CAN-SKIP-UNTIL=12.0
#EXT-X-PART-INF:PART-TARGET=0.33334
#EXT-X-MEDIA-SEQUENCE:266
#EXT-X-PROGRAM-DATE-TIME:2019-02-14T02:13:36.106Z
#EXT-X-MAP:URI=\"init.mp4\"
#EXTINF:4.00008,
fileSequence266.mp4
#EXTINF:4.00008,
fileSequence267.mp4
#EXT-X-PART:DURATION=0.33334,URI=\"filePart268.0.mp4\",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.33334,URI=\"filePart268.1.mp4\"
#EXTINF:0.66668,
fileSequence268.mp4
#EXT-X-PART:DURATION=0.33334,URI=\"filePart269.0.mp4\",INDEPENDENT=YES
#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"filePart269.1.mp4\"
#EXT-X-RENDITION-REPORT:URI=\"../1M/waitForMSN.php\",LAST-MSN=269,LAST-PART=1
";

    #[test]
    fn rfc_example() {
        let p = MediaPlaylist::parse(RFC_LIVE.as_bytes()).unwrap();
        assert_eq!(p.media_sequence, 2680);
        assert_eq!(p.segments.len(), 3);
        assert_eq!(p.segments[1].duration_us, 7_941_000);
        let again = MediaPlaylist::parse(p.to_m3u8().unwrap().as_bytes()).unwrap();
        assert_eq!(again, p);
    }

    #[test]
    fn ll_hls_example() {
        let p = MediaPlaylist::parse(LL.as_bytes()).unwrap();
        assert_eq!(p.part_target_us, Some(333_340));
        assert!(p.server_control.unwrap().can_block_reload);
        assert_eq!(p.segments[0].map.as_ref().unwrap().uri, "init.mp4");
        assert_eq!(p.segments[2].parts.len(), 2);
        assert!(p.segments[2].parts[0].independent);
        let os = p.open_segment.as_ref().unwrap();
        assert_eq!(os.parts.len(), 1);
        assert_eq!(p.preload_hints[0].uri, "filePart269.1.mp4");
        assert_eq!(p.rendition_reports[0].last_part, Some(1));
        assert_eq!(p.segments[0].program_date_time_ms, rfc3339::parse_millis("2019-02-14T02:13:36.106Z"));
        let text = p.to_m3u8().unwrap();
        assert_eq!(MediaPlaylist::parse(text.as_bytes()).unwrap(), p);
    }

    #[test]
    fn ffmpeg_style_date_and_byterange() {
        let s = "#EXTM3U\n#EXT-X-VERSION:4\n#EXT-X-TARGETDURATION:2\n#EXT-X-PROGRAM-DATE-TIME:2026-11-02T21:00:00.500+0900\n#EXTINF:2.000000,\n#EXT-X-BYTERANGE:1000@0\nall.ts\n#EXTINF:1.5,\n#EXT-X-BYTERANGE:500\nall.ts\n#EXT-X-ENDLIST\n";
        let p = MediaPlaylist::parse(s.as_bytes()).unwrap();
        assert_eq!(p.segments[0].program_date_time_ms, rfc3339::parse_millis("2026-11-02T12:00:00.500Z"));
        assert_eq!(p.segments[1].byte_range, Some(ByteRange { length: 500, offset: None }));
        assert!(p.end_list);
        assert_eq!(MediaPlaylist::parse(p.to_m3u8().unwrap().as_bytes()).unwrap(), p);
    }

    #[test]
    fn rejects() {
        for bad in [
            "",
            "#EXTM3\n",
            " #EXTM3U\n#EXT-X-TARGETDURATION:2\n",
            "#EXTM3U\n#EXTINF:2,\na.ts\n",                                        // TARGETDURATION なし
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\na.ts\n",                           // EXTINF なし
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\n",                     // URI なし
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-TARGETDURATION:2\n",        // 重複
            "#EXTM3U\n#EXT-X-TARGETDURATION:x\n",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2\na.ts\n",                // カンマなし
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:-2,\na.ts\n",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\na.ts\n#EXT-X-MEDIA-SEQUENCE:3\n",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-PART:DURATION=1,URI=\"a\"\n", // PART-INF なし
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-STREAM-INF:BANDWIDTH=1\na.m3u8\n",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\na b.ts\n",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\na\rb.ts\n",
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MAP:URI=\"i.mp4\"\n#EXTINF:2,\na.ts\n",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-ENDLIST\n#EXTINF:2,\na.ts\n",
        ] {
            assert!(MediaPlaylist::parse(bad.as_bytes()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn writer_rejects_injection() {
        let mut p = MediaPlaylist::new(2);
        p.segments.push(MediaSegment::new("a.ts\n#EXT-X-ENDLIST", 2_000_000));
        assert!(p.to_m3u8().is_err());
        let mut p = MediaPlaylist::new(2);
        let mut s = MediaSegment::new("a.ts", 2_000_000);
        s.map = Some(Map { uri: "x\"y".into(), byte_range: None });
        p.segments.push(s);
        assert!(p.to_m3u8().is_err());
    }

    #[test]
    fn validate_rules() {
        let mut p = MediaPlaylist::new(2);
        p.segments.push(MediaSegment::new("a.ts", 2_499_999));
        assert!(p.validate().is_ok());
        p.segments.push(MediaSegment::new("b.ts", 2_500_000));
        assert!(p.validate().is_err());
    }
}
