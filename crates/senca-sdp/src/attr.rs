//! 型付きの属性（rtpmap・fmtp・rtcp-fb・extmap・msid・ssrc・rid・simulcast・group・candidate・fingerprint ほか）。
//!
//! 各型は `parse`（属性値、つまり `a=name:` の後ろ）と `Display`（同じ形の値）を持つ。

use crate::error::{Error, Result};
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

/// 属性 1 つ（`a=name` または `a=name:value`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub name: String,
    pub value: Option<String>,
}

impl Attribute {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Attribute {
            name: name.into(),
            value: Some(value.into()),
        }
    }

    /// 値のない属性（`a=rtcp-mux` など）。
    pub fn flag(name: impl Into<String>) -> Self {
        Attribute {
            name: name.into(),
            value: None,
        }
    }
}

impl fmt::Display for Attribute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.value {
            Some(v) => write!(f, "{}:{}", self.name, v),
            None => f.write_str(&self.name),
        }
    }
}

/// RFC 8866 の token 文字。
pub(crate) fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`{|}~".contains(c)
}

pub(crate) fn is_token(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_token_char)
}

fn parse_pt(s: &str) -> Result<u8> {
    let v: u8 = parse_num(s, "payload type")?;
    if v > 127 {
        return Err(Error::Invalid("payload type が 127 を超える"));
    }
    Ok(v)
}

/// 先頭の 0 を含めた 10 進数だけを受け付ける（符号・空白は拒否）。
pub(crate) fn parse_num<T: std::str::FromStr>(s: &str, what: &'static str) -> Result<T> {
    if s.is_empty() || s.len() > 20 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Invalid(what));
    }
    s.parse().map_err(|_| Error::Invalid(what))
}

/// 方向属性（RFC 8866 6.7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

impl Direction {
    pub fn parse(s: &str) -> Option<Direction> {
        Some(match s {
            "sendrecv" => Direction::SendRecv,
            "sendonly" => Direction::SendOnly,
            "recvonly" => Direction::RecvOnly,
            "inactive" => Direction::Inactive,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Direction::SendRecv => "sendrecv",
            Direction::SendOnly => "sendonly",
            Direction::RecvOnly => "recvonly",
            Direction::Inactive => "inactive",
        }
    }

    /// 相手から見た方向（sendonly ↔ recvonly）。
    pub fn reverse(self) -> Direction {
        match self {
            Direction::SendOnly => Direction::RecvOnly,
            Direction::RecvOnly => Direction::SendOnly,
            d => d,
        }
    }

    pub fn sends(self) -> bool {
        matches!(self, Direction::SendRecv | Direction::SendOnly)
    }

    pub fn recvs(self) -> bool {
        matches!(self, Direction::SendRecv | Direction::RecvOnly)
    }

    pub fn from_flags(send: bool, recv: bool) -> Direction {
        match (send, recv) {
            (true, true) => Direction::SendRecv,
            (true, false) => Direction::SendOnly,
            (false, true) => Direction::RecvOnly,
            (false, false) => Direction::Inactive,
        }
    }

    /// 両方が許す方向だけを残す。
    pub fn intersect(self, other: Direction) -> Direction {
        Direction::from_flags(self.sends() && other.sends(), self.recvs() && other.recvs())
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `a=rtpmap:<pt> <encoding>/<clock>[/<channels>]`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpMap {
    pub payload_type: u8,
    pub encoding: String,
    pub clock_rate: u32,
    pub channels: Option<u16>,
}

impl RtpMap {
    pub fn parse(v: &str) -> Result<RtpMap> {
        let (pt, rest) = v.trim().split_once(' ').ok_or(Error::Invalid("rtpmap"))?;
        let mut it = rest.trim().split('/');
        let encoding = it.next().unwrap_or_default();
        if !is_token(encoding) {
            return Err(Error::Invalid("rtpmap の符号化名"));
        }
        let clock_rate: u32 = parse_num(
            it.next().ok_or(Error::Invalid("rtpmap のクロック"))?,
            "rtpmap のクロック",
        )?;
        if clock_rate == 0 {
            return Err(Error::Invalid("rtpmap のクロックが 0"));
        }
        let channels = match it.next() {
            Some(c) => {
                let c: u16 = parse_num(c, "rtpmap のチャンネル数")?;
                if c == 0 {
                    return Err(Error::Invalid("rtpmap のチャンネル数が 0"));
                }
                Some(c)
            }
            None => None,
        };
        if it.next().is_some() {
            return Err(Error::Invalid("rtpmap の余分な項目"));
        }
        Ok(RtpMap {
            payload_type: parse_pt(pt)?,
            encoding: encoding.to_string(),
            clock_rate,
            channels,
        })
    }

    /// 静的な payload type（RFC 3551）。rtpmap がないときに使う。
    pub fn static_type(pt: u8) -> Option<RtpMap> {
        let (enc, clock, ch) = match pt {
            0 => ("PCMU", 8000, 1),
            3 => ("GSM", 8000, 1),
            4 => ("G723", 8000, 1),
            8 => ("PCMA", 8000, 1),
            9 => ("G722", 8000, 1),
            14 => ("MPA", 90000, 0),
            26 => ("JPEG", 90000, 0),
            32 => ("MPV", 90000, 0),
            33 => ("MP2T", 90000, 0),
            _ => return None,
        };
        Some(RtpMap {
            payload_type: pt,
            encoding: enc.to_string(),
            clock_rate: clock,
            channels: (ch > 0).then_some(ch),
        })
    }
}

impl fmt::Display for RtpMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}/{}",
            self.payload_type, self.encoding, self.clock_rate
        )?;
        if let Some(c) = self.channels {
            write!(f, "/{c}")?;
        }
        Ok(())
    }
}

/// `a=fmtp:<pt> <params>`。params は `;` 区切りの `key=value`（`=` のない項目は値を空にする）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fmtp {
    pub payload_type: u8,
    pub params: Vec<(String, String)>,
}

/// fmtp の項目数の上限。
const MAX_FMTP_PARAMS: usize = 64;

impl Fmtp {
    pub fn parse(v: &str) -> Result<Fmtp> {
        let v = v.trim();
        let (pt, rest) = match v.split_once(' ') {
            Some(x) => x,
            None => (v, ""),
        };
        let pt = parse_pt(pt)?;
        Ok(Fmtp {
            payload_type: pt,
            params: Self::parse_params(rest)?,
        })
    }

    /// `key=value;key=value` を分割する。
    pub fn parse_params(s: &str) -> Result<Vec<(String, String)>> {
        let mut params = Vec::new();
        for item in s.split(';') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            if params.len() >= MAX_FMTP_PARAMS {
                return Err(Error::Limit("fmtp の項目数"));
            }
            match item.split_once('=') {
                Some((k, val)) => params.push((k.trim().to_string(), val.trim().to_string())),
                None => params.push((item.to_string(), String::new())),
            }
        }
        Ok(params)
    }

    /// 項目の値（名前の大文字小文字は区別しない）。
    pub fn get(&self, key: &str) -> Option<&str> {
        get_param(&self.params, key)
    }

    /// `;` 区切りの項目部分（payload type を除く）。
    pub fn params_string(&self) -> String {
        params_to_string(&self.params)
    }
}

pub(crate) fn get_param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

pub(crate) fn params_to_string(params: &[(String, String)]) -> String {
    let mut s = String::new();
    for (i, (k, v)) in params.iter().enumerate() {
        if i > 0 {
            s.push(';');
        }
        s.push_str(k);
        if !v.is_empty() {
            s.push('=');
            s.push_str(v);
        }
    }
    s
}

impl fmt::Display for Fmtp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.payload_type)?;
        if !self.params.is_empty() {
            write!(f, " {}", self.params_string())?;
        }
        Ok(())
    }
}

/// `a=rtcp-fb:<pt|*> <type> [<param>...]`（RFC 4585）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtcpFb {
    /// `None` は `*`（すべての payload type）。
    pub payload_type: Option<u8>,
    /// `nack`・`ccm`・`goog-remb`・`transport-cc` など。
    pub kind: String,
    /// `pli`・`fir` など（なければ `None`）。
    pub param: Option<String>,
}

impl RtcpFb {
    pub fn parse(v: &str) -> Result<RtcpFb> {
        let v = v.trim();
        let (pt, rest) = v.split_once(' ').ok_or(Error::Invalid("rtcp-fb"))?;
        let payload_type = if pt == "*" { None } else { Some(parse_pt(pt)?) };
        let rest = rest.trim();
        let (kind, param) = match rest.split_once(' ') {
            Some((k, p)) => (k, Some(p.split_whitespace().collect::<Vec<_>>().join(" "))),
            None => (rest, None),
        };
        if !is_token(kind) {
            return Err(Error::Invalid("rtcp-fb の種類"));
        }
        Ok(RtcpFb {
            payload_type,
            kind: kind.to_string(),
            param: param.filter(|p| !p.is_empty()),
        })
    }

    /// `nack pli` のような種類と引数の文字列。
    pub fn feedback(&self) -> String {
        match &self.param {
            Some(p) => format!("{} {}", self.kind, p),
            None => self.kind.clone(),
        }
    }
}

impl fmt::Display for RtcpFb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.payload_type {
            Some(pt) => write!(f, "{pt} {}", self.feedback()),
            None => write!(f, "* {}", self.feedback()),
        }
    }
}

/// `a=extmap:<id>[/<direction>] <uri> [<attributes>]`（RFC 8285）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extmap {
    /// 1〜14（1 バイト形式）または 1〜255（2 バイト形式）。15 は予約。
    pub id: u8,
    pub direction: Option<Direction>,
    pub uri: String,
    pub attributes: Option<String>,
}

impl Extmap {
    pub fn parse(v: &str) -> Result<Extmap> {
        let mut it = v.split_whitespace();
        let idd = it.next().ok_or(Error::Invalid("extmap"))?;
        let (id, direction) = match idd.split_once('/') {
            Some((i, d)) => (
                i,
                Some(Direction::parse(d).ok_or(Error::Invalid("extmap の方向"))?),
            ),
            None => (idd, None),
        };
        let id: u16 = parse_num(id, "extmap の id")?;
        if id == 0 || id == 15 || id > 255 {
            return Err(Error::Invalid("extmap の id の範囲"));
        }
        let uri = it.next().ok_or(Error::Invalid("extmap の URI"))?;
        if uri.chars().any(|c| c.is_control()) {
            return Err(Error::Invalid("extmap の URI"));
        }
        let rest: Vec<&str> = it.collect();
        Ok(Extmap {
            id: id as u8,
            direction,
            uri: uri.to_string(),
            attributes: (!rest.is_empty()).then(|| rest.join(" ")),
        })
    }
}

impl fmt::Display for Extmap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.id)?;
        if let Some(d) = self.direction {
            write!(f, "/{d}")?;
        }
        write!(f, " {}", self.uri)?;
        if let Some(a) = &self.attributes {
            write!(f, " {a}")?;
        }
        Ok(())
    }
}

/// `a=msid:<stream-id> [<track-id>]`（RFC 8830）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msid {
    pub stream_id: String,
    pub track_id: Option<String>,
}

impl Msid {
    pub fn parse(v: &str) -> Result<Msid> {
        let mut it = v.split_whitespace();
        let stream_id = it.next().ok_or(Error::Invalid("msid"))?;
        let track_id = it.next();
        if it.next().is_some() || stream_id.len() > 64 || track_id.is_some_and(|t| t.len() > 64) {
            return Err(Error::Invalid("msid"));
        }
        let ok = |s: &str| is_token(s);
        if !ok(stream_id) || !track_id.is_none_or(ok) {
            return Err(Error::Invalid("msid の文字"));
        }
        Ok(Msid {
            stream_id: stream_id.to_string(),
            track_id: track_id.map(str::to_string),
        })
    }
}

impl fmt::Display for Msid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.stream_id)?;
        if let Some(t) = &self.track_id {
            write!(f, " {t}")?;
        }
        Ok(())
    }
}

/// `a=ssrc:<ssrc> <attribute>[:<value>]`（RFC 5576）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsrcAttr {
    pub ssrc: u32,
    pub attribute: String,
    pub value: Option<String>,
}

impl SsrcAttr {
    pub fn parse(v: &str) -> Result<SsrcAttr> {
        let (ssrc, rest) = v.trim().split_once(' ').ok_or(Error::Invalid("ssrc"))?;
        let ssrc: u32 = parse_num(ssrc, "ssrc")?;
        let rest = rest.trim();
        let (attribute, value) = match rest.split_once(':') {
            Some((a, v)) => (a, Some(v.to_string())),
            None => (rest, None),
        };
        if !is_token(attribute) {
            return Err(Error::Invalid("ssrc の属性名"));
        }
        Ok(SsrcAttr {
            ssrc,
            attribute: attribute.to_string(),
            value,
        })
    }
}

impl fmt::Display for SsrcAttr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.ssrc, self.attribute)?;
        if let Some(v) = &self.value {
            write!(f, ":{v}")?;
        }
        Ok(())
    }
}

/// `a=ssrc-group:<semantics> <ssrc>...`（RFC 5576）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsrcGroup {
    pub semantics: String,
    pub ssrcs: Vec<u32>,
}

impl SsrcGroup {
    pub fn parse(v: &str) -> Result<SsrcGroup> {
        let mut it = v.split_whitespace();
        let semantics = it.next().ok_or(Error::Invalid("ssrc-group"))?;
        if !is_token(semantics) {
            return Err(Error::Invalid("ssrc-group の種類"));
        }
        let mut ssrcs = Vec::new();
        for s in it {
            if ssrcs.len() >= 32 {
                return Err(Error::Limit("ssrc-group の SSRC 数"));
            }
            ssrcs.push(parse_num(s, "ssrc-group の SSRC")?);
        }
        if ssrcs.is_empty() {
            return Err(Error::Invalid("ssrc-group が空"));
        }
        Ok(SsrcGroup {
            semantics: semantics.to_string(),
            ssrcs,
        })
    }
}

impl fmt::Display for SsrcGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.semantics)?;
        for s in &self.ssrcs {
            write!(f, " {s}")?;
        }
        Ok(())
    }
}

/// rid の方向（RFC 8851）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RidDirection {
    Send,
    Recv,
}

impl RidDirection {
    pub fn reverse(self) -> RidDirection {
        match self {
            RidDirection::Send => RidDirection::Recv,
            RidDirection::Recv => RidDirection::Send,
        }
    }
}

/// `a=rid:<id> <send|recv> [pt=<fmt>,...;<restriction>=<value>;...]`（RFC 8851）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rid {
    pub id: String,
    pub direction: RidDirection,
    pub payload_types: Vec<u8>,
    /// `max-width` などの制限（`pt` は含まない）。
    pub restrictions: Vec<(String, String)>,
}

fn is_rid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl Rid {
    pub fn parse(v: &str) -> Result<Rid> {
        let mut it = v.split_whitespace();
        let id = it.next().ok_or(Error::Invalid("rid"))?;
        if !is_rid_id(id) {
            return Err(Error::Invalid("rid の id"));
        }
        let direction = match it.next() {
            Some("send") => RidDirection::Send,
            Some("recv") => RidDirection::Recv,
            _ => return Err(Error::Invalid("rid の方向")),
        };
        let rest: Vec<&str> = it.collect();
        if rest.len() > 1 {
            return Err(Error::Invalid("rid の余分な項目"));
        }
        let mut payload_types = Vec::new();
        let mut restrictions = Vec::new();
        if let Some(r) = rest.first() {
            for (k, val) in Fmtp::parse_params(r)? {
                if k == "pt" {
                    for p in val.split(',') {
                        if payload_types.len() >= 128 {
                            return Err(Error::Limit("rid の pt 数"));
                        }
                        payload_types.push(parse_pt(p)?);
                    }
                } else {
                    if !is_token(&k) {
                        return Err(Error::Invalid("rid の制限名"));
                    }
                    restrictions.push((k, val));
                }
            }
        }
        Ok(Rid {
            id: id.to_string(),
            direction,
            payload_types,
            restrictions,
        })
    }
}

impl fmt::Display for Rid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}",
            self.id,
            match self.direction {
                RidDirection::Send => "send",
                RidDirection::Recv => "recv",
            }
        )?;
        let mut params = Vec::new();
        if !self.payload_types.is_empty() {
            let pts: Vec<String> = self.payload_types.iter().map(|p| p.to_string()).collect();
            params.push(("pt".to_string(), pts.join(",")));
        }
        params.extend(self.restrictions.iter().cloned());
        if !params.is_empty() {
            write!(f, " {}", params_to_string(&params))?;
        }
        Ok(())
    }
}

/// simulcast の 1 つの rid（`~` は一時停止）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulcastRid {
    pub rid: String,
    pub paused: bool,
}

/// `a=simulcast:[send <list>] [recv <list>]`（RFC 8853）。
///
/// list は `;` で区切ったストリームの並びで、各ストリームは `,` で区切った代替の rid。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Simulcast {
    pub send: Vec<Vec<SimulcastRid>>,
    pub recv: Vec<Vec<SimulcastRid>>,
}

impl Simulcast {
    pub fn parse(v: &str) -> Result<Simulcast> {
        let toks: Vec<&str> = v.split_whitespace().collect();
        if toks.is_empty() || !toks.len().is_multiple_of(2) || toks.len() > 4 {
            return Err(Error::Invalid("simulcast"));
        }
        let mut sc = Simulcast::default();
        let mut seen_send = false;
        let mut seen_recv = false;
        for pair in toks.chunks(2) {
            let list = Self::parse_list(pair[1])?;
            match pair[0] {
                "send" if !seen_send => {
                    seen_send = true;
                    sc.send = list;
                }
                "recv" if !seen_recv => {
                    seen_recv = true;
                    sc.recv = list;
                }
                _ => return Err(Error::Invalid("simulcast の方向")),
            }
        }
        Ok(sc)
    }

    fn parse_list(s: &str) -> Result<Vec<Vec<SimulcastRid>>> {
        let mut out = Vec::new();
        for stream in s.split(';') {
            if out.len() >= 16 {
                return Err(Error::Limit("simulcast のストリーム数"));
            }
            let mut alts = Vec::new();
            for alt in stream.split(',') {
                if alts.len() >= 16 {
                    return Err(Error::Limit("simulcast の代替数"));
                }
                let (paused, rid) = match alt.strip_prefix('~') {
                    Some(r) => (true, r),
                    None => (false, alt),
                };
                if !is_rid_id(rid) {
                    return Err(Error::Invalid("simulcast の rid"));
                }
                alts.push(SimulcastRid {
                    rid: rid.to_string(),
                    paused,
                });
            }
            out.push(alts);
        }
        Ok(out)
    }

    /// 送受信を入れ替える（answer で使う）。
    pub fn reverse(&self) -> Simulcast {
        Simulcast {
            send: self.recv.clone(),
            recv: self.send.clone(),
        }
    }

    fn write_list(f: &mut fmt::Formatter<'_>, list: &[Vec<SimulcastRid>]) -> fmt::Result {
        for (i, stream) in list.iter().enumerate() {
            if i > 0 {
                f.write_str(";")?;
            }
            for (j, r) in stream.iter().enumerate() {
                if j > 0 {
                    f.write_str(",")?;
                }
                if r.paused {
                    f.write_str("~")?;
                }
                f.write_str(&r.rid)?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for Simulcast {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        if !self.send.is_empty() {
            f.write_str("send ")?;
            Self::write_list(f, &self.send)?;
            first = false;
        }
        if !self.recv.is_empty() {
            if !first {
                f.write_str(" ")?;
            }
            f.write_str("recv ")?;
            Self::write_list(f, &self.recv)?;
        }
        Ok(())
    }
}

/// `a=group:<semantics> <mid>...`（RFC 5888）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub semantics: String,
    pub mids: Vec<String>,
}

impl Group {
    pub fn parse(v: &str) -> Result<Group> {
        let mut it = v.split_whitespace();
        let semantics = it.next().ok_or(Error::Invalid("group"))?;
        if !is_token(semantics) {
            return Err(Error::Invalid("group の種類"));
        }
        let mut mids = Vec::new();
        for m in it {
            if mids.len() >= 64 {
                return Err(Error::Limit("group の mid 数"));
            }
            if !is_token(m) || m.len() > 32 {
                return Err(Error::Invalid("group の mid"));
            }
            mids.push(m.to_string());
        }
        Ok(Group {
            semantics: semantics.to_string(),
            mids,
        })
    }
}

impl fmt::Display for Group {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.semantics)?;
        for m in &self.mids {
            write!(f, " {m}")?;
        }
        Ok(())
    }
}

/// ICE 候補の種類。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateType {
    Host,
    Srflx,
    Prflx,
    Relay,
    Other(String),
}

impl CandidateType {
    fn parse(s: &str) -> Result<CandidateType> {
        Ok(match s {
            "host" => CandidateType::Host,
            "srflx" => CandidateType::Srflx,
            "prflx" => CandidateType::Prflx,
            "relay" => CandidateType::Relay,
            t if is_token(t) => CandidateType::Other(t.to_string()),
            _ => return Err(Error::Invalid("candidate の typ")),
        })
    }

    pub fn as_str(&self) -> &str {
        match self {
            CandidateType::Host => "host",
            CandidateType::Srflx => "srflx",
            CandidateType::Prflx => "prflx",
            CandidateType::Relay => "relay",
            CandidateType::Other(s) => s,
        }
    }
}

/// 候補の接続先。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateAddress {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
    /// FQDN（mDNS の `xxxx.local` を含む）。
    Fqdn(String),
}

impl CandidateAddress {
    pub fn parse(s: &str) -> Result<CandidateAddress> {
        if s.contains(':') {
            return s
                .parse::<Ipv6Addr>()
                .map(CandidateAddress::V6)
                .map_err(|_| Error::Invalid("candidate の IPv6 アドレス"));
        }
        if let Ok(a) = s.parse::<Ipv4Addr>() {
            return Ok(CandidateAddress::V4(a));
        }
        let fqdn_ok = !s.is_empty()
            && s.len() <= 253
            && s.split('.').all(|l| {
                !l.is_empty()
                    && l.len() <= 63
                    && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            });
        if !fqdn_ok || s.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
            return Err(Error::Invalid("candidate のアドレス"));
        }
        Ok(CandidateAddress::Fqdn(s.to_string()))
    }
}

impl fmt::Display for CandidateAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CandidateAddress::V4(a) => write!(f, "{a}"),
            CandidateAddress::V6(a) => write!(f, "{a}"),
            CandidateAddress::Fqdn(s) => f.write_str(s),
        }
    }
}

/// `a=candidate:`（RFC 8839 5.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub foundation: String,
    pub component: u16,
    /// `UDP`・`TCP` など（大文字小文字を保つ）。
    pub transport: String,
    pub priority: u32,
    pub address: CandidateAddress,
    pub port: u16,
    pub typ: CandidateType,
    pub related_address: Option<CandidateAddress>,
    pub related_port: Option<u16>,
    /// `tcptype`（active・passive・so）。
    pub tcp_type: Option<String>,
    /// その他の拡張（`generation 0`・`network-id 1` など）。
    pub extensions: Vec<(String, String)>,
}

fn is_ice_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '+' || c == '/'
}

impl Candidate {
    /// 属性値を読む。先頭に `candidate:` が付いていても受け付ける（trickle ICE の文字列）。
    pub fn parse(v: &str) -> Result<Candidate> {
        let v = v.trim();
        let v = v.strip_prefix("candidate:").unwrap_or(v);
        let toks: Vec<&str> = v.split(' ').collect();
        if toks.len() < 8 || toks.len() > 64 || toks.iter().any(|t| t.is_empty()) {
            return Err(Error::Invalid("candidate の項目数"));
        }
        let foundation = toks[0];
        if foundation.is_empty() || foundation.len() > 32 || !foundation.chars().all(is_ice_char) {
            return Err(Error::Invalid("candidate の foundation"));
        }
        if toks[1].len() > 5 {
            return Err(Error::Invalid("candidate の component"));
        }
        let component: u16 = parse_num(toks[1], "candidate の component")?;
        if component == 0 {
            return Err(Error::Invalid("candidate の component が 0"));
        }
        if !is_token(toks[2]) {
            return Err(Error::Invalid("candidate の transport"));
        }
        if toks[3].len() > 10 {
            return Err(Error::Invalid("candidate の priority"));
        }
        let priority: u32 = parse_num(toks[3], "candidate の priority")?;
        let address = CandidateAddress::parse(toks[4])?;
        let port: u16 = parse_num(toks[5], "candidate の port")?;
        if toks[6] != "typ" {
            return Err(Error::Invalid("candidate に typ がない"));
        }
        let typ = CandidateType::parse(toks[7])?;
        let mut c = Candidate {
            foundation: foundation.to_string(),
            component,
            transport: toks[2].to_string(),
            priority,
            address,
            port,
            typ,
            related_address: None,
            related_port: None,
            tcp_type: None,
            extensions: Vec::new(),
        };
        let mut rest = &toks[8..];
        if !rest.len().is_multiple_of(2) {
            return Err(Error::Invalid("candidate の拡張が対になっていない"));
        }
        if rest.first() == Some(&"raddr") {
            c.related_address = Some(CandidateAddress::parse(rest[1])?);
            rest = &rest[2..];
        }
        if rest.first() == Some(&"rport") {
            c.related_port = Some(parse_num(rest[1], "candidate の rport")?);
            rest = &rest[2..];
        }
        for kv in rest.chunks(2) {
            let (k, val) = (kv[0], kv[1]);
            if !k.chars().all(|ch| ch.is_ascii_graphic())
                || !val.chars().all(|ch| ch.is_ascii_graphic())
            {
                return Err(Error::Invalid("candidate の拡張"));
            }
            match k {
                "raddr" | "rport" => return Err(Error::Invalid("candidate の raddr/rport の位置")),
                "tcptype" if c.tcp_type.is_none() => c.tcp_type = Some(val.to_string()),
                "tcptype" => return Err(Error::Invalid("candidate の tcptype が重複")),
                _ => c.extensions.push((k.to_string(), val.to_string())),
            }
        }
        Ok(c)
    }
}

impl fmt::Display for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {} {} {} {} typ {}",
            self.foundation,
            self.component,
            self.transport,
            self.priority,
            self.address,
            self.port,
            self.typ.as_str()
        )?;
        if let Some(a) = &self.related_address {
            write!(f, " raddr {a}")?;
        }
        if let Some(p) = self.related_port {
            write!(f, " rport {p}")?;
        }
        if let Some(t) = &self.tcp_type {
            write!(f, " tcptype {t}")?;
        }
        for (k, v) in &self.extensions {
            write!(f, " {k} {v}")?;
        }
        Ok(())
    }
}

/// `a=fingerprint:<hash-func> <XX:XX:...>`（RFC 8122）。
#[derive(Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// 小文字の名前（`sha-256` など）。
    pub hash_function: String,
    pub bytes: Vec<u8>,
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

impl Fingerprint {
    pub fn parse(v: &str) -> Result<Fingerprint> {
        let (h, fp) = v
            .trim()
            .split_once(' ')
            .ok_or(Error::Invalid("fingerprint"))?;
        let h = h.to_ascii_lowercase();
        if !is_token(&h) {
            return Err(Error::Invalid("fingerprint のハッシュ名"));
        }
        let mut bytes = Vec::new();
        for pair in fp.trim().split(':') {
            if pair.len() != 2 || bytes.len() >= 64 {
                return Err(Error::Invalid("fingerprint の値"));
            }
            bytes.push(
                u8::from_str_radix(pair, 16).map_err(|_| Error::Invalid("fingerprint の 16 進"))?,
            );
        }
        let want = match h.as_str() {
            "md5" => Some(16),
            "sha-1" => Some(20),
            "sha-224" => Some(28),
            "sha-256" => Some(32),
            "sha-384" => Some(48),
            "sha-512" => Some(64),
            _ => None,
        };
        if want.is_some_and(|w| w != bytes.len()) {
            return Err(Error::Invalid("fingerprint の長さがハッシュと合わない"));
        }
        Ok(Fingerprint {
            hash_function: h,
            bytes,
        })
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hash_function)?;
        f.write_str(" ")?;
        for (i, b) in self.bytes.iter().enumerate() {
            if i > 0 {
                f.write_str(":")?;
            }
            write!(f, "{b:02X}")?;
        }
        Ok(())
    }
}

/// `a=setup:`（RFC 4145）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setup {
    ActPass,
    Active,
    Passive,
    HoldConn,
}

impl Setup {
    pub fn parse(s: &str) -> Option<Setup> {
        Some(match s.trim() {
            "actpass" => Setup::ActPass,
            "active" => Setup::Active,
            "passive" => Setup::Passive,
            "holdconn" => Setup::HoldConn,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Setup::ActPass => "actpass",
            Setup::Active => "active",
            Setup::Passive => "passive",
            Setup::HoldConn => "holdconn",
        }
    }
}

impl fmt::Display for Setup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// ice-ufrag（4〜256 文字の ice-char、RFC 8839 5.4）を検査する。
pub fn validate_ice_ufrag(s: &str) -> Result<()> {
    if (4..=256).contains(&s.len()) && s.chars().all(is_ice_char) {
        Ok(())
    } else {
        Err(Error::Invalid("ice-ufrag"))
    }
}

/// ice-pwd（22〜256 文字の ice-char）を検査する。
pub fn validate_ice_pwd(s: &str) -> Result<()> {
    if (22..=256).contains(&s.len()) && s.chars().all(is_ice_char) {
        Ok(())
    } else {
        Err(Error::Invalid("ice-pwd"))
    }
}

/// `a=ice-options:` のトークン列。
pub fn parse_ice_options(s: &str) -> Result<Vec<String>> {
    let v: Vec<String> = s.split_whitespace().map(str::to_string).collect();
    if v.is_empty() || v.len() > 16 || !v.iter().all(|t| t.chars().all(is_ice_char_ext)) {
        return Err(Error::Invalid("ice-options"));
    }
    Ok(v)
}

fn is_ice_char_ext(c: char) -> bool {
    is_ice_char(c) || c == '-' || c == '_' || c == '.'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtpmap() {
        let r = RtpMap::parse("111 opus/48000/2").unwrap();
        assert_eq!(r.encoding, "opus");
        assert_eq!(r.channels, Some(2));
        assert_eq!(r.to_string(), "111 opus/48000/2");
        assert!(RtpMap::parse("128 x/90000").is_err());
        assert!(RtpMap::parse("96 H264").is_err());
        assert!(RtpMap::parse("96 H264/0").is_err());
        assert!(RtpMap::parse("96 H264/90000/2/3").is_err());
        assert!(RtpMap::parse("-1 H264/90000").is_err());
        assert_eq!(RtpMap::static_type(8).unwrap().encoding, "PCMA");
    }

    #[test]
    fn fmtp() {
        let f = Fmtp::parse("96 packetization-mode=1; sprop-parameter-sets=Z0IAH5WoFAFuQA==,aM48gA==;profile-level-id=42001F").unwrap();
        assert_eq!(f.get("PACKETIZATION-MODE"), Some("1"));
        assert_eq!(
            f.get("sprop-parameter-sets"),
            Some("Z0IAH5WoFAFuQA==,aM48gA==")
        );
        assert_eq!(
            f.to_string(),
            "96 packetization-mode=1;sprop-parameter-sets=Z0IAH5WoFAFuQA==,aM48gA==;profile-level-id=42001F"
        );
        let t = Fmtp::parse("101 0-15").unwrap();
        assert_eq!(t.params, vec![("0-15".to_string(), String::new())]);
        assert_eq!(Fmtp::parse("97").unwrap().params.len(), 0);
    }

    #[test]
    fn rtcp_fb_extmap() {
        let f = RtcpFb::parse("96 nack pli").unwrap();
        assert_eq!(
            (f.payload_type, f.kind.as_str(), f.param.as_deref()),
            (Some(96), "nack", Some("pli"))
        );
        assert_eq!(
            RtcpFb::parse("* transport-cc").unwrap().to_string(),
            "* transport-cc"
        );
        let e =
            Extmap::parse("3/recvonly http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time")
                .unwrap();
        assert_eq!(e.id, 3);
        assert_eq!(e.direction, Some(Direction::RecvOnly));
        assert!(Extmap::parse("15 urn:x").is_err());
        assert!(Extmap::parse("0 urn:x").is_err());
        assert!(Extmap::parse("256 urn:x").is_err());
        assert_eq!(
            Extmap::parse("200 urn:x a b").unwrap().to_string(),
            "200 urn:x a b"
        );
    }

    #[test]
    fn rid_simulcast() {
        let r = Rid::parse("h send pt=96,97;max-width=1280;max-height=720").unwrap();
        assert_eq!(r.payload_types, vec![96, 97]);
        assert_eq!(r.restrictions.len(), 2);
        assert_eq!(
            r.to_string(),
            "h send pt=96,97;max-width=1280;max-height=720"
        );
        assert!(Rid::parse("h sideways").is_err());
        let s = Simulcast::parse("send h;~m,l recv x").unwrap();
        assert_eq!(s.send.len(), 2);
        assert!(s.send[1][0].paused);
        assert_eq!(s.to_string(), "send h;~m,l recv x");
        assert_eq!(s.reverse().to_string(), "send x recv h;~m,l");
        assert!(Simulcast::parse("send").is_err());
        assert!(Simulcast::parse("send a send b").is_err());
    }

    #[test]
    fn candidate_rfc8839() {
        let c = Candidate::parse(
            "1 1 UDP 2130706431 203.0.113.141 8998 typ host generation 0 network-id 1",
        )
        .unwrap();
        assert_eq!(c.typ, CandidateType::Host);
        assert_eq!(c.extensions.len(), 2);
        let s = Candidate::parse(
            "candidate:2 1 UDP 1694498815 192.0.2.3 45664 typ srflx raddr 10.0.1.1 rport 8998",
        )
        .unwrap();
        assert_eq!(s.related_port, Some(8998));
        assert_eq!(
            s.to_string(),
            "2 1 UDP 1694498815 192.0.2.3 45664 typ srflx raddr 10.0.1.1 rport 8998"
        );
        let m = Candidate::parse("3 1 udp 2113937151 4f3c-11ab.local 54321 typ host").unwrap();
        assert!(matches!(m.address, CandidateAddress::Fqdn(_)));
        let t = Candidate::parse("4 1 TCP 1 2001:db8::1 9 typ host tcptype passive").unwrap();
        assert_eq!(t.tcp_type.as_deref(), Some("passive"));
        assert!(Candidate::parse("1 1 UDP 1 1.2.3.4 99999 typ host").is_err());
        assert!(Candidate::parse("1 1 UDP 1 1.2.3.4 9 type host").is_err());
        assert!(Candidate::parse("1 0 UDP 1 1.2.3.4 9 typ host").is_err());
        assert!(Candidate::parse("1 1 UDP 1 1.2.3.4 9 typ host x").is_err());
        assert!(Candidate::parse("1 1 UDP 1 1.2.3.999 9 typ host").is_err());
        assert!(Candidate::parse("1 1 UDP 99999999999 1.2.3.4 9 typ host").is_err());
    }

    #[test]
    fn fingerprint_setup_ice() {
        let v = "sha-256 19:E2:1C:3B:4B:9F:81:E6:B8:5C:F4:A5:A8:D8:73:04:BB:05:2F:70:9F:04:A9:0E:05:E9:26:33:E8:70:88:A2";
        let f = Fingerprint::parse(v).unwrap();
        assert_eq!(f.bytes.len(), 32);
        assert_eq!(f.to_string(), v);
        assert!(Fingerprint::parse("sha-256 19:E2").is_err());
        assert!(Fingerprint::parse("sha-1 GG:00").is_err());
        assert_eq!(Setup::parse("actpass"), Some(Setup::ActPass));
        assert!(validate_ice_ufrag("abcd").is_ok());
        assert!(validate_ice_ufrag("ab").is_err());
        assert!(validate_ice_pwd("short").is_err());
        assert!(validate_ice_pwd("asd88fgpdd777uzjYhagZg").is_ok());
        assert_eq!(parse_ice_options("trickle ice2").unwrap().len(), 2);
    }

    #[test]
    fn direction_ops() {
        assert_eq!(Direction::SendOnly.reverse(), Direction::RecvOnly);
        assert_eq!(
            Direction::SendRecv.intersect(Direction::RecvOnly),
            Direction::RecvOnly
        );
        assert_eq!(
            Direction::SendOnly.intersect(Direction::RecvOnly),
            Direction::Inactive
        );
    }

    #[test]
    fn ssrc_msid_group() {
        let s = SsrcAttr::parse("1234 cname:user@example.com").unwrap();
        assert_eq!(s.value.as_deref(), Some("user@example.com"));
        assert_eq!(s.to_string(), "1234 cname:user@example.com");
        assert!(SsrcAttr::parse("4294967296 cname:x").is_err());
        let g = SsrcGroup::parse("FID 1 2").unwrap();
        assert_eq!(g.ssrcs, vec![1, 2]);
        assert!(SsrcGroup::parse("FID").is_err());
        let m = Msid::parse("stream track").unwrap();
        assert_eq!(m.track_id.as_deref(), Some("track"));
        let gr = Group::parse("BUNDLE 0 1 data").unwrap();
        assert_eq!(gr.mids, vec!["0", "1", "data"]);
    }
}
