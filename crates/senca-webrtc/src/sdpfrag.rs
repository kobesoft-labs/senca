//! trickle ICE の SDP 断片（`application/trickle-ice-sdpfrag`、RFC 8840 9 節）。
//!
//! WHIP（RFC 9725 4.3）の PATCH の本文。候補の追加（trickle）と ICE restart（新しい ufrag・pwd）に使う。
//!
//! ```text
//! a=ice-options:trickle ice2
//! a=group:BUNDLE 0 1
//! m=audio 9 UDP/TLS/RTP/SAVPF 111
//! a=mid:0
//! a=ice-ufrag:EsAw
//! a=ice-pwd:P2uYro0UCOQ4zxjKXaWCBui1
//! a=candidate:1387637174 1 udp 2122260223 192.0.2.1 61764 typ host generation 0
//! a=end-of-candidates
//! ```
//!
//! ufrag・pwd は session 単位にも m 行単位にも置ける。BUNDLE では m 行は 1 つだけのことが多い。

use std::fmt;

/// 本文の上限（byte）。
pub const MAX_SDPFRAG_BYTES: usize = 64 * 1024;
/// 候補の数の上限。
pub const MAX_CANDIDATES: usize = 100;
/// m 行の数の上限。
pub const MAX_MEDIA: usize = 64;

/// 解析の誤り。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SdpFragError {
    TooLarge,
    TooMany,
    /// 行の形が不正（行番号は 1 から）。
    BadLine(usize),
    BadCredentials,
}

impl fmt::Display for SdpFragError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SdpFragError::TooLarge => f.write_str("sdpfrag が大きすぎる"),
            SdpFragError::TooMany => f.write_str("sdpfrag の候補・m 行が多すぎる"),
            SdpFragError::BadLine(n) => write!(f, "sdpfrag の {n} 行目が不正"),
            SdpFragError::BadCredentials => f.write_str("sdpfrag の ice-ufrag・ice-pwd が不正"),
        }
    }
}

impl std::error::Error for SdpFragError {}

/// m 行 1 つ分。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SdpFragMedia {
    /// `m=` の値（例 `audio 9 UDP/TLS/RTP/SAVPF 111`）。
    pub m_line: String,
    pub mid: Option<String>,
    pub ice_ufrag: Option<String>,
    pub ice_pwd: Option<String>,
    /// `candidate:...`（`a=` を除いた形）。
    pub candidates: Vec<String>,
    pub end_of_candidates: bool,
}

/// SDP 断片。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SdpFrag {
    pub ice_ufrag: Option<String>,
    pub ice_pwd: Option<String>,
    pub ice_options: Vec<String>,
    /// `a=group:` の値。
    pub group: Option<String>,
    pub media: Vec<SdpFragMedia>,
    /// session 単位の候補（m 行の前に置かれたもの。RFC 8840 の外だが受け付ける）。
    pub candidates: Vec<String>,
    pub end_of_candidates: bool,
}

