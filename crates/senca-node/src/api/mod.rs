//! 管理 API（`/api/v1`、仕様 10 章）。HTTP の読み書きは `http.rs`、ここは要求 → 応答の処理だけ。
//!
//! 共通の契約（仕様 10.1）:
//! - 本文は `application/json` のみ（415）。未知のフィールドは 400。
//! - POST は `Idempotency-Key` で 24 時間、同じ答えを返す（同じ鍵で別内容は 409）。秘密は伏せて覚える。
//! - 更新は `If-Match` 必須（428）、古ければ 412。ETag は `"{generation}"`。
//! - 一覧は `limit`（既定 100、最大 1000）と `cursor`。
//! - エラーは problem+json。`X-Request-Id` を返す。

pub mod audit;
pub mod io;
pub mod media_pipelines;
pub mod pipelines;
pub mod placement;
pub mod recordings;
pub mod rooms;
pub mod schedules;
pub mod streams;
pub mod system;
pub mod tokens;

use crate::app::App;
use crate::control::ControlCore;
use bytes::Bytes;
use senca_control::auth::{self, Principal, Scope};
use senca_control::model::IdempotencyRecord;
use senca_control::state::Command;
use senca_control::{Problem, ProblemCode};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub struct ApiRequest {
    pub method: String,
    /// `/api/v1` を除いたパス（例: `/streams/abc`）。
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    pub request_id: String,
    /// 送り元の IP（量の上限に使う）。
    pub peer_ip: String,
}

impl ApiRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn query_pairs(&self) -> Vec<(String, String)> {
        self.query
            .split('&')
            .filter(|s| !s.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (pct_decode(k), pct_decode(v))
            })
            .collect()
    }
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match h {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug)]
pub struct ApiResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl ApiResponse {
    pub fn json(status: u16, v: &Value) -> Self {
        ApiResponse {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: Bytes::from(serde_json::to_vec(v).unwrap_or_default()),
        }
    }

    pub fn empty(status: u16) -> Self {
        ApiResponse { status, headers: vec![], body: Bytes::new() }
    }

    pub fn bytes(status: u16, content_type: &str, body: Bytes) -> Self {
        ApiResponse { status, headers: vec![("Content-Type".into(), content_type.into())], body }
    }

    pub fn with_header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    pub fn problem(p: &Problem) -> Self {
        let mut r = ApiResponse {
            status: p.status,
            headers: vec![("Content-Type".into(), "application/problem+json".into())],
            body: Bytes::from(serde_json::to_vec(p).unwrap_or_default()),
        };
        if p.status == 401 {
            r.headers.push(("WWW-Authenticate".into(), "Bearer".into()));
        }
        if (p.status == 503 || p.status == 429 || p.status == 504) && p.retryable {
            r.headers.push(("Retry-After".into(), p.retry_after_secs().to_string()));
        }
        r
    }
}

pub type ApiResult = Result<ApiResponse, Problem>;

/// 処理の文脈。
pub struct Ctx<'a> {
    pub app: &'a App,
    pub req: &'a ApiRequest,
    pub principal: Principal,
    pub now_ms: i64,
}

