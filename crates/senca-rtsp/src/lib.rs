//! RTSP 1.0（RFC 2326、sans-io）。I/O・スレッド・時計を持たない（時刻は `Instant` を引数で渡す）。
//!
//! - [`message`] — 要求・応答、ヘッダー、RTSP と interleaved（`$`）が混ざったストリームの逐次復号（上限つき）
//! - [`headers`] — Transport・Session・Range・RTP-Info・Public
//! - [`auth`] — Digest（MD5・SHA-256、qop=auth と qop なし）と Basic。検証側の補助も
//! - [`url`] — rtsp(s):// URL（userinfo を資格情報に分ける）と制御 URL の解決
//! - [`client`] — [`ClientSession`]（カメラから pull）
//! - [`server`] — [`ServerSession`]（play 専用の配信、TCP 接続 1 本ぶん）
//!
//! RTSP 2.0（RFC 7826）・RECORD・multicast は扱わない（仕様 6.6）。SDP の解釈は `senca-sdp`、
//! RTP の depacketize は `senca-rtp` で行う（この crate は SDP を `Bytes` のまま渡す）。
//!
//! ## クライアントの使い方
//!
//! ```text
//! let mut c = ClientSession::new(ClientConfig::new("rtsp://user:pass@cam/stream"))?;
//! sock = connect(c.url().host_port());          // SSRF の方針は呼び出し側で確かめる
//! c.start(now)?;  c.describe(now)?;
//! loop {
//!     sock.write_all(&c.take_output())?;
//!     // 読めたら c.feed(now, &buf[..n])?、期限（c.poll_timeout()）なら c.handle_timeout(now)?
//!     while let Some(ev) = c.poll_event() {
//!         match ev {
//!             Described { content_base, sdp } => { /* SDP を解釈し、各 m= の a=control を
//!                 c.resolve_control(..) で URL にして c.setup(now, &url, TransportRequest::..)? */ }
//!             SetupDone { .. } => { /* 全トラック済みなら c.play(now, Some(Range::npt_from(0.0)))? */ }
//!             Interleaved { channel, data } => { /* RTP/RTCP を senca-rtp へ */ }
//!             Reconnect { url } => { /* 接続し直して c.reconnected(now)? */ }
//!             Failed { .. } => { /* 終了 */ }
//!             _ => {}
//!         }
//!     }
//! }
//! ```
//!
//! UDP のときは呼び出し側が RTP・RTCP のソケットを開き、そのポートを [`TransportRequest::Udp`] で渡す。
//! keepalive（Session の timeout の半分ごと。Public に GET_PARAMETER があればそれ、なければ OPTIONS）と
//! 401 への応答（Digest は SHA-256 > MD5、Basic は `allow_basic` のときだけ）は内部で行う。
//!
//! ## サーバーの使い方
//!
//! ```text
//! let mut s = ServerSession::new(ServerConfig { session_id: random_id(), auth: Some(..), .. });
//! loop {
//!     s.feed(now, &buf[..n])?;
//!     while let Some(ev) = s.poll_event() {
//!         match ev {
//!             Describe { id, path, user, .. } => s.accept_describe(id, sdp, None)?,   // または s.reject(id, 404)
//!             Setup { id, transports, .. } => { let t = s.accept_setup(id, choose(transports))?; }
//!             Play { id, .. } => s.accept_play(id, range, &rtp_info)?,               // 以後 send_interleaved
//!             Teardown { .. } | TimedOut => break,
//!             _ => {}
//!         }
//!     }
//!     sock.write_all(&s.take_output())?;
//! }
//! ```

pub mod auth;
pub mod client;
pub mod error;
pub mod headers;
pub mod message;
pub mod server;
pub mod url;

pub use auth::{Algorithm, Credentials};
pub use client::{ClientConfig, ClientEvent, ClientSession, TransportRequest};
pub use error::{Error, Result};
pub use headers::{LowerTransport, NptTime, Range, RtpInfoEntry, SessionHeader, Transport};
pub use message::{Decoder, Frame, Headers, Limits, Method, Request, Response};
pub use server::{RequestId, ServerAuth, ServerConfig, ServerEvent, ServerSession, SetupTrack};
pub use url::{RtspUrl, resolve_control};
