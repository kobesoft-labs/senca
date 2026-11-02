//! RTMP / RTMPS の publish（仕様 6.7）。
//!
//! H.264・AAC は従来の FLV、H.265・Opus は Enhanced RTMP（FourCC）で送る（`senca-flv` の [`FlvMuxer`]）。
//! RTMPS は [`TlsConnector`] が渡されたときだけ使える。無ければ `Unsupported` で止める（平文へ黙って落とさない）。

use super::{ByteStream, Connector, TlsConnector, Transport, TransportError, TransportErrorKind};
use crate::uri::{Secret, SimpleUrl};
use senca_core::{AccessUnit, CodecConfig};
use senca_flv::{FlvMuxer, OutTag, TagType};
use senca_rtmp::{ClientConfig, ClientEvent, ClientMode, ClientSession, MediaType};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// RTMP の送り先。
pub struct RtmpConnector {
    url: SimpleUrl,
    tc_url: String,
    key: Secret,
    tls: Option<Arc<dyn TlsConnector>>,
}

impl RtmpConnector {
    /// `url` は `rtmp(s)://host[:port]/app[?query]`（key を含まない）。
    pub fn new(
        url: &str,
        key: Secret,
        tls: Option<Arc<dyn TlsConnector>>,
    ) -> Result<Self, TransportError> {
        let u = SimpleUrl::parse("url", url)
            .map_err(|e| TransportError::new(TransportErrorKind::Unsupported, e.to_string()))?;
        if u.scheme == "rtmps" && tls.is_none() {
            return Err(TransportError::new(
                TransportErrorKind::Unsupported,
                "RTMPS needs a TLS provider (not built into this SDK build)",
            ));
        }
        if u.scheme != "rtmp" && u.scheme != "rtmps" {
            return Err(TransportError::new(
                TransportErrorKind::Unsupported,
                "not an rtmp:// URL",
            ));
        }
        Ok(RtmpConnector {
            tc_url: u.to_url(),
            url: u,
            key,
            tls,
        })
    }

    fn app(&self) -> String {
        let mut app = self.url.path.trim_start_matches('/').to_owned();
        if let Some(q) = &self.url.query {
            app.push('?');
            app.push_str(q);
        }
        app
    }

    fn resolve(&self) -> Result<Vec<SocketAddr>, TransportError> {
        let port = self.url.port.unwrap_or(if self.url.scheme == "rtmps" {
            443
        } else {
            1935
        });
        let v: Vec<SocketAddr> = (self.url.host.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| TransportError::network(format!("resolve {}: {e}", self.url.host)))?
            .collect();
        if v.is_empty() {
            return Err(TransportError::network(format!(
                "resolve {}: no address",
                self.url.host
            )));
        }
        Ok(v)
    }
}

impl Connector for RtmpConnector {
    fn connect(&mut self, timeout: Duration) -> Result<Box<dyn Transport>, TransportError> {
        let deadline = Instant::now() + timeout;
        let mut last = None;
        let mut tcp = None;
        for a in self.resolve()? {
            match TcpStream::connect_timeout(
                &a,
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
            ) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let tcp = tcp.ok_or_else(|| {
            TransportError::network(format!(
                "connect: {}",
                last.map_or("failed".into(), |e| e.to_string())
            ))
        })?;
        let _ = tcp.set_nodelay(true);
        let mut stream: Box<dyn ByteStream> = match (&self.tls, self.url.scheme.as_str()) {
            (Some(t), "rtmps") => t
                .connect(&self.url.host, tcp)
                .map_err(|e| TransportError::network(format!("tls: {e}")))?,
            _ => Box::new(tcp),
        };
        stream
            .set_write_timeout(Some(WRITE_TIMEOUT))
            .map_err(TransportError::network)?;
        let mut session = ClientSession::new(ClientConfig::new(
            &self.app(),
            &self.tc_url,
            ClientMode::Publish {
                stream_key: senca_rtmp::Secret::new(self.key.expose()),
                publish_type: "live".into(),
            },
        ));
        let mut sent = 0u64;
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let out = session.take_output();
            if !out.is_empty() {
                stream.write_all(&out).map_err(TransportError::network)?;
                sent += out.len() as u64;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(TransportError::network(
                    "rtmp: timed out waiting for publish to start",
                ));
            }
            stream
                .set_read_timeout(Some(left))
                .map_err(TransportError::network)?;
            let n = match stream.read(&mut buf) {
                Ok(0) => {
                    return Err(TransportError::network(
                        "rtmp: server closed the connection",
                    ));
                }
                Ok(n) => n,
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(TransportError::network(e)),
            };
            session
                .feed(&buf[..n])
                .map_err(|e| TransportError::new(TransportErrorKind::Protocol, e.to_string()))?;
            while let Some(ev) = session.poll_event() {
                match ev {
                    ClientEvent::PublishStarted { .. } => {
                        let out = session.take_output();
                        stream.write_all(&out).map_err(TransportError::network)?;
                        sent += out.len() as u64;
                        stream
                            .set_read_timeout(Some(Duration::from_millis(1)))
                            .map_err(TransportError::network)?;
                        return Ok(Box::new(RtmpTransport {
                            stream,
                            session,
                            muxer: FlvMuxer::new(),
                            tags: Vec::new(),
                            bytes_sent: sent,
                            buf,
                            last_ts: 0,
                        }));
                    }
                    ClientEvent::Rejected { command, code, .. } => {
                        return Err(TransportError::new(
                            TransportErrorKind::Rejected,
                            format!(
                                "rtmp: server rejected {command} ({})",
                                code.as_deref().unwrap_or("no code")
                            ),
                        ));
                    }
                    _ => {}
                }
            }
        }
    }
}

