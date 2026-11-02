//! SRT caller（live mode、payload は MPEG-TS、仕様 6.8）。
//!
//! AU を `senca-mpegts` の [`TsMuxer`] で TS にし、188 × 7 = 1316 バイトずつ SRT のメッセージにして送る。
//! AU の終わりでは 7 パケットに満たなくても送る（待たせない）。

use super::{Connector, Transport, TransportError, TransportErrorKind};
use crate::uri::Secret;
use senca_core::{AccessUnit, CodecConfig};
use senca_mpegts::{MuxerConfig, TsMuxer};
use senca_srt::{
    CloseReason, Config, Connection, Event, KeyLen, Passphrase, RejectReason, SendError,
};
use std::io::ErrorKind;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

const CHUNK: usize = 1316;
/// 送信バッファが空かないまま待つ上限。
const STALL_LIMIT: Duration = Duration::from_secs(5);

/// SRT の送り先。
pub struct SrtConnector {
    host: String,
    port: u16,
    cfg: Config,
}

impl SrtConnector {
    pub fn new(
        host: &str,
        port: u16,
        stream_id: Option<&Secret>,
        passphrase: Option<&Secret>,
        latency_ms: Option<u16>,
        pbkeylen: Option<u8>,
    ) -> Result<Self, TransportError> {
        let bad = |m: &str| TransportError::new(TransportErrorKind::Unsupported, m.to_owned());
        let mut cfg = Config {
            stream_id: stream_id.map(|s| s.expose().to_owned()),
            ..Config::default()
        };
        if let Some(l) = latency_ms {
            cfg.latency_ms = l;
            cfg.peer_latency_ms = l;
        }
        if let Some(p) = passphrase {
            cfg.passphrase = Some(
                Passphrase::new(p.expose())
                    .map_err(|_| bad("srt: passphrase must be 10..79 bytes"))?,
            );
        }
        if let Some(k) = pbkeylen {
            cfg.key_len = KeyLen::from_bytes(k as usize)
                .ok_or_else(|| bad("srt: pbkeylen must be 16, 24 or 32"))?;
        }
        Ok(SrtConnector {
            host: host.to_owned(),
            port,
            cfg,
        })
    }
}

fn now_us(start: Instant) -> u64 {
    start.elapsed().as_micros() as u64 + 1
}

fn close_error(reason: CloseReason) -> TransportError {
    match reason {
        CloseReason::Rejected(r)
            if [
                RejectReason::BADSECRET,
                RejectReason::UNSECURE,
                RejectReason::X_BAD_REQUEST,
                RejectReason::X_UNAUTHORIZED,
                RejectReason::X_FORBIDDEN,
                RejectReason::X_NOTFOUND,
                RejectReason::X_BAD_MODE,
            ]
            .contains(&r) =>
        {
            TransportError::new(TransportErrorKind::Rejected, format!("srt: rejected ({r})"))
        }
        CloseReason::Rejected(r) => TransportError::network(format!("srt: rejected ({r})")),
        other => TransportError::network(format!("srt: closed ({other:?})")),
    }
}

/// UDP ソケットと SRT の接続。
struct Link {
    sock: UdpSocket,
    conn: Connection,
    start: Instant,
    buf: Vec<u8>,
}

impl Link {
    /// 届いたものの処理・タイマー・送信。待たない。閉じたら Err。
    fn service(&mut self) -> Result<(), TransportError> {
        loop {
            match self.sock.recv_from(&mut self.buf) {
                Ok((n, from)) => {
                    let now = now_us(self.start);
                    self.conn.handle_datagram(now, from, &self.buf[..n]);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) =>
                {
                    break;
                }
                // ICMP の到達不能など。相手の無応答として時間切れに任せる
                Err(_) => break,
            }
        }
        self.conn.handle_timeout(now_us(self.start));
        while let Some((to, p)) = self.conn.poll_transmit() {
            let _ = self.sock.send_to(&p, to);
        }
        while let Some(ev) = self.conn.poll_event() {
            if let Event::Closed { reason } = ev {
                return Err(close_error(reason));
            }
        }
        if self.conn.is_closed() {
            return Err(TransportError::network("srt: closed"));
        }
        Ok(())
    }

    /// 次のタイマーまで（最長 `max`）届くのを待つ。
    fn wait(&mut self, max: Duration) {
        let now = now_us(self.start);
        let d = self
            .conn
            .poll_timeout()
            .map_or(max, |t| Duration::from_micros(t.saturating_sub(now)))
            .clamp(Duration::from_micros(200), max);
        let _ = self.sock.set_nonblocking(false);
        let _ = self.sock.set_read_timeout(Some(d));
        if let Ok((n, from)) = self.sock.recv_from(&mut self.buf) {
            let now = now_us(self.start);
            self.conn.handle_datagram(now, from, &self.buf[..n]);
        }
        let _ = self.sock.set_nonblocking(true);
    }
}

