//! 2 進の符号化（版付き・長さ前置・上限つき）。ノード間の転送と記憶域の両方で使う。
//!
//! メッセージの枠: `len:u32be | version:u8 | kind:u8 | from:u64 | to:u64 | term:u64 | body`。
//! `len` は自分より後ろの長さ。整数はすべて big endian の固定長。可変長の部分は長さを前置し、
//! 件数・長さに上限を置く。壊れた入力は `Err`（パニックしない・上限を超えて確保しない）。
//! payload（entry の data・snapshot の片）は受け取った [`Bytes`] の切り出しで返す（コピーしない）。

use crate::message::{Message, MsgBody};
use crate::types::{ConfState, Entry, EntryKind, HardState, SnapshotMeta};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::collections::BTreeSet;

/// 符号の版。
pub const VERSION: u8 = 1;
/// entry の data の上限。
pub const MAX_ENTRY_DATA: usize = 64 << 20;
/// snapshot の 1 片の上限。
pub const MAX_CHUNK: usize = 16 << 20;
/// 構成の 1 集合の ID 数の上限。
pub const MAX_IDS: usize = 1024;
/// 1 メッセージの entry 数の上限。
pub const MAX_ENTRIES_PER_MSG: usize = 1 << 16;
/// 枠の大きさの既定の上限。
pub const DEFAULT_MAX_FRAME: usize = 128 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// 途中で切れている。
    Truncated,
    /// 版が違う。
    Version(u8),
    /// 上限を超えている。
    TooLarge(&'static str),
    /// 値が不正。
    Invalid(&'static str),
    /// 余りがある。
    Trailing,
    /// CRC が合わない。
    Checksum,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::Truncated => write!(f, "truncated"),
            CodecError::Version(v) => write!(f, "unsupported version {v}"),
            CodecError::TooLarge(w) => write!(f, "too large: {w}"),
            CodecError::Invalid(w) => write!(f, "invalid: {w}"),
            CodecError::Trailing => write!(f, "trailing bytes"),
            CodecError::Checksum => write!(f, "checksum mismatch"),
        }
    }
}

impl std::error::Error for CodecError {}

// ---- CRC-32（IEEE、表引き） ----

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = crc_table();

/// 続けて計算できる CRC-32。初期値は 0。
pub fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0, data)
}

// ---- 読み手 ----

pub(crate) struct Reader {
    buf: Bytes,
}

impl Reader {
    pub fn new(buf: Bytes) -> Reader {
        Reader { buf }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len()
    }

    pub fn u8(&mut self) -> Result<u8, CodecError> {
        if self.buf.remaining() < 1 {
            return Err(CodecError::Truncated);
        }
        Ok(self.buf.get_u8())
    }

    pub fn u32(&mut self) -> Result<u32, CodecError> {
        if self.buf.remaining() < 4 {
            return Err(CodecError::Truncated);
        }
        Ok(self.buf.get_u32())
    }

    pub fn u64(&mut self) -> Result<u64, CodecError> {
        if self.buf.remaining() < 8 {
            return Err(CodecError::Truncated);
        }
        Ok(self.buf.get_u64())
    }

    pub fn bool(&mut self) -> Result<bool, CodecError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CodecError::Invalid("bool")),
        }
    }

    pub fn bytes(&mut self, len: usize) -> Result<Bytes, CodecError> {
        if self.buf.remaining() < len {
            return Err(CodecError::Truncated);
        }
        Ok(self.buf.split_to(len))
    }

    pub fn finish(&self) -> Result<(), CodecError> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(CodecError::Trailing)
        }
    }
}

// ---- 部品 ----

pub(crate) fn put_entry(w: &mut Vec<u8>, e: &Entry) {
    w.put_u64(e.term);
    w.put_u64(e.index);
    w.put_u8(e.kind.to_u8());
    w.put_u32(e.data.len() as u32);
    w.put_slice(&e.data);
}

pub(crate) fn get_entry(r: &mut Reader) -> Result<Entry, CodecError> {
    let term = r.u64()?;
    let index = r.u64()?;
    let kind = EntryKind::from_u8(r.u8()?).ok_or(CodecError::Invalid("entry kind"))?;
    let len = r.u32()? as usize;
    if len > MAX_ENTRY_DATA {
        return Err(CodecError::TooLarge("entry data"));
    }
    let data = r.bytes(len)?;
    Ok(Entry {
        term,
        index,
        kind,
        data,
    })
}

