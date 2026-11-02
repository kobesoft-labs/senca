//! SCTP パケットとチャンクの読み書き（RFC 9260 3、RFC 3758 3.2、RFC 6525 3・4）。
//!
//! 読むときは長さをすべて確かめ、壊れていれば `Err(Malformed)` を返す。
//! payload は元の `Bytes` からの切り出しで持つ。

use bytes::{BufMut, Bytes};

use crate::SctpError;
use crate::crc32c;

/// 共通ヘッダの長さ。
pub const COMMON_HEADER_LEN: usize = 12;
/// DATA チャンクのヘッダの長さ。
pub const DATA_HEADER_LEN: usize = 16;
/// 受け付けるパケットの上限。
pub const MAX_PACKET_LEN: usize = 65535;
/// 1 つのパケットのチャンク数の上限。
pub const MAX_CHUNKS_PER_PACKET: usize = 512;
/// 報告する未知の INIT パラメタの数の上限。
pub const MAX_UNRECOGNIZED_PARAMS: usize = 8;
/// 報告する未知の INIT パラメタ 1 つの大きさの上限。
pub const MAX_UNRECOGNIZED_PARAM_LEN: usize = 128;
/// Supported Extensions の数の上限。
pub const MAX_SUPPORTED_EXTENSIONS: usize = 64;
/// RE-CONFIG のパラメタの数の上限（RFC 6525 は 2 つまで）。
pub const MAX_RECONFIG_PARAMS: usize = 4;

/// チャンクの型（RFC 9260 3.2・RFC 3758・RFC 6525）。
pub mod ct {
    pub const DATA: u8 = 0;
    pub const INIT: u8 = 1;
    pub const INIT_ACK: u8 = 2;
    pub const SACK: u8 = 3;
    pub const HEARTBEAT: u8 = 4;
    pub const HEARTBEAT_ACK: u8 = 5;
    pub const ABORT: u8 = 6;
    pub const SHUTDOWN: u8 = 7;
    pub const SHUTDOWN_ACK: u8 = 8;
    pub const ERROR: u8 = 9;
    pub const COOKIE_ECHO: u8 = 10;
    pub const COOKIE_ACK: u8 = 11;
    pub const SHUTDOWN_COMPLETE: u8 = 14;
    pub const RE_CONFIG: u8 = 0x82;
    pub const FORWARD_TSN: u8 = 0xC0;
}

/// INIT・INIT-ACK のパラメタの型。
pub mod pt {
    pub const IPV4: u16 = 5;
    pub const IPV6: u16 = 6;
    pub const STATE_COOKIE: u16 = 7;
    pub const UNRECOGNIZED: u16 = 8;
    pub const COOKIE_PRESERVATIVE: u16 = 9;
    pub const HOST_NAME: u16 = 11;
    pub const SUPPORTED_ADDRESS_TYPES: u16 = 12;
    pub const ECN_CAPABLE: u16 = 0x8000;
    pub const ZERO_CHECKSUM_ACCEPTABLE: u16 = 0x8001;
    pub const RANDOM: u16 = 0x8002;
    pub const CHUNK_LIST: u16 = 0x8003;
    pub const HMAC_ALGO: u16 = 0x8004;
    pub const SUPPORTED_EXTENSIONS: u16 = 0x8008;
    pub const FORWARD_TSN_SUPPORTED: u16 = 0xC000;
    pub const ADAPTATION_LAYER: u16 = 0xC006;
    pub const HEARTBEAT_INFO: u16 = 1;
}

/// RE-CONFIG の結果（RFC 6525 4.4）。
pub mod reconfig_result {
    pub const SUCCESS_NOTHING_TO_DO: u32 = 0;
    pub const SUCCESS_PERFORMED: u32 = 1;
    pub const DENIED: u32 = 2;
    pub const ERROR_WRONG_SSN: u32 = 3;
    pub const ERROR_IN_PROGRESS: u32 = 4;
    pub const ERROR_BAD_SEQUENCE: u32 = 5;
    pub const IN_PROGRESS: u32 = 6;
}

fn malformed(s: &'static str) -> SctpError {
    SctpError::Malformed(s)
}

pub(crate) fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

