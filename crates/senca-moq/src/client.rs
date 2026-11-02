//! MoQ の client（native QUIC か WebTransport）。sans-io。

use std::net::SocketAddr;
use std::time::{Instant, SystemTime};

use bytes::BytesMut;
use senca_h3::{H3Config, H3Connection, H3Event, Headers};
use senca_quic::{ClientConfig, ConnectError, ConnectionHandle, Endpoint, StreamId, Transmit};

use crate::session::{Session, SessionConfig};
use crate::transport::{drive_quic, drive_webtransport};
use crate::WT_PROTOCOL;

enum Mode {
    Native,
    Wt {
        h3: Box<H3Connection>,
        authority: String,
        path: String,
        session: Option<StreamId>,
        rejected: Option<u16>,
    },
}

/// MoQ の client。QUIC の endpoint・接続と MoQ のセッションを持つ。
pub struct MoqClient {
    pub endpoint: Endpoint,
    pub conn: ConnectionHandle,
    pub session: Session,
    mode: Mode,
}

impl std::fmt::Debug for MoqClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MoqClient").field("session", &self.session).finish_non_exhaustive()
    }
}

impl MoqClient {
    /// native QUIC（ALPN `moqt-22`、`quic.tls` の ALPN を設定しておくこと）。
    pub fn connect_native(
        mut endpoint: Endpoint,
        quic: ClientConfig,
        remote: SocketAddr,
        server_name: &str,
        session: SessionConfig,
        now: Instant,
        wall: SystemTime,
    ) -> Result<Self, ConnectError> {
        let conn = endpoint.connect(now, wall, quic, remote, server_name)?;
        Ok(Self {
            endpoint,
            conn,
            session: Session::new(session, now),
            mode: Mode::Native,
        })
    }

    /// WebTransport（ALPN `h3`、`https://authority/path` への拡張 CONNECT）。
    #[allow(clippy::too_many_arguments)]
    pub fn connect_webtransport(
        mut endpoint: Endpoint,
        quic: ClientConfig,
        remote: SocketAddr,
        server_name: &str,
        authority: &str,
        path: &str,
        mut session: SessionConfig,
        now: Instant,
        wall: SystemTime,
    ) -> Result<Self, ConnectError> {
        let conn = endpoint.connect(now, wall, quic, remote, server_name)?;
        session.native_quic = false;
        Ok(Self {
            endpoint,
            conn,
            session: Session::new(session, now),
            mode: Mode::Wt {
                h3: Box::new(H3Connection::new(senca_quic::Side::Client, H3Config::default())),
                authority: authority.to_string(),
                path: path.to_string(),
                session: None,
                rejected: None,
            },
        })
    }

    /// WebTransport の session が拒否された（HTTP の status）。
    pub fn rejected(&self) -> Option<u16> {
        match &self.mode {
            Mode::Wt { rejected, .. } => *rejected,
            Mode::Native => None,
        }
    }

    pub fn handle_datagram(&mut self, now: Instant, wall: SystemTime, from: SocketAddr, data: BytesMut) {
        self.endpoint.handle_datagram(now, wall, from, data);
        self.drive(now);
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        self.endpoint.handle_timeout(now);
        self.drive(now);
    }

    pub fn poll_transmit(&mut self, now: Instant) -> Option<Transmit> {
        self.endpoint.poll_transmit(now)
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        self.endpoint.poll_timeout()
    }

    /// 接続が終わった。
    pub fn is_closed(&self) -> bool {
        self.endpoint.connection(self.conn).is_none_or(|c| c.is_closed())
    }

    /// 進める（アプリケーションの操作の後にも呼ぶ）。
    pub fn drive(&mut self, now: Instant) {
        let Some(conn) = self.endpoint.connection_mut(self.conn) else {
            if !self.session.is_closed() {
                self.session.on_transport_closed("connection gone".into());
            }
            return;
        };
        match &mut self.mode {
            Mode::Native => {
                if conn.is_established() || conn.is_closed() {
                    drive_quic(&mut self.session, conn, now);
                }
            }
            Mode::Wt {
                h3,
                authority,
                path,
                session,
                rejected,
            } => {
                h3.process(conn, now);
                let mut evs = Vec::new();
                while let Some(e) = h3.poll_event() {
                    evs.push(e);
                }
                if session.is_none() && rejected.is_none() && conn.is_established() {
                    let extra = Headers::new().with("wt-available-protocols", format!("\"{WT_PROTOCOL}\""));
                    if let Ok(s) = h3.wt_connect(conn, authority, path, extra) {
                        *session = Some(s);
                    }
                }
                let mut established = false;
                for e in &evs {
                    match e {
                        H3Event::WebTransportEstablished { .. } => established = true,
                        H3Event::WebTransportRejected { status, .. } => {
                            *rejected = Some(*status);
                            self.session.on_transport_closed(format!("WebTransport rejected: {status}"));
                        }
                        _ => {}
                    }
                }
                let _ = established;
                if let Some(s) = *session
                    && h3.wt_sessions().contains(&s)
                {
                    drive_webtransport(&mut self.session, h3, conn, s, &evs, now);
                } else if let Some(s) = *session {
                    // 確立前でも、閉じた・接続断は伝える
                    for e in &evs {
                        match e {
                            H3Event::WebTransportClosed { session: x, .. } if *x == s => {
                                self.session.on_transport_closed("WebTransport session closed".into())
                            }
                            H3Event::ConnectionClosed(err) => self.session.on_transport_closed(format!("{err}")),
                            _ => {}
                        }
                    }
                }
                h3.process(conn, now);
            }
        }
    }
}
