//! 部屋の API（ADR-0015、docs/realtime/signaling.md の REST）。
//!
//! - 部屋そのもの（作る・読む・変える・消す）は制御状態の資源。どのノードでも受ける。
//! - 参加者・メッセージ・統計は、部屋を置いたノードの部屋の核へ。別のノードにあれば、そのノードへ中継する
//!   （同じ認証のまま。利用者は案内を意識しない）。
//! - 参加トークンはどのノードでも発行できる（鍵は制御状態から導く）。無い部屋のトークンを頼むと、部屋を
//!   自動で作る（空いてしばらくすると消える）。
//! - `PATCH /rooms/{id}` の `settings` は JSON Merge Patch（RFC 7396）: 書いた項目だけを変え、`null` で既定に戻す。
//! - 参加者の変更・切断は、参加者のつないだノード（カスケードの edge）で行う: home は edge へ同じ要求を送る。
//! - E2EE の部屋では、サーバーで中身を見る処理（合成・合成の録画・文字起こし）を 422（`details.reason: "e2ee"`）で断る。

use super::{ApiResponse, ApiResult, Ctx, etag, paginate};
use crate::rooms::{AUDIENCE, ISSUER, Location, RoomCmd, locate, signaling_url};
use senca_control::auth::Scope;
use senca_control::model::{PlacementSpec, Room};
use senca_control::state::Command;
use senca_control::{Problem, ProblemCode};
use senca_rooms::api::{ApiError, PostMessage, UpdateParticipant};
use senca_rooms::model::{RoomSettings, valid_room_id};
use senca_rooms::service::RoomService;
use senca_rooms::{JoinClaims, Permissions, Role};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const MAX_METADATA: usize = 8 * 1024;

fn bad(t: impl Into<String>) -> Problem {
    Problem::new(ProblemCode::InvalidConfig, t)
}

fn from_api_error(e: ApiError) -> Problem {
    let code = match e.status {
        400 => ProblemCode::InvalidRequest,
        404 => ProblemCode::NotFound,
        409 => ProblemCode::Conflict,
        412 => ProblemCode::PreconditionFailed,
        428 => ProblemCode::PreconditionRequired,
        422 => ProblemCode::UnsupportedCodec,
        503 => ProblemCode::CapacityUnavailable,
        _ => ProblemCode::InternalError,
    };
    let mut p = Problem::new(code, e.title).with_status(e.status);
    if !e.errors.is_empty() {
        p = p.with_details(json!({ "errors": e.errors }));
    }
    p
}

fn check_settings(v: &Value) -> Result<RoomSettings, Problem> {
    let s: RoomSettings = serde_json::from_value(v.clone()).map_err(|e| bad(format!("settings: {e}")))?;
    // E2EE の部屋はサーバーで中身を見られない: 合成の録画は 422（メディアパイプラインの合成・文字起こしと同じ）
    if s.e2ee && s.recording == senca_rooms::model::RecordingMode::Composite {
        return Err(e2ee_refused("合成の録画（settings.recording: composite）"));
    }
    // 部屋の核と同じ検査（ここで通したものは、置いたノードで必ず開ける）
    if let Err(errs) = s.validate() {
        let fields: Vec<String> = errs.iter().map(|e| format!("settings.{}", e.field)).collect();
        return Err(bad(format!("settings が不正です: {}", fields.join("、"))).with_details(json!({ "errors": errs })));
    }
    Ok(s)
}

/// E2EE の部屋で、サーバーで中身を見る処理を頼まれた（422、`details.reason: "e2ee"`）。
pub fn e2ee_refused(what: &str) -> Problem {
    Problem::new(ProblemCode::UnsupportedCodec, format!("E2EE の部屋では{what}はできません（中身はサーバーから見えない）"))
        .with_status(422)
        .with_details(json!({ "reason": "e2ee" }))
}

