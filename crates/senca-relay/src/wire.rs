//! 接続の上の枠（版 1）。
//!
//! # 枠
//!
//! ```text
//! frame = type:u8  len:varint  body[len]        （len は相手の Hello の max_frame 以下）
//! ```
//!
//! | type | 名前 | body |
//! |---|---|---|
//! | 0x01 | Hello | `"SNCR"` version node_id capabilities max_frame |
//! | 0x02 | GoAway | code reason |
//! | 0x03 | Ping | nonce |
//! | 0x04 | Pong | nonce |
//! | 0x05 | Subscribe | channel priority:u8 stream_id tracks |
//! | 0x06 | SubscribeOk | channel |
//! | 0x07 | SubscribeErr | channel code reason |
//! | 0x08 | Unsubscribe | channel |
//! | 0x09 | Config | channel track codec_config |
//! | 0x0a | Media | channel track au_header payload（body の残り全部） |
//! | 0x0b | Batch | count ( channel track au_header len payload )* |
//! | 0x0c | Control | channel op:u8 ...（1 = キーフレームの要求 track、2 = トラック（層）の変更 tracks） |
//! | 0x0d | Event | channel kind:u8 ...（1 = 不連続、2 = 終わり、3 = セグメント） |
//! | 0x0e | Stats | rx_bytes rx_aus rx_gaps |
//!
//! 数は全て LEB128 の可変長整数（符号つきは zigzag）。文字列は長さ＋UTF-8。知らない type は読み飛ばす
//! （後方互換）。チャネル番号は購読する側が決める（向きごとに別の名前空間。使い回さない）。
//!
//! # AU の見出し
//!
//! トラックごとに直前の AU を覚えておき、差分だけを書く（[`TrackCodec`]）。
//!
//! ```text
//! flags:u8  （0x01 キーフレーム、0x02 不連続、0x04 完全、0x08 sequence が +1 でない、
//!            0x10 duration が前と違う、0x20 pts≠dts、0x40 撮影時刻あり、0x80 予約＝0）
//! 完全:     codec:u8 epoch config_generation tb_num tb_den sequence dts(zigzag)
//! 差分:     [sequence の差] dts の差(zigzag)
//! 続き:     [pts-dts(zigzag)] [duration] [撮影時刻の差(zigzag)]
//! ```
//!
//! 完全な見出しは、トラックの最初の AU と、codec・epoch・設定の世代・時間軸が変わったときだけ。
//! 普通の映像の AU の見出しは枠の見出しを含めて 8〜11 バイト、まとめた音声は 1 つ 5〜7 バイト。

use crate::event::{MAX_SEL_TRACKS, TrackSel};
use crate::varint::{self, put_i, put_u};
use bytes::{BufMut, Bytes, BytesMut};
use senca_core::media::{AudioParams, VideoParams};
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase, TrackId};
use std::fmt;

/// 規約の版（Hello で交換する。違えば GoAway で閉じる）。
pub const PROTOCOL_VERSION: u32 = 1;
pub const MAGIC: [u8; 4] = *b"SNCR";
/// 枠の大きさの既定の上限（4K のキーフレームでも収まる）。
pub const DEFAULT_MAX_FRAME: usize = 16 << 20;
/// 相手が名乗ってよい max_frame の範囲。
pub const MIN_MAX_FRAME: usize = 64 << 10;
pub const MAX_STREAM_ID: usize = 256;
pub const MAX_REASON: usize = 1024;
pub const MAX_BATCH_ITEMS: usize = 4096;
/// 設定レコードの上限。
pub const MAX_RECORD: usize = 1 << 20;

pub mod kind {
    pub const HELLO: u8 = 0x01;
    pub const GOAWAY: u8 = 0x02;
    pub const PING: u8 = 0x03;
    pub const PONG: u8 = 0x04;
    pub const SUBSCRIBE: u8 = 0x05;
    pub const SUBSCRIBE_OK: u8 = 0x06;
    pub const SUBSCRIBE_ERR: u8 = 0x07;
    pub const UNSUBSCRIBE: u8 = 0x08;
    pub const CONFIG: u8 = 0x09;
    pub const MEDIA: u8 = 0x0a;
    pub const BATCH: u8 = 0x0b;
    pub const CONTROL: u8 = 0x0c;
    pub const EVENT: u8 = 0x0d;
    pub const STATS: u8 = 0x0e;
}

/// Hello の capabilities の bit。
pub mod caps {
    /// Batch の枠を受けられる。
    pub const BATCH: u64 = 1 << 0;
    /// Stats の枠を送る。
    pub const STATS: u64 = 1 << 1;
    pub const ALL: u64 = BATCH | STATS;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// 枠の中身が足りない。
    Truncated,
    Malformed(&'static str),
    FrameTooLarge(usize),
    BadMagic,
    Version(u32),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Truncated => f.write_str("truncated frame"),
            WireError::Malformed(m) => write!(f, "malformed frame: {m}"),
            WireError::FrameTooLarge(n) => write!(f, "frame too large: {n} bytes"),
            WireError::BadMagic => f.write_str("not a senca relay peer"),
            WireError::Version(v) => write!(f, "unsupported protocol version {v}"),
        }
    }
}

impl std::error::Error for WireError {}

/// GoAway・SubscribeErr の理由の番号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ErrorCode(pub u32);

