//! multivariant playlist（RFC 8216 4.3.4）。

use crate::attr::{AttrList, AttrWriter, parse_integer};
use crate::error::HlsError;
use crate::lines::{Line, ParseLimits, split_lines};
use crate::media::MediaPlaylist;
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Audio,
    Video,
    Subtitles,
    ClosedCaptions,
}

impl MediaType {
    fn as_str(self) -> &'static str {
        match self {
            MediaType::Audio => "AUDIO",
            MediaType::Video => "VIDEO",
            MediaType::Subtitles => "SUBTITLES",
            MediaType::ClosedCaptions => "CLOSED-CAPTIONS",
        }
    }
}

/// `EXT-X-MEDIA`（別の rendition）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendition {
    pub media_type: MediaType,
    pub group_id: String,
    pub name: String,
    pub uri: Option<String>,
    pub language: Option<String>,
    pub assoc_language: Option<String>,
    pub default: bool,
    pub autoselect: bool,
    pub forced: bool,
    pub instream_id: Option<String>,
    pub characteristics: Option<String>,
    pub channels: Option<String>,
}

impl Rendition {
    pub fn new(media_type: MediaType, group_id: impl Into<String>, name: impl Into<String>) -> Self {
        Rendition {
            media_type,
            group_id: group_id.into(),
            name: name.into(),
            uri: None,
            language: None,
            assoc_language: None,
            default: false,
            autoselect: false,
            forced: false,
            instream_id: None,
            characteristics: None,
            channels: None,
        }
    }
}

/// `CLOSED-CAPTIONS` 属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosedCaptions {
    None,
    Group(String),
}

/// `EXT-X-STREAM-INF` と続く URI。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantStream {
    pub uri: String,
    pub bandwidth: u64,
    pub average_bandwidth: Option<u64>,
    pub codecs: Option<String>,
    pub resolution: Option<(u32, u32)>,
    /// FRAME-RATE（1/1000 fps 単位。29.97 → 29970）。
    pub frame_rate_milli: Option<u64>,
    pub audio: Option<String>,
    pub video: Option<String>,
    pub subtitles: Option<String>,
    pub closed_captions: Option<ClosedCaptions>,
}

impl VariantStream {
    pub fn new(uri: impl Into<String>, bandwidth: u64) -> Self {
        VariantStream {
            uri: uri.into(),
            bandwidth,
            average_bandwidth: None,
            codecs: None,
            resolution: None,
            frame_rate_milli: None,
            audio: None,
            video: None,
            subtitles: None,
            closed_captions: None,
        }
    }
}

/// multivariant playlist。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MultivariantPlaylist {
    pub version: Option<u32>,
    pub independent_segments: bool,
    pub renditions: Vec<Rendition>,
    pub variants: Vec<VariantStream>,
}

/// playlist の種類（読むまで分からないとき）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playlist {
    Media(Box<MediaPlaylist>),
    Multivariant(MultivariantPlaylist),
}

impl Playlist {
    /// tag を見てどちらかに決めて読む（両方の tag が混ざっていれば `Err`）。
    pub fn parse(input: &[u8]) -> Result<Playlist, HlsError> {
        let lines = split_lines(input, &ParseLimits::DEFAULT)?;
        let is_mv = lines.iter().any(|l| matches!(l, Line::Tag("EXT-X-STREAM-INF" | "EXT-X-MEDIA", _)));
        if is_mv {
            Ok(Playlist::Multivariant(parse_multivariant(&lines)?))
        } else {
            Ok(Playlist::Media(Box::new(MediaPlaylist::parse(input)?)))
        }
    }
}

fn frame_rate_milli(s: &str) -> Option<u64> {
    let (i, f) = s.split_once('.').unwrap_or((s, ""));
    if i.is_empty() || i.len() > 6 || !i.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if s.contains('.') && (f.is_empty() || f.len() > 3 || !f.bytes().all(|c| c.is_ascii_digit())) {
        return None;
    }
    let mut frac = f.to_owned();
    while frac.len() < 3 {
        frac.push('0');
    }
    Some(i.parse::<u64>().ok()? * 1000 + frac.parse::<u64>().ok()?)
}

