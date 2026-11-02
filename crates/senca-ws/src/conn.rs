//! WebSocket の接続（RFC 6455 5〜7 章）。sans-io。
//!
//! - mask の規則: クライアントの frame は mask 必須、サーバーの frame は mask 禁止（違えば 1002）。
//! - 分割: 続きの frame（continuation）の前後の誤り、分割の途中の別の data frame は 1002。制御 frame は
//!   分割の間に挟まってよい。制御 frame は FIN 必須で payload は 125 byte まで。
//! - text は UTF-8 を逐次に検査し、分割の途中でも誤りが確定した時点で 1007 で閉じる（fail fast）。
//! - 上限: frame の長さ・message の長さ・message の frame 数（超えたら 1009）。
//! - ping には同じ payload の pong を自動で返す。close を受けたら同じ code の close を返して閉じる。

use std::collections::VecDeque;

use bytes::{Bytes, BytesMut};

use crate::frame::{FrameHeader, HeaderError, OpCode, apply_mask, close_code_is_valid, encode_frame};

/// こちらの立場。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Server,
    Client,
}

/// 上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// 1 つの frame の payload の上限。
    pub max_frame_size: usize,
    /// 1 つの message（分割を組み立てた後）の上限。
    pub max_message_size: usize,
    /// 1 つの message の frame 数の上限（小さな frame を大量に送る攻撃を防ぐ）。
    pub max_fragments: usize,
    /// 送りの待ち行列の上限（frame 数）。超えた送りは `WsError::Backpressure`。
    pub max_queued_frames: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_frame_size: 1 << 20,
            max_message_size: 1 << 20,
            max_fragments: 1024,
            max_queued_frames: 4096,
        }
    }
}

/// close frame の中身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFrame {
    pub code: u16,
    pub reason: String,
}

/// 受けた message。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Bytes),
    /// pong は自動で返した。
    Ping(Bytes),
    Pong(Bytes),
    /// 相手の close（code の無い close は `None`）。返事の close は自動で送った（こちらが先に送っていなければ）。
    Close(Option<CloseFrame>),
}

/// 誤り。受信の誤りでは、こちらから close（[`close_code`](WsError::close_code)）を送って閉じた状態になる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsError {
    /// プロトコル違反（1002）。
    Protocol(&'static str),
    /// text・close の reason が UTF-8 でない（1007）。
    InvalidUtf8,
    /// 上限を超えた（1009）。
    TooBig,
    /// 閉じた後の送り。
    Closed,
    /// 送りの待ち行列があふれた。
    Backpressure,
    /// 送ろうとした close の code・reason が不正。
    InvalidClose,
}

impl WsError {
    /// 送る close の code。
    pub fn close_code(&self) -> u16 {
        match self {
            WsError::Protocol(_) => 1002,
            WsError::InvalidUtf8 => 1007,
            WsError::TooBig => 1009,
            _ => 1011,
        }
    }
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsError::Protocol(s) => write!(f, "protocol error: {s}"),
            WsError::InvalidUtf8 => f.write_str("invalid UTF-8"),
            WsError::TooBig => f.write_str("message too big"),
            WsError::Closed => f.write_str("connection closed"),
            WsError::Backpressure => f.write_str("send queue full"),
            WsError::InvalidClose => f.write_str("invalid close code or reason"),
        }
    }
}

impl std::error::Error for WsError {}

/// 接続の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    /// こちらが close を送り、相手の close を待っている。
    Closing,
    /// 閉じた（TCP を閉じてよい。サーバーが先に閉じる）。
    Closed,
}

#[derive(Debug)]
struct Fragmented {
    text: bool,
    data: BytesMut,
    frames: usize,
    /// 先頭から UTF-8 として確かめた長さ。
    valid: usize,
}

/// 1 つの WebSocket の接続。
#[derive(Debug)]
pub struct Connection {
    role: Role,
    limits: Limits,
    buf: BytesMut,
    frag: Option<Fragmented>,
    out: VecDeque<Bytes>,
    state: State,
    sent_close: bool,
    received_close: Option<Option<CloseFrame>>,
    error: Option<WsError>,
    rng: u64,
}