impl ErrorCode {
    pub const NORMAL: ErrorCode = ErrorCode(0);
    /// stream がこのノードに無い。
    pub const NOT_FOUND: ErrorCode = ErrorCode(1);
    /// 所有権が別のノードへ移った（ADR-0013）。
    pub const MOVED: ErrorCode = ErrorCode(2);
    pub const OVERLOADED: ErrorCode = ErrorCode(3);
    pub const GOING_AWAY: ErrorCode = ErrorCode(4);
    pub const BAD_REQUEST: ErrorCode = ErrorCode(5);
    pub const PROTOCOL: ErrorCode = ErrorCode(6);
    pub const VERSION: ErrorCode = ErrorCode(7);
    pub const TIMEOUT: ErrorCode = ErrorCode(8);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
    pub node_id: u64,
    pub capabilities: u64,
    /// 受けられる枠の最大の大きさ。
    pub max_frame: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlOp {
    /// キーフレームを要求する（購読する側 → 送る側）。
    KeyframeRequest(TrackId),
    /// 受けるトラック（サイマルキャストの層）を変える（購読する側 → 送る側）。
    SetTracks(TrackSel),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelEvent {
    Discontinuity,
    End,
    Segment {
        seq: u64,
        start_us: i64,
        duration_us: u64,
        discontinuity: bool,
    },
}

/// 受け手が数えた量（送り手が実際に届いた量を知るため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerStats {
    pub rx_bytes: u64,
    pub rx_aus: u64,
    /// sequence の飛び（送り手が捨てた・途中で失った）の数。
    pub rx_gaps: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullHeader {
    pub codec: Codec,
    pub epoch: u64,
    pub config_generation: u32,
    pub timebase: Timebase,
    pub sequence: u64,
    pub dts: i64,
}

/// 復号した（まだ直前の AU と組み合わせていない）AU の見出し。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuHeader {
    pub flags: u8,
    pub full: Option<FullHeader>,
    pub seq_delta: u64,
    pub dts_delta: i64,
    pub cts: i64,
    pub duration: Option<u64>,
    pub capture_delta: Option<i64>,
}

pub mod flag {
    pub const KEY: u8 = 0x01;
    pub const DISC: u8 = 0x02;
    pub const FULL: u8 = 0x04;
    pub const SEQ: u8 = 0x08;
    pub const DUR: u8 = 0x10;
    pub const CTS: u8 = 0x20;
    pub const CAP: u8 = 0x40;
    pub const RESERVED: u8 = 0x80;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaItem {
    pub channel: u32,
    pub track: TrackId,
    pub hdr: AuHeader,
    /// 受信バッファの切り出し（コピーしない）。
    pub payload: Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Hello(Hello),
    GoAway {
        code: ErrorCode,
        reason: String,
    },
    Ping(u64),
    Pong(u64),
    Subscribe {
        channel: u32,
        priority: u8,
        stream_id: String,
        tracks: TrackSel,
    },
    SubscribeOk {
        channel: u32,
    },
    SubscribeErr {
        channel: u32,
        code: ErrorCode,
        reason: String,
    },
    Unsubscribe {
        channel: u32,
    },
    Config {
        channel: u32,
        track: TrackId,
        config: CodecConfig,
    },
    Media(MediaItem),
    Batch(Vec<MediaItem>),
    Control {
        channel: u32,
        op: ControlOp,
    },
    Event {
        channel: u32,
        event: ChannelEvent,
    },
    Stats(PeerStats),
    /// 知らない type（読み飛ばした）。
    Unknown(u8),
}

pub fn codec_to_u8(c: Codec) -> u8 {
    match c {
        Codec::H264 => 1,
        Codec::H265 => 2,
        Codec::Av1 => 3,
        Codec::Vp8 => 4,
        Codec::Vp9 => 5,
        Codec::Aac => 6,
        Codec::Opus => 7,
        Codec::Pcma => 8,
        Codec::Pcmu => 9,
        Codec::WebVtt => 10,
    }
}

pub fn codec_from_u8(v: u8) -> Option<Codec> {
    Some(match v {
        1 => Codec::H264,
        2 => Codec::H265,
        3 => Codec::Av1,
        4 => Codec::Vp8,
        5 => Codec::Vp9,
        6 => Codec::Aac,
        7 => Codec::Opus,
        8 => Codec::Pcma,
        9 => Codec::Pcmu,
        10 => Codec::WebVtt,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// 読み

/// 枠の見出し（type と body の長さ）を読む。`Ok(None)` はバイトが足りない。
pub fn parse_frame_header(
    buf: &[u8],
    max_frame: usize,
) -> Result<Option<(u8, usize, usize)>, WireError> {
    let Some(&t) = buf.first() else {
        return Ok(None);
    };
    match varint::get_u(&buf[1..]) {
        Err(varint::BadVarint) => Err(WireError::Malformed("frame length")),
        Ok(None) => Ok(None),
        Ok(Some((len, n))) => {
            if len > max_frame as u64 {
                return Err(WireError::FrameTooLarge(len.min(usize::MAX as u64) as usize));
            }
            Ok(Some((t, 1 + n, len as usize)))
        }
    }
}

struct Rd<'a> {
    b: &'a Bytes,
    pos: usize,
}

impl<'a> Rd<'a> {
    fn new(b: &'a Bytes) -> Self {
        Rd { b, pos: 0 }
    }
    fn u8(&mut self) -> Result<u8, WireError> {
        let v = *self.b.get(self.pos).ok_or(WireError::Truncated)?;
        self.pos += 1;
        Ok(v)
    }
    fn u(&mut self) -> Result<u64, WireError> {
        match varint::get_u(&self.b[self.pos..]) {
            Ok(Some((v, n))) => {
                self.pos += n;
                Ok(v)
            }
            Ok(None) => Err(WireError::Truncated),
            Err(varint::BadVarint) => Err(WireError::Malformed("varint")),
        }
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        u32::try_from(self.u()?).map_err(|_| WireError::Malformed("value exceeds 32 bits"))
    }
    fn i(&mut self) -> Result<i64, WireError> {
        Ok(varint::unzigzag(self.u()?))
    }
    fn len(&mut self, max: usize) -> Result<usize, WireError> {
        let n = self.u()?;
        if n > max as u64 {
            return Err(WireError::Malformed("length over limit"));
        }
        let n = n as usize;
        if n > self.remaining() {
            return Err(WireError::Truncated);
        }
        Ok(n)
    }
    fn take(&mut self, n: usize) -> Result<Bytes, WireError> {
        if n > self.remaining() {
            return Err(WireError::Truncated);
        }
        let s = self.b.slice(self.pos..self.pos + n);
        self.pos += n;
        Ok(s)
    }
    fn str(&mut self, max: usize) -> Result<String, WireError> {
        let n = self.len(max)?;
        let s = std::str::from_utf8(&self.b[self.pos..self.pos + n])
            .map_err(|_| WireError::Malformed("utf-8"))?;
        self.pos += n;
        Ok(s.to_string())
    }
    fn remaining(&self) -> usize {
        self.b.len() - self.pos
    }
    fn rest(&mut self) -> Bytes {
        let s = self.b.slice(self.pos..);
        self.pos = self.b.len();
        s
    }
    fn end(&self) -> Result<(), WireError> {
        if self.pos != self.b.len() {
            return Err(WireError::Malformed("trailing bytes"));
        }
        Ok(())
    }
    fn track(&mut self) -> Result<TrackId, WireError> {
        Ok(TrackId(self.u32()?))
    }
    fn sel(&mut self) -> Result<TrackSel, WireError> {
        match self.u8()? {
            0 => Ok(TrackSel::All),
            1 => {
                let n = self.u()?;
                if n > MAX_SEL_TRACKS as u64 {
                    return Err(WireError::Malformed("too many tracks"));
                }
                let mut v = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    v.push(self.track()?);
                }
                Ok(TrackSel::only(v))
            }
            _ => Err(WireError::Malformed("track selection")),
        }
    }
    fn au_header(&mut self) -> Result<AuHeader, WireError> {
        let flags = self.u8()?;
        if flags & flag::RESERVED != 0 {
            return Err(WireError::Malformed("reserved flag"));
        }
        let mut h = AuHeader {
            flags,
            full: None,
            seq_delta: 1,
            dts_delta: 0,
            cts: 0,
            duration: None,
            capture_delta: None,
        };
        if flags & flag::FULL != 0 {
            if flags & flag::SEQ != 0 {
                return Err(WireError::Malformed("seq flag with full header"));
            }
            let codec = codec_from_u8(self.u8()?).ok_or(WireError::Malformed("codec"))?;
            let epoch = self.u()?;
            let config_generation = self.u32()?;
            let num = self.u32()?;
            let den = self.u32()?;
            if num == 0 || den == 0 {
                return Err(WireError::Malformed("timebase"));
            }
            let sequence = self.u()?;
            let dts = self.i()?;
            h.full = Some(FullHeader {
                codec,
                epoch,
                config_generation,
                timebase: Timebase::new(num, den),
                sequence,
                dts,
            });
        } else {
            if flags & flag::SEQ != 0 {
                h.seq_delta = self.u()?;
            }
            h.dts_delta = self.i()?;
        }
        if flags & flag::CTS != 0 {
            h.cts = self.i()?;
        }
        if flags & flag::DUR != 0 {
            h.duration = Some(self.u()?);
        }
        if flags & flag::CAP != 0 {
            h.capture_delta = Some(self.i()?);
        }
        Ok(h)
    }
    fn config(&mut self) -> Result<CodecConfig, WireError> {
        let codec = codec_from_u8(self.u8()?).ok_or(WireError::Malformed("codec"))?;
        let generation = self.u32()?;
        let n = self.len(MAX_RECORD)?;
        let record = self.take(n)?;
        let has = self.u8()?;
        if has & !3 != 0 {
            return Err(WireError::Malformed("config flags"));
        }
        let video = if has & 1 != 0 {
            Some(VideoParams {
                width: self.u32()?,
                height: self.u32()?,
                profile: self.u8()?,
                compat: self.u8()?,
                level: self.u8()?,
            })
        } else {
            None
        };
        let audio = if has & 2 != 0 {
            Some(AudioParams {
                sample_rate: self.u32()?,
                channels: self.u8()?,
                object_type: self.u8()?,
            })
        } else {
            None
        };
        Ok(CodecConfig {
            codec,
            generation,
            record,
            video,
            audio,
        })
    }
}

/// 枠の本体を読む。payload は `body` の切り出し。
pub fn decode_body(t: u8, body: &Bytes) -> Result<Frame, WireError> {
    let mut r = Rd::new(body);
    let f = match t {
        kind::HELLO => {
            let m = r.take(4)?;
            if m[..] != MAGIC {
                return Err(WireError::BadMagic);
            }
            let version = r.u32()?;
            if version != PROTOCOL_VERSION {
                // 版が違えば中身の形も違いうる。版だけを伝える
                return Ok(Frame::Hello(Hello {
                    version,
                    node_id: 0,
                    capabilities: 0,
                    max_frame: 0,
                }));
            }
            let h = Hello {
                version,
                node_id: r.u()?,
                capabilities: r.u()?,
                max_frame: r.u()?,
            };
            // 後の版で欄が増えても読めるよう、残りは読み飛ばす
            r.rest();
            Frame::Hello(h)
        }
        kind::GOAWAY => Frame::GoAway {
            code: ErrorCode(r.u32()?),
            reason: r.str(MAX_REASON)?,
        },
        kind::PING => Frame::Ping(r.u()?),
        kind::PONG => Frame::Pong(r.u()?),
        kind::SUBSCRIBE => {
            let channel = r.u32()?;
            let priority = r.u8()?;
            let stream_id = r.str(MAX_STREAM_ID)?;
            let tracks = r.sel()?;
            Frame::Subscribe {
                channel,
                priority,
                stream_id,
                tracks,
            }
        }
        kind::SUBSCRIBE_OK => Frame::SubscribeOk { channel: r.u32()? },
        kind::SUBSCRIBE_ERR => Frame::SubscribeErr {
            channel: r.u32()?,
            code: ErrorCode(r.u32()?),
            reason: r.str(MAX_REASON)?,
        },
        kind::UNSUBSCRIBE => Frame::Unsubscribe { channel: r.u32()? },
        kind::CONFIG => Frame::Config {
            channel: r.u32()?,
            track: r.track()?,
            config: r.config()?,
        },
        kind::MEDIA => {
            let channel = r.u32()?;
            let track = r.track()?;
            let hdr = r.au_header()?;
            Frame::Media(MediaItem {
                channel,
                track,
                hdr,
                payload: r.rest(),
            })
        }
        kind::BATCH => {
            let n = r.u()?;
            if n == 0 || n > MAX_BATCH_ITEMS as u64 {
                return Err(WireError::Malformed("batch count"));
            }
            let mut items = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let channel = r.u32()?;
                let track = r.track()?;
                let hdr = r.au_header()?;
                let len = r.len(usize::MAX)?;
                let payload = r.take(len)?;
                items.push(MediaItem {
                    channel,
                    track,
                    hdr,
                    payload,
                });
            }
            Frame::Batch(items)
        }
        kind::CONTROL => {
            let channel = r.u32()?;
            let op = match r.u8()? {
                1 => ControlOp::KeyframeRequest(r.track()?),
                2 => ControlOp::SetTracks(r.sel()?),
                _ => return Err(WireError::Malformed("control op")),
            };
            Frame::Control { channel, op }
        }
        kind::EVENT => {
            let channel = r.u32()?;
            let event = match r.u8()? {
                1 => ChannelEvent::Discontinuity,
                2 => ChannelEvent::End,
                3 => ChannelEvent::Segment {
                    seq: r.u()?,
                    start_us: r.i()?,
                    duration_us: r.u()?,
                    discontinuity: match r.u8()? {
                        0 => false,
                        1 => true,
                        _ => return Err(WireError::Malformed("bool")),
                    },
                },
                _ => return Err(WireError::Malformed("event kind")),
            };
            Frame::Event { channel, event }
        }
        kind::STATS => Frame::Stats(PeerStats {
            rx_bytes: r.u()?,
            rx_aus: r.u()?,
            rx_gaps: r.u()?,
        }),
        other => return Ok(Frame::Unknown(other)),
    };
    r.end()?;
    Ok(f)
}

/// 連続したバイト列から枠を 1 つ読む（試験・道具向け）。`Ok(None)` はバイトが足りない。
pub fn decode_frame(buf: &Bytes, max_frame: usize) -> Result<Option<(Frame, usize)>, WireError> {
    let Some((t, h, len)) = parse_frame_header(buf, max_frame)? else {
        return Ok(None);
    };
    if buf.len() < h + len {
        return Ok(None);
    }
    let body = buf.slice(h..h + len);
    Ok(Some((decode_body(t, &body)?, h + len)))
}

/// 2 つの AU が全ての欄と payload のバイト列で等しいか（`AccessUnit` は `PartialEq` を持たない）。
pub fn same_au(a: &AccessUnit, b: &AccessUnit) -> bool {
    (
        a.track,
        a.codec,
        a.epoch,
        a.sequence,
        a.config_generation,
        a.pts,
    ) == (
        b.track,
        b.codec,
        b.epoch,
        b.sequence,
        b.config_generation,
        b.pts,
    ) && (
        a.dts,
        a.duration,
        a.timebase,
        a.keyframe,
        a.discontinuity,
        a.capture_time_ns,
    ) == (
        b.dts,
        b.duration,
        b.timebase,
        b.keyframe,
        b.discontinuity,
        b.capture_time_ns,
    ) && a.payload == b.payload
}

// ---------------------------------------------------------------------------------------------
// トラックごとの差分の状態

#[derive(Debug, Clone)]
struct Last {
    codec: Codec,
    epoch: u64,
    generation: u32,
    timebase: Timebase,
    sequence: u64,
    dts: i64,
    duration: u64,
    capture: i64,
}

/// トラックの直前の AU（送り手と受け手が同じ状態を持つ）。
#[derive(Debug, Clone, Default)]
pub struct TrackCodec {
    last: Option<Last>,
}

impl TrackCodec {
    /// AU の見出しを書く。`gap` は直前に AU を捨てた（不連続の印をつける）。
    pub fn encode(&mut self, au: &AccessUnit, gap: bool, out: &mut impl BufMut) {
        let mut flags = 0u8;
        if au.keyframe {
            flags |= flag::KEY;
        }
        if au.discontinuity || gap {
            flags |= flag::DISC;
        }
        let full = match &self.last {
            None => true,
            Some(l) => {
                l.codec != au.codec
                    || l.epoch != au.epoch
                    || l.generation != au.config_generation
                    || l.timebase != au.timebase
                    || au.sequence <= l.sequence
            }
        };
        if full {
            self.last = Some(Last {
                codec: au.codec,
                epoch: au.epoch,
                generation: au.config_generation,
                timebase: au.timebase,
                sequence: au.sequence,
                dts: au.dts,
                duration: 0,
                capture: 0,
            });
            flags |= flag::FULL;
        }
        let l = self.last.as_mut().expect("set above");
        let seq_delta = au.sequence.wrapping_sub(l.sequence);
        if !full && seq_delta != 1 {
            flags |= flag::SEQ;
        }
        let cts = au.pts.wrapping_sub(au.dts);
        if cts != 0 {
            flags |= flag::CTS;
        }
        if au.duration != l.duration {
            flags |= flag::DUR;
        }
        if au.capture_time_ns.is_some() {
            flags |= flag::CAP;
        }
        out.put_u8(flags);
        if full {
            out.put_u8(codec_to_u8(au.codec));
            put_u(out, au.epoch);
            put_u(out, au.config_generation as u64);
            put_u(out, au.timebase.num as u64);
            put_u(out, au.timebase.den as u64);
            put_u(out, au.sequence);
            put_i(out, au.dts);
        } else {
            if flags & flag::SEQ != 0 {
                put_u(out, seq_delta);
            }
            put_i(out, au.dts.wrapping_sub(l.dts));
        }
        if flags & flag::CTS != 0 {
            put_i(out, cts);
        }
        if flags & flag::DUR != 0 {
            put_u(out, au.duration);
        }
        if let Some(c) = au.capture_time_ns {
            put_i(out, c.wrapping_sub(l.capture));
            l.capture = c;
        }
        l.sequence = au.sequence;
        l.dts = au.dts;
        l.duration = au.duration;
    }

