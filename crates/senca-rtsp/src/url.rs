//! rtsp:// と rtsps:// の URL、制御 URL の解決（RFC 2326 C.1.1）。
//!
//! userinfo（`user:pass@`）は [`Credentials`] として取り出し、要求に使う URL からは取り除く。

use crate::auth::Credentials;
use crate::error::{Error, Result};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scheme {
    Rtsp,
    Rtsps,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Rtsp => "rtsp",
            Scheme::Rtsps => "rtsps",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Rtsp => 554,
            Scheme::Rtsps => 322,
        }
    }
}

/// 解釈した RTSP URL。Debug・Display に資格情報は出さない。
#[derive(Clone, PartialEq, Eq)]
pub struct RtspUrl {
    pub scheme: Scheme,
    /// ホスト名または IP（IPv6 は角括弧なし）。
    pub host: String,
    pub port: u16,
    /// ポートを明示していたか。
    pub explicit_port: bool,
    /// `/` で始まるパスとクエリ（空もありうる）。
    pub path: String,
    pub credentials: Option<Credentials>,
}

/// URL の最大長。
pub const MAX_URL_LEN: usize = 4096;

impl RtspUrl {
    pub fn parse(s: &str) -> Result<RtspUrl> {
        if s.len() > MAX_URL_LEN {
            return Err(Error::LimitExceeded("URL の長さ"));
        }
        if s.bytes().any(|b| b <= b' ' || b == 0x7f) {
            return Err(Error::Malformed("URL に空白・制御文字"));
        }
        let (scheme, rest) = s
            .split_once("://")
            .ok_or(Error::Malformed("URL の scheme"))?;
        let scheme = match scheme.to_ascii_lowercase().as_str() {
            "rtsp" => Scheme::Rtsp,
            "rtsps" => Scheme::Rtsps,
            _ => return Err(Error::Unsupported("URL の scheme")),
        };
        let split = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, path) = rest.split_at(split);
        let path = path.split('#').next().unwrap_or("").to_string();
        let (userinfo, hostport) = match authority.rfind('@') {
            Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
            None => (None, authority),
        };
        let credentials = match userinfo {
            None => None,
            Some(ui) => {
                let (u, p) = ui.split_once(':').unwrap_or((ui, ""));
                Some(Credentials::new(percent_decode(u)?, percent_decode(p)?))
            }
        };
        let (host, port) = if let Some(r) = hostport.strip_prefix('[') {
            let (h, after) = r.split_once(']').ok_or(Error::Malformed("IPv6 のホスト"))?;
            if h.is_empty()
                || !h
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.' || b == b'%')
            {
                return Err(Error::Malformed("IPv6 のホスト"));
            }
            let port = match after {
                "" => None,
                p => Some(p.strip_prefix(':').ok_or(Error::Malformed("ポート"))?),
            };
            (h.to_string(), port)
        } else {
            let (h, p) = match hostport.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (hostport, None),
            };
            if h.is_empty()
                || !h
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
            {
                return Err(Error::Malformed("ホスト"));
            }
            (h.to_ascii_lowercase(), p)
        };
        let (port, explicit_port) = match port {
            None | Some("") => (scheme.default_port(), false),
            Some(p) => {
                if p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(Error::Malformed("ポート"));
                }
                let n: u32 = p.parse().map_err(|_| Error::Malformed("ポート"))?;
                if n == 0 || n > 65535 {
                    return Err(Error::Malformed("ポート"));
                }
                (n as u16, true)
            }
        };
        Ok(RtspUrl {
            scheme,
            host,
            port,
            explicit_port,
            path,
            credentials,
        })
    }

    /// 接続先（`host:port`。IPv6 は角括弧付き）。
    pub fn host_port(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// 要求に使う URL（userinfo なし）。
    pub fn request_url(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let port = if self.explicit_port {
            format!(":{}", self.port)
        } else {
            String::new()
        };
        format!("{}://{host}{port}{}", self.scheme.as_str(), self.path)
    }

    /// 同じ接続先（scheme・ホスト・ポート）か。
    pub fn same_origin(&self, other: &RtspUrl) -> bool {
        self.scheme == other.scheme && self.host == other.host && self.port == other.port
    }
}

impl fmt::Debug for RtspUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RtspUrl")
            .field("url", &self.request_url())
            .field("has_credentials", &self.credentials.is_some())
            .finish()
    }
}

impl fmt::Display for RtspUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.request_url())
    }
}