/// JSON Merge Patch（RFC 7396）: `patch` のオブジェクトの項目で置き換え、`null` の項目は消す（既定に戻る）。
pub fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(p) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Default::default());
    }
    let Value::Object(t) = target else { return };
    for (k, v) in p {
        if v.is_null() {
            t.remove(k);
        } else {
            merge_patch(t.entry(k.clone()).or_insert(Value::Null), v);
        }
    }
}

fn check_metadata(v: &Value) -> Result<(), Problem> {
    if serde_json::to_string(v).map(|s| s.len()).unwrap_or(usize::MAX) > MAX_METADATA {
        return Err(bad("metadata は 8 KiB まで"));
    }
    Ok(())
}

fn tenant_room(ctx: &Ctx, id: &str) -> Result<Room, Problem> {
    ctx.control().state.rooms.get(id).filter(|r| r.tenant == ctx.tenant()).cloned().ok_or_else(Problem::not_found)
}

/// 部屋の表示（制御状態 + 置き場所 + 手元にあれば今の様子）。
fn render(ctx: &Ctx, r: &Room) -> Value {
    let wid = format!("{}{}", senca_control::workloads::ROOM, r.id);
    let signaling = match locate(ctx.app, &r.id) {
        Location::Elsewhere { signaling, .. } => Some(signaling),
        _ => ctx.app.cfg.public.http.as_deref().map(signaling_url),
    };
    json!({
        "id": r.id,
        "generation": r.generation,
        "settings": r.settings,
        "metadata": r.metadata,
        "auto": r.auto,
        "created_at": crate::util::rfc3339(r.created_at_ms),
        "updated_at": crate::util::rfc3339(r.updated_at_ms),
        "signaling_url": signaling,
        "placement": r.placement,
        "placement_status": super::placement::status(ctx, &wid),
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    settings: Option<Value>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    placement: Option<PlacementSpec>,
}

fn new_room_id() -> Result<String, Problem> {
    let mut b = [0u8; 12];
    getrandom::fill(&mut b).map_err(|_| Problem::internal())?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

pub fn create(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let b: CreateBody = ctx.body()?;
    let id = match b.id {
        Some(id) if valid_room_id(&id) => id,
        Some(_) => return Err(bad("id は 1〜128 文字の英数字と -_.:@")),
        None => new_room_id()?,
    };
    let settings = b.settings.unwrap_or_else(|| json!({}));
    let s = check_settings(&settings)?;
    let metadata = b.metadata.unwrap_or(Value::Null);
    check_metadata(&metadata)?;
    let placement = super::placement::check(ctx, b.placement, None)?;
    let room = Room {
        id: id.clone(),
        tenant: ctx.tenant().into(),
        generation: 1,
        settings: serde_json::to_value(&s).unwrap_or(settings),
        metadata,
        placement,
        auto: false,
        created_at_ms: ctx.now_ms,
        updated_at_ms: ctx.now_ms,
    };
    if ctx.control().state.rooms.contains_key(&id) {
        return Err(Problem::new(ProblemCode::Conflict, "その ID の部屋はあります"));
    }
    ctx.app.propose(Command::PutRoom { room, expect_generation: None })?;
    ctx.app.audit(ctx, "room.create", &id);
    let r = tenant_room(ctx, &id)?;
    Ok(ApiResponse::json(201, &render(ctx, &r)).with_header("Location", format!("/api/v1/rooms/{id}")).with_header("ETag", etag(r.generation)))
}

pub fn list(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let (limit, cursor) = ctx.page()?;
    let items: Vec<(String, Room)> = ctx.control().state.rooms.values().filter(|r| r.tenant == ctx.tenant()).map(|r| (r.id.clone(), r.clone())).collect();
    let (page, next) = paginate(items, limit, cursor.as_deref());
    Ok(ApiResponse::json(200, &json!({ "items": page.iter().map(|r| render(ctx, r)).collect::<Vec<_>>(), "next_cursor": next })))
}

pub fn get(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let r = tenant_room(ctx, id)?;
    Ok(ApiResponse::json(200, &render(ctx, &r)).with_header("ETag", etag(r.generation)))
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    #[serde(default)]
    settings: Option<Value>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default, deserialize_with = "double_option")]
    placement: Option<Option<PlacementSpec>>,
}

fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::deserialize(d)?))
}