impl Ctx<'_> {
    pub fn require(&self, s: Scope) -> Result<(), Problem> {
        if self.principal.has(s) {
            Ok(())
        } else {
            Err(Problem::new(ProblemCode::Forbidden, format!("権限 {} が必要です", s.as_str())))
        }
    }

    pub fn tenant(&self) -> &str {
        &self.principal.tenant
    }

    /// JSON の本文を読む（Content-Type を検査、未知のフィールドは型側の deny_unknown_fields で拒否）。
    pub fn body<T: DeserializeOwned>(&self) -> Result<T, Problem> {
        let ct = self.req.header("content-type").unwrap_or("");
        let mime = ct.split(';').next().unwrap_or("").trim();
        if !mime.eq_ignore_ascii_case("application/json") {
            return Err(Problem::new(ProblemCode::UnsupportedMediaType, "Content-Type は application/json にしてください"));
        }
        serde_json::from_slice(&self.req.body)
            .map_err(|e| Problem::invalid("本文が不正です").with_details(json!({ "reason": e.to_string() })))
    }

    /// 本文が空なら既定値。
    pub fn body_or_default<T: DeserializeOwned + Default>(&self) -> Result<T, Problem> {
        if self.req.body.is_empty() { Ok(T::default()) } else { self.body() }
    }

    /// `If-Match: "N"` を読む。必須なら無いと 428。
    pub fn if_match(&self, required: bool) -> Result<Option<u64>, Problem> {
        match self.req.header("if-match") {
            None if required => Err(Problem::new(ProblemCode::PreconditionRequired, "If-Match が必要です")),
            None => Ok(None),
            Some(v) => {
                let v = v.trim();
                let v = v.strip_prefix("W/").unwrap_or(v);
                v.strip_prefix('"')
                    .and_then(|x| x.strip_suffix('"'))
                    .and_then(|x| x.parse::<u64>().ok())
                    .map(Some)
                    .ok_or_else(|| Problem::new(ProblemCode::PreconditionFailed, "If-Match の形が違います"))
            }
        }
    }

    pub fn control(&self) -> std::sync::MutexGuard<'_, ControlCore> {
        self.app.control.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `limit` と `cursor` だけを受け付ける。
    pub fn page(&self) -> Result<(usize, Option<String>), Problem> {
        let mut limit = 100usize;
        let mut cursor = None;
        for (k, v) in self.req.query_pairs() {
            match k.as_str() {
                "limit" => {
                    limit = v
                        .parse::<usize>()
                        .ok()
                        .filter(|n| (1..=1000).contains(n))
                        .ok_or_else(|| Problem::invalid("limit は 1〜1000"))?
                }
                "cursor" => cursor = Some(v),
                "stream" | "state" if self.req.path == "/recordings" || self.req.path == "/schedules" => {}
                "room" if self.req.path == "/media-pipelines" => {}
                "since" | "until" | "action" | "subject" | "resource" | "q" | "tenant" | "node" if self.req.path == "/audit" => {}
                _ => return Err(Problem::invalid(format!("知らないクエリ: {k}"))),
            }
        }
        Ok((limit, cursor))
    }
}

/// ID 順の一覧を切り出す（カーソルは最後の ID）。
pub fn paginate<T>(items: Vec<(String, T)>, limit: usize, cursor: Option<&str>) -> (Vec<T>, Option<String>) {
    let mut out = Vec::new();
    let mut next = None;
    for (id, it) in items.into_iter().filter(|(id, _)| cursor.is_none_or(|c| id.as_str() > c)) {
        if out.len() == limit {
            next = Some(id);
            break;
        }
        out.push((id, it));
    }
    let next = next.and(out.last().map(|(id, _)| id.clone()));
    (out.into_iter().map(|(_, x)| x).collect(), next)
}

pub fn etag(gen_: u64) -> String {
    format!("\"{gen_}\"")
}

pub fn new_id() -> Result<String, Problem> {
    senca_core::Id128::random().map(|i| i.to_string()).map_err(|_| Problem::internal())
}

/// 入口。認証 → Idempotency → 振り分け。
pub fn handle(app: &App, req: ApiRequest) -> ApiResponse {
    let now_ms = crate::util::now_ms();
    let rid = req.request_id.clone();
    let mut resp = match handle_inner(app, &req, now_ms) {
        Ok(r) => r,
        Err(mut p) => {
            p.request_id = Some(rid.clone());
            ApiResponse::problem(&p)
        }
    };
    resp.headers.push(("X-Request-Id".into(), rid));
    resp.headers.push(("Cache-Control".into(), "no-store".into()));
    resp
}