fn percent_decode(s: &str) -> Result<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = b.get(i + 1..i + 3).ok_or(Error::Malformed("% 符号化"))?;
            let h = std::str::from_utf8(h).map_err(|_| Error::Malformed("% 符号化"))?;
            out.push(u8::from_str_radix(h, 16).map_err(|_| Error::Malformed("% 符号化"))?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| Error::Malformed("UTF-8 ではない userinfo"))
}

/// URL から userinfo を取り除く（解釈できなければそのまま返す）。
pub fn strip_userinfo(url: &str) -> String {
    match RtspUrl::parse(url) {
        Ok(u) => u.request_url(),
        Err(_) => url.to_string(),
    }
}

fn is_absolute(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.starts_with("rtsp://") || l.starts_with("rtsps://") || l.starts_with("rtspu://")
}

/// SDP の `a=control` を基底 URL に対して解決する。
///
/// - 空・`*`: 基底 URL そのもの（集約制御）
/// - 絶対 URL: そのまま（userinfo は除く）
/// - `/` で始まる: 基底の scheme・ホスト・ポートに付ける
/// - それ以外: 基底が `/` で終わらなければ `/` を足してから続ける（FFmpeg・live555 と同じ扱い）
pub fn resolve_control(base: &str, control: &str) -> String {
    let control = control.trim();
    if control.is_empty() || control == "*" {
        return base.to_string();
    }
    if is_absolute(control) {
        return strip_userinfo(control);
    }
    if control.starts_with('/') {
        if let Ok(b) = RtspUrl::parse(base) {
            let mut u = b.clone();
            u.path = control.to_string();
            u.credentials = None;
            return u.request_url();
        }
        return control.to_string();
    }
    if base.ends_with('/') {
        format!("{base}{control}")
    } else {
        format!("{base}/{control}")
    }
}

/// DESCRIBE の応答から制御 URL の基底を決める（Content-Base > Content-Location > 要求 URL）。
pub fn content_base(
    request_url: &str,
    content_base: Option<&str>,
    content_location: Option<&str>,
) -> String {
    let pick = content_base
        .or(content_location)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match pick {
        None => request_url.to_string(),
        Some(s) if is_absolute(s) => strip_userinfo(s),
        Some(s) => resolve_control(request_url, s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_urls() {
        let u = RtspUrl::parse("rtsp://admin:p%40ss@Cam.Local:8554/live/ch1?x=1").unwrap();
        assert_eq!(u.host, "cam.local");
        assert_eq!(u.port, 8554);
        assert_eq!(u.path, "/live/ch1?x=1");
        let c = u.credentials.as_ref().unwrap();
        assert_eq!((c.username.as_str(), c.password()), ("admin", "p@ss"));
        assert_eq!(u.request_url(), "rtsp://cam.local:8554/live/ch1?x=1");
        assert!(!format!("{u:?}").contains("p@ss"));
        let u = RtspUrl::parse("rtsp://[::1]/a").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 554));
        assert_eq!(u.host_port(), "[::1]:554");
        assert_eq!(u.request_url(), "rtsp://[::1]/a");
        let u = RtspUrl::parse("rtsps://h").unwrap();
        assert_eq!((u.port, u.path.as_str()), (322, ""));
        for bad in [
            "http://h/",
            "rtsp://",
            "rtsp://h:0/",
            "rtsp://h:99999/",
            "rtsp://h:x/",
            "rtsp://h a/",
            "rtsp://[zz]/",
            "rtsp://u:%zz@h/",
            "rtsp://h/\r\n",
        ] {
            assert!(RtspUrl::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn control_resolution() {
        let b = "rtsp://h:554/live";
        assert_eq!(
            resolve_control(b, "trackID=1"),
            "rtsp://h:554/live/trackID=1"
        );
        assert_eq!(
            resolve_control("rtsp://h/live/", "trackID=1"),
            "rtsp://h/live/trackID=1"
        );
        assert_eq!(resolve_control(b, "*"), b);
        assert_eq!(resolve_control(b, ""), b);
        assert_eq!(
            resolve_control(b, "rtsp://u:p@other/x/track1"),
            "rtsp://other/x/track1"
        );
        assert_eq!(resolve_control(b, "/abs/track2"), "rtsp://h:554/abs/track2");
        assert_eq!(
            content_base("rtsp://h/a", Some("rtsp://h/a/"), Some("rtsp://h/b")),
            "rtsp://h/a/"
        );
        assert_eq!(
            content_base("rtsp://h/a", None, Some("rtsp://h/b/")),
            "rtsp://h/b/"
        );
        assert_eq!(content_base("rtsp://h/a", None, None), "rtsp://h/a");
        assert_eq!(
            content_base("rtsp://h/a", Some("sub/"), None),
            "rtsp://h/a/sub/"
        );
    }
}