fn group_check(v: &MultivariantPlaylist) -> Result<(), HlsError> {
    for var in &v.variants {
        for (g, t) in [(&var.audio, MediaType::Audio), (&var.video, MediaType::Video), (&var.subtitles, MediaType::Subtitles)] {
            if let Some(g) = g
                && !v.renditions.iter().any(|r| r.media_type == t && &r.group_id == g)
            {
                return Err(HlsError::Tag("STREAM-INF refers to unknown group"));
            }
        }
        if let Some(ClosedCaptions::Group(g)) = &var.closed_captions
            && !v.renditions.iter().any(|r| r.media_type == MediaType::ClosedCaptions && &r.group_id == g)
        {
            return Err(HlsError::Tag("STREAM-INF refers to unknown group"));
        }
    }
    for (i, r) in v.renditions.iter().enumerate() {
        if v.renditions[..i].iter().any(|o| o.media_type == r.media_type && o.group_id == r.group_id && o.name == r.name) {
            return Err(HlsError::Tag("duplicate rendition NAME in group"));
        }
    }
    Ok(())
}

impl MultivariantPlaylist {
    pub fn parse(input: &[u8]) -> Result<MultivariantPlaylist, HlsError> {
        parse_multivariant(&split_lines(input, &ParseLimits::DEFAULT)?)
    }

    pub fn to_m3u8(&self) -> Result<String, HlsError> {
        group_check(self)?;
        let mut o = String::from("#EXTM3U\n");
        let version = self.version.unwrap_or(0).max(if self.renditions.iter().any(|r| r.instream_id.as_deref().is_some_and(|s| s.starts_with("SERVICE"))) { 7 } else { 3 });
        let _ = writeln!(o, "#EXT-X-VERSION:{version}");
        if self.independent_segments {
            o.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
        }
        for r in &self.renditions {
            let mut a = AttrWriter::default();
            a.enumerated("TYPE", r.media_type.as_str())?;
            a.quoted("GROUP-ID", &r.group_id)?;
            a.quoted("NAME", &r.name)?;
            if let Some(l) = &r.language {
                a.quoted("LANGUAGE", l)?;
            }
            if let Some(l) = &r.assoc_language {
                a.quoted("ASSOC-LANGUAGE", l)?;
            }
            if r.default {
                a.yes("DEFAULT");
            }
            if r.autoselect || r.default {
                a.yes("AUTOSELECT");
            }
            if r.forced {
                if r.media_type != MediaType::Subtitles {
                    return Err(HlsError::InvalidValue("FORCED is only for SUBTITLES"));
                }
                a.yes("FORCED");
            }
            if let Some(v) = &r.instream_id {
                a.quoted("INSTREAM-ID", v)?;
            }
            if let Some(v) = &r.characteristics {
                a.quoted("CHARACTERISTICS", v)?;
            }
            if let Some(v) = &r.channels {
                a.quoted("CHANNELS", v)?;
            }
            match (&r.uri, r.media_type) {
                (Some(_), MediaType::ClosedCaptions) => return Err(HlsError::InvalidValue("URI with CLOSED-CAPTIONS")),
                (None, MediaType::Subtitles) => return Err(HlsError::InvalidValue("SUBTITLES rendition needs URI")),
                (Some(u), _) => {
                    a.quoted("URI", u)?;
                }
                _ => {}
            }
            let _ = writeln!(o, "#EXT-X-MEDIA:{}", a.finish());
        }
        for v in &self.variants {
            let mut a = AttrWriter::default();
            a.integer("BANDWIDTH", v.bandwidth);
            if let Some(x) = v.average_bandwidth {
                a.integer("AVERAGE-BANDWIDTH", x);
            }
            if let Some(x) = &v.codecs {
                a.quoted("CODECS", x)?;
            }
            if let Some((w, h)) = v.resolution {
                a.enumerated("RESOLUTION", &format!("{w}x{h}"))?;
            }
            if let Some(f) = v.frame_rate_milli {
                a.enumerated("FRAME-RATE", &format!("{}.{:03}", f / 1000, f % 1000))?;
            }
            if let Some(x) = &v.audio {
                a.quoted("AUDIO", x)?;
            }
            if let Some(x) = &v.video {
                a.quoted("VIDEO", x)?;
            }
            if let Some(x) = &v.subtitles {
                a.quoted("SUBTITLES", x)?;
            }
            match &v.closed_captions {
                Some(ClosedCaptions::None) => {
                    a.enumerated("CLOSED-CAPTIONS", "NONE")?;
                }
                Some(ClosedCaptions::Group(g)) => {
                    a.quoted("CLOSED-CAPTIONS", g)?;
                }
                None => {}
            }
            if v.uri.is_empty() || v.uri.starts_with('#') || v.uri.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(HlsError::InvalidValue("URI"));
            }
            let _ = writeln!(o, "#EXT-X-STREAM-INF:{}\n{}", a.finish(), v.uri);
        }
        Ok(o)
    }
}