pub fn patch(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let expect = ctx.if_match(true)?.unwrap_or(0);
    let b: PatchBody = ctx.body()?;
    let mut r = tenant_room(ctx, id)?;
    if r.generation != expect {
        return Err(Problem::new(ProblemCode::PreconditionFailed, "資源が更新されています（If-Match が古い）"));
    }
    if let Some(patch) = b.settings {
        // 書いた項目だけを変える（JSON Merge Patch。null で既定に戻す）
        if !patch.is_object() {
            return Err(bad("settings はオブジェクト（変える項目だけを書く。null で既定に戻す）"));
        }
        let mut merged = r.settings.clone();
        merge_patch(&mut merged, &patch);
        let new = check_settings(&merged)?;
        let old = serde_json::from_value::<RoomSettings>(r.settings.clone()).ok();
        // E2EE にする部屋に、中身を見るメディアパイプライン（合成・文字起こし）があれば 422
        if new.e2ee && !old.as_ref().is_some_and(|o| o.e2ee) {
            let core = ctx.control();
            if let Some(m) = core.state.media_pipelines.values().find(|m| m.room == id && !m.managed && (m.composite.is_some() || m.captions.is_some())) {
                return Err(e2ee_refused(&format!("合成・文字起こしのあるメディアパイプライン（{}）", m.id)));
            }
        }
        // 部屋の核の規則（参加者がいる間の e2ee・コーデックの変更）は置いたノードで確かめる
        if let Some(old) = &old
            && (old.e2ee != new.e2ee || old.video_codec != new.video_codec || old.audio_codec != new.audio_codec)
            && let rid = id.to_string()
            && let Ok(v) = live(ctx, id, move |svc, _| svc.api_get_room(&rid))
            && v.get("participants").and_then(|p| p.as_u64()).unwrap_or(0) > 0
        {
            return Err(Problem::new(ProblemCode::Conflict, "参加者がいる間は e2ee・コーデックを変えられません"));
        }
        r.settings = serde_json::to_value(&new).unwrap_or(merged);
    }
    if let Some(m) = b.metadata {
        check_metadata(&m)?;
        r.metadata = m;
    }
    if let Some(p) = b.placement {
        r.placement = super::placement::check(ctx, p, None)?;
    }
    r.updated_at_ms = ctx.now_ms;
    // 部屋の設定 recording から作ったメディアパイプライン（設定を変えると作り直す・消える）
    let mid = senca_control::state::room_recording_pipeline_id(id);
    let managed_before = ctx.control().state.media_pipelines.get(&mid).filter(|m| m.managed).cloned();
    ctx.app.propose(Command::PutRoom { room: r, expect_generation: Some(expect) })?;
    if let Some(m) = managed_before {
        let now = ctx.control().state.media_pipelines.get(&mid).cloned();
        // 消えた・合成をやめた: 合成のパイプラインを片付ける（書き出した stream・録画は残す）
        if now.as_ref().is_none_or(|n| n.composite.is_none() && m.composite.is_some()) {
            crate::mediapipes::remove_children(ctx.app, &m);
        }
    }
    ctx.app.audit(ctx, "room.update", id);
    let r = tenant_room(ctx, id)?;
    Ok(ApiResponse::json(200, &render(ctx, &r)).with_header("ETag", etag(r.generation)))
}

pub fn delete(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    tenant_room(ctx, id)?;
    // 部屋のメディアパイプラインの子（合成・送り出し）も片付ける（書き出した stream・録画は残す）
    super::media_pipelines::remove_for_room(ctx, id);
    ctx.app.propose(Command::DeleteRoom { tenant: ctx.tenant().into(), id: id.into() })?;
    ctx.app.audit(ctx, "room.delete", id);
    Ok(ApiResponse::empty(204))
}

// ---- 参加トークン ----

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenBody {
    role: Role,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    bundle_id: Option<String>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    permissions: Option<Permissions>,
    #[serde(default)]
    ttl_secs: Option<u32>,
}

