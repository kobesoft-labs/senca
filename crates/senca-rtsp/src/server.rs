//! RTSP 1.0 サーバー（TCP 接続 1 本ぶん、play 専用）。sans-io の状態機械。
//!
//! OPTIONS・GET_PARAMETER・SET_PARAMETER・TEARDOWN と認証はこの中で応答する。DESCRIBE・SETUP・PLAY・PAUSE は
//! イベントにして呼び出し側に判断させ、`accept_*` / [`ServerSession::reject`] で応答する。

use crate::auth::{Algorithm, basic_challenge, digest_challenge, verify_basic, verify_digest};
use crate::error::{Error, Result};
use crate::headers::{
    LowerTransport, Range, RtpInfoEntry, SessionHeader, Transport, format_rtp_info,
};
use crate::message::{
    Decoder, FEED_CHUNK, Frame, Limits, Method, Request, Response, write_interleaved,
};
use crate::url::RtspUrl;
use bytes::Bytes;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::time::{Duration, Instant};

/// パスワードを引く関数。
pub type PasswordLookup = Box<dyn Fn(&str) -> Option<String> + Send>;

/// 組み込みの認証（Digest。Basic は `allow_basic` のときだけ）。
pub struct ServerAuth {
    pub realm: String,
    /// 呼び出し側が乱数で作った nonce（接続ごとに変える）。
    pub nonce: String,
    pub algorithm: Algorithm,
    pub allow_basic: bool,
    pub password_of: PasswordLookup,
}

impl fmt::Debug for ServerAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerAuth")
            .field("realm", &self.realm)
            .field("algorithm", &self.algorithm)
            .field("allow_basic", &self.allow_basic)
            .finish_non_exhaustive()
    }
}

/// サーバーの設定。
#[derive(Debug)]
pub struct ServerConfig {
    pub limits: Limits,
    pub server_name: String,
    /// Session ID（呼び出し側が推測困難な値を作って渡す）。
    pub session_id: String,
    /// Session の timeout（秒）。
    pub session_timeout: u32,
    /// `None` なら認証しない。
    pub auth: Option<ServerAuth>,
}

impl ServerConfig {
    pub fn new(session_id: impl Into<String>) -> Self {
        ServerConfig {
            limits: Limits::default(),
            server_name: "SENCA".into(),
            session_id: session_id.into(),
            session_timeout: 60,
            auth: None,
        }
    }
}

/// 応答待ちの要求の識別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(u64);

/// サーバーの出来事。`user` は認証した利用者名（認証なしなら `None`）。
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    Describe {
        id: RequestId,
        uri: String,
        path: String,
        user: Option<String>,
    },
    /// 候補の Transport（対応する profile のものだけ）。[`ServerSession::accept_setup`] に 1 つ選んで渡す。
    Setup {
        id: RequestId,
        uri: String,
        path: String,
        transports: Vec<Transport>,
        user: Option<String>,
    },
    Play {
        id: RequestId,
        uri: String,
        range: Option<Range>,
        user: Option<String>,
    },
    Pause {
        id: RequestId,
        uri: String,
    },
    /// TEARDOWN（応答済み）。呼び出し側は送出をやめる。
    Teardown {
        uri: String,
    },
    /// クライアントからの interleaved（RTCP の受信報告など）。
    Interleaved {
        channel: u8,
        data: Bytes,
    },
    /// 資格情報付きの要求が認証に失敗した（記録用。応答は済んでいる）。
    AuthFailed {
        method: Method,
        uri: String,
    },
    /// Session が timeout した。呼び出し側は接続を閉じる。
    TimedOut,
}

/// SETUP 済みのトラック。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupTrack {
    pub uri: String,
    pub transport: Transport,
}

#[derive(Debug)]
struct Pending {
    cseq: u32,
    method: Method,
    uri: String,
}

const MAX_PENDING: usize = 16;
const MAX_TRACKS: usize = 16;

