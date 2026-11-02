//! 制御メッセージ（draft-22 9 章）。`Type (vi64) + Length (16) + Body`。

use bytes::Bytes;

use crate::coding::{Reader, put_bytes, put_varint};
use crate::error::MoqError;
use crate::params::{Params, SetupOptions, TrackProperties, param};
use crate::{Location, Namespace};

pub mod ty {
    pub const SETUP: u64 = 0x2f00;
    pub const GOAWAY: u64 = 0x10;
    pub const SUBSCRIBE: u64 = 0x3;
    pub const SUBSCRIBE_OK: u64 = 0x4;
    pub const PUBLISH_STATE_NOTIFY: u64 = 0x22;
    pub const PUBLISH: u64 = 0x1d;
    pub const PUBLISH_DONE: u64 = 0xb;
    pub const FETCH: u64 = 0x16;
    pub const FETCH_OK: u64 = 0x18;
    pub const TRACK_STATUS: u64 = 0xd;
    pub const PUBLISH_NAMESPACE: u64 = 0x6;
    pub const SUBSCRIBE_NAMESPACE: u64 = 0x50;
    pub const SUBSCRIBE_TRACKS: u64 = 0x51;
    pub const NAMESPACE: u64 = 0x8;
    pub const NAMESPACE_DONE: u64 = 0xe;
    pub const PUBLISH_SKIPPED: u64 = 0xf;
    pub const REQUEST_UPDATE: u64 = 0x2;
    pub const REQUEST_OK: u64 = 0x7;
    pub const REQUEST_ERROR: u64 = 0x5;
}

/// Track Namespace の要素数の上限（8.7）。
pub const MAX_NAMESPACE_FIELDS: usize = 32;
/// Full Track Name の長さの上限（8.7）。
pub const MAX_FULL_TRACK_NAME: usize = 4096;
/// 理由の文字列の上限（8.5）。
pub const MAX_REASON: usize = 1024;
/// GOAWAY の URI の上限（9.2）。
pub const MAX_GOAWAY_URI: usize = 8192;

pub fn put_namespace(out: &mut Vec<u8>, ns: &Namespace) {
    put_varint(out, ns.0.len() as u64);
    for f in &ns.0 {
        put_bytes(out, f);
    }
}

pub(crate) fn get_namespace(r: &mut Reader) -> Result<Namespace, MoqError> {
    let e = || MoqError::proto("Track Namespace が切れている");
    let n = r.varint().map_err(|_| e())?;
    if n as usize > MAX_NAMESPACE_FIELDS {
        return Err(MoqError::proto("Track Namespace の要素が 32 を超える"));
    }
    let mut v = Vec::with_capacity(n as usize);
    let mut total = 0;
    for _ in 0..n {
        let f = r.len_bytes(MAX_FULL_TRACK_NAME).map_err(|_| e())?;
        if f.is_empty() {
            return Err(MoqError::proto("Track Namespace の要素が空"));
        }
        total += f.len();
        if total > MAX_FULL_TRACK_NAME {
            return Err(MoqError::proto("Track Namespace が 4096 バイトを超える"));
        }
        v.push(f);
    }
    Ok(Namespace(v))
}

fn get_track_name(r: &mut Reader, ns: &Namespace) -> Result<Bytes, MoqError> {
    let name = r
        .len_bytes(MAX_FULL_TRACK_NAME)
        .map_err(|_| MoqError::proto("Track Name が切れている"))?;
    if ns.total_len() + name.len() > MAX_FULL_TRACK_NAME {
        return Err(MoqError::proto("Full Track Name が 4096 バイトを超える"));
    }
    Ok(name)
}

fn get_reason(r: &mut Reader) -> Result<String, MoqError> {
    let b = r
        .len_bytes(MAX_REASON)
        .map_err(|_| MoqError::proto("Reason Phrase が切れている・長すぎる"))?;
    // 受信した文字列はログに出す前に安全な形にする（15.9）。UTF-8 でなければ置き換える。
    Ok(String::from_utf8_lossy(&b).into_owned())
}

