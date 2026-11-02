//! MoQ のセッションと下位の transport（native QUIC・WebTransport）のつなぎ。
//!
//! [`Session`](crate::Session) は stream の番号（QUIC の stream ID）だけを扱い、書き込みは
//! [`Transport`] を通す。受信は [`drive_quic`]・[`drive_webtransport`] が下位のイベントを読んで
//! セッションへ渡す。

use std::time::Instant;

use bytes::Bytes;
use senca_h3::{H3Connection, H3Event, WtStreamKind};
use senca_quic::{Connection, Dir, Event, StreamError, StreamId};

use crate::Session;

/// stream が無い・閉じている。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamGone;

/// 書き込みの側の transport。
pub trait Transport {
    fn open_bi(&mut self) -> Option<u64>;
    fn open_uni(&mut self) -> Option<u64>;
    /// 受け付けたバイト数（stream が無ければ `Err`）。
    fn write(&mut self, stream: u64, data: Bytes) -> Result<usize, StreamGone>;
    fn finish(&mut self, stream: u64);
    fn reset(&mut self, stream: u64, code: u64);
    fn stop(&mut self, stream: u64, code: u64);
    /// 大きいほど先に送る。
    fn set_priority(&mut self, stream: u64, priority: i32);
    fn send_datagram(&mut self, data: Bytes) -> bool;
    fn max_datagram_size(&self) -> Option<usize>;
    /// 送信側で ACK されていないバイト数（stream が終わっていれば `None`）。
    fn unacked(&self, stream: u64) -> Option<u64>;
    /// セッションを閉じる（native QUIC は CONNECTION_CLOSE、WebTransport は CLOSE_WEBTRANSPORT_SESSION）。
    fn close(&mut self, code: u64, reason: &str);
}

/// native QUIC（ALPN `moqt-22`）。
pub struct QuicTransport<'a> {
    pub conn: &'a mut Connection,
    pub now: Instant,
}

impl Transport for QuicTransport<'_> {
    fn open_bi(&mut self) -> Option<u64> {
        self.conn.open_stream(Dir::Bi).map(|s| s.0)
    }
    fn open_uni(&mut self) -> Option<u64> {
        self.conn.open_stream(Dir::Uni).map(|s| s.0)
    }
    fn write(&mut self, stream: u64, data: Bytes) -> Result<usize, StreamGone> {
        self.conn.stream_send_bytes(StreamId(stream), data).map_err(|_| StreamGone)
    }
    fn finish(&mut self, stream: u64) {
        let _ = self.conn.stream_finish(StreamId(stream));
    }
    fn reset(&mut self, stream: u64, code: u64) {
        let _ = self.conn.stream_reset(StreamId(stream), code);
    }
    fn stop(&mut self, stream: u64, code: u64) {
        let _ = self.conn.stream_stop(StreamId(stream), code);
    }
    fn set_priority(&mut self, stream: u64, priority: i32) {
        let _ = self.conn.set_stream_priority(StreamId(stream), priority);
    }
    fn send_datagram(&mut self, data: Bytes) -> bool {
        self.conn.send_datagram(data).is_ok()
    }
    fn max_datagram_size(&self) -> Option<usize> {
        self.conn.max_datagram_size()
    }
    fn unacked(&self, stream: u64) -> Option<u64> {
        self.conn.stream_send_buffered(StreamId(stream))
    }
    fn close(&mut self, code: u64, reason: &str) {
        self.conn.close(self.now, code, reason.as_bytes());
    }
}

/// QUIC のイベントを読んでセッションへ渡し、セッションの送信を書き出す（native QUIC）。
pub fn drive_quic(session: &mut Session, conn: &mut Connection, now: Instant) {
    feed_quic(session, conn, now);
    flush_quic(session, conn, now);
}

/// セッションの送信を QUIC に書く。
pub fn flush_quic(session: &mut Session, conn: &mut Connection, now: Instant) {
    let mut t = QuicTransport { conn, now };
    session.flush(&mut t, now);
}

/// QUIC のイベントを読んでセッションへ渡す（書き出しはしない。relay が間で処理するとき）。
pub fn feed_quic(session: &mut Session, conn: &mut Connection, now: Instant) {
    while let Some(ev) = conn.poll_event() {
        match ev {
            Event::StreamOpened { id } | Event::StreamReadable { id } => read_all_quic(session, conn, id, now),
            Event::StreamStopped { id, code } => session.on_stop_sending(id.0, code),
            Event::DatagramReceived => {
                while let Some(d) = conn.recv_datagram() {
                    session.on_datagram(d, now);
                }
            }
            Event::ConnectionLost(e) => session.on_transport_closed(format!("{e}")),
            Event::StreamWritable { .. } | Event::StreamFinished { .. } => {}
            _ => {}
        }
    }
    session.handle_timeout(now);
}

