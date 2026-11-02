//! HTTP サーバー（管理 API・ライブ HLS・VOD・SSE・管理画面）。HTTP の解析は `senca-http`（sans-io）。
//!
//! パス:
//! - `/api/v1/...` 管理 API（`GET /api/v1/events` は SSE）
//! - `/s/{exp}/{sig}/live/{stream}/{file}` 署名付きのライブ HLS（メモリから）
//! - `/s/{exp}/{sig}/vod/{recording}/{file}` 署名付きの録画（VOD）
//! - `/healthz`・`/readyz`
//! - `/console/...` 管理画面（静的ファイル）
//! - `/whip/...`・`/whep/...` WebRTC（M8）

use crate::api::{self, ApiRequest, ApiResponse};
use crate::app::App;
use bytes::{Bytes, BytesMut};
use senca_http::{Event, Limits, Method, RequestHead, RequestParser, Response};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const IDLE: Duration = Duration::from_secs(60);
/// ブロッキング・リロードで待つ最長（LL-HLS）。
const BLOCK_MAX: Duration = Duration::from_secs(6);

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Reply {
    fn new(status: u16, ct: &str, body: Bytes) -> Self {
        Reply { status, headers: vec![("Content-Type".into(), ct.into())], body }
    }
    fn status(status: u16) -> Self {
        Reply::new(status, "text/plain; charset=utf-8", Bytes::from(senca_http::response::reason_phrase(status)))
    }
    fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

impl From<ApiResponse> for Reply {
    fn from(r: ApiResponse) -> Self {
        Reply { status: r.status, headers: r.headers, body: r.body }
    }
}

pub async fn serve<S>(app: Arc<App>, mut sock: S, peer: String)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut parser = RequestParser::new(Limits::DEFAULT);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // 1 要求を読む
        let mut head: Option<RequestHead> = None;
        let mut body = BytesMut::new();
        let done = loop {
            match parser.poll() {
                Ok(Event::NeedMore) => {
                    let n = match tokio::time::timeout(IDLE, sock.read(&mut buf)).await {
                        Ok(Ok(0)) | Err(_) => {
                            parser.feed_eof();
                            if head.is_none() {
                                return;
                            }
                            continue;
                        }
                        Ok(Ok(n)) => n,
                        Ok(Err(_)) => return,
                    };
                    if let Err(e) = parser.feed(&buf[..n]) {
                        let _ = write_reply(&mut sock, Reply::status(e.status()), senca_http::Version::Http11, false, false).await;
                        return;
                    }
                }
                Ok(Event::Head(h)) => {
                    // 本文の上限は経路で決める（録画の書き出しなどは無し、WHIP の SDP は 256 KiB）
                    let limit = if h.path().starts_with("/whip") || h.path().starts_with("/whep") { 256 * 1024 } else { 1 << 20 };
                    parser.set_body_limit(limit);
                    if h.expect_continue {
                        let _ = sock.write_all(senca_http::CONTINUE_100).await;
                    }
                    head = Some(h);
                }
                Ok(Event::Data(d)) => body.extend_from_slice(&d),
                Ok(Event::End(_)) => break true,
                Ok(Event::Closed) => return,
                Err(e) => {
                    let _ = write_reply(&mut sock, Reply::status(e.status()), senca_http::Version::Http11, false, false).await;
                    return;
                }
            }
        };
        if !done {
            return;
        }
        let Some(h) = head else { return };
        let keep = h.keep_alive;
        let version = h.version;
        let head_only = h.method == Method::Head;
        // SSE は長く開いたままにする
        if h.method == Method::Get && h.path() == "/api/v1/events" {
            crate::events::serve_sse(&app, &mut sock, &h).await;
            return;
        }
        let reply = route(&app, &h, body.freeze(), &peer).await;
        if write_reply(&mut sock, reply, version, keep, head_only).await.is_err() || !keep {
            return;
        }
    }
}