fn put_ids(w: &mut Vec<u8>, ids: &BTreeSet<u64>) {
    w.put_u32(ids.len() as u32);
    for id in ids {
        w.put_u64(*id);
    }
}

fn get_ids(r: &mut Reader) -> Result<BTreeSet<u64>, CodecError> {
    let n = r.u32()? as usize;
    if n > MAX_IDS {
        return Err(CodecError::TooLarge("ids"));
    }
    if r.remaining() < n * 8 {
        return Err(CodecError::Truncated);
    }
    let mut s = BTreeSet::new();
    for _ in 0..n {
        let id = r.u64()?;
        if id == 0 || !s.insert(id) {
            return Err(CodecError::Invalid("node id"));
        }
    }
    Ok(s)
}

pub(crate) fn put_conf_state(w: &mut Vec<u8>, c: &ConfState) {
    put_ids(w, &c.voters);
    put_ids(w, &c.voters_outgoing);
    put_ids(w, &c.learners);
}

pub(crate) fn get_conf_state(r: &mut Reader) -> Result<ConfState, CodecError> {
    let voters = get_ids(r)?;
    let voters_outgoing = get_ids(r)?;
    let learners = get_ids(r)?;
    Ok(ConfState {
        voters,
        voters_outgoing,
        learners,
    })
}

/// 構成を符号化する（`EntryKind::ConfChange` の data）。
pub fn encode_conf_state(c: &ConfState) -> Bytes {
    let mut w = Vec::new();
    w.put_u8(VERSION);
    put_conf_state(&mut w, c);
    Bytes::from(w)
}

pub fn decode_conf_state(b: &Bytes) -> Result<ConfState, CodecError> {
    let mut r = Reader::new(b.clone());
    let v = r.u8()?;
    if v != VERSION {
        return Err(CodecError::Version(v));
    }
    let c = get_conf_state(&mut r)?;
    r.finish()?;
    Ok(c)
}

pub(crate) fn put_snapshot_meta(w: &mut Vec<u8>, m: &SnapshotMeta) {
    w.put_u64(m.index);
    w.put_u64(m.term);
    put_conf_state(w, &m.conf_state);
}

pub(crate) fn get_snapshot_meta(r: &mut Reader) -> Result<SnapshotMeta, CodecError> {
    let index = r.u64()?;
    let term = r.u64()?;
    let conf_state = get_conf_state(r)?;
    Ok(SnapshotMeta {
        index,
        term,
        conf_state,
    })
}

/// entry 1 件を符号化する（記憶域のレコード用）。
pub fn encode_entry(e: &Entry) -> Vec<u8> {
    let mut w = Vec::with_capacity(e.data.len() + 21);
    put_entry(&mut w, e);
    w
}

pub fn decode_entry(b: Bytes) -> Result<Entry, CodecError> {
    let mut r = Reader::new(b);
    let e = get_entry(&mut r)?;
    r.finish()?;
    Ok(e)
}

pub fn encode_hard_state(hs: &HardState) -> Vec<u8> {
    let mut w = Vec::with_capacity(25);
    w.put_u8(VERSION);
    w.put_u64(hs.term);
    w.put_u64(hs.vote);
    w.put_u64(hs.commit);
    w
}

pub fn decode_hard_state(b: Bytes) -> Result<HardState, CodecError> {
    let mut r = Reader::new(b);
    let v = r.u8()?;
    if v != VERSION {
        return Err(CodecError::Version(v));
    }
    let hs = HardState {
        term: r.u64()?,
        vote: r.u64()?,
        commit: r.u64()?,
    };
    r.finish()?;
    Ok(hs)
}

pub fn encode_snapshot_meta(m: &SnapshotMeta) -> Vec<u8> {
    let mut w = Vec::new();
    w.put_u8(VERSION);
    put_snapshot_meta(&mut w, m);
    w
}