fn parse_multivariant(lines: &[Line<'_>]) -> Result<MultivariantPlaylist, HlsError> {
    let mut pl = MultivariantPlaylist::default();
    let mut pending: Option<VariantStream> = None;
    for line in lines {
        match *line {
            Line::Uri(u) => {
                let mut v = pending.take().ok_or(HlsError::Tag("URI without EXT-X-STREAM-INF"))?;
                v.uri = u.to_owned();
                pl.variants.push(v);
            }
            Line::Tag(name, value) => {
                if pending.is_some() {
                    return Err(HlsError::Tag("EXT-X-STREAM-INF must be followed by URI"));
                }
                let val = || value.ok_or(HlsError::Tag("tag value missing"));
                match name {
                    "EXT-X-VERSION" => {
                        if pl.version.is_some() {
                            return Err(HlsError::Tag("EXT-X-VERSION"));
                        }
                        pl.version = Some(parse_integer(val()?).and_then(|v| u32::try_from(v).ok()).ok_or(HlsError::Tag("EXT-X-VERSION"))?);
                    }
                    "EXT-X-INDEPENDENT-SEGMENTS" => pl.independent_segments = true,
                    "EXT-X-MEDIA" => {
                        let a = AttrList::parse(val()?)?;
                        let media_type = match AttrList::require(a.enumerated("TYPE")?, "TYPE")? {
                            "AUDIO" => MediaType::Audio,
                            "VIDEO" => MediaType::Video,
                            "SUBTITLES" => MediaType::Subtitles,
                            "CLOSED-CAPTIONS" => MediaType::ClosedCaptions,
                            _ => return Err(HlsError::Attribute("TYPE".into())),
                        };
                        let default = a.yes_no("DEFAULT")?.unwrap_or(false);
                        let autoselect = a.yes_no("AUTOSELECT")?;
                        if default && autoselect == Some(false) {
                            return Err(HlsError::Attribute("AUTOSELECT".into()));
                        }
                        let r = Rendition {
                            media_type,
                            group_id: AttrList::require(a.quoted("GROUP-ID")?, "GROUP-ID")?.to_owned(),
                            name: AttrList::require(a.quoted("NAME")?, "NAME")?.to_owned(),
                            uri: a.quoted("URI")?.map(str::to_owned),
                            language: a.quoted("LANGUAGE")?.map(str::to_owned),
                            assoc_language: a.quoted("ASSOC-LANGUAGE")?.map(str::to_owned),
                            default,
                            autoselect: autoselect.unwrap_or(false) || default,
                            forced: a.yes_no("FORCED")?.unwrap_or(false),
                            instream_id: a.quoted("INSTREAM-ID")?.map(str::to_owned),
                            characteristics: a.quoted("CHARACTERISTICS")?.map(str::to_owned),
                            channels: a.quoted("CHANNELS")?.map(str::to_owned),
                        };
                        match (media_type, &r.uri, &r.instream_id) {
                            (MediaType::ClosedCaptions, Some(_), _) | (MediaType::ClosedCaptions, _, None) => {
                                return Err(HlsError::Tag("CLOSED-CAPTIONS rendition"));
                            }
                            (MediaType::Subtitles, None, _) => return Err(HlsError::MissingAttribute("URI".into())),
                            _ => {}
                        }
                        if pl.renditions.len() >= 1000 {
                            return Err(HlsError::TooLarge);
                        }
                        pl.renditions.push(r);
                    }
                    "EXT-X-STREAM-INF" => {
                        let a = AttrList::parse(val()?)?;
                        let mut v = VariantStream::new(String::new(), AttrList::require(a.integer("BANDWIDTH")?, "BANDWIDTH")?);
                        v.average_bandwidth = a.integer("AVERAGE-BANDWIDTH")?;
                        v.codecs = a.quoted("CODECS")?.map(str::to_owned);
                        v.resolution = a.resolution("RESOLUTION")?;
                        v.frame_rate_milli = a
                            .enumerated("FRAME-RATE")?
                            .map(|s| frame_rate_milli(s).ok_or(HlsError::Attribute("FRAME-RATE".into())))
                            .transpose()?;
                        v.audio = a.quoted("AUDIO")?.map(str::to_owned);
                        v.video = a.quoted("VIDEO")?.map(str::to_owned);
                        v.subtitles = a.quoted("SUBTITLES")?.map(str::to_owned);
                        v.closed_captions = match (a.enumerated("CLOSED-CAPTIONS"), a.quoted("CLOSED-CAPTIONS")) {
                            (Ok(Some("NONE")), _) => Some(ClosedCaptions::None),
                            (Ok(Some(_)), _) => return Err(HlsError::Attribute("CLOSED-CAPTIONS".into())),
                            (_, Ok(Some(g))) => Some(ClosedCaptions::Group(g.to_owned())),
                            _ => None,
                        };
                        if pl.variants.len() >= 1000 {
                            return Err(HlsError::TooLarge);
                        }
                        pending = Some(v);
                    }
                    "EXTINF" | "EXT-X-TARGETDURATION" | "EXT-X-MEDIA-SEQUENCE" | "EXT-X-PART" | "EXT-X-ENDLIST" => {
                        return Err(HlsError::Tag("media playlist tag in multivariant playlist"));
                    }
                    _ => {}
                }
            }
        }
    }
    if pending.is_some() {
        return Err(HlsError::Tag("EXT-X-STREAM-INF must be followed by URI"));
    }
    group_check(&pl)?;
    Ok(pl)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 8216 8.6 / 8.7 の例を合わせたもの
    const EX: &str = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aac",LANGUAGE="en",NAME="English",DEFAULT=YES,AUTOSELECT=YES,URI="main/english-audio.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aac",LANGUAGE="de",NAME="Deutsch",DEFAULT=NO,AUTOSELECT=YES,URI="main/german-audio.m3u8"
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",LANGUAGE="ja",NAME="日本語",URI="subs/ja.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1280000,CODECS="avc1.4d401f,mp4a.40.2",AUDIO="aac",SUBTITLES="subs"
low/video-only.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2560000,AVERAGE-BANDWIDTH=2000000,RESOLUTION=1280x720,FRAME-RATE=29.97,AUDIO="aac",CLOSED-CAPTIONS=NONE
mid/video-only.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=65000,CODECS="mp4a.40.5",AUDIO="aac"
main/english-audio.m3u8
"#;

    #[test]
    fn parse_and_roundtrip() {
        let p = MultivariantPlaylist::parse(EX.as_bytes()).unwrap();
        assert_eq!(p.renditions.len(), 3);
        assert_eq!(p.variants.len(), 3);
        assert_eq!(p.variants[1].frame_rate_milli, Some(29_970));
        assert_eq!(p.variants[1].resolution, Some((1280, 720)));
        assert_eq!(p.variants[1].closed_captions, Some(ClosedCaptions::None));
        assert_eq!(p.renditions[2].name, "日本語");
        let text = p.to_m3u8().unwrap();
        let mut again = MultivariantPlaylist::parse(text.as_bytes()).unwrap();
        again.version = None;
        assert_eq!(again, p);
        assert!(matches!(Playlist::parse(EX.as_bytes()).unwrap(), Playlist::Multivariant(_)));
    }

    #[test]
    fn rejects() {
        for bad in [
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n",
            "#EXTM3U\n#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=1\na.m3u8\n",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,AUDIO=\"x\"\na.m3u8\n",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n#EXT-X-VERSION:3\na.m3u8\n",
            "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\"\n",
            "#EXTM3U\n#EXT-X-MEDIA:TYPE=FOO,GROUP-ID=\"a\",NAME=\"n\"\n",
            "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"n\",DEFAULT=YES,AUTOSELECT=NO\n",
            "#EXTM3U\n#EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,GROUP-ID=\"a\",NAME=\"n\",URI=\"x\"\n",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,FRAME-RATE=29.9999\na.m3u8\n",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\na.m3u8\n#EXTINF:2,\nb.ts\n",
        ] {
            assert!(MultivariantPlaylist::parse(bad.as_bytes()).is_err(), "{bad:?}");
        }
    }
}