fn be16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// DATA チャンク。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataChunk {
    pub tsn: u32,
    pub stream: u16,
    pub ssn: u16,
    pub ppid: u32,
    pub unordered: bool,
    pub begin: bool,
    pub end: bool,
    /// I ビット（RFC 7053）。読むだけで使わない。
    pub immediate: bool,
    pub payload: Bytes,
}

/// INIT・INIT-ACK。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InitChunk {
    pub initiate_tag: u32,
    pub a_rwnd: u32,
    pub num_outbound_streams: u16,
    pub num_inbound_streams: u16,
    pub initial_tsn: u32,
    /// Forward-TSN-Supported（0xC000）があった・付ける。
    pub forward_tsn_supported: bool,
    /// Supported Extensions（0x8008）のチャンク型の一覧。
    pub supported_extensions: Vec<u8>,
    /// State Cookie（INIT-ACK だけ）。
    pub state_cookie: Option<Bytes>,
    /// 報告すべき未知のパラメタ（TLV のまま）。INIT-ACK では Unrecognized Parameter として書く。
    pub unrecognized: Vec<Bytes>,
}

impl InitChunk {
    /// 相手が FORWARD-TSN を使える。
    pub fn supports_forward_tsn(&self) -> bool {
        self.forward_tsn_supported || self.supported_extensions.contains(&ct::FORWARD_TSN)
    }
    /// 相手が RE-CONFIG を使える。
    pub fn supports_reconfig(&self) -> bool {
        self.supported_extensions.contains(&ct::RE_CONFIG)
    }
}

/// SACK。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SackChunk {
    pub cum_tsn_ack: u32,
    pub a_rwnd: u32,
    /// cumulative TSN からの相対位置（start, end）。
    pub gap_blocks: Vec<(u16, u16)>,
    pub dup_tsns: Vec<u32>,
}

/// RE-CONFIG のパラメタ（RFC 6525 4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconfigParam {
    /// Outgoing SSN Reset Request（13）。
    OutgoingReset {
        req_seq: u32,
        resp_seq: u32,
        last_tsn: u32,
        streams: Vec<u16>,
    },
    /// Incoming SSN Reset Request（14）。
    IncomingReset { req_seq: u32, streams: Vec<u16> },
    /// SSN/TSN Reset Request（15）。
    SsnTsnReset { req_seq: u32 },
    /// Re-configuration Response（16）。
    Response {
        resp_seq: u32,
        result: u32,
        next_tsns: Option<(u32, u32)>,
    },
    /// Add Outgoing Streams（17）。
    AddOutgoing { req_seq: u32, count: u16 },
    /// Add Incoming Streams（18）。
    AddIncoming { req_seq: u32, count: u16 },
    /// 知らないパラメタ（読み飛ばす）。
    Unknown { param_type: u16 },
}

/// チャンク。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chunk {
    Data(DataChunk),
    Init(InitChunk),
    InitAck(InitChunk),
    Sack(SackChunk),
    /// HEARTBEAT（値はパラメタ列のまま）。
    Heartbeat {
        info: Bytes,
    },
    HeartbeatAck {
        info: Bytes,
    },
    Abort {
        t_bit: bool,
        causes: Bytes,
    },
    Shutdown {
        cum_tsn_ack: u32,
    },
    ShutdownAck,
    Error {
        causes: Bytes,
    },
    CookieEcho {
        cookie: Bytes,
    },
    CookieAck,
    ShutdownComplete {
        t_bit: bool,
    },
    ForwardTsn {
        new_cum_tsn: u32,
        streams: Vec<(u16, u16)>,
    },
    Reconfig {
        params: Vec<ReconfigParam>,
    },
    /// 知らないチャンク（`raw` はヘッダを含むチャンク全体、詰め物を除く）。
    Unknown {
        chunk_type: u8,
        raw: Bytes,
    },
}

/// パケット。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub src_port: u16,
    pub dst_port: u16,
    pub vtag: u32,
    pub chunks: Vec<Chunk>,
}