pub fn decode_snapshot_meta(b: Bytes) -> Result<SnapshotMeta, CodecError> {
    let mut r = Reader::new(b);
    let v = r.u8()?;
    if v != VERSION {
        return Err(CodecError::Version(v));
    }
    let m = get_snapshot_meta(&mut r)?;
    r.finish()?;
    Ok(m)
}

// ---- メッセージ ----

/// メッセージを枠ごと符号化する（先頭に長さ）。
pub fn encode_message(m: &Message) -> Vec<u8> {
    let mut w = Vec::with_capacity(64);
    w.put_u32(0); // 後で長さを入れる
    w.put_u8(VERSION);
    w.put_u8(m.body.kind());
    w.put_u64(m.from);
    w.put_u64(m.to);
    w.put_u64(m.term);
    match &m.body {
        MsgBody::PreVote {
            last_index,
            last_term,
        } => {
            w.put_u64(*last_index);
            w.put_u64(*last_term);
        }
        MsgBody::PreVoteResp { reject } | MsgBody::VoteResp { reject } => w.put_u8(*reject as u8),
        MsgBody::Vote {
            last_index,
            last_term,
            transfer,
        } => {
            w.put_u64(*last_index);
            w.put_u64(*last_term);
            w.put_u8(*transfer as u8);
        }
        MsgBody::Append {
            prev_index,
            prev_term,
            entries,
            commit,
        } => {
            w.put_u64(*prev_index);
            w.put_u64(*prev_term);
            w.put_u64(*commit);
            w.put_u32(entries.len() as u32);
            for e in entries {
                put_entry(&mut w, e);
            }
        }
        MsgBody::AppendResp {
            reject,
            index,
            reject_hint,
            hint_term,
        } => {
            w.put_u8(*reject as u8);
            w.put_u64(*index);
            w.put_u64(*reject_hint);
            w.put_u64(*hint_term);
        }
        MsgBody::Heartbeat { commit, read_seq } => {
            w.put_u64(*commit);
            w.put_u64(*read_seq);
        }
        MsgBody::HeartbeatResp { read_seq } => w.put_u64(*read_seq),
        MsgBody::Snapshot {
            meta,
            total_len,
            offset,
            data,
            done,
        } => {
            put_snapshot_meta(&mut w, meta);
            w.put_u64(*total_len);
            w.put_u64(*offset);
            w.put_u8(*done as u8);
            w.put_u32(data.len() as u32);
            w.put_slice(data);
        }
        MsgBody::SnapshotResp {
            snap_index,
            snap_term,
            next_offset,
            reject,
        } => {
            w.put_u64(*snap_index);
            w.put_u64(*snap_term);
            w.put_u64(*next_offset);
            w.put_u8(*reject as u8);
        }
        MsgBody::TimeoutNow => {}
    }
    let len = (w.len() - 4) as u32;
    w[0..4].copy_from_slice(&len.to_be_bytes());
    w
}

