//! クライアント側のセッション（外部のサーバーへ publish する、または外部から play で取り込む）。
//!
//! 使い方はサーバーと同じ（[`ClientSession::feed`]・[`ClientSession::poll_event`]・
//! [`ClientSession::take_output`]）。作った直後から C0+C1 が出力にある。
//!
//! 流れ: ハンドシェイク → connect → （publish なら releaseStream・FCPublish）→ createStream →
//! publish または play → [`ClientEvent::PublishStarted`] / [`ClientEvent::PlayStarted`]。

use crate::chunk::ChunkLimits;
use crate::conn::{Conn, Incoming, info_code};
use crate::handshake::ClientHandshake;
use crate::message::{Command, MediaMessage, MediaType, UserControl, csid};
use crate::{Error, Secret};
use bytes::{Bytes, BytesMut};
use senca_amf0::{Limits, Value};
use std::collections::VecDeque;

/// publish か play か。
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMode {
    Publish {
        stream_key: Secret,
        /// `live`・`record`・`append`。
        publish_type: String,
    },
    Play {
        stream_key: Secret,
        /// -2 = ライブ優先、-1 = ライブのみ、0 以上 = 録画の開始位置（秒）。
        start: f64,
    },
}

/// クライアントの設定。
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub app: String,
    /// 例: `rtmp://host:1935/live`（トークンを含みうるので秘密扱い）。
    pub tc_url: Secret,
    pub flash_ver: String,
    pub mode: ClientMode,
    /// 自分が送る chunk size。
    pub chunk_size: u32,
    pub chunk_limits: ChunkLimits,
    pub amf_limits: Limits,
    /// play のときに知らせる buffer length（ミリ秒）。
    pub buffer_length_ms: u32,
    pub handshake_time: u32,
    pub handshake_seed: u64,
}

impl ClientConfig {
    pub fn new(app: &str, tc_url: &str, mode: ClientMode) -> Self {
        ClientConfig {
            app: app.to_owned(),
            tc_url: Secret::new(tc_url),
            flash_ver: "FMLE/3.0 (compatible; senca)".to_owned(),
            mode,
            chunk_size: 4096,
            chunk_limits: ChunkLimits::default(),
            amf_limits: Limits::default(),
            buffer_length_ms: 3000,
            handshake_time: 0,
            handshake_seed: 0x5E4C_A000_0000_0002,
        }
    }
}

/// クライアントのイベント。
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    /// connect が受け入れられた。
    Connected,
    /// publish が始まった。以後 [`ClientSession::send_media`] で送れる。
    PublishStarted {
        stream_id: u32,
    },
    /// play が始まった。以後 [`ClientEvent::Media`] が届く。
    PlayStarted {
        stream_id: u32,
    },
    /// onStatus を受けた（参考。description は相手次第で stream key を含みうるので秘密扱い）。
    Status {
        stream_id: u32,
        level: String,
        code: String,
        description: Secret,
    },
    /// 相手がこちらの要求を断った（`_error`、または level が error の onStatus）。以後は使えない。
    Rejected {
        command: String,
        code: Option<String>,
        description: Secret,
    },
    Media {
        stream_id: u32,
        message: MediaMessage,
    },
    /// onMetaData（payload は FLV の script タグ本体と同じ形）。
    Metadata {
        stream_id: u32,
        timestamp: u32,
        payload: Bytes,
    },
    Data {
        stream_id: u32,
        timestamp: u32,
        payload: Bytes,
    },
    /// 相手が Stream EOF または NetStream.Play.Stop / UnpublishNotify を送った。
    StreamEnded {
        stream_id: u32,
    },
    PingResponse {
        timestamp: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Handshake,
    Connecting,
    CreatingStream,
    Starting { stream_id: u32 },
    Active { stream_id: u32 },
    Failed,
    Closed,
}

const TXN_CONNECT: f64 = 1.0;
const TXN_CREATE_STREAM: f64 = 4.0;

/// クライアント側の RTMP セッション。
pub struct ClientSession {
    config: ClientConfig,
    state: State,
    hs: ClientHandshake,
    conn: Conn,
    events: VecDeque<ClientEvent>,
    next_txn: f64,
    scratch: Vec<Incoming>,
}

