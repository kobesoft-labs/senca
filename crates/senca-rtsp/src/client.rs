//! RTSP 1.0 クライアント（カメラから pull する側）。sans-io の状態機械。
//!
//! 要求は 1 つずつ送る（前の応答を待ってから次を送る）。SETUP を続けて呼んでも、2 つ目以降は
//! 最初の SETUP で得た Session を付けて送られる。

use crate::auth::{ClientAuth, Credentials, parse_challenges};
use crate::error::{Error, Result};
use crate::headers::{
    DEFAULT_SESSION_TIMEOUT, Range, RtpInfoEntry, SessionHeader, Transport, parse_public,
    parse_rtp_info,
};
use crate::message::{Decoder, FEED_CHUNK, Frame, Headers, Limits, Method, Request, Response};
use crate::url::{RtspUrl, content_base, resolve_control};
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

/// cnonce を作る関数（Digest の qop=auth・-sess のときだけ呼ばれる）。
pub type CnonceFn = Box<dyn FnMut() -> String + Send>;

/// クライアントの設定。
pub struct ClientConfig {
    /// `rtsp://[user:pass@]host[:port]/path`。userinfo は資格情報として使い、要求からは除く。
    pub url: String,
    /// 資格情報（URL の userinfo より優先）。
    pub credentials: Option<Credentials>,
    /// Basic 認証を許すか（既定 false。TLS または閉域の例外だけで真にする: 仕様 6.6）。
    pub allow_basic: bool,
    pub user_agent: String,
    /// 追えるリダイレクトの回数。
    pub max_redirects: u32,
    pub limits: Limits,
    /// 応答の期限。
    pub request_timeout: Duration,
    /// cnonce の生成（乱数を入れること）。`None` なら nonce と CSeq から作る（推測可能なので非推奨）。
    pub cnonce: Option<CnonceFn>,
}

impl ClientConfig {
    pub fn new(url: impl Into<String>) -> Self {
        ClientConfig {
            url: url.into(),
            credentials: None,
            allow_basic: false,
            user_agent: "SENCA".into(),
            max_redirects: 3,
            limits: Limits::default(),
            request_timeout: Duration::from_secs(10),
            cnonce: None,
        }
    }
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("url", &crate::url::strip_userinfo(&self.url))
            .field("credentials", &self.credentials)
            .field("allow_basic", &self.allow_basic)
            .field("max_redirects", &self.max_redirects)
            .finish_non_exhaustive()
    }
}

/// SETUP で求める輸送。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportRequest {
    /// UDP unicast。呼び出し側が開いた RTP・RTCP のポート。
    Udp { rtp_port: u16, rtcp_port: u16 },
    /// TCP interleaved（RTSP の接続に混ぜる）。
    Interleaved { rtp_channel: u8, rtcp_channel: u8 },
}

impl TransportRequest {
    fn to_transport(self) -> Transport {
        let mut t = match self {
            TransportRequest::Udp {
                rtp_port,
                rtcp_port,
            } => Transport::udp(rtp_port, rtcp_port),
            TransportRequest::Interleaved {
                rtp_channel,
                rtcp_channel,
            } => Transport::interleaved(rtp_channel, rtcp_channel),
        };
        t.mode = None;
        t
    }
}

/// クライアントの出来事。
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    /// OPTIONS の応答（keepalive のものは出さない）。
    Options {
        public: Vec<Method>,
    },
    /// DESCRIBE の応答。`content_base` は制御 URL の解決に使う基底（Content-Base > Content-Location > 要求 URL）。
    Described {
        content_base: String,
        sdp: Bytes,
    },
    /// SETUP の応答。`transport` はサーバーが返したもの（server_port・interleaved・ssrc）。
    SetupDone {
        track: usize,
        control_url: String,
        transport: Transport,
        session: String,
    },
    Playing {
        range: Option<Range>,
        rtp_info: Vec<RtpInfoEntry>,
    },
    Paused,
    /// GET_PARAMETER（keepalive 以外）の応答本文。
    Parameter {
        body: Bytes,
    },
    TornDown,
    /// interleaved の RTP/RTCP。
    Interleaved {
        channel: u8,
        data: Bytes,
    },
    /// 別の接続先へのリダイレクト。呼び出し側は今の接続を閉じ、（SSRF の方針を確かめてから）
    /// `url` の接続先へつなぎ直して [`ClientSession::reconnected`] を呼ぶ。
    Reconnect {
        url: String,
    },
    /// 要求が失敗した（2xx 以外、または認証方式が受け入れられない）。
    Failed {
        method: Method,
        status: u16,
        reason: String,
    },
}