    /// 見出しと payload から AU を組み立てる。
    pub fn decode(
        &mut self,
        track: TrackId,
        h: &AuHeader,
        payload: Bytes,
    ) -> Result<AccessUnit, WireError> {
        if let Some(f) = &h.full {
            self.last = Some(Last {
                codec: f.codec,
                epoch: f.epoch,
                generation: f.config_generation,
                timebase: f.timebase,
                sequence: f.sequence,
                dts: f.dts,
                duration: 0,
                capture: 0,
            });
        } else {
            let l = self
                .last
                .as_mut()
                .ok_or(WireError::Malformed("delta header without state"))?;
            l.sequence = l.sequence.wrapping_add(h.seq_delta);
            l.dts = l.dts.wrapping_add(h.dts_delta);
        }
        let l = self.last.as_mut().expect("set above");
        if let Some(d) = h.duration {
            l.duration = d;
        }
        let capture_time_ns = h.capture_delta.map(|d| {
            l.capture = l.capture.wrapping_add(d);
            l.capture
        });
        Ok(AccessUnit {
            track,
            codec: l.codec,
            epoch: l.epoch,
            sequence: l.sequence,
            config_generation: l.generation,
            pts: l.dts.wrapping_add(h.cts),
            dts: l.dts,
            duration: l.duration,
            timebase: l.timebase,
            keyframe: h.flags & flag::KEY != 0,
            discontinuity: h.flags & flag::DISC != 0,
            capture_time_ns,
            payload,
        })
    }