/// パケットを読む（checksum を確かめる）。
pub fn parse_packet(raw: Bytes) -> Result<Packet, SctpError> {
    if raw.len() < COMMON_HEADER_LEN + 4 {
        return Err(malformed("short packet"));
    }
    if raw.len() > MAX_PACKET_LEN {
        return Err(malformed("packet too long"));
    }
    let want = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
    if crc32c::packet_checksum(&raw) != want {
        return Err(malformed("checksum"));
    }
    let src_port = be16(&raw, 0);
    let dst_port = be16(&raw, 2);
    let vtag = be32(&raw, 4);
    let mut chunks = Vec::new();
    let mut off = COMMON_HEADER_LEN;
    while off < raw.len() {
        if raw.len() - off < 4 {
            return Err(malformed("truncated chunk header"));
        }
        let ty = raw[off];
        let flags = raw[off + 1];
        let len = be16(&raw, off + 2) as usize;
        if len < 4 || off + len > raw.len() {
            return Err(malformed("chunk length"));
        }
        if chunks.len() >= MAX_CHUNKS_PER_PACKET {
            return Err(malformed("too many chunks"));
        }
        let value = raw.slice(off + 4..off + len);
        match parse_chunk(ty, flags, value, raw.slice(off..off + len))? {
            Parsed::Continue(c) => chunks.push(c),
            Parsed::Stop(c) => {
                chunks.push(c);
                break;
            }
        }
        off += pad4(len);
    }
    Ok(Packet {
        src_port,
        dst_port,
        vtag,
        chunks,
    })
}

enum Parsed {
    Continue(Chunk),
    /// 以降のチャンクを読まない（未知のチャンクで上位 2 bit が 00・01）。
    Stop(Chunk),
}

fn parse_chunk(ty: u8, flags: u8, v: Bytes, raw: Bytes) -> Result<Parsed, SctpError> {
    let c = match ty {
        ct::DATA => {
            if v.len() < 12 {
                return Err(malformed("DATA length"));
            }
            if v.len() == 12 {
                return Err(malformed("DATA without user data"));
            }
            Chunk::Data(DataChunk {
                tsn: be32(&v, 0),
                stream: be16(&v, 4),
                ssn: be16(&v, 6),
                ppid: be32(&v, 8),
                unordered: flags & 0x04 != 0,
                begin: flags & 0x02 != 0,
                end: flags & 0x01 != 0,
                immediate: flags & 0x08 != 0,
                payload: v.slice(12..),
            })
        }
        ct::INIT => Chunk::Init(parse_init(&v)?),
        ct::INIT_ACK => Chunk::InitAck(parse_init(&v)?),
        ct::SACK => Chunk::Sack(parse_sack(&v)?),
        ct::HEARTBEAT => Chunk::Heartbeat { info: v },
        ct::HEARTBEAT_ACK => Chunk::HeartbeatAck { info: v },
        ct::ABORT => Chunk::Abort {
            t_bit: flags & 1 != 0,
            causes: v,
        },
        ct::SHUTDOWN => {
            if v.len() != 4 {
                return Err(malformed("SHUTDOWN length"));
            }
            Chunk::Shutdown {
                cum_tsn_ack: be32(&v, 0),
            }
        }
        ct::SHUTDOWN_ACK => Chunk::ShutdownAck,
        ct::ERROR => Chunk::Error { causes: v },
        ct::COOKIE_ECHO => Chunk::CookieEcho { cookie: v },
        ct::COOKIE_ACK => Chunk::CookieAck,
        ct::SHUTDOWN_COMPLETE => Chunk::ShutdownComplete {
            t_bit: flags & 1 != 0,
        },
        ct::FORWARD_TSN => {
            if v.len() < 4 || !(v.len() - 4).is_multiple_of(4) {
                return Err(malformed("FORWARD-TSN length"));
            }
            let streams = (0..(v.len() - 4) / 4)
                .map(|i| (be16(&v, 4 + i * 4), be16(&v, 6 + i * 4)))
                .collect();
            Chunk::ForwardTsn {
                new_cum_tsn: be32(&v, 0),
                streams,
            }
        }
        ct::RE_CONFIG => Chunk::Reconfig {
            params: parse_reconfig(&v)?,
        },
        _ => {
            let c = Chunk::Unknown {
                chunk_type: ty,
                raw,
            };
            return Ok(if ty & 0x80 == 0 {
                Parsed::Stop(c)
            } else {
                Parsed::Continue(c)
            });
        }
    };
    Ok(Parsed::Continue(c))
}

