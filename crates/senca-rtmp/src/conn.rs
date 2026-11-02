//! サーバー・クライアント共通の部分（chunk の入出力、protocol control、受信確認）。

use crate::Error;
use crate::chunk::{ChunkDecoder, ChunkEncoder, ChunkLimits, Message};
use crate::message::{
    Command, MediaMessage, MediaType, UserControl, csid, data_name, parse_u32, split_aggregate,
    type_id,
};
use bytes::{Bytes, BytesMut};
use senca_amf0::{Limits, Value};

/// 上位（セッション）へ渡す message。
#[derive(Debug)]
pub(crate) enum Incoming {
    Command {
        stream_id: u32,
        command: Command,
    },
    Media {
        stream_id: u32,
        message: MediaMessage,
    },
    /// AMF0 data message。`name` は先頭の文字列、`payload` は名前を含む全体。
    Data {
        stream_id: u32,
        timestamp: u32,
        name: String,
        name_end: usize,
        payload: Bytes,
    },
    UserControl(UserControl),
    PeerBandwidth(u32),
}

pub(crate) struct Conn {
    pub inbuf: BytesMut,
    pub out: BytesMut,
    pub dec: ChunkDecoder,
    pub enc: ChunkEncoder,
    pub amf_limits: Limits,
    bytes_in: u64,
    last_ack: u64,
    /// 相手が求めた受信確認の間隔（Window Acknowledgement Size）。0 なら送らない。
    ack_window: u32,
    /// 自分が最後に相手に知らせた Window Acknowledgement Size。
    pub sent_window: Option<u32>,
}

impl Conn {
    pub fn new(limits: ChunkLimits, amf_limits: Limits) -> Self {
        Conn {
            inbuf: BytesMut::new(),
            out: BytesMut::new(),
            dec: ChunkDecoder::new(limits),
            enc: ChunkEncoder::new(),
            amf_limits,
            bytes_in: 0,
            last_ack: 0,
            ack_window: 0,
            sent_window: None,
        }
    }

    /// 受け取ったバイト数を数え、必要なら Acknowledgement を送る。
    pub fn count_in(&mut self, n: usize) -> Result<(), Error> {
        self.bytes_in += n as u64;
        if self.ack_window > 0 && self.bytes_in - self.last_ack >= self.ack_window as u64 {
            self.last_ack = self.bytes_in;
            let seq = (self.bytes_in & 0xFFFF_FFFF) as u32;
            self.send_raw(
                csid::CONTROL,
                type_id::ACKNOWLEDGEMENT,
                0,
                0,
                &seq.to_be_bytes(),
            )?;
        }
        Ok(())
    }

    pub fn send_raw(
        &mut self,
        cs: u32,
        t: u8,
        stream_id: u32,
        timestamp: u32,
        payload: &[u8],
    ) -> Result<(), Error> {
        self.enc
            .write(&mut self.out, cs, timestamp, t, stream_id, &[payload])
    }

    pub fn send_parts(
        &mut self,
        cs: u32,
        t: u8,
        stream_id: u32,
        timestamp: u32,
        parts: &[&[u8]],
    ) -> Result<(), Error> {
        self.enc
            .write(&mut self.out, cs, timestamp, t, stream_id, parts)
    }

    pub fn send_command(&mut self, cs: u32, stream_id: u32, cmd: &Command) -> Result<(), Error> {
        let b = cmd.encode()?;
        self.send_raw(cs, type_id::COMMAND_AMF0, stream_id, 0, &b)
    }

    pub fn send_user_control(&mut self, u: UserControl) -> Result<(), Error> {
        self.send_raw(csid::CONTROL, type_id::USER_CONTROL, 0, 0, &u.encode())
    }

    /// 自分の送る chunk size を変える（Set Chunk Size を送ってから切り替える）。
    pub fn set_out_chunk_size(&mut self, size: u32) -> Result<(), Error> {
        let size = size.clamp(1, crate::chunk::MAX_CHUNK_SIZE);
        self.send_raw(
            csid::CONTROL,
            type_id::SET_CHUNK_SIZE,
            0,
            0,
            &size.to_be_bytes(),
        )?;
        self.enc.set_chunk_size(size);
        Ok(())
    }

    pub fn send_window_ack_size(&mut self, size: u32) -> Result<(), Error> {
        self.sent_window = Some(size);
        self.send_raw(
            csid::CONTROL,
            type_id::WINDOW_ACK_SIZE,
            0,
            0,
            &size.to_be_bytes(),
        )
    }