#[derive(Debug, Clone)]
enum Kind {
    Options { keepalive: bool },
    Describe,
    Setup { track: usize },
    Play,
    Pause,
    Teardown,
    GetParameter { keepalive: bool },
}

#[derive(Debug, Clone)]
struct Queued {
    method: Method,
    uri: String,
    headers: Headers,
    kind: Kind,
    auth_attempts: u8,
    stale_retried: bool,
}

#[derive(Debug)]
struct InFlight {
    cseq: u32,
    q: Queued,
    deadline: Instant,
}

/// 待たせておける要求の数。
const MAX_QUEUE: usize = 32;

/// RTSP クライアントのセッション。
pub struct ClientSession {
    cfg: ClientConfig,
    url: RtspUrl,
    credentials: Option<Credentials>,
    decoder: Decoder,
    out: Vec<u8>,
    events: VecDeque<ClientEvent>,
    queue: VecDeque<Queued>,
    inflight: Option<InFlight>,
    awaiting_reconnect: Option<Queued>,
    cseq: u32,
    auth: Option<ClientAuth>,
    session: Option<SessionHeader>,
    public: Vec<Method>,
    base: Option<String>,
    aggregate: Option<String>,
    keepalive_at: Option<Instant>,
    use_get_parameter: bool,
    redirects: u32,
    tracks: usize,
}

impl fmt::Debug for ClientSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientSession")
            .field("url", &self.url)
            .field("cseq", &self.cseq)
            .field("has_session", &self.session.is_some())
            .finish_non_exhaustive()
    }
}

impl ClientSession {
    pub fn new(cfg: ClientConfig) -> Result<Self> {
        let mut url = RtspUrl::parse(&cfg.url)?;
        let credentials = cfg.credentials.clone().or(url.credentials.take());
        Ok(ClientSession {
            decoder: Decoder::new(cfg.limits),
            cfg,
            url,
            credentials,
            out: Vec::new(),
            events: VecDeque::new(),
            queue: VecDeque::new(),
            inflight: None,
            awaiting_reconnect: None,
            cseq: 0,
            auth: None,
            session: None,
            public: Vec::new(),
            base: None,
            aggregate: None,
            keepalive_at: None,
            use_get_parameter: false,
            redirects: 0,
            tracks: 0,
        })
    }

    /// 今の接続先（リダイレクト後はその先）。
    pub fn url(&self) -> &RtspUrl {
        &self.url
    }

