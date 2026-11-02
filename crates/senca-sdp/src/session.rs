//! SDP 本体（RFC 8866）の解析と組み立て。
//!
//! 解析は厳格に行う: 行の種類・順序・重複を文法どおりに検査し、知らない種類の行は拒否する（RFC 8866 5）。
//! 全体の大きさ・行数・media 数・属性数・行の長さに上限を置く（仕様 9.4）。
//! 出力（`Display`）は CRLF 区切りで、解析 → 出力 → 解析で同じ値に戻る。

use crate::attr::{
    Attribute, Candidate, Direction, Extmap, Fingerprint, Fmtp, Group, Msid, Rid, RtcpFb, RtpMap,
    Setup, Simulcast, SsrcAttr, SsrcGroup, is_token, parse_ice_options, parse_num,
};
use crate::error::{Error, Result};
use std::fmt;

/// 全体の大きさの上限（仕様 9.4: SDP 256 KiB）。
pub const MAX_SDP_BYTES: usize = 256 * 1024;
/// 行数の上限。
pub const MAX_LINES: usize = 8192;
/// 1 行の長さの上限（バイト）。
pub const MAX_LINE_LEN: usize = 16 * 1024;
/// media 記述の数の上限。
pub const MAX_MEDIA: usize = 64;
/// 1 つの節（session か media）の属性数の上限。
pub const MAX_ATTRIBUTES: usize = 1024;
/// 1 つの m 行の format 数の上限。
pub const MAX_FORMATS: usize = 128;
/// b・e・p・t・r 行それぞれの数の上限。
const MAX_REPEATED: usize = 32;

/// 解析の設定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseOptions {
    /// `o=`・`s=`・`t=` を必須にする（RFC 8866 どおり）。古いカメラには省くものがあるため、RTSP では外せる。
    pub strict_required_lines: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        ParseOptions {
            strict_required_lines: true,
        }
    }
}

impl ParseOptions {
    /// RTSP カメラ向け（`o=`・`s=`・`t=` の欠落と空の `s=` を許す）。それ以外の検査は同じ。
    pub fn lenient() -> Self {
        ParseOptions {
            strict_required_lines: false,
        }
    }
}

/// `o=<username> <sess-id> <sess-version> <nettype> <addrtype> <unicast-address>`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub username: String,
    pub session_id: u64,
    pub session_version: u64,
    pub net_type: String,
    pub addr_type: String,
    pub address: String,
}

impl Default for Origin {
    fn default() -> Self {
        Origin {
            username: "-".into(),
            session_id: 0,
            session_version: 0,
            net_type: "IN".into(),
            addr_type: "IP4".into(),
            address: "0.0.0.0".into(),
        }
    }
}

impl Origin {
    fn parse(v: &str) -> Result<Origin> {
        let t: Vec<&str> = v.split(' ').collect();
        if t.len() != 6 || t.iter().any(|s| s.is_empty()) {
            return Err(Error::Invalid("o= の項目数"));
        }
        Ok(Origin {
            username: t[0].into(),
            session_id: parse_num(t[1], "o= の sess-id")?,
            session_version: parse_num(t[2], "o= の sess-version")?,
            net_type: token(t[3], "o= の nettype")?,
            addr_type: token(t[4], "o= の addrtype")?,
            address: addr(t[5], "o= のアドレス")?,
        })
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {} {} {} {}",
            self.username,
            self.session_id,
            self.session_version,
            self.net_type,
            self.addr_type,
            self.address
        )
    }
}

fn token(s: &str, what: &'static str) -> Result<String> {
    if is_token(s) {
        Ok(s.to_string())
    } else {
        Err(Error::Invalid(what))
    }
}

/// アドレス（IP・FQDN・`/ttl` 付きの多重送信アドレス）の文字を検査する。
fn addr(s: &str, what: &'static str) -> Result<String> {
    if !s.is_empty()
        && s.len() <= 255
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || ".:-/_%".contains(c))
    {
        Ok(s.to_string())
    } else {
        Err(Error::Invalid(what))
    }
}

/// `c=<nettype> <addrtype> <connection-address>`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub net_type: String,
    pub addr_type: String,
    /// `/ttl`・`/count` を含む元の表記。
    pub address: String,
}

