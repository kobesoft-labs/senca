//! ヘッダー値の型: Transport（RFC 2326 12.39）、Session（12.37）、Range（12.29・3.5〜3.7）、
//! RTP-Info（12.33）、Public（12.28）。

use crate::error::{Error, Result};
use crate::message::{Method, parse_u32_strict};
use std::fmt;

/// 下位の輸送。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LowerTransport {
    Udp,
    Tcp,
}

/// Transport ヘッダーの 1 つの候補。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transport {
    pub lower: LowerTransport,
    /// `RTP/AVP/UDP` のように下位輸送を明示していたか（書き出しの形を保つため）。
    pub explicit_lower: bool,
    /// unicast（既定）なら真。
    pub unicast: bool,
    pub destination: Option<String>,
    pub source: Option<String>,
    pub interleaved: Option<(u8, u8)>,
    pub ttl: Option<u8>,
    pub layers: Option<u32>,
    pub port: Option<(u16, u16)>,
    pub client_port: Option<(u16, u16)>,
    pub server_port: Option<(u16, u16)>,
    pub ssrc: Option<u32>,
    /// `mode="PLAY"` など（引用符は外す）。
    pub mode: Option<String>,
    /// 知らない引数（そのまま保つ）。
    pub other: Vec<String>,
}

/// 1 つの Transport ヘッダーで受け入れる候補の数。
const MAX_TRANSPORTS: usize = 16;
const MAX_PARAMS: usize = 32;

impl Transport {
    /// UDP unicast（client_port）。
    pub fn udp(rtp_port: u16, rtcp_port: u16) -> Self {
        Transport {
            client_port: Some((rtp_port, rtcp_port)),
            ..Transport::base(LowerTransport::Udp)
        }
    }

    /// TCP interleaved。
    pub fn interleaved(rtp_channel: u8, rtcp_channel: u8) -> Self {
        Transport {
            explicit_lower: true,
            interleaved: Some((rtp_channel, rtcp_channel)),
            ..Transport::base(LowerTransport::Tcp)
        }
    }

    fn base(lower: LowerTransport) -> Self {
        Transport {
            lower,
            explicit_lower: false,
            unicast: true,
            destination: None,
            source: None,
            interleaved: None,
            ttl: None,
            layers: None,
            port: None,
            client_port: None,
            server_port: None,
            ssrc: None,
            mode: None,
            other: Vec::new(),
        }
    }

