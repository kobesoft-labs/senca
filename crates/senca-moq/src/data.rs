//! データ stream と datagram（draft-22 11 章）。

use bytes::{Buf, Bytes, BytesMut};

use crate::coding::{Reader, SliceReader, put_varint};
use crate::error::MoqError;
use crate::params::{GroupOrder, parse_kvps};
use crate::varint;

/// 単方向 stream の種別（6.4.1）。
pub mod stream_type {
    pub const FETCH_HEADER: u64 = 0x05;
    pub const SETUP: u64 = 0x2f00;
    pub const PADDING: u64 = 0x132b_3e28;
    pub const PADDING_DATAGRAM: u64 = 0x132b_3e29;
}

/// Object Status（11.1.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectStatus {
    Normal,
    EndOfGroup,
    EndOfTrack,
}

impl ObjectStatus {
    pub fn to_u64(self) -> u64 {
        match self {
            Self::Normal => 0,
            Self::EndOfGroup => 3,
            Self::EndOfTrack => 4,
        }
    }
    pub fn from_u64(v: u64) -> Result<Self, MoqError> {
        match v {
            0 => Ok(Self::Normal),
            3 => Ok(Self::EndOfGroup),
            4 => Ok(Self::EndOfTrack),
            _ => Err(MoqError::proto(format!("未知の Object Status {v}"))),
        }
    }
}

/// Subgroup ID の表し方（11.3.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubgroupId {
    Zero,
    FirstObject,
    Explicit(u64),
}

/// SUBGROUP_HEADER。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubgroupHeader {
    pub track_alias: u64,
    pub group: u64,
    pub subgroup: SubgroupId,
    /// `None` なら購読の既定の優先度を使う。
    pub priority: Option<u8>,
    pub has_properties: bool,
    pub end_of_group: bool,
    pub first_object: bool,
}

impl SubgroupHeader {
    pub fn encode(&self) -> Vec<u8> {
        let mut flags: u8 = 0x10;
        if self.has_properties {
            flags |= 0x01;
        }
        flags |= match self.subgroup {
            SubgroupId::Zero => 0,
            SubgroupId::FirstObject => 0x02,
            SubgroupId::Explicit(_) => 0x04,
        };
        if self.end_of_group {
            flags |= 0x08;
        }
        if self.priority.is_none() {
            flags |= 0x20;
        }
        if self.first_object {
            flags |= 0x40;
        }
        let mut out = vec![flags];
        put_varint(&mut out, self.track_alias);
        put_varint(&mut out, self.group);
        if let SubgroupId::Explicit(s) = self.subgroup {
            put_varint(&mut out, s);
        }
        if let Some(p) = self.priority {
            out.push(p);
        }
        out
    }

    fn decode_after_type(flags: u64, r: &mut SliceReader<'_>) -> Result<Option<Self>, MoqError> {
        if flags >= 128 || flags & 0x10 == 0 || (flags & 0x06) == 0x06 {
            return Err(MoqError::proto(format!("SUBGROUP_HEADER の Type Flags 0x{flags:x} が不正")));
        }
        let Ok(track_alias) = r.varint() else { return Ok(None) };
        let Ok(group) = r.varint() else { return Ok(None) };
        let subgroup = match (flags & 0x06) >> 1 {
            0 => SubgroupId::Zero,
            1 => SubgroupId::FirstObject,
            _ => {
                let Ok(s) = r.varint() else { return Ok(None) };
                SubgroupId::Explicit(s)
            }
        };
        let priority = if flags & 0x20 == 0 {
            let Ok(p) = r.u8() else { return Ok(None) };
            Some(p)
        } else {
            None
        };
        Ok(Some(Self {
            track_alias,
            group,
            subgroup,
            priority,
            has_properties: flags & 0x01 != 0,
            end_of_group: flags & 0x08 != 0,
            first_object: flags & 0x40 != 0,
        }))
    }
}

/// stream・datagram から読んだ object。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectData {
    pub group: u64,
    pub subgroup: Option<u64>,
    pub object: u64,
    pub priority: Option<u8>,
    pub properties: Bytes,
    pub status: ObjectStatus,
    pub payload: Bytes,
}