/// UTF-8 の逐次の検査。`Ok(valid)` は確かめた長さ（末尾の途中の文字は除く）。
fn check_utf8(data: &[u8], from: usize, complete: bool) -> Result<usize, WsError> {
    match std::str::from_utf8(&data[from..]) {
        Ok(_) => Ok(data.len()),
        Err(e) => {
            if e.error_len().is_some() || complete {
                Err(WsError::InvalidUtf8)
            } else {
                Ok(from + e.valid_up_to())
            }
        }
    }
}

impl Connection {
    pub fn new(role: Role, limits: Limits) -> Connection {
        let mut seed = [0u8; 8];
        let rng = match getrandom::fill(&mut seed) {
            Ok(()) => u64::from_le_bytes(seed) | 1,
            Err(_) => 0x9E37_79B9_7F4A_7C15,
        };
        Connection {
            role,
            limits,
            buf: BytesMut::new(),
            frag: None,
            out: VecDeque::new(),
            state: State::Open,
            sent_close: false,
            received_close: None,
            error: None,
            rng,
        }
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// TCP を閉じてよい（close の手順が終わったか、誤りで閉じた）。
    pub fn is_closed(&self) -> bool {
        self.state == State::Closed
    }

    /// 受信の誤り（あれば）。
    pub fn error(&self) -> Option<&WsError> {
        self.error.as_ref()
    }

    /// 相手の close（受けていれば）。
    pub fn peer_close(&self) -> Option<&Option<CloseFrame>> {
        self.received_close.as_ref()
    }

    /// 受けた byte を入れる。
    pub fn feed(&mut self, data: &[u8]) {
        if self.state != State::Closed {
            self.buf.extend_from_slice(data);
        }
    }

    /// TCP が閉じた（close の手順の前なら異常終了、1006 相当）。
    pub fn feed_eof(&mut self) {
        self.state = State::Closed;
        self.buf.clear();
    }

    /// 送る byte。
    pub fn poll_transmit(&mut self) -> Option<Bytes> {
        self.out.pop_front()
    }

    /// 送る待ちの frame の数。
    pub fn queued(&self) -> usize {
        self.out.len()
    }

    fn next_mask(&mut self) -> [u8; 4] {
        let mut m = [0u8; 4];
        if getrandom::fill(&mut m).is_ok() {
            return m;
        }
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng as u32).to_le_bytes()
    }

    fn queue(&mut self, fin: bool, op: OpCode, payload: &[u8]) {
        let mask = match self.role {
            Role::Client => Some(self.next_mask()),
            Role::Server => None,
        };
        self.out.push_back(encode_frame(fin, op, payload, mask));
    }

    /// 1 つの frame を送る（分割して送る試験・大きな message の逐次送りに使う）。制御 frame の規則は検査する。
    pub fn send_frame(&mut self, fin: bool, op: OpCode, payload: &[u8]) -> Result<(), WsError> {
        if self.state != State::Open {
            return Err(WsError::Closed);
        }
        if op.is_control() && (!fin || payload.len() > 125) {
            return Err(WsError::Protocol("control frame must be final and <= 125 bytes"));
        }
        if op == OpCode::Close {
            return Err(WsError::InvalidClose);
        }
        if self.out.len() >= self.limits.max_queued_frames {
            return Err(WsError::Backpressure);
        }
        self.queue(fin, op, payload);
        Ok(())
    }

    pub fn send_text(&mut self, text: &str) -> Result<(), WsError> {
        self.send_frame(true, OpCode::Text, text.as_bytes())
    }

    pub fn send_binary(&mut self, data: &[u8]) -> Result<(), WsError> {
        self.send_frame(true, OpCode::Binary, data)
    }

    pub fn send_ping(&mut self, data: &[u8]) -> Result<(), WsError> {
        self.send_frame(true, OpCode::Ping, data)
    }

    pub fn send_pong(&mut self, data: &[u8]) -> Result<(), WsError> {
        self.send_frame(true, OpCode::Pong, data)
    }