fn put_reason(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    put_bytes(out, &b[..b.len().min(MAX_REASON)]);
}

/// REQUEST_ERROR の Redirect（9.4.1）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redirect {
    pub uri: Bytes,
    pub namespace: Namespace,
    pub track: Bytes,
}

/// 制御メッセージ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Setup(SetupOptions),
    GoAway {
        uri: Bytes,
        timeout_ms: u64,
    },
    RequestOk {
        params: Params,
        properties: Bytes,
    },
    RequestError {
        code: u64,
        retry_interval: u64,
        reason: String,
        redirect: Option<Redirect>,
    },
    RequestUpdate {
        request_id: u64,
        params: Params,
    },
    Subscribe {
        request_id: u64,
        namespace: Namespace,
        track: Bytes,
        params: Params,
    },
    SubscribeOk {
        track_alias: u64,
        params: Params,
        properties: Bytes,
    },
    Publish {
        request_id: u64,
        namespace: Namespace,
        track: Bytes,
        track_alias: u64,
        params: Params,
        properties: Bytes,
    },
    PublishDone {
        status: u64,
        stream_count: u64,
        reason: String,
    },
    PublishStateNotify {
        params: Params,
    },
    Fetch {
        request_id: u64,
        namespace: Namespace,
        track: Bytes,
        params: Params,
    },
    FetchOk {
        end_of_track: bool,
        end: Location,
        params: Params,
        properties: Bytes,
    },
    TrackStatus {
        request_id: u64,
        namespace: Namespace,
        track: Bytes,
        params: Params,
    },
    PublishNamespace {
        request_id: u64,
        prefix: Namespace,
        params: Params,
    },
    SubscribeNamespace {
        request_id: u64,
        prefix: Namespace,
        params: Params,
    },
    SubscribeTracks {
        request_id: u64,
        prefix: Namespace,
        params: Params,
    },
    Namespace {
        suffix: Namespace,
    },
    NamespaceDone {
        suffix: Namespace,
    },
    PublishSkipped {
        suffix: Namespace,
        track: Bytes,
    },
}

impl Message {
    pub fn type_id(&self) -> u64 {
        match self {
            Self::Setup(_) => ty::SETUP,
            Self::GoAway { .. } => ty::GOAWAY,
            Self::RequestOk { .. } => ty::REQUEST_OK,
            Self::RequestError { .. } => ty::REQUEST_ERROR,
            Self::RequestUpdate { .. } => ty::REQUEST_UPDATE,
            Self::Subscribe { .. } => ty::SUBSCRIBE,
            Self::SubscribeOk { .. } => ty::SUBSCRIBE_OK,
            Self::Publish { .. } => ty::PUBLISH,
            Self::PublishDone { .. } => ty::PUBLISH_DONE,
            Self::PublishStateNotify { .. } => ty::PUBLISH_STATE_NOTIFY,
            Self::Fetch { .. } => ty::FETCH,
            Self::FetchOk { .. } => ty::FETCH_OK,
            Self::TrackStatus { .. } => ty::TRACK_STATUS,
            Self::PublishNamespace { .. } => ty::PUBLISH_NAMESPACE,
            Self::SubscribeNamespace { .. } => ty::SUBSCRIBE_NAMESPACE,
            Self::SubscribeTracks { .. } => ty::SUBSCRIBE_TRACKS,
            Self::Namespace { .. } => ty::NAMESPACE,
            Self::NamespaceDone { .. } => ty::NAMESPACE_DONE,
            Self::PublishSkipped { .. } => ty::PUBLISH_SKIPPED,
        }
    }

    /// 要求 stream の最初に置けるメッセージか（6.3）。
    pub fn is_request(&self) -> bool {
        matches!(
            self,
            Self::Subscribe { .. }
                | Self::Publish { .. }
                | Self::Fetch { .. }
                | Self::TrackStatus { .. }
                | Self::PublishNamespace { .. }
                | Self::SubscribeNamespace { .. }
                | Self::SubscribeTracks { .. }
        )
    }

