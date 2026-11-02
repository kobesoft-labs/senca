//! chunk stream の復号・符号化（RTMP 仕様 5.3）。
//!
//! 復号:
//! - basic header（1〜3 バイト）、message header（fmt 0〜3）、extended timestamp を読む。
//! - fmt 1/2 の時刻は差分、fmt 3 で新しい message が始まるときは「直前の timestamp field の値」を差分として使う
//!   （fmt 0 のあとなら絶対値の field を差分として使う。FFmpeg・librtmp と同じ解釈）。
//! - extended timestamp を使った chunk stream の fmt 3 継続 chunk には 4 バイトの extended timestamp が
//!   付く決まりだが、付けない実装もあるので、継続 chunk では値が一致したときだけ読み取る。
//! - message は受信時に一度だけ `BytesMut` に組み立てて `freeze` する。1 chunk に収まる message は
//!   入力バッファの切り出しで渡す（コピーなし）。
//! - 上限: message 長（既定 16 MiB）、同時 chunk stream 数（既定 64）、組み立て途中の総バイト数（既定 32 MiB）。
//!   相手の chunk size は 1〜0x7FFFFFFF を受け付ける（実際に読む量は message 長の上限で抑えられる）。
//!
//! 符号化:
//! - 同じ chunk stream の直前の message と比べて fmt を選ぶ。fmt 3 で新しい message を始めるのは
//!   直前が fmt 1/2 で差分が等しいときだけ（fmt 0 のあとの fmt 3 は実装により解釈が分かれるため）。

use crate::Error;
use bytes::{Buf, Bytes, BytesMut};
use std::collections::HashMap;

/// 既定の chunk size（RTMP 仕様の初期値）。
pub const DEFAULT_CHUNK_SIZE: u32 = 128;
/// chunk size の上限（31 bit）。
pub const MAX_CHUNK_SIZE: u32 = 0x7FFF_FFFF;

/// 組み立て済みの message。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub csid: u32,
    /// ミリ秒（32 bit で折り返す）。
    pub timestamp: u32,
    pub type_id: u8,
    pub stream_id: u32,
    pub payload: Bytes,
}

/// 復号の上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkLimits {
    pub max_message_size: u32,
    pub max_chunk_streams: usize,
    pub max_pending_bytes: usize,
}

impl Default for ChunkLimits {
    fn default() -> Self {
        ChunkLimits {
            max_message_size: 16 * 1024 * 1024,
            max_chunk_streams: 64,
            max_pending_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Default)]
struct InStream {
    has_header: bool,
    timestamp: u32,
    /// 直前の timestamp field（fmt 0 なら絶対値、fmt 1/2 なら差分。extended を含む値）。
    ts_field: u32,
    extended: bool,
    length: u32,
    type_id: u8,
    stream_id: u32,
    /// 組み立て途中の payload。
    partial: Option<BytesMut>,
}

impl InStream {
    fn received(&self) -> usize {
        self.partial.as_ref().map_or(0, |p| p.len())
    }
}

/// chunk の途中（payload を読み進めている）。
#[derive(Debug, Clone, Copy)]
struct InChunk {
    csid: u32,
    remaining: usize,
}

/// chunk stream の復号器。
#[derive(Debug)]
pub struct ChunkDecoder {
    limits: ChunkLimits,
    chunk_size: u32,
    streams: HashMap<u32, InStream>,
    current: Option<InChunk>,
    pending_bytes: usize,
}

impl ChunkDecoder {
    pub fn new(limits: ChunkLimits) -> Self {
        ChunkDecoder {
            limits,
            chunk_size: DEFAULT_CHUNK_SIZE,
            streams: HashMap::new(),
            current: None,
            pending_bytes: 0,
        }
    }

    pub fn chunk_size(&self) -> u32 {
        self.chunk_size
    }

    /// 相手から Set Chunk Size を受けたとき。
    pub fn set_chunk_size(&mut self, size: u32) -> Result<(), Error> {
        if size == 0 || size > MAX_CHUNK_SIZE {
            return Err(Error::Protocol("invalid chunk size"));
        }
        self.chunk_size = size;
        Ok(())
    }

    /// Abort Message を受けたとき。組み立て途中の message を捨てる。
    pub fn abort(&mut self, csid: u32) {
        if let Some(s) = self.streams.get_mut(&csid)
            && let Some(p) = s.partial.take()
        {
            self.pending_bytes -= p.len();
            if let Some(c) = self.current
                && c.csid == csid
            {
                // chunk の途中で中断されることはない（Abort は別の chunk stream で届く）が、念のため。
                self.current = None;
            }
        }
    }