fn parse_init(v: &Bytes) -> Result<InitChunk, SctpError> {
    if v.len() < 16 {
        return Err(malformed("INIT length"));
    }
    let mut c = InitChunk {
        initiate_tag: be32(v, 0),
        a_rwnd: be32(v, 4),
        num_outbound_streams: be16(v, 8),
        num_inbound_streams: be16(v, 10),
        initial_tsn: be32(v, 12),
        ..Default::default()
    };
    let mut off = 16;
    while off < v.len() {
        if v.len() - off < 4 {
            return Err(malformed("truncated INIT parameter"));
        }
        let ty = be16(v, off);
        let len = be16(v, off + 2) as usize;
        if len < 4 || off + len > v.len() {
            return Err(malformed("INIT parameter length"));
        }
        let pv = v.slice(off + 4..off + len);
        match ty {
            pt::STATE_COOKIE => c.state_cookie = Some(pv),
            pt::FORWARD_TSN_SUPPORTED => c.forward_tsn_supported = true,
            pt::SUPPORTED_EXTENSIONS => {
                for &b in pv.iter() {
                    if c.supported_extensions.len() < MAX_SUPPORTED_EXTENSIONS
                        && !c.supported_extensions.contains(&b)
                    {
                        c.supported_extensions.push(b);
                    }
                }
            }
            // 知っているが使わないもの（DTLS の上なのでアドレスは意味がない。ECN・AUTH・
            // zero checksum は広告しないので無視する）。
            pt::IPV4
            | pt::IPV6
            | pt::UNRECOGNIZED
            | pt::COOKIE_PRESERVATIVE
            | pt::HOST_NAME
            | pt::SUPPORTED_ADDRESS_TYPES
            | pt::ECN_CAPABLE
            | pt::ZERO_CHECKSUM_ACCEPTABLE
            | pt::RANDOM
            | pt::CHUNK_LIST
            | pt::HMAC_ALGO
            | pt::ADAPTATION_LAYER => {}
            _ => {
                // 上位 2 bit で扱いを決める（RFC 9260 3.2.1）。
                let report = ty & 0x4000 != 0;
                if report
                    && c.unrecognized.len() < MAX_UNRECOGNIZED_PARAMS
                    && len <= MAX_UNRECOGNIZED_PARAM_LEN
                {
                    c.unrecognized.push(v.slice(off..off + len));
                }
                if ty & 0x8000 == 0 {
                    break;
                }
            }
        }
        off += pad4(len);
    }
    Ok(c)
}

fn parse_sack(v: &Bytes) -> Result<SackChunk, SctpError> {
    if v.len() < 12 {
        return Err(malformed("SACK length"));
    }
    let ng = be16(v, 8) as usize;
    let nd = be16(v, 10) as usize;
    if v.len() < 12 + 4 * ng + 4 * nd {
        return Err(malformed("SACK length"));
    }
    let gap_blocks = (0..ng)
        .map(|i| (be16(v, 12 + i * 4), be16(v, 14 + i * 4)))
        .collect();
    let base = 12 + 4 * ng;
    let dup_tsns = (0..nd).map(|i| be32(v, base + i * 4)).collect();
    Ok(SackChunk {
        cum_tsn_ack: be32(v, 0),
        a_rwnd: be32(v, 4),
        gap_blocks,
        dup_tsns,
    })
}

fn parse_streams(v: &[u8]) -> Result<Vec<u16>, SctpError> {
    if !v.len().is_multiple_of(2) {
        return Err(malformed("stream list length"));
    }
    Ok((0..v.len() / 2).map(|i| be16(v, i * 2)).collect())
}

