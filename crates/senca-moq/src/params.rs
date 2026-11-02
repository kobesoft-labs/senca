//! Setup Options・Message Parameters・Properties（draft-22 8.3・8.4・8.9・9.1・9.20・10）。

use bytes::Bytes;

use crate::coding::{Reader, put_bytes, put_varint};
use crate::error::{MoqError, SessionErrorCode};
use crate::{Location, Namespace};

/// Key-Value-Pair の値。偶数の型は varint、奇数の型は長さつきのバイト列。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KvValue {
    Int(u64),
    Bytes(Bytes),
}

/// 値の最大長（2^16 - 1）。
const MAX_KV_LEN: usize = 0xffff;

/// Key-Value-Pair の並びを読む（長さで区切られた範囲すべて）。
pub fn parse_kvps(raw: &Bytes) -> Result<Vec<(u64, KvValue)>, MoqError> {
    let mut r = Reader::new(raw.clone());
    let mut out = Vec::new();
    let mut ty: u64 = 0;
    let bad = || MoqError::new(SessionErrorCode::KEY_VALUE_FORMATTING_ERROR, "Key-Value-Pair が不正");
    while !r.is_empty() {
        let delta = r.varint().map_err(|_| bad())?;
        ty = ty.checked_add(delta).ok_or_else(|| MoqError::proto("Key-Value-Pair の型があふれる"))?;
        let v = if ty.is_multiple_of(2) {
            KvValue::Int(r.varint().map_err(|_| bad())?)
        } else {
            let n = r.varint().map_err(|_| bad())?;
            if n > MAX_KV_LEN as u64 {
                return Err(MoqError::proto("Key-Value-Pair の値が 2^16-1 を超える"));
            }
            KvValue::Bytes(r.bytes(n as usize).map_err(|_| bad())?)
        };
        if out.len() >= 1024 {
            return Err(MoqError::proto("Key-Value-Pair が多すぎる"));
        }
        out.push((ty, v));
    }
    Ok(out)
}

/// Key-Value-Pair の並びを書く（型の昇順に並べてから差分で）。
pub fn write_kvps(out: &mut Vec<u8>, kvs: &[(u64, KvValue)]) {
    let mut v: Vec<&(u64, KvValue)> = kvs.iter().collect();
    v.sort_by_key(|(t, _)| *t);
    let mut prev = 0;
    for (t, val) in v {
        put_varint(out, t - prev);
        prev = *t;
        match val {
            KvValue::Int(i) => put_varint(out, *i),
            KvValue::Bytes(b) => put_bytes(out, b),
        }
    }
}

// ---------------------------------------------------------------------------
// Token（8.9）
// ---------------------------------------------------------------------------

/// 認可トークン。alias は使わない（MAX_AUTH_TOKEN_CACHE_SIZE を 0 で広告する）。
#[derive(Clone, PartialEq, Eq)]
pub struct Token {
    pub token_type: u64,
    pub value: Bytes,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Token(type={}, <{} bytes>)", self.token_type, self.value.len())
    }
}

/// Token 構造の読み取りの結果。
#[derive(Debug)]
pub(crate) enum TokenUse {
    Value(Token),
    /// 登録（SETUP では USE_VALUE として扱う）。
    Register(Token),
    /// 未登録の alias の参照・削除（alias を受け付けないので常に未知）。
    UnknownAlias,
}

fn parse_token(b: Bytes) -> Result<TokenUse, MoqError> {
    let bad = || MoqError::new(SessionErrorCode::KEY_VALUE_FORMATTING_ERROR, "Token 構造が不正");
    let mut r = Reader::new(b);
    let alias_type = r.varint().map_err(|_| bad())?;
    match alias_type {
        0 | 2 => {
            r.varint().map_err(|_| bad())?;
            if !r.is_empty() {
                return Err(bad());
            }
            Ok(TokenUse::UnknownAlias)
        }
        1 => {
            r.varint().map_err(|_| bad())?;
            let token_type = r.varint().map_err(|_| bad())?;
            Ok(TokenUse::Register(Token {
                token_type,
                value: r.rest(),
            }))
        }
        3 => {
            let token_type = r.varint().map_err(|_| bad())?;
            Ok(TokenUse::Value(Token {
                token_type,
                value: r.rest(),
            }))
        }
        _ => Err(bad()),
    }
}