    /// 次の AU を完全な見出しで送る（受け手の状態が分からなくなったとき）。
    pub fn reset(&mut self) {
        self.last = None;
    }
}

// ---------------------------------------------------------------------------------------------
// 書き

fn put_str(b: &mut impl BufMut, s: &str, max: usize) {
    let s = if s.len() > max {
        let mut n = max;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        &s[..n]
    } else {
        s
    };
    put_u(b, s.len() as u64);
    b.put_slice(s.as_bytes());
}

fn put_sel(b: &mut impl BufMut, s: &TrackSel) {
    match s {
        TrackSel::All => b.put_u8(0),
        TrackSel::Only(v) => {
            b.put_u8(1);
            let v = &v[..v.len().min(MAX_SEL_TRACKS)];
            put_u(b, v.len() as u64);
            for t in v {
                put_u(b, t.0 as u64);
            }
        }
    }
}

fn put_config(b: &mut impl BufMut, c: &CodecConfig) {
    b.put_u8(codec_to_u8(c.codec));
    put_u(b, c.generation as u64);
    put_u(b, c.record.len() as u64);
    b.put_slice(&c.record);
    b.put_u8(c.video.is_some() as u8 | (c.audio.is_some() as u8) << 1);
    if let Some(v) = c.video {
        put_u(b, v.width as u64);
        put_u(b, v.height as u64);
        b.put_u8(v.profile);
        b.put_u8(v.compat);
        b.put_u8(v.level);
    }
    if let Some(a) = c.audio {
        put_u(b, a.sample_rate as u64);
        b.put_u8(a.channels);
        b.put_u8(a.object_type);
    }
}

/// 制御の枠（Media・Batch 以外）の本体を書く。Media・Batch は [`Writer`] を使う。
fn put_control_body(b: &mut Vec<u8>, f: &Frame) -> u8 {
    match f {
        Frame::Hello(h) => {
            b.put_slice(&MAGIC);
            put_u(b, h.version as u64);
            put_u(b, h.node_id);
            put_u(b, h.capabilities);
            put_u(b, h.max_frame);
            kind::HELLO
        }
        Frame::GoAway { code, reason } => {
            put_u(b, code.0 as u64);
            put_str(b, reason, MAX_REASON);
            kind::GOAWAY
        }
        Frame::Ping(n) => {
            put_u(b, *n);
            kind::PING
        }
        Frame::Pong(n) => {
            put_u(b, *n);
            kind::PONG
        }
        Frame::Subscribe {
            channel,
            priority,
            stream_id,
            tracks,
        } => {
            put_u(b, *channel as u64);
            b.put_u8(*priority);
            put_str(b, stream_id, MAX_STREAM_ID);
            put_sel(b, tracks);
            kind::SUBSCRIBE
        }
        Frame::SubscribeOk { channel } => {
            put_u(b, *channel as u64);
            kind::SUBSCRIBE_OK
        }
        Frame::SubscribeErr {
            channel,
            code,
            reason,
        } => {
            put_u(b, *channel as u64);
            put_u(b, code.0 as u64);
            put_str(b, reason, MAX_REASON);
            kind::SUBSCRIBE_ERR
        }
        Frame::Unsubscribe { channel } => {
            put_u(b, *channel as u64);
            kind::UNSUBSCRIBE
        }
        Frame::Config {
            channel,
            track,
            config,
        } => {
            put_u(b, *channel as u64);
            put_u(b, track.0 as u64);
            put_config(b, config);
            kind::CONFIG
        }
        Frame::Control { channel, op } => {
            put_u(b, *channel as u64);
            match op {
                ControlOp::KeyframeRequest(t) => {
                    b.put_u8(1);
                    put_u(b, t.0 as u64);
                }
                ControlOp::SetTracks(s) => {
                    b.put_u8(2);
                    put_sel(b, s);
                }
            }
            kind::CONTROL
        }
        Frame::Event { channel, event } => {
            put_u(b, *channel as u64);
            match event {
                ChannelEvent::Discontinuity => b.put_u8(1),
                ChannelEvent::End => b.put_u8(2),
                ChannelEvent::Segment {
                    seq,
                    start_us,
                    duration_us,
                    discontinuity,
                } => {
                    b.put_u8(3);
                    put_u(b, *seq);
                    put_i(b, *start_us);
                    put_u(b, *duration_us);
                    b.put_u8(*discontinuity as u8);
                }
            }
            kind::EVENT
        }
        Frame::Stats(s) => {
            put_u(b, s.rx_bytes);
            put_u(b, s.rx_aus);
            put_u(b, s.rx_gaps);
            kind::STATS
        }
        Frame::Media(m) => {
            put_u(b, m.channel as u64);
            put_u(b, m.track.0 as u64);
            put_raw_header(b, &m.hdr);
            b.put_slice(&m.payload);
            kind::MEDIA
        }
        Frame::Batch(items) => {
            put_u(b, items.len() as u64);
            for m in items {
                put_u(b, m.channel as u64);
                put_u(b, m.track.0 as u64);
                put_raw_header(b, &m.hdr);
                put_u(b, m.payload.len() as u64);
                b.put_slice(&m.payload);
            }
            kind::BATCH
        }
        Frame::Unknown(t) => *t,
    }
}

/// 復号した見出しをそのまま書き戻す（試験用。普段は [`TrackCodec::encode`]）。
fn put_raw_header(b: &mut impl BufMut, h: &AuHeader) {
    b.put_u8(h.flags);
    if let Some(f) = &h.full {
        b.put_u8(codec_to_u8(f.codec));
        put_u(b, f.epoch);
        put_u(b, f.config_generation as u64);
        put_u(b, f.timebase.num as u64);
        put_u(b, f.timebase.den as u64);
        put_u(b, f.sequence);
        put_i(b, f.dts);
    } else {
        if h.flags & flag::SEQ != 0 {
            put_u(b, h.seq_delta);
        }
        put_i(b, h.dts_delta);
    }
    if h.flags & flag::CTS != 0 {
        put_i(b, h.cts);
    }
    if let Some(d) = h.duration {
        put_u(b, d);
    }
    if let Some(c) = h.capture_delta {
        put_i(b, c);
    }
}

/// 枠を 1 つの連続したバイト列に書く（試験・道具向け。Media・Batch も書ける）。
pub fn encode_frame(f: &Frame, out: &mut Vec<u8>) {
    let mut body = Vec::new();
    let t = put_control_body(&mut body, f);
    out.put_u8(t);
    put_u(out, body.len() as u64);
    out.extend_from_slice(&body);
}

/// まとめる AU の見出し（payload の長さを除く）を先に作っておく入れ物。
#[derive(Debug, Default)]
pub struct BatchBuilder {
    hdrs: Vec<u8>,
    items: Vec<(usize, usize, Bytes)>,
    body: usize,
}

impl BatchBuilder {
    pub fn push(
        &mut self,
        channel: u32,
        track: TrackId,
        st: &mut TrackCodec,
        au: &AccessUnit,
        gap: bool,
    ) {
        let s = self.hdrs.len();
        put_u(&mut self.hdrs, channel as u64);
        put_u(&mut self.hdrs, track.0 as u64);
        st.encode(au, gap, &mut self.hdrs);
        let e = self.hdrs.len();
        self.body += e - s + varint::len_u(au.payload.len() as u64) + au.payload.len();
        self.items.push((s, e, au.payload.clone()));
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    /// 枠にしたときの body の大きさ。
    pub fn body_len(&self) -> usize {
        varint::len_u(self.items.len() as u64) + self.body
    }
    pub fn clear(&mut self) {
        self.hdrs.clear();
        self.items.clear();
        self.body = 0;
    }
}

/// 送る枠を断片の列にする。見出しは小さなバッファにまとめて書き、大きな payload は別の断片
/// （`Bytes` の参照）にする。まとめ書き（writev）にそのまま渡せる。
#[derive(Debug)]
pub struct Writer {
    buf: BytesMut,
    chunks: Vec<Bytes>,
    scratch: Vec<u8>,
    /// これより小さい payload は見出しのバッファへ写す（断片を増やさない）。
    pub inline_max: usize,
    bytes: usize,
    frames: usize,
}

impl Default for Writer {
    fn default() -> Self {
        Writer::new()
    }
}

impl Writer {
    pub fn new() -> Self {
        Writer {
            buf: BytesMut::with_capacity(16 << 10),
            chunks: Vec::new(),
            scratch: Vec::new(),
            inline_max: 256,
            bytes: 0,
            frames: 0,
        }
    }