    /// 要求の Request ID。
    pub fn request_id(&self) -> Option<u64> {
        match self {
            Self::Subscribe { request_id, .. }
            | Self::Publish { request_id, .. }
            | Self::Fetch { request_id, .. }
            | Self::TrackStatus { request_id, .. }
            | Self::PublishNamespace { request_id, .. }
            | Self::SubscribeNamespace { request_id, .. }
            | Self::SubscribeTracks { request_id, .. }
            | Self::RequestUpdate { request_id, .. } => Some(*request_id),
            _ => None,
        }
    }

    /// 枠（型・長さ）つきで書く。本文が 65535 バイトを超えるときは `None`。
    pub fn encode(&self) -> Option<Vec<u8>> {
        let mut b = Vec::new();
        match self {
            Self::Setup(s) => b.extend_from_slice(&s.encode()),
            Self::GoAway { uri, timeout_ms } => {
                put_bytes(&mut b, uri);
                put_varint(&mut b, *timeout_ms);
            }
            Self::RequestOk { params, properties } => {
                params.encode(&mut b);
                b.extend_from_slice(properties);
            }
            Self::RequestError {
                code,
                retry_interval,
                reason,
                redirect,
            } => {
                put_varint(&mut b, *code);
                put_varint(&mut b, *retry_interval);
                put_reason(&mut b, reason);
                if let Some(r) = redirect {
                    put_bytes(&mut b, &r.uri);
                    put_namespace(&mut b, &r.namespace);
                    put_bytes(&mut b, &r.track);
                }
            }
            Self::RequestUpdate { request_id, params } => {
                put_varint(&mut b, *request_id);
                params.encode(&mut b);
            }
            Self::Subscribe {
                request_id,
                namespace,
                track,
                params,
            }
            | Self::Fetch {
                request_id,
                namespace,
                track,
                params,
            }
            | Self::TrackStatus {
                request_id,
                namespace,
                track,
                params,
            } => {
                put_varint(&mut b, *request_id);
                put_namespace(&mut b, namespace);
                put_bytes(&mut b, track);
                params.encode(&mut b);
            }
            Self::SubscribeOk {
                track_alias,
                params,
                properties,
            } => {
                put_varint(&mut b, *track_alias);
                params.encode(&mut b);
                b.extend_from_slice(properties);
            }
            Self::Publish {
                request_id,
                namespace,
                track,
                track_alias,
                params,
                properties,
            } => {
                put_varint(&mut b, *request_id);
                put_namespace(&mut b, namespace);
                put_bytes(&mut b, track);
                put_varint(&mut b, *track_alias);
                params.encode(&mut b);
                b.extend_from_slice(properties);
            }
            Self::PublishDone {
                status,
                stream_count,
                reason,
            } => {
                put_varint(&mut b, *status);
                put_varint(&mut b, *stream_count);
                put_reason(&mut b, reason);
            }
            Self::PublishStateNotify { params } => params.encode(&mut b),
            Self::FetchOk {
                end_of_track,
                end,
                params,
                properties,
            } => {
                b.push(u8::from(*end_of_track));
                put_varint(&mut b, end.group);
                put_varint(&mut b, end.object);
                params.encode(&mut b);
                b.extend_from_slice(properties);
            }
            Self::PublishNamespace {
                request_id,
                prefix,
                params,
            }
            | Self::SubscribeNamespace {
                request_id,
                prefix,
                params,
            }
            | Self::SubscribeTracks {
                request_id,
                prefix,
                params,
            } => {
                put_varint(&mut b, *request_id);
                put_namespace(&mut b, prefix);
                params.encode(&mut b);
            }
            Self::Namespace { suffix } | Self::NamespaceDone { suffix } => put_namespace(&mut b, suffix),
            Self::PublishSkipped { suffix, track } => {
                put_namespace(&mut b, suffix);
                put_bytes(&mut b, track);
            }
        }
        if b.len() > 0xffff {
            return None;
        }
        let mut out = Vec::with_capacity(b.len() + 4);
        put_varint(&mut out, self.type_id());
        out.extend_from_slice(&(b.len() as u16).to_be_bytes());
        out.extend_from_slice(&b);
        Some(out)
    }