fn encode_token(t: &Token) -> Vec<u8> {
    let mut v = Vec::new();
    put_varint(&mut v, 3); // USE_VALUE
    put_varint(&mut v, t.token_type);
    v.extend_from_slice(&t.value);
    v
}

// ---------------------------------------------------------------------------
// Setup Options（9.1）
// ---------------------------------------------------------------------------

pub mod setup_opt {
    pub const PATH: u64 = 0x01;
    pub const AUTHORIZATION_TOKEN: u64 = 0x03;
    pub const MAX_AUTH_TOKEN_CACHE_SIZE: u64 = 0x04;
    pub const AUTHORITY: u64 = 0x05;
    pub const MAX_FILTER_RANGES: u64 = 0x06;
    pub const MOQT_IMPLEMENTATION: u64 = 0x07;
    pub const MAX_REQUEST_UPDATES: u64 = 0x08;
}

/// SETUP の options。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetupOptions {
    pub path: Option<String>,
    pub authority: Option<String>,
    pub auth_tokens: Vec<Token>,
    pub max_auth_token_cache_size: u64,
    pub implementation: Option<String>,
    pub max_filter_ranges: u64,
    pub max_request_updates: u64,
}

impl SetupOptions {
    pub fn encode(&self) -> Vec<u8> {
        let mut kv = Vec::new();
        if let Some(p) = &self.path {
            kv.push((setup_opt::PATH, KvValue::Bytes(Bytes::copy_from_slice(p.as_bytes()))));
        }
        if let Some(a) = &self.authority {
            kv.push((setup_opt::AUTHORITY, KvValue::Bytes(Bytes::copy_from_slice(a.as_bytes()))));
        }
        if let Some(i) = &self.implementation {
            kv.push((setup_opt::MOQT_IMPLEMENTATION, KvValue::Bytes(Bytes::copy_from_slice(i.as_bytes()))));
        }
        if self.max_request_updates > 0 {
            kv.push((setup_opt::MAX_REQUEST_UPDATES, KvValue::Int(self.max_request_updates)));
        }
        // AUTHORIZATION TOKEN は複数を許す（型の昇順の並びの中で差分 0 になる）
        for t in &self.auth_tokens {
            kv.push((setup_opt::AUTHORIZATION_TOKEN, KvValue::Bytes(Bytes::from(encode_token(t)))));
        }
        let mut out = Vec::new();
        write_kvps(&mut out, &kv);
        out
    }

