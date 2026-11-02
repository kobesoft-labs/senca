//! field（ヘッダー）と HTTP/3 のメッセージの検査（RFC 9114 4.2・4.3）。

/// 一つの field line。名前・値はバイト列（値は UTF-8 とは限らない）。
#[derive(Clone, PartialEq, Eq)]
pub struct Field {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

impl Field {
    pub fn new(name: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Self {
        Self {
            name: name.as_ref().to_vec(),
            value: value.as_ref().to_vec(),
        }
    }
}

impl std::fmt::Debug for Field {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 秘密になりうる値（authorization・cookie）は伏せる
        let n = String::from_utf8_lossy(&self.name);
        if matches!(&*n, "authorization" | "cookie" | "set-cookie" | "proxy-authorization") {
            write!(f, "{n}: <伏せ字>")
        } else {
            write!(f, "{n}: {}", String::from_utf8_lossy(&self.value))
        }
    }
}

/// field の並び。
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Headers(Vec<Field>);

impl std::fmt::Debug for Headers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.0.iter()).finish()
    }
}

impl Headers {
    pub fn new() -> Self {
        Self(Vec::new())
    }
    pub fn push(&mut self, f: Field) {
        self.0.push(f);
    }
    /// 名前と値を足す（builder 形式）。
    pub fn with(mut self, name: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Self {
        self.0.push(Field::new(name, value));
        self
    }
    pub fn iter(&self) -> impl Iterator<Item = &Field> {
        self.0.iter()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// 最初に一致した値。
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.0.iter().find(|f| f.name == name.as_bytes()).map(|f| f.value.as_slice())
    }
    /// UTF-8 の値。
    pub fn get_str(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(|v| std::str::from_utf8(v).ok())
    }
    /// :status を数値で。
    pub fn status(&self) -> Option<u16> {
        self.get_str(":status").and_then(|s| s.parse().ok())
    }
}

/// メッセージの種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Request,
    Response,
    Trailers,
}

const CONNECTION_SPECIFIC: [&str; 5] = ["connection", "keep-alive", "proxy-connection", "transfer-encoding", "upgrade"];

/// field の並びが正しいか（RFC 9114 4.2・4.3）。誤りは H3_MESSAGE_ERROR（stream エラー）。
pub(crate) fn validate(h: &Headers, kind: Kind) -> Result<(), &'static str> {
    let mut regular_seen = false;
    let mut seen: Vec<&[u8]> = Vec::new();
    for f in h.iter() {
        if f.name.is_empty() {
            return Err("空の名前");
        }
        if f.name.iter().any(|&c| c.is_ascii_uppercase() || c <= 0x20 || c == 0x7f || c == b':' && f.name[0] != b':') {
            return Err("名前に使えない文字");
        }
        if f.value.iter().any(|&c| c == 0 || c == b'\r' || c == b'\n') {
            return Err("値に使えない文字");
        }
        if f.name[0] == b':' {
            if regular_seen || kind == Kind::Trailers {
                return Err("疑似ヘッダーが通常の field の後にある");
            }
            let ok = match kind {
                Kind::Request => matches!(&f.name[..], b":method" | b":scheme" | b":authority" | b":path" | b":protocol"),
                Kind::Response => &f.name[..] == b":status",
                Kind::Trailers => false,
            };
            if !ok {
                return Err("未知・不適切な疑似ヘッダー");
            }
            if seen.contains(&&f.name[..]) {
                return Err("疑似ヘッダーの重複");
            }
            seen.push(&f.name);
        } else {
            regular_seen = true;
            let n = std::str::from_utf8(&f.name).unwrap_or("");
            if CONNECTION_SPECIFIC.contains(&n) {
                return Err("接続固有の field");
            }
            if n == "te" && f.value != b"trailers" {
                return Err("TE は trailers だけ");
            }
        }
    }
    match kind {
        Kind::Request => {
            let method = h.get(":method").ok_or(":method がない")?;
            let protocol = h.get(":protocol");
            if method == b"CONNECT" {
                if h.get(":authority").is_none_or(|a| a.is_empty()) {
                    return Err("CONNECT に :authority がない");
                }
                if protocol.is_some() {
                    // 拡張 CONNECT（RFC 9220）: :scheme と :path も要る
                    if h.get(":scheme").is_none() || h.get(":path").is_none_or(|p| p.is_empty()) {
                        return Err("拡張 CONNECT に :scheme・:path がない");
                    }
                } else if h.get(":scheme").is_some() || h.get(":path").is_some() {
                    return Err("CONNECT に :scheme・:path がある");
                }
            } else {
                if protocol.is_some() {
                    return Err("CONNECT 以外に :protocol");
                }
                if h.get(":scheme").is_none() || h.get(":path").is_none_or(|p| p.is_empty()) {
                    return Err(":scheme・:path がない");
                }
            }
        }
        Kind::Response => {
            let s = h.get_str(":status").ok_or(":status がない")?;
            if s.len() != 3 || !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err(":status が不正");
            }
        }
        Kind::Trailers => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_validation() {
        let ok = Headers::new()
            .with(":method", "GET")
            .with(":scheme", "https")
            .with(":authority", "x")
            .with(":path", "/");
        assert!(validate(&ok, Kind::Request).is_ok());
        let wt = Headers::new()
            .with(":method", "CONNECT")
            .with(":protocol", "webtransport")
            .with(":scheme", "https")
            .with(":authority", "x")
            .with(":path", "/wt");
        assert!(validate(&wt, Kind::Request).is_ok());
        assert!(validate(&ok.clone().with("Upper", "x"), Kind::Request).is_err());
        assert!(validate(&ok.clone().with("connection", "close"), Kind::Request).is_err());
        assert!(validate(&ok.clone().with(":path", "/again"), Kind::Request).is_err());
        assert!(validate(&Headers::new().with("a", "b").with(":method", "GET"), Kind::Request).is_err());
        assert!(validate(&Headers::new().with(":status", "2000"), Kind::Response).is_err());
        assert!(validate(&ok.with("x", "a\r\nb"), Kind::Request).is_err());
    }
}
