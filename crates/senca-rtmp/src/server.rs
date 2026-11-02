//! サーバー側のセッション（相手が publish または play する）。
//!
//! 使い方（sans-io）:
//! 1. 受信したバイト列を [`ServerSession::feed`] に入れる。
//! 2. [`ServerSession::poll_event`] でイベントを取り出し、判断が必要なもの（[`ServerEvent::Connect`]・
//!    [`ServerEvent::Publish`]・[`ServerEvent::Play`]）には `accept_*`・`reject_*` を呼ぶ。
//! 3. [`ServerSession::take_output`] で送るバイト列を取り出して相手に書く（feed や accept のたびに確認する）。
//!
//! `Err` を返したらセッションは使えない（接続を閉じる）。stream key は [`Secret`] で包み、Debug に出さない。
//!
//! connect の判断待ちの間は、後続の受信データを解釈せずに持っておく（上限 1 MiB）。

use crate::chunk::ChunkLimits;
use crate::conn::{Conn, Incoming};
use crate::handshake::ServerHandshake;
use crate::message::{Command, MediaMessage, MediaType, UserControl, csid, status_object};
use crate::{Error, Secret};
use bytes::Bytes;
use senca_amf0::{Limits, Value};
use std::collections::{HashMap, VecDeque};

/// サーバーの設定。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// 自分が送る chunk size（connect の受理時に通知する）。
    pub chunk_size: u32,
    /// 相手に求める受信確認の間隔。
    pub window_ack_size: u32,
    pub peer_bandwidth: u32,
    pub chunk_limits: ChunkLimits,
    pub amf_limits: Limits,
    /// 1 接続で作れる stream の数。
    pub max_streams: u32,
    /// ハンドシェイクの S1 の時刻欄と乱数欄の種。
    pub handshake_time: u32,
    pub handshake_seed: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            chunk_size: 4096,
            window_ack_size: 2_500_000,
            peer_bandwidth: 2_500_000,
            chunk_limits: ChunkLimits::default(),
            amf_limits: Limits::default(),
            max_streams: 8,
            handshake_time: 0,
            handshake_seed: 0x5E4C_A000_0000_0001,
        }
    }
}

/// connect の内容。
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectInfo {
    /// app（`?` 以降を除いたもの）。
    pub app: String,
    /// app の `?` 以降（トークンが入ることがあるので秘密扱い）。
    pub app_query: Option<Secret>,
    /// tcUrl（クエリにトークンが入ることがあるので秘密扱い）。
    pub tc_url: Option<Secret>,
    pub flash_ver: Option<String>,
    pub object_encoding: f64,
}

/// stream が閉じた理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    FcUnpublish,
    CloseStream,
    DeleteStream,
}

/// サーバーのイベント。
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    /// connect を受けた。`accept_connect` か `reject_connect` を呼ぶ。
    Connect(ConnectInfo),
    /// publish を受けた。`accept_publish` か `reject_publish` を呼ぶ。
    /// `stream_key` は publish の名前そのもの（`?` 以降を含む）。
    Publish {
        stream_id: u32,
        stream_key: Secret,
        publish_type: String,
    },
    /// play を受けた。`accept_play` か `reject_play` を呼ぶ。
    Play {
        stream_id: u32,
        stream_key: Secret,
        start: f64,
        duration: f64,
        reset: bool,
    },
    /// publish 中の stream の音声・映像。
    Media {
        stream_id: u32,
        message: MediaMessage,
    },
    /// `@setDataFrame` で送られた onMetaData。payload は FLV の script タグ本体と同じ形
    /// （`"onMetaData", {...}`。`@setDataFrame` は外してある）。
    Metadata {
        stream_id: u32,
        timestamp: u32,
        payload: Bytes,
    },
    /// その他の AMF0 data message（payload は名前から始まる）。
    Data {
        stream_id: u32,
        timestamp: u32,
        payload: Bytes,
    },
    /// publish・play していた stream が終わった。
    StreamClosed { stream_id: u32, reason: CloseReason },
    /// こちらの ping に応答があった。
    PingResponse { timestamp: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Handshake,
    WaitConnect,
    ConnectPending { txn: f64 },
    Connected,
    Closed,
}

#[derive(Debug, Clone, PartialEq)]
enum StreamState {
    Idle,
    PublishPending(Secret),
    Publishing(Secret),
    PlayPending,
    Playing,
}

impl StreamState {
    fn is_active(&self) -> bool {
        !matches!(self, StreamState::Idle)
    }
}

const MAX_DEFERRED_INPUT: usize = 1024 * 1024;