impl Connection {
    pub fn ip4(address: impl Into<String>) -> Self {
        Connection {
            net_type: "IN".into(),
            addr_type: "IP4".into(),
            address: address.into(),
        }
    }

    pub fn parse(v: &str) -> Result<Connection> {
        let t: Vec<&str> = v.split(' ').collect();
        if t.len() != 3 {
            return Err(Error::Invalid("c= の項目数"));
        }
        Ok(Connection {
            net_type: token(t[0], "c= の nettype")?,
            addr_type: token(t[1], "c= の addrtype")?,
            address: addr(t[2], "c= のアドレス")?,
        })
    }

    /// `/ttl` などを除いたアドレス。
    pub fn base_address(&self) -> &str {
        self.address.split('/').next().unwrap_or_default()
    }
}

impl fmt::Display for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {}", self.net_type, self.addr_type, self.address)
    }
}

/// `b=<bwtype>:<bandwidth>`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bandwidth {
    pub bwtype: String,
    pub bandwidth: u64,
}

impl Bandwidth {
    pub fn parse(v: &str) -> Result<Bandwidth> {
        let (t, b) = v.split_once(':').ok_or(Error::Invalid("b="))?;
        Ok(Bandwidth {
            bwtype: token(t, "b= の種類")?,
            bandwidth: parse_num(b, "b= の値")?,
        })
    }
}

impl fmt::Display for Bandwidth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.bwtype, self.bandwidth)
    }
}

/// `t=<start> <stop>` と、それに続く `r=` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timing {
    pub start: u64,
    pub stop: u64,
    /// `r=` 行の値（解釈しない）。
    pub repeats: Vec<String>,
}

impl Timing {
    fn parse(v: &str) -> Result<Timing> {
        let (a, b) = v.split_once(' ').ok_or(Error::Invalid("t="))?;
        Ok(Timing {
            start: parse_num(a, "t= の開始")?,
            stop: parse_num(b, "t= の終了")?,
            repeats: Vec::new(),
        })
    }
}

/// media 記述（`m=` から次の `m=` まで）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDescription {
    /// `audio`・`video`・`application` など。
    pub media: String,
    pub port: u16,
    pub port_count: Option<u16>,
    /// `RTP/AVP`・`UDP/TLS/RTP/SAVPF` など。
    pub protocol: String,
    pub formats: Vec<String>,
    pub info: Option<String>,
    pub connection: Option<Connection>,
    pub bandwidths: Vec<Bandwidth>,
    /// 廃止された `k=` 行（往復のためだけに持つ）。
    pub key: Option<String>,
    pub attributes: Vec<Attribute>,
}

impl MediaDescription {
    pub fn new(media: impl Into<String>, port: u16, protocol: impl Into<String>) -> Self {
        MediaDescription {
            media: media.into(),
            port,
            port_count: None,
            protocol: protocol.into(),
            formats: Vec::new(),
            info: None,
            connection: None,
            bandwidths: Vec::new(),
            key: None,
            attributes: Vec::new(),
        }
    }

    fn parse_m(v: &str) -> Result<MediaDescription> {
        let t: Vec<&str> = v.split(' ').collect();
        if t.len() < 4 {
            return Err(Error::Invalid("m= の項目数"));
        }
        if t.len() - 3 > MAX_FORMATS {
            return Err(Error::Limit("m= の format 数"));
        }
        let (port, port_count) = match t[1].split_once('/') {
            Some((p, n)) => (p, Some(parse_num(n, "m= のポート数")?)),
            None => (t[1], None),
        };
        let proto = t[2];
        if proto.is_empty() || !proto.split('/').all(is_token) {
            return Err(Error::Invalid("m= の proto"));
        }
        let mut m = MediaDescription::new(
            token(t[0], "m= の media")?,
            parse_num(port, "m= のポート")?,
            proto,
        );
        m.port_count = port_count;
        for f in &t[3..] {
            m.formats.push(token(f, "m= の format")?);
        }
        Ok(m)
    }

    /// 最初の同名属性の値（値のない属性は `Some("")`）。
    pub fn attribute(&self, name: &str) -> Option<&str> {
        find_attr(&self.attributes, name)
    }

