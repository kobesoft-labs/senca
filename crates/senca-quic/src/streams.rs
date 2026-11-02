//! stream（RFC 9000 2〜4 章）: 送受信のバッファ、フロー制御、stream 数の上限。

use std::collections::{BTreeMap, HashMap, VecDeque};

use bytes::Bytes;

use crate::config::TransportConfig;
use crate::error::{StreamError, TransportError, terr};
use crate::frame::{self, ty};
use crate::ranges::RangeSet;
use crate::recovery::SentFrame;
use crate::transport_params::TransportParameters;
use crate::varint;

/// 接続のどちら側か。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Client,
    Server,
}

impl Side {
    pub fn is_client(self) -> bool {
        self == Side::Client
    }
}

/// stream の方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dir {
    Bi,
    Uni,
}

impl Dir {
    fn idx(self) -> usize {
        match self {
            Dir::Bi => 0,
            Dir::Uni => 1,
        }
    }
}

/// stream ID（RFC 9000 2.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamId(pub u64);

impl StreamId {
    pub fn new(initiator: Side, dir: Dir, index: u64) -> Self {
        let mut v = index << 2;
        if initiator == Side::Server {
            v |= 1;
        }
        if dir == Dir::Uni {
            v |= 2;
        }
        Self(v)
    }
    pub fn initiator(self) -> Side {
        if self.0 & 1 == 0 { Side::Client } else { Side::Server }
    }
    pub fn dir(self) -> Dir {
        if self.0 & 2 == 0 { Dir::Bi } else { Dir::Uni }
    }
    pub fn index(self) -> u64 {
        self.0 >> 2
    }
}

impl std::fmt::Display for StreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// 送信バッファ
// ---------------------------------------------------------------------------

/// 送信データ（アプリケーションから受け取り、ACK されるまで持つ）。
#[derive(Debug, Default)]
pub(crate) struct SendBuffer {
    chunks: VecDeque<Bytes>,
    /// chunks[0] の先頭の offset（ここより前は ACK 済みで解放した）。
    base: u64,
    /// 受け取った総量。
    end: u64,
    /// まだ一度も送っていない最初の offset。
    unsent: u64,
    /// 再送が要る範囲。
    retransmit: RangeSet,
    /// base 以降で ACK された範囲。
    acked: RangeSet,
}

impl SendBuffer {
    pub(crate) fn write(&mut self, b: Bytes) {
        self.end += b.len() as u64;
        if !b.is_empty() {
            self.chunks.push_back(b);
        }
    }
    pub(crate) fn end(&self) -> u64 {
        self.end
    }
    pub(crate) fn unsent(&self) -> u64 {
        self.unsent
    }
    /// 未 ACK で抱えている量。
    pub(crate) fn buffered(&self) -> u64 {
        self.end - self.base
    }
    pub(crate) fn has_retransmit(&self) -> bool {
        !self.retransmit.is_empty()
    }
    pub(crate) fn has_unsent(&self) -> bool {
        self.unsent < self.end
    }
    pub(crate) fn all_acked(&self) -> bool {
        self.base == self.end
    }

    /// 次に送る範囲。再送を先にする。新しいデータは `limit`（フロー制御の上限 offset）まで。
    pub(crate) fn next_range(&mut self, max_len: usize, limit: u64) -> Option<(u64, usize, bool)> {
        if max_len == 0 {
            return None;
        }
        if let Some(r) = self.retransmit.pop_min(max_len as u64) {
            return Some((r.start, (r.end - r.start) as usize, true));
        }
        let lim = limit.min(self.end);
        if self.unsent >= lim {
            return None;
        }
        let len = ((lim - self.unsent) as usize).min(max_len);
        let off = self.unsent;
        self.unsent += len as u64;
        Some((off, len, false))
    }

    /// `[offset, offset+len)` を `out` へ写す。
    pub(crate) fn copy_into(&self, offset: u64, len: usize, out: &mut Vec<u8>) {
        let mut pos = self.base;
        let mut need_from = offset;
        let need_to = offset + len as u64;
        for c in &self.chunks {
            let c_end = pos + c.len() as u64;
            if c_end > need_from && pos < need_to {
                let s = (need_from - pos) as usize;
                let e = (need_to.min(c_end) - pos) as usize;
                out.extend_from_slice(&c[s..e]);
                need_from = pos + e as u64;
            }
            if c_end >= need_to {
                break;
            }
            pos = c_end;
        }
    }