impl std::fmt::Debug for ClientSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientSession")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl ClientSession {
    /// 作ると同時に C0+C1 を出力に積む。
    pub fn new(config: ClientConfig) -> Self {
        let mut conn = Conn::new(config.chunk_limits, config.amf_limits);
        let mut out = BytesMut::new();
        let hs = ClientHandshake::start(config.handshake_time, config.handshake_seed, &mut out);
        conn.out = out;
        ClientSession {
            config,
            state: State::Handshake,
            hs,
            conn,
            events: VecDeque::new(),
            next_txn: TXN_CREATE_STREAM + 1.0,
            scratch: Vec::new(),
        }
    }

    /// 受信したバイト列を入れる。`Err` のあとは使えない。[`close`](Self::close) のあとは何もしない。
    pub fn feed(&mut self, data: &[u8]) -> Result<(), Error> {
        match self.state {
            // close() のあとに届いたもの（_result など）は読み捨てる。
            State::Closed => return Ok(()),
            State::Failed => return Err(Error::Closed),
            _ => {}
        }
        let r = self.feed_inner(data);
        if r.is_err() {
            self.state = State::Failed;
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
            self.start_connect()?;
        }
        self.conn.inbuf.extend_from_slice(data);
        self.conn.count_in(data.len())?;
        while let Some(m) = self.conn.next_message()? {
            let mut inc = std::mem::take(&mut self.scratch);
            self.conn.handle(m, &mut inc)?;
            for i in inc.drain(..) {
                self.on_incoming(i)?;
            }
            self.scratch = inc;
            if self.state == State::Failed {
                break;
            }
        }
        Ok(())
    }

    pub fn poll_event(&mut self) -> Option<ClientEvent> {
        self.events.pop_front()
    }

    pub fn take_output(&mut self) -> Bytes {
        self.conn.out.split().freeze()
    }

    pub fn pending_output(&self) -> usize {
        self.conn.out.len()
    }

    /// publish・play が始まっていればその stream id。
    pub fn active_stream(&self) -> Option<u32> {
        match self.state {
            State::Active { stream_id } => Some(stream_id),
            _ => None,
        }
    }

    fn start_connect(&mut self) -> Result<(), Error> {
        self.state = State::Connecting;
        self.conn.set_out_chunk_size(self.config.chunk_size)?;
        let mut props = vec![
            ("app".to_owned(), Value::string(self.config.app.clone())),
            ("type".to_owned(), Value::string("nonprivate")),
            (
                "flashVer".to_owned(),
                Value::string(self.config.flash_ver.clone()),
            ),
            (
                "tcUrl".to_owned(),
                Value::string(self.config.tc_url.expose()),
            ),
        ];
        if matches!(self.config.mode, ClientMode::Play { .. }) {
            props.extend([
                ("fpad".to_owned(), Value::Boolean(false)),
                ("capabilities".to_owned(), Value::Number(15.0)),
                ("audioCodecs".to_owned(), Value::Number(4071.0)),
                ("videoCodecs".to_owned(), Value::Number(252.0)),
                ("videoFunction".to_owned(), Value::Number(1.0)),
                ("objectEncoding".to_owned(), Value::Number(0.0)),
            ]);
        }
        self.conn.send_command(
            csid::COMMAND,
            0,
            &Command::new("connect", TXN_CONNECT, Value::Object(props), vec![]),
        )
    }

    fn txn(&mut self) -> f64 {
        let t = self.next_txn;
        self.next_txn += 1.0;
        t
    }

    fn stream_key(&self) -> &Secret {
        match &self.config.mode {
            ClientMode::Publish { stream_key, .. } | ClientMode::Play { stream_key, .. } => {
                stream_key
            }
        }
    }

    fn fail(&mut self, command: &str, cmd: &Command) {
        let info = cmd.arg(0);
        self.events.push_back(ClientEvent::Rejected {
            command: command.to_owned(),
            code: info.and_then(|v| v.get_str("code")).map(str::to_owned),
            description: Secret::new(info.and_then(|v| v.get_str("description")).unwrap_or("")),
        });
        self.state = State::Failed;
    }

    fn on_incoming(&mut self, i: Incoming) -> Result<(), Error> {
        match i {
            Incoming::Command { stream_id, command } => self.on_command(stream_id, command),
            Incoming::Media { stream_id, message } => {
                if self.active_stream() == Some(stream_id) {
                    self.events
                        .push_back(ClientEvent::Media { stream_id, message });
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
                let ev = match name.as_str() {
                    "onMetaData" => ClientEvent::Metadata {
                        stream_id,
                        timestamp,
                        payload,
                    },
                    "@setDataFrame" => ClientEvent::Metadata {
                        stream_id,
                        timestamp,
                        payload: payload.slice(name_end..),
                    },
                    "|RtmpSampleAccess" => return Ok(()),
                    _ => ClientEvent::Data {
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
                        .push_back(ClientEvent::PingResponse { timestamp: t }),
                    UserControl::StreamEof(id) if self.active_stream() == Some(id) => self
                        .events
                        .push_back(ClientEvent::StreamEnded { stream_id: id }),
                    _ => {}
                }
                Ok(())
            }
            Incoming::PeerBandwidth(size) => {
                // 仕様: 前回知らせた値と違えば Window Acknowledgement Size で応える。
                if self.conn.sent_window != Some(size) {
                    self.conn.send_window_ack_size(size)?;
                }
                Ok(())
            }
        }
    }

    fn on_command(&mut self, stream_id: u32, c: Command) -> Result<(), Error> {
        match (c.name.as_str(), self.state) {
            ("_result", State::Connecting) if c.transaction_id == TXN_CONNECT => {
                self.events.push_back(ClientEvent::Connected);
                let key = self.stream_key().expose().to_owned();
                if matches!(self.config.mode, ClientMode::Publish { .. }) {
                    for (name, txn) in [("releaseStream", 2.0), ("FCPublish", 3.0)] {
                        self.conn.send_command(
                            csid::COMMAND,
                            0,
                            &Command::new(
                                name,
                                txn,
                                Value::Null,
                                vec![Value::string(key.as_str())],
                            ),
                        )?;
                    }
                }
                self.conn.send_command(
                    csid::COMMAND,
                    0,
                    &Command::new("createStream", TXN_CREATE_STREAM, Value::Null, vec![]),
                )?;
                self.state = State::CreatingStream;
            }
            ("_error", State::Connecting) if c.transaction_id == TXN_CONNECT => {
                self.fail("connect", &c)
            }
            ("_result", State::CreatingStream) if c.transaction_id == TXN_CREATE_STREAM => {
                let id = c.arg(0).and_then(Value::as_f64).unwrap_or(-1.0);
                if !(1.0..=u32::MAX as f64).contains(&id) {
                    return Err(Error::Protocol("invalid stream id from createStream"));
                }
                let sid = id as u32;
                let key = self.stream_key().expose().to_owned();
                match self.config.mode.clone() {
                    ClientMode::Publish { publish_type, .. } => {
                        let txn = self.txn();
                        self.conn.send_command(
                            csid::STREAM_COMMAND,
                            sid,
                            &Command::new(
                                "publish",
                                txn,
                                Value::Null,
                                vec![Value::string(key), Value::string(publish_type)],
                            ),
                        )?;
                    }
                    ClientMode::Play { start, .. } => {
                        let txn = self.txn();
                        self.conn.send_command(
                            csid::STREAM_COMMAND,
                            sid,
                            &Command::new(
                                "play",
                                txn,
                                Value::Null,
                                vec![Value::string(key), Value::Number(start)],
                            ),
                        )?;
                        self.conn.send_user_control(UserControl::SetBufferLength {
                            stream_id: sid,
                            millis: self.config.buffer_length_ms,
                        })?;
                    }
                }
                self.state = State::Starting { stream_id: sid };
            }
            ("_error", State::CreatingStream) if c.transaction_id == TXN_CREATE_STREAM => {
                self.fail("createStream", &c)
            }
            ("onStatus", _) => {
                let info = c.arg(0);
                let level = info
                    .and_then(|v| v.get_str("level"))
                    .unwrap_or("")
                    .to_owned();
                let code = info_code(&c).unwrap_or("").to_owned();
                self.events.push_back(ClientEvent::Status {
                    stream_id,
                    level: level.clone(),
                    code: code.clone(),
                    description: Secret::new(
                        info.and_then(|v| v.get_str("description")).unwrap_or(""),
                    ),
                });
                match self.state {
                    State::Starting { stream_id: sid } if sid == stream_id => {
                        if level == "error" {
                            self.fail("onStatus", &c);
                        } else if code == "NetStream.Publish.Start"
                            && matches!(self.config.mode, ClientMode::Publish { .. })
                        {
                            self.state = State::Active { stream_id: sid };
                            self.events
                                .push_back(ClientEvent::PublishStarted { stream_id: sid });
                        } else if code == "NetStream.Play.Start"
                            && matches!(self.config.mode, ClientMode::Play { .. })
                        {
                            self.state = State::Active { stream_id: sid };
                            self.events
                                .push_back(ClientEvent::PlayStarted { stream_id: sid });
                        }
                    }
                    State::Active { stream_id: sid } if sid == stream_id => {
                        if level == "error" {
                            self.fail("onStatus", &c);
                        } else if matches!(
                            code.as_str(),
                            "NetStream.Play.Stop"
                                | "NetStream.Play.UnpublishNotify"
                                | "NetStream.Play.Complete"
                        ) {
                            self.events
                                .push_back(ClientEvent::StreamEnded { stream_id: sid });
                        }
                    }
                    _ => {}
                }
            }
            ("_error", _) => self.fail("call", &c),
            // onFCPublish・onBWDone・その他の _result などは使わない。
            _ => {}
        }
        Ok(())
    }

    fn publishing_stream(&self) -> Result<u32, Error> {
        match (self.active_stream(), &self.config.mode) {
            (Some(sid), ClientMode::Publish { .. }) => Ok(sid),
            _ => Err(Error::InvalidState("not publishing")),
        }
    }

    /// publish 中に音声・映像を送る。payload は FLV タグ本体（部品の連結）。
    pub fn send_media(
        &mut self,
        media_type: MediaType,
        timestamp: u32,
        parts: &[&[u8]],
    ) -> Result<(), Error> {
        let sid = self.publishing_stream()?;
        self.conn.send_parts(
            media_type.csid(),
            media_type.type_id(),
            sid,
            timestamp,
            parts,
        )
    }

    /// publish 中に onMetaData などを送る。`script_body` は FLV の script タグ本体
    /// （`"onMetaData", {...}`）で、`@setDataFrame` を前に付けて送る。
    pub fn send_metadata(&mut self, timestamp: u32, script_body: &[u8]) -> Result<(), Error> {
        let sid = self.publishing_stream()?;
        let mut prefix = Vec::new();
        senca_amf0::encode(&Value::string("@setDataFrame"), &mut prefix)?;
        self.conn.send_data(sid, timestamp, &[&prefix, script_body])
    }

    /// Ping Request を送る。
    pub fn ping(&mut self, timestamp: u32) -> Result<(), Error> {
        self.conn
            .send_user_control(UserControl::PingRequest(timestamp))
    }

    /// 終わりを知らせる（publish なら FCUnpublish、どちらも deleteStream）。出力を送ったら接続を閉じてよい。
    pub fn close(&mut self) -> Result<(), Error> {
        let sid = match self.state {
            State::Active { stream_id } | State::Starting { stream_id } => Some(stream_id),
            _ => None,
        };
        if let Some(sid) = sid {
            if matches!(self.config.mode, ClientMode::Publish { .. }) {
                let key = self.stream_key().expose().to_owned();
                let txn = self.txn();
                self.conn.send_command(
                    csid::COMMAND,
                    0,
                    &Command::new("FCUnpublish", txn, Value::Null, vec![Value::string(key)]),
                )?;
            }
            let txn = self.txn();
            self.conn.send_command(
                csid::COMMAND,
                0,
                &Command::new(
                    "deleteStream",
                    txn,
                    Value::Null,
                    vec![Value::Number(sid as f64)],
                ),
            )?;
        }
        self.state = State::Closed;
        Ok(())
    }
}