    /// 同名属性の値をすべて。
    pub fn attributes_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.attributes
            .iter()
            .filter(move |a| a.name == name)
            .map(|a| a.value.as_deref().unwrap_or(""))
    }

    pub fn has_flag(&self, name: &str) -> bool {
        self.attributes.iter().any(|a| a.name == name)
    }

    pub fn push_attr(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.attributes.push(Attribute::new(name, value));
    }

    pub fn push_flag(&mut self, name: impl Into<String>) {
        self.attributes.push(Attribute::flag(name));
    }

    /// 解析できる rtpmap を順に（壊れたものは飛ばす）。rtpmap のない静的 payload type は含まない。
    pub fn rtpmaps(&self) -> Vec<RtpMap> {
        self.attributes_named("rtpmap")
            .filter_map(|v| RtpMap::parse(v).ok())
            .collect()
    }

    /// payload type の rtpmap。なければ静的な割り当て（RFC 3551）。
    pub fn rtpmap(&self, pt: u8) -> Option<RtpMap> {
        self.attributes_named("rtpmap")
            .filter_map(|v| RtpMap::parse(v).ok())
            .find(|r| r.payload_type == pt)
            .or_else(|| RtpMap::static_type(pt))
    }

    pub fn fmtp(&self, pt: u8) -> Option<Fmtp> {
        self.attributes_named("fmtp")
            .filter_map(|v| Fmtp::parse(v).ok())
            .find(|f| f.payload_type == pt)
    }

    /// payload type に当てはまる rtcp-fb（`*` を含む）。
    pub fn rtcp_fbs(&self, pt: u8) -> Vec<RtcpFb> {
        self.attributes_named("rtcp-fb")
            .filter_map(|v| RtcpFb::parse(v).ok())
            .filter(|f| f.payload_type.is_none_or(|p| p == pt))
            .collect()
    }

    pub fn extmaps(&self) -> Vec<Extmap> {
        self.attributes_named("extmap")
            .filter_map(|v| Extmap::parse(v).ok())
            .collect()
    }

    pub fn mid(&self) -> Option<&str> {
        self.attribute("mid").filter(|m| !m.is_empty())
    }

    /// `a=control:`（RTSP）。
    pub fn control(&self) -> Option<&str> {
        self.attribute("control")
    }

    /// media 単位の方向属性（なければ `None`。既定値を含めた値は [`SessionDescription::effective_direction`]）。
    pub fn direction(&self) -> Option<Direction> {
        self.attributes.iter().rev().find_map(|a| {
            a.value
                .is_none()
                .then(|| Direction::parse(&a.name))
                .flatten()
        })
    }

    pub fn msid(&self) -> Option<Msid> {
        self.attribute("msid").and_then(|v| Msid::parse(v).ok())
    }

    pub fn ssrcs(&self) -> Vec<SsrcAttr> {
        self.attributes_named("ssrc")
            .filter_map(|v| SsrcAttr::parse(v).ok())
            .collect()
    }

    pub fn ssrc_groups(&self) -> Vec<SsrcGroup> {
        self.attributes_named("ssrc-group")
            .filter_map(|v| SsrcGroup::parse(v).ok())
            .collect()
    }

    pub fn rids(&self) -> Vec<Rid> {
        self.attributes_named("rid")
            .filter_map(|v| Rid::parse(v).ok())
            .collect()
    }

    pub fn simulcast(&self) -> Option<Simulcast> {
        self.attribute("simulcast")
            .and_then(|v| Simulcast::parse(v).ok())
    }

    pub fn candidates(&self) -> Vec<Candidate> {
        self.attributes_named("candidate")
            .filter_map(|v| Candidate::parse(v).ok())
            .collect()
    }

    pub fn sctp_port(&self) -> Option<u16> {
        self.attribute("sctp-port")
            .and_then(|v| parse_num(v, "sctp-port").ok())
    }

    pub fn max_message_size(&self) -> Option<u64> {
        self.attribute("max-message-size")
            .and_then(|v| parse_num(v, "max-message-size").ok())
    }

    /// m 行の format を payload type として（数値でないものは飛ばす）。
    pub fn payload_types(&self) -> Vec<u8> {
        self.formats
            .iter()
            .filter_map(|f| parse_num::<u8>(f, "pt").ok().filter(|p| *p <= 127))
            .collect()
    }

    /// port 0（拒否された、または無効の m 行）。
    pub fn is_rejected(&self) -> bool {
        self.port == 0 && !self.has_flag("bundle-only")
    }
}

