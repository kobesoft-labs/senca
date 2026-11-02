//! フレーム（RFC 9000 19 章、RFC 9221 4 章）の読み書き。

use crate::cid::{ConnectionId, ResetToken};
use crate::coding::{Reader, Short};
use crate::error::{TransportError, terr};
use crate::varint;

pub(crate) mod ty {
    pub const PADDING: u64 = 0x00;
    pub const PING: u64 = 0x01;
    pub const ACK: u64 = 0x02;
    pub const ACK_ECN: u64 = 0x03;
    pub const RESET_STREAM: u64 = 0x04;
    pub const STOP_SENDING: u64 = 0x05;
    pub const CRYPTO: u64 = 0x06;
    pub const NEW_TOKEN: u64 = 0x07;
    pub const STREAM: u64 = 0x08; // 0x08..=0x0f
    pub const MAX_DATA: u64 = 0x10;
    pub const MAX_STREAM_DATA: u64 = 0x11;
    pub const MAX_STREAMS_BIDI: u64 = 0x12;
    pub const MAX_STREAMS_UNI: u64 = 0x13;
    pub const DATA_BLOCKED: u64 = 0x14;
    pub const STREAM_DATA_BLOCKED: u64 = 0x15;
    pub const STREAMS_BLOCKED_BIDI: u64 = 0x16;
    pub const STREAMS_BLOCKED_UNI: u64 = 0x17;
    pub const NEW_CONNECTION_ID: u64 = 0x18;
    pub const RETIRE_CONNECTION_ID: u64 = 0x19;
    pub const PATH_CHALLENGE: u64 = 0x1a;
    pub const PATH_RESPONSE: u64 = 0x1b;
    pub const CONNECTION_CLOSE: u64 = 0x1c;
    pub const APPLICATION_CLOSE: u64 = 0x1d;
    pub const HANDSHAKE_DONE: u64 = 0x1e;
    pub const DATAGRAM: u64 = 0x30;
    pub const DATAGRAM_LEN: u64 = 0x31;
}

/// 一つの ACK フレームに入れる区間の上限（受信側の処理量の上限）。
pub(crate) const MAX_ACK_RANGES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Frame<'a> {
    Padding,
    Ping,
    Ack {
        /// 降順の閉区間 (smallest, largest)。先頭が最大。
        ranges: Vec<(u64, u64)>,
        delay: u64,
    },
    ResetStream {
        id: u64,
        code: u64,
        final_size: u64,
    },
    StopSending {
        id: u64,
        code: u64,
    },
    Crypto {
        offset: u64,
        data: &'a [u8],
    },
    NewToken {
        token: &'a [u8],
    },
    Stream {
        id: u64,
        offset: u64,
        data: &'a [u8],
        fin: bool,
    },
    MaxData(u64),
    MaxStreamData {
        id: u64,
        max: u64,
    },
    MaxStreams {
        bidi: bool,
        max: u64,
    },
    DataBlocked(u64),
    StreamDataBlocked {
        id: u64,
        limit: u64,
    },
    StreamsBlocked {
        bidi: bool,
        limit: u64,
    },
    NewConnectionId {
        seq: u64,
        retire_prior_to: u64,
        cid: ConnectionId,
        token: ResetToken,
    },
    RetireConnectionId {
        seq: u64,
    },
    PathChallenge([u8; 8]),
    PathResponse([u8; 8]),
    ConnectionClose {
        app: bool,
        code: u64,
        frame_type: u64,
        reason: &'a [u8],
    },
    HandshakeDone,
    Datagram {
        data: &'a [u8],
    },
}

impl Frame<'_> {
    /// ACK を引き起こすか（RFC 9002 2）。
    pub(crate) fn ack_eliciting(&self) -> bool {
        !matches!(self, Frame::Padding | Frame::Ack { .. } | Frame::ConnectionClose { .. })
    }

    /// path probing フレームか（RFC 9000 9.1）。
    pub(crate) fn probing(&self) -> bool {
        matches!(
            self,
            Frame::Padding | Frame::PathChallenge(_) | Frame::PathResponse(_) | Frame::NewConnectionId { .. }
        )
    }

    /// Initial・Handshake パケットに入れてよいか（RFC 9000 12.4 表 3）。
    pub(crate) fn allowed_in_handshake(&self) -> bool {
        matches!(
            self,
            Frame::Padding
                | Frame::Ping
                | Frame::Ack { .. }
                | Frame::Crypto { .. }
                | Frame::ConnectionClose { app: false, .. }
        )
    }
}