    /// 1 つの候補を解釈する（`RTP/AVP` で始まるもの）。
    pub fn parse(s: &str) -> Result<Transport> {
        let mut parts = s.trim().split(';');
        let spec = parts.next().unwrap_or("").trim();
        let mut t = match spec.to_ascii_uppercase().as_str() {
            "RTP/AVP" => Transport::base(LowerTransport::Udp),
            "RTP/AVP/UDP" => Transport {
                explicit_lower: true,
                ..Transport::base(LowerTransport::Udp)
            },
            "RTP/AVP/TCP" => Transport {
                explicit_lower: true,
                ..Transport::base(LowerTransport::Tcp)
            },
            _ => return Err(Error::Unsupported("Transport の profile")),
        };
        for (i, p) in parts.enumerate() {
            if i >= MAX_PARAMS {
                return Err(Error::LimitExceeded("Transport の引数"));
            }
            let p = p.trim();
            if p.is_empty() {
                continue;
            }
            let (k, v) = match p.split_once('=') {
                Some((k, v)) => (k.trim(), Some(v.trim())),
                None => (p, None),
            };
            fn need(v: Option<&str>) -> Result<&str> {
                v.ok_or(Error::Malformed("Transport の値がない"))
            }
            match k.to_ascii_lowercase().as_str() {
                "unicast" => t.unicast = true,
                "multicast" => t.unicast = false,
                "destination" => t.destination = Some(check_value(need(v)?)?.to_string()),
                "source" => t.source = Some(check_value(need(v)?)?.to_string()),
                "interleaved" => {
                    let (a, b) = parse_pair(need(v)?, 255)?;
                    t.interleaved = Some((a as u8, b as u8));
                }
                "ttl" => {
                    let n = parse_u32_strict(need(v)?).ok_or(Error::Malformed("ttl"))?;
                    t.ttl = Some(u8::try_from(n).map_err(|_| Error::Malformed("ttl"))?);
                }
                "layers" => {
                    t.layers = Some(parse_u32_strict(need(v)?).ok_or(Error::Malformed("layers"))?)
                }
                "port" => t.port = Some(parse_port_pair(need(v)?)?),
                "client_port" => t.client_port = Some(parse_port_pair(need(v)?)?),
                "server_port" => t.server_port = Some(parse_port_pair(need(v)?)?),
                "ssrc" => {
                    let v = need(v)?;
                    if v.is_empty() || v.len() > 8 || !v.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return Err(Error::Malformed("ssrc"));
                    }
                    t.ssrc =
                        Some(u32::from_str_radix(v, 16).map_err(|_| Error::Malformed("ssrc"))?);
                }
                "mode" => {
                    let v = need(v)?.trim_matches('"');
                    t.mode = Some(check_value(v)?.to_string());
                }
                _ => t.other.push(check_value(p)?.to_string()),
            }
        }
        Ok(t)
    }

    /// カンマ区切りの候補を順に解釈する。未対応の profile（RTP/SAVP など）は飛ばす。
    /// 1 つも解釈できなければ `Err`。
    pub fn parse_list(s: &str) -> Result<Vec<Transport>> {
        let mut out = Vec::new();
        let mut last_err = Error::Malformed("Transport が空");
        for (i, item) in s.split(',').enumerate() {
            if i >= MAX_TRANSPORTS {
                return Err(Error::LimitExceeded("Transport の候補"));
            }
            match Transport::parse(item) {
                Ok(t) => out.push(t),
                Err(e) => last_err = e,
            }
        }
        if out.is_empty() {
            return Err(last_err);
        }
        Ok(out)
    }

    /// カンマ区切りで書く。
    pub fn format_list(list: &[Transport]) -> String {
        list.iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn check_value(v: &str) -> Result<&str> {
    if v.len() > 256
        || v.bytes()
            .any(|b| b < b' ' || b == b',' || b == b';' || b == 0x7f)
    {
        return Err(Error::Malformed("Transport の値"));
    }
    Ok(v)
}

fn parse_pair(v: &str, max: u32) -> Result<(u32, u32)> {
    let (a, b) = match v.split_once('-') {
        Some((a, b)) => {
            let a = parse_u32_strict(a.trim()).ok_or(Error::Malformed("範囲"))?;
            let b = parse_u32_strict(b.trim()).ok_or(Error::Malformed("範囲"))?;
            (a, b)
        }
        None => {
            let a = parse_u32_strict(v.trim()).ok_or(Error::Malformed("範囲"))?;
            (a, a.saturating_add(1))
        }
    };
    if a > max || b > max || b < a {
        return Err(Error::Malformed("範囲"));
    }
    Ok((a, b))
}

fn parse_port_pair(v: &str) -> Result<(u16, u16)> {
    let (a, b) = parse_pair(v, 65535)?;
    Ok((a as u16, b as u16))
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match (self.lower, self.explicit_lower) {
            (LowerTransport::Tcp, _) => "RTP/AVP/TCP",
            (LowerTransport::Udp, true) => "RTP/AVP/UDP",
            (LowerTransport::Udp, false) => "RTP/AVP",
        })?;
        f.write_str(if self.unicast {
            ";unicast"
        } else {
            ";multicast"
        })?;
        if let Some(d) = &self.destination {
            write!(f, ";destination={d}")?;
        }
        if let Some(s) = &self.source {
            write!(f, ";source={s}")?;
        }
        if let Some((a, b)) = self.interleaved {
            write!(f, ";interleaved={a}-{b}")?;
        }
        if let Some(t) = self.ttl {
            write!(f, ";ttl={t}")?;
        }
        if let Some(l) = self.layers {
            write!(f, ";layers={l}")?;
        }
        for (name, p) in [
            ("port", self.port),
            ("client_port", self.client_port),
            ("server_port", self.server_port),
        ] {
            if let Some((a, b)) = p {
                write!(f, ";{name}={a}-{b}")?;
            }
        }
        if let Some(s) = self.ssrc {
            write!(f, ";ssrc={s:08X}")?;
        }
        if let Some(m) = &self.mode {
            write!(f, ";mode=\"{m}\"")?;
        }
        for o in &self.other {
            write!(f, ";{o}")?;
        }
        Ok(())
    }
}