fn ice_chars(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

impl SdpFrag {
    /// 解析する。知らない行は無視する。
    pub fn parse(text: &str) -> Result<SdpFrag, SdpFragError> {
        if text.len() > MAX_SDPFRAG_BYTES {
            return Err(SdpFragError::TooLarge);
        }
        let mut f = SdpFrag::default();
        let mut ncand = 0usize;
        for (i, raw) in text.split('\n').enumerate() {
            let line = raw.strip_suffix('\r').unwrap_or(raw).trim_end();
            if line.is_empty() {
                continue;
            }
            let bad = || SdpFragError::BadLine(i + 1);
            if line.len() < 2 || line.as_bytes()[1] != b'=' {
                return Err(bad());
            }
            let (typ, val) = (line.as_bytes()[0], &line[2..]);
            if typ == b'm' {
                if f.media.len() >= MAX_MEDIA {
                    return Err(SdpFragError::TooMany);
                }
                f.media.push(SdpFragMedia {
                    m_line: val.to_string(),
                    ..Default::default()
                });
                continue;
            }
            if typ != b'a' {
                continue;
            }
            let (name, value) = match val.split_once(':') {
                Some((n, v)) => (n, Some(v)),
                None => (val, None),
            };
            let media = f.media.last_mut();
            match (name, value) {
                ("ice-ufrag", Some(v)) => {
                    if !ice_chars(v) {
                        return Err(SdpFragError::BadCredentials);
                    }
                    match media {
                        Some(m) => m.ice_ufrag = Some(v.to_string()),
                        None => f.ice_ufrag = Some(v.to_string()),
                    }
                }
                ("ice-pwd", Some(v)) => {
                    if !ice_chars(v) {
                        return Err(SdpFragError::BadCredentials);
                    }
                    match media {
                        Some(m) => m.ice_pwd = Some(v.to_string()),
                        None => f.ice_pwd = Some(v.to_string()),
                    }
                }
                ("ice-options", Some(v)) => {
                    f.ice_options = v.split_whitespace().take(16).map(str::to_string).collect()
                }
                ("group", Some(v)) => f.group = Some(v.to_string()),
                ("mid", Some(v)) => {
                    if let Some(m) = media {
                        if v.is_empty() || v.len() > 64 {
                            return Err(bad());
                        }
                        m.mid = Some(v.to_string());
                    }
                }
                ("candidate", Some(v)) => {
                    ncand += 1;
                    if ncand > MAX_CANDIDATES {
                        return Err(SdpFragError::TooMany);
                    }
                    if v.is_empty() || v.len() > 1024 {
                        return Err(bad());
                    }
                    let c = format!("candidate:{v}");
                    match media {
                        Some(m) => m.candidates.push(c),
                        None => f.candidates.push(c),
                    }
                }
                ("end-of-candidates", None) => match media {
                    Some(m) => m.end_of_candidates = true,
                    None => f.end_of_candidates = true,
                },
                _ => {}
            }
        }
        Ok(f)
    }

    /// ICE の資格情報（m 行のものがあればそれ、なければ session 単位）。
    pub fn credentials(&self) -> Option<(&str, &str)> {
        for m in &self.media {
            if let (Some(u), Some(p)) = (&m.ice_ufrag, &m.ice_pwd) {
                return Some((u, p));
            }
        }
        match (&self.ice_ufrag, &self.ice_pwd) {
            (Some(u), Some(p)) => Some((u, p)),
            _ => None,
        }
    }

    /// ufrag だけ（pwd がない PATCH でも ufrag は付く）。
    pub fn ufrag(&self) -> Option<&str> {
        self.media
            .iter()
            .find_map(|m| m.ice_ufrag.as_deref())
            .or(self.ice_ufrag.as_deref())
    }

    /// すべての候補。
    pub fn all_candidates(&self) -> impl Iterator<Item = &str> {
        self.candidates
            .iter()
            .chain(self.media.iter().flat_map(|m| m.candidates.iter()))
            .map(String::as_str)
    }

    /// end-of-candidates があるか。
    pub fn has_end_of_candidates(&self) -> bool {
        self.end_of_candidates || self.media.iter().any(|m| m.end_of_candidates)
    }

    /// 1 つの m 行（BUNDLE の代表）で断片を作る。
    pub fn build(
        mid: &str,
        m_line: &str,
        ufrag: &str,
        pwd: &str,
        candidates: &[String],
        end_of_candidates: bool,
    ) -> SdpFrag {
        SdpFrag {
            ice_ufrag: Some(ufrag.to_string()),
            ice_pwd: Some(pwd.to_string()),
            ice_options: vec!["trickle".into()],
            group: Some(format!("BUNDLE {mid}")),
            media: vec![SdpFragMedia {
                m_line: m_line.to_string(),
                mid: Some(mid.to_string()),
                ice_ufrag: None,
                ice_pwd: None,
                candidates: candidates.to_vec(),
                end_of_candidates,
            }],
            candidates: Vec::new(),
            end_of_candidates: false,
        }
    }
}

impl fmt::Display for SdpFrag {
    /// CRLF で書く。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.ice_options.is_empty() {
            write!(f, "a=ice-options:{}\r\n", self.ice_options.join(" "))?;
        }
        if let Some(g) = &self.group {
            write!(f, "a=group:{g}\r\n")?;
        }
        if let Some(u) = &self.ice_ufrag {
            write!(f, "a=ice-ufrag:{u}\r\n")?;
        }
        if let Some(p) = &self.ice_pwd {
            write!(f, "a=ice-pwd:{p}\r\n")?;
        }
        for c in &self.candidates {
            write!(f, "a={}\r\n", strip_prefix(c))?;
        }
        if self.end_of_candidates {
            f.write_str("a=end-of-candidates\r\n")?;
        }
        for m in &self.media {
            write!(f, "m={}\r\n", m.m_line)?;
            if let Some(mid) = &m.mid {
                write!(f, "a=mid:{mid}\r\n")?;
            }
            if let Some(u) = &m.ice_ufrag {
                write!(f, "a=ice-ufrag:{u}\r\n")?;
            }
            if let Some(p) = &m.ice_pwd {
                write!(f, "a=ice-pwd:{p}\r\n")?;
            }
            for c in &m.candidates {
                write!(f, "a={}\r\n", strip_prefix(c))?;
            }
            if m.end_of_candidates {
                f.write_str("a=end-of-candidates\r\n")?;
            }
        }
        Ok(())
    }
}