async fn write_reply<S: AsyncWrite + Unpin>(sock: &mut S, r: Reply, v: senca_http::Version, keep: bool, head_only: bool) -> std::io::Result<()> {
    let mut resp = Response::new(r.status);
    for (k, val) in &r.headers {
        resp = match resp.header(k, Bytes::from(val.clone())) {
            Ok(x) => x,
            Err(_) => return Err(std::io::Error::other("bad header")),
        };
    }
    let enc = resp.encode(r.body, v, keep, head_only).map_err(|_| std::io::Error::other("encode"))?;
    // head と本文を連結しない
    let mut slices = [std::io::IoSlice::new(&enc.head), std::io::IoSlice::new(&enc.body)];
    let total = enc.head.len() + enc.body.len();
    let mut written = 0;
    let mut bufs: &mut [std::io::IoSlice] = &mut slices;
    while written < total {
        let n = sock.write_vectored(bufs).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        written += n;
        std::io::IoSlice::advance_slices(&mut bufs, n);
    }
    sock.flush().await
}

async fn route(app: &Arc<App>, h: &RequestHead, body: Bytes, peer: &str) -> Reply {
    let path = h.path();
    if let Some(rest) = path.strip_prefix("/api/v1") {
        let headers: Vec<(String, String)> =
            h.headers.iter().map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v).into_owned())).collect();
        let rid = crate::util::request_id(h.headers.get_str("x-request-id"));
        let req = ApiRequest {
            method: h.method.as_str().to_string(),
            path: if rest.is_empty() { "/".into() } else { rest.to_string() },
            query: h.query().unwrap_or("").to_string(),
            headers,
            body,
            request_id: rid,
        };
        let app2 = app.clone();
        // API は制御状態のロックとファイル I/O を使うので、ブロックしてよいスレッドで
        return match tokio::task::spawn_blocking(move || api::handle(&app2, req)).await {
            Ok(r) => r.into(),
            Err(_) => Reply::status(500),
        };
    }
    match path {
        "/healthz" => return Reply::new(200, "text/plain", Bytes::from_static(b"ok\n")),
        "/readyz" => {
            return if app.ready() { Reply::new(200, "text/plain", Bytes::from_static(b"ready\n")) } else { Reply::status(503) };
        }
        _ => {}
    }
    if let Some(rest) = path.strip_prefix("/s/") {
        return signed(app, rest, h).await;
    }
    if path.starts_with("/whip") || path.starts_with("/whep") {
        return app.webrtc_http(h, body, peer).await;
    }
    if path == "/console" || path.starts_with("/console/") {
        return console(app, path);
    }
    if path == "/sender" || path.starts_with("/sender/") {
        // ブラウザーからの送出（WHIP）のページ（apps/sender-web）
        let Some(dir) = app.cfg.sender_dir.as_ref() else { return Reply::status(404) };
        return static_files(dir, path.strip_prefix("/sender").unwrap_or(""));
    }
    if path == "/" {
        return Reply::status(302).header("Location", "/console/");
    }
    Reply::status(404)
}

/// `/s/{exp}/{sig}/{kind}/{id}/{file}`
async fn signed(app: &Arc<App>, rest: &str, h: &RequestHead) -> Reply {
    let segs: Vec<&str> = rest.split('/').collect();
    let [exp, sig, kind, id, file] = segs.as_slice() else { return Reply::status(404) };
    if file.is_empty() || file.contains("..") {
        return Reply::status(404);
    }
    let Ok(exp) = exp.parse::<i64>() else { return Reply::status(404) };
    if !app.verify_signature(kind, id, exp, sig) {
        return Reply::status(403);
    }
    if crate::util::now_ms() / 1000 > exp {
        return Reply::status(403);
    }
    let cors = |r: Reply| r.header("Access-Control-Allow-Origin", "*");
    // 分散: データを持つノードへ案内する（仕事はデータのある所で。REQ-010 R10-9）
    let elsewhere = match *kind {
        "live" if app.live_cache(id).is_none() => app.owner_base(id),
        "vod" if app.read_index(id).is_none() => app.recording_base(id),
        _ => None,
    };
    if let Some(base) = elsewhere {
        let q = h.query().map(|q| format!("?{q}")).unwrap_or_default();
        return cors(Reply::status(307).header("Location", format!("{base}/s/{rest}{q}")));
    }
    match *kind {
        "live" => cors(live(app, id, file, h).await),
        "vod" => cors(vod(app, id, file, h)),
        _ => Reply::status(404),
    }
}