/// Session ヘッダー（RFC 2326 12.37）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHeader {
    pub id: String,
    /// 秒。指定がなければ `None`（既定 60 秒）。
    pub timeout: Option<u32>,
}

/// Session の既定の timeout（秒）。
pub const DEFAULT_SESSION_TIMEOUT: u32 = 60;

impl SessionHeader {
    pub fn parse(s: &str) -> Result<SessionHeader> {
        let mut it = s.split(';');
        let id = it.next().unwrap_or("").trim();
        if id.is_empty()
            || id.len() > 256
            || !id
                .bytes()
                .all(|b| b.is_ascii_graphic() && b != b';' && b != b',')
        {
            return Err(Error::Malformed("Session ID"));
        }
        let mut timeout = None;
        for p in it {
            let p = p.trim();
            if let Some((k, v)) = p.split_once('=')
                && k.trim().eq_ignore_ascii_case("timeout")
            {
                let n = parse_u32_strict(v.trim()).ok_or(Error::Malformed("Session timeout"))?;
                timeout = Some(n);
            }
        }
        Ok(SessionHeader {
            id: id.to_string(),
            timeout,
        })
    }

    /// 実際の timeout（秒）。0 や指定なしは既定値。
    pub fn timeout_secs(&self) -> u32 {
        match self.timeout {
            Some(0) | None => DEFAULT_SESSION_TIMEOUT,
            Some(t) => t,
        }
    }
}

impl fmt::Display for SessionHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.id)?;
        if let Some(t) = self.timeout {
            write!(f, ";timeout={t}")?;
        }
        Ok(())
    }
}

/// NPT の時点（RFC 2326 3.6）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NptTime {
    Now,
    /// 秒。
    Seconds(f64),
}

/// Range ヘッダー（RFC 2326 12.29）。
#[derive(Debug, Clone, PartialEq)]
pub enum Range {
    /// `npt=start-[end]`。
    Npt { start: NptTime, end: Option<f64> },
    /// `clock=YYYYMMDDThhmmss[.f]Z-[...]`（文字列のまま）。
    Clock { start: String, end: Option<String> },
    /// `smpte...=start-[end]`（文字列のまま）。
    Smpte {
        kind: String,
        start: String,
        end: Option<String>,
    },
}

impl Range {
    /// `npt=start-`。
    pub fn npt_from(start: f64) -> Range {
        Range::Npt {
            start: NptTime::Seconds(start),
            end: None,
        }
    }