    /// close を送る（`code` が `None` なら本文なし）。相手の close を受けたら閉じた状態になる。
    pub fn close(&mut self, code: Option<u16>, reason: &str) -> Result<(), WsError> {
        if self.sent_close || self.state == State::Closed {
            return Err(WsError::Closed);
        }
        let payload = match code {
            Some(c) => {
                if !close_code_is_valid(c) || reason.len() > 123 {
                    return Err(WsError::InvalidClose);
                }
                let mut p = c.to_be_bytes().to_vec();
                p.extend_from_slice(reason.as_bytes());
                p
            }
            None if reason.is_empty() => Vec::new(),
            None => return Err(WsError::InvalidClose),
        };
        self.queue(true, OpCode::Close, &payload);
        self.sent_close = true;
        self.state = if self.received_close.is_some() {
            State::Closed
        } else {
            State::Closing
        };
        Ok(())
    }

    fn fail(&mut self, e: WsError) -> WsError {
        if !self.sent_close && self.state != State::Closed {
            let code = e.close_code();
            self.queue(true, OpCode::Close, &code.to_be_bytes());
            self.sent_close = true;
        }
        self.state = State::Closed;
        self.frag = None;
        self.buf.clear();
        self.error = Some(e.clone());
        e
    }

    /// 次の message。足りなければ `Ok(None)`。誤りでは close を送る準備をして `Err`。
    pub fn poll(&mut self) -> Result<Option<Message>, WsError> {
        loop {
            if self.state == State::Closed || self.received_close.is_some() {
                return Ok(None);
            }
            let h = match FrameHeader::parse(&self.buf) {
                Ok(Some(h)) => h,
                Ok(None) => return Ok(None),
                Err(HeaderError::BadLength) => {
                    return Err(self.fail(WsError::Protocol("non-minimal payload length")));
                }
            };
            let op = match h.opcode {
                Ok(op) => op,
                Err(_) => return Err(self.fail(WsError::Protocol("reserved opcode"))),
            };
            if h.rsv != 0 {
                return Err(self.fail(WsError::Protocol("RSV bits without extension")));
            }
            match (self.role, h.mask.is_some()) {
                (Role::Server, false) => return Err(self.fail(WsError::Protocol("client frame not masked"))),
                (Role::Client, true) => return Err(self.fail(WsError::Protocol("server frame masked"))),
                _ => {}
            }
            if op.is_control() {
                if !h.fin {
                    return Err(self.fail(WsError::Protocol("fragmented control frame")));
                }
                if h.payload_len > 125 {
                    return Err(self.fail(WsError::Protocol("control frame too long")));
                }
            } else if h.payload_len > self.limits.max_frame_size as u64
                || h.payload_len > self.limits.max_message_size as u64
            {
                return Err(self.fail(WsError::TooBig));
            }
            let total = h.header_len + h.payload_len as usize;
            if self.buf.len() < total {
                return Ok(None);
            }
            let frame = self.buf.split_to(total);
            let mut payload = BytesMut::from(&frame[h.header_len..]);
            if let Some(m) = h.mask {
                apply_mask(&mut payload, m, 0);
            }
            if let Some(m) = self.on_frame(h.fin, op, payload)? {
                return Ok(Some(m));
            }
        }
    }