    /// `from_server` は相手が server か（server は PATH・AUTHORITY を送れない）。
    pub fn decode(raw: Bytes, from_server: bool) -> Result<Self, MoqError> {
        let mut s = Self::default();
        let mut seen: Vec<u64> = Vec::new();
        for (t, v) in parse_kvps(&raw)? {
            let known = matches!(t, 0x01 | 0x03..=0x08);
            if known && t != setup_opt::AUTHORIZATION_TOKEN {
                if seen.contains(&t) {
                    return Err(MoqError::proto(format!("SETUP option 0x{t:x} の重複")));
                }
                seen.push(t);
            }
            let fmt = || MoqError::new(SessionErrorCode::KEY_VALUE_FORMATTING_ERROR, "SETUP option の形式が不正");
            let utf8 = |b: &Bytes| String::from_utf8(b.to_vec()).map_err(|_| fmt());
            match (t, v) {
                (setup_opt::PATH, KvValue::Bytes(b)) => {
                    if from_server {
                        return Err(MoqError::new(SessionErrorCode::INVALID_PATH, "server からの PATH"));
                    }
                    let p = String::from_utf8(b.to_vec())
                        .map_err(|_| MoqError::new(SessionErrorCode::MALFORMED_PATH, "PATH が UTF-8 でない"))?;
                    if !p.is_empty() && !p.starts_with('/') || p.chars().any(|c| c.is_control() || c == ' ') {
                        return Err(MoqError::new(SessionErrorCode::MALFORMED_PATH, "PATH の形式が不正"));
                    }
                    s.path = Some(p);
                }
                (setup_opt::AUTHORITY, KvValue::Bytes(b)) => {
                    if from_server {
                        return Err(MoqError::new(SessionErrorCode::INVALID_AUTHORITY, "server からの AUTHORITY"));
                    }
                    let a = String::from_utf8(b.to_vec())
                        .map_err(|_| MoqError::new(SessionErrorCode::MALFORMED_AUTHORITY, "AUTHORITY が UTF-8 でない"))?;
                    if a.is_empty() || a.chars().any(|c| c.is_control() || c == ' ' || c == '/') {
                        return Err(MoqError::new(SessionErrorCode::MALFORMED_AUTHORITY, "AUTHORITY の形式が不正"));
                    }
                    s.authority = Some(a);
                }
                (setup_opt::AUTHORIZATION_TOKEN, KvValue::Bytes(b)) => match parse_token(b)? {
                    TokenUse::Value(t) | TokenUse::Register(t) => {
                        if s.auth_tokens.len() < 8 {
                            s.auth_tokens.push(t);
                        }
                    }
                    TokenUse::UnknownAlias => {
                        return Err(MoqError::proto("SETUP の Token に DELETE・USE_ALIAS"));
                    }
                },
                (setup_opt::MAX_AUTH_TOKEN_CACHE_SIZE, KvValue::Int(i)) => s.max_auth_token_cache_size = i,
                (setup_opt::MAX_FILTER_RANGES, KvValue::Int(i)) => s.max_filter_ranges = i,
                (setup_opt::MOQT_IMPLEMENTATION, KvValue::Bytes(b)) => s.implementation = Some(utf8(&b)?),
                (setup_opt::MAX_REQUEST_UPDATES, KvValue::Int(i)) => s.max_request_updates = i,
                _ => {} // 未知の option は無視する（9.1）
            }
        }
        Ok(s)
    }
}

// ---------------------------------------------------------------------------
// Message Parameters（9.20）
// ---------------------------------------------------------------------------

pub mod param {
    pub const OBJECT_DELIVERY_TIMEOUT: u64 = 0x02;
    pub const AUTHORIZATION_TOKEN: u64 = 0x03;
    pub const RENDEZVOUS_TIMEOUT: u64 = 0x04;
    pub const SUBGROUP_DELIVERY_TIMEOUT: u64 = 0x06;
    pub const EXPIRES: u64 = 0x08;
    pub const LARGEST_OBJECT: u64 = 0x09;
    pub const FILL_TIMEOUT: u64 = 0x0a;
    pub const FORWARD: u64 = 0x10;
    pub const SUBSCRIBER_PRIORITY: u64 = 0x20;
    pub const LOCATION_FILTER: u64 = 0x21;
    pub const GROUP_ORDER: u64 = 0x22;
    pub const FILL_PARAMETERS: u64 = 0x23;
    pub const SUBGROUP_FILTER: u64 = 0x25;
    pub const OBJECTID_FILTER: u64 = 0x26;
    pub const PRIORITY_FILTER: u64 = 0x27;
    pub const OBJECT_PROPERTY_FILTER: u64 = 0x28;
    pub const TRACK_PROPERTY_FILTER: u64 = 0x29;
    pub const NEW_GROUP_REQUEST: u64 = 0x32;
    pub const TRACK_NAMESPACE_PREFIX: u64 = 0x34;
    pub const INCLUDE_PROPERTIES: u64 = 0x35;