    /// 書いたバイト数（まだ取り出していない分）。
    pub fn pending_bytes(&self) -> usize {
        self.bytes
    }

    pub fn pending_frames(&self) -> usize {
        self.frames
    }

    /// Media・Batch 以外の枠を書く。
    pub fn control(&mut self, f: &Frame) {
        self.scratch.clear();
        let mut s = std::mem::take(&mut self.scratch);
        let t = put_control_body(&mut s, f);
        self.buf.put_u8(t);
        put_u(&mut self.buf, s.len() as u64);
        self.buf.extend_from_slice(&s);
        self.bytes += 1 + varint::len_u(s.len() as u64) + s.len();
        self.frames += 1;
        self.scratch = s;
    }

    fn payload(&mut self, p: &Bytes) {
        if p.len() <= self.inline_max {
            self.buf.extend_from_slice(p);
        } else {
            if !self.buf.is_empty() {
                self.chunks.push(self.buf.split().freeze());
            }
            self.chunks.push(p.clone());
        }
    }

    /// AU を 1 つの Media の枠として書く。
    pub fn media(
        &mut self,
        channel: u32,
        track: TrackId,
        st: &mut TrackCodec,
        au: &AccessUnit,
        gap: bool,
    ) {
        let mut h = [0u8; 96];
        let mut hb = &mut h[..];
        put_u(&mut hb, channel as u64);
        put_u(&mut hb, track.0 as u64);
        st.encode(au, gap, &mut hb);
        let n = 96 - hb.len();
        let body = n + au.payload.len();
        self.buf.put_u8(kind::MEDIA);
        put_u(&mut self.buf, body as u64);
        self.buf.extend_from_slice(&h[..n]);
        self.bytes += 1 + varint::len_u(body as u64) + body;
        self.frames += 1;
        self.payload(&au.payload);
    }

