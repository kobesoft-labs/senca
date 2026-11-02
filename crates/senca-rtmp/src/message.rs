//! message の種類と、protocol control・user control・AMF0 command の読み書き。

use crate::Error;
use bytes::Bytes;
use senca_amf0::{Decoder, Limits, Value};

/// message type id。
pub mod type_id {
    pub const SET_CHUNK_SIZE: u8 = 1;
    pub const ABORT: u8 = 2;
    pub const ACKNOWLEDGEMENT: u8 = 3;
    pub const USER_CONTROL: u8 = 4;
    pub const WINDOW_ACK_SIZE: u8 = 5;
    pub const SET_PEER_BANDWIDTH: u8 = 6;
    pub const AUDIO: u8 = 8;
    pub const VIDEO: u8 = 9;
    pub const DATA_AMF3: u8 = 15;
    pub const SHARED_OBJECT_AMF3: u8 = 16;
    pub const COMMAND_AMF3: u8 = 17;
    pub const DATA_AMF0: u8 = 18;
    pub const SHARED_OBJECT_AMF0: u8 = 19;
    pub const COMMAND_AMF0: u8 = 20;
    pub const AGGREGATE: u8 = 22;
}

/// 送るときの chunk stream id。
pub mod csid {
    pub const CONTROL: u32 = 2;
    pub const COMMAND: u32 = 3;
    pub const AUDIO: u32 = 4;
    pub const DATA: u32 = 5;
    pub const VIDEO: u32 = 6;
    pub const STREAM_COMMAND: u32 = 8;
}

/// user control の event type。
pub mod user_control {
    pub const STREAM_BEGIN: u16 = 0;
    pub const STREAM_EOF: u16 = 1;
    pub const STREAM_DRY: u16 = 2;
    pub const SET_BUFFER_LENGTH: u16 = 3;
    pub const STREAM_IS_RECORDED: u16 = 4;
    pub const PING_REQUEST: u16 = 6;
    pub const PING_RESPONSE: u16 = 7;
}

/// 音声か映像か（RTMP の message type 8・9）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaType {
    Audio,
    Video,
}

impl MediaType {
    pub fn type_id(self) -> u8 {
        match self {
            MediaType::Audio => type_id::AUDIO,
            MediaType::Video => type_id::VIDEO,
        }
    }

    pub fn csid(self) -> u32 {
        match self {
            MediaType::Audio => csid::AUDIO,
            MediaType::Video => csid::VIDEO,
        }
    }
}

/// 音声・映像の message（payload は FLV のタグ本体と同じ形）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaMessage {
    pub media_type: MediaType,
    /// ミリ秒（32 bit で折り返す）。
    pub timestamp: u32,
    pub payload: Bytes,
}

/// user control の内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserControl {
    StreamBegin(u32),
    StreamEof(u32),
    StreamDry(u32),
    SetBufferLength { stream_id: u32, millis: u32 },
    StreamIsRecorded(u32),
    PingRequest(u32),
    PingResponse(u32),
    Other(u16),
}