    /// メッセージごとに許される型（9.20.1）。
    pub const SUBSCRIBE: &[u64] = &[0x02, 0x03, 0x04, 0x06, 0x10, 0x20, 0x21, 0x22, 0x23, 0x25, 0x26, 0x27, 0x28, 0x32, 0x35];
    pub const SUBSCRIBE_OK: &[u64] = &[0x08, 0x09];
    pub const PUBLISH: &[u64] = &[0x02, 0x03, 0x06, 0x08, 0x09, 0x10, 0x20, 0x21, 0x22];
    pub const REQUEST_OK: &[u64] = &[0x08, 0x09];
    pub const REQUEST_UPDATE: &[u64] = &[0x02, 0x03, 0x06, 0x10, 0x20, 0x21, 0x23, 0x25, 0x26, 0x27, 0x28, 0x29, 0x32, 0x34];
    pub const FETCH: &[u64] = &[0x03, 0x0a, 0x20, 0x21, 0x22, 0x25, 0x26, 0x27, 0x28, 0x35];
    pub const FETCH_OK: &[u64] = &[];
    pub const TRACK_STATUS: &[u64] = &[0x03, 0x35];
    pub const PUBLISH_NAMESPACE: &[u64] = &[0x03];
    pub const SUBSCRIBE_NAMESPACE: &[u64] = &[0x03];
    pub const SUBSCRIBE_TRACKS: &[u64] = &[0x03, 0x10, 0x22, 0x25, 0x26, 0x27, 0x28, 0x29, 0x35];
    pub const PUBLISH_STATE_NOTIFY: &[u64] = &[0x09, 0x10, 0x21];
    pub const FILL: &[u64] = &[0x0a, 0x20, 0x21, 0x22, 0x25, 0x26, 0x27, 0x28];
}

/// 群の順序（9.20.8）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupOrder {
    Ascending,
    Descending,
}

/// Location Filter（9.20.9）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocationFilter {
    None,
    /// {Largest.Group + 1 - n, 0} から。
    RelativeStart(u64),
    AbsoluteStart(Location),
    /// `end_object` が無ければ end_group の最後まで。end_group は絶対値。
    AbsoluteRange {
        start: Location,
        end_group: u64,
        end_object: Option<u64>,
    },
    NextObject,
}

impl LocationFilter {
    fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            Self::None => put_varint(out, 0),
            Self::RelativeStart(n) => {
                put_varint(out, 1);
                put_varint(out, n);
            }
            Self::AbsoluteStart(l) => {
                put_varint(out, 2);
                put_varint(out, l.group);
                put_varint(out, l.object);
            }
            Self::AbsoluteRange {
                start,
                end_group,
                end_object,
            } => {
                put_varint(out, if end_object.is_some() { 4 } else { 3 });
                put_varint(out, start.group);
                put_varint(out, start.object);
                put_varint(out, end_group.saturating_sub(start.group));
                if let Some(e) = end_object {
                    put_varint(out, e);
                }
            }
            Self::NextObject => put_varint(out, 5),
        }
    }

    fn decode(r: &mut Reader) -> Result<Self, MoqError> {
        let e = || MoqError::proto("LOCATION_FILTER が切れている");
        let t = r.varint().map_err(|_| e())?;
        Ok(match t {
            0 => Self::None,
            1 => Self::RelativeStart(r.varint().map_err(|_| e())?),
            2 => Self::AbsoluteStart(Location::new(r.varint().map_err(|_| e())?, r.varint().map_err(|_| e())?)),
            3 | 4 => {
                let start = Location::new(r.varint().map_err(|_| e())?, r.varint().map_err(|_| e())?);
                let delta = r.varint().map_err(|_| e())?;
                let end_group = start
                    .group
                    .checked_add(delta)
                    .ok_or_else(|| MoqError::proto("EndGroupDelta があふれる"))?;
                let end_object = if t == 4 { Some(r.varint().map_err(|_| e())?) } else { None };
                Self::AbsoluteRange {
                    start,
                    end_group,
                    end_object,
                }
            }
            5 => Self::NextObject,
            _ => return Err(MoqError::proto(format!("未知の Location Filter Type {t}"))),
        })
    }
}