/// フレームを順に読む。
pub(crate) struct Iter<'a> {
    r: Reader<'a>,
    pub(crate) last_type: u64,
}

impl<'a> Iter<'a> {
    pub(crate) fn new(payload: &'a [u8]) -> Self {
        Self {
            r: Reader::new(payload),
            last_type: 0,
        }
    }

    fn enc(&self) -> TransportError {
        terr!(FRAME_ENCODING_ERROR, "フレームの符号化が不正").frame(self.last_type)
    }

    pub(crate) fn next_frame(&mut self) -> Option<Result<Frame<'a>, TransportError>> {
        if self.r.is_empty() {
            return None;
        }
        Some(self.read())
    }

    fn read(&mut self) -> Result<Frame<'a>, TransportError> {
        let start = self.r.pos();
        let t = self.r.varint().map_err(|_| self.enc())?;
        self.last_type = t;
        // フレームの型は最短で符号化しなければならない（RFC 9000 12.4）
        if self.r.pos() - start != varint::encoded_len(t) {
            return Err(terr!(PROTOCOL_VIOLATION, "フレームの型が最短の符号化でない").frame(t));
        }
        let e = |_| terr!(FRAME_ENCODING_ERROR, "フレームの符号化が不正").frame(t);
        let r = &mut self.r;
        Ok(match t {
            ty::PADDING => {
                // 連続する PADDING はまとめて読む
                while r.remaining() > 0 && r.clone().u8() == Ok(0) {
                    let _ = r.u8();
                }
                Frame::Padding
            }
            ty::PING => Frame::Ping,
            ty::ACK | ty::ACK_ECN => {
                let largest = r.varint().map_err(e)?;
                let delay = r.varint().map_err(e)?;
                let count = r.varint().map_err(e)?;
                let first = r.varint().map_err(e)?;
                if first > largest {
                    return Err(e(Short));
                }
                if count as usize >= MAX_ACK_RANGES {
                    return Err(terr!(FRAME_ENCODING_ERROR, "ACK の区間が多すぎる").frame(t));
                }
                let mut ranges = Vec::with_capacity(count as usize + 1);
                let mut smallest = largest - first;
                ranges.push((smallest, largest));
                for _ in 0..count {
                    let gap = r.varint().map_err(e)?;
                    let len = r.varint().map_err(e)?;
                    let hi = smallest.checked_sub(gap).and_then(|v| v.checked_sub(2)).ok_or_else(|| e(Short))?;
                    let lo = hi.checked_sub(len).ok_or_else(|| e(Short))?;
                    ranges.push((lo, hi));
                    smallest = lo;
                }
                if t == ty::ACK_ECN {
                    for _ in 0..3 {
                        r.varint().map_err(e)?;
                    }
                }
                Frame::Ack { ranges, delay }
            }
            ty::RESET_STREAM => Frame::ResetStream {
                id: r.varint().map_err(e)?,
                code: r.varint().map_err(e)?,
                final_size: r.varint().map_err(e)?,
            },
            ty::STOP_SENDING => Frame::StopSending {
                id: r.varint().map_err(e)?,
                code: r.varint().map_err(e)?,
            },
            ty::CRYPTO => {
                let offset = r.varint().map_err(e)?;
                let data = r.varint_bytes().map_err(e)?;
                if offset + data.len() as u64 > varint::MAX {
                    return Err(terr!(FRAME_ENCODING_ERROR, "CRYPTO の範囲が 2^62 を超える").frame(t));
                }
                Frame::Crypto { offset, data }
            }
            ty::NEW_TOKEN => {
                let token = r.varint_bytes().map_err(e)?;
                if token.is_empty() {
                    return Err(e(Short));
                }
                Frame::NewToken { token }
            }
            0x08..=0x0f => {
                let id = r.varint().map_err(e)?;
                let offset = if t & 0x04 != 0 { r.varint().map_err(e)? } else { 0 };
                let data = if t & 0x02 != 0 {
                    r.varint_bytes().map_err(e)?
                } else {
                    r.rest()
                };
                if offset + data.len() as u64 > varint::MAX {
                    return Err(terr!(FRAME_ENCODING_ERROR, "STREAM の範囲が 2^62 を超える").frame(t));
                }
                Frame::Stream {
                    id,
                    offset,
                    data,
                    fin: t & 0x01 != 0,
                }
            }
            ty::MAX_DATA => Frame::MaxData(r.varint().map_err(e)?),
            ty::MAX_STREAM_DATA => Frame::MaxStreamData {
                id: r.varint().map_err(e)?,
                max: r.varint().map_err(e)?,
            },
            ty::MAX_STREAMS_BIDI | ty::MAX_STREAMS_UNI => {
                let max = r.varint().map_err(e)?;
                if max > 1 << 60 {
                    return Err(terr!(FRAME_ENCODING_ERROR, "MAX_STREAMS が 2^60 を超える").frame(t));
                }
                Frame::MaxStreams {
                    bidi: t == ty::MAX_STREAMS_BIDI,
                    max,
                }
            }
            ty::DATA_BLOCKED => Frame::DataBlocked(r.varint().map_err(e)?),
            ty::STREAM_DATA_BLOCKED => Frame::StreamDataBlocked {
                id: r.varint().map_err(e)?,
                limit: r.varint().map_err(e)?,
            },
            ty::STREAMS_BLOCKED_BIDI | ty::STREAMS_BLOCKED_UNI => {
                let limit = r.varint().map_err(e)?;
                if limit > 1 << 60 {
                    return Err(terr!(FRAME_ENCODING_ERROR, "STREAMS_BLOCKED が 2^60 を超える").frame(t));
                }
                Frame::StreamsBlocked {
                    bidi: t == ty::STREAMS_BLOCKED_BIDI,
                    limit,
                }
            }
            ty::NEW_CONNECTION_ID => {
                let seq = r.varint().map_err(e)?;
                let retire_prior_to = r.varint().map_err(e)?;
                let len = r.u8().map_err(e)? as usize;
                if len == 0 || len > 20 || retire_prior_to > seq {
                    return Err(e(Short));
                }
                let cid = ConnectionId::new(r.bytes(len).map_err(e)?).ok_or_else(|| e(Short))?;
                let token = r.array::<16>().map_err(e)?;
                Frame::NewConnectionId {
                    seq,
                    retire_prior_to,
                    cid,
                    token,
                }
            }
            ty::RETIRE_CONNECTION_ID => Frame::RetireConnectionId {
                seq: r.varint().map_err(e)?,
            },
            ty::PATH_CHALLENGE => Frame::PathChallenge(r.array().map_err(e)?),
            ty::PATH_RESPONSE => Frame::PathResponse(r.array().map_err(e)?),
            ty::CONNECTION_CLOSE | ty::APPLICATION_CLOSE => {
                let code = r.varint().map_err(e)?;
                let frame_type = if t == ty::CONNECTION_CLOSE { r.varint().map_err(e)? } else { 0 };
                let reason = r.varint_bytes().map_err(e)?;
                Frame::ConnectionClose {
                    app: t == ty::APPLICATION_CLOSE,
                    code,
                    frame_type,
                    reason,
                }
            }
            ty::HANDSHAKE_DONE => Frame::HandshakeDone,
            ty::DATAGRAM => Frame::Datagram { data: r.rest() },
            ty::DATAGRAM_LEN => Frame::Datagram {
                data: r.varint_bytes().map_err(e)?,
            },
            _ => return Err(terr!(FRAME_ENCODING_ERROR, "未知のフレーム型 0x{t:x}").frame(t)),
        })
    }
}