    pub fn parse(s: &str) -> Result<Range> {
        let main = s.split(';').next().unwrap_or("").trim();
        let (unit, value) = main.split_once('=').ok_or(Error::Malformed("Range"))?;
        let unit = unit.trim().to_ascii_lowercase();
        let (a, b) = value
            .trim()
            .split_once('-')
            .ok_or(Error::Malformed("Range"))?;
        let (a, b) = (a.trim(), b.trim());
        let b = (!b.is_empty()).then_some(b);
        match unit.as_str() {
            "npt" => {
                let start = if a.eq_ignore_ascii_case("now") {
                    NptTime::Now
                } else if a.is_empty() {
                    // `npt=-10` は end だけ（start は 0 とみなす）
                    NptTime::Seconds(0.0)
                } else {
                    NptTime::Seconds(parse_npt(a)?)
                };
                let end = b.map(parse_npt).transpose()?;
                Ok(Range::Npt { start, end })
            }
            "clock" => Ok(Range::Clock {
                start: check_range_value(a)?.to_string(),
                end: b.map(check_range_value).transpose()?.map(str::to_string),
            }),
            u if u.starts_with("smpte") => Ok(Range::Smpte {
                kind: check_range_value(u)?.to_string(),
                start: check_range_value(a)?.to_string(),
                end: b.map(check_range_value).transpose()?.map(str::to_string),
            }),
            _ => Err(Error::Unsupported("Range の単位")),
        }
    }
}

fn check_range_value(v: &str) -> Result<&str> {
    if v.is_empty() || v.len() > 64 || !v.bytes().all(|b| b.is_ascii_graphic() && b != b';') {
        return Err(Error::Malformed("Range"));
    }
    Ok(v)
}

/// `123.45` または `h:mm:ss.fff`。
fn parse_npt(s: &str) -> Result<f64> {
    if s.is_empty()
        || s.len() > 32
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'.' || b == b':')
    {
        return Err(Error::Malformed("NPT"));
    }
    let parts: Vec<&str> = s.split(':').collect();
    let v = match parts.as_slice() {
        [sec] => sec.parse::<f64>().map_err(|_| Error::Malformed("NPT"))?,
        [h, m, sec] => {
            let h: u32 = parse_u32_strict(h).ok_or(Error::Malformed("NPT"))?;
            let m: u32 = parse_u32_strict(m).ok_or(Error::Malformed("NPT"))?;
            let sec: f64 = sec.parse().map_err(|_| Error::Malformed("NPT"))?;
            if m >= 60 || sec >= 60.0 {
                return Err(Error::Malformed("NPT"));
            }
            h as f64 * 3600.0 + m as f64 * 60.0 + sec
        }
        _ => return Err(Error::Malformed("NPT")),
    };
    if !v.is_finite() || v < 0.0 {
        return Err(Error::Malformed("NPT"));
    }
    Ok(v)
}

fn fmt_npt(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Range::Npt { start, end } => {
                f.write_str("npt=")?;
                match start {
                    NptTime::Now => f.write_str("now")?,
                    NptTime::Seconds(s) => f.write_str(&fmt_npt(*s))?,
                }
                f.write_str("-")?;
                if let Some(e) = end {
                    f.write_str(&fmt_npt(*e))?;
                }
                Ok(())
            }
            Range::Clock { start, end } => {
                write!(f, "clock={start}-{}", end.as_deref().unwrap_or(""))
            }
            Range::Smpte { kind, start, end } => {
                write!(f, "{kind}={start}-{}", end.as_deref().unwrap_or(""))
            }
        }
    }
}

/// RTP-Info の 1 項目（RFC 2326 12.33）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpInfoEntry {
    pub url: String,
    pub seq: Option<u16>,
    pub rtptime: Option<u32>,
}

const MAX_RTP_INFO: usize = 64;