/// Message Parameters。各メッセージで使うものだけを入れる。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Params {
    pub object_delivery_timeout: Option<u64>,
    pub auth_tokens: Vec<Token>,
    /// 未登録の alias を参照した（UNKNOWN_AUTH_TOKEN_ALIAS で拒否する）。
    pub unknown_token_alias: bool,
    pub rendezvous_timeout: Option<u64>,
    pub subgroup_delivery_timeout: Option<u64>,
    pub expires: Option<u64>,
    pub largest_object: Option<Location>,
    pub fill_timeout: Option<u64>,
    pub forward: Option<bool>,
    pub subscriber_priority: Option<u8>,
    pub location_filter: Option<LocationFilter>,
    pub group_order: Option<GroupOrder>,
    pub fill: Option<Box<Params>>,
    /// Range Filter（0x25〜0x29）。MAX_FILTER_RANGES を 0 で広告するので、受けたら INVALID_FILTER で拒否する。
    pub range_filters: Vec<(u64, Bytes)>,
    pub new_group_request: Option<u64>,
    pub track_namespace_prefix: Option<Namespace>,
    pub include_properties: Option<bool>,
}

impl Params {
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut items: Vec<(u64, Vec<u8>)> = Vec::new();
        let int = |v: u64| {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            b
        };
        if let Some(v) = self.object_delivery_timeout {
            items.push((param::OBJECT_DELIVERY_TIMEOUT, int(v)));
        }
        for t in &self.auth_tokens {
            let mut b = Vec::new();
            put_bytes(&mut b, &encode_token(t));
            items.push((param::AUTHORIZATION_TOKEN, b));
        }
        if let Some(v) = self.rendezvous_timeout {
            items.push((param::RENDEZVOUS_TIMEOUT, int(v)));
        }
        if let Some(v) = self.subgroup_delivery_timeout {
            items.push((param::SUBGROUP_DELIVERY_TIMEOUT, int(v)));
        }
        if let Some(v) = self.expires {
            items.push((param::EXPIRES, int(v)));
        }
        if let Some(l) = self.largest_object {
            let mut b = int(l.group);
            put_varint(&mut b, l.object);
            items.push((param::LARGEST_OBJECT, b));
        }
        if let Some(v) = self.fill_timeout {
            items.push((param::FILL_TIMEOUT, int(v)));
        }
        if let Some(v) = self.forward {
            items.push((param::FORWARD, vec![u8::from(v)]));
        }
        if let Some(v) = self.subscriber_priority {
            items.push((param::SUBSCRIBER_PRIORITY, vec![v]));
        }
        if let Some(f) = &self.location_filter {
            let mut b = Vec::new();
            f.encode(&mut b);
            items.push((param::LOCATION_FILTER, b));
        }
        if let Some(g) = self.group_order {
            items.push((param::GROUP_ORDER, vec![if g == GroupOrder::Ascending { 1 } else { 2 }]));
        }
        if let Some(f) = &self.fill {
            let mut inner = Vec::new();
            f.encode(&mut inner);
            let mut b = Vec::new();
            put_bytes(&mut b, &inner);
            items.push((param::FILL_PARAMETERS, b));
        }
        if let Some(v) = self.new_group_request {
            items.push((param::NEW_GROUP_REQUEST, int(v)));
        }
        if let Some(ns) = &self.track_namespace_prefix {
            let mut b = Vec::new();
            crate::message::put_namespace(&mut b, ns);
            items.push((param::TRACK_NAMESPACE_PREFIX, b));
        }
        if let Some(v) = self.include_properties {
            items.push((param::INCLUDE_PROPERTIES, vec![u8::from(v)]));
        }
        items.sort_by_key(|(t, _)| *t);
        put_varint(out, items.len() as u64);
        let mut prev = 0;
        for (t, v) in items {
            put_varint(out, t - prev);
            prev = t;
            out.extend_from_slice(&v);
        }
    }

    /// 読む。`allowed` に無い型は PROTOCOL_VIOLATION（9.20.1）。`in_setup` は使わない。
    pub(crate) fn decode(r: &mut Reader, allowed: &[u64]) -> Result<Self, MoqError> {
        let e = || MoqError::proto("Message Parameter が切れている");
        let count = r.varint().map_err(|_| e())?;
        if count > 64 {
            return Err(MoqError::proto("Message Parameter が多すぎる"));
        }
        let mut p = Self::default();
        let mut ty: u64 = 0;
        let mut seen: Vec<u64> = Vec::new();
        for _ in 0..count {
            let delta = r.varint().map_err(|_| e())?;
            ty = ty.checked_add(delta).ok_or_else(|| MoqError::proto("Parameter の型があふれる"))?;
            if !allowed.contains(&ty) {
                return Err(MoqError::proto(format!("このメッセージに使えない Parameter 0x{ty:x}")));
            }
            let repeatable = ty == param::AUTHORIZATION_TOKEN || (0x25..=0x29).contains(&ty);
            if !repeatable {
                if seen.contains(&ty) {
                    return Err(MoqError::proto(format!("Parameter 0x{ty:x} の重複")));
                }
                seen.push(ty);
            }
            let u8v = |r: &mut Reader| r.u8().map_err(|_| e());
            let boolv = |r: &mut Reader, what: &str| -> Result<bool, MoqError> {
                match r.u8().map_err(|_| e())? {
                    0 => Ok(false),
                    1 => Ok(true),
                    v => Err(MoqError::proto(format!("{what} の値 {v} は 0・1 でない"))),
                }
            };
            match ty {
                param::OBJECT_DELIVERY_TIMEOUT => p.object_delivery_timeout = Some(r.varint().map_err(|_| e())?),
                param::AUTHORIZATION_TOKEN => {
                    let b = r.len_bytes(0xffff).map_err(|_| e())?;
                    match parse_token(b)? {
                        TokenUse::Value(t) => {
                            if p.auth_tokens.len() < 8 {
                                p.auth_tokens.push(t);
                            }
                        }
                        TokenUse::Register(_) => {
                            // MAX_AUTH_TOKEN_CACHE_SIZE を 0 で広告している（9.1.3）
                            return Err(MoqError::new(
                                SessionErrorCode::AUTH_TOKEN_CACHE_OVERFLOW,
                                "token の登録は受け付けない",
                            ));
                        }
                        TokenUse::UnknownAlias => p.unknown_token_alias = true,
                    }
                }
                param::RENDEZVOUS_TIMEOUT => p.rendezvous_timeout = Some(r.varint().map_err(|_| e())?),
                param::SUBGROUP_DELIVERY_TIMEOUT => p.subgroup_delivery_timeout = Some(r.varint().map_err(|_| e())?),
                param::EXPIRES => p.expires = Some(r.varint().map_err(|_| e())?),
                param::LARGEST_OBJECT => {
                    p.largest_object = Some(Location::new(r.varint().map_err(|_| e())?, r.varint().map_err(|_| e())?))
                }
                param::FILL_TIMEOUT => p.fill_timeout = Some(r.varint().map_err(|_| e())?),
                param::FORWARD => p.forward = Some(boolv(r, "FORWARD")?),
                param::SUBSCRIBER_PRIORITY => p.subscriber_priority = Some(u8v(r)?),
                param::LOCATION_FILTER => p.location_filter = Some(LocationFilter::decode(r)?),
                param::GROUP_ORDER => {
                    p.group_order = Some(match u8v(r)? {
                        1 => GroupOrder::Ascending,
                        2 => GroupOrder::Descending,
                        v => return Err(MoqError::proto(format!("GROUP_ORDER の値 {v}"))),
                    })
                }
                param::FILL_PARAMETERS => {
                    let b = r.len_bytes(0xffff).map_err(|_| e())?;
                    let mut inner = Reader::new(b);
                    let f = Params::decode(&mut inner, param::FILL)?;
                    if !inner.is_empty() {
                        return Err(MoqError::proto("FILL_PARAMETERS の長さが合わない"));
                    }
                    p.fill = Some(Box::new(f));
                }
                0x25..=0x29 => {
                    let b = r.len_bytes(0xffff).map_err(|_| e())?;
                    if p.range_filters.len() < 16 {
                        p.range_filters.push((ty, b));
                    }
                }
                param::NEW_GROUP_REQUEST => p.new_group_request = Some(r.varint().map_err(|_| e())?),
                param::TRACK_NAMESPACE_PREFIX => p.track_namespace_prefix = Some(crate::message::get_namespace(r)?),
                param::INCLUDE_PROPERTIES => p.include_properties = Some(boolv(r, "INCLUDE_PROPERTIES")?),
                _ => return Err(MoqError::proto(format!("未知の Parameter 0x{ty:x}"))),
            }
        }
        Ok(p)
    }
}