impl fmt::Display for MediaDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "m={} {}", self.media, self.port)?;
        if let Some(n) = self.port_count {
            write!(f, "/{n}")?;
        }
        write!(f, " {}", self.protocol)?;
        for fmt in &self.formats {
            write!(f, " {fmt}")?;
        }
        f.write_str("\r\n")?;
        if let Some(i) = &self.info {
            write!(f, "i={i}\r\n")?;
        }
        if let Some(c) = &self.connection {
            write!(f, "c={c}\r\n")?;
        }
        for b in &self.bandwidths {
            write!(f, "b={b}\r\n")?;
        }
        if let Some(k) = &self.key {
            write!(f, "k={k}\r\n")?;
        }
        for a in &self.attributes {
            write!(f, "a={a}\r\n")?;
        }
        Ok(())
    }
}

fn find_attr<'a>(attrs: &'a [Attribute], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|a| a.name == name)
        .map(|a| a.value.as_deref().unwrap_or(""))
}

/// セッション記述全体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDescription {
    /// `v=`（常に 0）。
    pub version: u32,
    pub origin: Origin,
    pub session_name: String,
    pub info: Option<String>,
    pub uri: Option<String>,
    pub emails: Vec<String>,
    pub phones: Vec<String>,
    pub connection: Option<Connection>,
    pub bandwidths: Vec<Bandwidth>,
    pub times: Vec<Timing>,
    /// `z=` 行（解釈しない）。
    pub zone: Option<String>,
    /// 廃止された `k=` 行（往復のためだけに持つ）。
    pub key: Option<String>,
    pub attributes: Vec<Attribute>,
    pub media: Vec<MediaDescription>,
}

impl Default for SessionDescription {
    fn default() -> Self {
        SessionDescription {
            version: 0,
            origin: Origin::default(),
            session_name: "-".into(),
            info: None,
            uri: None,
            emails: Vec::new(),
            phones: Vec::new(),
            connection: None,
            bandwidths: Vec::new(),
            times: vec![Timing {
                start: 0,
                stop: 0,
                repeats: Vec::new(),
            }],
            zone: None,
            key: None,
            attributes: Vec::new(),
            media: Vec::new(),
        }
    }
}

/// session 節の行の順位（RFC 8866 5 の並び）。
fn session_rank(c: u8) -> Option<u8> {
    Some(match c {
        b'v' => 0,
        b'o' => 1,
        b's' => 2,
        b'i' => 3,
        b'u' => 4,
        b'e' => 5,
        b'p' => 6,
        b'c' => 7,
        b'b' => 8,
        b't' => 9,
        b'r' => 10,
        b'z' => 11,
        b'k' => 12,
        b'a' => 13,
        _ => return None,
    })
}

fn media_rank(c: u8) -> Option<u8> {
    Some(match c {
        b'i' => 0,
        b'c' => 1,
        b'b' => 2,
        b'k' => 3,
        b'a' => 4,
        _ => return None,
    })
}

fn parse_attribute(v: &str) -> Result<Attribute> {
    let (name, value) = match v.split_once(':') {
        Some((n, val)) => (n, Some(val.to_string())),
        None => (v, None),
    };
    if !is_token(name) {
        return Err(Error::Invalid("属性名"));
    }
    Ok(Attribute {
        name: name.to_string(),
        value,
    })
}

impl SessionDescription {
    /// 厳格に解析する（[`ParseOptions::default`]）。
    pub fn parse(s: &str) -> Result<SessionDescription> {
        Self::parse_with(s, &ParseOptions::default())
    }

    /// バイト列を解析する（UTF-8 でなければ `Err`）。
    pub fn parse_bytes(b: &[u8]) -> Result<SessionDescription> {
        Self::parse_bytes_with(b, &ParseOptions::default())
    }