    /// 本文を読む。本文は長さちょうどで使い切らなければならない（9 章）。
    pub fn decode_body(t: u64, body: Bytes, from_server: bool) -> Result<Self, MoqError> {
        let mut r = Reader::new(body);
        let e = |what: &str| MoqError::proto(format!("{what} が切れている"));
        let m = match t {
            ty::SETUP => {
                let rest = r.rest();
                Self::Setup(SetupOptions::decode(rest, from_server)?)
            }
            ty::GOAWAY => {
                let uri = r.len_bytes(MAX_GOAWAY_URI).map_err(|_| MoqError::proto("GOAWAY の URI が長すぎる"))?;
                if !from_server && !uri.is_empty() {
                    return Err(MoqError::proto("client の GOAWAY に URI がある"));
                }
                let timeout_ms = r.varint().map_err(|_| e("GOAWAY"))?;
                Self::GoAway { uri, timeout_ms }
            }
            ty::REQUEST_OK => {
                let params = Params::decode(&mut r, param::REQUEST_OK)?;
                let properties = r.rest();
                TrackProperties::parse(properties.clone())?;
                Self::RequestOk { params, properties }
            }
            ty::REQUEST_ERROR => {
                let code = r.varint().map_err(|_| e("REQUEST_ERROR"))?;
                let retry_interval = r.varint().map_err(|_| e("REQUEST_ERROR"))?;
                let reason = get_reason(&mut r)?;
                let redirect = if code == crate::RequestErrorCode::REDIRECT.0 {
                    let uri = r.len_bytes(MAX_GOAWAY_URI).map_err(|_| e("Redirect"))?;
                    if !from_server && !uri.is_empty() {
                        return Err(MoqError::proto("client の Redirect に URI がある"));
                    }
                    let namespace = get_namespace(&mut r)?;
                    let track = get_track_name(&mut r, &namespace)?;
                    Some(Redirect { uri, namespace, track })
                } else {
                    None
                };
                Self::RequestError {
                    code,
                    retry_interval,
                    reason,
                    redirect,
                }
            }
            ty::REQUEST_UPDATE => {
                let request_id = r.varint().map_err(|_| e("REQUEST_UPDATE"))?;
                let params = Params::decode(&mut r, param::REQUEST_UPDATE)?;
                Self::RequestUpdate { request_id, params }
            }
            ty::SUBSCRIBE | ty::FETCH | ty::TRACK_STATUS => {
                let request_id = r.varint().map_err(|_| e("要求"))?;
                let namespace = get_namespace(&mut r)?;
                let track = get_track_name(&mut r, &namespace)?;
                let allowed = match t {
                    ty::SUBSCRIBE => param::SUBSCRIBE,
                    ty::FETCH => param::FETCH,
                    _ => param::TRACK_STATUS,
                };
                let params = Params::decode(&mut r, allowed)?;
                match t {
                    ty::SUBSCRIBE => Self::Subscribe {
                        request_id,
                        namespace,
                        track,
                        params,
                    },
                    ty::FETCH => Self::Fetch {
                        request_id,
                        namespace,
                        track,
                        params,
                    },
                    _ => Self::TrackStatus {
                        request_id,
                        namespace,
                        track,
                        params,
                    },
                }
            }
            ty::SUBSCRIBE_OK => {
                let track_alias = r.varint().map_err(|_| e("SUBSCRIBE_OK"))?;
                let params = Params::decode(&mut r, param::SUBSCRIBE_OK)?;
                let properties = r.rest();
                TrackProperties::parse(properties.clone())?;
                Self::SubscribeOk {
                    track_alias,
                    params,
                    properties,
                }
            }
            ty::PUBLISH => {
                let request_id = r.varint().map_err(|_| e("PUBLISH"))?;
                let namespace = get_namespace(&mut r)?;
                let track = get_track_name(&mut r, &namespace)?;
                let track_alias = r.varint().map_err(|_| e("PUBLISH"))?;
                let params = Params::decode(&mut r, param::PUBLISH)?;
                let properties = r.rest();
                TrackProperties::parse(properties.clone())?;
                Self::Publish {
                    request_id,
                    namespace,
                    track,
                    track_alias,
                    params,
                    properties,
                }
            }
            ty::PUBLISH_DONE => Self::PublishDone {
                status: r.varint().map_err(|_| e("PUBLISH_DONE"))?,
                stream_count: r.varint().map_err(|_| e("PUBLISH_DONE"))?,
                reason: get_reason(&mut r)?,
            },
            ty::PUBLISH_STATE_NOTIFY => Self::PublishStateNotify {
                params: Params::decode(&mut r, param::PUBLISH_STATE_NOTIFY)?,
            },
            ty::FETCH_OK => {
                let eot = r.u8().map_err(|_| e("FETCH_OK"))?;
                if eot > 1 {
                    return Err(MoqError::proto("FETCH_OK の End Of Track が 0・1 でない"));
                }
                let end = Location::new(
                    r.varint().map_err(|_| e("FETCH_OK"))?,
                    r.varint().map_err(|_| e("FETCH_OK"))?,
                );
                let params = Params::decode(&mut r, param::FETCH_OK)?;
                let properties = r.rest();
                TrackProperties::parse(properties.clone())?;
                Self::FetchOk {
                    end_of_track: eot == 1,
                    end,
                    params,
                    properties,
                }
            }
            ty::PUBLISH_NAMESPACE | ty::SUBSCRIBE_NAMESPACE | ty::SUBSCRIBE_TRACKS => {
                let request_id = r.varint().map_err(|_| e("要求"))?;
                let prefix = get_namespace(&mut r)?;
                let allowed = match t {
                    ty::PUBLISH_NAMESPACE => param::PUBLISH_NAMESPACE,
                    ty::SUBSCRIBE_NAMESPACE => param::SUBSCRIBE_NAMESPACE,
                    _ => param::SUBSCRIBE_TRACKS,
                };
                let params = Params::decode(&mut r, allowed)?;
                match t {
                    ty::PUBLISH_NAMESPACE => Self::PublishNamespace {
                        request_id,
                        prefix,
                        params,
                    },
                    ty::SUBSCRIBE_NAMESPACE => Self::SubscribeNamespace {
                        request_id,
                        prefix,
                        params,
                    },
                    _ => Self::SubscribeTracks {
                        request_id,
                        prefix,
                        params,
                    },
                }
            }
            ty::NAMESPACE => Self::Namespace {
                suffix: get_namespace(&mut r)?,
            },
            ty::NAMESPACE_DONE => Self::NamespaceDone {
                suffix: get_namespace(&mut r)?,
            },
            ty::PUBLISH_SKIPPED => {
                let suffix = get_namespace(&mut r)?;
                let track = get_track_name(&mut r, &suffix)?;
                Self::PublishSkipped { suffix, track }
            }
            _ => return Err(MoqError::proto(format!("未知の制御メッセージ 0x{t:x}"))),
        };
        if !r.is_empty() {
            return Err(MoqError::proto(format!("制御メッセージ 0x{t:x} の長さが本文と合わない")));
        }
        Ok(m)
    }
}