/// RTP-Info を解釈する。URL にカンマが含まれる実装もあるので、`url=` で始まらない断片は直前の URL に続ける。
pub fn parse_rtp_info(s: &str) -> Result<Vec<RtpInfoEntry>> {
    let mut items: Vec<String> = Vec::new();
    for piece in s.split(',') {
        let t = piece.trim_start();
        if t.len() >= 4 && t[..4].eq_ignore_ascii_case("url=") || items.is_empty() {
            if items.len() >= MAX_RTP_INFO {
                return Err(Error::LimitExceeded("RTP-Info の項目"));
            }
            items.push(t.to_string());
        } else if let Some(last) = items.last_mut() {
            last.push(',');
            last.push_str(piece);
        }
    }
    let mut out = Vec::new();
    for item in items {
        let mut e = RtpInfoEntry {
            url: String::new(),
            seq: None,
            rtptime: None,
        };
        // url の中に ';' があり得るので、seq= と rtptime= だけを後ろから取り出す。
        let mut rest = item.trim();
        while let Some(pos) = rest.rfind(';') {
            let (head, tail) = (&rest[..pos], rest[pos + 1..].trim());
            if let Some((k, v)) = tail.split_once('=') {
                match k.trim().to_ascii_lowercase().as_str() {
                    "seq" => {
                        let n =
                            parse_u32_strict(v.trim()).ok_or(Error::Malformed("RTP-Info seq"))?;
                        e.seq =
                            Some(u16::try_from(n).map_err(|_| Error::Malformed("RTP-Info seq"))?);
                        rest = head;
                        continue;
                    }
                    "rtptime" => {
                        e.rtptime = Some(
                            parse_u32_strict(v.trim())
                                .ok_or(Error::Malformed("RTP-Info rtptime"))?,
                        );
                        rest = head;
                        continue;
                    }
                    _ => {}
                }
            }
            break;
        }
        let url = rest.trim();
        let url = url
            .get(..4)
            .filter(|p| p.eq_ignore_ascii_case("url="))
            .map(|_| &url[4..])
            .ok_or(Error::Malformed("RTP-Info url"))?;
        if url.bytes().any(|b| b <= b' ') {
            return Err(Error::Malformed("RTP-Info url"));
        }
        e.url = url.to_string();
        out.push(e);
    }
    Ok(out)
}