    /// `buf` の先頭から読めるだけ読み、組み立て終わった message を 1 つ返す。
    /// 足りなければ `Ok(None)`（`buf` には未処理分が残る）。
    pub fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Message>, Error> {
        loop {
            if let Some(cur) = self.current {
                if buf.is_empty() {
                    return Ok(None);
                }
                let n = cur.remaining.min(buf.len());
                let s = self
                    .streams
                    .get_mut(&cur.csid)
                    .ok_or(Error::Protocol("lost chunk stream"))?;
                let p = s
                    .partial
                    .as_mut()
                    .ok_or(Error::Protocol("lost partial message"))?;
                p.extend_from_slice(&buf[..n]);
                buf.advance(n);
                self.pending_bytes += n;
                if self.pending_bytes > self.limits.max_pending_bytes {
                    return Err(Error::LimitExceeded("pending message bytes"));
                }
                let remaining = cur.remaining - n;
                if remaining > 0 {
                    self.current = Some(InChunk { remaining, ..cur });
                    return Ok(None);
                }
                self.current = None;
                if p.len() == s.length as usize {
                    let payload = s.partial.take().unwrap_or_default().freeze();
                    self.pending_bytes -= payload.len();
                    return Ok(Some(Message {
                        csid: cur.csid,
                        timestamp: s.timestamp,
                        type_id: s.type_id,
                        stream_id: s.stream_id,
                        payload,
                    }));
                }
                continue;
            }
            if !self.read_header(buf)? {
                return Ok(None);
            }
            // 長さ 0 の message はヘッダーだけで完結する。
            if let Some(m) = self.take_empty_message() {
                return Ok(Some(m));
            }
            // 1 chunk で完結し、全部そろっていればコピーせず切り出す。
            if let Some(cur) = self.current {
                let s = &self.streams[&cur.csid];
                if s.received() == 0
                    && cur.remaining == s.length as usize
                    && buf.len() >= cur.remaining
                {
                    let payload = buf.split_to(cur.remaining).freeze();
                    let s = self.streams.get_mut(&cur.csid).expect("stream exists");
                    s.partial = None;
                    self.current = None;
                    return Ok(Some(Message {
                        csid: cur.csid,
                        timestamp: s.timestamp,
                        type_id: s.type_id,
                        stream_id: s.stream_id,
                        payload,
                    }));
                }
            }
        }
    }

    fn take_empty_message(&mut self) -> Option<Message> {
        let cur = self.current?;
        let s = self.streams.get_mut(&cur.csid)?;
        if s.length != 0 {
            return None;
        }
        s.partial = None;
        self.current = None;
        Some(Message {
            csid: cur.csid,
            timestamp: s.timestamp,
            type_id: s.type_id,
            stream_id: s.stream_id,
            payload: Bytes::new(),
        })
    }