fn parse_reconfig(v: &Bytes) -> Result<Vec<ReconfigParam>, SctpError> {
    let mut out = Vec::new();
    let mut off = 0;
    while off < v.len() {
        if v.len() - off < 4 {
            return Err(malformed("truncated RE-CONFIG parameter"));
        }
        if out.len() >= MAX_RECONFIG_PARAMS {
            return Err(malformed("too many RE-CONFIG parameters"));
        }
        let ty = be16(v, off);
        let len = be16(v, off + 2) as usize;
        if len < 4 || off + len > v.len() {
            return Err(malformed("RE-CONFIG parameter length"));
        }
        let p = &v[off + 4..off + len];
        let param = match ty {
            13 => {
                if p.len() < 12 {
                    return Err(malformed("outgoing reset length"));
                }
                ReconfigParam::OutgoingReset {
                    req_seq: be32(p, 0),
                    resp_seq: be32(p, 4),
                    last_tsn: be32(p, 8),
                    streams: parse_streams(&p[12..])?,
                }
            }
            14 => {
                if p.len() < 4 {
                    return Err(malformed("incoming reset length"));
                }
                ReconfigParam::IncomingReset {
                    req_seq: be32(p, 0),
                    streams: parse_streams(&p[4..])?,
                }
            }
            15 => {
                if p.len() != 4 {
                    return Err(malformed("SSN/TSN reset length"));
                }
                ReconfigParam::SsnTsnReset {
                    req_seq: be32(p, 0),
                }
            }
            16 => match p.len() {
                8 => ReconfigParam::Response {
                    resp_seq: be32(p, 0),
                    result: be32(p, 4),
                    next_tsns: None,
                },
                16 => ReconfigParam::Response {
                    resp_seq: be32(p, 0),
                    result: be32(p, 4),
                    next_tsns: Some((be32(p, 8), be32(p, 12))),
                },
                _ => return Err(malformed("reconfig response length")),
            },
            17 | 18 => {
                if p.len() != 8 {
                    return Err(malformed("add streams length"));
                }
                let req_seq = be32(p, 0);
                let count = be16(p, 4);
                if ty == 17 {
                    ReconfigParam::AddOutgoing { req_seq, count }
                } else {
                    ReconfigParam::AddIncoming { req_seq, count }
                }
            }
            _ => ReconfigParam::Unknown { param_type: ty },
        };
        out.push(param);
        off += pad4(len);
    }
    Ok(out)
}

fn put_param(out: &mut Vec<u8>, ty: u16, value: &[u8]) {
    out.put_u16(ty);
    out.put_u16((4 + value.len()) as u16);
    out.put_slice(value);
    pad_to4(out);
}

fn pad_to4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

impl ReconfigParam {
    fn write(&self, out: &mut Vec<u8>) {
        let mut v = Vec::new();
        let ty = match self {
            ReconfigParam::OutgoingReset {
                req_seq,
                resp_seq,
                last_tsn,
                streams,
            } => {
                v.put_u32(*req_seq);
                v.put_u32(*resp_seq);
                v.put_u32(*last_tsn);
                for s in streams {
                    v.put_u16(*s);
                }
                13
            }
            ReconfigParam::IncomingReset { req_seq, streams } => {
                v.put_u32(*req_seq);
                for s in streams {
                    v.put_u16(*s);
                }
                14
            }
            ReconfigParam::SsnTsnReset { req_seq } => {
                v.put_u32(*req_seq);
                15
            }
            ReconfigParam::Response {
                resp_seq,
                result,
                next_tsns,
            } => {
                v.put_u32(*resp_seq);
                v.put_u32(*result);
                if let Some((a, b)) = next_tsns {
                    v.put_u32(*a);
                    v.put_u32(*b);
                }
                16
            }
            ReconfigParam::AddOutgoing { req_seq, count }
            | ReconfigParam::AddIncoming { req_seq, count } => {
                v.put_u32(*req_seq);
                v.put_u16(*count);
                v.put_u16(0);
                if matches!(self, ReconfigParam::AddOutgoing { .. }) {
                    17
                } else {
                    18
                }
            }
            ReconfigParam::Unknown { param_type } => *param_type,
        };
        put_param(out, ty, &v);
    }
}

fn write_init(out: &mut Vec<u8>, c: &InitChunk) {
    out.put_u32(c.initiate_tag);
    out.put_u32(c.a_rwnd);
    out.put_u16(c.num_outbound_streams);
    out.put_u16(c.num_inbound_streams);
    out.put_u32(c.initial_tsn);
    if let Some(cookie) = &c.state_cookie {
        put_param(out, pt::STATE_COOKIE, cookie);
    }
    if c.forward_tsn_supported {
        put_param(out, pt::FORWARD_TSN_SUPPORTED, &[]);
    }
    if !c.supported_extensions.is_empty() {
        put_param(out, pt::SUPPORTED_EXTENSIONS, &c.supported_extensions);
    }
    for u in &c.unrecognized {
        put_param(out, pt::UNRECOGNIZED, u);
    }
}