// ---------------------------------------------------------------------------
// 書き込み
// ---------------------------------------------------------------------------

/// ACK フレーム。`ranges` は昇順の半開区間（RangeSet の順）。区間が多いときは大きい側から
/// `max_ranges` 個まで。書いたバイト数を返す。
pub(crate) fn write_ack(out: &mut Vec<u8>, ranges: &[(u64, u64)], delay: u64, max_ranges: usize) {
    // ranges: 降順の閉区間
    let n = ranges.len().min(max_ranges).max(1);
    let (lo0, hi0) = ranges[0];
    varint::write(out, ty::ACK);
    varint::write(out, hi0);
    varint::write(out, delay);
    varint::write(out, (n - 1) as u64);
    varint::write(out, hi0 - lo0);
    let mut smallest = lo0;
    for &(lo, hi) in &ranges[1..n] {
        varint::write(out, smallest - hi - 2);
        varint::write(out, hi - lo);
        smallest = lo;
    }
}

pub(crate) fn write_crypto_header(out: &mut Vec<u8>, offset: u64, len: usize) {
    varint::write(out, ty::CRYPTO);
    varint::write(out, offset);
    varint::write(out, len as u64);
}

/// STREAM フレームのヘッダー（offset・length を常に入れる）。
pub(crate) fn write_stream_header(out: &mut Vec<u8>, id: u64, offset: u64, len: usize, fin: bool) {
    let mut t = ty::STREAM | 0x02;
    if offset != 0 {
        t |= 0x04;
    }
    if fin {
        t |= 0x01;
    }
    varint::write(out, t);
    varint::write(out, id);
    if offset != 0 {
        varint::write(out, offset);
    }
    varint::write(out, len as u64);
}

