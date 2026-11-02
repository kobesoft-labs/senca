//! HTTP/3 の接続（RFC 9114）と WebTransport over HTTP/3（draft-07）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use bytes::{Buf, Bytes};
use senca_quic::{Connection, DatagramError, Dir, Event, RecvChunk, Side, StreamError, StreamId, varint};

use crate::capsule::{Capsule, CapsuleReader};
use crate::error::{H3Error, H3ErrorCode, h3err, wt_error_to_h3};
use crate::frame::{Frame, FrameReader, Settings, frame_header, stream_ty, ty};
use crate::headers::{Headers, Kind, validate};
use crate::qpack::{Decoded, Decoder, Encoder, QpackError};

/// HTTP/3 の設定と上限（仕様 6.4・9.4）。
#[derive(Debug, Clone)]
pub struct H3Config {
    /// SETTINGS_MAX_FIELD_SECTION_SIZE（ヘッダーの総量の上限、既定 32 KiB）。
    pub max_field_section_size: u64,
    /// QPACK の動的テーブルの容量（相手の encoder に許す量）。
    pub qpack_max_table_capacity: usize,
    /// QPACK の blocked streams の上限。
    pub qpack_blocked_streams: usize,
    /// WebTransport を受け付ける（server）・使う（client）。
    pub enable_webtransport: bool,
    /// SETTINGS_WEBTRANSPORT_MAX_SESSIONS。
    pub webtransport_max_sessions: u64,
    /// HTTP Datagrams（SETTINGS_H3_DATAGRAM）。WebTransport には必須。
    pub enable_datagrams: bool,
    /// 拡張 CONNECT（SETTINGS_ENABLE_CONNECT_PROTOCOL、server）。
    pub enable_connect_protocol: bool,
    /// stream ごとに、アプリケーションが読んでいない body を溜める上限（超えたら QUIC から読まない）。
    pub max_body_buffer: usize,
    /// stream ごとの送信待ちの上限。
    pub max_outbox: usize,
    /// session が確立する前に届いた WebTransport の stream を溜める数。
    pub max_buffered_wt_streams: usize,
    /// session が確立する前に届いた datagram を溜める数。
    pub max_buffered_datagrams: usize,
}

impl Default for H3Config {
    fn default() -> Self {
        Self {
            max_field_section_size: 32 * 1024,
            qpack_max_table_capacity: 4096,
            qpack_blocked_streams: 16,
            enable_webtransport: true,
            webtransport_max_sessions: 16,
            enable_datagrams: true,
            enable_connect_protocol: true,
            max_body_buffer: 1 << 20,
            max_outbox: 1 << 20,
            max_buffered_wt_streams: 16,
            max_buffered_datagrams: 64,
        }
    }
}

/// 交渉した WebTransport over HTTP/3 の版。stream・datagram・capsule の形式は両者で同じ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WtVersion {
    /// draft-ietf-webtrans-http3-02（SETTINGS_ENABLE_WEBTRANSPORT、Chrome の既定）。
    Draft02,
    /// draft-ietf-webtrans-http3-07（SETTINGS_WEBTRANSPORT_MAX_SESSIONS）。
    Draft07,
}

/// WebTransport の stream の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WtStreamKind {
    Bi,
    Uni,
}

/// アプリケーションへのイベント。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum H3Event {
    /// 相手の SETTINGS を受けた。
    PeerSettings(Settings),
    /// server: 要求を受けた。
    Request { stream: StreamId, headers: Headers },
    /// client: 応答（最終応答）を受けた。
    Response { stream: StreamId, headers: Headers },
    /// body が読める（[`H3Connection::recv_body`]）。
    Data { stream: StreamId },
    /// trailer を受けた。
    Trailers { stream: StreamId, headers: Headers },
    /// 相手が送信を終えた（FIN）。
    Finished { stream: StreamId },
    /// 相手が stream をリセットした。
    StreamReset { stream: StreamId, code: u64 },
    /// GOAWAY を受けた。
    GoAway { id: u64 },
    /// server: WebTransport の拡張 CONNECT を受けた。[`H3Connection::wt_accept`] か `wt_reject` で応える。
    WebTransportRequest { session: StreamId, headers: Headers },
    /// client: WebTransport の session が確立した（2xx）。
    WebTransportEstablished { session: StreamId, headers: Headers },
    /// client: WebTransport の session が拒否された。
    WebTransportRejected { session: StreamId, status: u16 },
    /// 相手が WebTransport の stream を開いた。
    WebTransportStream {
        session: StreamId,
        stream: StreamId,
        kind: WtStreamKind,
    },
    /// WebTransport の stream が読める（[`H3Connection::wt_recv`]）。
    WebTransportReadable { session: StreamId, stream: StreamId },
    /// WebTransport の stream に書ける。
    WebTransportWritable { session: StreamId, stream: StreamId },
    /// 相手が WebTransport の stream の受信を止めた（STOP_SENDING）。
    WebTransportStopped {
        session: StreamId,
        stream: StreamId,
        code: Option<u32>,
    },
    WebTransportDatagram { session: StreamId, data: Bytes },
    WebTransportDraining { session: StreamId },
    /// session が終わった（CLOSE_WEBTRANSPORT_SESSION・FIN・リセット）。
    WebTransportClosed { session: StreamId, code: u32, reason: String },
    /// WebTransport 以外の CONNECT の HTTP Datagram。
    HttpDatagram { stream: StreamId, data: Bytes },
    /// 接続が終わった。
    ConnectionClosed(senca_quic::ConnectionError),
}