async fn live(app: &Arc<App>, stream: &str, file: &str, h: &RequestHead) -> Reply {
    let Some(cache) = app.live_cache(stream) else { return Reply::status(404) };
    if file == "index.m3u8" {
        // LL-HLS のブロッキング・リロード
        let q = senca_http::pct::parse_query(h.query().unwrap_or("")).unwrap_or_default();
        let get = |k: &str| q.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        let skip = get("_HLS_skip").is_some_and(|v| v == "YES" || v == "v2");
        if let Some(msn) = get("_HLS_msn") {
            let Ok(msn) = msn.parse::<u64>() else { return Reply::status(400) };
            let part = match get("_HLS_part") {
                Some(p) => match p.parse::<u32>() {
                    Ok(p) => Some(p),
                    Err(_) => return Reply::status(400),
                },
                None => None,
            };
            let mut rx = cache.subscribe();
            let deadline = tokio::time::Instant::now() + BLOCK_MAX;
            loop {
                match cache.ready(msn, part) {
                    crate::live::Ready::Now => break,
                    crate::live::Ready::Bad => return Reply::status(400),
                    crate::live::Ready::Wait => {
                        if tokio::time::timeout_at(deadline, rx.changed()).await.is_err() {
                            return Reply::status(503).header("Retry-After", "1");
                        }
                    }
                }
            }
        }
        return match cache.playlist(skip) {
            Some(p) => Reply::new(200, "application/vnd.apple.mpegurl", p).header("Cache-Control", "no-cache"),
            None => Reply::status(404),
        };
    }
    if let Some((b, ct)) = cache.extra(file) {
        return Reply::new(200, ct, b).header("Cache-Control", "no-cache");
    }
    match cache.object(file) {
        Some(b) => {
            let ct = if file.ends_with(".ts") {
                "video/mp2t"
            } else if file.ends_with(".mp4") {
                "video/mp4"
            } else {
                "video/iso.segment"
            };
            Reply::new(200, ct, b).header("Cache-Control", "max-age=60")
        }
        None => Reply::status(404),
    }
}

fn vod(app: &Arc<App>, rec: &str, file: &str, h: &RequestHead) -> Reply {
    if file == "index.m3u8" {
        return match app.vod_playlist(rec) {
            Some(p) => Reply::new(200, "application/vnd.apple.mpegurl", Bytes::from(p)),
            None => Reply::status(404),
        };
    }
    let Some((ct, data)) = app.vod_object(rec, file) else { return Reply::status(404) };
    // Range（1 区間だけ）
    if let Some(r) = h.headers.get_str("range")
        && let senca_http::range::RangeParse::Single(spec) = senca_http::range::parse_range(r)
    {
        return match spec.resolve(data.len() as u64) {
            Some((a, b)) => Reply::new(206, ct, data.slice(a as usize..=b as usize))
                .header("Content-Range", format!("bytes {a}-{b}/{}", data.len()))
                .header("Accept-Ranges", "bytes"),
            None => Reply::status(416).header("Content-Range", format!("bytes */{}", data.len())),
        };
    }
    Reply::new(200, ct, data).header("Accept-Ranges", "bytes").header("Cache-Control", "max-age=3600")
}

fn console(app: &Arc<App>, path: &str) -> Reply {
    let Some(dir) = app.cfg.console_dir.as_ref() else { return Reply::status(404) };
    static_files(dir, path.strip_prefix("/console").unwrap_or(""))
}

fn static_files(dir: &std::path::Path, rel: &str) -> Reply {
    let rel = rel.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    if rel.split('/').any(|s| s.is_empty() || s == "." || s == ".." || s.starts_with('.')) {
        return Reply::status(404);
    }
    let p = dir.join(rel);
    // 単一ページアプリ: ファイルが無ければ index.html
    let (p, ext) = match std::fs::metadata(&p) {
        Ok(m) if m.is_file() => {
            let ext = rel.rsplit('.').next().unwrap_or("").to_string();
            (p, ext)
        }
        _ => (dir.join("index.html"), "html".to_string()),
    };
    let ct = match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "json" => "application/json",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    };
    match std::fs::read(&p) {
        Ok(b) => Reply::new(200, ct, Bytes::from(b))
            .header("X-Content-Type-Options", "nosniff")
            .header("Content-Security-Policy", "default-src 'self'; media-src 'self' blob: data: *; connect-src 'self' *; img-src 'self' data: blob: *; style-src 'self' 'unsafe-inline'"),
        Err(_) => Reply::status(404),
    }
}