/// つながった RTMP。
pub struct RtmpTransport {
    stream: Box<dyn ByteStream>,
    session: ClientSession,
    muxer: FlvMuxer,
    tags: Vec<OutTag>,
    bytes_sent: u64,
    buf: Vec<u8>,
    /// 最後に送ったタグの時刻（設定を送るときに使う）。
    last_ts: u32,
}

impl RtmpTransport {
    fn flush(&mut self) -> Result<(), TransportError> {
        let out = self.session.take_output();
        if !out.is_empty() {
            self.stream
                .write_all(&out)
                .map_err(TransportError::network)?;
            self.bytes_sent += out.len() as u64;
        }
        Ok(())
    }
}

fn proto_err(e: impl std::fmt::Display) -> TransportError {
    TransportError::new(TransportErrorKind::Protocol, e.to_string())
}

impl Transport for RtmpTransport {
    fn set_config(&mut self, config: &CodecConfig) -> Result<(), TransportError> {
        let unsup = |e: senca_flv::Error| {
            TransportError::new(TransportErrorKind::Unsupported, e.to_string())
        };
        self.muxer.set_config(config.clone()).map_err(unsup)?;
        // 設定（sequence header）はすぐに送る。受け手が最初のフレームの前に両方のトラックを知れるように
        // （FlvMuxer も次の AU の前にもう一度出すが、同じ内容の再送は受け手が読み捨てる）。
        if config.record.is_empty() {
            return Ok(());
        }
        let (mt, body) = match config.codec.kind() {
            senca_core::MediaKind::Video => (
                MediaType::Video,
                Some(
                    senca_flv::build::video_sequence_start(config.codec, config.record.clone())
                        .map_err(unsup)?,
                ),
            ),
            _ => (
                MediaType::Audio,
                senca_flv::build::audio_sequence_start(config).map_err(unsup)?,
            ),
        };
        if let Some(b) = body {
            self.session
                .send_media(mt, self.last_ts, &b.parts())
                .map_err(proto_err)?;
            self.flush()?;
        }
        Ok(())
    }

    fn send(&mut self, au: &AccessUnit) -> Result<(), TransportError> {
        self.tags.clear();
        self.muxer
            .access_unit(au, &mut self.tags)
            .map_err(|e| match e {
                senca_flv::Error::UnsupportedCodec(_) => {
                    TransportError::new(TransportErrorKind::Unsupported, e.to_string())
                }
                _ => proto_err(e),
            })?;
        for t in &self.tags {
            self.last_ts = t.timestamp;
            let mt = match t.tag_type {
                TagType::Video => MediaType::Video,
                TagType::Audio => MediaType::Audio,
                TagType::Script => continue,
            };
            self.session
                .send_media(mt, t.timestamp, &t.body.parts())
                .map_err(proto_err)?;
        }
        self.flush()
    }

    fn poll(&mut self) -> Result<(), TransportError> {
        loop {
            match self.stream.read(&mut self.buf) {
                Ok(0) => {
                    return Err(TransportError::network(
                        "rtmp: server closed the connection",
                    ));
                }
                Ok(n) => {
                    self.session.feed(&self.buf[..n]).map_err(proto_err)?;
                    while let Some(ev) = self.session.poll_event() {
                        match ev {
                            ClientEvent::Rejected { command, code, .. } => {
                                return Err(TransportError::new(
                                    TransportErrorKind::Rejected,
                                    format!(
                                        "rtmp: server rejected {command} ({})",
                                        code.as_deref().unwrap_or("no code")
                                    ),
                                ));
                            }
                            ClientEvent::StreamEnded { .. } => {
                                return Err(TransportError::network(
                                    "rtmp: stream ended by server",
                                ));
                            }
                            _ => {}
                        }
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) =>
                {
                    break;
                }
                Err(e) => return Err(TransportError::network(e)),
            }
        }
        // Acknowledgement などの返事
        self.flush()
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_millis(50)
    }

    fn bytes_sent(&self) -> u64 {
        self.bytes_sent
    }

    fn close(&mut self, linger: Duration) {
        let _ = self
            .stream
            .set_write_timeout(Some(linger.max(Duration::from_millis(1))));
        if self.session.close().is_ok() {
            let _ = self.flush();
        }
        let _ = self.stream.flush();
        self.stream.shutdown();
        // 相手が閉じるまで読み捨てる（未読のまま閉じると RST になり、相手が送り残しを捨てうる）
        let _ = self
            .stream
            .set_read_timeout(Some(Duration::from_millis(50)));
        let deadline = Instant::now() + linger;
        while Instant::now() < deadline {
            match self.stream.read(&mut self.buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) => {}
                Err(_) => break,
            }
        }
    }
}