    /// まとめた AU を書く（1 つだけなら Media の枠にする）。`b` は空になる。
    pub fn batch(&mut self, b: &mut BatchBuilder) {
        if b.items.is_empty() {
            return;
        }
        if b.items.len() == 1 {
            let (s, e, p) = b.items.pop().expect("one");
            let body = e - s + p.len();
            self.buf.put_u8(kind::MEDIA);
            put_u(&mut self.buf, body as u64);
            self.buf.extend_from_slice(&b.hdrs[s..e]);
            self.bytes += 1 + varint::len_u(body as u64) + body;
            self.frames += 1;
            self.payload(&p);
            b.clear();
            return;
        }
        let body = b.body_len();
        self.buf.put_u8(kind::BATCH);
        put_u(&mut self.buf, body as u64);
        put_u(&mut self.buf, b.items.len() as u64);
        self.bytes += 1 + varint::len_u(body as u64) + body;
        self.frames += 1;
        let items = std::mem::take(&mut b.items);
        for (s, e, p) in &items {
            self.buf.extend_from_slice(&b.hdrs[*s..*e]);
            put_u(&mut self.buf, p.len() as u64);
            self.payload(p);
        }
        b.items = items;
        b.clear();
    }

    /// 書いたものを断片の列として取り出す。
    pub fn take(&mut self, out: &mut Vec<Bytes>) -> usize {
        if !self.buf.is_empty() {
            self.chunks.push(self.buf.split().freeze());
        }
        out.append(&mut self.chunks);
        // 見出しのバッファは使い回す（split で切り離した分は相手が持つ）
        if self.buf.capacity() < 4096 {
            self.buf.reserve(16 << 10);
        }
        let n = self.bytes;
        self.bytes = 0;
        self.frames = 0;
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn au(seq: u64, dts: i64, pts: i64, key: bool, len: usize) -> AccessUnit {
        AccessUnit {
            track: TrackId(0),
            codec: Codec::H264,
            epoch: 3,
            sequence: seq,
            config_generation: 1,
            pts,
            dts,
            duration: 0,
            timebase: Timebase::MPEG,
            keyframe: key,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from(vec![seq as u8; len]),
        }
    }

    fn same(a: &AccessUnit, b: &AccessUnit) {
        assert!(same_au(a, b), "{a:?} != {b:?}");
    }

    fn concat(chunks: &[Bytes]) -> Bytes {
        let mut v = Vec::new();
        for c in chunks {
            v.extend_from_slice(c);
        }
        Bytes::from(v)
    }

    fn all_frames() -> Vec<Frame> {
        vec![
            Frame::Hello(Hello {
                version: PROTOCOL_VERSION,
                node_id: 7,
                capabilities: caps::ALL,
                max_frame: DEFAULT_MAX_FRAME as u64,
            }),
            Frame::GoAway {
                code: ErrorCode::GOING_AWAY,
                reason: "保守".into(),
            },
            Frame::Ping(123456),
            Frame::Pong(u64::MAX),
            Frame::Subscribe {
                channel: 9,
                priority: 200,
                stream_id: "str_abc".into(),
                tracks: TrackSel::only([TrackId(2), TrackId(0)]),
            },
            Frame::Subscribe {
                channel: 10,
                priority: 0,
                stream_id: String::new(),
                tracks: TrackSel::All,
            },
            Frame::SubscribeOk { channel: 9 },
            Frame::SubscribeErr {
                channel: 9,
                code: ErrorCode::MOVED,
                reason: "moved".into(),
            },
            Frame::Unsubscribe { channel: u32::MAX },
            Frame::Config {
                channel: 1,
                track: TrackId(0),
                config: CodecConfig {
                    codec: Codec::H264,
                    generation: 2,
                    record: Bytes::from_static(&[1, 0x64, 0, 0x1f, 0xff]),
                    video: Some(VideoParams {
                        width: 1920,
                        height: 1080,
                        profile: 100,
                        compat: 0,
                        level: 40,
                    }),
                    audio: None,
                },
            },
            Frame::Config {
                channel: 1,
                track: TrackId(1),
                config: CodecConfig {
                    codec: Codec::Aac,
                    generation: 1,
                    record: Bytes::from_static(&[0x12, 0x10]),
                    video: None,
                    audio: Some(AudioParams {
                        sample_rate: 48000,
                        channels: 2,
                        object_type: 2,
                    }),
                },
            },
            Frame::Control {
                channel: 3,
                op: ControlOp::KeyframeRequest(TrackId(4)),
            },
            Frame::Control {
                channel: 3,
                op: ControlOp::SetTracks(TrackSel::only([TrackId(1)])),
            },
            Frame::Event {
                channel: 3,
                event: ChannelEvent::Discontinuity,
            },
            Frame::Event {
                channel: 3,
                event: ChannelEvent::End,
            },
            Frame::Event {
                channel: 3,
                event: ChannelEvent::Segment {
                    seq: 5,
                    start_us: -1,
                    duration_us: 2_000_000,
                    discontinuity: true,
                },
            },
            Frame::Stats(PeerStats {
                rx_bytes: 1 << 40,
                rx_aus: 5,
                rx_gaps: 0,
            }),
        ]
    }

    #[test]
    fn control_frames_round_trip() {
        for f in all_frames() {
            let mut v = Vec::new();
            encode_frame(&f, &mut v);
            let b = Bytes::from(v.clone());
            let (g, n) = decode_frame(&b, DEFAULT_MAX_FRAME).unwrap().unwrap();
            assert_eq!(n, v.len());
            assert_eq!(g, f);
            // Writer でも同じバイト列
            let mut w = Writer::new();
            w.control(&f);
            let mut out = Vec::new();
            assert_eq!(w.take(&mut out), v.len());
            assert_eq!(concat(&out), b);
            // 足りないときは None
            for cut in 0..v.len() {
                assert_eq!(
                    decode_frame(&b.slice(..cut), DEFAULT_MAX_FRAME).unwrap(),
                    None
                );
            }
        }
    }

    #[test]
    fn media_round_trip_with_deltas() {
        let mut aus = Vec::new();
        for i in 0..50u64 {
            let mut a = au(
                100 + i,
                3000 * i as i64 - 9000,
                3000 * i as i64 - 3000,
                i.is_multiple_of(10),
                (i as usize * 37) % 900 + 1,
            );
            if i == 7 {
                a.sequence += 5; // 飛び
            }
            if i >= 7 {
                a.sequence += 5;
            }
            if i == 20 {
                a.epoch = 4; // 完全な見出しに戻る
            }
            if i >= 20 {
                a.epoch = 4;
            }
            if i.is_multiple_of(3) {
                a.duration = 3000;
            }
            if i > 30 {
                a.capture_time_ns = Some(1_700_000_000_000_000_000 + i as i64 * 33_000_000);
            }
            aus.push(a);
        }
        let mut tx = TrackCodec::default();
        let mut w = Writer::new();
        for (i, a) in aus.iter().enumerate() {
            w.media(5, TrackId(0), &mut tx, a, i == 12);
        }
        let mut chunks = Vec::new();
        w.take(&mut chunks);
        let wire = concat(&chunks);
        let mut rx = TrackCodec::default();
        let mut off = 0;
        for (i, a) in aus.iter().enumerate() {
            let (f, n) = decode_frame(&wire.slice(off..), DEFAULT_MAX_FRAME)
                .unwrap()
                .unwrap();
            off += n;
            let Frame::Media(m) = f else { panic!() };
            assert_eq!(m.channel, 5);
            let got = rx.decode(m.track, &m.hdr, m.payload).unwrap();
            let mut want = a.clone();
            if i == 12 {
                want.discontinuity = true;
            }
            same(&got, &want);
        }
        assert_eq!(off, wire.len());
    }

    #[test]
    fn batch_round_trip_and_overhead() {
        let mut tx = [TrackCodec::default(), TrackCodec::default()];
        let mut bb = BatchBuilder::default();
        let mut aus = Vec::new();
        for i in 0..20u64 {
            // 2 トラックの AAC（トラックごとに sequence は連続、dts は 1024 ずつ）
            let mut a = au(
                i / 2,
                (i / 2) as i64 * 1024,
                (i / 2) as i64 * 1024,
                true,
                300 + i as usize,
            );
            a.codec = Codec::Aac;
            a.timebase = Timebase::new(1, 48000);
            a.track = TrackId((i % 2) as u32);
            aus.push(a);
        }
        // 1 巡目（完全な見出し）は捨てて、2 巡目の大きさを測る
        let mut w = Writer::new();
        for a in &aus[..2] {
            bb.push(1, a.track, &mut tx[a.track.0 as usize], a, false);
        }
        w.batch(&mut bb);
        let mut chunks = Vec::new();
        w.take(&mut chunks);
        let first = concat(&chunks);
        chunks.clear();
        for a in &aus[2..] {
            bb.push(1, a.track, &mut tx[a.track.0 as usize], a, false);
        }
        w.batch(&mut bb);
        let n = w.take(&mut chunks);
        let wire = concat(&chunks);
        assert_eq!(n, wire.len());
        let payload: usize = aus[2..].iter().map(|a| a.payload.len()).sum();
        let per_au = (wire.len() - payload) as f64 / 18.0;
        assert!(per_au < 7.5, "まとめた AU の見出し {per_au} バイト");
        let mut rx = [TrackCodec::default(), TrackCodec::default()];
        let mut got = Vec::new();
        for w in [first, wire] {
            let (f, _) = decode_frame(&w, DEFAULT_MAX_FRAME).unwrap().unwrap();
            match f {
                Frame::Batch(items) => got.extend(items),
                Frame::Media(m) => got.push(m),
                _ => panic!(),
            }
        }
        assert_eq!(got.len(), 20);
        for (m, a) in got.into_iter().zip(&aus) {
            let g = rx[m.track.0 as usize]
                .decode(m.track, &m.hdr, m.payload)
                .unwrap();
            same(&g, a);
        }
    }

    #[test]
    fn video_overhead_is_small() {
        let mut tx = TrackCodec::default();
        let mut w = Writer::new();
        let mut chunks = Vec::new();
        w.media(3, TrackId(0), &mut tx, &au(1, 0, 6000, true, 20000), false);
        w.take(&mut chunks);
        chunks.clear();
        // 30 fps の B フレームつき（pts≠dts）
        let n = w.take(&mut chunks);
        assert_eq!(n, 0);
        w.media(
            3,
            TrackId(0),
            &mut tx,
            &au(2, 3000, 12000, false, 16000),
            false,
        );
        let n = w.take(&mut chunks);
        assert!(n - 16000 <= 11, "映像の見出し {} バイト", n - 16000);
        // payload は別の断片（コピーしない）
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1].len(), 16000);
    }