/// サーバー側の RTMP セッション。
pub struct ServerSession {
    config: ServerConfig,
    state: State,
    hs: ServerHandshake,
    conn: Conn,
    events: VecDeque<ServerEvent>,
    streams: HashMap<u32, StreamState>,
    next_stream_id: u32,
    dropped_media: u64,
    scratch: Vec<Incoming>,
}

impl std::fmt::Debug for ServerSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerSession")
            .field("state", &self.state)
            .field("streams", &self.streams.len())
            .finish_non_exhaustive()
    }
}

impl ServerSession {
    pub fn new(config: ServerConfig) -> Self {
        let hs = ServerHandshake::new(config.handshake_time, config.handshake_seed);
        let conn = Conn::new(config.chunk_limits, config.amf_limits);
        ServerSession {
            config,
            state: State::Handshake,
            hs,
            conn,
            events: VecDeque::new(),
            streams: HashMap::new(),
            next_stream_id: 1,
            dropped_media: 0,
            scratch: Vec::new(),
        }
    }

    /// 受信したバイト列を入れる。
    pub fn feed(&mut self, data: &[u8]) -> Result<(), Error> {
        if self.state == State::Closed {
            return Err(Error::Closed);
        }
        let r = self.feed_inner(data);
        if r.is_err() {
            self.state = State::Closed;
        }
        r
    }

    fn feed_inner(&mut self, mut data: &[u8]) -> Result<(), Error> {
        if self.state == State::Handshake {
            let used = self.hs.feed(data, &mut self.conn.out)?;
            data = &data[used..];
            if !self.hs.is_done() {
                return Ok(());
            }
            self.state = State::WaitConnect;
        }
        self.conn.inbuf.extend_from_slice(data);
        self.conn.count_in(data.len())?;
        self.process()
    }

    fn process(&mut self) -> Result<(), Error> {
        loop {
            if matches!(self.state, State::ConnectPending { .. }) {
                if self.conn.inbuf.len() > MAX_DEFERRED_INPUT {
                    return Err(Error::LimitExceeded("input while connect is pending"));
                }
                return Ok(());
            }
            let Some(m) = self.conn.next_message()? else {
                return Ok(());
            };
            let mut inc = std::mem::take(&mut self.scratch);
            self.conn.handle(m, &mut inc)?;
            for i in inc.drain(..) {
                self.on_incoming(i)?;
            }
            self.scratch = inc;
        }
    }

    /// 次のイベント。
    pub fn poll_event(&mut self) -> Option<ServerEvent> {
        self.events.pop_front()
    }

    /// 送るべきバイト列を取り出す（なければ空）。
    pub fn take_output(&mut self) -> Bytes {
        self.conn.out.split().freeze()
    }

    /// 送るべきバイト数。
    pub fn pending_output(&self) -> usize {
        self.conn.out.len()
    }

    /// publish 前・未受理の stream に届いて捨てた音声・映像の数。
    pub fn dropped_media(&self) -> u64 {
        self.dropped_media
    }

    pub fn is_connected(&self) -> bool {
        self.state == State::Connected
    }

    /// connect を受け入れる。
    pub fn accept_connect(&mut self) -> Result<(), Error> {
        let State::ConnectPending { txn } = self.state else {
            return Err(Error::InvalidState("no pending connect"));
        };
        self.conn
            .send_window_ack_size(self.config.window_ack_size)?;
        self.conn
            .send_peer_bandwidth(self.config.peer_bandwidth, 2)?;
        self.conn.set_out_chunk_size(self.config.chunk_size)?;
        let props = Value::object([
            ("fmsVer", Value::string("FMS/3,0,1,123")),
            ("capabilities", Value::Number(31.0)),
        ]);
        let mut info = status_object(
            "status",
            "NetConnection.Connect.Success",
            "Connection succeeded.",
        );
        if let Value::Object(p) = &mut info {
            p.push(("objectEncoding".into(), Value::Number(0.0)));
        }
        self.conn.send_command(
            csid::COMMAND,
            0,
            &Command::new("_result", txn, props, vec![info]),
        )?;
        self.state = State::Connected;
        // 判断待ちの間にためていた入力を処理する。
        let r = self.process();
        if r.is_err() {
            self.state = State::Closed;
        }
        r
    }