/// 枠の中身（長さの後ろ）を復号する。
pub fn decode_message_body(b: Bytes) -> Result<Message, CodecError> {
    let mut r = Reader::new(b);
    let v = r.u8()?;
    if v != VERSION {
        return Err(CodecError::Version(v));
    }
    let kind = r.u8()?;
    let from = r.u64()?;
    let to = r.u64()?;
    let term = r.u64()?;
    if from == 0 || to == 0 {
        return Err(CodecError::Invalid("node id"));
    }
    let body = match kind {
        1 => MsgBody::PreVote {
            last_index: r.u64()?,
            last_term: r.u64()?,
        },
        2 => MsgBody::PreVoteResp { reject: r.bool()? },
        3 => MsgBody::Vote {
            last_index: r.u64()?,
            last_term: r.u64()?,
            transfer: r.bool()?,
        },
        4 => MsgBody::VoteResp { reject: r.bool()? },
        5 => {
            let prev_index = r.u64()?;
            let prev_term = r.u64()?;
            let commit = r.u64()?;
            let n = r.u32()? as usize;
            if n > MAX_ENTRIES_PER_MSG {
                return Err(CodecError::TooLarge("entries"));
            }
            // 1 件あたり最低 21 バイト。足りなければ確保する前に弾く。
            if r.remaining() < n.saturating_mul(21) {
                return Err(CodecError::Truncated);
            }
            let mut entries = Vec::with_capacity(n);
            for _ in 0..n {
                entries.push(get_entry(&mut r)?);
            }
            MsgBody::Append {
                prev_index,
                prev_term,
                entries,
                commit,
            }
        }
        6 => MsgBody::AppendResp {
            reject: r.bool()?,
            index: r.u64()?,
            reject_hint: r.u64()?,
            hint_term: r.u64()?,
        },
        7 => MsgBody::Heartbeat {
            commit: r.u64()?,
            read_seq: r.u64()?,
        },
        8 => MsgBody::HeartbeatResp { read_seq: r.u64()? },
        9 => {
            let meta = get_snapshot_meta(&mut r)?;
            let total_len = r.u64()?;
            let offset = r.u64()?;
            let done = r.bool()?;
            let len = r.u32()? as usize;
            if len > MAX_CHUNK {
                return Err(CodecError::TooLarge("snapshot chunk"));
            }
            let data = r.bytes(len)?;
            if offset
                .checked_add(len as u64)
                .is_none_or(|end| end > total_len)
            {
                return Err(CodecError::Invalid("snapshot chunk range"));
            }
            MsgBody::Snapshot {
                meta,
                total_len,
                offset,
                data,
                done,
            }
        }
        10 => MsgBody::SnapshotResp {
            snap_index: r.u64()?,
            snap_term: r.u64()?,
            next_offset: r.u64()?,
            reject: r.bool()?,
        },
        11 => MsgBody::TimeoutNow,
        _ => return Err(CodecError::Invalid("message kind")),
    };
    r.finish()?;
    Ok(Message {
        from,
        to,
        term,
        body,
    })
}

/// 枠ごとの復号（`encode_message` の出力 1 つ）。
pub fn decode_message(frame: Bytes, max_frame: usize) -> Result<Message, CodecError> {
    let mut d = FrameDecoder::new(max_frame);
    d.push(&frame);
    let m = d.next_message()?.ok_or(CodecError::Truncated)?;
    if d.buffered() != 0 {
        return Err(CodecError::Trailing);
    }
    Ok(m)
}

/// 流れ（TCP など）からメッセージを切り出す。誤りを返したら以後は使わない（接続を閉じる）。
pub struct FrameDecoder {
    buf: BytesMut,
    max_frame: usize,
    failed: bool,
}