/// subgroup stream の object の見出しを書く（payload は別に足す）。
pub fn encode_subgroup_object(prev: Option<u64>, object: u64, has_properties: bool, properties: &[u8], status: ObjectStatus, payload_len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let delta = match prev {
        Some(p) => object - p - 1,
        None => object,
    };
    put_varint(&mut out, delta);
    if has_properties {
        put_varint(&mut out, properties.len() as u64);
        out.extend_from_slice(properties);
    }
    put_varint(&mut out, payload_len as u64);
    if payload_len == 0 {
        put_varint(&mut out, status.to_u64());
    }
    out
}

/// FETCH_HEADER。
pub fn encode_fetch_header(request_id: u64) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, stream_type::FETCH_HEADER);
    put_varint(&mut out, request_id);
    out
}

/// fetch stream の前の object（差分の基準）。
#[derive(Clone, Copy, Debug, Default)]
pub struct FetchPrior {
    group: Option<u64>,
    object: u64,
    subgroup: Option<u64>,
    priority: Option<u8>,
}

/// fetch の範囲の印（11.4.1.2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeEnd {
    NonExistent,
    Unknown,
    TimedOut,
}

/// fetch stream の object を書く。`order` は群の順序（差分の向き）。
pub fn encode_fetch_object(prior: &mut FetchPrior, order: GroupOrder, o: &ObjectData) -> Vec<u8> {
    let mut out = Vec::new();
    let mut flags: u64 = 0x10; // priority は常に書く
    let datagram = o.subgroup.is_none();
    if datagram {
        flags |= 0x40;
    } else {
        flags |= 0x03;
    }
    let same_group = prior.group == Some(o.group);
    if !same_group {
        flags |= 0x08 | 0x04;
    } else {
        flags |= 0x04;
    }
    if !o.properties.is_empty() {
        flags |= 0x20;
    }
    put_varint(&mut out, flags);
    if !same_group {
        let gd = match (prior.group, order) {
            (None, _) => o.group,
            (Some(pg), GroupOrder::Ascending) => o.group - pg - 1,
            (Some(pg), GroupOrder::Descending) => pg - o.group - 1,
        };
        put_varint(&mut out, gd);
    }
    if !datagram {
        put_varint(&mut out, o.subgroup.unwrap_or(0));
    }
    let od = if same_group { o.object - prior.object } else { o.object };
    put_varint(&mut out, od);
    out.push(o.priority.unwrap_or(128));
    if !o.properties.is_empty() {
        put_varint(&mut out, o.properties.len() as u64);
        out.extend_from_slice(&o.properties);
    }
    put_varint(&mut out, o.payload.len() as u64);
    prior.group = Some(o.group);
    prior.object = o.object;
    prior.subgroup = o.subgroup;
    prior.priority = o.priority;
    out
}

/// fetch の範囲の印を書く。
pub fn encode_fetch_range_end(prior: &mut FetchPrior, end: RangeEnd, group: u64, object: u64) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(
        &mut out,
        match end {
            RangeEnd::NonExistent => 0x8c,
            RangeEnd::Unknown => 0x10c,
            RangeEnd::TimedOut => 0x20c,
        },
    );
    put_varint(&mut out, group);
    put_varint(&mut out, object);
    prior.group = Some(group);
    prior.object = object;
    out
}

// ---------------------------------------------------------------------------
// datagram（11.2.1）
// ---------------------------------------------------------------------------

/// OBJECT_DATAGRAM を書く。
pub fn encode_datagram(track_alias: u64, o: &ObjectData, end_of_group: bool) -> Vec<u8> {
    let mut flags: u64 = 0;
    if !o.properties.is_empty() {
        flags |= 0x01;
    }
    let status = o.payload.is_empty() && o.status != ObjectStatus::Normal;
    if end_of_group && !status {
        flags |= 0x02;
    }
    if o.object == 0 {
        flags |= 0x04;
    }
    if o.priority.is_none() {
        flags |= 0x08;
    }
    if status {
        flags |= 0x20;
    }
    let mut out = Vec::with_capacity(16 + o.payload.len());
    put_varint(&mut out, flags);
    put_varint(&mut out, track_alias);
    put_varint(&mut out, o.group);
    if o.object != 0 {
        put_varint(&mut out, o.object);
    }
    if let Some(p) = o.priority {
        out.push(p);
    }
    if !o.properties.is_empty() {
        put_varint(&mut out, o.properties.len() as u64);
        out.extend_from_slice(&o.properties);
    }
    if status {
        put_varint(&mut out, o.status.to_u64());
    } else {
        out.extend_from_slice(&o.payload);
    }
    out
}