/// RTSP サーバーの 1 接続。
#[derive(Debug)]
pub struct ServerSession {
    cfg: ServerConfig,
    decoder: Decoder,
    out: Vec<u8>,
    events: VecDeque<ServerEvent>,
    pending: HashMap<u64, Pending>,
    next_id: u64,
    tracks: Vec<SetupTrack>,
    playing: bool,
    closed: bool,
    last_activity: Option<Instant>,
}

impl ServerSession {
    pub fn new(cfg: ServerConfig) -> Self {
        ServerSession {
            decoder: Decoder::new(cfg.limits),
            cfg,
            out: Vec::new(),
            events: VecDeque::new(),
            pending: HashMap::new(),
            next_id: 0,
            tracks: Vec::new(),
            playing: false,
            closed: false,
            last_activity: None,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.cfg.session_id
    }

    pub fn tracks(&self) -> &[SetupTrack] {
        &self.tracks
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// TEARDOWN・timeout のあと。
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn poll_event(&mut self) -> Option<ServerEvent> {
        self.events.pop_front()
    }

    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// 受信したバイト列を渡す。`Err` なら接続を閉じる。
    pub fn feed(&mut self, now: Instant, data: &[u8]) -> Result<()> {
        for chunk in data.chunks(FEED_CHUNK) {
            self.decoder.feed(chunk)?;
            while let Some(f) = self.decoder.decode()? {
                if !self.tracks.is_empty() {
                    self.last_activity = Some(now);
                }
                match f {
                    Frame::Interleaved { channel, data } => self
                        .events
                        .push_back(ServerEvent::Interleaved { channel, data }),
                    Frame::Response(_) => {
                        // こちらから要求は出さないので無視する
                    }
                    Frame::Request(r) => self.on_request(now, r)?,
                }
            }
        }
        Ok(())
    }

    fn respond(&mut self, cseq: u32, mut r: Response, with_session: bool) -> Result<()> {
        r.headers.set("CSeq", cseq.to_string());
        r.headers.set("Server", self.cfg.server_name.clone());
        if with_session && !self.tracks.is_empty() {
            r.headers.set(
                "Session",
                SessionHeader {
                    id: self.cfg.session_id.clone(),
                    timeout: Some(self.cfg.session_timeout),
                }
                .to_string(),
            );
        }
        r.encode(&mut self.out)
    }

    fn simple(&mut self, cseq: u32, status: u16) -> Result<()> {
        self.respond(cseq, Response::new(status), false)
    }

    /// 認証を確かめる。`Ok(None)` なら 401 を返し済み。
    fn authenticate(&mut self, cseq: u32, req: &Request) -> Result<Option<Option<String>>> {
        let Some(auth) = &self.cfg.auth else {
            return Ok(Some(None));
        };
        let header = req.headers.get("Authorization");
        let result = header.map(|h| {
            if h.len() >= 6 && h[..6].eq_ignore_ascii_case("basic ") {
                if auth.allow_basic {
                    verify_basic(h, &auth.password_of)
                } else {
                    Err(Error::Auth("Basic は許可していない"))
                }
            } else {
                verify_digest(
                    req.method.as_str(),
                    &req.uri,
                    h,
                    &auth.realm,
                    |n| n == auth.nonce,
                    &auth.password_of,
                )
            }
        });
        if let Some(Ok(user)) = result {
            return Ok(Some(Some(user)));
        }
        let mut r = Response::new(401);
        r.headers.append(
            "WWW-Authenticate",
            digest_challenge(&auth.realm, &auth.nonce, auth.algorithm, false, false),
        );
        if auth.allow_basic {
            r.headers
                .append("WWW-Authenticate", basic_challenge(&auth.realm));
        }
        if header.is_some() {
            self.events.push_back(ServerEvent::AuthFailed {
                method: req.method.clone(),
                uri: req.uri.clone(),
            });
        }
        self.respond(cseq, r, false)?;
        Ok(None)
    }

    fn new_pending(&mut self, cseq: u32, req: &Request) -> Option<RequestId> {
        if self.pending.len() >= MAX_PENDING {
            return None;
        }
        self.next_id += 1;
        self.pending.insert(
            self.next_id,
            Pending {
                cseq,
                method: req.method.clone(),
                uri: req.uri.clone(),
            },
        );
        Some(RequestId(self.next_id))
    }

    fn on_request(&mut self, _now: Instant, req: Request) -> Result<()> {
        let Some(cseq) = req.cseq() else {
            // CSeq がなければ応答の対応が取れない
            return Err(Error::Protocol("CSeq がない要求"));
        };
        if self.closed {
            return self.simple(cseq, 454);
        }
        // Session の確認（OPTIONS・DESCRIBE 以外）
        let session_ok = req.headers.get("Session").map(|v| {
            SessionHeader::parse(v)
                .is_ok_and(|s| s.id == self.cfg.session_id && !self.tracks.is_empty())
        });
        if req.method == Method::Options {
            let mut r = Response::new(200);
            r.headers.set(
                "Public",
                "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN, GET_PARAMETER, SET_PARAMETER",
            );
            return self.respond(cseq, r, session_ok == Some(true));
        }
        let Some(user) = self.authenticate(cseq, &req)? else {
            return Ok(());
        };
        let path = path_of(&req.uri);
        match req.method {
            Method::Describe => {
                let Some(id) = self.new_pending(cseq, &req) else {
                    return self.simple(cseq, 503);
                };
                self.events.push_back(ServerEvent::Describe {
                    id,
                    uri: req.uri,
                    path,
                    user,
                });
            }
            Method::Setup => {
                if session_ok == Some(false) {
                    return self.simple(cseq, 454);
                }
                if self.playing {
                    return self.simple(cseq, 455);
                }
                if self.tracks.len() >= MAX_TRACKS {
                    return self.simple(cseq, 453);
                }
                let transports = match req.headers.get("Transport").map(Transport::parse_list) {
                    Some(Ok(t)) => t,
                    Some(Err(_)) => return self.simple(cseq, 461),
                    None => return self.simple(cseq, 400),
                };
                let Some(id) = self.new_pending(cseq, &req) else {
                    return self.simple(cseq, 503);
                };
                self.events.push_back(ServerEvent::Setup {
                    id,
                    uri: req.uri,
                    path,
                    transports,
                    user,
                });
            }
            Method::Play | Method::Pause => {
                if session_ok != Some(true) {
                    return self.simple(cseq, 454);
                }
                let Some(id) = self.new_pending(cseq, &req) else {
                    return self.simple(cseq, 503);
                };
                if req.method == Method::Play {
                    let range = req.headers.get("Range").and_then(|v| Range::parse(v).ok());
                    self.events.push_back(ServerEvent::Play {
                        id,
                        uri: req.uri,
                        range,
                        user,
                    });
                } else {
                    self.events
                        .push_back(ServerEvent::Pause { id, uri: req.uri });
                }
            }
            Method::Teardown => {
                if session_ok != Some(true) {
                    return self.simple(cseq, 454);
                }
                self.respond(cseq, Response::new(200), true)?;
                self.playing = false;
                self.closed = true;
                self.events
                    .push_back(ServerEvent::Teardown { uri: req.uri });
            }
            Method::GetParameter | Method::SetParameter => {
                if session_ok == Some(false) {
                    return self.simple(cseq, 454);
                }
                if req.method == Method::SetParameter && !req.body.is_empty() {
                    return self.simple(cseq, 451);
                }
                self.respond(cseq, Response::new(200), session_ok == Some(true))?;
            }
            _ => {
                let mut r = Response::new(501);
                r.headers.set(
                    "Allow",
                    "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN, GET_PARAMETER, SET_PARAMETER",
                );
                self.respond(cseq, r, false)?;
            }
        }
        Ok(())
    }

    fn take_pending(&mut self, id: RequestId, method: Option<Method>) -> Result<Pending> {
        let p = self
            .pending
            .remove(&id.0)
            .ok_or(Error::InvalidState("応答待ちではない要求"))?;
        if let Some(m) = method
            && p.method != m
        {
            self.pending.insert(id.0, p);
            return Err(Error::InvalidState("メソッドが違う"));
        }
        Ok(p)
    }

    /// DESCRIBE に SDP で応える。`content_base` が `None` なら要求 URI に `/` を足したもの。
    pub fn accept_describe(
        &mut self,
        id: RequestId,
        sdp: Bytes,
        content_base: Option<&str>,
    ) -> Result<()> {
        let p = self.take_pending(id, Some(Method::Describe))?;
        if sdp.len() > self.cfg.limits.max_body {
            return Err(Error::LimitExceeded("SDP"));
        }
        let mut r = Response::new(200);
        r.headers.set("Content-Type", "application/sdp");
        let base = match content_base {
            Some(b) => b.to_string(),
            None if p.uri.ends_with('/') => p.uri.clone(),
            None => format!("{}/", p.uri),
        };
        r.headers.set("Content-Base", base);
        r.body = sdp;
        self.respond(p.cseq, r, false)
    }

    /// SETUP に応える。TCP で `interleaved` が `None` なら空いている channel を割り当てる。
    /// 戻り値は応答に書いた Transport。
    pub fn accept_setup(&mut self, id: RequestId, mut transport: Transport) -> Result<Transport> {
        let p = self.take_pending(id, Some(Method::Setup))?;
        if transport.lower == LowerTransport::Tcp {
            let used: Vec<u8> = self
                .tracks
                .iter()
                .filter_map(|t| t.transport.interleaved)
                .flat_map(|(a, b)| [a, b])
                .collect();
            let clash = transport
                .interleaved
                .is_none_or(|(a, b)| used.contains(&a) || used.contains(&b));
            if clash {
                let free = (0u8..=254)
                    .step_by(2)
                    .find(|c| !used.contains(c) && !used.contains(&(c + 1)))
                    .ok_or(Error::LimitExceeded("interleaved channel"))?;
                transport.interleaved = Some((free, free + 1));
            }
        } else if transport.client_port.is_none() {
            self.simple(p.cseq, 461)?;
            return Err(Error::Protocol("UDP の client_port がない"));
        }
        self.tracks.push(SetupTrack {
            uri: p.uri,
            transport: transport.clone(),
        });
        let mut r = Response::new(200);
        r.headers.set("Transport", transport.to_string());
        self.respond(p.cseq, r, true)?;
        Ok(transport)
    }

    /// PLAY に応える。
    pub fn accept_play(
        &mut self,
        id: RequestId,
        range: Option<Range>,
        rtp_info: &[RtpInfoEntry],
    ) -> Result<()> {
        let p = self.take_pending(id, Some(Method::Play))?;
        let mut r = Response::new(200);
        if let Some(rg) = range {
            r.headers.set("Range", rg.to_string());
        }
        if !rtp_info.is_empty() {
            r.headers.set("RTP-Info", format_rtp_info(rtp_info));
        }
        self.playing = true;
        self.respond(p.cseq, r, true)
    }

    /// 本文なしの 200 で応える（PAUSE など）。
    pub fn accept(&mut self, id: RequestId) -> Result<()> {
        let p = self.take_pending(id, None)?;
        if p.method == Method::Pause {
            self.playing = false;
        }
        self.respond(p.cseq, Response::new(200), true)
    }

    /// 失敗の状態で応える（404・403・461 など）。
    pub fn reject(&mut self, id: RequestId, status: u16) -> Result<()> {
        let p = self.take_pending(id, None)?;
        if !(300..=599).contains(&status) {
            return Err(Error::InvalidState("reject は 3xx〜5xx"));
        }
        self.simple(p.cseq, status)
    }

    /// interleaved で RTP/RTCP を送る（TCP のとき）。
    pub fn send_interleaved(&mut self, channel: u8, data: &[u8]) -> Result<()> {
        if self.closed {
            return Err(Error::InvalidState("終了したセッション"));
        }
        write_interleaved(channel, data, &mut self.out)
    }

    /// 次に [`handle_timeout`](Self::handle_timeout) を呼ぶ時刻。
    pub fn poll_timeout(&self) -> Option<Instant> {
        if self.closed {
            return None;
        }
        self.last_activity
            .map(|t| t + Duration::from_secs(u64::from(self.cfg.session_timeout)))
    }

    /// Session の timeout を確かめる（RTSP・RTCP のどちらかが届けば延びる）。
    /// UDP の RTCP を受けたときは [`touch`](Self::touch) を呼ぶ。
    pub fn handle_timeout(&mut self, now: Instant) {
        if let Some(d) = self.poll_timeout()
            && now >= d
        {
            self.closed = true;
            self.playing = false;
            self.events.push_back(ServerEvent::TimedOut);
        }
    }

    /// 活動があった（UDP で RTCP を受けたなど）。
    pub fn touch(&mut self, now: Instant) {
        if !self.tracks.is_empty() {
            self.last_activity = Some(now);
        }
    }
}

fn path_of(uri: &str) -> String {
    match RtspUrl::parse(uri) {
        Ok(u) => {
            let p = u.path.split('?').next().unwrap_or("");
            if p.is_empty() {
                "/".into()
            } else {
                p.to_string()
            }
        }
        Err(_) => uri.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ClientConfig, ClientEvent, ClientSession, TransportRequest};
    use crate::headers::NptTime;

    fn pump(c: &mut ClientSession, s: &mut ServerSession, now: Instant) {
        for _ in 0..8 {
            let a = c.take_output();
            if !a.is_empty() {
                s.feed(now, &a).unwrap();
            }
            let b = s.take_output();
            if !b.is_empty() {
                c.feed(now, &b).unwrap();
            }
            if a.is_empty() && b.is_empty() {
                break;
            }
        }
    }

    fn server_with_auth() -> ServerSession {
        let mut cfg = ServerConfig::new("5EC0A1D2");
        cfg.auth = Some(ServerAuth {
            realm: "senca".into(),
            nonce: "n0nce".into(),
            algorithm: Algorithm::Md5,
            allow_basic: false,
            password_of: Box::new(|u| (u == "alice").then(|| "s3cret".to_string())),
        });
        ServerSession::new(cfg)
    }

    #[test]
    fn loopback_full_session_with_digest() {
        let now = Instant::now();
        let mut s = server_with_auth();
        let mut c = ClientSession::new(ClientConfig::new(
            "rtsp://alice:s3cret@127.0.0.1:8554/live/cam",
        ))
        .unwrap();
        c.start(now).unwrap();
        pump(&mut c, &mut s, now);
        assert!(
            matches!(c.poll_event(), Some(ClientEvent::Options { public }) if public.contains(&Method::GetParameter))
        );

        c.describe(now).unwrap();
        pump(&mut c, &mut s, now);
        let Some(ServerEvent::Describe { id, path, user, .. }) = s.poll_event() else {
            panic!()
        };
        assert_eq!(path, "/live/cam");
        assert_eq!(user.as_deref(), Some("alice"));
        let sdp = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=video 0 RTP/AVP 96\r\na=control:trackID=0\r\nm=audio 0 RTP/AVP 97\r\na=control:trackID=1\r\n";
        s.accept_describe(id, Bytes::from(sdp), None).unwrap();
        pump(&mut c, &mut s, now);
        let Some(ClientEvent::Described {
            content_base,
            sdp: got,
        }) = c.poll_event()
        else {
            panic!()
        };
        assert_eq!(content_base, "rtsp://127.0.0.1:8554/live/cam/");
        assert_eq!(&got[..], sdp.as_bytes());

        let t0 = c
            .setup(
                now,
                &c.resolve_control("trackID=0"),
                TransportRequest::Interleaved {
                    rtp_channel: 0,
                    rtcp_channel: 1,
                },
            )
            .unwrap();
        let t1 = c
            .setup(
                now,
                &c.resolve_control("trackID=1"),
                TransportRequest::Interleaved {
                    rtp_channel: 0,
                    rtcp_channel: 1,
                },
            )
            .unwrap();
        for expect_track in [t0, t1] {
            pump(&mut c, &mut s, now);
            let Some(ServerEvent::Setup {
                id,
                path,
                transports,
                ..
            }) = s.poll_event()
            else {
                panic!()
            };
            assert!(path.ends_with(&format!("trackID={expect_track}")));
            // 2 つ目は channel が重なるので割り当て直される
            let tr = s.accept_setup(id, transports[0].clone()).unwrap();
            assert_eq!(
                tr.interleaved,
                Some((2 * expect_track as u8, 2 * expect_track as u8 + 1))
            );
            pump(&mut c, &mut s, now);
            let Some(ClientEvent::SetupDone {
                track,
                transport,
                session,
                ..
            }) = c.poll_event()
            else {
                panic!()
            };
            assert_eq!(track, expect_track);
            assert_eq!(transport.interleaved, tr.interleaved);
            assert_eq!(session, "5EC0A1D2");
        }
        assert_eq!(s.tracks().len(), 2);

        c.play(now, Some(Range::npt_from(0.0))).unwrap();
        pump(&mut c, &mut s, now);
        let Some(ServerEvent::Play { id, range, .. }) = s.poll_event() else {
            panic!()
        };
        assert_eq!(
            range,
            Some(Range::Npt {
                start: NptTime::Seconds(0.0),
                end: None
            })
        );
        let info = vec![RtpInfoEntry {
            url: "rtsp://127.0.0.1:8554/live/cam/trackID=0".into(),
            seq: Some(1),
            rtptime: Some(0),
        }];
        s.accept_play(id, Some(Range::npt_from(0.0)), &info)
            .unwrap();
        for i in 0..3u8 {
            s.send_interleaved(0, &[0x80, 96, 0, i]).unwrap();
        }
        s.send_interleaved(3, &[0x80, 200]).unwrap();
        pump(&mut c, &mut s, now);
        let Some(ClientEvent::Playing { rtp_info, .. }) = c.poll_event() else {
            panic!()
        };
        assert_eq!(rtp_info, info);
        let ev: Vec<_> = std::iter::from_fn(|| c.poll_event()).collect();
        assert_eq!(ev.len(), 4);
        assert_eq!(
            ev[3],
            ClientEvent::Interleaved {
                channel: 3,
                data: Bytes::from_static(&[0x80, 200])
            }
        );
        // クライアントから RTCP
        c.send_interleaved(1, &[0x81, 201]).unwrap();
        pump(&mut c, &mut s, now);
        assert!(matches!(
            s.poll_event(),
            Some(ServerEvent::Interleaved { channel: 1, .. })
        ));

        // keepalive（GET_PARAMETER）
        c.handle_timeout(now + Duration::from_secs(30)).unwrap();
        pump(&mut c, &mut s, now);
        assert_eq!(c.poll_event(), None);

        c.teardown(now).unwrap();
        pump(&mut c, &mut s, now);
        assert!(matches!(s.poll_event(), Some(ServerEvent::Teardown { .. })));
        assert_eq!(c.poll_event(), Some(ClientEvent::TornDown));
        assert!(s.is_closed());
    }

    #[test]
    fn wrong_password_and_basic_policy() {
        let now = Instant::now();
        let mut s = server_with_auth();
        let mut c = ClientSession::new(ClientConfig::new("rtsp://alice:bad@h/x")).unwrap();
        c.describe(now).unwrap();
        pump(&mut c, &mut s, now);
        assert!(matches!(
            c.poll_event(),
            Some(ClientEvent::Failed { status: 401, .. })
        ));
        assert!(matches!(
            s.poll_event(),
            Some(ServerEvent::AuthFailed { .. })
        ));
        // Basic を送っても許可していなければ 401
        let mut req = Request::new(Method::Describe, "rtsp://h/x");
        req.headers.set("CSeq", "1");
        req.headers.set("Authorization", "Basic YWxpY2U6czNjcmV0");
        let mut b = Vec::new();
        req.encode(&mut b).unwrap();
        s.feed(now, &b).unwrap();
        assert!(
            String::from_utf8(s.take_output())
                .unwrap()
                .starts_with("RTSP/1.0 401")
        );
    }

    #[test]
    fn session_rules_and_timeout() {
        let now = Instant::now();
        let mut s = ServerSession::new(ServerConfig::new("S1"));
        let send = |s: &mut ServerSession, raw: &str| {
            s.feed(now, raw.as_bytes()).unwrap();
            String::from_utf8(s.take_output()).unwrap()
        };
        assert!(
            send(&mut s, "PLAY rtsp://h/x RTSP/1.0\r\nCSeq: 1\r\n\r\n").starts_with("RTSP/1.0 454")
        );
        assert!(
            send(&mut s, "RECORD rtsp://h/x RTSP/1.0\r\nCSeq: 2\r\n\r\n")
                .starts_with("RTSP/1.0 501")
        );
        assert!(
            send(
                &mut s,
                "SETUP rtsp://h/x/t RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/SAVP\r\n\r\n"
            )
            .starts_with("RTSP/1.0 461")
        );
        assert!(
            send(&mut s, "SETUP rtsp://h/x/t RTSP/1.0\r\nCSeq: 4\r\n\r\n")
                .starts_with("RTSP/1.0 400")
        );
        assert!(send(&mut s, "SETUP rtsp://h/x/t RTSP/1.0\r\nCSeq: 5\r\nTransport: RTP/AVP;unicast;client_port=7000-7001\r\n\r\n").is_empty());
        let Some(ServerEvent::Setup {
            id, mut transports, ..
        }) = s.poll_event()
        else {
            panic!()
        };
        transports[0].server_port = Some((9000, 9001));
        s.accept_setup(id, transports.remove(0)).unwrap();
        let out = String::from_utf8(s.take_output()).unwrap();
        assert!(out.contains("Session: S1;timeout=60"), "{out}");
        assert!(out.contains("server_port=9000-9001"));
        assert!(
            send(
                &mut s,
                "PLAY rtsp://h/x RTSP/1.0\r\nCSeq: 6\r\nSession: OTHER\r\n\r\n"
            )
            .starts_with("RTSP/1.0 454")
        );
        assert!(
            send(
                &mut s,
                "GET_PARAMETER rtsp://h/x RTSP/1.0\r\nCSeq: 7\r\nSession: S1\r\n\r\n"
            )
            .starts_with("RTSP/1.0 200")
        );
        // CSeq のない要求は接続を閉じる
        assert!(s.feed(now, b"OPTIONS * RTSP/1.0\r\n\r\n").is_err());
        let mut s2 = ServerSession::new(ServerConfig::new("S1"));
        s2.feed(now, b"SETUP rtsp://h/x/t RTSP/1.0\r\nCSeq: 1\r\nTransport: RTP/AVP/TCP;interleaved=0-1\r\n\r\n").unwrap();
        let Some(ServerEvent::Setup { id, transports, .. }) = s2.poll_event() else {
            panic!()
        };
        s2.accept_setup(id, transports[0].clone()).unwrap();
        assert!(s2.reject(id, 404).is_err());
        assert_eq!(s2.poll_timeout(), None);
        s2.feed(
            now,
            b"GET_PARAMETER rtsp://h/x RTSP/1.0\r\nCSeq: 2\r\nSession: S1\r\n\r\n",
        )
        .unwrap();
        let d = s2.poll_timeout().unwrap();
        s2.handle_timeout(d - Duration::from_secs(1));
        assert_eq!(s2.poll_event(), None);
        s2.handle_timeout(d);
        assert_eq!(s2.poll_event(), Some(ServerEvent::TimedOut));
        assert!(s2.is_closed());
    }

    #[test]
    fn fuzz_server_and_client() {
        let base = b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\nSETUP rtsp://h/x/t RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;interleaved=0-1\r\n\r\n$\x01\x00\x02ab";
        let mut seed = 7u64;
        let now = Instant::now();
        for _ in 0..5000 {
            let mut m = base.to_vec();
            for _ in 0..3 {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let i = (seed >> 33) as usize % m.len();
                m[i] = (seed >> 20) as u8;
            }
            let mut s = server_with_auth();
            let _ = s.feed(now, &m);
            while s.poll_event().is_some() {}
            let mut c = ClientSession::new(ClientConfig::new("rtsp://u:p@h/x")).unwrap();
            c.describe(now).unwrap();
            let _ = c.feed(now, &m);
            while c.poll_event().is_some() {}
        }
    }
}