    pub fn parse_bytes_with(b: &[u8], opts: &ParseOptions) -> Result<SessionDescription> {
        if b.len() > MAX_SDP_BYTES {
            return Err(Error::TooLarge {
                limit: MAX_SDP_BYTES,
            });
        }
        let s = std::str::from_utf8(b).map_err(|_| Error::NotUtf8)?;
        Self::parse_with(s, opts)
    }

    pub fn parse_with(s: &str, opts: &ParseOptions) -> Result<SessionDescription> {
        if s.len() > MAX_SDP_BYTES {
            return Err(Error::TooLarge {
                limit: MAX_SDP_BYTES,
            });
        }
        let mut sd = SessionDescription {
            times: Vec::new(),
            ..Default::default()
        };
        let mut seen_v = false;
        let mut seen_o = false;
        let mut seen_s = false;
        // session 節で最後に読んだ行の順位
        let mut last_rank: Option<u8> = None;
        let mut last_media_rank: Option<u8> = None;
        // 末尾の空行は許す（最後の改行のあとなど）。途中の空行は拒否する。
        let body = s.trim_end_matches(['\r', '\n']);
        if body.is_empty() {
            return Err(Error::Missing("v="));
        }
        for (idx, raw) in body.split('\n').enumerate() {
            let line_no = idx + 1;
            if line_no > MAX_LINES {
                return Err(Error::Limit("行数"));
            }
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            if line.len() > MAX_LINE_LEN {
                return Err(Error::Limit("行の長さ"));
            }
            let syn = |what| Error::Syntax {
                line: line_no,
                what,
            };
            let ord = |what| Error::Order {
                line: line_no,
                what,
            };
            if line.chars().any(|c| c == '\0' || c == '\r') {
                return Err(syn("NUL・CR を含む"));
            }
            let b = line.as_bytes();
            if b.len() < 2 || b[1] != b'=' || !b[0].is_ascii_lowercase() {
                return Err(syn("<type>=<value> の形ではない"));
            }
            let ty = b[0];
            let v = &line[2..];
            let with_line = |e: Error| match e {
                Error::Invalid(w) => syn(w),
                other => other,
            };

            if idx == 0 {
                if ty != b'v' {
                    return Err(ord("最初の行が v= ではない"));
                }
                if v != "0" {
                    return Err(syn("v= が 0 ではない"));
                }
                seen_v = true;
                last_rank = Some(0);
                continue;
            }

            if ty == b'm' {
                if sd.media.len() >= MAX_MEDIA {
                    return Err(Error::Limit("media の数"));
                }
                if sd.media.is_empty() {
                    Self::check_session_required(&sd, seen_o, seen_s, opts, line_no)?;
                }
                sd.media
                    .push(MediaDescription::parse_m(v).map_err(with_line)?);
                last_media_rank = None;
                continue;
            }

            if let Some(m) = sd.media.last_mut() {
                // media 節
                let rank = media_rank(ty).ok_or_else(|| ord("media 節で使えない行"))?;
                if last_media_rank.is_some_and(|l| rank < l) {
                    return Err(ord("media 節の行の順序"));
                }
                let repeated = last_media_rank == Some(rank);
                last_media_rank = Some(rank);
                match ty {
                    b'i' if !repeated => m.info = Some(v.to_string()),
                    b'c' if !repeated => {
                        m.connection = Some(Connection::parse(v).map_err(with_line)?)
                    }
                    b'b' => {
                        if m.bandwidths.len() >= MAX_REPEATED {
                            return Err(Error::Limit("b= の数"));
                        }
                        m.bandwidths.push(Bandwidth::parse(v).map_err(with_line)?);
                    }
                    b'k' if !repeated => m.key = Some(v.to_string()),
                    b'a' => {
                        if m.attributes.len() >= MAX_ATTRIBUTES {
                            return Err(Error::Limit("属性の数"));
                        }
                        m.attributes.push(parse_attribute(v).map_err(with_line)?);
                    }
                    _ => return Err(ord("media 節で重複できない行")),
                }
                continue;
            }

            // session 節
            let rank = session_rank(ty).ok_or_else(|| syn("知らない種類の行"))?;
            let last = last_rank.unwrap_or(0);
            // r= のあとに t= が来るのは許す（t r* t r*）
            let back_to_t = ty == b't' && (last == 9 || last == 10);
            if rank < last && !back_to_t {
                return Err(ord("session 節の行の順序"));
            }
            if ty == b'r' && !(last == 9 || last == 10) {
                return Err(ord("r= が t= の後ろにない"));
            }
            let repeated = last == rank;
            last_rank = Some(rank);
            match ty {
                b'v' => return Err(ord("v= が重複")),
                b'o' if !repeated => {
                    sd.origin = Origin::parse(v).map_err(with_line)?;
                    seen_o = true;
                }
                b's' if !repeated => {
                    if v.is_empty() && opts.strict_required_lines {
                        return Err(syn("s= が空"));
                    }
                    sd.session_name = v.to_string();
                    seen_s = true;
                }
                b'i' if !repeated => sd.info = Some(v.to_string()),
                b'u' if !repeated => sd.uri = Some(v.to_string()),
                b'e' => {
                    if sd.emails.len() >= MAX_REPEATED {
                        return Err(Error::Limit("e= の数"));
                    }
                    sd.emails.push(v.to_string())
                }
                b'p' => {
                    if sd.phones.len() >= MAX_REPEATED {
                        return Err(Error::Limit("p= の数"));
                    }
                    sd.phones.push(v.to_string())
                }
                b'c' if !repeated => sd.connection = Some(Connection::parse(v).map_err(with_line)?),
                b'b' => {
                    if sd.bandwidths.len() >= MAX_REPEATED {
                        return Err(Error::Limit("b= の数"));
                    }
                    sd.bandwidths.push(Bandwidth::parse(v).map_err(with_line)?)
                }
                b't' => {
                    if sd.times.len() >= MAX_REPEATED {
                        return Err(Error::Limit("t= の数"));
                    }
                    sd.times.push(Timing::parse(v).map_err(with_line)?)
                }
                b'r' => {
                    let t = sd
                        .times
                        .last_mut()
                        .ok_or_else(|| ord("r= の前に t= がない"))?;
                    if t.repeats.len() >= MAX_REPEATED {
                        return Err(Error::Limit("r= の数"));
                    }
                    t.repeats.push(v.to_string())
                }
                b'z' if !repeated => sd.zone = Some(v.to_string()),
                b'k' if !repeated => sd.key = Some(v.to_string()),
                b'a' => {
                    if sd.attributes.len() >= MAX_ATTRIBUTES {
                        return Err(Error::Limit("属性の数"));
                    }
                    sd.attributes.push(parse_attribute(v).map_err(with_line)?)
                }
                _ => return Err(ord("session 節で重複できない行")),
            }
        }
        if !seen_v {
            return Err(Error::Missing("v="));
        }
        if sd.media.is_empty() {
            Self::check_session_required(&sd, seen_o, seen_s, opts, 0)?;
        }
        if sd.times.is_empty() && !opts.strict_required_lines {
            sd.times.push(Timing {
                start: 0,
                stop: 0,
                repeats: Vec::new(),
            });
        }
        Ok(sd)
    }