/// datagram を読んだ結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Datagram {
    Object {
        track_alias: u64,
        object: ObjectData,
        end_of_group: bool,
    },
    Padding,
}

pub fn decode_datagram(d: Bytes) -> Result<Datagram, MoqError> {
    let mut r = Reader::new(d);
    let e = || MoqError::proto("OBJECT_DATAGRAM が切れている");
    let flags = r.varint().map_err(|_| e())?;
    if flags == stream_type::PADDING_DATAGRAM {
        if r.rest().iter().any(|&b| b != 0) {
            return Err(MoqError::proto("padding datagram が 0 でない"));
        }
        return Ok(Datagram::Padding);
    }
    if flags & !0x2f != 0 || flags & 0x10 != 0 || (flags & 0x22) == 0x22 {
        return Err(MoqError::proto(format!("OBJECT_DATAGRAM の Type Flags 0x{flags:x} が不正")));
    }
    let track_alias = r.varint().map_err(|_| e())?;
    let group = r.varint().map_err(|_| e())?;
    let object = if flags & 0x04 != 0 { 0 } else { r.varint().map_err(|_| e())? };
    let priority = if flags & 0x08 != 0 { None } else { Some(r.u8().map_err(|_| e())?) };
    let properties = if flags & 0x01 != 0 {
        let p = r.len_bytes(0xffff).map_err(|_| e())?;
        if p.is_empty() {
            return Err(MoqError::proto("PROPERTIES が立っているのに長さ 0"));
        }
        parse_kvps(&p)?;
        p
    } else {
        Bytes::new()
    };
    let (status, payload) = if flags & 0x20 != 0 {
        let s = ObjectStatus::from_u64(r.varint().map_err(|_| e())?)?;
        if !properties.is_empty() && s != ObjectStatus::Normal {
            return Err(MoqError::proto("Normal でない object に Properties"));
        }
        if !r.is_empty() {
            return Err(MoqError::proto("Status つき datagram に payload"));
        }
        (s, Bytes::new())
    } else {
        (ObjectStatus::Normal, r.rest())
    };
    Ok(Datagram::Object {
        track_alias,
        object: ObjectData {
            group,
            subgroup: None,
            object,
            priority,
            properties,
            status,
            payload,
        },
        end_of_group: flags & 0x02 != 0,
    })
}

// ---------------------------------------------------------------------------
// 単方向 stream の読み取り
// ---------------------------------------------------------------------------

/// stream の見出し。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamHeader {
    Subgroup(SubgroupHeader),
    Fetch { request_id: u64 },
    Padding,
    /// 制御 stream（SETUP の型）。見出しの後は制御メッセージとして読む。
    Control,
}

/// 読んだもの。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamItem {
    Header(StreamHeader),
    Object(ObjectData),
    /// fetch の範囲の印。
    RangeEnd { kind: RangeEnd, group: u64, object: u64 },
}

#[derive(Debug)]
enum RState {
    Type,
    Subgroup { header: SubgroupHeader, prev: Option<u64>, first_object_id: Option<u64> },
    Fetch { prior: FetchPriorIn },
    Padding,
    Control,
}

#[derive(Debug, Default, Clone, Copy)]
struct FetchPriorIn {
    group: Option<u64>,
    object: u64,
    subgroup: Option<u64>,
    priority: Option<u8>,
    had_object: bool,
}

/// 単方向 stream を少しずつ読む。object は payload まで揃ってから返す。
#[derive(Debug)]
pub struct DataStreamReader {
    buf: BytesMut,
    state: RState,
    max_object: usize,
    /// fetch の群の順序（差分の向き）。FETCH_OK を受けるまでは昇順とみなす。
    pub fetch_order: GroupOrder,
}