    /// chunk のヘッダーを読む。そろっていなければ何も消費せず `Ok(false)`。
    fn read_header(&mut self, buf: &mut BytesMut) -> Result<bool, Error> {
        let b = &buf[..];
        if b.is_empty() {
            return Ok(false);
        }
        let fmt = b[0] >> 6;
        let (csid, mut pos) = match b[0] & 0x3F {
            0 => {
                if b.len() < 2 {
                    return Ok(false);
                }
                (64 + b[1] as u32, 2)
            }
            1 => {
                if b.len() < 3 {
                    return Ok(false);
                }
                (64 + b[1] as u32 + 256 * b[2] as u32, 3)
            }
            id => (id as u32, 1),
        };
        let mh = [11, 7, 3, 0][fmt as usize];
        if b.len() < pos + mh {
            return Ok(false);
        }
        let u24 = |s: &[u8]| u32::from_be_bytes([0, s[0], s[1], s[2]]);
        let h = &b[pos..pos + mh];
        let ts_raw = if fmt <= 2 { Some(u24(&h[0..3])) } else { None };
        let length = if fmt <= 1 { Some(u24(&h[3..6])) } else { None };
        let type_id = if fmt <= 1 { Some(h[6]) } else { None };
        let stream_id = if fmt == 0 {
            Some(u32::from_le_bytes([h[7], h[8], h[9], h[10]]))
        } else {
            None
        };
        pos += mh;

        if !self.streams.contains_key(&csid) {
            if fmt != 0 {
                return Err(Error::Protocol(
                    "first chunk of a chunk stream must be fmt 0",
                ));
            }
            if self.streams.len() >= self.limits.max_chunk_streams {
                return Err(Error::LimitExceeded("chunk streams"));
            }
        }
        let continuation = self.streams.get(&csid).is_some_and(|s| s.partial.is_some());
        if continuation && fmt != 3 {
            // 組み立て途中に新しいヘッダーが来た。途中の message は捨てられたものとみなすのが一般的だが、
            // 厳格に拒否する。
            return Err(Error::Protocol(
                "new message header before previous message completed",
            ));
        }
        let prev_extended = self.streams.get(&csid).is_some_and(|s| s.extended);
        let prev_ts_field = self.streams.get(&csid).map_or(0, |s| s.ts_field);

        // extended timestamp
        let mut ext: Option<u32> = None;
        match ts_raw {
            Some(0xFF_FFFF) => {
                if b.len() < pos + 4 {
                    return Ok(false);
                }
                ext = Some(u32::from_be_bytes([
                    b[pos],
                    b[pos + 1],
                    b[pos + 2],
                    b[pos + 3],
                ]));
                pos += 4;
            }
            Some(_) => {}
            None if prev_extended => {
                if b.len() < pos + 4 {
                    return Ok(false);
                }
                let v = u32::from_be_bytes([b[pos], b[pos + 1], b[pos + 2], b[pos + 3]]);
                if !continuation || v == prev_ts_field {
                    ext = Some(v);
                    pos += 4;
                }
            }
            None => {}
        }

        let s = self.streams.entry(csid).or_default();
        if continuation {
            let remaining = (s.length as usize - s.received()).min(self.chunk_size as usize);
            buf.advance(pos);
            self.current = Some(InChunk { csid, remaining });
            return Ok(true);
        }

        // 新しい message
        let field = match ts_raw {
            Some(0xFF_FFFF) => ext.unwrap_or(0),
            Some(v) => v,
            None => prev_ts_field,
        };
        if let Some(l) = length {
            if l > self.limits.max_message_size {
                return Err(Error::LimitExceeded("message size"));
            }
            s.length = l;
        }
        if let Some(t) = type_id {
            s.type_id = t;
        }
        if let Some(id) = stream_id {
            s.stream_id = id;
        }
        if fmt == 0 {
            s.timestamp = field;
        } else {
            s.timestamp = s.timestamp.wrapping_add(field);
        }
        s.ts_field = field;
        if ts_raw.is_some() {
            s.extended = ts_raw == Some(0xFF_FFFF);
        }
        s.has_header = true;
        let len = s.length as usize;
        // 少しずつ育てる（宣言長だけで大きく確保しない）。
        s.partial = Some(BytesMut::with_capacity(len.min(64 * 1024)));
        buf.advance(pos);
        self.current = Some(InChunk {
            csid,
            remaining: len.min(self.chunk_size as usize),
        });
        Ok(true)
    }
}

#[derive(Debug, Clone, Copy)]
struct OutStream {
    timestamp: u32,
    ts_field: u32,
    length: u32,
    type_id: u8,
    stream_id: u32,
    last_fmt: u8,
}

/// chunk stream の符号化器。
#[derive(Debug)]
pub struct ChunkEncoder {
    chunk_size: u32,
    streams: HashMap<u32, OutStream>,
}

impl Default for ChunkEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkEncoder {
    pub fn new() -> Self {
        ChunkEncoder {
            chunk_size: DEFAULT_CHUNK_SIZE,
            streams: HashMap::new(),
        }
    }

    pub fn chunk_size(&self) -> u32 {
        self.chunk_size
    }

    /// 送る chunk size を変える。相手に Set Chunk Size を送った直後に呼ぶこと。
    pub fn set_chunk_size(&mut self, size: u32) {
        self.chunk_size = size.clamp(1, MAX_CHUNK_SIZE);
    }