    fn check_session_required(
        sd: &SessionDescription,
        seen_o: bool,
        seen_s: bool,
        opts: &ParseOptions,
        _line: usize,
    ) -> Result<()> {
        if !opts.strict_required_lines {
            return Ok(());
        }
        if !seen_o {
            return Err(Error::Missing("o="));
        }
        if !seen_s {
            return Err(Error::Missing("s="));
        }
        if sd.times.is_empty() {
            return Err(Error::Missing("t="));
        }
        Ok(())
    }

    /// session 単位の最初の同名属性の値（値のない属性は `Some("")`）。
    pub fn attribute(&self, name: &str) -> Option<&str> {
        find_attr(&self.attributes, name)
    }

    pub fn attributes_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.attributes
            .iter()
            .filter(move |a| a.name == name)
            .map(|a| a.value.as_deref().unwrap_or(""))
    }

    pub fn has_flag(&self, name: &str) -> bool {
        self.attributes.iter().any(|a| a.name == name)
    }

    pub fn push_attr(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.attributes.push(Attribute::new(name, value));
    }

    pub fn push_flag(&mut self, name: impl Into<String>) {
        self.attributes.push(Attribute::flag(name));
    }

    /// `a=group:` をすべて。
    pub fn groups(&self) -> Vec<Group> {
        self.attributes_named("group")
            .filter_map(|v| Group::parse(v).ok())
            .collect()
    }

    /// `a=group:BUNDLE` をすべて（mid の並び）。
    pub fn bundle_groups(&self) -> Vec<Vec<String>> {
        self.groups()
            .into_iter()
            .filter(|g| g.semantics == "BUNDLE")
            .map(|g| g.mids)
            .collect()
    }

    /// mid で media を探す。
    pub fn media_by_mid(&self, mid: &str) -> Option<(usize, &MediaDescription)> {
        self.media
            .iter()
            .enumerate()
            .find(|(_, m)| m.mid() == Some(mid))
    }

    /// media 単位を優先し、なければ session 単位の属性値。
    pub fn effective_attribute(&self, media_idx: usize, name: &str) -> Option<&str> {
        self.media
            .get(media_idx)
            .and_then(|m| m.attribute(name))
            .or_else(|| self.attribute(name))
    }

    /// 方向（media 単位 → session 単位 → 既定の sendrecv）。
    pub fn effective_direction(&self, media_idx: usize) -> Direction {
        self.media
            .get(media_idx)
            .and_then(|m| m.direction())
            .or_else(|| {
                self.attributes.iter().rev().find_map(|a| {
                    a.value
                        .is_none()
                        .then(|| Direction::parse(&a.name))
                        .flatten()
                })
            })
            .unwrap_or(Direction::SendRecv)
    }

    /// 接続先（media 単位 → session 単位）。
    pub fn effective_connection(&self, media_idx: usize) -> Option<&Connection> {
        self.media
            .get(media_idx)
            .and_then(|m| m.connection.as_ref())
            .or(self.connection.as_ref())
    }

    pub fn ice_ufrag(&self, media_idx: usize) -> Option<&str> {
        self.effective_attribute(media_idx, "ice-ufrag")
    }

    pub fn ice_pwd(&self, media_idx: usize) -> Option<&str> {
        self.effective_attribute(media_idx, "ice-pwd")
    }

    pub fn ice_options(&self, media_idx: usize) -> Vec<String> {
        self.effective_attribute(media_idx, "ice-options")
            .and_then(|v| parse_ice_options(v).ok())
            .unwrap_or_default()
    }

    pub fn fingerprint(&self, media_idx: usize) -> Option<Fingerprint> {
        self.effective_attribute(media_idx, "fingerprint")
            .and_then(|v| Fingerprint::parse(v).ok())
    }

    pub fn setup(&self, media_idx: usize) -> Option<Setup> {
        self.effective_attribute(media_idx, "setup")
            .and_then(Setup::parse)
    }

    /// `a=control:`（media 単位）。RTSP の集約 URL は session 単位の [`Self::attribute`] で取る。
    pub fn control(&self, media_idx: usize) -> Option<&str> {
        self.media.get(media_idx).and_then(|m| m.control())
    }
}