impl DataStreamReader {
    pub fn new(max_object: usize) -> Self {
        Self {
            buf: BytesMut::new(),
            state: RState::Type,
            max_object,
            fetch_order: GroupOrder::Ascending,
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        if matches!(self.state, RState::Padding) {
            return; // padding は捨てる（11.5.1）
        }
        self.buf.extend_from_slice(data);
    }

    /// object の途中か（FIN が来たら PROTOCOL_VIOLATION）。
    pub fn mid_object(&self) -> bool {
        !self.buf.is_empty() && !matches!(self.state, RState::Padding | RState::Control)
    }

    /// 制御 stream なら残りのバイト列を取り出す。
    pub fn take_control_bytes(&mut self) -> Bytes {
        self.buf.split().freeze()
    }

    pub fn next_item(&mut self) -> Result<Option<StreamItem>, MoqError> {
        // バッファを写さずに位置だけ読み、確定したら切り出す
        let mut r = SliceReader::new(&self.buf);
        match &mut self.state {
            RState::Type => {
                let Ok(t) = r.varint() else { return Ok(None) };
                let item = match t {
                    stream_type::FETCH_HEADER => {
                        let Ok(request_id) = r.varint() else { return Ok(None) };
                        self.state = RState::Fetch {
                            prior: FetchPriorIn::default(),
                        };
                        StreamHeader::Fetch { request_id }
                    }
                    stream_type::PADDING => {
                        self.state = RState::Padding;
                        self.buf.clear();
                        return Ok(Some(StreamItem::Header(StreamHeader::Padding)));
                    }
                    stream_type::SETUP => {
                        // 型は SETUP メッセージの型を兼ねる。見出しは消費しない（制御メッセージとして読む）
                        self.state = RState::Control;
                        return Ok(Some(StreamItem::Header(StreamHeader::Control)));
                    }
                    t if t & 0x10 != 0 && t < 128 => {
                        let Some(h) = SubgroupHeader::decode_after_type(t, &mut r)? else { return Ok(None) };
                        self.state = RState::Subgroup {
                            header: h.clone(),
                            prev: None,
                            first_object_id: None,
                        };
                        StreamHeader::Subgroup(h)
                    }
                    _ => return Err(MoqError::proto(format!("未知の単方向 stream の種別 0x{t:x}"))),
                };
                let n = r.pos();
                self.buf.advance(n);
                Ok(Some(StreamItem::Header(item)))
            }
            RState::Subgroup {
                header,
                prev,
                first_object_id,
            } => {
                let Ok(delta) = r.varint() else { return Ok(None) };
                let object = match prev {
                    Some(p) => p.checked_add(delta).and_then(|v| v.checked_add(1)),
                    None => Some(delta),
                }
                .ok_or_else(|| MoqError::proto("Object ID があふれる"))?;
                let properties = if header.has_properties {
                    let Ok(n) = r.varint() else { return Ok(None) };
                    if n > 0xffff {
                        return Err(MoqError::proto("Object Properties が長すぎる"));
                    }
                    let Ok(p) = r.bytes(n as usize) else { return Ok(None) };
                    let p = Bytes::copy_from_slice(p);
                    parse_kvps(&p)?;
                    p
                } else {
                    Bytes::new()
                };
                let Ok(len) = r.varint() else { return Ok(None) };
                if len > self.max_object as u64 {
                    return Err(MoqError::proto(format!("object が上限を超える ({len} バイト)")));
                }
                let status = if len == 0 {
                    let Ok(s) = r.varint() else { return Ok(None) };
                    let s = ObjectStatus::from_u64(s)?;
                    if s != ObjectStatus::Normal && !properties.is_empty() {
                        return Err(MoqError::proto("Normal でない object に Properties"));
                    }
                    s
                } else {
                    ObjectStatus::Normal
                };
                if r.remaining() < len as usize {
                    return Ok(None);
                }
                let hdr = r.pos();
                *prev = Some(object);
                if first_object_id.is_none() {
                    *first_object_id = Some(object);
                }
                let subgroup = match header.subgroup {
                    SubgroupId::Zero => 0,
                    SubgroupId::FirstObject => first_object_id.unwrap_or(object),
                    SubgroupId::Explicit(s) => s,
                };
                let group = header.group;
                let priority = header.priority;
                self.buf.advance(hdr);
                let payload = self.buf.split_to(len as usize).freeze();
                Ok(Some(StreamItem::Object(ObjectData {
                    group,
                    subgroup: Some(subgroup),
                    object,
                    priority,
                    properties,
                    status,
                    payload,
                })))
            }
            RState::Fetch { prior } => {
                let Ok(flags) = r.varint() else { return Ok(None) };
                if flags >= 128 {
                    let kind = match flags {
                        0x8c => RangeEnd::NonExistent,
                        0x10c => RangeEnd::Unknown,
                        0x20c => RangeEnd::TimedOut,
                        _ => return Err(MoqError::proto(format!("未知の Serialization Flags 0x{flags:x}"))),
                    };
                    let Ok(group) = r.varint() else { return Ok(None) };
                    let Ok(object) = r.varint() else { return Ok(None) };
                    let n = r.pos();
                    prior.group = Some(group);
                    prior.object = object;
                    self.buf.advance(n);
                    return Ok(Some(StreamItem::RangeEnd { kind, group, object }));
                }
                let first = prior.group.is_none();
                if first && (flags & 0x08 == 0 || flags & 0x04 == 0) {
                    return Err(MoqError::proto("fetch の最初の object に Group・Object の差分がない"));
                }
                let group = if flags & 0x08 != 0 {
                    let Ok(d) = r.varint() else { return Ok(None) };
                    match prior.group {
                        None => d,
                        Some(pg) => match self.fetch_order {
                            GroupOrder::Ascending => pg.checked_add(d).and_then(|v| v.checked_add(1)),
                            GroupOrder::Descending => pg.checked_sub(d).and_then(|v| v.checked_sub(1)),
                        }
                        .ok_or_else(|| MoqError::proto("Group ID があふれる"))?,
                    }
                } else {
                    prior.group.unwrap_or(0)
                };
                let datagram = flags & 0x40 != 0;
                let subgroup = if datagram {
                    None
                } else {
                    Some(match flags & 0x03 {
                        0 => 0,
                        1 => prior
                            .subgroup
                            .filter(|_| prior.had_object)
                            .ok_or_else(|| MoqError::proto("前の Subgroup ID が無い"))?,
                        2 => prior
                            .subgroup
                            .filter(|_| prior.had_object)
                            .and_then(|s| s.checked_add(1))
                            .ok_or_else(|| MoqError::proto("前の Subgroup ID が無い"))?,
                        _ => {
                            let Ok(s) = r.varint() else { return Ok(None) };
                            s
                        }
                    })
                };
                let object = if flags & 0x04 != 0 {
                    let Ok(d) = r.varint() else { return Ok(None) };
                    if flags & 0x08 != 0 { Some(d) } else { prior.object.checked_add(d) }
                } else {
                    prior.object.checked_add(1)
                }
                .ok_or_else(|| MoqError::proto("Object ID があふれる"))?;
                let priority = if flags & 0x10 != 0 {
                    let Ok(p) = r.u8() else { return Ok(None) };
                    Some(p)
                } else {
                    if !prior.had_object {
                        return Err(MoqError::proto("前の Priority が無い"));
                    }
                    prior.priority
                };
                let properties = if flags & 0x20 != 0 {
                    let Ok(n) = r.varint() else { return Ok(None) };
                    if n > 0xffff {
                        return Err(MoqError::proto("Object Properties が長すぎる"));
                    }
                    let Ok(p) = r.bytes(n as usize) else { return Ok(None) };
                    let p = Bytes::copy_from_slice(p);
                    parse_kvps(&p)?;
                    p
                } else {
                    Bytes::new()
                };
                let Ok(len) = r.varint() else { return Ok(None) };
                if len > self.max_object as u64 {
                    return Err(MoqError::proto(format!("object が上限を超える ({len} バイト)")));
                }
                if r.remaining() < len as usize {
                    return Ok(None);
                }
                let hdr = r.pos();
                *prior = FetchPriorIn {
                    group: Some(group),
                    object,
                    subgroup,
                    priority,
                    had_object: true,
                };
                self.buf.advance(hdr);
                let payload = self.buf.split_to(len as usize).freeze();
                Ok(Some(StreamItem::Object(ObjectData {
                    group,
                    subgroup,
                    object,
                    priority,
                    properties,
                    status: ObjectStatus::Normal,
                    payload,
                })))
            }
            RState::Padding | RState::Control => Ok(None),
        }
    }
}

/// 型の varint を読んだだけで制御 stream か分かるか（先頭のバイト列）。
pub fn peek_stream_type(b: &[u8]) -> Option<u64> {
    varint::read(b).map(|(v, _)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(group: u64, object: u64, payload: &'static [u8]) -> ObjectData {
        ObjectData {
            group,
            subgroup: Some(0),
            object,
            priority: Some(5),
            properties: Bytes::new(),
            status: ObjectStatus::Normal,
            payload: Bytes::from_static(payload),
        }
    }

    #[test]
    fn subgroup_stream_roundtrip_bytewise() {
        let h = SubgroupHeader {
            track_alias: 7,
            group: 3,
            subgroup: SubgroupId::Explicit(0),
            priority: Some(5),
            has_properties: true,
            end_of_group: true,
            first_object: true,
        };
        let mut s = h.encode();
        let props = {
            let mut p = Vec::new();
            crate::params::write_kvps(&mut p, &[(0x78, crate::params::KvValue::Int(1))]);
            p
        };
        s.extend(encode_subgroup_object(None, 0, true, &props, ObjectStatus::Normal, 3));
        s.extend_from_slice(b"abc");
        s.extend(encode_subgroup_object(Some(0), 1, true, &[], ObjectStatus::Normal, 0));
        let mut r = DataStreamReader::new(1 << 20);
        let mut items = Vec::new();
        for b in &s {
            r.push(&[*b]);
            while let Some(i) = r.next_item().unwrap() {
                items.push(i);
            }
        }
        assert_eq!(items[0], StreamItem::Header(StreamHeader::Subgroup(h)));
        let StreamItem::Object(o) = &items[1] else { panic!() };
        assert_eq!((o.group, o.object, &o.payload[..]), (3, 0, &b"abc"[..]));
        assert_eq!(&o.properties[..], &props[..]);
        let StreamItem::Object(o) = &items[2] else { panic!() };
        assert_eq!((o.object, o.status, o.payload.len()), (1, ObjectStatus::Normal, 0));
        assert!(!r.mid_object());
    }

    #[test]
    fn fetch_stream_roundtrip() {
        for order in [GroupOrder::Ascending, GroupOrder::Descending] {
            let objs = match order {
                GroupOrder::Ascending => vec![obj(1, 0, b"a"), obj(1, 1, b"b"), obj(4, 0, b"c")],
                GroupOrder::Descending => vec![obj(4, 0, b"c"), obj(4, 2, b"d"), obj(1, 0, b"a")],
            };
            let mut s = encode_fetch_header(9);
            let mut prior = FetchPrior::default();
            for o in &objs {
                s.extend(encode_fetch_object(&mut prior, order, o));
                s.extend_from_slice(&o.payload);
            }
            s.extend(encode_fetch_range_end(&mut prior, RangeEnd::Unknown, 0, 5));
            let mut r = DataStreamReader::new(1 << 20);
            r.fetch_order = order;
            r.push(&s);
            assert_eq!(r.next_item().unwrap(), Some(StreamItem::Header(StreamHeader::Fetch { request_id: 9 })));
            for o in &objs {
                let Some(StreamItem::Object(g)) = r.next_item().unwrap() else { panic!() };
                assert_eq!((g.group, g.object, g.payload.clone()), (o.group, o.object, o.payload.clone()));
            }
            assert!(matches!(r.next_item().unwrap(), Some(StreamItem::RangeEnd { kind: RangeEnd::Unknown, .. })));
        }
    }

    #[test]
    fn datagram_roundtrip_and_validation() {
        let mut o = obj(2, 0, b"payload");
        o.subgroup = None;
        let d = encode_datagram(4, &o, true);
        assert_eq!(
            decode_datagram(Bytes::from(d)).unwrap(),
            Datagram::Object {
                track_alias: 4,
                object: o.clone(),
                end_of_group: true
            }
        );
        // STATUS と END_OF_GROUP の両方
        assert!(decode_datagram(Bytes::from_static(&[0x22, 0, 0, 0])).is_err());
        // 予約ビット
        assert!(decode_datagram(Bytes::from_static(&[0x10, 0, 0])).is_err());
        assert!(decode_datagram(Bytes::from_static(&[0x40, 0, 0])).is_err());
    }

    #[test]
    fn rejects_bad_streams() {
        let mut r = DataStreamReader::new(10);
        r.push(&[0x77]); // 未知
        assert!(r.next_item().is_err());
        let mut r = DataStreamReader::new(10);
        r.push(&[0x16, 0x01, 0x01]); // SUBGROUP_ID_MODE=0b11
        assert!(r.next_item().is_err());
        let mut r = DataStreamReader::new(10);
        let h = SubgroupHeader {
            track_alias: 1,
            group: 1,
            subgroup: SubgroupId::Zero,
            priority: None,
            has_properties: false,
            end_of_group: false,
            first_object: false,
        };
        r.push(&h.encode());
        r.next_item().unwrap();
        r.push(&encode_subgroup_object(None, 0, false, &[], ObjectStatus::Normal, 100)); // 上限超え
        assert!(r.next_item().is_err());
        // fetch の最初が差分なし
        let mut r = DataStreamReader::new(10);
        r.push(&encode_fetch_header(1));
        r.next_item().unwrap();
        r.push(&[0x10, 0x05, 0x00]);
        assert!(r.next_item().is_err());
    }
}