/// `POST /rooms/{id}/tokens`。無い部屋なら自動で作る（`auto: true`、空いてしばらくすると消える）。
pub fn issue_token(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let b: TokenBody = ctx.body()?;
    if !valid_room_id(id) {
        return Err(Problem::not_found());
    }
    let ttl = b.ttl_secs.unwrap_or(600);
    if !(10..=86_400).contains(&ttl) {
        return Err(bad("ttl_secs は 10〜86400"));
    }
    for (name, v) in [("client_id", &b.client_id), ("bundle_id", &b.bundle_id)] {
        if v.as_ref().is_some_and(|s| s.is_empty() || s.len() > 255 || s.chars().any(|c| c.is_control())) {
            return Err(bad(format!("{name} は 1〜255 文字")));
        }
    }
    if let Some(m) = &b.metadata {
        check_metadata(m)?;
    }
    let existing = ctx.control().state.rooms.get(id).cloned();
    match existing {
        Some(r) if r.tenant != ctx.tenant() => return Err(Problem::not_found()),
        Some(_) => {}
        None => {
            let room = Room {
                id: id.into(),
                tenant: ctx.tenant().into(),
                generation: 1,
                settings: serde_json::to_value(RoomSettings::default()).unwrap_or_default(),
                metadata: Value::Null,
                placement: None,
                auto: true,
                created_at_ms: ctx.now_ms,
                updated_at_ms: ctx.now_ms,
            };
            match ctx.app.propose(Command::PutRoom { room, expect_generation: None }) {
                Ok(()) => ctx.app.audit(ctx, "room.auto_create", id),
                // 同時に作られた（同じテナントなら続ける）
                Err(e) if e.status == 409 && ctx.control().state.rooms.get(id).is_some_and(|r| r.tenant == ctx.tenant()) => {}
                Err(e) => return Err(e),
            }
        }
    }
    let signer = crate::rooms::signer(ctx.app).ok_or_else(|| Problem::new(ProblemCode::CapacityUnavailable, "鍵がまだありません").with_retryable(true))?;
    let iat = ctx.now_ms / 1000;
    let mut jti = [0u8; 12];
    getrandom::fill(&mut jti).map_err(|_| Problem::internal())?;
    let claims = JoinClaims {
        iss: ISSUER.into(),
        aud: senca_rooms::token::Audience::One(AUDIENCE.into()),
        exp: iat + ttl as i64,
        nbf: None,
        iat: Some(iat),
        jti: Some(jti.iter().map(|x| format!("{x:02x}")).collect()),
        sub: Some(ctx.tenant().to_string()),
        room: id.into(),
        role: b.role,
        client_id: b.client_id,
        bundle_id: b.bundle_id,
        metadata: b.metadata,
        permissions: b.permissions,
    };
    let token = signer.sign(&claims).map_err(|e| Problem::new(ProblemCode::InternalError, e.to_string()))?;
    let r = tenant_room(ctx, id)?;
    let room = render(ctx, &r);
    Ok(ApiResponse::json(
        201,
        &json!({ "token": token, "room_id": id, "expires_at": crate::util::rfc3339((iat + ttl as i64) * 1000), "signaling_url": room["signaling_url"] }),
    ))
}

// ---- 部屋の核（置いたノード）----

/// 部屋を置いたノードがこのノードなら、核で `f` を動かす（別のノードなら Err。中継はしない）。
fn live<T, F>(ctx: &Ctx, room: &str, f: F) -> Result<Value, Problem>
where
    T: Serialize + 'static,
    F: FnOnce(&mut RoomService, Instant) -> Result<T, ApiError> + Send + 'static,
{
    match locate(ctx.app, room) {
        Location::Here => match live_or_forward(ctx, room, f)? {
            Ok(v) => Ok(v),
            Err(_) => Err(Problem::internal()),
        },
        _ => Err(Problem::new(ProblemCode::CapacityUnavailable, "部屋は別のノードにあります")),
    }
}