    /// connect を断る。`_error` を送ったあと、呼び出し側は送信を終えたら接続を閉じる。
    pub fn reject_connect(&mut self, description: &str) -> Result<(), Error> {
        let State::ConnectPending { txn } = self.state else {
            return Err(Error::InvalidState("no pending connect"));
        };
        let info = status_object("error", "NetConnection.Connect.Rejected", description);
        self.conn.send_command(
            csid::COMMAND,
            0,
            &Command::new("_error", txn, Value::Null, vec![info]),
        )?;
        self.state = State::Closed;
        Ok(())
    }

    /// publish を受け入れる。以後その stream の音声・映像が [`ServerEvent::Media`] で届く。
    pub fn accept_publish(&mut self, stream_id: u32) -> Result<(), Error> {
        let key = match self.streams.get(&stream_id) {
            Some(StreamState::PublishPending(k)) => k.clone(),
            _ => return Err(Error::InvalidState("no pending publish on this stream")),
        };
        self.conn
            .send_user_control(UserControl::StreamBegin(stream_id))?;
        self.send_status(
            stream_id,
            "status",
            "NetStream.Publish.Start",
            "Start publishing.",
        )?;
        self.streams.insert(stream_id, StreamState::Publishing(key));
        Ok(())
    }

    /// publish を断る。`code` は例えば `NetStream.Publish.BadName`・`NetStream.Publish.Denied`。
    pub fn reject_publish(
        &mut self,
        stream_id: u32,
        code: &str,
        description: &str,
    ) -> Result<(), Error> {
        match self.streams.get(&stream_id) {
            Some(StreamState::PublishPending(_)) => {}
            _ => return Err(Error::InvalidState("no pending publish on this stream")),
        }
        self.send_status(stream_id, "error", code, description)?;
        self.streams.insert(stream_id, StreamState::Idle);
        Ok(())
    }

    /// play を受け入れる。以後 [`send_media`](Self::send_media) で送れる。
    pub fn accept_play(&mut self, stream_id: u32) -> Result<(), Error> {
        match self.streams.get(&stream_id) {
            Some(StreamState::PlayPending) => {}
            _ => return Err(Error::InvalidState("no pending play on this stream")),
        }
        self.conn
            .send_user_control(UserControl::StreamBegin(stream_id))?;
        self.send_status(
            stream_id,
            "status",
            "NetStream.Play.Reset",
            "Playing and resetting.",
        )?;
        self.send_status(
            stream_id,
            "status",
            "NetStream.Play.Start",
            "Started playing.",
        )?;
        let mut b = Vec::new();
        for v in [
            Value::string("|RtmpSampleAccess"),
            Value::Boolean(true),
            Value::Boolean(true),
        ] {
            senca_amf0::encode(&v, &mut b)?;
        }
        self.conn.send_data(stream_id, 0, &[&b])?;
        self.streams.insert(stream_id, StreamState::Playing);
        Ok(())
    }

    /// play を断る。`code` は例えば `NetStream.Play.StreamNotFound`。
    pub fn reject_play(
        &mut self,
        stream_id: u32,
        code: &str,
        description: &str,
    ) -> Result<(), Error> {
        match self.streams.get(&stream_id) {
            Some(StreamState::PlayPending) => {}
            _ => return Err(Error::InvalidState("no pending play on this stream")),
        }
        self.send_status(stream_id, "error", code, description)?;
        self.streams.insert(stream_id, StreamState::Idle);
        Ok(())
    }

    fn require_playing(&self, stream_id: u32) -> Result<(), Error> {
        match self.streams.get(&stream_id) {
            Some(StreamState::Playing) => Ok(()),
            _ => Err(Error::InvalidState("stream is not playing")),
        }
    }

    /// play 中の stream に音声・映像を送る。payload は FLV タグ本体（部品の連結）。
    pub fn send_media(
        &mut self,
        stream_id: u32,
        media_type: MediaType,
        timestamp: u32,
        parts: &[&[u8]],
    ) -> Result<(), Error> {
        self.require_playing(stream_id)?;
        self.conn.send_parts(
            media_type.csid(),
            media_type.type_id(),
            stream_id,
            timestamp,
            parts,
        )
    }

    /// play 中の stream に data message を送る。`script_body` は FLV の script タグ本体
    /// （`"onMetaData", {...}` など）。
    pub fn send_data(
        &mut self,
        stream_id: u32,
        timestamp: u32,
        script_body: &[u8],
    ) -> Result<(), Error> {
        self.require_playing(stream_id)?;
        self.conn.send_data(stream_id, timestamp, &[script_body])
    }