impl fmt::Display for SessionDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v={}\r\n", self.version)?;
        write!(f, "o={}\r\n", self.origin)?;
        write!(f, "s={}\r\n", self.session_name)?;
        if let Some(i) = &self.info {
            write!(f, "i={i}\r\n")?;
        }
        if let Some(u) = &self.uri {
            write!(f, "u={u}\r\n")?;
        }
        for e in &self.emails {
            write!(f, "e={e}\r\n")?;
        }
        for p in &self.phones {
            write!(f, "p={p}\r\n")?;
        }
        if let Some(c) = &self.connection {
            write!(f, "c={c}\r\n")?;
        }
        for b in &self.bandwidths {
            write!(f, "b={b}\r\n")?;
        }
        for t in &self.times {
            write!(f, "t={} {}\r\n", t.start, t.stop)?;
            for r in &t.repeats {
                write!(f, "r={r}\r\n")?;
            }
        }
        if let Some(z) = &self.zone {
            write!(f, "z={z}\r\n")?;
        }
        if let Some(k) = &self.key {
            write!(f, "k={k}\r\n")?;
        }
        for a in &self.attributes {
            write!(f, "a={a}\r\n")?;
        }
        for m in &self.media {
            write!(f, "{m}")?;
        }
        Ok(())
    }
}