/// stream から制御メッセージを一つずつ取り出す。
#[derive(Debug, Default)]
pub(crate) struct MessageReader {
    buf: bytes::BytesMut,
}

impl MessageReader {
    pub(crate) fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// 完全なメッセージの型と本文。
    pub(crate) fn next(&mut self) -> Result<Option<(u64, Bytes)>, MoqError> {
        let Some((t, n)) = crate::varint::read(&self.buf) else {
            if self.buf.len() > 9 {
                return Err(MoqError::proto("制御メッセージの型が読めない"));
            }
            return Ok(None);
        };
        if self.buf.len() < n + 2 {
            return Ok(None);
        }
        let len = u16::from_be_bytes([self.buf[n], self.buf[n + 1]]) as usize;
        if self.buf.len() < n + 2 + len {
            return Ok(None);
        }
        let _ = self.buf.split_to(n + 2);
        let body = self.buf.split_to(len).freeze();
        Ok(Some((t, body)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{GroupOrder, LocationFilter};

    fn roundtrip(m: Message, from_server: bool) {
        let enc = m.encode().unwrap();
        let mut r = MessageReader::default();
        // 1 バイトずつ入れても同じものが出る
        let mut got = None;
        for b in &enc {
            r.push(&[*b]);
            if let Some((t, body)) = r.next().unwrap() {
                got = Some(Message::decode_body(t, body, from_server).unwrap());
            }
        }
        assert_eq!(got.unwrap(), m);
    }

    #[test]
    fn messages_roundtrip() {
        let ns = Namespace::from_path("live/room1");
        roundtrip(
            Message::Subscribe {
                request_id: 2,
                namespace: ns.clone(),
                track: Bytes::from_static(b"video"),
                params: Params {
                    subscriber_priority: Some(10),
                    group_order: Some(GroupOrder::Ascending),
                    location_filter: Some(LocationFilter::RelativeStart(1)),
                    forward: Some(true),
                    ..Default::default()
                },
            },
            false,
        );
        roundtrip(
            Message::SubscribeOk {
                track_alias: 9,
                params: Params {
                    largest_object: Some(Location::new(3, 4)),
                    ..Default::default()
                },
                properties: Bytes::new(),
            },
            true,
        );
        roundtrip(
            Message::Publish {
                request_id: 1,
                namespace: ns.clone(),
                track: Bytes::from_static(b"audio"),
                track_alias: 3,
                params: Params::default(),
                properties: Bytes::from_static(&[0x0e, 0x05]),
            },
            true,
        );
        roundtrip(
            Message::RequestError {
                code: 0x10,
                retry_interval: 0,
                reason: "no".into(),
                redirect: None,
            },
            true,
        );
        roundtrip(
            Message::FetchOk {
                end_of_track: true,
                end: Location::new(7, 0),
                params: Params::default(),
                properties: Bytes::new(),
            },
            true,
        );
        roundtrip(Message::Namespace { suffix: ns.clone() }, true);
        roundtrip(
            Message::PublishDone {
                status: 2,
                stream_count: 5,
                reason: "ended".into(),
            },
            true,
        );
        roundtrip(
            Message::GoAway {
                uri: Bytes::from_static(b"moqt://other/"),
                timeout_ms: 1000,
            },
            true,
        );
        roundtrip(
            Message::Setup(SetupOptions {
                implementation: Some("senca".into()),
                ..Default::default()
            }),
            true,
        );
    }

    #[test]
    fn rejects_bad_messages() {
        // 本文の長さが合わない（末尾にゴミ）
        let mut enc = Message::Namespace {
            suffix: Namespace::from_path("a"),
        }
        .encode()
        .unwrap();
        enc[2] += 1;
        enc.push(0);
        let mut r = MessageReader::default();
        r.push(&enc);
        let (t, b) = r.next().unwrap().unwrap();
        assert!(Message::decode_body(t, b, true).is_err());
        // 空の namespace 要素
        assert!(Message::decode_body(ty::NAMESPACE, Bytes::from_static(&[0x01, 0x00]), true).is_err());
        // 33 要素
        let mut v = vec![33u8];
        for _ in 0..33 {
            v.extend_from_slice(&[1, b'a']);
        }
        assert!(Message::decode_body(ty::NAMESPACE, Bytes::from(v), true).is_err());
        // 未知の型
        assert!(Message::decode_body(0x77, Bytes::new(), true).is_err());
        // client の GOAWAY に URI
        assert!(Message::decode_body(ty::GOAWAY, Bytes::from_static(&[0x01, b'x', 0x00]), false).is_err());
    }
}