fn read_all_quic(session: &mut Session, conn: &mut Connection, id: StreamId, now: Instant) {
    loop {
        match conn.stream_recv(id, 64 * 1024) {
            Ok(c) => {
                if c.data.is_empty() && !c.fin {
                    return;
                }
                session.on_stream_data(id.0, id.dir() == Dir::Bi, c.data, c.fin, now);
                if c.fin {
                    return;
                }
            }
            Err(StreamError::Reset(code)) => {
                session.on_stream_reset(id.0, code);
                return;
            }
            Err(_) => return,
        }
    }
}

/// WebTransport（draft-02/07）の一つの session。
pub struct WtTransport<'a> {
    pub h3: &'a mut H3Connection,
    pub conn: &'a mut Connection,
    pub session: StreamId,
    pub now: Instant,
}

impl Transport for WtTransport<'_> {
    fn open_bi(&mut self) -> Option<u64> {
        self.h3.wt_open(self.conn, self.session, WtStreamKind::Bi).ok().map(|s| s.0)
    }
    fn open_uni(&mut self) -> Option<u64> {
        self.h3.wt_open(self.conn, self.session, WtStreamKind::Uni).ok().map(|s| s.0)
    }
    fn write(&mut self, stream: u64, data: Bytes) -> Result<usize, StreamGone> {
        self.h3.wt_send(self.conn, StreamId(stream), data).map_err(|_| StreamGone)
    }
    fn finish(&mut self, stream: u64) {
        let _ = self.h3.wt_finish(self.conn, StreamId(stream));
    }
    fn reset(&mut self, stream: u64, code: u64) {
        let _ = self.h3.wt_reset(self.conn, StreamId(stream), code.min(u64::from(u32::MAX)) as u32);
    }
    fn stop(&mut self, stream: u64, code: u64) {
        let _ = self.h3.wt_stop(self.conn, StreamId(stream), code.min(u64::from(u32::MAX)) as u32);
    }
    fn set_priority(&mut self, stream: u64, priority: i32) {
        let _ = self.conn.set_stream_priority(StreamId(stream), priority);
    }
    fn send_datagram(&mut self, data: Bytes) -> bool {
        self.h3.wt_send_datagram(self.conn, self.session, &data).is_ok()
    }
    fn max_datagram_size(&self) -> Option<usize> {
        self.h3.wt_max_datagram_size(self.conn, self.session)
    }
    fn unacked(&self, stream: u64) -> Option<u64> {
        self.conn.stream_send_buffered(StreamId(stream))
    }
    fn close(&mut self, code: u64, reason: &str) {
        self.h3.wt_close(self.conn, self.session, code.min(u64::from(u32::MAX)) as u32, reason);
    }
}

/// WebTransport の session のイベントをセッションへ渡し、送信を書き出す。
pub fn drive_webtransport(
    session: &mut Session,
    h3: &mut H3Connection,
    conn: &mut Connection,
    wt_session: StreamId,
    events: &[H3Event],
    now: Instant,
) {
    feed_webtransport(session, h3, conn, wt_session, events, now);
    flush_webtransport(session, h3, conn, wt_session, now);
}

/// セッションの送信を WebTransport の session に書く。
pub fn flush_webtransport(session: &mut Session, h3: &mut H3Connection, conn: &mut Connection, wt_session: StreamId, now: Instant) {
    let mut t = WtTransport {
        h3,
        conn,
        session: wt_session,
        now,
    };
    session.flush(&mut t, now);
}

/// WebTransport の session のイベントをセッションへ渡す。`events` は [`H3Connection::poll_event`] で
/// 読んだもの（この session に関係しないものは無視する）。
pub fn feed_webtransport(
    session: &mut Session,
    h3: &mut H3Connection,
    conn: &mut Connection,
    wt_session: StreamId,
    events: &[H3Event],
    now: Instant,
) {
    for ev in events {
        match ev {
            H3Event::WebTransportStream { session: s, stream, .. } | H3Event::WebTransportReadable { session: s, stream }
                if *s == wt_session =>
            {
                loop {
                    match h3.wt_recv(conn, *stream, 64 * 1024) {
                        Ok(c) => {
                            if c.data.is_empty() && !c.fin {
                                break;
                            }
                            session.on_stream_data(stream.0, stream.dir() == Dir::Bi, c.data, c.fin, now);
                            if c.fin {
                                break;
                            }
                        }
                        Err(senca_h3::ApiError::Stream(StreamError::Reset(code))) => {
                            session.on_stream_reset(stream.0, code);
                            break;
                        }
                        Err(_) => break,
                    }
                }
            }
            H3Event::WebTransportStopped { session: s, stream, code } if *s == wt_session => {
                session.on_stop_sending(stream.0, code.map_or(0, u64::from));
            }
            H3Event::WebTransportDatagram { session: s, data } if *s == wt_session => session.on_datagram(data.clone(), now),
            H3Event::WebTransportClosed { session: s, code, reason } if *s == wt_session => {
                session.on_transport_closed(format!("WebTransport session closed: {code} {reason}"));
            }
            H3Event::ConnectionClosed(e) => session.on_transport_closed(format!("{e}")),
            _ => {}
        }
    }
    session.handle_timeout(now);
}