    /// play の終わりを知らせる（Stream EOF と NetStream.Play.Stop）。
    pub fn end_play(&mut self, stream_id: u32) -> Result<(), Error> {
        self.require_playing(stream_id)?;
        self.conn
            .send_user_control(UserControl::StreamEof(stream_id))?;
        self.send_status(
            stream_id,
            "status",
            "NetStream.Play.Stop",
            "Stopped playing.",
        )?;
        self.streams.insert(stream_id, StreamState::Idle);
        Ok(())
    }

    /// onStatus を送る。
    pub fn send_status(
        &mut self,
        stream_id: u32,
        level: &str,
        code: &str,
        description: &str,
    ) -> Result<(), Error> {
        let cmd = Command::new(
            "onStatus",
            0.0,
            Value::Null,
            vec![status_object(level, code, description)],
        );
        self.conn
            .send_command(csid::STREAM_COMMAND, stream_id, &cmd)
    }

    /// Ping Request を送る（`timestamp` は呼び出し側の時計のミリ秒）。
    pub fn ping(&mut self, timestamp: u32) -> Result<(), Error> {
        self.conn
            .send_user_control(UserControl::PingRequest(timestamp))
    }

    fn reply(&mut self, name: &str, txn: f64, args: Vec<Value>) -> Result<(), Error> {
        if txn == 0.0 {
            return Ok(());
        }
        self.conn.send_command(
            csid::COMMAND,
            0,
            &Command::new(name, txn, Value::Null, args),
        )
    }

    fn close_stream(&mut self, stream_id: u32, reason: CloseReason) {
        if let Some(s) = self.streams.get_mut(&stream_id)
            && s.is_active()
        {
            *s = StreamState::Idle;
            self.events
                .push_back(ServerEvent::StreamClosed { stream_id, reason });
        }
    }

    fn on_incoming(&mut self, i: Incoming) -> Result<(), Error> {
        match i {
            Incoming::Command { stream_id, command } => self.on_command(stream_id, command),
            Incoming::Media { stream_id, message } => {
                if self.state != State::Connected {
                    return Err(Error::Protocol("media before connect"));
                }
                if matches!(
                    self.streams.get(&stream_id),
                    Some(StreamState::Publishing(_))
                ) {
                    self.events
                        .push_back(ServerEvent::Media { stream_id, message });
                } else {
                    self.dropped_media += 1;
                }
                Ok(())
            }
            Incoming::Data {
                stream_id,
                timestamp,
                name,
                name_end,
                payload,
            } => {
                if !matches!(
                    self.streams.get(&stream_id),
                    Some(StreamState::Publishing(_))
                ) {
                    return Ok(());
                }
                let ev = match name.as_str() {
                    "@setDataFrame" => {
                        let rest = payload.slice(name_end..);
                        match crate::message::data_name(&rest) {
                            Some((n, _)) if n == "onMetaData" => ServerEvent::Metadata {
                                stream_id,
                                timestamp,
                                payload: rest,
                            },
                            Some(_) => ServerEvent::Data {
                                stream_id,
                                timestamp,
                                payload: rest,
                            },
                            None => return Ok(()),
                        }
                    }
                    "onMetaData" => ServerEvent::Metadata {
                        stream_id,
                        timestamp,
                        payload,
                    },
                    "@clearDataFrame" => return Ok(()),
                    _ => ServerEvent::Data {
                        stream_id,
                        timestamp,
                        payload,
                    },
                };
                self.events.push_back(ev);
                Ok(())
            }
            Incoming::UserControl(u) => {
                match u {
                    UserControl::PingRequest(t) => {
                        self.conn.send_user_control(UserControl::PingResponse(t))?
                    }
                    UserControl::PingResponse(t) => self
                        .events
                        .push_back(ServerEvent::PingResponse { timestamp: t }),
                    _ => {}
                }
                Ok(())
            }
            Incoming::PeerBandwidth(_) => Ok(()),
        }
    }