/// 手元で動かせたら値、別のノードへ中継したらその応答。
fn live_or_forward<T, F>(ctx: &Ctx, room: &str, f: F) -> Result<Result<Value, ApiResponse>, Problem>
where
    T: Serialize + 'static,
    F: FnOnce(&mut RoomService, Instant) -> Result<T, ApiError> + Send + 'static,
{
    tenant_room(ctx, room)?;
    match locate(ctx.app, room) {
        Location::Here => {
            let rtc = ctx.app.rtc.get().ok_or_else(|| Problem::new(ProblemCode::CapacityUnavailable, "WebRTC（--webrtc-udp）が動いていません"))?;
            let (tx, rx) = std::sync::mpsc::channel();
            rtc.room_cmd(RoomCmd::Call(Box::new(move |svc, now| {
                let _ = tx.send(f(svc, now).map(|v| serde_json::to_value(v).unwrap_or(Value::Null)));
            })));
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(r) => r.map(Ok).map_err(from_api_error),
                Err(_) => Err(Problem::new(ProblemCode::CapacityUnavailable, "部屋の係が応えません").with_retryable(true)),
            }
        }
        Location::Elsewhere { api, .. } => {
            let home = home_node(ctx, room).unwrap_or_default();
            forward_to(ctx, &api, &home).map(Err)
        }
        Location::Pending => Err(Problem::new(ProblemCode::CapacityUnavailable, "部屋を置くノードが決まっていません").with_retryable(true)),
        Location::Unknown => Err(Problem::not_found()),
    }
}

/// 同じ要求を別のノード（部屋を置いたノード・参加者のノード）へ送る（認証の見出しをそのまま。ノード間は同じ
/// 制御状態なので通る）。`X-Senca-Forwarded` に通ったノードを並べ、同じノードへ戻る・3 つ以上を通る中継はしない
/// （置き場所の見え方がずれている間の往復を止める）。
fn forward_to(ctx: &Ctx, api_base: &str, target: &str) -> Result<ApiResponse, Problem> {
    let chain: Vec<String> = ctx
        .req
        .header("x-senca-forwarded")
        .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    if chain.len() >= 2 || chain.iter().any(|n| n == target || n == &ctx.app.node_id) {
        return Err(Problem::new(ProblemCode::CapacityUnavailable, "部屋の置き場所が移っています").with_retryable(true));
    }
    let mut via = chain;
    via.push(ctx.app.node_id.clone());
    forward(ctx, api_base, &via.join(","))
}

fn forward(ctx: &Ctx, api_base: &str, via: &str) -> Result<ApiResponse, Problem> {
    let rt = ctx.app.rt.get().ok_or_else(Problem::internal)?.clone();
    let u = crate::httpc::parse_url(api_base).ok_or_else(Problem::internal)?;
    // 宛先はクラスタのノード（制御状態に載っている）なので、その宛先の IP だけを許す
    let ips: Vec<std::net::IpAddr> = rt
        .block_on(tokio::net::lookup_host((u.host.as_str(), u.port)))
        .map(|it| it.map(|a| a.ip()).collect())
        .unwrap_or_default();
    let allow: Vec<String> = ips.iter().map(|ip| if ip.is_ipv4() { format!("{ip}/32") } else { format!("{ip}/128") }).collect();
    if allow.is_empty() {
        return Err(Problem::new(ProblemCode::CapacityUnavailable, "部屋を置いたノードの宛先が引けません").with_retryable(true));
    }
    let url = format!(
        "{}/api/v1{}{}",
        api_base.trim_end_matches('/'),
        ctx.req.path,
        if ctx.req.query.is_empty() { String::new() } else { format!("?{}", ctx.req.query) }
    );
    let pass = ["authorization", "content-type", "if-match", "idempotency-key", "x-request-id"];
    let headers: Vec<(&str, String)> = ctx
        .req
        .headers
        .iter()
        .filter(|(k, _)| pass.contains(&k.to_ascii_lowercase().as_str()))
        .map(|(k, v)| (k.as_str(), v.clone()))
        .chain(std::iter::once(("X-Senca-Forwarded", via.to_string())))
        .collect();
    let method = senca_http::Method::parse(ctx.req.method.as_bytes()).map_err(|_| Problem::internal())?;
    let body = (!ctx.req.body.is_empty()).then(|| ctx.req.body.clone());
    let r = rt
        .block_on(crate::httpc::request(&allow, method, &url, &headers, body, 4 << 20, Duration::from_secs(10)))
        .map_err(|e| Problem::new(ProblemCode::CapacityUnavailable, format!("部屋を置いたノードに届きません: {e}")).with_retryable(true))?;
    let mut out = ApiResponse::bytes(r.status, r.header("content-type").unwrap_or("application/json"), r.body.clone());
    if let Some(e) = r.header("etag") {
        out = out.with_header("ETag", e.to_string());
    }
    Ok(out)
}