    /// Session ID（SETUP 後）。
    pub fn session_id(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.id.as_str())
    }

    /// DESCRIBE で決まった制御 URL の基底。
    pub fn content_base(&self) -> Option<&str> {
        self.base.as_deref()
    }

    /// SDP の `a=control` を制御 URL にする（基底は Content-Base など、未取得なら要求 URL）。
    pub fn resolve_control(&self, control: &str) -> String {
        let base = self.base.clone().unwrap_or_else(|| self.url.request_url());
        resolve_control(&base, control)
    }

    /// 集約制御の URL（PLAY・PAUSE・TEARDOWN・keepalive に使う）を指定する。
    /// SDP のセッション階層の `a=control` を解決したものを渡す。既定は Content-Base。
    pub fn set_aggregate_url(&mut self, url: impl Into<String>) {
        self.aggregate = Some(url.into());
    }

    fn aggregate_url(&self) -> String {
        self.aggregate
            .clone()
            .or_else(|| self.base.clone())
            .unwrap_or_else(|| self.url.request_url())
    }

    /// 接続直後に呼ぶ。OPTIONS を送る。
    pub fn start(&mut self, now: Instant) -> Result<()> {
        self.enqueue(
            now,
            Method::Options,
            self.url.request_url(),
            Headers::new(),
            Kind::Options { keepalive: false },
        )
    }

    pub fn describe(&mut self, now: Instant) -> Result<()> {
        let mut h = Headers::new();
        h.set("Accept", "application/sdp");
        self.enqueue(
            now,
            Method::Describe,
            self.url.request_url(),
            h,
            Kind::Describe,
        )
    }

    /// SETUP を送る。戻り値はトラック番号（[`ClientEvent::SetupDone`] の `track`）。
    pub fn setup(
        &mut self,
        now: Instant,
        control_url: &str,
        transport: TransportRequest,
    ) -> Result<usize> {
        let track = self.tracks;
        let mut h = Headers::new();
        h.set("Transport", transport.to_transport().to_string());
        self.enqueue(
            now,
            Method::Setup,
            control_url.to_string(),
            h,
            Kind::Setup { track },
        )?;
        self.tracks += 1;
        Ok(track)
    }

    /// PLAY を送る。`range` が `None` なら Range を付けない。
    pub fn play(&mut self, now: Instant, range: Option<Range>) -> Result<()> {
        self.need_session()?;
        let mut h = Headers::new();
        if let Some(r) = range {
            h.set("Range", r.to_string());
        }
        self.enqueue(now, Method::Play, self.aggregate_url(), h, Kind::Play)
    }

    pub fn pause(&mut self, now: Instant) -> Result<()> {
        self.need_session()?;
        self.enqueue(
            now,
            Method::Pause,
            self.aggregate_url(),
            Headers::new(),
            Kind::Pause,
        )
    }

    pub fn teardown(&mut self, now: Instant) -> Result<()> {
        self.need_session()?;
        self.enqueue(
            now,
            Method::Teardown,
            self.aggregate_url(),
            Headers::new(),
            Kind::Teardown,
        )
    }

    /// GET_PARAMETER（本文なし）を送る。
    pub fn get_parameter(&mut self, now: Instant) -> Result<()> {
        self.enqueue(
            now,
            Method::GetParameter,
            self.aggregate_url(),
            Headers::new(),
            Kind::GetParameter { keepalive: false },
        )
    }

    fn need_session(&self) -> Result<()> {
        if self.session.is_none() {
            return Err(Error::InvalidState("SETUP の前"));
        }
        Ok(())
    }

    fn enqueue(
        &mut self,
        now: Instant,
        method: Method,
        uri: String,
        headers: Headers,
        kind: Kind,
    ) -> Result<()> {
        if self.queue.len() >= MAX_QUEUE {
            return Err(Error::LimitExceeded("送信待ちの要求"));
        }
        self.queue.push_back(Queued {
            method,
            uri,
            headers,
            kind,
            auth_attempts: 0,
            stale_retried: false,
        });
        self.send_next(now)
    }

    fn send_next(&mut self, now: Instant) -> Result<()> {
        if self.inflight.is_some() || self.awaiting_reconnect.is_some() {
            return Ok(());
        }
        let Some(q) = self.queue.pop_front() else {
            return Ok(());
        };
        self.cseq = self
            .cseq
            .checked_add(1)
            .ok_or(Error::Protocol("CSeq の桁あふれ"))?;
        let mut req = Request::new(q.method.clone(), q.uri.clone());
        req.headers.set("CSeq", self.cseq.to_string());
        for (n, v) in q.headers.iter() {
            req.headers.set(n, v);
        }
        req.headers.set("User-Agent", self.cfg.user_agent.clone());
        if let Some(s) = &self.session {
            req.headers.set("Session", s.id.clone());
        }
        if let (Some(auth), Some(cred)) = (self.auth.as_mut(), self.credentials.as_ref()) {
            let cnonce = if auth.needs_cnonce() {
                match self.cfg.cnonce.as_mut() {
                    Some(f) => f(),
                    None => {
                        let h = Sha256::digest(
                            format!("{}:{}:{}", self.cseq, q.uri, q.method).as_bytes(),
                        );
                        h[..12].iter().map(|b| format!("{b:02x}")).collect()
                    }
                }
            } else {
                String::new()
            };
            let v = auth.authorization(cred, q.method.as_str(), &q.uri, &cnonce)?;
            req.headers.set("Authorization", v);
        }
        req.encode(&mut self.out)?;
        self.inflight = Some(InFlight {
            cseq: self.cseq,
            q,
            deadline: now + self.cfg.request_timeout,
        });
        Ok(())
    }

    /// 受信したバイト列を渡す。`Err` なら接続を閉じる。
    pub fn feed(&mut self, now: Instant, data: &[u8]) -> Result<()> {
        for chunk in data.chunks(FEED_CHUNK) {
            self.decoder.feed(chunk)?;
            while let Some(frame) = self.decoder.decode()? {
                match frame {
                    Frame::Interleaved { channel, data } => self
                        .events
                        .push_back(ClientEvent::Interleaved { channel, data }),
                    Frame::Response(r) => self.on_response(now, r)?,
                    Frame::Request(r) => self.on_server_request(r)?,
                }
            }
        }
        Ok(())
    }

    fn on_server_request(&mut self, r: Request) -> Result<()> {
        let status = match r.method {
            Method::Options | Method::GetParameter | Method::SetParameter => 200,
            _ => 501,
        };
        let mut resp = Response::new(status);
        if let Some(c) = r.cseq() {
            resp.headers.set("CSeq", c.to_string());
        }
        resp.encode(&mut self.out)
    }

    fn fail(&mut self, method: Method, status: u16, reason: impl Into<String>) {
        self.events.push_back(ClientEvent::Failed {
            method,
            status,
            reason: reason.into(),
        });
    }

    fn on_response(&mut self, now: Instant, r: Response) -> Result<()> {
        let inflight = self
            .inflight
            .take()
            .ok_or(Error::Protocol("要求していない応答"))?;
        if r.cseq() != Some(inflight.cseq) {
            return Err(Error::Protocol("CSeq が一致しない"));
        }
        let mut q = inflight.q;
        // Session の一致を確かめる
        if let (Some(cur), Some(v)) = (&self.session, r.headers.get("Session"))
            && SessionHeader::parse(v)?.id != cur.id
        {
            return Err(Error::Protocol("Session ID が変わった"));
        }
        match r.status {
            401 => {
                self.on_unauthorized(now, q, &r)?;
                return self.send_next(now);
            }
            301 | 302 | 303 | 305 | 307 => {
                self.on_redirect(q, &r)?;
                return self.send_next(now);
            }
            _ => {}
        }
        if !r.is_success() {
            match q.kind {
                Kind::GetParameter { keepalive: true } => self.use_get_parameter = false,
                Kind::Options { keepalive: true } => {}
                Kind::Teardown => {
                    self.session = None;
                    self.keepalive_at = None;
                    self.events.push_back(ClientEvent::TornDown);
                }
                _ => self.fail(q.method.clone(), r.status, r.reason.clone()),
            }
            self.touch(now);
            return self.send_next(now);
        }
        match &mut q.kind {
            Kind::Options { keepalive } => {
                let public = r
                    .headers
                    .get("Public")
                    .map(parse_public)
                    .unwrap_or_default();
                self.use_get_parameter = public.contains(&Method::GetParameter);
                if !*keepalive {
                    self.events.push_back(ClientEvent::Options {
                        public: public.clone(),
                    });
                }
                self.public = public;
            }
            Kind::Describe => {
                let ct = r
                    .headers
                    .get("Content-Type")
                    .map(|s| s.to_ascii_lowercase());
                if ct
                    .as_deref()
                    .is_some_and(|c| !c.starts_with("application/sdp"))
                {
                    self.fail(
                        Method::Describe,
                        r.status,
                        "Content-Type が application/sdp ではない",
                    );
                } else if r.body.is_empty() {
                    self.fail(Method::Describe, r.status, "SDP が空");
                } else {
                    let base = content_base(
                        &q.uri,
                        r.headers.get("Content-Base"),
                        r.headers.get("Content-Location"),
                    );
                    self.base = Some(base.clone());
                    self.events.push_back(ClientEvent::Described {
                        content_base: base,
                        sdp: r.body.clone(),
                    });
                }
            }
            Kind::Setup { track } => {
                let s = SessionHeader::parse(
                    r.headers
                        .get("Session")
                        .ok_or(Error::Protocol("SETUP の応答に Session がない"))?,
                )?;
                let t = r
                    .headers
                    .get("Transport")
                    .ok_or(Error::Protocol("SETUP の応答に Transport がない"))?;
                let transport = Transport::parse_list(t)?
                    .into_iter()
                    .next()
                    .ok_or(Error::Protocol("Transport が空"))?;
                if self.session.is_none() {
                    self.session = Some(s.clone());
                }
                self.events.push_back(ClientEvent::SetupDone {
                    track: *track,
                    control_url: q.uri.clone(),
                    transport,
                    session: s.id,
                });
            }
            Kind::Play => {
                let range = r.headers.get("Range").and_then(|v| Range::parse(v).ok());
                let rtp_info = r
                    .headers
                    .get("RTP-Info")
                    .and_then(|v| parse_rtp_info(v).ok())
                    .unwrap_or_default();
                self.events
                    .push_back(ClientEvent::Playing { range, rtp_info });
            }
            Kind::Pause => self.events.push_back(ClientEvent::Paused),
            Kind::Teardown => {
                self.session = None;
                self.keepalive_at = None;
                self.events.push_back(ClientEvent::TornDown);
            }
            Kind::GetParameter { keepalive } => {
                if !*keepalive {
                    self.events.push_back(ClientEvent::Parameter {
                        body: r.body.clone(),
                    });
                }
            }
        }
        self.touch(now);
        self.send_next(now)
    }

    fn touch(&mut self, now: Instant) {
        self.keepalive_at = self.session.as_ref().map(|s| {
            let t = s.timeout_secs().max(2);
            now + Duration::from_secs(u64::from(t) / 2)
        });
    }

    fn on_unauthorized(&mut self, _now: Instant, mut q: Queued, r: &Response) -> Result<()> {
        let method = q.method.clone();
        if self.credentials.is_none() {
            self.fail(method, 401, "資格情報がない");
            return Ok(());
        }
        let challenges = parse_challenges(r.headers.get_all("WWW-Authenticate"));
        let stale = challenges
            .iter()
            .any(|c| matches!(c, crate::auth::Challenge::Digest(d) if d.stale));
        if q.auth_attempts >= 1 && self.auth.is_some() {
            if !(stale && !q.stale_retried) {
                self.auth = None;
                self.fail(method, 401, "認証に失敗した");
                return Ok(());
            }
            q.stale_retried = true;
        }
        match ClientAuth::from_challenges(&challenges, self.cfg.allow_basic) {
            Ok(a) => self.auth = Some(a),
            Err(e) => {
                self.fail(method, 401, e.to_string());
                return Ok(());
            }
        }
        q.auth_attempts = q.auth_attempts.saturating_add(1);
        self.queue.push_front(q);
        Ok(())
    }

    fn on_redirect(&mut self, mut q: Queued, r: &Response) -> Result<()> {
        let method = q.method.clone();
        let Some(loc) = r.headers.get("Location") else {
            self.fail(method, r.status, "Location がないリダイレクト");
            return Ok(());
        };
        self.redirects += 1;
        if self.redirects > self.cfg.max_redirects {
            self.fail(method, r.status, "リダイレクトが多すぎる");
            return Ok(());
        }
        let target = resolve_control(&q.uri, loc);
        let mut new_url = RtspUrl::parse(&target)?;
        new_url.credentials = None;
        let old_req = self.url.request_url();
        q.uri = new_url.request_url();
        let same = new_url.same_origin(&self.url);
        if !same {
            // 接続先が変わる: 認証とセッションは持ち越さない
            self.url = new_url;
            self.auth = None;
            self.session = None;
            self.keepalive_at = None;
            self.base = None;
            q.auth_attempts = 0;
            self.awaiting_reconnect = Some(q);
            self.events.push_back(ClientEvent::Reconnect {
                url: self.url.request_url(),
            });
        } else {
            if self.url.request_url() == old_req
                && matches!(q.kind, Kind::Options { .. } | Kind::Describe)
            {
                self.url = new_url;
            }
            self.queue.push_front(q);
        }
        Ok(())
    }

    /// [`ClientEvent::Reconnect`] のあと、新しい接続ができたら呼ぶ。リダイレクトされた要求を送り直す。
    pub fn reconnected(&mut self, now: Instant) -> Result<()> {
        let q = self
            .awaiting_reconnect
            .take()
            .ok_or(Error::InvalidState("再接続を待っていない"))?;
        self.decoder = Decoder::new(self.cfg.limits);
        self.inflight = None;
        self.queue.push_front(q);
        self.send_next(now)
    }

    pub fn poll_event(&mut self) -> Option<ClientEvent> {
        self.events.pop_front()
    }

    /// 送るバイト列（空なら何もしない）。
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// interleaved で RTCP（受信報告など）を送る。
    pub fn send_interleaved(&mut self, channel: u8, data: &[u8]) -> Result<()> {
        crate::message::write_interleaved(channel, data, &mut self.out)
    }

    /// 次に [`handle_timeout`](Self::handle_timeout) を呼ぶ時刻。
    pub fn poll_timeout(&self) -> Option<Instant> {
        let a = self.inflight.as_ref().map(|i| i.deadline);
        match (a, self.keepalive_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// 時間経過の処理。応答の期限切れは `Err(Error::Timeout)`。keepalive の時刻なら送る。
    pub fn handle_timeout(&mut self, now: Instant) -> Result<()> {
        if let Some(i) = &self.inflight
            && now >= i.deadline
        {
            return Err(Error::Timeout);
        }
        if let Some(k) = self.keepalive_at
            && now >= k
            && self.inflight.is_none()
            && self.queue.is_empty()
            && self.session.is_some()
        {
            self.keepalive_at = None;
            let uri = self.aggregate_url();
            if self.use_get_parameter {
                self.enqueue(
                    now,
                    Method::GetParameter,
                    uri,
                    Headers::new(),
                    Kind::GetParameter { keepalive: true },
                )?;
            } else {
                self.enqueue(
                    now,
                    Method::Options,
                    uri,
                    Headers::new(),
                    Kind::Options { keepalive: true },
                )?;
            }
        }
        Ok(())
    }

    /// サーバーの Public（OPTIONS の応答）。
    pub fn public_methods(&self) -> &[Method] {
        &self.public
    }

    /// Session の timeout（秒、SETUP 前は既定値）。
    pub fn session_timeout(&self) -> u32 {
        self.session
            .as_ref()
            .map_or(DEFAULT_SESSION_TIMEOUT, |s| s.timeout_secs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(cseq: u32, status: u16, extra: &[(&str, &str)], body: &str) -> Vec<u8> {
        let mut r = Response::new(status);
        r.headers.set("CSeq", cseq.to_string());
        for (n, v) in extra {
            r.headers.append(n, *v);
        }
        r.body = Bytes::copy_from_slice(body.as_bytes());
        let mut o = Vec::new();
        r.encode(&mut o).unwrap();
        o
    }

    fn sent(c: &mut ClientSession) -> Request {
        let out = c.take_output();
        let mut d = Decoder::new(Limits::default());
        d.feed(&out).unwrap();
        match d.decode().unwrap() {
            Some(Frame::Request(r)) => r,
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn digest_flow_mediamtx_style() {
        let now = Instant::now();
        let mut c =
            ClientSession::new(ClientConfig::new("rtsp://user:pass@127.0.0.1:8554/cam")).unwrap();
        c.describe(now).unwrap();
        let r = sent(&mut c);
        assert_eq!(r.uri, "rtsp://127.0.0.1:8554/cam");
        assert!(r.headers.get("Authorization").is_none());
        c.feed(
            now,
            &resp(
                1,
                401,
                &[
                    ("WWW-Authenticate", "Basic realm=\"mediamtx\""),
                    (
                        "WWW-Authenticate",
                        "Digest realm=\"mediamtx\", nonce=\"0123abcd\"",
                    ),
                ],
                "",
            ),
        )
        .unwrap();
        let r = sent(&mut c);
        assert_eq!(r.cseq(), Some(2));
        let a = r.headers.get("Authorization").unwrap();
        assert!(a.starts_with("Digest "));
        assert!(
            crate::auth::verify_digest(
                "DESCRIBE",
                &r.uri,
                a,
                "mediamtx",
                |n| n == "0123abcd",
                |_| Some("pass".into())
            )
            .is_ok()
        );
        c.feed(
            now,
            &resp(
                2,
                200,
                &[
                    ("Content-Type", "application/sdp"),
                    ("Content-Base", "rtsp://127.0.0.1:8554/cam/"),
                ],
                "v=0\r\n",
            ),
        )
        .unwrap();
        assert_eq!(
            c.poll_event(),
            Some(ClientEvent::Described {
                content_base: "rtsp://127.0.0.1:8554/cam/".into(),
                sdp: Bytes::from_static(b"v=0\r\n")
            })
        );
        // 後続の要求にも Authorization が付く
        let ctl = c.resolve_control("trackID=0");
        c.setup(
            now,
            &ctl,
            TransportRequest::Interleaved {
                rtp_channel: 0,
                rtcp_channel: 1,
            },
        )
        .unwrap();
        let r = sent(&mut c);
        assert_eq!(r.uri, "rtsp://127.0.0.1:8554/cam/trackID=0");
        assert!(r.headers.get("Authorization").is_some());
        assert_eq!(
            r.headers.get("Transport"),
            Some("RTP/AVP/TCP;unicast;interleaved=0-1")
        );
    }

    #[test]
    fn second_401_fails_and_basic_refused() {
        let now = Instant::now();
        let mut c = ClientSession::new(ClientConfig::new("rtsp://u:p@h/x")).unwrap();
        c.describe(now).unwrap();
        sent(&mut c);
        c.feed(
            now,
            &resp(1, 401, &[("WWW-Authenticate", "Basic realm=\"x\"")], ""),
        )
        .unwrap();
        assert!(matches!(
            c.poll_event(),
            Some(ClientEvent::Failed { status: 401, .. })
        ));
        assert!(c.take_output().is_empty());

        let mut cfg = ClientConfig::new("rtsp://u:p@h/x");
        cfg.allow_basic = true;
        let mut c = ClientSession::new(cfg).unwrap();
        c.describe(now).unwrap();
        sent(&mut c);
        c.feed(
            now,
            &resp(1, 401, &[("WWW-Authenticate", "Basic realm=\"x\"")], ""),
        )
        .unwrap();
        let r = sent(&mut c);
        assert_eq!(r.headers.get("Authorization"), Some("Basic dTpw"));
        c.feed(
            now,
            &resp(2, 401, &[("WWW-Authenticate", "Basic realm=\"x\"")], ""),
        )
        .unwrap();
        assert!(matches!(
            c.poll_event(),
            Some(ClientEvent::Failed { status: 401, .. })
        ));

        // stale=true は 1 回だけ送り直す
        let mut c = ClientSession::new(ClientConfig::new("rtsp://u:p@h/x")).unwrap();
        c.describe(now).unwrap();
        sent(&mut c);
        c.feed(
            now,
            &resp(
                1,
                401,
                &[("WWW-Authenticate", "Digest realm=\"r\", nonce=\"a\"")],
                "",
            ),
        )
        .unwrap();
        sent(&mut c);
        c.feed(
            now,
            &resp(
                2,
                401,
                &[(
                    "WWW-Authenticate",
                    "Digest realm=\"r\", nonce=\"b\", stale=TRUE",
                )],
                "",
            ),
        )
        .unwrap();
        let r = sent(&mut c);
        assert!(
            r.headers
                .get("Authorization")
                .unwrap()
                .contains("nonce=\"b\"")
        );
        c.feed(
            now,
            &resp(
                3,
                401,
                &[(
                    "WWW-Authenticate",
                    "Digest realm=\"r\", nonce=\"c\", stale=TRUE",
                )],
                "",
            ),
        )
        .unwrap();
        assert!(matches!(
            c.poll_event(),
            Some(ClientEvent::Failed { status: 401, .. })
        ));
    }

    #[test]
    fn redirects() {
        let now = Instant::now();
        let mut cfg = ClientConfig::new("rtsp://h1/a");
        cfg.max_redirects = 2;
        let mut c = ClientSession::new(cfg).unwrap();
        c.describe(now).unwrap();
        sent(&mut c);
        // 同じ接続先
        c.feed(now, &resp(1, 302, &[("Location", "rtsp://h1/b")], ""))
            .unwrap();
        let r = sent(&mut c);
        assert_eq!(r.uri, "rtsp://h1/b");
        // 別の接続先 → Reconnect
        c.feed(now, &resp(2, 301, &[("Location", "rtsp://h2:8554/c")], ""))
            .unwrap();
        assert_eq!(
            c.poll_event(),
            Some(ClientEvent::Reconnect {
                url: "rtsp://h2:8554/c".into()
            })
        );
        assert!(c.take_output().is_empty());
        c.reconnected(now).unwrap();
        let r = sent(&mut c);
        assert_eq!(r.uri, "rtsp://h2:8554/c");
        assert_eq!(c.url().host_port(), "h2:8554");
        // 上限
        c.feed(now, &resp(3, 302, &[("Location", "rtsp://h2:8554/d")], ""))
            .unwrap();
        assert!(matches!(
            c.poll_event(),
            Some(ClientEvent::Failed { status: 302, .. })
        ));
    }

    #[test]
    fn cseq_and_session_checks() {
        let now = Instant::now();
        let mut c = ClientSession::new(ClientConfig::new("rtsp://h/a")).unwrap();
        c.setup(
            now,
            "rtsp://h/a/t0",
            TransportRequest::Udp {
                rtp_port: 5000,
                rtcp_port: 5001,
            },
        )
        .unwrap();
        c.setup(
            now,
            "rtsp://h/a/t1",
            TransportRequest::Udp {
                rtp_port: 5002,
                rtcp_port: 5003,
            },
        )
        .unwrap();
        let r = sent(&mut c);
        assert_eq!(
            r.headers.get("Transport"),
            Some("RTP/AVP;unicast;client_port=5000-5001")
        );
        assert!(r.headers.get("Session").is_none());
        // 1 つずつ送る
        assert!(c.take_output().is_empty());
        c.feed(
            now,
            &resp(
                1,
                200,
                &[
                    ("Session", "ABC;timeout=30"),
                    (
                        "Transport",
                        "RTP/AVP;unicast;client_port=5000-5001;server_port=6000-6001;ssrc=1234ABCD",
                    ),
                ],
                "",
            ),
        )
        .unwrap();
        let Some(ClientEvent::SetupDone {
            track: 0,
            transport,
            session,
            ..
        }) = c.poll_event()
        else {
            panic!()
        };
        assert_eq!(transport.server_port, Some((6000, 6001)));
        assert_eq!(transport.ssrc, Some(0x1234_ABCD));
        assert_eq!(session, "ABC");
        let r = sent(&mut c);
        assert_eq!(r.headers.get("Session"), Some("ABC"));
        // 応答待ち（10 秒）と keepalive（15 秒）の早い方
        assert_eq!(c.poll_timeout(), Some(now + Duration::from_secs(10)));
        // 違う Session は拒否
        assert!(
            c.feed(
                now,
                &resp(2, 200, &[("Session", "XYZ"), ("Transport", "RTP/AVP")], "")
            )
            .is_err()
        );

        let mut c = ClientSession::new(ClientConfig::new("rtsp://h/a")).unwrap();
        c.start(now).unwrap();
        sent(&mut c);
        assert!(c.feed(now, &resp(7, 200, &[], "")).is_err());
        let mut c = ClientSession::new(ClientConfig::new("rtsp://h/a")).unwrap();
        assert!(c.feed(now, &resp(1, 200, &[], "")).is_err());
        assert!(c.play(now, None).is_err());
    }

    #[test]
    fn keepalive_and_timeout() {
        let now = Instant::now();
        let mut c = ClientSession::new(ClientConfig::new("rtsp://h/a")).unwrap();
        c.start(now).unwrap();
        sent(&mut c);
        c.feed(
            now,
            &resp(
                1,
                200,
                &[("Public", "OPTIONS, DESCRIBE, SETUP, PLAY, GET_PARAMETER")],
                "",
            ),
        )
        .unwrap();
        assert!(matches!(c.poll_event(), Some(ClientEvent::Options { .. })));
        c.setup(
            now,
            "rtsp://h/a/t",
            TransportRequest::Interleaved {
                rtp_channel: 0,
                rtcp_channel: 1,
            },
        )
        .unwrap();
        sent(&mut c);
        c.feed(
            now,
            &resp(
                2,
                200,
                &[
                    ("Session", "S;timeout=20"),
                    ("Transport", "RTP/AVP/TCP;interleaved=0-1"),
                ],
                "",
            ),
        )
        .unwrap();
        c.poll_event();
        assert_eq!(c.poll_timeout(), Some(now + Duration::from_secs(10)));
        c.handle_timeout(now + Duration::from_secs(5)).unwrap();
        assert!(c.take_output().is_empty());
        let t = now + Duration::from_secs(10);
        c.handle_timeout(t).unwrap();
        let r = sent(&mut c);
        assert_eq!(r.method, Method::GetParameter);
        assert_eq!(r.headers.get("Session"), Some("S"));
        // 405 なら OPTIONS に切り替える（イベントは出さない）
        c.feed(t, &resp(3, 405, &[], "")).unwrap();
        assert_eq!(c.poll_event(), None);
        let t2 = t + Duration::from_secs(10);
        c.handle_timeout(t2).unwrap();
        assert_eq!(sent(&mut c).method, Method::Options);
        // 応答がなければ期限切れ
        assert_eq!(
            c.handle_timeout(t2 + Duration::from_secs(11)),
            Err(Error::Timeout)
        );
    }

    #[test]
    fn interleaved_mixed_and_server_requests() {
        let now = Instant::now();
        let mut c = ClientSession::new(ClientConfig::new("rtsp://h/a")).unwrap();
        c.start(now).unwrap();
        sent(&mut c);
        let mut stream = Vec::new();
        crate::message::write_interleaved(0, &[1, 2, 3], &mut stream).unwrap();
        stream.extend_from_slice(&resp(1, 200, &[], ""));
        stream.extend_from_slice(b"GET_PARAMETER rtsp://h/a RTSP/1.0\r\nCSeq: 9\r\n\r\n");
        crate::message::write_interleaved(1, &[4], &mut stream).unwrap();
        for b in &stream {
            c.feed(now, &[*b]).unwrap();
        }
        let ev: Vec<_> = std::iter::from_fn(|| c.poll_event()).collect();
        assert_eq!(ev.len(), 3);
        assert_eq!(
            ev[0],
            ClientEvent::Interleaved {
                channel: 0,
                data: Bytes::from_static(&[1, 2, 3])
            }
        );
        let out = String::from_utf8(c.take_output()).unwrap();
        assert!(out.starts_with("RTSP/1.0 200 OK\r\nCSeq: 9\r\n"));
    }

    #[test]
    fn describe_failures() {
        let now = Instant::now();
        let mut c = ClientSession::new(ClientConfig::new("rtsp://h/a")).unwrap();
        c.describe(now).unwrap();
        sent(&mut c);
        c.feed(now, &resp(1, 200, &[("Content-Type", "text/html")], "x"))
            .unwrap();
        assert!(matches!(c.poll_event(), Some(ClientEvent::Failed { .. })));
        c.describe(now).unwrap();
        sent(&mut c);
        c.feed(now, &resp(2, 404, &[], "")).unwrap();
        assert!(matches!(
            c.poll_event(),
            Some(ClientEvent::Failed { status: 404, .. })
        ));
        // Content-Base がなければ要求 URL
        c.describe(now).unwrap();
        sent(&mut c);
        c.feed(
            now,
            &resp(3, 200, &[("Content-Type", "application/sdp")], "v=0\r\n"),
        )
        .unwrap();
        assert!(
            matches!(c.poll_event(), Some(ClientEvent::Described { content_base, .. }) if content_base == "rtsp://h/a")
        );
        assert!(!format!("{:?}", ClientConfig::new("rtsp://u:secretpw@h/")).contains("secretpw"));
    }
}