    fn on_frame(&mut self, fin: bool, op: OpCode, payload: BytesMut) -> Result<Option<Message>, WsError> {
        // こちらが close を送った後の data は捨てる（相手の close を待つ）
        let discard = self.sent_close;
        match op {
            OpCode::Text | OpCode::Binary => {
                if self.frag.is_some() {
                    return Err(self.fail(WsError::Protocol("new data frame inside fragmented message")));
                }
                let text = op == OpCode::Text;
                if fin {
                    if discard {
                        return Ok(None);
                    }
                    return self.complete(text, payload).map(Some);
                }
                let valid = if text {
                    match check_utf8(&payload, 0, false) {
                        Ok(v) => v,
                        Err(e) => return Err(self.fail(e)),
                    }
                } else {
                    0
                };
                self.frag = Some(Fragmented {
                    text,
                    data: payload,
                    frames: 1,
                    valid,
                });
                Ok(None)
            }
            OpCode::Continuation => {
                let Some(mut f) = self.frag.take() else {
                    return Err(self.fail(WsError::Protocol("continuation without start")));
                };
                f.frames += 1;
                if f.frames > self.limits.max_fragments
                    || f.data.len() + payload.len() > self.limits.max_message_size
                {
                    return Err(self.fail(WsError::TooBig));
                }
                f.data.extend_from_slice(&payload);
                if f.text {
                    match check_utf8(&f.data, f.valid, false) {
                        Ok(v) => f.valid = v,
                        Err(e) => return Err(self.fail(e)),
                    }
                }
                if !fin {
                    self.frag = Some(f);
                    return Ok(None);
                }
                if discard {
                    return Ok(None);
                }
                self.complete(f.text, f.data).map(Some)
            }
            OpCode::Ping => {
                let data = payload.freeze();
                if !self.sent_close && self.out.len() < self.limits.max_queued_frames {
                    self.queue(true, OpCode::Pong, &data);
                }
                Ok(Some(Message::Ping(data)))
            }
            OpCode::Pong => Ok(Some(Message::Pong(payload.freeze()))),
            OpCode::Close => {
                let cf = match payload.len() {
                    0 => None,
                    1 => return Err(self.fail(WsError::Protocol("close payload of 1 byte"))),
                    _ => {
                        let code = u16::from_be_bytes([payload[0], payload[1]]);
                        if !close_code_is_valid(code) {
                            return Err(self.fail(WsError::Protocol("invalid close code")));
                        }
                        let reason = match std::str::from_utf8(&payload[2..]) {
                            Ok(s) => s.to_string(),
                            Err(_) => return Err(self.fail(WsError::InvalidUtf8)),
                        };
                        Some(CloseFrame { code, reason })
                    }
                };
                if !self.sent_close {
                    let echo = cf.as_ref().map(|c| c.code.to_be_bytes().to_vec()).unwrap_or_default();
                    self.queue(true, OpCode::Close, &echo);
                    self.sent_close = true;
                }
                self.received_close = Some(cf.clone());
                self.state = State::Closed;
                self.frag = None;
                Ok(Some(Message::Close(cf)))
            }
        }
    }