    /// message を chunk にして `out` に書く。payload は `parts` の連結。
    pub fn write(
        &mut self,
        out: &mut BytesMut,
        csid: u32,
        timestamp: u32,
        type_id: u8,
        stream_id: u32,
        parts: &[&[u8]],
    ) -> Result<(), Error> {
        if !(2..=65599).contains(&csid) {
            return Err(Error::Protocol("invalid chunk stream id"));
        }
        let len: usize = parts.iter().map(|p| p.len()).sum();
        if len > 0xFF_FFFF {
            return Err(Error::LimitExceeded("message size"));
        }
        let len = len as u32;
        let prev = self.streams.get(&csid).copied();
        let (fmt, field) = match prev {
            Some(p) if p.stream_id == stream_id => {
                let delta = timestamp.wrapping_sub(p.timestamp);
                if delta >= 0x8000_0000 {
                    (0, timestamp)
                } else if p.length != len || p.type_id != type_id {
                    (1, delta)
                } else if delta == p.ts_field && p.last_fmt != 0 {
                    (3, delta)
                } else {
                    (2, delta)
                }
            }
            _ => (0, timestamp),
        };
        let extended = field >= 0xFF_FFFF;
        out.reserve(len as usize + 18 + (len / self.chunk_size) as usize * 8);
        write_basic_header(out, fmt, csid);
        let ts24 = if extended { 0xFF_FFFF } else { field };
        if fmt <= 2 {
            out.extend_from_slice(&ts24.to_be_bytes()[1..]);
        }
        if fmt <= 1 {
            out.extend_from_slice(&len.to_be_bytes()[1..]);
            out.extend_from_slice(&[type_id]);
        }
        if fmt == 0 {
            out.extend_from_slice(&stream_id.to_le_bytes());
        }
        if extended {
            out.extend_from_slice(&field.to_be_bytes());
        }
        // payload を chunk size ごとに区切る
        let cs = self.chunk_size as usize;
        let mut in_chunk = 0usize;
        for part in parts {
            let mut p: &[u8] = part;
            while !p.is_empty() {
                if in_chunk == cs {
                    write_basic_header(out, 3, csid);
                    if extended {
                        out.extend_from_slice(&field.to_be_bytes());
                    }
                    in_chunk = 0;
                }
                let n = (cs - in_chunk).min(p.len());
                out.extend_from_slice(&p[..n]);
                p = &p[n..];
                in_chunk += n;
            }
        }
        self.streams.insert(
            csid,
            OutStream {
                timestamp,
                ts_field: field,
                length: len,
                type_id,
                stream_id,
                last_fmt: fmt,
            },
        );
        Ok(())
    }
}

fn write_basic_header(out: &mut BytesMut, fmt: u8, csid: u32) {
    if csid < 64 {
        out.extend_from_slice(&[(fmt << 6) | csid as u8]);
    } else if csid < 320 {
        out.extend_from_slice(&[fmt << 6, (csid - 64) as u8]);
    } else {
        let v = csid - 64;
        out.extend_from_slice(&[(fmt << 6) | 1, v as u8, (v >> 8) as u8]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(dec: &mut ChunkDecoder, data: &[u8], step: usize) -> Result<Vec<Message>, Error> {
        let mut buf = BytesMut::new();
        let mut out = Vec::new();
        for c in data.chunks(step) {
            buf.extend_from_slice(c);
            while let Some(m) = dec.decode(&mut buf)? {
                out.push(m);
            }
        }
        Ok(out)
    }

    #[test]
    fn roundtrip_various() {
        let mut enc = ChunkEncoder::new();
        let mut out = BytesMut::new();
        let big = vec![7u8; 1000];
        let msgs: Vec<(u32, u32, u8, u32, Vec<u8>)> = vec![
            (3, 0, 20, 0, b"hello".to_vec()),
            (4, 1000, 8, 1, big.clone()),
            (4, 1020, 8, 1, big.clone()),
            (4, 1040, 8, 1, big.clone()),        // fmt 3 新 message
            (4, 1050, 8, 1, vec![1; 10]),        // fmt 1
            (4, 0x0100_0000, 8, 1, big.clone()), // extended（fmt 2 の差分が大きい）
            (4, 0x0200_0000, 8, 1, big.clone()), // extended、fmt 3 新 message
            (400, 0xFFFF_FFF0, 9, 1, vec![]),    // 3 バイト basic header、長さ 0
            (70, 5, 9, 1, vec![2; 300]),         // 2 バイト basic header
            (4, 10, 8, 1, vec![3; 3]),           // 時刻が戻る → fmt 0
        ];
        for (i, (csid, ts, t, sid, p)) in msgs.iter().enumerate() {
            if i == 3 {
                enc.set_chunk_size(300);
            }
            let (a, b) = p.split_at(p.len() / 2);
            enc.write(&mut out, *csid, *ts, *t, *sid, &[a, b]).unwrap();
        }
        for step in [1, 7, 128, 100_000] {
            let mut dec = ChunkDecoder::new(ChunkLimits::default());
            let mut buf = BytesMut::new();
            let mut got = Vec::new();
            let mut set = false;
            for c in out.chunks(step) {
                buf.extend_from_slice(c);
                while let Some(m) = dec.decode(&mut buf).unwrap() {
                    got.push(m);
                    if got.len() == 3 && !set {
                        dec.set_chunk_size(300).unwrap();
                        set = true;
                    }
                }
            }
            assert_eq!(got.len(), msgs.len(), "step {step}");
            for (m, (csid, ts, t, sid, p)) in got.iter().zip(&msgs) {
                assert_eq!(
                    (m.csid, m.timestamp, m.type_id, m.stream_id),
                    (*csid, *ts, *t, *sid)
                );
                assert_eq!(&m.payload[..], &p[..]);
            }
        }
    }

    #[test]
    fn fmt3_continuation_without_extended_timestamp() {
        // extended timestamp を使う message で、継続 chunk に extended を付けない実装。
        let mut data = vec![0x04];
        data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0, 0, 200, 8, 1, 0, 0, 0]);
        data.extend_from_slice(&0x0100_0000u32.to_be_bytes());
        data.extend_from_slice(&[1; 128]);
        data.push(0xC4);
        data.extend_from_slice(&[2; 72]);
        let mut dec = ChunkDecoder::new(ChunkLimits::default());
        let m = decode_all(&mut dec, &data, 1000).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].timestamp, 0x0100_0000);
        assert_eq!(&m[0].payload[128..], &[2; 72]);
    }

    #[test]
    fn single_chunk_is_zero_copy() {
        let mut enc = ChunkEncoder::new();
        let mut out = BytesMut::new();
        enc.write(&mut out, 3, 0, 20, 0, &[&[9; 100]]).unwrap();
        let mut dec = ChunkDecoder::new(ChunkLimits::default());
        let base = out.as_ptr() as usize;
        let m = dec.decode(&mut out).unwrap().unwrap();
        assert_eq!(m.payload.as_ptr() as usize, base + 12);
    }

    #[test]
    fn limits() {
        let lim = ChunkLimits {
            max_message_size: 100,
            max_chunk_streams: 2,
            max_pending_bytes: 150,
        };
        // 長すぎる message
        let mut dec = ChunkDecoder::new(lim);
        let d = [0x03, 0, 0, 0, 0, 0, 101, 8, 0, 0, 0, 0];
        assert!(matches!(
            decode_all(&mut dec, &d, 100),
            Err(Error::LimitExceeded(_))
        ));
        // chunk stream が多すぎる
        let mut dec = ChunkDecoder::new(lim);
        let mut d = Vec::new();
        for cs in 3..6u8 {
            d.extend_from_slice(&[cs, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 0]);
        }
        assert!(matches!(
            decode_all(&mut dec, &d, 100),
            Err(Error::LimitExceeded(_))
        ));
        // 組み立て途中の総量
        let mut dec = ChunkDecoder::new(lim);
        let mut d = Vec::new();
        for cs in 3..5u8 {
            d.extend_from_slice(&[cs, 0, 0, 0, 0, 0, 100, 8, 0, 0, 0, 0]);
            d.extend_from_slice(&[0; 80]);
        }
        dec.set_chunk_size(80).unwrap();
        assert!(matches!(
            decode_all(&mut dec, &d, 1000),
            Err(Error::LimitExceeded(_))
        ));
        // 最初が fmt 0 でない
        let mut dec = ChunkDecoder::new(lim);
        assert!(decode_all(&mut dec, &[0x43, 0, 0, 0, 0, 0, 1, 8, 1], 10).is_err());
        assert!(dec.set_chunk_size(0).is_err());
        assert!(dec.set_chunk_size(0x8000_0000).is_err());
    }

    #[test]
    fn garbage_never_panics() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for round in 0..3000 {
            let mut v = Vec::new();
            for _ in 0..(round % 200) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                v.push(x as u8);
            }
            let mut dec = ChunkDecoder::new(ChunkLimits::default());
            if round % 3 == 0 {
                let _ = dec.set_chunk_size(1 + (x as u32 % 300));
            }
            let _ = decode_all(&mut dec, &v, 1 + round % 13);
        }
    }
}