impl Chunk {
    /// 詰め物を含む長さ。
    pub fn encoded_len(&self) -> usize {
        let mut v = Vec::new();
        self.write(&mut v);
        v.len()
    }

    /// チャンクを書く（詰め物を含む）。
    pub fn write(&self, out: &mut Vec<u8>) {
        let start = out.len();
        let (ty, flags) = self.type_and_flags();
        out.put_u8(ty);
        out.put_u8(flags);
        out.put_u16(0);
        match self {
            Chunk::Data(d) => {
                out.put_u32(d.tsn);
                out.put_u16(d.stream);
                out.put_u16(d.ssn);
                out.put_u32(d.ppid);
                out.put_slice(&d.payload);
            }
            Chunk::Init(c) | Chunk::InitAck(c) => write_init(out, c),
            Chunk::Sack(s) => {
                out.put_u32(s.cum_tsn_ack);
                out.put_u32(s.a_rwnd);
                out.put_u16(s.gap_blocks.len() as u16);
                out.put_u16(s.dup_tsns.len() as u16);
                for (a, b) in &s.gap_blocks {
                    out.put_u16(*a);
                    out.put_u16(*b);
                }
                for d in &s.dup_tsns {
                    out.put_u32(*d);
                }
            }
            Chunk::Heartbeat { info } | Chunk::HeartbeatAck { info } => out.put_slice(info),
            Chunk::Abort { causes, .. } | Chunk::Error { causes } => out.put_slice(causes),
            Chunk::Shutdown { cum_tsn_ack } => out.put_u32(*cum_tsn_ack),
            Chunk::ShutdownAck | Chunk::CookieAck | Chunk::ShutdownComplete { .. } => {}
            Chunk::CookieEcho { cookie } => out.put_slice(cookie),
            Chunk::ForwardTsn {
                new_cum_tsn,
                streams,
            } => {
                out.put_u32(*new_cum_tsn);
                for (s, n) in streams {
                    out.put_u16(*s);
                    out.put_u16(*n);
                }
            }
            Chunk::Reconfig { params } => {
                for p in params {
                    p.write(out);
                }
            }
            Chunk::Unknown { raw, .. } => {
                out.truncate(start);
                out.put_slice(raw);
                pad_to4(out);
                return;
            }
        }
        let len = out.len() - start;
        out[start + 2..start + 4].copy_from_slice(&(len as u16).to_be_bytes());
        pad_to4(out);
    }

    fn type_and_flags(&self) -> (u8, u8) {
        match self {
            Chunk::Data(d) => (
                ct::DATA,
                (d.immediate as u8) << 3
                    | (d.unordered as u8) << 2
                    | (d.begin as u8) << 1
                    | d.end as u8,
            ),
            Chunk::Init(_) => (ct::INIT, 0),
            Chunk::InitAck(_) => (ct::INIT_ACK, 0),
            Chunk::Sack(_) => (ct::SACK, 0),
            Chunk::Heartbeat { .. } => (ct::HEARTBEAT, 0),
            Chunk::HeartbeatAck { .. } => (ct::HEARTBEAT_ACK, 0),
            Chunk::Abort { t_bit, .. } => (ct::ABORT, *t_bit as u8),
            Chunk::Shutdown { .. } => (ct::SHUTDOWN, 0),
            Chunk::ShutdownAck => (ct::SHUTDOWN_ACK, 0),
            Chunk::Error { .. } => (ct::ERROR, 0),
            Chunk::CookieEcho { .. } => (ct::COOKIE_ECHO, 0),
            Chunk::CookieAck => (ct::COOKIE_ACK, 0),
            Chunk::ShutdownComplete { t_bit } => (ct::SHUTDOWN_COMPLETE, *t_bit as u8),
            Chunk::ForwardTsn { .. } => (ct::FORWARD_TSN, 0),
            Chunk::Reconfig { .. } => (ct::RE_CONFIG, 0),
            Chunk::Unknown { chunk_type, .. } => (*chunk_type, 0),
        }
    }
}