    pub(crate) fn ack(&mut self, offset: u64, len: usize) {
        let r = offset.max(self.base)..offset + len as u64;
        if r.start >= r.end {
            return;
        }
        self.acked.insert(r.clone());
        self.retransmit.remove(r);
        // 先頭から連続して ACK された分を解放
        loop {
            let Some(first) = self.acked.iter().next() else { break };
            if first.start > self.base {
                break;
            }
            let new_base = first.end;
            self.acked.remove(first);
            let mut drop = new_base - self.base;
            while drop > 0 {
                let Some(c) = self.chunks.front_mut() else { break };
                if (c.len() as u64) <= drop {
                    drop -= c.len() as u64;
                    self.chunks.pop_front();
                } else {
                    let _ = c.split_to(drop as usize);
                    drop = 0;
                }
            }
            self.base = new_base;
        }
    }

    pub(crate) fn lost(&mut self, offset: u64, len: usize) {
        let r = offset.max(self.base)..offset + len as u64;
        if r.start >= r.end {
            return;
        }
        for g in self.acked.gaps_within(r) {
            self.retransmit.insert(g);
        }
    }

    /// 未 ACK の送信済み範囲をすべて再送に回す（PTO のプローブ）。
    pub(crate) fn requeue_unacked(&mut self) {
        let r = self.base..self.unsent;
        if r.start < r.end {
            for g in self.acked.gaps_within(r) {
                self.retransmit.insert(g);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 受信の並べ直し
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub(crate) struct Assembler {
    /// アプリケーション（または TLS）へ渡した位置。
    read: u64,
    chunks: BTreeMap<u64, Bytes>,
    buffered: usize,
}

impl Assembler {
    pub(crate) fn read_offset(&self) -> u64 {
        self.read
    }
    pub(crate) fn fragments(&self) -> usize {
        self.chunks.len()
    }

    /// 重なりを除いて加える。
    pub(crate) fn insert(&mut self, offset: u64, data: Bytes) {
        let end = offset + data.len() as u64;
        let mut cur = offset.max(self.read);
        if cur >= end {
            return;
        }
        if let Some((&s, c)) = self.chunks.range(..=cur).next_back() {
            let ce = s + c.len() as u64;
            if ce > cur {
                cur = ce;
            }
        }
        while cur < end {
            let next = self.chunks.range(cur..).next().map(|(&s, c)| (s, s + c.len() as u64));
            let gap_end = next.map_or(end, |(s, _)| s.min(end));
            if gap_end > cur {
                let piece = data.slice((cur - offset) as usize..(gap_end - offset) as usize);
                self.buffered += piece.len();
                self.chunks.insert(cur, piece);
            }
            match next {
                Some((s, e)) if s < end => cur = e,
                _ => break,
            }
        }
    }

    /// 連続した先頭部分を最大 `max` バイト取り出す。
    pub(crate) fn pop(&mut self, max: usize) -> Option<Bytes> {
        let (&s, _) = self.chunks.iter().next()?;
        if s != self.read {
            return None;
        }
        let mut c = self.chunks.remove(&s)?;
        if c.len() > max {
            let rest = c.split_off(max);
            self.chunks.insert(s + max as u64, rest);
        }
        self.read += c.len() as u64;
        self.buffered -= c.len();
        Some(c)
    }

    pub(crate) fn clear(&mut self) {
        self.chunks.clear();
        self.buffered = 0;
    }
}

// ---------------------------------------------------------------------------
// stream の状態
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinState {
    None,
    /// FIN を送る（まだ送っていないか失われた）。
    Pending,
    Sent,
    Acked,
}

#[derive(Debug)]
struct ResetState {
    code: u64,
    final_size: u64,
    pending: bool,
    acked: bool,
}

#[derive(Debug)]
pub(crate) struct SendStream {
    buf: SendBuffer,
    /// 相手の MAX_STREAM_DATA。
    max_data: u64,
    fin: FinState,
    reset: Option<ResetState>,
    /// 相手の STOP_SENDING。
    stopped: Option<u64>,
    priority: i32,
    rr: u64,
    blocked_at: Option<u64>,
    blocked_pending: bool,
    want_writable: bool,
}

impl SendStream {
    fn new(max_data: u64) -> Self {
        Self {
            buf: SendBuffer::default(),
            max_data,
            fin: FinState::None,
            reset: None,
            stopped: None,
            priority: 0,
            rr: 0,
            blocked_at: None,
            blocked_pending: false,
            want_writable: false,
        }
    }

    fn is_done(&self) -> bool {
        match &self.reset {
            Some(r) => r.acked,
            None => self.fin == FinState::Acked && self.buf.all_acked(),
        }
    }

    fn has_data(&self, conn_credit: u64) -> bool {
        if self.reset.is_some() {
            return false;
        }
        self.buf.has_retransmit()
            || (self.buf.has_unsent() && self.buf.unsent() < self.max_data && conn_credit > 0)
            || (self.fin == FinState::Pending && !self.buf.has_unsent())
    }
}

#[derive(Debug)]
pub(crate) struct RecvStream {
    asm: Assembler,
    /// 自分が広告した上限。
    max_data: u64,
    window: u64,
    highest: u64,
    final_size: Option<u64>,
    reset: Option<u64>,
    max_data_pending: bool,
    fin_delivered: bool,
    readable_signaled: bool,
}

impl RecvStream {
    fn new(window: u64) -> Self {
        Self {
            asm: Assembler::default(),
            max_data: window,
            window,
            highest: 0,
            final_size: None,
            reset: None,
            max_data_pending: false,
            fin_delivered: false,
            readable_signaled: false,
        }
    }
}

/// アプリケーションへ渡す受信データ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecvChunk {
    /// 受け取ったデータ（空なら今は無い）。
    pub data: Bytes,
    /// これで終わり（FIN まで読んだ）。
    pub fin: bool,
}

/// stream に関するイベント（conn が [`crate::Event`] にする）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StreamEvent {
    Opened(StreamId),
    Readable(StreamId),
    Writable(StreamId),
    Stopped(StreamId, u64),
    /// 送信側が完了（FIN が ACK された・リセットが ACK された）。
    Finished(StreamId),
}

/// 接続の全 stream。
#[derive(Debug)]
pub(crate) struct Streams {
    side: Side,
    send: HashMap<StreamId, SendStream>,
    recv: HashMap<StreamId, RecvStream>,
    next_local: [u64; 2],
    max_local: [u64; 2],
    next_remote: [u64; 2],
    max_remote: [u64; 2],
    max_remote_pending: [bool; 2],
    streams_blocked_at: [Option<u64>; 2],
    streams_blocked_pending: [bool; 2],
    // 接続のフロー制御（送信）
    peer_max_data: u64,
    sent_data: u64,
    data_blocked_at: Option<u64>,
    data_blocked_pending: bool,
    // 接続のフロー制御（受信）
    local_max_data: u64,
    recv_data: u64,
    consumed: u64,
    window: u64,
    max_data_pending: bool,
    // 相手の初期値
    peer_bidi_local: u64,
    peer_bidi_remote: u64,
    peer_uni: u64,
    // 自分の初期値
    stream_window: u64,
    send_buffer: usize,
    max_fragments: usize,
    rr_counter: u64,
    stop_pending: Vec<(StreamId, u64)>,
    pub(crate) events: VecDeque<StreamEvent>,
}

impl Streams {
    pub(crate) fn new(side: Side, cfg: &TransportConfig) -> Self {
        Self {
            side,
            send: HashMap::new(),
            recv: HashMap::new(),
            next_local: [0; 2],
            max_local: [0; 2],
            next_remote: [0; 2],
            max_remote: [cfg.max_concurrent_bidi_streams, cfg.max_concurrent_uni_streams],
            max_remote_pending: [false; 2],
            streams_blocked_at: [None; 2],
            streams_blocked_pending: [false; 2],
            peer_max_data: 0,
            sent_data: 0,
            data_blocked_at: None,
            data_blocked_pending: false,
            local_max_data: cfg.receive_window,
            recv_data: 0,
            consumed: 0,
            window: cfg.receive_window,
            max_data_pending: false,
            peer_bidi_local: 0,
            peer_bidi_remote: 0,
            peer_uni: 0,
            stream_window: cfg.stream_receive_window,
            send_buffer: cfg.stream_send_buffer,
            max_fragments: cfg.max_stream_fragments,
            rr_counter: 0,
            stop_pending: Vec::new(),
            events: VecDeque::new(),
        }
    }

    /// 相手の transport parameters を適用する。
    pub(crate) fn apply_peer_params(&mut self, p: &TransportParameters) {
        self.peer_max_data = p.initial_max_data;
        self.max_local = [p.initial_max_streams_bidi, p.initial_max_streams_uni];
        self.peer_bidi_local = p.initial_max_stream_data_bidi_local;
        self.peer_bidi_remote = p.initial_max_stream_data_bidi_remote;
        self.peer_uni = p.initial_max_stream_data_uni;
    }

    /// 自分の transport parameters に入れる値。
    pub(crate) fn local_params(&self, p: &mut TransportParameters) {
        p.initial_max_data = self.local_max_data;
        p.initial_max_stream_data_bidi_local = self.stream_window;
        p.initial_max_stream_data_bidi_remote = self.stream_window;
        p.initial_max_stream_data_uni = self.stream_window;
        p.initial_max_streams_bidi = self.max_remote[0];
        p.initial_max_streams_uni = self.max_remote[1];
    }

    fn is_local(&self, id: StreamId) -> bool {
        id.initiator() == self.side
    }

    /// 自分から stream を開く。上限に達していれば `None`（STREAMS_BLOCKED を送る）。
    pub(crate) fn open(&mut self, dir: Dir) -> Option<StreamId> {
        let d = dir.idx();
        if self.next_local[d] >= self.max_local[d] {
            if self.streams_blocked_at[d] != Some(self.max_local[d]) {
                self.streams_blocked_at[d] = Some(self.max_local[d]);
                self.streams_blocked_pending[d] = true;
            }
            return None;
        }
        let id = StreamId::new(self.side, dir, self.next_local[d]);
        self.next_local[d] += 1;
        // 自分が開いた stream に相手が送れる量は bidi_remote（相手から見て remote）
        let peer_max = if dir == Dir::Bi { self.peer_bidi_remote } else { self.peer_uni };
        self.send.insert(id, SendStream::new(peer_max));
        if dir == Dir::Bi {
            self.recv.insert(id, RecvStream::new(self.stream_window));
        }
        Some(id)
    }

    /// 自分から開ける残りの数。
    pub(crate) fn remaining_local(&self, dir: Dir) -> u64 {
        self.max_local[dir.idx()] - self.next_local[dir.idx()]
    }

    /// フレームの stream ID を検査し、相手の新しい stream なら開く。
    /// 閉じて忘れた stream なら `Ok(false)`（フレームは無視する）。
    fn touch(&mut self, id: StreamId, frame_ty: u64, sending_part: bool) -> Result<bool, TransportError> {
        let d = id.dir().idx();
        if self.is_local(id) {
            if id.index() >= self.next_local[d] {
                return Err(terr!(STREAM_STATE_ERROR, "開いていない stream {id}").frame(frame_ty));
            }
            // 自分の uni に相手が送ってきた
            if id.dir() == Dir::Uni && !sending_part {
                return Err(terr!(STREAM_STATE_ERROR, "送信専用の stream {id} へのデータ").frame(frame_ty));
            }
        } else {
            if id.dir() == Dir::Uni && sending_part {
                return Err(terr!(STREAM_STATE_ERROR, "受信専用の stream {id} への制御").frame(frame_ty));
            }
            if id.index() >= self.max_remote[d] {
                return Err(terr!(STREAM_LIMIT_ERROR, "stream 数の上限を超えた {id}").frame(frame_ty));
            }
            while self.next_remote[d] <= id.index() {
                let nid = StreamId::new(id.initiator(), id.dir(), self.next_remote[d]);
                self.next_remote[d] += 1;
                self.recv.insert(nid, RecvStream::new(self.stream_window));
                if id.dir() == Dir::Bi {
                    // 相手が開いた bidi に自分が送れる量は相手の bidi_local
                    self.send.insert(nid, SendStream::new(self.peer_bidi_local));
                }
                self.events.push_back(StreamEvent::Opened(nid));
            }
        }
        Ok(if sending_part {
            self.send.contains_key(&id)
        } else {
            self.recv.contains_key(&id)
        })
    }

    fn release(&mut self, id: StreamId) {
        if self.send.contains_key(&id) || self.recv.contains_key(&id) {
            return;
        }
        if !self.is_local(id) {
            let d = id.dir().idx();
            self.max_remote[d] += 1;
            self.max_remote_pending[d] = true;
        }
    }

    fn signal_readable(&mut self, id: StreamId) {
        if let Some(s) = self.recv.get_mut(&id)
            && !s.readable_signaled
        {
            s.readable_signaled = true;
            self.events.push_back(StreamEvent::Readable(id));
        }
    }

    pub(crate) fn on_stream(&mut self, id: StreamId, offset: u64, data: Bytes, fin: bool) -> Result<(), TransportError> {
        let t = ty::STREAM;
        if !self.touch(id, t, false)? {
            return Ok(());
        }
        let Some(s) = self.recv.get_mut(&id) else { return Ok(()) };
        let end = offset + data.len() as u64;
        if let Some(f) = s.final_size
            && (end > f || (fin && end != f))
        {
            return Err(terr!(FINAL_SIZE_ERROR, "final size の不一致 {id}").frame(t));
        }
        if fin && end < s.highest {
            return Err(terr!(FINAL_SIZE_ERROR, "final size が受信済みより小さい {id}").frame(t));
        }
        if end > s.max_data {
            return Err(terr!(FLOW_CONTROL_ERROR, "stream {id} のフロー制御違反").frame(t));
        }
        if end > s.highest {
            let inc = end - s.highest;
            if self.recv_data + inc > self.local_max_data {
                return Err(terr!(FLOW_CONTROL_ERROR, "接続のフロー制御違反").frame(t));
            }
            self.recv_data += inc;
            s.highest = end;
        }
        if fin {
            s.final_size = Some(end);
        }
        if s.reset.is_some() {
            return Ok(());
        }
        s.asm.insert(offset, data);
        if s.asm.fragments() > self.max_fragments {
            return Err(terr!(PROTOCOL_VIOLATION, "stream {id} の断片が多すぎる").frame(t));
        }
        if s.asm.ready() || s.final_size == Some(s.asm.read_offset()) {
            self.signal_readable(id);
        }
        Ok(())
    }

    pub(crate) fn on_reset(&mut self, id: StreamId, code: u64, final_size: u64) -> Result<(), TransportError> {
        let t = ty::RESET_STREAM;
        if !self.touch(id, t, false)? {
            return Ok(());
        }
        let Some(s) = self.recv.get_mut(&id) else { return Ok(()) };
        if final_size < s.highest || s.final_size.is_some_and(|f| f != final_size) {
            return Err(terr!(FINAL_SIZE_ERROR, "RESET_STREAM の final size が不正 {id}").frame(t));
        }
        if final_size > s.max_data {
            return Err(terr!(FLOW_CONTROL_ERROR, "RESET_STREAM がフロー制御を超える {id}").frame(t));
        }
        let inc = final_size - s.highest;
        if self.recv_data + inc > self.local_max_data {
            return Err(terr!(FLOW_CONTROL_ERROR, "接続のフロー制御違反").frame(t));
        }
        self.recv_data += inc;
        s.highest = final_size;
        s.final_size = Some(final_size);
        if s.reset.is_none() && !s.fin_delivered {
            s.reset = Some(code);
            // 読まれないデータは消費したとみなして接続の窓を返す
            let unread = final_size - s.asm.read_offset();
            s.asm.clear();
            self.consumed += unread;
            self.maybe_update_max_data();
            self.signal_readable(id);
        }
        Ok(())
    }

    pub(crate) fn on_stop_sending(&mut self, id: StreamId, code: u64) -> Result<(), TransportError> {
        if !self.touch(id, ty::STOP_SENDING, true)? {
            return Ok(());
        }
        let Some(s) = self.send.get_mut(&id) else { return Ok(()) };
        if s.stopped.is_none() {
            s.stopped = Some(code);
            self.events.push_back(StreamEvent::Stopped(id, code));
        }
        // RFC 9000 3.5: STOP_SENDING を受けたら RESET_STREAM を送る
        if s.reset.is_none() && s.fin != FinState::Acked {
            s.reset = Some(ResetState {
                code,
                final_size: s.buf.unsent(),
                pending: true,
                acked: false,
            });
        }
        Ok(())
    }

    pub(crate) fn on_max_stream_data(&mut self, id: StreamId, max: u64) -> Result<(), TransportError> {
        if !self.touch(id, ty::MAX_STREAM_DATA, true)? {
            return Ok(());
        }
        if let Some(s) = self.send.get_mut(&id)
            && max > s.max_data
        {
            s.max_data = max;
        }
        Ok(())
    }

    pub(crate) fn on_stream_data_blocked(&mut self, id: StreamId) -> Result<(), TransportError> {
        self.touch(id, ty::STREAM_DATA_BLOCKED, false)?;
        Ok(())
    }

    pub(crate) fn on_max_data(&mut self, max: u64) {
        self.peer_max_data = self.peer_max_data.max(max);
    }

    pub(crate) fn on_max_streams(&mut self, bidi: bool, max: u64) {
        let d = if bidi { 0 } else { 1 };
        self.max_local[d] = self.max_local[d].max(max);
    }

    fn maybe_update_max_data(&mut self) {
        if self.local_max_data - self.consumed < self.window / 2 {
            self.local_max_data = self.consumed + self.window;
            self.max_data_pending = true;
        }
    }

    // ---------------------------------------------------------------- アプリケーション API

    pub(crate) fn read(&mut self, id: StreamId, max: usize) -> Result<RecvChunk, StreamError> {
        let s = self.recv.get_mut(&id).ok_or(StreamError::UnknownStream)?;
        if let Some(code) = s.reset {
            self.recv.remove(&id);
            self.release(id);
            return Err(StreamError::Reset(code));
        }
        let data = s.asm.pop(max.max(1));
        let n = data.as_ref().map_or(0, |d| d.len() as u64);
        let fin = s.final_size == Some(s.asm.read_offset());
        if n > 0 {
            // stream の窓
            if s.max_data - s.asm.read_offset() < s.window / 2 && s.final_size.is_none() {
                s.max_data = s.asm.read_offset() + s.window;
                s.max_data_pending = true;
            }
        }
        if data.is_none() && !fin {
            s.readable_signaled = false;
        }
        if fin {
            s.fin_delivered = true;
            self.recv.remove(&id);
            self.release(id);
        }
        if n > 0 {
            self.consumed += n;
            self.maybe_update_max_data();
        }
        Ok(RecvChunk {
            data: data.unwrap_or_default(),
            fin,
        })
    }

    /// 受け付けたバイト数を返す（送信バッファの上限まで）。
    pub(crate) fn write(&mut self, id: StreamId, data: &Bytes) -> Result<usize, StreamError> {
        let cap = self.send_buffer as u64;
        let s = self.send.get_mut(&id).ok_or(StreamError::UnknownStream)?;
        if let Some(code) = s.stopped {
            return Err(StreamError::Stopped(code));
        }
        if s.reset.is_some() || s.fin != FinState::None {
            return Err(StreamError::Finished);
        }
        let room = cap.saturating_sub(s.buf.buffered()) as usize;
        let n = room.min(data.len());
        if n < data.len() {
            s.want_writable = true;
        }
        if n > 0 {
            s.buf.write(data.slice(..n));
        }
        Ok(n)
    }

    pub(crate) fn finish(&mut self, id: StreamId) -> Result<(), StreamError> {
        let s = self.send.get_mut(&id).ok_or(StreamError::UnknownStream)?;
        if let Some(code) = s.stopped {
            return Err(StreamError::Stopped(code));
        }
        if s.reset.is_some() || s.fin != FinState::None {
            return Err(StreamError::Finished);
        }
        s.fin = FinState::Pending;
        Ok(())
    }

    pub(crate) fn reset(&mut self, id: StreamId, code: u64) -> Result<(), StreamError> {
        let s = self.send.get_mut(&id).ok_or(StreamError::UnknownStream)?;
        if s.reset.is_some() || s.fin == FinState::Acked {
            return Ok(());
        }
        s.reset = Some(ResetState {
            code,
            final_size: s.buf.unsent(),
            pending: true,
            acked: false,
        });
        Ok(())
    }

    pub(crate) fn stop(&mut self, id: StreamId, code: u64) -> Result<(), StreamError> {
        let s = self.recv.get_mut(&id).ok_or(StreamError::UnknownStream)?;
        if s.reset.is_none() && s.final_size != Some(s.asm.read_offset()) {
            if self.stop_pending.len() < 1024 {
                self.stop_pending.push((id, code));
            }
            let unread = s.highest - s.asm.read_offset();
            s.asm.clear();
            self.consumed += unread;
            self.maybe_update_max_data();
        }
        // 受信側はもう読まない。相手の RESET_STREAM か FIN を待たずに忘れる（後のフレームは無視）。
        self.recv.remove(&id);
        self.release(id);
        Ok(())
    }

    pub(crate) fn set_priority(&mut self, id: StreamId, p: i32) -> Result<(), StreamError> {
        self.send.get_mut(&id).ok_or(StreamError::UnknownStream)?.priority = p;
        Ok(())
    }

    pub(crate) fn exists(&self, id: StreamId) -> bool {
        self.send.contains_key(&id) || self.recv.contains_key(&id)
    }

    pub(crate) fn send_buffered(&self, id: StreamId) -> Option<u64> {
        self.send.get(&id).map(|s| s.buf.buffered())
    }

    // ---------------------------------------------------------------- 送信

    pub(crate) fn has_pending(&self) -> bool {
        let credit = self.peer_max_data.saturating_sub(self.sent_data);
        self.max_data_pending
            || self.data_blocked_pending
            || self.max_remote_pending.iter().any(|&b| b)
            || self.streams_blocked_pending.iter().any(|&b| b)
            || !self.stop_pending.is_empty()
            || self.recv.values().any(|s| s.max_data_pending)
            || self.send.values().any(|s| s.has_data(credit) || s.blocked_pending || s.reset.as_ref().is_some_and(|r| r.pending))
    }

    /// 制御フレームと stream データを書く。`max` は使ってよいバイト数。
    pub(crate) fn write_frames(&mut self, out: &mut Vec<u8>, max: usize, sent: &mut Vec<SentFrame>) {
        let start = out.len();
        let room = |out: &Vec<u8>| max.saturating_sub(out.len() - start);
        // 接続の制御
        if self.max_data_pending && room(out) >= 9 {
            frame::write2(out, ty::MAX_DATA, self.local_max_data);
            self.max_data_pending = false;
            sent.push(SentFrame::MaxData);
        }
        for d in 0..2 {
            if self.max_remote_pending[d] && room(out) >= 9 {
                let t = if d == 0 { ty::MAX_STREAMS_BIDI } else { ty::MAX_STREAMS_UNI };
                frame::write2(out, t, self.max_remote[d]);
                self.max_remote_pending[d] = false;
                sent.push(SentFrame::MaxStreams { bidi: d == 0 });
            }
            if self.streams_blocked_pending[d] && room(out) >= 9 {
                let t = if d == 0 { ty::STREAMS_BLOCKED_BIDI } else { ty::STREAMS_BLOCKED_UNI };
                frame::write2(out, t, self.max_local[d]);
                self.streams_blocked_pending[d] = false;
                sent.push(SentFrame::StreamsBlocked { bidi: d == 0 });
            }
        }
        if self.data_blocked_pending && room(out) >= 9 {
            frame::write2(out, ty::DATA_BLOCKED, self.peer_max_data);
            self.data_blocked_pending = false;
            sent.push(SentFrame::DataBlocked);
        }
        // stream ごとの制御
        for (id, s) in self.recv.iter_mut() {
            if s.max_data_pending && room(out) >= 17 {
                frame::write3(out, ty::MAX_STREAM_DATA, id.0, s.max_data);
                s.max_data_pending = false;
                sent.push(SentFrame::MaxStreamData { id: *id });
            }
        }
        for (id, s) in self.send.iter_mut() {
            if let Some(r) = &mut s.reset
                && r.pending
                && room(out) >= 25
            {
                varint::write(out, ty::RESET_STREAM);
                varint::write(out, id.0);
                varint::write(out, r.code);
                varint::write(out, r.final_size);
                r.pending = false;
                sent.push(SentFrame::ResetStream { id: *id });
            }
            if s.blocked_pending && room(out) >= 17 {
                frame::write3(out, ty::STREAM_DATA_BLOCKED, id.0, s.max_data);
                s.blocked_pending = false;
                sent.push(SentFrame::StreamDataBlocked { id: *id });
            }
        }
        // STOP_SENDING（stop() で recv から消した stream の分も送るため別に持つ）
        while let Some(&(id, code)) = self.stop_pending.last() {
            if room(out) < 17 {
                break;
            }
            frame::write3(out, ty::STOP_SENDING, id.0, code);
            self.stop_pending.pop();
            sent.push(SentFrame::StopSending { id, code });
        }

        // データ（優先度の高い順、同じ優先度は順番に）
        let credit = self.peer_max_data.saturating_sub(self.sent_data);
        let mut order: Vec<(i32, u64, StreamId)> = self
            .send
            .iter()
            .filter(|(_, s)| s.has_data(credit))
            .map(|(id, s)| (s.priority, s.rr, *id))
            .collect();
        order.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        for (_, _, id) in order {
            let avail = room(out);
            if avail < 8 {
                break;
            }
            let credit = self.peer_max_data.saturating_sub(self.sent_data);
            let Some(s) = self.send.get_mut(&id) else { continue };
            let limit = s.max_data.min(s.buf.unsent().saturating_add(credit));
            // ヘッダーの長さの見積もり（offset・length は最大 8 バイト）
            let hdr = frame::stream_header_len(id.0, s.buf.end(), avail);
            if avail <= hdr {
                continue;
            }
            let before_unsent = s.buf.unsent();
            let mut wrote = false;
            if let Some((off, len, _retx)) = s.buf.next_range(avail - hdr, limit) {
                let fin = s.fin != FinState::None && s.fin != FinState::Acked && off + len as u64 == s.buf.end();
                frame::write_stream_header(out, id.0, off, len, fin);
                s.buf.copy_into(off, len, out);
                if fin {
                    s.fin = FinState::Sent;
                }
                sent.push(SentFrame::Stream { id, offset: off, len, fin });
                wrote = true;
            } else if s.fin == FinState::Pending && !s.buf.has_unsent() {
                let off = s.buf.end();
                frame::write_stream_header(out, id.0, off, 0, true);
                s.fin = FinState::Sent;
                sent.push(SentFrame::Stream { id, offset: off, len: 0, fin: true });
                wrote = true;
            }
            let new = s.buf.unsent() - before_unsent;
            self.sent_data += new;
            if wrote {
                self.rr_counter += 1;
                s.rr = self.rr_counter;
            }
            // フロー制御で止まった
            if s.buf.has_unsent() && s.buf.unsent() >= s.max_data && s.blocked_at != Some(s.max_data) {
                s.blocked_at = Some(s.max_data);
                s.blocked_pending = true;
            }
        }
        let any_unsent = self.send.values().any(|s| s.buf.has_unsent() && s.reset.is_none());
        if any_unsent && self.sent_data >= self.peer_max_data && self.data_blocked_at != Some(self.peer_max_data) {
            self.data_blocked_at = Some(self.peer_max_data);
            self.data_blocked_pending = true;
        }
    }

    // ---------------------------------------------------------------- ACK・損失

    pub(crate) fn on_acked(&mut self, f: &SentFrame) {
        match *f {
            SentFrame::Stream { id, offset, len, fin } => {
                let Some(s) = self.send.get_mut(&id) else { return };
                s.buf.ack(offset, len);
                if fin && s.fin == FinState::Sent {
                    s.fin = FinState::Acked;
                }
                if s.want_writable && s.buf.buffered() < self.send_buffer as u64 && s.reset.is_none() {
                    s.want_writable = false;
                    self.events.push_back(StreamEvent::Writable(id));
                }
                if s.is_done() {
                    self.send.remove(&id);
                    self.events.push_back(StreamEvent::Finished(id));
                    self.release(id);
                }
            }
            SentFrame::ResetStream { id } => {
                if let Some(s) = self.send.get_mut(&id)
                    && let Some(r) = &mut s.reset
                {
                    r.acked = true;
                    self.send.remove(&id);
                    self.release(id);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn on_lost(&mut self, f: &SentFrame) {
        match *f {
            SentFrame::Stream { id, offset, len, fin } => {
                let Some(s) = self.send.get_mut(&id) else { return };
                if s.reset.is_some() {
                    return;
                }
                s.buf.lost(offset, len);
                if fin && s.fin == FinState::Sent {
                    s.fin = FinState::Pending;
                }
            }
            SentFrame::ResetStream { id } => {
                if let Some(s) = self.send.get_mut(&id)
                    && let Some(r) = &mut s.reset
                    && !r.acked
                {
                    r.pending = true;
                }
            }
            SentFrame::MaxData => self.max_data_pending = true,
            SentFrame::StopSending { id, code } => {
                if self.stop_pending.len() < 1024 {
                    self.stop_pending.push((id, code));
                }
            }
            SentFrame::MaxStreamData { id } => {
                if let Some(s) = self.recv.get_mut(&id)
                    && s.final_size.is_none()
                {
                    s.max_data_pending = true;
                }
            }
            SentFrame::MaxStreams { bidi } => self.max_remote_pending[if bidi { 0 } else { 1 }] = true,
            SentFrame::DataBlocked => {
                if self.data_blocked_at == Some(self.peer_max_data) {
                    self.data_blocked_pending = true;
                }
            }
            SentFrame::StreamDataBlocked { id } => {
                if let Some(s) = self.send.get_mut(&id)
                    && s.blocked_at == Some(s.max_data)
                {
                    s.blocked_pending = true;
                }
            }
            SentFrame::StreamsBlocked { bidi } => {
                let d = if bidi { 0 } else { 1 };
                if self.streams_blocked_at[d] == Some(self.max_local[d]) {
                    self.streams_blocked_pending[d] = true;
                }
            }
            _ => {}
        }
    }

    /// PTO のプローブのため、未 ACK のデータを再送に回す（優先度の高い stream から）。
    pub(crate) fn requeue_for_probe(&mut self) {
        for s in self.send.values_mut() {
            if s.reset.is_none() {
                s.buf.requeue_unacked();
                if s.fin == FinState::Sent {
                    s.fin = FinState::Pending;
                }
            }
        }
    }
}

impl Assembler {
    /// 先頭から読めるデータがあるか。
    pub(crate) fn ready(&self) -> bool {
        self.chunks.keys().next() == Some(&self.read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembler_overlaps() {
        let mut a = Assembler::default();
        a.insert(5, Bytes::from_static(b"fghij"));
        a.insert(0, Bytes::from_static(b"abcdefg"));
        a.insert(3, Bytes::from_static(b"dXXXXXXXXk"));
        let mut s = Vec::new();
        while let Some(c) = a.pop(3) {
            s.extend_from_slice(&c);
        }
        assert_eq!(s, b"abcdefghijXXk".to_vec());
    }

    #[test]
    fn send_buffer_ack_and_loss() {
        let mut b = SendBuffer::default();
        b.write(Bytes::from_static(b"hello "));
        b.write(Bytes::from_static(b"world"));
        let (o1, l1, _) = b.next_range(4, u64::MAX).unwrap();
        let (o2, l2, _) = b.next_range(100, u64::MAX).unwrap();
        assert_eq!((o1, l1, o2, l2), (0, 4, 4, 7));
        b.lost(0, 4);
        b.ack(4, 7);
        let (o3, l3, re) = b.next_range(100, u64::MAX).unwrap();
        assert_eq!((o3, l3, re), (0, 4, true));
        let mut out = Vec::new();
        b.copy_into(0, 4, &mut out);
        assert_eq!(out, b"hell");
        b.ack(0, 4);
        assert!(b.all_acked());
    }
}