impl Connector for SrtConnector {
    fn connect(&mut self, timeout: Duration) -> Result<Box<dyn Transport>, TransportError> {
        let peer: SocketAddr = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|e| TransportError::network(format!("resolve {}: {e}", self.host)))?
            .next()
            .ok_or_else(|| TransportError::network(format!("resolve {}: no address", self.host)))?;
        let bind: SocketAddr = if peer.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        }
        .parse()
        .map_err(TransportError::network)?;
        let sock = UdpSocket::bind(bind).map_err(TransportError::network)?;
        sock.set_nonblocking(true)
            .map_err(TransportError::network)?;
        let start = Instant::now();
        let cfg = Config {
            connect_timeout_ms: timeout.as_millis().clamp(100, 60_000) as u64,
            ..self.cfg.clone()
        };
        let conn = Connection::connect(cfg, peer, now_us(start)).map_err(|e| {
            TransportError::new(TransportErrorKind::Unsupported, format!("srt: {e}"))
        })?;
        let mut link = Link {
            sock,
            conn,
            start,
            buf: vec![0u8; 2048],
        };
        loop {
            link.conn.handle_timeout(now_us(link.start));
            while let Some((to, p)) = link.conn.poll_transmit() {
                let _ = link.sock.send_to(&p, to);
            }
            while let Some(ev) = link.conn.poll_event() {
                match ev {
                    Event::Connected(_) => {
                        return Ok(Box::new(SrtTransport {
                            link,
                            mux: TsMuxer::new(MuxerConfig::default()),
                            ts: Vec::new(),
                        }));
                    }
                    Event::Closed { reason } => return Err(close_error(reason)),
                    Event::Data { .. } => {}
                }
            }
            link.wait(Duration::from_millis(50));
        }
    }
}

/// つながった SRT。
pub struct SrtTransport {
    link: Link,
    mux: TsMuxer,
    ts: Vec<u8>,
}

impl SrtTransport {
    fn send_chunk(&mut self, len: usize) -> Result<(), TransportError> {
        let since = Instant::now();
        loop {
            let now = now_us(self.link.start);
            match self.link.conn.send(&self.ts[..len], now) {
                Ok(()) => {
                    self.ts.drain(..len);
                    return Ok(());
                }
                Err(SendError::BufferFull) => {
                    // 相手の ACK を待つ（この間に入ったフレームは送信キューが捨てる）
                    self.link.service()?;
                    if since.elapsed() > STALL_LIMIT {
                        return Err(TransportError::network("srt: send buffer stalled"));
                    }
                    self.link.wait(Duration::from_millis(5));
                }
                Err(SendError::NotConnected) => {
                    return Err(TransportError::network("srt: not connected"));
                }
                Err(e) => {
                    return Err(TransportError::new(
                        TransportErrorKind::Protocol,
                        format!("srt: {e}"),
                    ));
                }
            }
        }
    }
}

impl Transport for SrtTransport {
    fn set_config(&mut self, config: &CodecConfig) -> Result<(), TransportError> {
        let track = match config.codec.kind() {
            senca_core::MediaKind::Video => crate::frame::VIDEO_TRACK,
            _ => crate::frame::AUDIO_TRACK,
        };
        let r = if self.mux.pid(track).is_some() {
            self.mux.update_config(track, config)
        } else {
            self.mux.add_track(track, config).map(|_| ())
        };
        r.map_err(|e| TransportError::new(TransportErrorKind::Unsupported, format!("mpeg-ts: {e}")))
    }

    fn send(&mut self, au: &AccessUnit) -> Result<(), TransportError> {
        self.mux.push(au, &mut self.ts).map_err(|e| {
            TransportError::new(TransportErrorKind::Protocol, format!("mpeg-ts: {e}"))
        })?;
        while !self.ts.is_empty() {
            let len = self.ts.len().min(CHUNK);
            self.send_chunk(len)?;
        }
        self.link.service()
    }

    fn poll(&mut self) -> Result<(), TransportError> {
        self.link.service()
    }

    fn poll_interval(&self) -> Duration {
        let now = now_us(self.link.start);
        self.link
            .conn
            .poll_timeout()
            .map_or(Duration::from_millis(10), |t| {
                Duration::from_micros(t.saturating_sub(now))
            })
            .clamp(Duration::from_micros(500), Duration::from_millis(10))
    }

    fn rtt_us(&self) -> Option<u64> {
        Some(self.link.conn.stats().rtt_us)
    }

    fn bytes_sent(&self) -> u64 {
        self.link.conn.stats().bytes_sent
    }

    fn close(&mut self, linger: Duration) {
        let deadline = Instant::now() + linger;
        self.link.conn.close(now_us(self.link.start));
        while Instant::now() < deadline {
            if self.link.service().is_err() || self.link.conn.is_closed() {
                break;
            }
            self.link.wait(Duration::from_millis(20));
        }
        // 時間内に閉じ終わらなければ SHUTDOWN を送らずに捨てる（相手は無応答で閉じる）
    }
}