    pub fn send_peer_bandwidth(&mut self, size: u32, limit_type: u8) -> Result<(), Error> {
        let mut b = size.to_be_bytes().to_vec();
        b.push(limit_type);
        self.send_raw(csid::CONTROL, type_id::SET_PEER_BANDWIDTH, 0, 0, &b)
    }

    /// AMF0 data message を送る（`payload` は名前から始まる AMF0 の列）。
    pub fn send_data(
        &mut self,
        stream_id: u32,
        timestamp: u32,
        parts: &[&[u8]],
    ) -> Result<(), Error> {
        self.send_parts(csid::DATA, type_id::DATA_AMF0, stream_id, timestamp, parts)
    }

    /// 次の message を 1 つ取り出す。
    pub fn next_message(&mut self) -> Result<Option<Message>, Error> {
        self.dec.decode(&mut self.inbuf)
    }

    /// message を解釈する。protocol control はここで処理し、上位に渡すものを `out` に積む。
    pub fn handle(&mut self, m: Message, out: &mut Vec<Incoming>) -> Result<(), Error> {
        match m.type_id {
            type_id::SET_CHUNK_SIZE => {
                let v = parse_u32(&m.payload)?;
                if v & 0x8000_0000 != 0 {
                    return Err(Error::Protocol("invalid chunk size"));
                }
                self.dec.set_chunk_size(v)?;
            }
            type_id::ABORT => self.dec.abort(parse_u32(&m.payload)?),
            type_id::ACKNOWLEDGEMENT => {
                parse_u32(&m.payload)?;
            }
            type_id::WINDOW_ACK_SIZE => {
                self.ack_window = parse_u32(&m.payload)?;
            }
            type_id::SET_PEER_BANDWIDTH => {
                let v = parse_u32(&m.payload)?;
                out.push(Incoming::PeerBandwidth(v));
            }
            type_id::USER_CONTROL => {
                out.push(Incoming::UserControl(UserControl::parse(&m.payload)?))
            }
            type_id::AUDIO | type_id::VIDEO => {
                let media_type = if m.type_id == type_id::AUDIO {
                    MediaType::Audio
                } else {
                    MediaType::Video
                };
                // 空の音声・映像 message は捨てる（一部の実装が送る）。
                if !m.payload.is_empty() {
                    out.push(Incoming::Media {
                        stream_id: m.stream_id,
                        message: MediaMessage {
                            media_type,
                            timestamp: m.timestamp,
                            payload: m.payload,
                        },
                    });
                }
            }
            type_id::COMMAND_AMF0 | type_id::COMMAND_AMF3 => {
                let p = if m.type_id == type_id::COMMAND_AMF3 {
                    // AMF3 command は先頭 1 バイトが 0 で、残りは AMF0。
                    match m.payload.first() {
                        Some(0) => &m.payload[1..],
                        _ => return Err(Error::Protocol("unsupported AMF3 command")),
                    }
                } else {
                    &m.payload[..]
                };
                let command = Command::parse(p, self.amf_limits)?;
                out.push(Incoming::Command {
                    stream_id: m.stream_id,
                    command,
                });
            }
            type_id::DATA_AMF0 | type_id::DATA_AMF3 => {
                let payload = if m.type_id == type_id::DATA_AMF3 && m.payload.first() == Some(&0) {
                    m.payload.slice(1..)
                } else {
                    m.payload
                };
                // 中身が AMF0 として正しいかを上限付きで確かめる。
                senca_amf0::Decoder::with_limits(&payload, self.amf_limits)?.read_all()?;
                if let Some((name, name_end)) = data_name(&payload) {
                    out.push(Incoming::Data {
                        stream_id: m.stream_id,
                        timestamp: m.timestamp,
                        name,
                        name_end,
                        payload,
                    });
                }
            }
            type_id::AGGREGATE => {
                for (t, ts, body) in split_aggregate(m.timestamp, &m.payload)? {
                    let sub = Message {
                        csid: m.csid,
                        timestamp: ts,
                        type_id: t,
                        stream_id: m.stream_id,
                        payload: body,
                    };
                    match t {
                        type_id::AUDIO | type_id::VIDEO | type_id::DATA_AMF0 => {
                            self.handle(sub, out)?
                        }
                        _ => return Err(Error::Protocol("unexpected message in aggregate")),
                    }
                }
            }
            // shared object などは使わないので捨てる。
            _ => {}
        }
        Ok(())
    }
}

/// command の情報 object から code を取る。
pub(crate) fn info_code(cmd: &Command) -> Option<&str> {
    cmd.arg(0).and_then(|v: &Value| v.get_str("code"))
}