/// API の誤り。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    Stream(StreamError),
    /// 今は開けない（stream 数の上限・相手の SETTINGS 待ち）。
    Blocked,
    /// この操作はできない。
    Invalid(&'static str),
    Datagram(DatagramError),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for ApiError {}

impl From<StreamError> for ApiError {
    fn from(e: StreamError) -> Self {
        Self::Stream(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RState {
    AwaitHeaders,
    Body,
    Trailers,
}

#[derive(Debug)]
struct ReqStream {
    reader: FrameReader,
    state: RState,
    body: VecDeque<Bytes>,
    body_len: usize,
    data_signaled: bool,
    fin: bool,
    fin_signaled: bool,
    /// QPACK で blocked の HEADERS。
    blocked: Option<Bytes>,
    /// WebTransport の CONNECT の stream（確立後は DATA の中身が capsule）。
    wt_session: bool,
    is_connect: bool,
}

impl ReqStream {
    fn new(max: usize) -> Self {
        Self {
            reader: FrameReader::new(max),
            state: RState::AwaitHeaders,
            body: VecDeque::new(),
            body_len: 0,
            data_signaled: false,
            fin: false,
            fin_signaled: false,
            blocked: None,
            wt_session: false,
            is_connect: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WtState {
    /// server: 要求を受けた（未応答）。client: 送った（応答待ち）。
    Pending,
    Established,
}

#[derive(Debug)]
struct WtSession {
    state: WtState,
    streams: HashSet<StreamId>,
    capsules: CapsuleReader,
    close_sent: bool,
}

#[derive(Debug)]
struct WtData {
    session: StreamId,
    /// 見出しの後ろで既に QUIC から読んだ分。
    leftover: Bytes,
    /// 見出しを読んだときに FIN まで読んでいた。
    leftover_fin: bool,
    announced: bool,
    kind: WtStreamKind,
}

#[derive(Debug, Default)]
struct Outbox {
    queue: VecDeque<Bytes>,
    size: usize,
    fin: bool,
}

/// HTTP/3 の接続（client・server）。QUIC 接続は呼び出し側が持ち、各メソッドに渡す。
#[derive(Debug)]
pub struct H3Connection {
    side: Side,
    config: H3Config,
    started: bool,
    control_out: Option<StreamId>,
    encoder_out: Option<StreamId>,
    decoder_out: Option<StreamId>,
    peer_control: Option<StreamId>,
    peer_encoder: Option<StreamId>,
    peer_decoder: Option<StreamId>,
    peer_control_reader: FrameReader,
    peer_settings: Option<Settings>,
    encoder: Encoder,
    decoder: Decoder,
    peer_decoder_buf: Vec<u8>,
    /// 種別を読んでいる途中の単方向 stream。
    uni_pending: HashMap<StreamId, Vec<u8>>,
    /// 先頭を読んでいる途中の相手の双方向 stream（0x41 か要求か）。
    bidi_pending: HashMap<StreamId, Vec<u8>>,
    streams: HashMap<StreamId, ReqStream>,
    wt_sessions: HashMap<StreamId, WtSession>,
    wt_streams: HashMap<StreamId, WtData>,
    buffered_dgrams: VecDeque<(StreamId, Bytes)>,
    outbox: HashMap<StreamId, Outbox>,
    /// server が受けた最大の要求 stream。
    max_request: Option<u64>,
    goaway_sent: Option<u64>,
    goaway_recv: Option<u64>,
    /// 相手の SETTINGS を待つ処理（client の WebTransport 要求・server の WebTransport 受付）。
    deferred_wt: Vec<(StreamId, Headers)>,
    events: VecDeque<H3Event>,
    closed: bool,
}

const READ_CHUNK: usize = 64 * 1024;

impl H3Connection {
    pub fn new(side: Side, config: H3Config) -> Self {
        let decoder = Decoder::new(
            config.qpack_max_table_capacity,
            config.qpack_blocked_streams,
            config.max_field_section_size as usize,
        );
        Self {
            side,
            peer_control_reader: FrameReader::new(config.max_field_section_size as usize),
            config,
            started: false,
            control_out: None,
            encoder_out: None,
            decoder_out: None,
            peer_control: None,
            peer_encoder: None,
            peer_decoder: None,
            peer_settings: None,
            encoder: Encoder,
            decoder,
            peer_decoder_buf: Vec::new(),
            uni_pending: HashMap::new(),
            bidi_pending: HashMap::new(),
            streams: HashMap::new(),
            wt_sessions: HashMap::new(),
            wt_streams: HashMap::new(),
            buffered_dgrams: VecDeque::new(),
            outbox: HashMap::new(),
            max_request: None,
            goaway_sent: None,
            goaway_recv: None,
            deferred_wt: Vec::new(),
            events: VecDeque::new(),
            closed: false,
        }
    }

    pub fn peer_settings(&self) -> Option<&Settings> {
        self.peer_settings.as_ref()
    }

    pub fn poll_event(&mut self) -> Option<H3Event> {
        self.events.pop_front()
    }

    fn local_settings(&self) -> Settings {
        Settings {
            qpack_max_table_capacity: self.config.qpack_max_table_capacity as u64,
            max_field_section_size: Some(self.config.max_field_section_size),
            qpack_blocked_streams: self.config.qpack_blocked_streams as u64,
            enable_connect_protocol: self.side == Side::Server && self.config.enable_connect_protocol,
            h3_datagram: self.config.enable_datagrams,
            webtransport_max_sessions: if self.config.enable_webtransport {
                self.config.webtransport_max_sessions
            } else {
                0
            },
            webtransport_draft02: self.config.enable_webtransport,
        }
    }

    /// この接続で使う WebTransport の版（両者が広告したうち新しい方、draft-07 > draft-02）。
    pub fn wt_version(&self) -> Option<WtVersion> {
        if !self.config.enable_webtransport {
            return None;
        }
        let p = self.peer_settings.as_ref()?;
        if !p.h3_datagram {
            return None;
        }
        if p.webtransport_max_sessions > 0 {
            Some(WtVersion::Draft07)
        } else if p.webtransport_draft02 {
            Some(WtVersion::Draft02)
        } else {
            None
        }
    }

    fn start(&mut self, quic: &mut Connection) {
        if self.started || quic.peer_transport_parameters().is_none() || quic.streams_available(Dir::Uni) < 3 {
            return;
        }
        self.started = true;
        let mut open = |t: u64, extra: Vec<u8>| -> Option<StreamId> {
            let id = quic.open_stream(Dir::Uni)?;
            let _ = quic.set_stream_priority(id, 1000);
            let mut v = Vec::new();
            varint::write(&mut v, t);
            v.extend_from_slice(&extra);
            let _ = quic.stream_send(id, &v);
            Some(id)
        };
        let settings = self.local_settings().encode();
        self.control_out = open(stream_ty::CONTROL, settings);
        self.encoder_out = open(stream_ty::QPACK_ENCODER, Vec::new());
        self.decoder_out = open(stream_ty::QPACK_DECODER, Vec::new());
    }

    /// QUIC のイベントを取り込み、送信待ちを流す。アプリケーションの操作の後にも呼ぶ。
    pub fn process(&mut self, quic: &mut Connection, now: Instant) {
        if self.closed {
            while quic.poll_event().is_some() {}
            return;
        }
        self.start(quic);
        while let Some(ev) = quic.poll_event() {
            let r = match ev {
                Event::StreamOpened { id } | Event::StreamReadable { id } => self.on_readable(quic, id),
                Event::StreamWritable { id } => {
                    self.flush(quic, id);
                    if let Some(w) = self.wt_streams.get(&id) {
                        self.events.push_back(H3Event::WebTransportWritable {
                            session: w.session,
                            stream: id,
                        });
                    }
                    Ok(())
                }
                Event::StreamStopped { id, code } => {
                    if let Some(w) = self.wt_streams.get(&id) {
                        self.events.push_back(H3Event::WebTransportStopped {
                            session: w.session,
                            stream: id,
                            code: crate::error::h3_to_wt_error(code),
                        });
                    }
                    self.outbox.remove(&id);
                    Ok(())
                }
                Event::DatagramReceived => self.on_datagrams(quic),
                Event::HandshakeCompleted => {
                    self.start(quic);
                    Ok(())
                }
                Event::ConnectionLost(e) => {
                    self.closed = true;
                    self.events.push_back(H3Event::ConnectionClosed(e));
                    return;
                }
                _ => Ok(()),
            };
            if let Err(e) = r {
                self.fail(quic, now, e);
                return;
            }
        }
        self.start(quic);
        let ids: Vec<StreamId> = self.outbox.keys().copied().collect();
        for id in ids {
            self.flush(quic, id);
        }
        let ins = self.decoder.take_instructions();
        if !ins.is_empty()
            && let Some(d) = self.decoder_out
        {
            let _ = quic.stream_send(d, &ins);
        }
    }

    fn fail(&mut self, quic: &mut Connection, now: Instant, e: H3Error) {
        self.closed = true;
        quic.close(now, e.code.0, e.reason.as_bytes());
    }

    // ------------------------------------------------------------------ 受信

    fn on_readable(&mut self, quic: &mut Connection, id: StreamId) -> Result<(), H3Error> {
        let peer_initiated = id.initiator() != self.side;
        if let Some(w) = self.wt_streams.get(&id) {
            if self.wt_sessions.get(&w.session).is_some_and(|s| s.state == WtState::Established) {
                self.events.push_back(H3Event::WebTransportReadable {
                    session: w.session,
                    stream: id,
                });
            }
            return Ok(());
        }
        if id.dir() == Dir::Uni {
            if !peer_initiated {
                return Ok(());
            }
            return self.on_uni(quic, id);
        }
        if peer_initiated && !self.streams.contains_key(&id) {
            return self.on_new_bidi(quic, id);
        }
        self.read_request_stream(quic, id)
    }

    fn on_uni(&mut self, quic: &mut Connection, id: StreamId) -> Result<(), H3Error> {
        if Some(id) == self.peer_control || Some(id) == self.peer_encoder || Some(id) == self.peer_decoder {
            return self.read_critical(quic, id);
        }
        // 種別（と WebTransport の session ID）を読む
        let mut buf = self.uni_pending.remove(&id).unwrap_or_default();
        loop {
            let ch = match quic.stream_recv(id, 16) {
                Ok(c) => c,
                Err(_) => return Ok(()),
            };
            buf.extend_from_slice(&ch.data);
            let Some((t, n)) = varint::read(&buf) else {
                if ch.fin {
                    return Ok(());
                }
                if ch.data.is_empty() {
                    self.uni_pending.insert(id, buf);
                    return Ok(());
                }
                continue;
            };
            let rest = Bytes::copy_from_slice(&buf[n..]);
            match t {
                stream_ty::CONTROL | stream_ty::QPACK_ENCODER | stream_ty::QPACK_DECODER => {
                    let slot = match t {
                        stream_ty::CONTROL => &mut self.peer_control,
                        stream_ty::QPACK_ENCODER => &mut self.peer_encoder,
                        _ => &mut self.peer_decoder,
                    };
                    if slot.is_some() {
                        return Err(h3err!(STREAM_CREATION_ERROR, "重要な stream の重複 (種別 {t})"));
                    }
                    *slot = Some(id);
                    if ch.fin {
                        return Err(h3err!(CLOSED_CRITICAL_STREAM, "重要な stream が閉じた"));
                    }
                    self.critical_data(quic, id, &rest)?;
                    return self.read_critical(quic, id);
                }
                stream_ty::PUSH => {
                    // MAX_PUSH_ID を送らないので push は受けない
                    return Err(if self.side == Side::Client {
                        h3err!(ID_ERROR, "MAX_PUSH_ID を送っていないのに push stream")
                    } else {
                        h3err!(STREAM_CREATION_ERROR, "client からの push stream")
                    });
                }
                stream_ty::WEBTRANSPORT => {
                    let Some((sid, m)) = varint::read(&rest) else {
                        if ch.fin || buf.len() > 16 {
                            return Ok(());
                        }
                        if ch.data.is_empty() {
                            self.uni_pending.insert(id, buf);
                            return Ok(());
                        }
                        continue;
                    };
                    let leftover = rest.slice(m..);
                    return self.on_wt_stream(quic, id, StreamId(sid), WtStreamKind::Uni, leftover, ch.fin);
                }
                _ => {
                    // 未知・grease の stream は読まない（RFC 9114 6.2）
                    let _ = quic.stream_stop(id, H3ErrorCode::STREAM_CREATION_ERROR.0);
                    return Ok(());
                }
            }
        }
    }

    fn read_critical(&mut self, quic: &mut Connection, id: StreamId) -> Result<(), H3Error> {
        loop {
            match quic.stream_recv(id, READ_CHUNK) {
                Ok(RecvChunk { data, fin }) => {
                    if !data.is_empty() {
                        self.critical_data(quic, id, &data)?;
                    }
                    if fin {
                        return Err(h3err!(CLOSED_CRITICAL_STREAM, "重要な stream が閉じた"));
                    }
                    if data.is_empty() {
                        return Ok(());
                    }
                }
                Err(_) => return Err(h3err!(CLOSED_CRITICAL_STREAM, "重要な stream がリセットされた")),
            }
        }
    }

    fn critical_data(&mut self, quic: &mut Connection, id: StreamId, data: &[u8]) -> Result<(), H3Error> {
        if Some(id) == self.peer_control {
            self.peer_control_reader.push(data);
            while let Some(f) = self.peer_control_reader.next()? {
                self.on_control_frame(quic, f)?;
            }
        } else if Some(id) == self.peer_encoder {
            self.decoder.on_encoder_stream(data).map_err(qpack_err)?;
            for s in self.decoder.unblocked() {
                let sid = StreamId(s);
                if let Some(rs) = self.streams.get_mut(&sid)
                    && let Some(b) = rs.blocked.take()
                {
                    self.on_headers(quic, sid, b)?;
                    self.read_request_stream(quic, sid)?;
                }
            }
        } else if Some(id) == self.peer_decoder {
            Decoder::on_decoder_stream(&mut self.peer_decoder_buf, data).map_err(qpack_err)?;
        }
        Ok(())
    }

    fn on_control_frame(&mut self, quic: &mut Connection, f: Frame) -> Result<(), H3Error> {
        if self.peer_settings.is_none() && !matches!(f, Frame::Settings(_)) {
            return Err(h3err!(MISSING_SETTINGS, "control stream の最初が SETTINGS でない"));
        }
        match f {
            Frame::Settings(p) => {
                if self.peer_settings.is_some() {
                    return Err(h3err!(FRAME_UNEXPECTED, "二度目の SETTINGS"));
                }
                let s = Settings::decode(&p)?;
                // WebTransport draft-07 は拡張 CONNECT と RFC の HTTP Datagram を前提にする
                if self.side == Side::Client
                    && s.webtransport_max_sessions > 0
                    && !s.webtransport_draft02
                    && (!s.enable_connect_protocol || !s.h3_datagram)
                {
                    return Err(h3err!(SETTINGS_ERROR, "WebTransport に拡張 CONNECT・H3_DATAGRAM がない"));
                }
                self.peer_settings = Some(s.clone());
                self.events.push_back(H3Event::PeerSettings(s));
                // SETTINGS 待ちだった WebTransport を進める
                let deferred = std::mem::take(&mut self.deferred_wt);
                for (sid, h) in deferred {
                    match self.side {
                        Side::Server => self.wt_request(quic, sid, h)?,
                        Side::Client => {
                            let h = self.with_wt_draft_header(h);
                            if self.wt_version().is_none() {
                                self.wt_sessions.remove(&sid);
                                self.events.push_back(H3Event::WebTransportRejected { session: sid, status: 0 });
                                let _ = quic.stream_reset(sid, H3ErrorCode::REQUEST_CANCELLED.0);
                                continue;
                            }
                            let _ = self.send_headers(quic, sid, &h, false);
                        }
                    }
                }
            }
            Frame::GoAway(id) => {
                if self.side == Side::Client && id % 4 != 0 {
                    return Err(h3err!(ID_ERROR, "GOAWAY の ID が client の双方向 stream でない"));
                }
                if self.goaway_recv.is_some_and(|g| id > g) {
                    return Err(h3err!(ID_ERROR, "GOAWAY の ID が増えた"));
                }
                self.goaway_recv = Some(id);
                self.events.push_back(H3Event::GoAway { id });
            }
            Frame::MaxPushId(_) => {
                if self.side == Side::Client {
                    return Err(h3err!(FRAME_UNEXPECTED, "server からの MAX_PUSH_ID"));
                }
            }
            Frame::CancelPush(_) => {}
            Frame::Data(_) | Frame::Headers(_) | Frame::PushPromise => {
                return Err(h3err!(FRAME_UNEXPECTED, "control stream に DATA・HEADERS・PUSH_PROMISE"));
            }
        }
        Ok(())
    }

    fn on_new_bidi(&mut self, quic: &mut Connection, id: StreamId) -> Result<(), H3Error> {
        let mut buf = self.bidi_pending.remove(&id).unwrap_or_default();
        loop {
            let ch = match quic.stream_recv(id, if buf.is_empty() { 1 } else { 16 }) {
                Ok(c) => c,
                Err(_) => return Ok(()),
            };
            buf.extend_from_slice(&ch.data);
            let Some((t, n)) = varint::read(&buf) else {
                if ch.fin {
                    return Ok(());
                }
                if ch.data.is_empty() {
                    self.bidi_pending.insert(id, buf);
                    return Ok(());
                }
                continue;
            };
            if t == ty::WEBTRANSPORT_STREAM {
                let Some((sid, m)) = varint::read(&buf[n..]) else {
                    if ch.fin || buf.len() > 16 {
                        return Ok(());
                    }
                    if ch.data.is_empty() {
                        self.bidi_pending.insert(id, buf);
                        return Ok(());
                    }
                    continue;
                };
                let leftover = Bytes::copy_from_slice(&buf[n + m..]);
                return self.on_wt_stream(quic, id, StreamId(sid), WtStreamKind::Bi, leftover, ch.fin);
            }
            if self.side == Side::Client {
                return Err(h3err!(STREAM_CREATION_ERROR, "server が開いた双方向 stream が WebTransport でない"));
            }
            // 要求 stream
            if let Some(g) = self.goaway_sent
                && id.0 >= g
            {
                let _ = quic.stream_stop(id, H3ErrorCode::REQUEST_REJECTED.0);
                let _ = quic.stream_reset(id, H3ErrorCode::REQUEST_REJECTED.0);
                return Ok(());
            }
            self.max_request = Some(self.max_request.map_or(id.0, |m| m.max(id.0)));
            let mut rs = ReqStream::new(self.config.max_field_section_size as usize);
            rs.reader.push(&buf);
            if ch.fin {
                rs.fin = true;
            }
            self.streams.insert(id, rs);
            return self.read_request_stream(quic, id);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_wt_stream(
        &mut self,
        quic: &mut Connection,
        id: StreamId,
        session: StreamId,
        kind: WtStreamKind,
        leftover: Bytes,
        leftover_fin: bool,
    ) -> Result<(), H3Error> {
        if session.dir() != Dir::Bi || session.initiator() != Side::Client {
            return Err(h3err!(ID_ERROR, "WebTransport の session ID が client の双方向 stream でない"));
        }
        if !self.config.enable_webtransport {
            let _ = quic.stream_stop(id, H3ErrorCode::STREAM_CREATION_ERROR.0);
            return Ok(());
        }
        let established = self.wt_sessions.get(&session).is_some_and(|s| s.state == WtState::Established);
        if !established {
            // session の確立前（並べ替え）: 少しだけ溜める
            let buffered = self.wt_streams.values().filter(|w| !w.announced).count();
            if buffered >= self.config.max_buffered_wt_streams {
                let code = H3ErrorCode::WEBTRANSPORT_BUFFERED_STREAM_REJECTED.0;
                let _ = quic.stream_stop(id, code);
                if kind == WtStreamKind::Bi {
                    let _ = quic.stream_reset(id, code);
                }
                return Ok(());
            }
        }
        self.wt_streams.insert(
            id,
            WtData {
                session,
                leftover,
                leftover_fin,
                announced: false,
                kind,
            },
        );
        if established {
            self.announce_wt_stream(id);
        }
        Ok(())
    }

    fn announce_wt_stream(&mut self, id: StreamId) {
        let Some(w) = self.wt_streams.get_mut(&id) else { return };
        if w.announced {
            return;
        }
        w.announced = true;
        let session = w.session;
        let kind = w.kind;
        if let Some(s) = self.wt_sessions.get_mut(&session) {
            s.streams.insert(id);
        }
        self.events.push_back(H3Event::WebTransportStream { session, stream: id, kind });
        self.events.push_back(H3Event::WebTransportReadable { session, stream: id });
    }

    fn read_request_stream(&mut self, quic: &mut Connection, id: StreamId) -> Result<(), H3Error> {
        loop {
            let Some(rs) = self.streams.get_mut(&id) else { return Ok(()) };
            if rs.blocked.is_some() {
                return Ok(());
            }
            // 溜まっているフレームを処理
            while let Some(f) = {
                let rs = self.streams.get_mut(&id).ok_or_else(|| h3err!(INTERNAL_ERROR, "stream"))?;
                if rs.blocked.is_some() { None } else { rs.reader.next()? }
            } {
                self.on_request_frame(quic, id, f)?;
                if !self.streams.contains_key(&id) {
                    return Ok(());
                }
            }
            let Some(rs) = self.streams.get_mut(&id) else { return Ok(()) };
            if rs.fin {
                if !rs.fin_signaled && rs.blocked.is_none() {
                    rs.fin_signaled = true;
                    return self.on_request_fin(quic, id);
                }
                return Ok(());
            }
            if rs.body_len >= self.config.max_body_buffer {
                return Ok(()); // アプリケーションが読むまで QUIC から読まない
            }
            match quic.stream_recv(id, READ_CHUNK) {
                Ok(RecvChunk { data, fin }) => {
                    if data.is_empty() && !fin {
                        return Ok(());
                    }
                    rs.reader.push(&data);
                    if fin {
                        rs.fin = true;
                    }
                }
                Err(StreamError::Reset(code)) => {
                    self.on_request_reset(quic, id, code);
                    return Ok(());
                }
                Err(_) => return Ok(()),
            }
        }
    }

    fn on_request_reset(&mut self, quic: &mut Connection, id: StreamId, code: u64) {
        self.decoder.cancel_stream(id.0);
        if let Some(rs) = self.streams.remove(&id)
            && rs.wt_session
        {
            self.close_session_state(quic, id, 0, String::new());
            return;
        }
        self.events.push_back(H3Event::StreamReset { stream: id, code });
    }

    fn on_request_fin(&mut self, quic: &mut Connection, id: StreamId) -> Result<(), H3Error> {
        let Some(rs) = self.streams.get(&id) else { return Ok(()) };
        if rs.reader.mid_frame() {
            return self.stream_error(quic, id, H3ErrorCode::FRAME_ERROR);
        }
        if rs.state == RState::AwaitHeaders {
            return self.stream_error(quic, id, H3ErrorCode::REQUEST_INCOMPLETE);
        }
        if rs.wt_session {
            // FIN は CLOSE_WEBTRANSPORT_SESSION(0) と同じ（draft-07 5）
            self.close_session_state(quic, id, 0, String::new());
            let _ = quic.stream_finish(id);
            return Ok(());
        }
        self.events.push_back(H3Event::Finished { stream: id });
        Ok(())
    }

    /// stream エラー: 読み書きを打ち切る（RFC 9114 8）。
    fn stream_error(&mut self, quic: &mut Connection, id: StreamId, code: H3ErrorCode) -> Result<(), H3Error> {
        let _ = quic.stream_stop(id, code.0);
        let _ = quic.stream_reset(id, code.0);
        self.decoder.cancel_stream(id.0);
        self.streams.remove(&id);
        self.outbox.remove(&id);
        if self.wt_sessions.contains_key(&id) {
            self.close_session_state(quic, id, 0, String::new());
        }
        Ok(())
    }

    fn on_request_frame(&mut self, quic: &mut Connection, id: StreamId, f: Frame) -> Result<(), H3Error> {
        let state = self.streams.get(&id).map(|r| r.state).ok_or_else(|| h3err!(INTERNAL_ERROR, "stream"))?;
        match f {
            Frame::Headers(p) => match state {
                RState::AwaitHeaders | RState::Body => self.on_headers(quic, id, p),
                RState::Trailers => Err(h3err!(FRAME_UNEXPECTED, "trailer の後の HEADERS")),
            },
            Frame::Data(d) => {
                if state != RState::Body {
                    return Err(h3err!(FRAME_UNEXPECTED, "HEADERS の前・trailer の後の DATA"));
                }
                let rs = self.streams.get_mut(&id).ok_or_else(|| h3err!(INTERNAL_ERROR, "stream"))?;
                if rs.wt_session {
                    let caps = match self.wt_sessions.get_mut(&id) {
                        Some(s) => s.capsules.push(&d),
                        None => Ok(Vec::new()),
                    };
                    let caps = match caps {
                        Ok(c) => c,
                        Err(_) => return self.stream_error(quic, id, H3ErrorCode::MESSAGE_ERROR),
                    };
                    for c in caps {
                        match c {
                            Capsule::CloseWebTransportSession { code, reason } => {
                                self.close_session_state(quic, id, code, reason);
                                let _ = quic.stream_finish(id);
                            }
                            Capsule::DrainWebTransportSession => {
                                self.events.push_back(H3Event::WebTransportDraining { session: id })
                            }
                            Capsule::Unknown { .. } => {}
                        }
                    }
                    return Ok(());
                }
                rs.body_len += d.len();
                rs.body.push_back(d);
                if !rs.data_signaled {
                    rs.data_signaled = true;
                    self.events.push_back(H3Event::Data { stream: id });
                }
                Ok(())
            }
            Frame::PushPromise => Err(h3err!(ID_ERROR, "MAX_PUSH_ID を送っていないのに PUSH_PROMISE")),
            Frame::Settings(_) | Frame::GoAway(_) | Frame::MaxPushId(_) | Frame::CancelPush(_) => {
                Err(h3err!(FRAME_UNEXPECTED, "要求 stream に制御フレーム"))
            }
        }
    }

    fn on_headers(&mut self, quic: &mut Connection, id: StreamId, block: Bytes) -> Result<(), H3Error> {
        let fields = match self.decoder.decode(id.0, &block) {
            Ok(Decoded::Done(f)) => f,
            Ok(Decoded::Blocked) => {
                if let Some(rs) = self.streams.get_mut(&id) {
                    rs.blocked = Some(block);
                }
                return Ok(());
            }
            Err(QpackError::TooLarge) => return self.stream_error(quic, id, H3ErrorCode::MESSAGE_ERROR),
            Err(e) => return Err(qpack_err(e)),
        };
        let Some(rs) = self.streams.get_mut(&id) else { return Ok(()) };
        let state = rs.state;
        if state == RState::Body {
            // trailer
            if validate(&fields, Kind::Trailers).is_err() {
                return self.stream_error(quic, id, H3ErrorCode::MESSAGE_ERROR);
            }
            rs.state = RState::Trailers;
            self.events.push_back(H3Event::Trailers { stream: id, headers: fields });
            return Ok(());
        }
        match self.side {
            Side::Server => {
                if validate(&fields, Kind::Request).is_err() {
                    return self.stream_error(quic, id, H3ErrorCode::MESSAGE_ERROR);
                }
                rs.state = RState::Body;
                let is_connect = fields.get(":method") == Some(b"CONNECT");
                rs.is_connect = is_connect;
                if is_connect && fields.get(":protocol").is_some() && !self.config.enable_connect_protocol {
                    return self.stream_error(quic, id, H3ErrorCode::MESSAGE_ERROR);
                }
                if is_connect && fields.get(":protocol") == Some(b"webtransport") {
                    if self.peer_settings.is_none() {
                        // client の SETTINGS を待つ（draft-07 3.1）
                        self.deferred_wt.push((id, fields));
                        return Ok(());
                    }
                    return self.wt_request(quic, id, fields);
                }
                self.events.push_back(H3Event::Request { stream: id, headers: fields });
            }
            Side::Client => {
                if validate(&fields, Kind::Response).is_err() {
                    return self.stream_error(quic, id, H3ErrorCode::MESSAGE_ERROR);
                }
                let status = fields.status().unwrap_or(0);
                if (100..200).contains(&status) {
                    if status == 101 {
                        return self.stream_error(quic, id, H3ErrorCode::MESSAGE_ERROR);
                    }
                    return Ok(()); // 中間応答は読み捨てる
                }
                rs.state = RState::Body;
                if let Some(s) = self.wt_sessions.get_mut(&id) {
                    if (200..300).contains(&status) {
                        s.state = WtState::Established;
                        rs.wt_session = true;
                        self.events.push_back(H3Event::WebTransportEstablished {
                            session: id,
                            headers: fields,
                        });
                        self.release_buffered(id);
                    } else {
                        self.wt_sessions.remove(&id);
                        self.events.push_back(H3Event::WebTransportRejected { session: id, status });
                    }
                    return Ok(());
                }
                self.events.push_back(H3Event::Response { stream: id, headers: fields });
            }
        }
        Ok(())
    }

    fn wt_request(&mut self, quic: &mut Connection, id: StreamId, fields: Headers) -> Result<(), H3Error> {
        let ok = self.wt_version().is_some() && quic.max_datagram_size().is_some();
        if !ok {
            let h = Headers::new().with(":status", "400");
            let _ = self.send_headers(quic, id, &h, true);
            self.streams.remove(&id);
            return Ok(());
        }
        let active = self.wt_sessions.len() as u64;
        if active >= self.config.webtransport_max_sessions {
            let h = Headers::new().with(":status", "429");
            let _ = self.send_headers(quic, id, &h, true);
            self.streams.remove(&id);
            return Ok(());
        }
        self.wt_sessions.insert(
            id,
            WtSession {
                state: WtState::Pending,
                streams: HashSet::new(),
                capsules: CapsuleReader::default(),
                close_sent: false,
            },
        );
        // 以後の DATA は capsule として読む（受付前に届いたものも）
        if let Some(rs) = self.streams.get_mut(&id) {
            rs.wt_session = true;
        }
        self.events.push_back(H3Event::WebTransportRequest {
            session: id,
            headers: fields,
        });
        Ok(())
    }

    fn release_buffered(&mut self, session: StreamId) {
        let ids: Vec<StreamId> = self
            .wt_streams
            .iter()
            .filter(|(_, w)| w.session == session && !w.announced)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.announce_wt_stream(id);
        }
        let mut keep = VecDeque::new();
        while let Some((s, d)) = self.buffered_dgrams.pop_front() {
            if s == session {
                self.events.push_back(H3Event::WebTransportDatagram { session, data: d });
            } else {
                keep.push_back((s, d));
            }
        }
        self.buffered_dgrams = keep;
    }

    fn on_datagrams(&mut self, quic: &mut Connection) -> Result<(), H3Error> {
        while let Some(d) = quic.recv_datagram() {
            if !self.config.enable_datagrams {
                return Err(h3err!(DATAGRAM_ERROR, "H3_DATAGRAM を広告していないのに datagram"));
            }
            let Some((qid, n)) = varint::read(&d) else {
                return Err(h3err!(DATAGRAM_ERROR, "Quarter Stream ID がない"));
            };
            if qid >= 1 << 60 {
                return Err(h3err!(DATAGRAM_ERROR, "Quarter Stream ID が大きすぎる"));
            }
            let sid = StreamId(qid * 4);
            let payload = d.slice(n..);
            match self.wt_sessions.get(&sid).map(|s| s.state) {
                Some(WtState::Established) => self.events.push_back(H3Event::WebTransportDatagram {
                    session: sid,
                    data: payload,
                }),
                Some(WtState::Pending) => {
                    if self.buffered_dgrams.len() < self.config.max_buffered_datagrams {
                        self.buffered_dgrams.push_back((sid, payload));
                    }
                }
                None => {
                    if self.streams.get(&sid).is_some_and(|r| r.is_connect) {
                        self.events.push_back(H3Event::HttpDatagram {
                            stream: sid,
                            data: payload,
                        });
                    }
                    // 知らない stream の datagram は捨てる（RFC 9297 2.1）
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ 送信

    fn queue(&mut self, id: StreamId, data: Bytes) {
        let o = self.outbox.entry(id).or_default();
        o.size += data.len();
        o.queue.push_back(data);
    }

    fn flush(&mut self, quic: &mut Connection, id: StreamId) {
        let Some(o) = self.outbox.get_mut(&id) else { return };
        while let Some(front) = o.queue.front_mut() {
            match quic.stream_send_bytes(id, front.clone()) {
                Ok(n) if n == front.len() => {
                    o.size -= n;
                    o.queue.pop_front();
                }
                Ok(n) => {
                    o.size -= n;
                    front.advance(n);
                    return;
                }
                Err(_) => {
                    self.outbox.remove(&id);
                    return;
                }
            }
        }
        if o.fin {
            let _ = quic.stream_finish(id);
            self.outbox.remove(&id);
        }
    }

    fn send_headers(&mut self, quic: &mut Connection, id: StreamId, h: &Headers, fin: bool) -> Result<(), ApiError> {
        let block = self.encoder.encode(h);
        let mut f = frame_header(ty::HEADERS, block.len());
        f.extend_from_slice(&block);
        self.queue(id, Bytes::from(f));
        if fin {
            self.outbox.entry(id).or_default().fin = true;
        }
        self.flush(quic, id);
        Ok(())
    }

    /// client: 要求を送る。疑似ヘッダー（:method・:scheme・:authority・:path）を含めること。
    pub fn send_request(&mut self, quic: &mut Connection, headers: Headers, end_stream: bool) -> Result<StreamId, ApiError> {
        if self.side != Side::Client {
            return Err(ApiError::Invalid("server は要求を送れない"));
        }
        if self.goaway_recv.is_some() {
            return Err(ApiError::Invalid("GOAWAY を受けた"));
        }
        validate(&headers, Kind::Request).map_err(ApiError::Invalid)?;
        let id = quic.open_stream(Dir::Bi).ok_or(ApiError::Blocked)?;
        self.streams.insert(id, ReqStream::new(self.config.max_field_section_size as usize));
        self.send_headers(quic, id, &headers, end_stream)?;
        Ok(id)
    }

    /// server: 応答のヘッダーを送る（:status を含めること）。
    pub fn send_response(&mut self, quic: &mut Connection, stream: StreamId, headers: Headers, end_stream: bool) -> Result<(), ApiError> {
        if self.side != Side::Server {
            return Err(ApiError::Invalid("client は応答を送れない"));
        }
        validate(&headers, Kind::Response).map_err(ApiError::Invalid)?;
        self.send_headers(quic, stream, &headers, end_stream)
    }

    /// body を送る。受け付けたバイト数を返す（送信待ちの上限まで）。
    pub fn send_data(&mut self, quic: &mut Connection, stream: StreamId, data: Bytes, end_stream: bool) -> Result<usize, ApiError> {
        let size = self.outbox.get(&stream).map_or(0, |o| o.size);
        let room = self.config.max_outbox.saturating_sub(size);
        let n = room.min(data.len());
        if n > 0 {
            self.queue(stream, Bytes::from(frame_header(ty::DATA, n)));
            self.queue(stream, data.slice(..n));
        }
        if end_stream && n == data.len() {
            self.outbox.entry(stream).or_default().fin = true;
        }
        self.flush(quic, stream);
        Ok(n)
    }

    /// 送信を終える（FIN）。
    pub fn finish(&mut self, quic: &mut Connection, stream: StreamId) {
        self.outbox.entry(stream).or_default().fin = true;
        self.flush(quic, stream);
    }

    /// stream を打ち切る（H3_REQUEST_CANCELLED など）。
    pub fn reset_stream(&mut self, quic: &mut Connection, stream: StreamId, code: H3ErrorCode) {
        let _ = self.stream_error(quic, stream, code);
    }

    /// 受け取った body を最大 `max` バイト取り出す。
    pub fn recv_body(&mut self, quic: &mut Connection, stream: StreamId, max: usize) -> Option<Bytes> {
        let rs = self.streams.get_mut(&stream)?;
        let Some(front) = rs.body.front_mut() else {
            rs.data_signaled = false;
            return None;
        };
        let out = if front.len() > max {
            front.split_to(max)
        } else {
            rs.body.pop_front().unwrap_or_default()
        };
        let was_full = rs.body_len >= self.config.max_body_buffer;
        rs.body_len -= out.len();
        if rs.body.is_empty() {
            rs.data_signaled = false;
        }
        if was_full {
            let _ = self.read_request_stream(quic, stream);
        }
        Some(out)
    }

    /// server: GOAWAY を送る（以後の新しい要求は拒否する）。
    pub fn goaway(&mut self, quic: &mut Connection) {
        let Some(c) = self.control_out else { return };
        let id = match self.side {
            Side::Server => self.max_request.map_or(0, |m| m + 4),
            Side::Client => 0,
        };
        if self.goaway_sent.is_some_and(|g| id >= g) {
            return;
        }
        self.goaway_sent = Some(id);
        let mut p = Vec::new();
        varint::write(&mut p, id);
        let mut f = frame_header(ty::GOAWAY, p.len());
        f.extend_from_slice(&p);
        let _ = quic.stream_send(c, &f);
    }

    // ------------------------------------------------------------------ WebTransport

    /// client: WebTransport の session を開く（拡張 CONNECT）。`origin` はブラウザーでなければ省略可。
    pub fn wt_connect(&mut self, quic: &mut Connection, authority: &str, path: &str, extra: Headers) -> Result<StreamId, ApiError> {
        if self.side != Side::Client || !self.config.enable_webtransport {
            return Err(ApiError::Invalid("WebTransport を使えない"));
        }
        let mut h = Headers::new()
            .with(":method", "CONNECT")
            .with(":protocol", "webtransport")
            .with(":scheme", "https")
            .with(":authority", authority)
            .with(":path", path);
        for f in extra.iter() {
            h.push(f.clone());
        }
        validate(&h, Kind::Request).map_err(ApiError::Invalid)?;
        let id = quic.open_stream(Dir::Bi).ok_or(ApiError::Blocked)?;
        let mut rs = ReqStream::new(self.config.max_field_section_size as usize);
        rs.is_connect = true;
        self.streams.insert(id, rs);
        self.wt_sessions.insert(
            id,
            WtSession {
                state: WtState::Pending,
                streams: HashSet::new(),
                capsules: CapsuleReader::default(),
                close_sent: false,
            },
        );
        if self.peer_settings.is_none() {
            // server の SETTINGS を見てから送る（draft-07 3.1）
            self.deferred_wt.push((id, h));
        } else if self.wt_version().is_none() {
            self.streams.remove(&id);
            self.wt_sessions.remove(&id);
            let _ = quic.stream_reset(id, H3ErrorCode::REQUEST_CANCELLED.0);
            return Err(ApiError::Invalid("server が WebTransport を広告していない"));
        } else {
            let h = self.with_wt_draft_header(h);
            self.send_headers(quic, id, &h, false)?;
        }
        Ok(id)
    }

    /// server: WebTransport の要求を受け付ける（200）。
    pub fn wt_accept(&mut self, quic: &mut Connection, session: StreamId, extra: Headers) -> Result<(), ApiError> {
        let s = self.wt_sessions.get_mut(&session).ok_or(ApiError::Invalid("未知の session"))?;
        if s.state != WtState::Pending || self.side != Side::Server {
            return Err(ApiError::Invalid("受け付けられない状態"));
        }
        s.state = WtState::Established;
        if let Some(rs) = self.streams.get_mut(&session) {
            rs.wt_session = true;
        }
        let mut h = Headers::new().with(":status", "200");
        if self.wt_version() == Some(WtVersion::Draft02) {
            // draft-02 の server は応答に版を書く（Chrome・QUICHE と同じ）
            h.push(crate::headers::Field::new("sec-webtransport-http3-draft", "draft02"));
        }
        for f in extra.iter() {
            h.push(f.clone());
        }
        self.send_headers(quic, session, &h, false)?;
        self.release_buffered(session);
        Ok(())
    }

    /// server: WebTransport の要求を拒否する（4xx・5xx）。
    pub fn wt_reject(&mut self, quic: &mut Connection, session: StreamId, status: u16) -> Result<(), ApiError> {
        if self.wt_sessions.remove(&session).is_none() {
            return Err(ApiError::Invalid("未知の session"));
        }
        let h = Headers::new().with(":status", status.to_string());
        self.send_headers(quic, session, &h, true)?;
        self.drop_session_streams(quic, session);
        self.streams.remove(&session);
        Ok(())
    }

    fn with_wt_draft_header(&self, mut h: Headers) -> Headers {
        if self.wt_version() == Some(WtVersion::Draft02) {
            h.push(crate::headers::Field::new("sec-webtransport-http3-draft02", "1"));
        }
        h
    }

    fn established(&self, session: StreamId) -> Result<(), ApiError> {
        match self.wt_sessions.get(&session) {
            Some(s) if s.state == WtState::Established && !s.close_sent => Ok(()),
            _ => Err(ApiError::Invalid("session が確立していない")),
        }
    }

    /// WebTransport の stream を開く。
    pub fn wt_open(&mut self, quic: &mut Connection, session: StreamId, kind: WtStreamKind) -> Result<StreamId, ApiError> {
        self.established(session)?;
        let (dir, t) = match kind {
            WtStreamKind::Bi => (Dir::Bi, ty::WEBTRANSPORT_STREAM),
            WtStreamKind::Uni => (Dir::Uni, stream_ty::WEBTRANSPORT),
        };
        let id = quic.open_stream(dir).ok_or(ApiError::Blocked)?;
        let mut hdr = Vec::new();
        varint::write(&mut hdr, t);
        varint::write(&mut hdr, session.0);
        quic.stream_send(id, &hdr)?;
        self.wt_streams.insert(
            id,
            WtData {
                session,
                leftover: Bytes::new(),
                leftover_fin: false,
                announced: true,
                kind,
            },
        );
        if let Some(s) = self.wt_sessions.get_mut(&session) {
            s.streams.insert(id);
        }
        Ok(id)
    }

    /// WebTransport の stream に書く（受け付けたバイト数）。
    pub fn wt_send(&mut self, quic: &mut Connection, stream: StreamId, data: Bytes) -> Result<usize, ApiError> {
        if !self.wt_streams.contains_key(&stream) {
            return Err(ApiError::Invalid("WebTransport の stream でない"));
        }
        Ok(quic.stream_send_bytes(stream, data)?)
    }

    pub fn wt_finish(&mut self, quic: &mut Connection, stream: StreamId) -> Result<(), ApiError> {
        Ok(quic.stream_finish(stream)?)
    }

    /// WebTransport の stream を打ち切る（アプリケーションのコードは 32 bit、draft-07 4.3 で写像する）。
    pub fn wt_reset(&mut self, quic: &mut Connection, stream: StreamId, code: u32) -> Result<(), ApiError> {
        Ok(quic.stream_reset(stream, wt_error_to_h3(code))?)
    }

    pub fn wt_stop(&mut self, quic: &mut Connection, stream: StreamId, code: u32) -> Result<(), ApiError> {
        Ok(quic.stream_stop(stream, wt_error_to_h3(code))?)
    }

    /// WebTransport の stream から読む。リセットのコードは WebTransport の 32 bit に戻して返す。
    pub fn wt_recv(&mut self, quic: &mut Connection, stream: StreamId, max: usize) -> Result<RecvChunk, ApiError> {
        let w = self.wt_streams.get_mut(&stream).ok_or(ApiError::Invalid("WebTransport の stream でない"))?;
        if !w.leftover.is_empty() || w.leftover_fin {
            let n = w.leftover.len().min(max.max(1));
            let data = w.leftover.split_to(n);
            let fin = w.leftover.is_empty() && w.leftover_fin;
            if fin {
                w.leftover_fin = false;
                if w.kind == WtStreamKind::Uni {
                    self.forget_wt_stream(stream);
                }
            }
            return Ok(RecvChunk { data, fin });
        }
        match quic.stream_recv(stream, max) {
            Ok(c) => {
                if c.fin && w.kind == WtStreamKind::Uni {
                    self.forget_wt_stream(stream);
                }
                Ok(c)
            }
            Err(StreamError::Reset(code)) => {
                self.forget_wt_stream(stream);
                Err(ApiError::Stream(StreamError::Reset(
                    crate::error::h3_to_wt_error(code).map_or(code, u64::from),
                )))
            }
            Err(e) => Err(e.into()),
        }
    }

    fn forget_wt_stream(&mut self, stream: StreamId) {
        if let Some(w) = self.wt_streams.remove(&stream)
            && let Some(s) = self.wt_sessions.get_mut(&w.session)
        {
            s.streams.remove(&stream);
        }
    }

    /// stream が属する session。
    pub fn wt_session_of(&self, stream: StreamId) -> Option<StreamId> {
        self.wt_streams.get(&stream).map(|w| w.session)
    }

    /// datagram の最大長（Quarter Stream ID の分を引いたもの）。
    pub fn wt_max_datagram_size(&self, quic: &Connection, session: StreamId) -> Option<usize> {
        let m = quic.max_datagram_size()?;
        Some(m.saturating_sub(varint::encoded_len(session.0 / 4)))
    }

    /// WebTransport の datagram を送る。
    pub fn wt_send_datagram(&mut self, quic: &mut Connection, session: StreamId, data: &[u8]) -> Result<(), ApiError> {
        self.established(session)?;
        if !self.peer_settings.as_ref().is_some_and(|s| s.h3_datagram) {
            return Err(ApiError::Invalid("相手が H3_DATAGRAM を広告していない"));
        }
        let mut v = Vec::with_capacity(8 + data.len());
        varint::write(&mut v, session.0 / 4);
        v.extend_from_slice(data);
        quic.send_datagram(Bytes::from(v)).map_err(ApiError::Datagram)
    }

    /// session を閉じる（CLOSE_WEBTRANSPORT_SESSION と FIN）。
    pub fn wt_close(&mut self, quic: &mut Connection, session: StreamId, code: u32, reason: &str) {
        let Some(s) = self.wt_sessions.get_mut(&session) else { return };
        if s.close_sent {
            return;
        }
        s.close_sent = true;
        let cap = Capsule::CloseWebTransportSession {
            code,
            reason: reason.chars().take(256).collect(),
        }
        .encode();
        let _ = self.send_data(quic, session, Bytes::from(cap), true);
        self.close_session_state(quic, session, code, reason.to_string());
    }

    /// session の状態を消し、属する stream を打ち切る（draft-07 5）。
    fn close_session_state(&mut self, quic: &mut Connection, session: StreamId, code: u32, reason: String) {
        if self.wt_sessions.contains_key(&session) {
            self.drop_session_streams(quic, session);
            self.wt_sessions.remove(&session);
            self.events.push_back(H3Event::WebTransportClosed { session, code, reason });
        }
        self.buffered_dgrams.retain(|(s, _)| *s != session);
    }

    fn drop_session_streams(&mut self, quic: &mut Connection, session: StreamId) {
        let ids: Vec<StreamId> = self.wt_streams.iter().filter(|(_, w)| w.session == session).map(|(i, _)| *i).collect();
        let gone = H3ErrorCode::WEBTRANSPORT_SESSION_GONE.0;
        for id in ids {
            let _ = quic.stream_reset(id, gone);
            let _ = quic.stream_stop(id, gone);
            self.wt_streams.remove(&id);
        }
    }

    /// 確立した session の一覧。
    pub fn wt_sessions(&self) -> Vec<StreamId> {
        self.wt_sessions
            .iter()
            .filter(|(_, s)| s.state == WtState::Established)
            .map(|(i, _)| *i)
            .collect()
    }
}

fn qpack_err(e: QpackError) -> H3Error {
    match e {
        QpackError::Decompression(w) | QpackError::EncoderStream(w) | QpackError::DecoderStream(w) => {
            let code = match e {
                QpackError::Decompression(_) => H3ErrorCode::QPACK_DECOMPRESSION_FAILED,
                QpackError::EncoderStream(_) => H3ErrorCode::QPACK_ENCODER_STREAM_ERROR,
                _ => H3ErrorCode::QPACK_DECODER_STREAM_ERROR,
            };
            H3Error::new(code, w)
        }
        QpackError::TooLarge => h3err!(EXCESSIVE_LOAD, "field section が大きすぎる"),
    }
}
