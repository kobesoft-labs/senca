//! Server-Sent Events の frame（HTML Living Standard 9.2）。`GET /events` 用。
//!
//! 応答は `Content-Type: text/event-stream` で chunked（または Close）にし、frame を chunk として送る。

use crate::error::HttpError;
use bytes::Bytes;

/// 1 つの event。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent<'a> {
    /// `event:`（省略時は message）。改行を含めない。
    pub event: Option<&'a str>,
    /// `id:`。改行・NUL を含めない。
    pub id: Option<&'a str>,
    /// `retry:`（ミリ秒）。
    pub retry: Option<u64>,
    /// `data:`。改行（LF・CRLF・CR）で複数の data 行に分ける。
    pub data: &'a str,
}

impl SseEvent<'_> {
    pub fn encode(&self) -> Result<Bytes, HttpError> {
        let mut out = String::with_capacity(self.data.len() + 32);
        if let Some(e) = self.event {
            if e.contains(['\r', '\n']) {
                return Err(HttpError::InvalidInput("sse event name"));
            }
            out.push_str("event: ");
            out.push_str(e);
            out.push('\n');
        }
        if let Some(id) = self.id {
            if id.contains(['\r', '\n', '\0']) {
                return Err(HttpError::InvalidInput("sse id"));
            }
            out.push_str("id: ");
            out.push_str(id);
            out.push('\n');
        }
        if let Some(r) = self.retry {
            out.push_str(&format!("retry: {r}\n"));
        }
        let data = self.data.replace("\r\n", "\n").replace('\r', "\n");
        for line in data.split('\n') {
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        Ok(Bytes::from(out))
    }
}

/// 接続維持用の comment 行（`: keepalive`）。
pub fn keepalive() -> Bytes {
    Bytes::from_static(b": keepalive\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode() {
        let e = SseEvent { event: Some("stream.state"), id: Some("42"), retry: Some(3000), data: "{\"a\":1}\r\nline2" };
        assert_eq!(&e.encode().unwrap()[..], b"event: stream.state\nid: 42\nretry: 3000\ndata: {\"a\":1}\ndata: line2\n\n");
        assert_eq!(&SseEvent { data: "", ..Default::default() }.encode().unwrap()[..], b"data: \n\n");
        assert!(SseEvent { event: Some("a\nb"), ..Default::default() }.encode().is_err());
    }
}
