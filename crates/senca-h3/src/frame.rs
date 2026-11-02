//! HTTP/3 のフレーム（RFC 9114 7 章）と SETTINGS。

use bytes::{Buf, Bytes, BytesMut};
use senca_quic::varint;

use crate::error::{H3Error, h3err};

pub(crate) mod ty {
    pub const DATA: u64 = 0x00;
    pub const HEADERS: u64 = 0x01;
    pub const CANCEL_PUSH: u64 = 0x03;
    pub const SETTINGS: u64 = 0x04;
    pub const PUSH_PROMISE: u64 = 0x05;
    pub const GOAWAY: u64 = 0x07;
    pub const MAX_PUSH_ID: u64 = 0x0d;
    /// WebTransport draft-07 の WEBTRANSPORT_STREAM（長さを持たない signal）。
    pub const WEBTRANSPORT_STREAM: u64 = 0x41;
}

pub(crate) mod stream_ty {
    pub const CONTROL: u64 = 0x00;
    pub const PUSH: u64 = 0x01;
    pub const QPACK_ENCODER: u64 = 0x02;
    pub const QPACK_DECODER: u64 = 0x03;
    pub const WEBTRANSPORT: u64 = 0x54;
}

mod setting {
    pub const QPACK_MAX_TABLE_CAPACITY: u64 = 0x01;
    pub const MAX_FIELD_SECTION_SIZE: u64 = 0x06;
    pub const QPACK_BLOCKED_STREAMS: u64 = 0x07;
    pub const ENABLE_CONNECT_PROTOCOL: u64 = 0x08;
    pub const H3_DATAGRAM: u64 = 0x33;
    pub const WEBTRANSPORT_MAX_SESSIONS_DRAFT07: u64 = 0xc671_706a;
    pub const WEBTRANSPORT_ENABLE_DRAFT02: u64 = 0x2b60_3742;
}

/// SETTINGS（RFC 9114 7.2.4、RFC 9204 5、RFC 9220、RFC 9297、WebTransport draft-07）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub qpack_max_table_capacity: u64,
    pub max_field_section_size: Option<u64>,
    pub qpack_blocked_streams: u64,
    pub enable_connect_protocol: bool,
    pub h3_datagram: bool,
    /// SETTINGS_WEBTRANSPORT_MAX_SESSIONS（draft-07）。0 なら WebTransport を受け付けない。
    pub webtransport_max_sessions: u64,
    /// SETTINGS_ENABLE_WEBTRANSPORT（draft-02、Chrome の既定）。
    pub webtransport_draft02: bool,
}

impl Settings {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut p = Vec::new();
        let mut put = |id: u64, v: u64| {
            varint::write(&mut p, id);
            varint::write(&mut p, v);
        };
        put(setting::QPACK_MAX_TABLE_CAPACITY, self.qpack_max_table_capacity);
        if let Some(m) = self.max_field_section_size {
            put(setting::MAX_FIELD_SECTION_SIZE, m);
        }
        put(setting::QPACK_BLOCKED_STREAMS, self.qpack_blocked_streams);
        if self.enable_connect_protocol {
            put(setting::ENABLE_CONNECT_PROTOCOL, 1);
        }
        if self.h3_datagram {
            put(setting::H3_DATAGRAM, 1);
        }
        if self.webtransport_max_sessions > 0 {
            put(setting::WEBTRANSPORT_MAX_SESSIONS_DRAFT07, self.webtransport_max_sessions);
        }
        if self.webtransport_draft02 {
            put(setting::WEBTRANSPORT_ENABLE_DRAFT02, 1);
        }
        // grease（RFC 9114 7.2.4.1: 0x1f * N + 0x21）
        put(0x1f * 3 + 0x21, 0);
        let mut f = Vec::new();
        varint::write(&mut f, ty::SETTINGS);
        varint::write(&mut f, p.len() as u64);
        f.extend_from_slice(&p);
        f
    }

    pub(crate) fn decode(mut p: &[u8]) -> Result<Self, H3Error> {
        let mut s = Self::default();
        let mut seen = Vec::new();
        while !p.is_empty() {
            let (id, a) = varint::read(p).ok_or_else(|| h3err!(FRAME_ERROR, "SETTINGS が切れている"))?;
            let (v, b) = varint::read(&p[a..]).ok_or_else(|| h3err!(FRAME_ERROR, "SETTINGS が切れている"))?;
            p = &p[a + b..];
            if seen.contains(&id) {
                return Err(h3err!(SETTINGS_ERROR, "SETTINGS の重複 0x{id:x}"));
            }
            if seen.len() < 64 {
                seen.push(id);
            }
            let bool01 = |v: u64| -> Result<bool, H3Error> {
                match v {
                    0 => Ok(false),
                    1 => Ok(true),
                    _ => Err(h3err!(SETTINGS_ERROR, "0・1 以外の値 0x{id:x}={v}")),
                }
            };
            match id {
                0x00 | 0x02 | 0x03 | 0x04 | 0x05 => return Err(h3err!(SETTINGS_ERROR, "HTTP/2 の SETTINGS 0x{id:x}")),
                setting::QPACK_MAX_TABLE_CAPACITY => s.qpack_max_table_capacity = v,
                setting::MAX_FIELD_SECTION_SIZE => s.max_field_section_size = Some(v),
                setting::QPACK_BLOCKED_STREAMS => s.qpack_blocked_streams = v,
                setting::ENABLE_CONNECT_PROTOCOL => s.enable_connect_protocol = bool01(v)?,
                setting::H3_DATAGRAM => s.h3_datagram = bool01(v)?,
                setting::WEBTRANSPORT_MAX_SESSIONS_DRAFT07 => s.webtransport_max_sessions = v,
                setting::WEBTRANSPORT_ENABLE_DRAFT02 => s.webtransport_draft02 = v != 0,
                _ => {} // 未知は無視
            }
        }
        Ok(s)
    }
}