/// 部屋の核の時刻（UNIX ミリ秒）を API の形（RFC 3339）にそろえる。
fn rfc3339_times(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, x) in m.iter_mut() {
                if matches!(k.as_str(), "joined_at" | "at" | "created_at" | "updated_at")
                    && let Some(ms) = x.as_i64()
                {
                    *x = Value::String(crate::util::rfc3339(ms));
                } else {
                    rfc3339_times(x);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(rfc3339_times),
        _ => {}
    }
}

fn reply(r: Result<Value, ApiResponse>, status: u16) -> ApiResult {
    Ok(match r {
        Ok(mut v) => {
            rfc3339_times(&mut v);
            ApiResponse::json(status, &v)
        }
        Err(fwd) => fwd,
    })
}

pub fn participants(ctx: &Ctx, room: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let (limit, cursor) = ctx.page()?;
    let rid = room.to_string();
    reply(live_or_forward(ctx, room, move |svc, _| svc.api_list_participants(&rid, limit, cursor.as_deref()))?, 200)
}

pub fn participant(ctx: &Ctx, room: &str, cid: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let (rid, c) = (room.to_string(), cid.to_string());
    reply(live_or_forward(ctx, room, move |svc, _| svc.api_get_participant(&rid, &c))?, 200)
}

pub fn update_participant(ctx: &Ctx, room: &str, cid: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let b: UpdateParticipant = ctx.body()?;
    check_metadata(&b.metadata)?;
    let (rid, c) = (room.to_string(), cid.to_string());
    let r = on_participant_node(ctx, room, cid, move |svc, now| svc.api_update_participant(now, &rid, &c, b))?;
    ctx.app.audit(ctx, "room.participant.update", &format!("{room}/{cid}"));
    reply(r, 200)
}

pub fn disconnect_participant(ctx: &Ctx, room: &str, cid: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let (rid, c) = (room.to_string(), cid.to_string());
    let r = on_participant_node(ctx, room, cid, move |svc, now| svc.api_disconnect_participant(now, &rid, &c))?;
    ctx.app.audit(ctx, "room.participant.disconnect", &format!("{room}/{cid}"));
    Ok(match r {
        Ok(_) => ApiResponse::empty(204),
        Err(fwd) => fwd,
    })
}