fn strip_prefix(c: &str) -> String {
    let c = c.strip_prefix("a=").unwrap_or(c);
    if c.starts_with("candidate:") {
        c.to_string()
    } else {
        format!("candidate:{c}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RFC_EXAMPLE: &str = "a=ice-options:trickle ice2\r\na=group:BUNDLE 0 1\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\na=ice-ufrag:EsAw\r\na=ice-pwd:P2uYro0UCOQ4zxjKXaWCBui1\r\na=candidate:1387637174 1 udp 2122260223 192.0.2.1 61764 typ host generation 0 ufrag EsAw network-id 1\r\na=candidate:3471623853 1 udp 2122194687 198.51.100.2 61765 typ host generation 0 ufrag EsAw network-id 2\r\na=candidate:473322822 1 tcp 1518280447 192.0.2.1 9 typ host tcptype active generation 0 ufrag EsAw network-id 1\r\na=end-of-candidates\r\n";

    #[test]
    fn parse_rfc_example() {
        let f = SdpFrag::parse(RFC_EXAMPLE).unwrap();
        assert_eq!(f.ice_options, vec!["trickle", "ice2"]);
        assert_eq!(f.group.as_deref(), Some("BUNDLE 0 1"));
        assert_eq!(f.media.len(), 1);
        assert_eq!(f.media[0].mid.as_deref(), Some("0"));
        assert_eq!(f.credentials(), Some(("EsAw", "P2uYro0UCOQ4zxjKXaWCBui1")));
        assert_eq!(f.all_candidates().count(), 3);
        assert!(f.all_candidates().all(|c| c.starts_with("candidate:")));
        assert!(f.has_end_of_candidates());
        // 書いて読み直すと同じ
        let again = SdpFrag::parse(&f.to_string()).unwrap();
        assert_eq!(again, f);
    }

    #[test]
    fn build_and_session_level() {
        let f = SdpFrag::build(
            "0",
            "audio 9 UDP/TLS/RTP/SAVPF 0",
            "abcd",
            "0123456789012345678901",
            &["candidate:1 1 udp 1 192.0.2.1 5000 typ host".into()],
            true,
        );
        let s = f.to_string();
        assert!(s.contains("a=ice-ufrag:abcd\r\n"));
        assert!(s.contains("m=audio 9 UDP/TLS/RTP/SAVPF 0\r\na=mid:0\r\n"));
        assert!(s.contains("a=candidate:1 1 udp 1 192.0.2.1 5000 typ host\r\n"));
        let p = SdpFrag::parse(&s).unwrap();
        assert_eq!(p.credentials(), Some(("abcd", "0123456789012345678901")));
        let only =
            SdpFrag::parse("a=ice-ufrag:xyzw\na=candidate:1 1 udp 1 1.2.3.4 9 typ host\n").unwrap();
        assert_eq!(only.ufrag(), Some("xyzw"));
        assert_eq!(only.all_candidates().count(), 1);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(SdpFrag::parse("x").is_err());
        assert!(SdpFrag::parse("a=ice-ufrag:bad ufrag\r\n").is_err());
        assert!(SdpFrag::parse(&"a".repeat(MAX_SDPFRAG_BYTES + 1)).is_err());
        let many: String = (0..=MAX_CANDIDATES)
            .map(|i| format!("a=candidate:{i} 1 udp 1 1.2.3.4 9 typ host\n"))
            .collect();
        assert_eq!(SdpFrag::parse(&many), Err(SdpFragError::TooMany));
    }

    #[test]
    fn fuzz_no_panic() {
        let mut s = 0x1234_5678u32;
        let base = RFC_EXAMPLE.as_bytes();
        for _ in 0..3000 {
            let mut v = base.to_vec();
            for _ in 0..4 {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                let k = s as usize % v.len();
                v[k] = (s >> 8) as u8;
            }
            s ^= s << 13;
            v.truncate(s as usize % (v.len() + 1));
            if let Ok(t) = std::str::from_utf8(&v) {
                let _ = SdpFrag::parse(t).map(|f| f.to_string());
            }
        }
    }
}