impl UserControl {
    pub fn parse(p: &[u8]) -> Result<UserControl, Error> {
        let short = Error::Protocol("short user control message");
        if p.len() < 2 {
            return Err(short);
        }
        let ev = u16::from_be_bytes([p[0], p[1]]);
        let arg = |i: usize| -> Result<u32, Error> {
            p.get(i..i + 4)
                .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
                .ok_or(Error::Protocol("short user control message"))
        };
        Ok(match ev {
            user_control::STREAM_BEGIN => UserControl::StreamBegin(arg(2)?),
            user_control::STREAM_EOF => UserControl::StreamEof(arg(2)?),
            user_control::STREAM_DRY => UserControl::StreamDry(arg(2)?),
            user_control::SET_BUFFER_LENGTH => UserControl::SetBufferLength {
                stream_id: arg(2)?,
                millis: arg(6)?,
            },
            user_control::STREAM_IS_RECORDED => UserControl::StreamIsRecorded(arg(2)?),
            user_control::PING_REQUEST => UserControl::PingRequest(arg(2)?),
            user_control::PING_RESPONSE => UserControl::PingResponse(arg(2)?),
            other => UserControl::Other(other),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let (ev, a, b) = match *self {
            UserControl::StreamBegin(s) => (user_control::STREAM_BEGIN, s, None),
            UserControl::StreamEof(s) => (user_control::STREAM_EOF, s, None),
            UserControl::StreamDry(s) => (user_control::STREAM_DRY, s, None),
            UserControl::SetBufferLength { stream_id, millis } => {
                (user_control::SET_BUFFER_LENGTH, stream_id, Some(millis))
            }
            UserControl::StreamIsRecorded(s) => (user_control::STREAM_IS_RECORDED, s, None),
            UserControl::PingRequest(t) => (user_control::PING_REQUEST, t, None),
            UserControl::PingResponse(t) => (user_control::PING_RESPONSE, t, None),
            UserControl::Other(e) => (e, 0, None),
        };
        let mut v = ev.to_be_bytes().to_vec();
        v.extend_from_slice(&a.to_be_bytes());
        if let Some(b) = b {
            v.extend_from_slice(&b.to_be_bytes());
        }
        v
    }
}

/// 4 バイトの値だけを持つ protocol control message を読む。
pub fn parse_u32(p: &[u8]) -> Result<u32, Error> {
    p.get(..4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or(Error::Protocol("short protocol control message"))
}

/// AMF0 command（`名前, transaction id, command object, 引数...`）。
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub name: String,
    pub transaction_id: f64,
    /// command object（ない・null なら `Value::Null`）。
    pub object: Value,
    pub args: Vec<Value>,
}

impl Command {
    pub fn new(name: &str, transaction_id: f64, object: Value, args: Vec<Value>) -> Self {
        Command {
            name: name.to_owned(),
            transaction_id,
            object,
            args,
        }
    }

    /// AMF0 command の payload を読む。AMF3 command（type 17）は先頭 1 バイト（0）を外してから渡す。
    pub fn parse(payload: &[u8], limits: Limits) -> Result<Command, Error> {
        let mut d = Decoder::with_limits(payload, limits)?;
        let name = match d.next_value()? {
            Some(Value::String(s)) => s,
            _ => return Err(Error::Protocol("command name is not a string")),
        };
        // transaction id と command object は省略する実装がある（onStatus 等）ので緩く読む。
        let transaction_id = match d.next_value()? {
            Some(Value::Number(n)) => n,
            None => 0.0,
            Some(_) => return Err(Error::Protocol("transaction id is not a number")),
        };
        let object = d.next_value()?.unwrap_or(Value::Null);
        let args = d.read_all()?;
        Ok(Command {
            name,
            transaction_id,
            object,
            args,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        senca_amf0::encode(&Value::String(self.name.clone()), &mut out)?;
        senca_amf0::encode(&Value::Number(self.transaction_id), &mut out)?;
        senca_amf0::encode(&self.object, &mut out)?;
        for a in &self.args {
            senca_amf0::encode(a, &mut out)?;
        }
        Ok(out)
    }

    /// n 番目の引数（command object の次から 0 番）。
    pub fn arg(&self, n: usize) -> Option<&Value> {
        self.args.get(n)
    }
}

/// onStatus・_result の info object を作る。
pub fn status_object(level: &str, code: &str, description: &str) -> Value {
    Value::object([
        ("level", Value::string(level)),
        ("code", Value::string(code)),
        ("description", Value::string(description)),
    ])
}

/// AMF0 data message の payload の先頭の文字列を読み、その終わりの位置を返す。
pub(crate) fn data_name(payload: &[u8]) -> Option<(String, usize)> {
    let mut d = Decoder::with_limits(payload, Limits::default()).ok()?;
    match d.next_value().ok()? {
        Some(Value::String(s)) => Some((s, d.position())),
        _ => None,
    }
}

/// aggregate message を分解する（中身は FLV のタグ列。時刻は先頭タグを message の時刻に合わせて補正）。
pub fn split_aggregate(timestamp: u32, payload: &Bytes) -> Result<Vec<(u8, u32, Bytes)>, Error> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut base: Option<u32> = None;
    while pos < payload.len() {
        let h = payload
            .get(pos..pos + 11)
            .ok_or(Error::Protocol("truncated aggregate message"))?;
        let t = h[0] & 0x1F;
        let size = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        let ts = u32::from_be_bytes([h[7], h[4], h[5], h[6]]);
        let start = pos + 11;
        let end = start + size;
        if end + 4 > payload.len() {
            return Err(Error::Protocol("truncated aggregate message"));
        }
        let b = *base.get_or_insert(ts);
        out.push((
            t,
            timestamp.wrapping_add(ts.wrapping_sub(b)),
            payload.slice(start..end),
        ));
        pos = end + 4;
        if out.len() > 4096 {
            return Err(Error::LimitExceeded("aggregate sub-messages"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_roundtrip() {
        let c = Command::new(
            "connect",
            1.0,
            Value::object([("app", Value::string("live"))]),
            vec![Value::Null],
        );
        let b = c.encode().unwrap();
        assert_eq!(Command::parse(&b, Limits::default()).unwrap(), c);
        assert!(Command::parse(&[0x05], Limits::default()).is_err());
    }

    #[test]
    fn user_control_roundtrip() {
        for u in [
            UserControl::StreamBegin(1),
            UserControl::PingRequest(99),
            UserControl::SetBufferLength {
                stream_id: 1,
                millis: 3000,
            },
        ] {
            assert_eq!(UserControl::parse(&u.encode()).unwrap(), u);
        }
        assert!(UserControl::parse(&[0, 3, 0, 0, 0, 1]).is_err());
        assert!(UserControl::parse(&[0]).is_err());
    }

    #[test]
    fn aggregate() {
        let mut p = Vec::new();
        for (t, ts, body) in [(9u8, 1000u32, &b"ab"[..]), (8, 1020, &b"c"[..])] {
            p.push(t);
            p.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
            p.extend_from_slice(&ts.to_be_bytes()[1..]);
            p.push((ts >> 24) as u8);
            p.extend_from_slice(&[0, 0, 0]);
            p.extend_from_slice(body);
            p.extend_from_slice(&(11 + body.len() as u32).to_be_bytes());
        }
        let v = split_aggregate(5000, &Bytes::from(p.clone())).unwrap();
        assert_eq!(v[0], (9, 5000, Bytes::from_static(b"ab")));
        assert_eq!(v[1], (8, 5020, Bytes::from_static(b"c")));
        for i in 1..p.len() {
            let _ = split_aggregate(0, &Bytes::copy_from_slice(&p[..i]));
        }
    }
}