    fn complete(&mut self, text: bool, data: BytesMut) -> Result<Message, WsError> {
        if data.len() > self.limits.max_message_size {
            return Err(self.fail(WsError::TooBig));
        }
        if text {
            match String::from_utf8(data.to_vec()) {
                Ok(s) => Ok(Message::Text(s)),
                Err(_) => Err(self.fail(WsError::InvalidUtf8)),
            }
        } else {
            Ok(Message::Binary(data.freeze()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Option<[u8; 4]> = Some([0x12, 0x34, 0x56, 0x78]);

    fn server() -> Connection {
        Connection::new(Role::Server, Limits::default())
    }

    fn frame(fin: bool, op: u8, payload: &[u8]) -> Vec<u8> {
        let op = OpCode::from_u8(op).unwrap();
        encode_frame(fin, op, payload, M).to_vec()
    }

    fn all(c: &mut Connection) -> Result<Vec<Message>, WsError> {
        let mut v = Vec::new();
        while let Some(m) = c.poll()? {
            v.push(m);
        }
        Ok(v)
    }

    fn sent(c: &mut Connection) -> Vec<Vec<u8>> {
        let mut v = Vec::new();
        while let Some(b) = c.poll_transmit() {
            v.push(b.to_vec());
        }
        v
    }

    /// サーバーの送った close の code。
    fn close_code_of(frames: &[Vec<u8>]) -> Option<u16> {
        let f = frames.iter().find(|f| f[0] & 0x0f == 8)?;
        if f[1] & 0x7f >= 2 {
            Some(u16::from_be_bytes([f[2], f[3]]))
        } else {
            Some(0)
        }
    }

    #[test]
    fn text_binary_byte_by_byte() {
        let mut c = server();
        let mut input = frame(true, 1, "こんにちは".as_bytes());
        input.extend(frame(true, 2, &[0, 1, 2, 255]));
        input.extend(frame(true, 1, b""));
        let mut got = Vec::new();
        for b in input {
            c.feed(&[b]);
            got.extend(all(&mut c).unwrap());
        }
        assert_eq!(
            got,
            vec![
                Message::Text("こんにちは".into()),
                Message::Binary(Bytes::from_static(&[0, 1, 2, 255])),
                Message::Text(String::new())
            ]
        );
        // サーバーの送りは mask しない
        c.send_text("hi").unwrap();
        assert_eq!(sent(&mut c), vec![vec![0x81, 0x02, b'h', b'i']]);
    }

    #[test]
    fn mask_rules() {
        let mut c = server();
        c.feed(&encode_frame(true, OpCode::Text, b"x", None));
        assert!(matches!(c.poll(), Err(WsError::Protocol(_))));
        assert_eq!(close_code_of(&sent(&mut c)), Some(1002));
        assert!(c.is_closed());

        let mut cl = Connection::new(Role::Client, Limits::default());
        cl.feed(&encode_frame(true, OpCode::Text, b"x", M));
        assert!(matches!(cl.poll(), Err(WsError::Protocol(_))));
        // クライアントの送りは mask する（frame ごとに別の鍵）
        let mut cl = Connection::new(Role::Client, Limits::default());
        cl.send_text("hello").unwrap();
        cl.send_text("hello").unwrap();
        let f = sent(&mut cl);
        assert_eq!(f[0][1] & 0x80, 0x80);
        let mut s = server();
        for x in &f {
            s.feed(x);
        }
        assert_eq!(all(&mut s).unwrap(), vec![Message::Text("hello".into()), Message::Text("hello".into())]);
    }

    #[test]
    fn fragmentation_with_interleaved_control() {
        let mut c = server();
        let mut input = frame(false, 1, b"fra");
        input.extend(frame(true, 9, b"ping!"));
        input.extend(frame(false, 0, b"gme"));
        input.extend(frame(true, 10, b"unsolicited"));
        input.extend(frame(true, 0, b"nted"));
        c.feed(&input);
        let got = all(&mut c).unwrap();
        assert_eq!(
            got,
            vec![
                Message::Ping(Bytes::from_static(b"ping!")),
                Message::Pong(Bytes::from_static(b"unsolicited")),
                Message::Text("fragmented".into())
            ]
        );
        // ping には同じ payload の pong
        assert_eq!(sent(&mut c), vec![encode_frame(true, OpCode::Pong, b"ping!", None).to_vec()]);
    }

    #[test]
    fn fragmentation_errors() {
        // 始まりのない continuation
        let mut c = server();
        c.feed(&frame(true, 0, b"x"));
        assert!(matches!(c.poll(), Err(WsError::Protocol(_))));
        // 分割の途中の新しい text
        let mut c = server();
        c.feed(&frame(false, 1, b"a"));
        c.feed(&frame(true, 1, b"b"));
        assert!(matches!(c.poll(), Err(WsError::Protocol(_))));
        // 分割した ping
        let mut c = server();
        c.feed(&frame(false, 9, b"a"));
        assert!(matches!(c.poll(), Err(WsError::Protocol(_))));
        // 126 byte の ping
        let mut c = server();
        c.feed(&frame(true, 9, &[0u8; 126]));
        assert!(matches!(c.poll(), Err(WsError::Protocol(_))));
        assert_eq!(close_code_of(&sent(&mut c)), Some(1002));
        // 125 byte の ping はよい
        let mut c = server();
        c.feed(&frame(true, 9, &[7u8; 125]));
        assert!(matches!(c.poll(), Ok(Some(Message::Ping(p))) if p.len() == 125));
        // 予約の opcode・RSV
        for b0 in [0x83u8, 0x8b, 0x91, 0xa1, 0xc1] {
            let mut c = server();
            c.feed(&[b0, 0x80, 0, 0, 0, 0]);
            assert!(matches!(c.poll(), Err(WsError::Protocol(_))), "{b0:x}");
        }
    }

    #[test]
    fn utf8_fail_fast_across_fragments() {
        // 正しい文字が frame の境目で割れている
        let s = "κόσμε🎉".as_bytes();
        for cut in 1..s.len() {
            let mut c = server();
            c.feed(&frame(false, 1, &s[..cut]));
            c.feed(&frame(true, 0, &s[cut..]));
            assert_eq!(all(&mut c).unwrap(), vec![Message::Text("κόσμε🎉".into())], "cut {cut}");
        }
        // 誤りは最後の frame を待たずに 1007
        let mut c = server();
        c.feed(&frame(false, 1, &[0xce, 0xba, 0xe1, 0xbd, 0xb9, 0xcf, 0x83, 0xce, 0xbc, 0xce, 0xb5, 0xf4, 0x90, 0x80, 0x80]));
        assert_eq!(c.poll(), Err(WsError::InvalidUtf8));
        assert_eq!(close_code_of(&sent(&mut c)), Some(1007));
        // 途中で切れたまま終わる
        let mut c = server();
        c.feed(&frame(false, 1, &[0xf0, 0x9f]));
        assert_eq!(c.poll(), Ok(None));
        c.feed(&frame(true, 0, &[0x8e]));
        assert_eq!(c.poll(), Err(WsError::InvalidUtf8));
        // 1 frame の不正な text
        let mut c = server();
        c.feed(&frame(true, 1, &[0xc0, 0xaf]));
        assert_eq!(c.poll(), Err(WsError::InvalidUtf8));
        // binary は検査しない
        let mut c = server();
        c.feed(&frame(true, 2, &[0xc0, 0xaf]));
        assert!(matches!(c.poll(), Ok(Some(Message::Binary(_)))));
    }

    #[test]
    fn close_handshake_server_initiated_and_peer_initiated() {
        // 相手から: 同じ code を返して閉じる
        let mut c = server();
        let mut p = 1000u16.to_be_bytes().to_vec();
        p.extend_from_slice("さようなら".as_bytes());
        c.feed(&frame(true, 8, &p));
        c.feed(&frame(true, 1, b"after close is ignored"));
        assert_eq!(
            all(&mut c).unwrap(),
            vec![Message::Close(Some(CloseFrame {
                code: 1000,
                reason: "さようなら".into()
            }))]
        );
        assert_eq!(sent(&mut c), vec![vec![0x88, 0x02, 0x03, 0xe8]]);
        assert!(c.is_closed());
        assert_eq!(c.send_text("x"), Err(WsError::Closed));
        // 本文の無い close には本文の無い close
        let mut c = server();
        c.feed(&frame(true, 8, b""));
        assert_eq!(all(&mut c).unwrap(), vec![Message::Close(None)]);
        assert_eq!(sent(&mut c), vec![vec![0x88, 0x00]]);
        // こちらから: 相手の close を待つ間の data は捨てる
        let mut c = server();
        c.close(Some(4001), "auth").unwrap();
        assert_eq!(c.state(), State::Closing);
        c.feed(&frame(true, 1, b"late"));
        c.feed(&frame(true, 8, &4001u16.to_be_bytes()));
        assert_eq!(
            all(&mut c).unwrap(),
            vec![Message::Close(Some(CloseFrame { code: 4001, reason: String::new() }))]
        );
        assert!(c.is_closed());
        let s = sent(&mut c);
        assert_eq!(s.len(), 1, "close is not sent twice");
    }

    #[test]
    fn close_errors() {
        for (payload, want) in [
            (vec![0x03u8], 1002u16),
            (1005u16.to_be_bytes().to_vec(), 1002),
            (999u16.to_be_bytes().to_vec(), 1002),
            (5000u16.to_be_bytes().to_vec(), 1002),
            ([0x03, 0xe8, 0xff, 0xfe].to_vec(), 1007),
        ] {
            let mut c = server();
            c.feed(&frame(true, 8, &payload));
            assert!(c.poll().is_err(), "{payload:?}");
            assert_eq!(close_code_of(&sent(&mut c)), Some(want), "{payload:?}");
        }
        let mut c = server();
        assert_eq!(c.close(Some(1005), ""), Err(WsError::InvalidClose));
        assert_eq!(c.close(Some(1000), &"x".repeat(124)), Err(WsError::InvalidClose));
        assert!(c.close(Some(1000), &"x".repeat(123)).is_ok());
    }

    #[test]
    fn limits() {
        let lim = Limits {
            max_frame_size: 1000,
            max_message_size: 2500,
            max_fragments: 5,
            max_queued_frames: 3,
        };
        // 1 frame が大きすぎる: payload を待たずに見出しで 1009
        let mut c = Connection::new(Role::Server, lim);
        let f = frame(true, 2, &[0u8; 1001]);
        c.feed(&f[..8]);
        assert_eq!(c.poll(), Err(WsError::TooBig));
        assert_eq!(close_code_of(&sent(&mut c)), Some(1009));
        // message の合計
        let mut c = Connection::new(Role::Server, lim);
        c.feed(&frame(false, 2, &[0u8; 1000]));
        c.feed(&frame(false, 0, &[0u8; 1000]));
        c.feed(&frame(true, 0, &[0u8; 501]));
        assert_eq!(all(&mut c), Err(WsError::TooBig));
        // frame の数
        let mut c = Connection::new(Role::Server, lim);
        c.feed(&frame(false, 2, b"a"));
        for _ in 0..4 {
            c.feed(&frame(false, 0, b"a"));
        }
        c.feed(&frame(true, 0, b"a"));
        assert_eq!(all(&mut c), Err(WsError::TooBig));
        // 送りの待ち行列
        let mut c = Connection::new(Role::Server, lim);
        for _ in 0..3 {
            c.send_text("x").unwrap();
        }
        assert_eq!(c.send_text("x"), Err(WsError::Backpressure));
    }

    #[test]
    fn client_server_round_trip() {
        let mut cl = Connection::new(Role::Client, Limits::default());
        let mut sv = server();
        // 分割した text（UTF-8 の文字の途中で切る）と ping
        let s = "分割された message";
        let b = s.as_bytes();
        cl.send_frame(false, OpCode::Text, &b[..4]).unwrap();
        cl.send_ping(b"p").unwrap();
        cl.send_frame(false, OpCode::Continuation, &b[4..9]).unwrap();
        cl.send_frame(true, OpCode::Continuation, &b[9..]).unwrap();
        cl.send_binary(&vec![9u8; 70_000]).unwrap();
        for f in sent(&mut cl) {
            sv.feed(&f);
        }
        let got = all(&mut sv).unwrap();
        assert_eq!(got[0], Message::Ping(Bytes::from_static(b"p")));
        assert_eq!(got[1], Message::Text(s.into()));
        assert!(matches!(&got[2], Message::Binary(x) if x.len() == 70_000));
        // サーバーの pong と text がクライアントに届く
        sv.send_text("ok").unwrap();
        for f in sent(&mut sv) {
            cl.feed(&f);
        }
        assert_eq!(
            all(&mut cl).unwrap(),
            vec![Message::Pong(Bytes::from_static(b"p")), Message::Text("ok".into())]
        );
        // close の手順: サーバーから閉じ、クライアントが同じ code を返す
        sv.close(Some(1001), "going away").unwrap();
        for f in sent(&mut sv) {
            cl.feed(&f);
        }
        assert!(matches!(&all(&mut cl).unwrap()[0], Message::Close(Some(c)) if c.code == 1001 && c.reason == "going away"));
        assert!(cl.is_closed());
        for f in sent(&mut cl) {
            sv.feed(&f);
        }
        assert!(matches!(&all(&mut sv).unwrap()[0], Message::Close(Some(c)) if c.code == 1001));
        assert!(sv.is_closed());
    }

    #[test]
    fn eof_and_fuzz() {
        let mut c = server();
        c.feed(&frame(false, 1, b"abc"));
        c.feed_eof();
        assert_eq!(c.poll(), Ok(None));
        assert!(c.is_closed());
        // 乱れた入力でパニック・無限ループしない
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..3000 {
            let mut c = server();
            let mut input = frame(false, 1, b"seed");
            input.extend(frame(true, 9, b"p"));
            for _ in 0..(x % 40) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let i = (x as usize) % input.len();
                input[i] = (x >> 32) as u8;
            }
            for chunk in input.chunks(1 + (x % 7) as usize) {
                c.feed(chunk);
                let mut n = 0;
                while let Ok(Some(_)) = c.poll() {
                    n += 1;
                    assert!(n < 100);
                }
            }
        }
    }
}