/// 参加者の手元（その参加者のつないだノードの部屋の核）で `f` を動かす。カスケード: home に別のノードの参加者
/// として載っていれば、そのノード（edge）へ同じ要求を送る。edge に送られてきた要求は、手元の参加者なら手元で。
fn on_participant_node<T, F>(ctx: &Ctx, room: &str, cid: &str, f: F) -> Result<Result<Value, ApiResponse>, Problem>
where
    T: Serialize + 'static,
    F: FnOnce(&mut RoomService, Instant) -> Result<T, ApiError> + Send + 'static,
{
    enum Found<T> {
        Local(Result<T, ApiError>),
        Remote(String),
        Absent,
    }
    tenant_room(ctx, room)?;
    let forwarded = ctx.req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-senca-forwarded"));
    let loc = locate(ctx.app, room);
    // 手元の部屋の核（home でも edge でも）に聞く: 手元の参加者なら実行、別のノードの参加者ならそのノードへ
    if let Some(rtc) = ctx.app.rtc.get() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (rid, c) = (room.to_string(), cid.to_string());
        rtc.room_cmd(RoomCmd::Call(Box::new(move |svc, now| {
            let local = svc.local_participants(&rid).is_some_and(|v| v.iter().any(|p| p.connection_id == c));
            let found = if local {
                Found::Local(f(svc, now).map(|v| serde_json::to_value(v).unwrap_or(Value::Null)))
            } else {
                match svc.remote_participants(&rid).and_then(|v| v.into_iter().find(|p| p.connection_id == c)).and_then(|p| p.node) {
                    Some(n) => Found::Remote(n),
                    None => Found::Absent,
                }
            };
            let _ = tx.send(found);
        })));
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Found::Local(r)) => return r.map(Ok).map_err(from_api_error),
            Ok(Found::Remote(node)) => {
                let api = ctx.control().state.nodes.get(&node).and_then(|n| n.media_addr.clone());
                let api = api.ok_or_else(|| Problem::new(ProblemCode::CapacityUnavailable, "参加者のノードの宛先が分かりません").with_retryable(true))?;
                return forward_to(ctx, &api, &node).map(Err);
            }
            Ok(Found::Absent) => {}
            Err(_) => return Err(Problem::new(ProblemCode::CapacityUnavailable, "部屋の係が応えません").with_retryable(true)),
        }
    }
    match loc {
        Location::Here => Err(Problem::not_found()),
        Location::Elsewhere { api, .. } if !forwarded => {
            let home = home_node(ctx, room).unwrap_or_default();
            forward_to(ctx, &api, &home).map(Err)
        }
        Location::Elsewhere { .. } => Err(Problem::not_found()),
        Location::Pending => Err(Problem::new(ProblemCode::CapacityUnavailable, "部屋を置くノードが決まっていません").with_retryable(true)),
        Location::Unknown => Err(Problem::not_found()),
    }
}

/// 部屋を置いたノードの ID。
fn home_node(ctx: &Ctx, room: &str) -> Option<String> {
    let wid = format!("{}{room}", senca_control::workloads::ROOM);
    ctx.control().state.grid.assignments.get(&wid).map(|a| a.node.clone())
}

pub fn post_message(ctx: &Ctx, room: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let b: PostMessage = ctx.body()?;
    if serde_json::to_string(&b.data).map(|s| s.len()).unwrap_or(usize::MAX) > 64 * 1024 {
        return Err(bad("data は 64 KiB まで"));
    }
    let rid = room.to_string();
    reply(live_or_forward(ctx, room, move |svc, now| svc.api_post_message(now, &rid, b))?, 200)
}

pub fn stats(ctx: &Ctx, room: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let rid = room.to_string();
    reply(live_or_forward(ctx, room, move |svc, now| svc.api_stats(now, &rid))?, 200)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_merge_patch() {
        let mut t = json!({ "max_participants": 10, "e2ee": false, "spotlight": { "number": 2, "unfocused": "off" }, "data_labels": ["#a"] });
        merge_patch(&mut t, &json!({ "max_participants": 20, "spotlight": { "number": 3 }, "data_labels": null }));
        assert_eq!(t, json!({ "max_participants": 20, "e2ee": false, "spotlight": { "number": 3, "unfocused": "off" } }));
        merge_patch(&mut t, &json!({ "spotlight": null }));
        assert!(t.get("spotlight").is_none());
        // 消した項目は既定に戻る
        let s: RoomSettings = serde_json::from_value(t).unwrap();
        assert_eq!(s.data_labels, RoomSettings::default().data_labels);
    }

    #[test]
    fn e2ee_composite_recording_is_422() {
        let e = check_settings(&json!({ "e2ee": true, "recording": "composite" })).unwrap_err();
        assert_eq!((e.status, e.details["reason"].as_str()), (422, Some("e2ee")));
        assert!(check_settings(&json!({ "e2ee": true, "recording": "per_participant" })).is_ok());
    }
}