// ---------------------------------------------------------------------------
// Properties（8.4・10）
// ---------------------------------------------------------------------------

pub mod prop {
    pub const OBJECT_DELIVERY_TIMEOUT: u64 = 0x02;
    pub const MAX_CACHE_DURATION: u64 = 0x04;
    pub const SUBGROUP_DELIVERY_TIMEOUT: u64 = 0x06;
    pub const IMMUTABLE: u64 = 0x0b;
    pub const DEFAULT_PUBLISHER_PRIORITY: u64 = 0x0e;
    pub const DEFAULT_PUBLISHER_GROUP_ORDER: u64 = 0x22;
    pub const DYNAMIC_GROUPS: u64 = 0x30;
    pub const PRIOR_GROUP_ID_GAP: u64 = 0x3c;
    pub const PRIOR_OBJECT_ID_GAP: u64 = 0x3e;
}

/// Track Properties の中で使う値。生のバイト列は relay がそのまま転送する。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackProperties {
    pub raw: Bytes,
    pub object_delivery_timeout: Option<u64>,
    pub subgroup_delivery_timeout: Option<u64>,
    pub max_cache_duration: Option<u64>,
    pub default_publisher_priority: Option<u8>,
    pub default_group_order: Option<GroupOrder>,
}

impl TrackProperties {
    pub fn parse(raw: Bytes) -> Result<Self, MoqError> {
        let mut t = Self {
            raw: raw.clone(),
            ..Default::default()
        };
        for (ty, v) in parse_kvps(&raw)? {
            match (ty, v) {
                (prop::OBJECT_DELIVERY_TIMEOUT, KvValue::Int(i)) => t.object_delivery_timeout = Some(i),
                (prop::SUBGROUP_DELIVERY_TIMEOUT, KvValue::Int(i)) => t.subgroup_delivery_timeout = Some(i),
                (prop::MAX_CACHE_DURATION, KvValue::Int(i)) => t.max_cache_duration = Some(i),
                (prop::DEFAULT_PUBLISHER_PRIORITY, KvValue::Int(i)) => {
                    if i > 255 {
                        return Err(MoqError::proto("DEFAULT PUBLISHER PRIORITY が 255 を超える"));
                    }
                    t.default_publisher_priority = Some(i as u8);
                }
                (prop::DEFAULT_PUBLISHER_GROUP_ORDER, KvValue::Int(i)) => {
                    t.default_group_order = Some(match i {
                        1 => GroupOrder::Ascending,
                        2 => GroupOrder::Descending,
                        _ => return Err(MoqError::proto("DEFAULT_PUBLISHER_GROUP_ORDER の値")),
                    })
                }
                (prop::DYNAMIC_GROUPS, KvValue::Int(i)) if i > 1 => {
                    return Err(MoqError::proto("DYNAMIC_GROUPS の値"));
                }
                // 入れ子の Immutable は malformed
                (prop::IMMUTABLE, KvValue::Bytes(b)) if parse_kvps(&b)?.iter().any(|(t, _)| *t == prop::IMMUTABLE) => {
                    return Err(MoqError::proto("Immutable Properties の入れ子"));
                }
                _ => {}
            }
        }
        Ok(t)
    }