    /// 壊れた・切れた入力で panic せず、上限を守る。
    #[test]
    fn fuzz_malformed_and_truncated() {
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut corpus: Vec<Vec<u8>> = Vec::new();
        for f in all_frames() {
            let mut v = Vec::new();
            encode_frame(&f, &mut v);
            corpus.push(v);
        }
        let mut tx = TrackCodec::default();
        let mut w = Writer::new();
        let mut bb = BatchBuilder::default();
        for i in 0..3 {
            bb.push(
                1,
                TrackId(0),
                &mut tx,
                &au(i, i as i64, i as i64, i == 0, 10),
                false,
            );
        }
        w.batch(&mut bb);
        let mut ch = Vec::new();
        w.take(&mut ch);
        corpus.push(concat(&ch).to_vec());
        for _ in 0..200_000 {
            let mut v = corpus[(rnd() % corpus.len() as u64) as usize].clone();
            match rnd() % 4 {
                0 => {
                    let n = (rnd() % v.len() as u64) as usize;
                    v.truncate(n);
                }
                1 => {
                    for _ in 0..1 + rnd() % 4 {
                        let i = (rnd() % v.len() as u64) as usize;
                        v[i] = rnd() as u8;
                    }
                }
                2 => {
                    let i = (rnd() % v.len() as u64) as usize;
                    v.insert(i, rnd() as u8);
                }
                _ => {
                    let n = (rnd() % 40) as usize;
                    v = (0..n).map(|_| rnd() as u8).collect();
                }
            }
            let b = Bytes::from(v);
            if let Ok(Some((Frame::Media(m), _))) = decode_frame(&b, 4096) {
                let mut st = TrackCodec::default();
                let _ = st.decode(m.track, &m.hdr, m.payload);
            }
        }
    }