fn handle_inner(app: &App, req: &ApiRequest, now_ms: i64) -> ApiResult {
    let principal = authenticate(app, req.header("authorization"), &req.peer_ip, &req.request_id, now_ms)?;
    // 主体ごとの量の上限（テナント＋主体。同じトークンを複数の IP から使っても同じバケット）
    if let Err(w) = app.limits.principal.take(&format!("{}\u{0}{}", principal.tenant, principal.subject), std::time::Instant::now()) {
        return Err(Problem::rate_limited("token", crate::ratelimit::retry_after_secs(w)));
    }
    let ctx = Ctx { app, req, principal, now_ms };

    if req.method == "POST"
        && let Some(key) = req.header("idempotency-key")
    {
        return idempotent(&ctx, key);
    }
    route(&ctx)
}

/// 認証する（認証の失敗の上限つき: 使い切った IP は、正しい資格情報でも 429）。SSE と共通。
pub fn authenticate(app: &App, header: Option<&str>, peer_ip: &str, request_id: &str, now_ms: i64) -> Result<Principal, Problem> {
    let now = std::time::Instant::now();
    if let Some(w) = app.limits.auth_failures.exhausted(peer_ip, now) {
        return Err(Problem::rate_limited("auth_failures", crate::ratelimit::retry_after_secs(w)));
    }
    let principal = {
        let core = app.control.lock().unwrap_or_else(|e| e.into_inner());
        auth::authenticate(header, &core.state, core.jwt.as_ref(), now_ms)
    };
    principal.map_err(|e| {
        app.log(&format!("auth failed: {e} request_id={request_id}"));
        let _ = app.limits.auth_failures.take(peer_ip, now);
        Problem::new(ProblemCode::AuthFailed, "認証に失敗しました")
    })
}