pub(crate) fn frame_header(t: u64, len: usize) -> Vec<u8> {
    let mut o = Vec::with_capacity(16);
    varint::write(&mut o, t);
    varint::write(&mut o, len as u64);
    o
}

/// 読み出したフレーム（DATA は少しずつ渡す）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Frame {
    Data(Bytes),
    Headers(Bytes),
    Settings(Bytes),
    GoAway(u64),
    MaxPushId(u64),
    CancelPush(u64),
    PushPromise,
}

#[derive(Debug)]
enum State {
    Header,
    /// 溜める（HEADERS・SETTINGS など）。
    Collect { ty: u64, len: usize },
    /// DATA を少しずつ渡す。
    Data { remaining: u64 },
    /// 未知のフレームを読み捨てる。
    Skip { remaining: u64 },
}

/// stream 上のフレームを少しずつ読む。
#[derive(Debug)]
pub(crate) struct FrameReader {
    buf: BytesMut,
    state: State,
    max_collect: usize,
}

impl FrameReader {
    pub(crate) fn new(max_collect: usize) -> Self {
        Self {
            buf: BytesMut::new(),
            state: State::Header,
            max_collect,
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// フレームの途中か（FIN が来たら誤り）。
    pub(crate) fn mid_frame(&self) -> bool {
        !matches!(self.state, State::Header) || !self.buf.is_empty()
    }

    pub(crate) fn next(&mut self) -> Result<Option<Frame>, H3Error> {
        loop {
            match self.state {
                State::Header => {
                    let Some((t, a)) = varint::read(&self.buf) else { return Ok(None) };
                    if t == ty::WEBTRANSPORT_STREAM {
                        return Err(h3err!(FRAME_ERROR, "WEBTRANSPORT_STREAM が stream の先頭以外にある"));
                    }
                    let Some((len, b)) = varint::read(&self.buf[a..]) else { return Ok(None) };
                    self.buf.advance(a + b);
                    self.state = match t {
                        ty::DATA => State::Data { remaining: len },
                        ty::HEADERS | ty::SETTINGS | ty::GOAWAY | ty::MAX_PUSH_ID | ty::CANCEL_PUSH | ty::PUSH_PROMISE => {
                            let limit = if t == ty::HEADERS || t == ty::PUSH_PROMISE { self.max_collect } else { 4096 };
                            if len > limit as u64 {
                                return Err(h3err!(EXCESSIVE_LOAD, "フレーム 0x{t:x} が大きすぎる ({len})"));
                            }
                            State::Collect { ty: t, len: len as usize }
                        }
                        // HTTP/2 のフレーム型は使えない（RFC 9114 7.2.8）
                        0x02 | 0x06 | 0x08 | 0x09 => return Err(h3err!(FRAME_UNEXPECTED, "HTTP/2 のフレーム 0x{t:x}")),
                        _ => State::Skip { remaining: len },
                    };
                }
                State::Collect { ty: t, len } => {
                    if self.buf.len() < len {
                        return Ok(None);
                    }
                    let p = self.buf.split_to(len).freeze();
                    self.state = State::Header;
                    let one = |p: &Bytes| -> Result<u64, H3Error> {
                        match varint::read(p) {
                            Some((v, n)) if n == p.len() => Ok(v),
                            _ => Err(h3err!(FRAME_ERROR, "フレーム 0x{t:x} の長さが不正")),
                        }
                    };
                    return Ok(Some(match t {
                        ty::HEADERS => Frame::Headers(p),
                        ty::SETTINGS => Frame::Settings(p),
                        ty::GOAWAY => Frame::GoAway(one(&p)?),
                        ty::MAX_PUSH_ID => Frame::MaxPushId(one(&p)?),
                        ty::CANCEL_PUSH => Frame::CancelPush(one(&p)?),
                        _ => Frame::PushPromise,
                    }));
                }
                State::Data { remaining } => {
                    if remaining == 0 {
                        self.state = State::Header;
                        continue;
                    }
                    if self.buf.is_empty() {
                        return Ok(None);
                    }
                    let n = (remaining as usize).min(self.buf.len());
                    let d = self.buf.split_to(n).freeze();
                    self.state = State::Data {
                        remaining: remaining - n as u64,
                    };
                    return Ok(Some(Frame::Data(d)));
                }
                State::Skip { remaining } => {
                    if remaining == 0 {
                        self.state = State::Header;
                        continue;
                    }
                    if self.buf.is_empty() {
                        return Ok(None);
                    }
                    let n = (remaining as usize).min(self.buf.len());
                    self.buf.advance(n);
                    self.state = State::Skip {
                        remaining: remaining - n as u64,
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_and_errors() {
        let s = Settings {
            qpack_max_table_capacity: 4096,
            max_field_section_size: Some(32768),
            qpack_blocked_streams: 16,
            enable_connect_protocol: true,
            h3_datagram: true,
            webtransport_max_sessions: 4,
            webtransport_draft02: true,
        };
        let f = s.encode();
        let mut r = FrameReader::new(1024);
        r.push(&f);
        let Some(Frame::Settings(p)) = r.next().unwrap() else { panic!() };
        assert_eq!(Settings::decode(&p).unwrap(), s);
        assert!(Settings::decode(&[0x01, 0x00, 0x01, 0x00]).is_err()); // 重複
        assert!(Settings::decode(&[0x02, 0x00]).is_err()); // HTTP/2
        assert!(Settings::decode(&[0x08, 0x02]).is_err()); // 0/1 以外
    }

    #[test]
    fn fuzz_frames_and_settings() {
        let mut x: u64 = 0x0123_4567_89ab_cdef;
        for _ in 0..50_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x % 48) as usize;
            let d: Vec<u8> = (0..len).map(|i| (x >> (i % 8 * 8)) as u8 ^ i as u8).collect();
            let _ = Settings::decode(&d);
            let mut r = FrameReader::new(64);
            r.push(&d);
            for _ in 0..8 {
                match r.next() {
                    Ok(Some(_)) => {}
                    _ => break,
                }
            }
            let mut c = crate::capsule::CapsuleReader::default();
            let _ = c.push(&d);
            let _ = crate::headers::validate(&crate::headers::Headers::new().with(&d, &d), crate::headers::Kind::Request);
        }
    }

    #[test]
    fn reader_streams_data_and_skips_unknown() {
        let mut v = frame_header(0x21, 3);
        v.extend_from_slice(b"xyz");
        v.extend_from_slice(&frame_header(ty::DATA, 5));
        v.extend_from_slice(b"hello");
        v.extend_from_slice(&frame_header(ty::HEADERS, 2));
        v.extend_from_slice(b"hh");
        let mut r = FrameReader::new(100);
        let mut got = Vec::new();
        for b in v.chunks(2) {
            r.push(b);
            while let Some(f) = r.next().unwrap() {
                got.push(f);
            }
        }
        let data: Vec<u8> = got
            .iter()
            .filter_map(|f| if let Frame::Data(d) = f { Some(d.to_vec()) } else { None })
            .flatten()
            .collect();
        assert_eq!(data, b"hello");
        assert_eq!(got.last(), Some(&Frame::Headers(Bytes::from_static(b"hh"))));
        let mut r = FrameReader::new(100);
        r.push(&frame_header(ty::HEADERS, 1000));
        assert!(r.next().is_err());
        let mut r = FrameReader::new(100);
        r.push(&[0x06, 0x00]);
        assert!(r.next().is_err());
    }
}