/// DATA チャンクの詰め物込みの長さ。
pub(crate) fn data_chunk_len(payload: usize) -> usize {
    pad4(DATA_HEADER_LEN + payload)
}

/// パケットを書く（checksum を little-endian で入れる）。
pub fn serialize_packet(src_port: u16, dst_port: u16, vtag: u32, chunks: &[Chunk]) -> Bytes {
    let mut out = Vec::with_capacity(256);
    out.put_u16(src_port);
    out.put_u16(dst_port);
    out.put_u32(vtag);
    out.put_u32(0);
    for c in chunks {
        c.write(&mut out);
    }
    let crc = crc32c::packet_checksum(&out);
    out[8..12].copy_from_slice(&crc.to_le_bytes());
    Bytes::from(out)
}

/// 未知のチャンクを報告する ERROR の原因（Unrecognized Chunk Type、原因コード 6）。
pub(crate) fn unrecognized_chunk_cause(raw: &[u8]) -> Bytes {
    let mut v = Vec::new();
    v.put_u16(6);
    v.put_u16((4 + raw.len()) as u16);
    v.put_slice(raw);
    pad_to4(&mut v);
    Bytes::from(v)
}

/// 未知のパラメタを報告する ERROR の原因（Unrecognized Parameters、原因コード 8）。
pub(crate) fn unrecognized_params_cause(params: &[Bytes]) -> Bytes {
    let mut v = Vec::new();
    v.put_u16(8);
    v.put_u16(0);
    for p in params {
        v.put_slice(p);
        pad_to4(&mut v);
    }
    let len = v.len() as u16;
    v[2..4].copy_from_slice(&len.to_be_bytes());
    Bytes::from(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(c: Chunk) {
        let p = serialize_packet(5000, 5000, 0x1234_5678, std::slice::from_ref(&c));
        assert_eq!(p.len() % 4, 0);
        let q = parse_packet(p).expect("parse");
        assert_eq!(q.src_port, 5000);
        assert_eq!(q.vtag, 0x1234_5678);
        assert_eq!(q.chunks, vec![c]);
    }

    #[test]
    fn round_trips() {
        rt(Chunk::Data(DataChunk {
            tsn: 0xffff_fff0,
            stream: 7,
            ssn: 65535,
            ppid: 51,
            unordered: true,
            begin: true,
            end: false,
            immediate: false,
            payload: Bytes::from_static(b"hello"),
        }));
        rt(Chunk::Init(InitChunk {
            initiate_tag: 1,
            a_rwnd: 131072,
            num_outbound_streams: 65535,
            num_inbound_streams: 65535,
            initial_tsn: 99,
            forward_tsn_supported: true,
            supported_extensions: vec![ct::FORWARD_TSN, ct::RE_CONFIG],
            state_cookie: None,
            unrecognized: vec![],
        }));
        rt(Chunk::InitAck(InitChunk {
            initiate_tag: 2,
            a_rwnd: 1,
            num_outbound_streams: 1,
            num_inbound_streams: 2,
            initial_tsn: 3,
            forward_tsn_supported: false,
            supported_extensions: vec![ct::RE_CONFIG],
            state_cookie: Some(Bytes::from_static(b"cookie-123")),
            unrecognized: vec![],
        }));
        rt(Chunk::Sack(SackChunk {
            cum_tsn_ack: 10,
            a_rwnd: 5000,
            gap_blocks: vec![(2, 3), (5, 9)],
            dup_tsns: vec![7, 8],
        }));
        rt(Chunk::Heartbeat {
            info: Bytes::from_static(&[0, 1, 0, 8, 1, 2, 3, 4]),
        });
        rt(Chunk::HeartbeatAck {
            info: Bytes::from_static(&[0, 1, 0, 8, 1, 2, 3, 4]),
        });
        rt(Chunk::Abort {
            t_bit: true,
            causes: Bytes::new(),
        });
        rt(Chunk::Shutdown { cum_tsn_ack: 77 });
        rt(Chunk::ShutdownAck);
        rt(Chunk::Error {
            causes: unrecognized_chunk_cause(&[0x40, 0, 0, 4]),
        });
        rt(Chunk::CookieEcho {
            cookie: Bytes::from_static(b"abc"),
        });
        rt(Chunk::CookieAck);
        rt(Chunk::ShutdownComplete { t_bit: false });
        rt(Chunk::ForwardTsn {
            new_cum_tsn: 100,
            streams: vec![(1, 2), (3, 4)],
        });
        rt(Chunk::Reconfig {
            params: vec![
                ReconfigParam::OutgoingReset {
                    req_seq: 1,
                    resp_seq: 2,
                    last_tsn: 3,
                    streams: vec![1, 2, 3],
                },
                ReconfigParam::Response {
                    resp_seq: 5,
                    result: 1,
                    next_tsns: None,
                },
            ],
        });
        rt(Chunk::Reconfig {
            params: vec![
                ReconfigParam::IncomingReset {
                    req_seq: 1,
                    streams: vec![],
                },
                ReconfigParam::Response {
                    resp_seq: 5,
                    result: 6,
                    next_tsns: Some((1, 2)),
                },
            ],
        });
        rt(Chunk::Reconfig {
            params: vec![
                ReconfigParam::SsnTsnReset { req_seq: 9 },
                ReconfigParam::AddOutgoing {
                    req_seq: 10,
                    count: 4,
                },
            ],
        });
    }

    #[test]
    fn encoded_len_matches() {
        let c = Chunk::Reconfig {
            params: vec![ReconfigParam::OutgoingReset {
                req_seq: 1,
                resp_seq: 2,
                last_tsn: 3,
                streams: vec![1],
            }],
        };
        assert_eq!(c.encoded_len(), 4 + 20);
        assert_eq!(data_chunk_len(5), 24);
    }

    #[test]
    fn bad_checksum_rejected() {
        let p = serialize_packet(1, 2, 3, &[Chunk::CookieAck]);
        let mut v = p.to_vec();
        v[12] ^= 0;
        v[9] ^= 1;
        assert!(parse_packet(Bytes::from(v)).is_err());
    }

    #[test]
    fn unknown_chunk_actions() {
        // 0x40（01）で止まる・0x80（10）は読み飛ばして続ける。
        let mk = |ty: u8| Chunk::Unknown {
            chunk_type: ty,
            raw: Bytes::from(vec![ty, 0, 0, 4]),
        };
        let p = serialize_packet(
            1,
            2,
            3,
            &[mk(0x80), Chunk::CookieAck, mk(0x40), Chunk::CookieAck],
        );
        let q = parse_packet(p).unwrap();
        assert_eq!(q.chunks.len(), 3);
        assert_eq!(q.chunks[1], Chunk::CookieAck);
    }

    #[test]
    fn init_unknown_params() {
        let mut v = Vec::new();
        v.put_u32(1);
        v.put_u32(2);
        v.put_u16(3);
        v.put_u16(4);
        v.put_u32(5);
        put_param(&mut v, 0xC123, &[1, 2, 3]); // 11: 読み飛ばして報告
        put_param(&mut v, 0x8123, &[1]); // 10: 読み飛ばす
        put_param(&mut v, 0x4123, &[]); // 01: 止めて報告
        put_param(&mut v, pt::FORWARD_TSN_SUPPORTED, &[]);
        let c = parse_init(&Bytes::from(v)).unwrap();
        assert_eq!(c.unrecognized.len(), 2);
        assert!(!c.forward_tsn_supported);
    }

    #[test]
    fn truncated_inputs_rejected() {
        let p = serialize_packet(
            1,
            2,
            3,
            &[Chunk::Sack(SackChunk {
                cum_tsn_ack: 1,
                a_rwnd: 2,
                gap_blocks: vec![(1, 1)],
                dup_tsns: vec![],
            })],
        );
        for n in 0..p.len() {
            let mut v = p[..n].to_vec();
            if v.len() >= 12 {
                let c = crc32c::packet_checksum(&v);
                v[8..12].copy_from_slice(&c.to_le_bytes());
            }
            let _ = parse_packet(Bytes::from(v));
        }
    }
}