pub(crate) fn stream_header_len(id: u64, offset: u64, len: usize) -> usize {
    1 + varint::encoded_len(id)
        + if offset != 0 { varint::encoded_len(offset) } else { 0 }
        + varint::encoded_len(len as u64)
}

pub(crate) fn write2(out: &mut Vec<u8>, t: u64, a: u64) {
    varint::write(out, t);
    varint::write(out, a);
}

pub(crate) fn write3(out: &mut Vec<u8>, t: u64, a: u64, b: u64) {
    varint::write(out, t);
    varint::write(out, a);
    varint::write(out, b);
}

pub(crate) fn write_new_cid(out: &mut Vec<u8>, seq: u64, retire_prior_to: u64, cid: &ConnectionId, token: &ResetToken) {
    varint::write(out, ty::NEW_CONNECTION_ID);
    varint::write(out, seq);
    varint::write(out, retire_prior_to);
    out.push(cid.len() as u8);
    out.extend_from_slice(cid.as_slice());
    out.extend_from_slice(token);
}

pub(crate) fn write_close(out: &mut Vec<u8>, app: bool, code: u64, frame_type: u64, reason: &[u8]) {
    if app {
        varint::write(out, ty::APPLICATION_CLOSE);
        varint::write(out, code);
    } else {
        varint::write(out, ty::CONNECTION_CLOSE);
        varint::write(out, code);
        varint::write(out, frame_type);
    }
    varint::write(out, reason.len() as u64);
    out.extend_from_slice(reason);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(p: &[u8]) -> Result<Vec<Frame<'_>>, TransportError> {
        let mut it = Iter::new(p);
        let mut v = Vec::new();
        while let Some(f) = it.next_frame() {
            v.push(f?);
        }
        Ok(v)
    }

    #[test]
    fn ack_roundtrip() {
        let mut o = Vec::new();
        write_ack(&mut o, &[(10, 12), (5, 7), (0, 2)], 9, 16);
        let f = all(&o).unwrap();
        assert_eq!(
            f,
            vec![Frame::Ack {
                ranges: vec![(10, 12), (5, 7), (0, 2)],
                delay: 9
            }]
        );
    }

    #[test]
    fn rejects_bad_frames() {
        // ACK の first range が largest を超える
        assert!(all(&[0x02, 0x01, 0x00, 0x00, 0x05]).is_err());
        // ACK のギャップが 0 未満
        assert!(all(&[0x02, 0x05, 0x00, 0x01, 0x04, 0x01, 0x00]).is_err());
        // 未知の型
        assert!(all(&[0x21]).is_err());
        // 型の非最短符号化
        assert!(all(&[0x40, 0x01]).is_err());
        // NEW_CONNECTION_ID の retire_prior_to > seq
        let mut n = vec![0x18, 0x01, 0x02, 0x04, 1, 2, 3, 4];
        n.extend_from_slice(&[0; 16]);
        assert!(all(&n).is_err());
        // 短い PATH_CHALLENGE
        assert!(all(&[0x1a, 1, 2, 3]).is_err());
        // STREAM の範囲あふれ
        let mut s = vec![0x0e, 0x00];
        varint::write(&mut s, varint::MAX);
        s.extend_from_slice(&[0x01, 0xaa]);
        assert!(all(&s).is_err());
    }

    #[test]
    fn stream_and_close_roundtrip() {
        let mut o = Vec::new();
        write_stream_header(&mut o, 4, 100, 3, true);
        o.extend_from_slice(b"abc");
        write_close(&mut o, true, 7, 0, b"bye");
        let f = all(&o).unwrap();
        assert_eq!(
            f[0],
            Frame::Stream {
                id: 4,
                offset: 100,
                data: b"abc",
                fin: true
            }
        );
        assert_eq!(
            f[1],
            Frame::ConnectionClose {
                app: true,
                code: 7,
                frame_type: 0,
                reason: b"bye"
            }
        );
    }
}