fn idempotent(ctx: &Ctx, key: &str) -> ApiResult {
    if key.is_empty() || key.len() > 255 || !key.bytes().all(|c| c.is_ascii_graphic()) {
        return Err(Problem::invalid("Idempotency-Key の形が違います"));
    }
    let mut h = Sha256::new();
    h.update(ctx.req.method.as_bytes());
    h.update(b"\n");
    h.update(ctx.req.path.as_bytes());
    h.update(b"?");
    h.update(ctx.req.query.as_bytes());
    h.update(b"\n");
    h.update(&ctx.req.body);
    let fp = senca_control::secret::hex(&h.finalize());
    // 同じ鍵の処理を直列にする（同時の再送で二重に作らない）
    let _guard = ctx.app.idem_lock.lock().unwrap_or_else(|e| e.into_inner());
    {
        let core = ctx.control();
        if let Some(rec) = core.state.idempotency(ctx.tenant(), key)
            && rec.created_at_ms + senca_control::state::IDEMPOTENCY_TTL_MS > ctx.now_ms
        {
            if rec.fingerprint != fp || rec.subject.as_deref().is_some_and(|s| s != ctx.principal.subject) {
                return Err(Problem::new(ProblemCode::IdempotencyConflict, "同じ Idempotency-Key で別の要求です"));
            }
            let ct = rec.content_type.clone().unwrap_or_else(|| "application/json".into());
            let mut body = Bytes::from(rec.body.clone());
            if ct.starts_with("application/problem+json")
                && let Ok(mut v) = serde_json::from_slice::<Value>(&body)
            {
                v["request_id"] = Value::String(ctx.req.request_id.clone());
                body = Bytes::from(serde_json::to_vec(&v).unwrap_or_default());
            }
            let mut r = ApiResponse::bytes(rec.status, &ct, body).with_header("Idempotency-Replayed", "true");
            if let Some(l) = &rec.location {
                r = r.with_header("Location", l.clone());
            }
            if let Some(e) = &rec.etag {
                r = r.with_header("ETag", e.clone());
            }
            return Ok(r);
        }
    }
    let result = route(ctx);
    let hdr = |r: &ApiResponse, k: &str| r.headers.iter().find(|(h, _)| h.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
    let (status, body, location, etag_v, ct) = match &result {
        Ok(r) => (r.status, redact_secrets(&r.body), hdr(r, "Location"), hdr(r, "ETag"), hdr(r, "Content-Type")),
        Err(p) if p.status >= 500 => return result,
        Err(p) => {
            let mut p = p.clone();
            p.request_id = None;
            (p.status, Bytes::from(serde_json::to_vec(&p).unwrap_or_default()), None, None, Some("application/problem+json".into()))
        }
    };
    if body.len() <= 256 * 1024 {
        let rec = IdempotencyRecord {
            tenant: ctx.tenant().to_string(),
            key: key.to_string(),
            fingerprint: fp,
            status,
            body: String::from_utf8_lossy(&body).into_owned(),
            location,
            etag: etag_v,
            subject: Some(ctx.principal.subject.clone()),
            content_type: ct,
            created_at_ms: ctx.now_ms,
        };
        // 期限切れの古い記録は先に消す
        let stale = ctx.control().state.idempotency(ctx.tenant(), key).is_some();
        if stale {
            let _ = ctx.app.propose(Command::ExpireIdempotency { before_ms: ctx.now_ms - senca_control::state::IDEMPOTENCY_TTL_MS });
        }
        if let Err(e) = ctx.app.propose(Command::PutIdempotency { record: rec }) {
            ctx.app.log(&format!("idempotency record not stored: {e}"));
        }
    }
    result
}

/// 覚える応答から秘密（`stream_key`・`token`）を消す。
fn redact_secrets(body: &Bytes) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else { return body.clone() };
    fn walk(v: &mut Value) {
        match v {
            Value::Object(m) => {
                for k in ["stream_key", "token", "secret"] {
                    if m.get(k).is_some_and(|x| !x.is_null()) {
                        m.insert(k.into(), Value::Null);
                        m.insert(format!("{k}_redacted"), Value::Bool(true));
                    }
                }
                for (_, x) in m.iter_mut() {
                    walk(x);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut v);
    Bytes::from(serde_json::to_vec(&v).unwrap_or_default())
}

/// パスを `/` で分ける。空の区切り（`//`）・`.`・`..` は 404。
fn segments(path: &str) -> Option<Vec<&str>> {
    let p = path.strip_prefix('/')?;
    let segs: Vec<&str> = if p.is_empty() { vec![] } else { p.split('/').collect() };
    if segs.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return None;
    }
    Some(segs)
}

fn route(ctx: &Ctx) -> ApiResult {
    let segs = segments(&ctx.req.path).ok_or_else(Problem::not_found)?;
    let m = ctx.req.method.as_str();
    let not_allowed = || Err(Problem::new(ProblemCode::MethodNotAllowed, "このメソッドは使えません"));
    match (m, segs.as_slice()) {
        ("GET", ["capabilities"]) => system::capabilities(ctx),
        ("GET", ["nodes"]) => system::nodes(ctx),
        ("GET", ["nodes", id]) => system::node(ctx, id),
        ("POST", ["nodes", id, "drain"]) => system::drain(ctx, id, true),
        ("POST", ["nodes", id, "undrain"]) => system::drain(ctx, id, false),
        ("GET", ["devices"]) => system::devices(ctx),
        ("GET", ["grid"]) => system::grid(ctx),
        ("POST", ["media-pipelines"]) => media_pipelines::create(ctx),
        ("GET", ["media-pipelines"]) => media_pipelines::list(ctx),
        ("GET", ["media-pipelines", id]) => media_pipelines::get(ctx, id),
        ("PATCH", ["media-pipelines", id]) => media_pipelines::patch(ctx, id),
        ("DELETE", ["media-pipelines", id]) => media_pipelines::delete(ctx, id),
        ("POST", ["rooms"]) => rooms::create(ctx),
        ("GET", ["rooms"]) => rooms::list(ctx),
        ("GET", ["rooms", id]) => rooms::get(ctx, id),
        ("PATCH", ["rooms", id]) => rooms::patch(ctx, id),
        ("DELETE", ["rooms", id]) => rooms::delete(ctx, id),
        ("POST", ["rooms", id, "tokens"]) => rooms::issue_token(ctx, id),
        ("GET", ["rooms", id, "participants"]) => rooms::participants(ctx, id),
        ("GET", ["rooms", id, "participants", c]) => rooms::participant(ctx, id, c),
        ("PATCH", ["rooms", id, "participants", c]) => rooms::update_participant(ctx, id, c),
        ("DELETE", ["rooms", id, "participants", c]) => rooms::disconnect_participant(ctx, id, c),
        ("POST", ["rooms", id, "messages"]) => rooms::post_message(ctx, id),
        ("GET", ["rooms", id, "stats"]) => rooms::stats(ctx, id),
        ("POST", ["nodes", id, "devices", dev, "disable"]) => system::device_health(ctx, id, dev, false),
        ("POST", ["nodes", id, "devices", dev, "enable"]) => system::device_health(ctx, id, dev, true),
        ("GET", ["events"]) => system::events_unsupported(),

        ("POST", ["streams"]) => streams::create(ctx),
        ("GET", ["streams"]) => streams::list(ctx),
        ("GET", ["streams", id]) => streams::get(ctx, id),
        ("PATCH", ["streams", id]) => streams::patch(ctx, id),
        ("DELETE", ["streams", id]) => streams::delete(ctx, id),
        ("POST", ["streams", id, "keys"]) => streams::rotate_key(ctx, id),
        ("POST", ["streams", id, "playback-url"]) => streams::playback_url(ctx, id),
        ("POST", ["streams", id, "recordings"]) => recordings::start(ctx, id),
        ("POST", ["streams", id, "plan"]) => streams::plan(ctx, id),
        ("GET", ["streams", id, "snapshot"]) => streams::snapshot(ctx, id),
        ("GET", ["streams", id, "snapshots"]) => streams::snapshots_list(ctx, id),
        ("GET", ["streams", id, "snapshots", sid]) => streams::snapshot_get(ctx, id, sid),
        ("DELETE", ["streams", id, "snapshots", sid]) => streams::snapshot_delete(ctx, id, sid),

        ("GET", ["recordings"]) => recordings::list(ctx),
        ("GET", ["recordings", id]) => recordings::get(ctx, id),
        ("PATCH", ["recordings", id]) => recordings::patch(ctx, id),
        ("DELETE", ["recordings", id]) => recordings::delete(ctx, id),
        ("POST", ["recordings", id, "stop"]) => recordings::stop(ctx, id),
        ("POST", ["recordings", id, "playback-url"]) => recordings::playback_url(ctx, id),
        ("GET", ["recordings", id, "thumbnail"]) => recordings::thumbnail(ctx, id),
        ("GET", ["recordings", id, "export.mp4"]) => recordings::export_mp4(ctx, id),

        ("POST", ["schedules"]) => schedules::create(ctx),
        ("POST", ["schedules", "check"]) => schedules::check(ctx),
        ("GET", ["schedules"]) => schedules::list(ctx),
        ("GET", ["schedules", id]) => schedules::get(ctx, id),
        ("PATCH", ["schedules", id]) => schedules::patch(ctx, id),
        ("DELETE", ["schedules", id]) => schedules::delete(ctx, id),
        ("GET", ["schedules", id, "runs"]) => schedules::runs(ctx, id),

        ("POST", ["streams", id, "inputs"]) => io::create_input(ctx, id),
        ("GET", ["streams", id, "inputs"]) => io::list_inputs(ctx, id),
        ("POST", ["streams", id, "outputs"]) => io::create_output(ctx, id),
        ("GET", ["streams", id, "outputs"]) => io::list_outputs(ctx, id),
        ("GET", ["inputs", id]) => io::get_input(ctx, id),
        ("PATCH", ["inputs", id]) => io::patch_input(ctx, id),
        ("DELETE", ["inputs", id]) => io::delete_input(ctx, id),
        ("GET", ["outputs", id]) => io::get_output(ctx, id),
        ("PATCH", ["outputs", id]) => io::patch_output(ctx, id),
        ("DELETE", ["outputs", id]) => io::delete_output(ctx, id),
        ("POST", ["secrets"]) => io::create_secret(ctx),
        ("GET", ["secrets"]) => io::list_secrets(ctx),
        ("DELETE", ["secrets", id]) => io::delete_secret(ctx, id),

        ("POST", ["pipelines"]) => pipelines::create(ctx),
        ("GET", ["pipelines"]) => pipelines::list(ctx),
        ("GET", ["pipelines", id]) => pipelines::get(ctx, id),
        ("PATCH", ["pipelines", id]) => pipelines::patch(ctx, id),
        ("DELETE", ["pipelines", id]) => pipelines::delete(ctx, id),
        ("POST", ["pipelines", id, "validate"]) => pipelines::validate(ctx, id),
        ("POST", ["pipelines", id, "control"]) => pipelines::control(ctx, id),
        ("GET", ["pipelines", id, "snapshots"]) => pipelines::snapshots_list(ctx, id),
        ("GET", ["pipelines", id, "snapshots", sid]) => pipelines::snapshot_get(ctx, id, sid),
        ("DELETE", ["pipelines", id, "snapshots", sid]) => pipelines::snapshot_delete(ctx, id, sid),

        ("GET", ["audit"]) => audit::list(ctx),

        ("POST", ["tokens"]) => tokens::create(ctx),
        ("GET", ["tokens"]) => tokens::list(ctx),
        ("GET", ["tokens", id]) => tokens::get(ctx, id),
        ("DELETE", ["tokens", id]) => tokens::delete(ctx, id),

        (
            _,
            ["capabilities"] | ["nodes", ..] | ["devices"] | ["events"] | ["streams", ..] | ["recordings", ..] | ["schedules", ..]
            | ["tokens", ..]
            | ["audit"]
            | ["inputs", ..]
            | ["outputs", ..]
            | ["secrets", ..]
            | ["pipelines", ..]
            | ["rooms", ..]
            | ["media-pipelines", ..]
            | ["grid"],
        ) => not_allowed(),
        _ => Err(Problem::not_found()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact() {
        let b = Bytes::from_static(br#"{"id":"x","ingest":{"stream_key":"sk_abc"},"token":null}"#);
        let v: Value = serde_json::from_slice(&redact_secrets(&b)).unwrap();
        assert_eq!(v["ingest"]["stream_key"], Value::Null);
        assert_eq!(v["ingest"]["stream_key_redacted"], Value::Bool(true));
        assert!(v.get("token_redacted").is_none());
    }

    #[test]
    fn paging() {
        let items: Vec<(String, u32)> = (0..5).map(|i| (format!("{i}"), i)).collect();
        let (a, n) = paginate(items.clone(), 2, None);
        assert_eq!(a, vec![0, 1]);
        assert_eq!(n.as_deref(), Some("1"));
        let (b, n) = paginate(items.clone(), 2, n.as_deref());
        assert_eq!(b, vec![2, 3]);
        let (c, n) = paginate(items, 2, n.as_deref());
        assert_eq!(c, vec![4]);
        assert_eq!(n, None);
    }

    #[test]
    fn segs() {
        assert_eq!(segments("/streams/a").unwrap(), vec!["streams", "a"]);
        assert!(segments("/streams//a").is_none());
        assert!(segments("/streams/../a").is_none());
        assert_eq!(pct_decode("a%20b+c%zz"), "a b c%zz");
    }
}