    #[test]
    fn bounds() {
        // 上限を超える長さ
        let mut v = vec![kind::MEDIA];
        put_u(&mut v, 1 << 30);
        assert!(matches!(
            parse_frame_header(&v, DEFAULT_MAX_FRAME),
            Err(WireError::FrameTooLarge(_))
        ));
        // 長すぎる stream_id
        let mut body = Vec::new();
        put_u(&mut body, 1);
        body.push(0);
        put_u(&mut body, (MAX_STREAM_ID + 1) as u64);
        body.extend(std::iter::repeat_n(b'a', MAX_STREAM_ID + 1));
        body.push(0);
        assert!(decode_body(kind::SUBSCRIBE, &Bytes::from(body)).is_err());
        // トラックの選択が多すぎる
        let mut body = Vec::new();
        put_u(&mut body, 1);
        body.push(0);
        put_u(&mut body, 0);
        body.push(1);
        put_u(&mut body, 1000);
        assert!(decode_body(kind::SUBSCRIBE, &Bytes::from(body)).is_err());
        // まとめの数が 0
        assert!(decode_body(kind::BATCH, &Bytes::from_static(&[0])).is_err());
        // 予約の flag
        let mut body = vec![1, 0, 0x80];
        body.push(0);
        assert!(decode_body(kind::MEDIA, &Bytes::from(body)).is_err());
        // 状態なしの差分
        let Frame::Media(m) =
            decode_body(kind::MEDIA, &Bytes::from_static(&[1, 0, 0, 2, 9])).unwrap()
        else {
            panic!()
        };
        assert!(
            TrackCodec::default()
                .decode(m.track, &m.hdr, m.payload)
                .is_err()
        );
        // 違う版・違う魔法の数
        let mut v = Vec::new();
        encode_frame(
            &Frame::Hello(Hello {
                version: 9,
                node_id: 1,
                capabilities: 0,
                max_frame: 0,
            }),
            &mut v,
        );
        let (f, _) = decode_frame(&Bytes::from(v), DEFAULT_MAX_FRAME)
            .unwrap()
            .unwrap();
        assert!(matches!(f, Frame::Hello(Hello { version: 9, .. })));
        assert_eq!(
            decode_body(kind::HELLO, &Bytes::from_static(b"XXXX\x01")),
            Err(WireError::BadMagic)
        );
        // 知らない type は読み飛ばす
        assert_eq!(
            decode_body(0x7f, &Bytes::from_static(b"abc")).unwrap(),
            Frame::Unknown(0x7f)
        );
        // 末尾の余り
        assert!(decode_body(kind::PING, &Bytes::from_static(&[1, 2])).is_err());
    }
}