    pub fn build(kvs: &[(u64, KvValue)]) -> Self {
        let mut out = Vec::new();
        write_kvps(&mut out, kvs);
        Self::parse(Bytes::from(out)).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_roundtrip_and_rules() {
        let p = Params {
            subscriber_priority: Some(7),
            forward: Some(true),
            location_filter: Some(LocationFilter::AbsoluteRange {
                start: Location::new(5, 1),
                end_group: 9,
                end_object: Some(3),
            }),
            group_order: Some(GroupOrder::Descending),
            auth_tokens: vec![Token {
                token_type: 0,
                value: Bytes::from_static(b"secret"),
            }],
            object_delivery_timeout: Some(500),
            fill: Some(Box::new(Params {
                group_order: Some(GroupOrder::Ascending),
                ..Default::default()
            })),
            ..Default::default()
        };
        let mut b = Vec::new();
        p.encode(&mut b);
        let mut r = Reader::new(Bytes::from(b.clone()));
        assert_eq!(Params::decode(&mut r, param::SUBSCRIBE).unwrap(), p);
        assert!(r.is_empty());
        // FETCH_OK には使えない
        let mut r = Reader::new(Bytes::from(b));
        assert!(Params::decode(&mut r, param::FETCH_OK).is_err());
        // 重複
        let mut r = Reader::new(Bytes::from_static(&[0x02, 0x20, 0x01, 0x00, 0x02]));
        assert!(Params::decode(&mut r, param::SUBSCRIBE).is_err());
        // FORWARD に 2
        let mut r = Reader::new(Bytes::from_static(&[0x01, 0x10, 0x02]));
        assert!(Params::decode(&mut r, param::SUBSCRIBE).is_err());
        // 未知の型
        let mut r = Reader::new(Bytes::from_static(&[0x01, 0x3e, 0x00]));
        assert!(Params::decode(&mut r, param::SUBSCRIBE).is_err());
    }

    #[test]
    fn setup_options_roundtrip() {
        let s = SetupOptions {
            path: Some("/moq?x=1".into()),
            authority: Some("relay.example:443".into()),
            implementation: Some("senca".into()),
            auth_tokens: vec![Token {
                token_type: 0,
                value: Bytes::from_static(b"t"),
            }],
            ..Default::default()
        };
        let enc = s.encode();
        assert_eq!(SetupOptions::decode(Bytes::from(enc.clone()), false).unwrap(), s);
        assert!(SetupOptions::decode(Bytes::from(enc), true).is_err());
        // 未知の option（grease 0x9d）は無視、重複も許す
        let mut v = Vec::new();
        write_kvps(&mut v, &[(0x9d, KvValue::Bytes(Bytes::from_static(b"x")))]);
        v.extend_from_slice(&[0x00, 0x01, b'y']);
        assert!(SetupOptions::decode(Bytes::from(v), true).is_ok());
    }

    #[test]
    fn kvp_limits() {
        assert!(parse_kvps(&Bytes::from_static(&[0x01, 0xc0, 0xff, 0xff])).is_err());
        let t = TrackProperties::build(&[
            (prop::DEFAULT_PUBLISHER_PRIORITY, KvValue::Int(3)),
            (prop::MAX_CACHE_DURATION, KvValue::Int(1000)),
        ]);
        assert_eq!(t.default_publisher_priority, Some(3));
        assert_eq!(t.max_cache_duration, Some(1000));
    }
}