impl FrameDecoder {
    pub fn new(max_frame: usize) -> FrameDecoder {
        FrameDecoder {
            buf: BytesMut::new(),
            max_frame,
            failed: false,
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// 溜まっているバイト数（呼び手が受信を止める目安）。
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    pub fn next_message(&mut self) -> Result<Option<Message>, CodecError> {
        if self.failed {
            return Err(CodecError::Invalid("decoder failed earlier"));
        }
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > self.max_frame {
            self.failed = true;
            return Err(CodecError::TooLarge("frame"));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        self.buf.advance(4);
        let body = self.buf.split_to(len).freeze();
        match decode_message_body(body) {
            Ok(m) => Ok(Some(m)),
            Err(e) => {
                self.failed = true;
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<Message> {
        let e = |i: u64| Entry {
            term: 3,
            index: i,
            kind: EntryKind::Normal,
            data: Bytes::from(vec![i as u8; i as usize]),
        };
        let conf = ConfState {
            voters: [1, 2, 3].into(),
            voters_outgoing: [1, 2].into(),
            learners: [9].into(),
        };
        let bodies = vec![
            MsgBody::PreVote {
                last_index: 5,
                last_term: 2,
            },
            MsgBody::PreVoteResp { reject: true },
            MsgBody::Vote {
                last_index: 5,
                last_term: 2,
                transfer: true,
            },
            MsgBody::VoteResp { reject: false },
            MsgBody::Append {
                prev_index: 4,
                prev_term: 2,
                entries: vec![e(5), e(6)],
                commit: 4,
            },
            MsgBody::Append {
                prev_index: 4,
                prev_term: 2,
                entries: vec![],
                commit: 4,
            },
            MsgBody::AppendResp {
                reject: true,
                index: 9,
                reject_hint: 3,
                hint_term: 1,
            },
            MsgBody::Heartbeat {
                commit: 7,
                read_seq: 11,
            },
            MsgBody::HeartbeatResp { read_seq: 11 },
            MsgBody::Snapshot {
                meta: SnapshotMeta {
                    index: 100,
                    term: 4,
                    conf_state: conf,
                },
                total_len: 10,
                offset: 4,
                data: Bytes::from_static(b"abcdef"),
                done: true,
            },
            MsgBody::SnapshotResp {
                snap_index: 100,
                snap_term: 4,
                next_offset: 4,
                reject: false,
            },
            MsgBody::TimeoutNow,
        ];
        bodies
            .into_iter()
            .map(|body| Message {
                from: 1,
                to: 2,
                term: 7,
                body,
            })
            .collect()
    }

    #[test]
    fn roundtrip() {
        for m in samples() {
            let b = encode_message(&m);
            let d = decode_message(Bytes::from(b.clone()), DEFAULT_MAX_FRAME).unwrap();
            assert_eq!(d, m);
            // すべての切り詰めは Err か None（パニックしない）
            for cut in 0..b.len() {
                let mut fd = FrameDecoder::new(DEFAULT_MAX_FRAME);
                fd.push(&b[..cut]);
                assert!(matches!(fd.next_message(), Ok(None) | Err(_)));
                // 長さを切り詰めに合わせて書き換えると、本体が足りない → Err
                if cut >= 4 {
                    let mut t = b[..cut].to_vec();
                    let l = (cut - 4) as u32;
                    t[0..4].copy_from_slice(&l.to_be_bytes());
                    let r = decode_message(Bytes::from(t), DEFAULT_MAX_FRAME);
                    if cut < b.len() {
                        assert!(r.is_err(), "cut {cut} of {:?}", m.body);
                    }
                }
            }
        }
    }

    #[test]
    fn stream_and_limits() {
        let mut all = Vec::new();
        for m in samples() {
            all.extend(encode_message(&m));
        }
        let mut fd = FrameDecoder::new(DEFAULT_MAX_FRAME);
        let mut got = Vec::new();
        for chunk in all.chunks(7) {
            fd.push(chunk);
            while let Some(m) = fd.next_message().unwrap() {
                got.push(m);
            }
        }
        assert_eq!(got, samples());
        let mut fd = FrameDecoder::new(16);
        fd.push(&encode_message(&samples()[4]));
        assert_eq!(fd.next_message(), Err(CodecError::TooLarge("frame")));
        assert!(fd.next_message().is_err());
        // 件数だけ大きい Append
        let mut b = encode_message(&Message {
            from: 1,
            to: 2,
            term: 1,
            body: MsgBody::Append {
                prev_index: 0,
                prev_term: 0,
                entries: vec![],
                commit: 0,
            },
        });
        let n = b.len();
        b[n - 4..].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode_message(Bytes::from(b), DEFAULT_MAX_FRAME).is_err());
    }

    #[test]
    fn garbage_never_panics() {
        let mut x = 0x1234_5678_9abc_def0u64;
        for _ in 0..20000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (x % 80) as usize;
            let mut v = Vec::with_capacity(n + 6);
            v.extend_from_slice(&(n as u32 + 2).to_be_bytes());
            v.push(VERSION);
            v.push((x >> 8) as u8 % 13);
            let mut y = x;
            for _ in 0..n {
                y = y
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                v.push((y >> 33) as u8);
            }
            let _ = decode_message(Bytes::from(v), DEFAULT_MAX_FRAME);
        }
    }

    #[test]
    fn crc_known() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_update(crc32(b"1234"), b"56789"), 0xCBF4_3926);
    }

    #[test]
    fn other_records() {
        let hs = HardState {
            term: 5,
            vote: 2,
            commit: 9,
        };
        assert_eq!(
            decode_hard_state(Bytes::from(encode_hard_state(&hs))).unwrap(),
            hs
        );
        let c = ConfState::new([1, 2, 3]);
        assert_eq!(decode_conf_state(&encode_conf_state(&c)).unwrap(), c);
        let e = Entry {
            term: 1,
            index: 2,
            kind: EntryKind::Noop,
            data: Bytes::new(),
        };
        assert_eq!(decode_entry(Bytes::from(encode_entry(&e))).unwrap(), e);
    }
}