    fn on_command(&mut self, stream_id: u32, c: Command) -> Result<(), Error> {
        let txn = c.transaction_id;
        if self.state == State::WaitConnect {
            if c.name != "connect" {
                return Err(Error::Protocol("expected connect"));
            }
            let obj = &c.object;
            let app_full = obj.get_str("app").unwrap_or("");
            let (app, query) = match app_full.split_once('?') {
                Some((a, q)) => (a.to_owned(), Some(Secret::new(q))),
                None => (app_full.to_owned(), None),
            };
            let info = ConnectInfo {
                app,
                app_query: query,
                tc_url: obj.get_str("tcUrl").map(Secret::new),
                flash_ver: obj.get_str("flashVer").map(str::to_owned),
                object_encoding: obj.get_f64("objectEncoding").unwrap_or(0.0),
            };
            if info.object_encoding != 0.0 {
                // AMF3 での応答は扱わない。
                return Err(Error::Protocol("AMF3 object encoding is not supported"));
            }
            self.state = State::ConnectPending { txn };
            self.events.push_back(ServerEvent::Connect(info));
            return Ok(());
        }
        if self.state != State::Connected {
            return Err(Error::Protocol("command in unexpected state"));
        }
        let name_arg = |c: &Command| c.arg(0).and_then(Value::as_str).map(str::to_owned);
        match c.name.as_str() {
            "connect" => return Err(Error::Protocol("duplicate connect")),
            "releaseStream" | "FCSubscribe" => {
                self.reply("_result", txn, vec![Value::Undefined])?
            }
            "FCPublish" => {
                self.reply("_result", txn, vec![Value::Undefined])?;
                let info = status_object("status", "NetStream.Publish.Start", "FCPublish");
                self.conn.send_command(
                    csid::COMMAND,
                    0,
                    &Command::new("onFCPublish", 0.0, Value::Null, vec![info]),
                )?;
            }
            "createStream" => {
                if self.streams.len() as u32 >= self.config.max_streams {
                    self.reply(
                        "_error",
                        txn,
                        vec![status_object(
                            "error",
                            "NetConnection.Call.Failed",
                            "Too many streams.",
                        )],
                    )?;
                    return Ok(());
                }
                let id = self.next_stream_id;
                self.next_stream_id += 1;
                self.streams.insert(id, StreamState::Idle);
                self.reply("_result", txn, vec![Value::Number(id as f64)])?;
            }
            "publish" => {
                let Some(name) = name_arg(&c) else {
                    return Err(Error::Protocol("publish without a name"));
                };
                match self.streams.get(&stream_id) {
                    Some(StreamState::Idle) => {}
                    Some(_) => return Err(Error::Protocol("publish on a busy stream")),
                    None => return Err(Error::Protocol("publish on an unknown stream")),
                }
                let publish_type = c
                    .arg(1)
                    .and_then(Value::as_str)
                    .unwrap_or("live")
                    .to_owned();
                let key = Secret::new(name);
                self.streams
                    .insert(stream_id, StreamState::PublishPending(key.clone()));
                self.events.push_back(ServerEvent::Publish {
                    stream_id,
                    stream_key: key,
                    publish_type,
                });
            }
            "play" => {
                let Some(name) = name_arg(&c) else {
                    return Err(Error::Protocol("play without a name"));
                };
                match self.streams.get(&stream_id) {
                    Some(StreamState::Idle) => {}
                    Some(_) => return Err(Error::Protocol("play on a busy stream")),
                    None => return Err(Error::Protocol("play on an unknown stream")),
                }
                let start = c.arg(1).and_then(Value::as_f64).unwrap_or(-2.0);
                let duration = c.arg(2).and_then(Value::as_f64).unwrap_or(-1.0);
                let reset = c.arg(3).and_then(Value::as_bool).unwrap_or(true);
                self.streams.insert(stream_id, StreamState::PlayPending);
                self.events.push_back(ServerEvent::Play {
                    stream_id,
                    stream_key: Secret::new(name),
                    start,
                    duration,
                    reset,
                });
            }
            "FCUnpublish" => {
                self.reply("_result", txn, vec![Value::Undefined])?;
                if let Some(name) = name_arg(&c) {
                    let found = self.streams.iter().find_map(|(id, s)| match s {
                        StreamState::Publishing(k) | StreamState::PublishPending(k)
                            if k.expose() == name =>
                        {
                            Some(*id)
                        }
                        _ => None,
                    });
                    if let Some(id) = found {
                        self.close_stream(id, CloseReason::FcUnpublish);
                    }
                }
            }
            "closeStream" => self.close_stream(stream_id, CloseReason::CloseStream),
            "deleteStream" => {
                let id = c.arg(0).and_then(Value::as_f64).unwrap_or(stream_id as f64);
                if id >= 1.0 && id <= u32::MAX as f64 {
                    let id = id as u32;
                    self.close_stream(id, CloseReason::DeleteStream);
                    self.streams.remove(&id);
                }
            }
            // 受け取るだけで応答しないもの。
            "receiveAudio" | "receiveVideo" | "pause" | "seek" | "_result" | "_error"
            | "onStatus" => {}
            _ => self.reply(
                "_error",
                txn,
                vec![status_object(
                    "error",
                    "NetConnection.Call.Failed",
                    "Method not found.",
                )],
            )?,
        }
        Ok(())
    }
}