/// RTP-Info を書く。
pub fn format_rtp_info(list: &[RtpInfoEntry]) -> String {
    list.iter()
        .map(|e| {
            let mut s = format!("url={}", e.url);
            if let Some(q) = e.seq {
                s.push_str(&format!(";seq={q}"));
            }
            if let Some(t) = e.rtptime {
                s.push_str(&format!(";rtptime={t}"));
            }
            s
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Public ヘッダー（メソッドの一覧）。解釈できない名前は飛ばす。
pub fn parse_public(s: &str) -> Vec<Method> {
    s.split(',')
        .take(64)
        .filter_map(|m| Method::parse(m.trim()).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_rfc_examples() {
        // RFC 2326 12.39
        let t = Transport::parse("RTP/AVP;multicast;ttl=127;mode=\"PLAY\"").unwrap();
        assert!(!t.unicast);
        assert_eq!(t.ttl, Some(127));
        assert_eq!(t.mode.as_deref(), Some("PLAY"));
        let l = Transport::parse_list(
            "RTP/AVP;multicast;ttl=127;mode=\"PLAY\",RTP/AVP;unicast;client_port=3456-3457;mode=\"PLAY\"",
        )
        .unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l[1].client_port, Some((3456, 3457)));
        // 14.4 の応答
        let t = Transport::parse(
            "RTP/AVP;unicast;client_port=4588-4589;server_port=6256-6257;ssrc=0A13F2B4",
        )
        .unwrap();
        assert_eq!(t.server_port, Some((6256, 6257)));
        assert_eq!(t.ssrc, Some(0x0A13_F2B4));
        assert_eq!(Transport::parse(&t.to_string()).unwrap(), t);
        let t = Transport::parse("RTP/AVP/TCP;unicast;interleaved=2-3").unwrap();
        assert_eq!(t.lower, LowerTransport::Tcp);
        assert_eq!(t.interleaved, Some((2, 3)));
        assert_eq!(t.to_string(), "RTP/AVP/TCP;unicast;interleaved=2-3");
        let t = Transport::parse("RTP/AVP/TCP;interleaved=4").unwrap();
        assert_eq!(t.interleaved, Some((4, 5)));
        // 未知の引数は保つ
        let t = Transport::parse("RTP/AVP;unicast;client_port=5000-5001;x-foo=1").unwrap();
        assert_eq!(t.other, vec!["x-foo=1".to_string()]);
        assert_eq!(Transport::parse(&t.to_string()).unwrap(), t);
    }

    #[test]
    fn transport_rejects() {
        for s in [
            "RTP/SAVP;unicast",
            "RTP/AVP;client_port=70000-70001",
            "RTP/AVP;client_port=5-4",
            "RTP/AVP/TCP;interleaved=300-301",
            "RTP/AVP;ssrc=123456789",
            "RTP/AVP;client_port",
            "RTP/AVP;destination=a\x01b",
        ] {
            assert!(Transport::parse(s).is_err(), "{s}");
        }
        // 候補の中に未対応があっても残りを使う
        let l = Transport::parse_list("RTP/SAVP;unicast,RTP/AVP;unicast;client_port=1-2").unwrap();
        assert_eq!(l.len(), 1);
        assert!(Transport::parse_list(&"RTP/AVP,".repeat(20)).is_err());
    }

    #[test]
    fn session_header() {
        let s = SessionHeader::parse("12345678;timeout=30").unwrap();
        assert_eq!(s.id, "12345678");
        assert_eq!(s.timeout_secs(), 30);
        assert_eq!(s.to_string(), "12345678;timeout=30");
        assert_eq!(SessionHeader::parse("abc").unwrap().timeout_secs(), 60);
        assert!(SessionHeader::parse("").is_err());
        assert!(SessionHeader::parse("a;timeout=x").is_err());
        assert!(SessionHeader::parse("a b").is_err());
    }

    #[test]
    fn range() {
        assert_eq!(
            Range::parse("npt=0.000-").unwrap(),
            Range::Npt {
                start: NptTime::Seconds(0.0),
                end: None
            }
        );
        assert_eq!(
            Range::parse("npt=now-").unwrap(),
            Range::Npt {
                start: NptTime::Now,
                end: None
            }
        );
        assert_eq!(
            Range::parse("npt=0:01:02.5-1:00:00").unwrap(),
            Range::Npt {
                start: NptTime::Seconds(62.5),
                end: Some(3600.0)
            }
        );
        assert_eq!(Range::npt_from(1.25).to_string(), "npt=1.25-");
        assert_eq!(Range::npt_from(0.0).to_string(), "npt=0-");
        let c = Range::parse("clock=19961108T143720.25Z-").unwrap();
        assert_eq!(c.to_string(), "clock=19961108T143720.25Z-");
        let s = Range::parse("smpte=10:07:33-;time=19970123T153600Z").unwrap();
        assert!(matches!(s, Range::Smpte { .. }));
        for bad in ["npt", "npt=abc-", "npt=1:99:00-", "foo=1-2", "npt=-1-"] {
            assert!(Range::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rtp_info() {
        // RFC 2326 12.33
        let v = parse_rtp_info(
            "url=rtsp://foo.com/bar.avi/streamid=0;seq=45102,url=rtsp://foo.com/bar.avi/streamid=1;seq=30211",
        )
        .unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].url, "rtsp://foo.com/bar.avi/streamid=0");
        assert_eq!(v[1].seq, Some(30211));
        let v = parse_rtp_info("url=rtsp://h/a;b=1,c/trackID=1;seq=1;rtptime=4294967295").unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].url, "rtsp://h/a;b=1,c/trackID=1");
        assert_eq!(v[0].rtptime, Some(u32::MAX));
        assert_eq!(parse_rtp_info(&format_rtp_info(&v)).unwrap(), v);
        assert!(parse_rtp_info("url=x;seq=70000").is_err());
        assert!(parse_rtp_info("seq=1").is_err());
    }

    #[test]
    fn public() {
        let p = parse_public("OPTIONS, DESCRIBE, SETUP, TEARDOWN, PLAY, PAUSE, GET_PARAMETER");
        assert!(p.contains(&Method::GetParameter));
        assert_eq!(p.len(), 7);
    }
}
