//! stream の API（仕様 10.2）。

use super::{ApiResponse, ApiResult, Ctx, etag, new_id, paginate};
use crate::util::rfc3339;
use senca_control::auth::Scope;
use senca_control::model::*;
use senca_control::secret;
use senca_control::state::Command;
use senca_control::{Problem, ProblemCode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const KEY_PREFIX: &str = "sk_";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamBody {
    pub name: String,
    #[serde(default)]
    pub latency_profile: LatencyProfile,
    #[serde(default)]
    pub codec_profile: CodecProfile,
    #[serde(default)]
    pub recording: RecordingPolicy,
    #[serde(default)]
    pub snapshots: Option<SnapshotPolicy>,
    #[serde(default)]
    pub hls: HlsSettings,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub desired_state: Option<String>,
    #[serde(default)]
    pub captions: Option<CaptionSettings>,
}

/// PATCH の本文（JSON Merge Patch の簡易版: 書いたフィールドだけ置き換える）。
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StreamPatch {
    pub name: Option<String>,
    pub latency_profile: Option<LatencyProfile>,
    pub codec_profile: Option<CodecProfile>,
    pub recording: Option<RecordingPolicy>,
    #[serde(default, deserialize_with = "double_option")]
    pub snapshots: Option<Option<SnapshotPolicy>>,
    pub hls: Option<HlsSettings>,
    pub limits: Option<Limits>,
    pub metadata: Option<BTreeMap<String, String>>,
    pub desired_state: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub captions: Option<Option<CaptionSettings>>,
}

fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::deserialize(d)?))
}

pub fn validate_name(name: &str) -> Result<(), Problem> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        && !name.starts_with('.');
    if ok { Ok(()) } else { Err(Problem::new(ProblemCode::InvalidConfig, "name は 1〜64 文字の英数字と -_. です")) }
}

fn validate(s: &Stream) -> Result<(), Problem> {
    validate_name(&s.name)?;
    let bad = |t: &str| Err(Problem::new(ProblemCode::InvalidConfig, t.to_string()));
    if !(500..=10_000).contains(&s.hls.segment_ms) {
        return bad("hls.segment_ms は 500〜10000");
    }
    if !(3..=60).contains(&s.hls.window) {
        return bad("hls.window は 3〜60");
    }
    if s.hls.container != "ts" && s.hls.container != "fmp4" {
        return bad("hls.container は ts か fmp4");
    }
    if s.codec_profile == CodecProfile::RoyaltyFree && s.hls.container == "ts" {
        // TS でも Opus は運べるが、AV1/VP9 は運べないので fMP4 を求める
        return bad("royalty_free では hls.container を fmp4 にしてください");
    }
    if let Some(p) = &s.snapshots
        && (!(1..=86_400).contains(&p.interval_secs) || !(1..=10_000).contains(&p.keep))
    {
        return bad("snapshots は interval_secs 1〜86400、keep 1〜10000");
    }
    if let Some(d) = s.recording.retention_days
        && !(1..=36_500).contains(&d)
    {
        return bad("recording.retention_days は 1〜36500");
    }
    if s.metadata.len() > 32 || s.metadata.iter().any(|(k, v)| k.is_empty() || k.len() > 64 || v.len() > 1024) {
        return bad("metadata は 32 個まで、キー 64 文字・値 1024 文字まで");
    }
    if s.desired_state != "running" && s.desired_state != "stopped" {
        return bad("desired_state は running か stopped");
    }
    if let Some(c) = &s.captions {
        validate_captions(c)?;
    }
    Ok(())
}

/// 字幕の設定の検査（stream とメディアパイプラインで共通）。
pub(crate) fn validate_captions(c: &CaptionSettings) -> Result<(), Problem> {
    let bad = |t: &str| Err(Problem::new(ProblemCode::InvalidConfig, t.to_string()));
    if !matches!(c.asr_provider.as_str(), "openai" | "whisper_cpp") {
        return bad("captions.asr_provider は openai か whisper_cpp");
    }
    if senca_caption::http::Endpoint::parse(&c.asr_endpoint).is_err() {
        return bad("captions.asr_endpoint の形が違います");
    }
    if let Some(t) = &c.translate_provider {
        if !matches!(t.as_str(), "anthropic" | "deepl" | "openai") {
            return bad("captions.translate_provider は anthropic・deepl・openai");
        }
        if c.translate_endpoint.as_deref().is_none_or(|e| senca_caption::http::Endpoint::parse(e).is_err()) {
            return bad("captions.translate_endpoint が要ります");
        }
    }
    if c.targets.len() > 8 || c.targets.iter().chain(std::iter::once(&c.language)).any(|l| l.is_empty() || l.len() > 8 || !l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')) {
        return bad("captions の言語は 8 個まで、ISO 639-1 の形");
    }
    Ok(())
}

/// 字幕の資格情報の参照が、このテナントの秘密値を指しているか。
pub(crate) fn check_caption_refs(ctx: &Ctx, c: &Option<CaptionSettings>) -> Result<(), Problem> {
    let Some(c) = c else { return Ok(()) };
    for r in [&c.asr_credential_ref, &c.translate_credential_ref].into_iter().flatten() {
        let name = r.strip_prefix("secret:").ok_or_else(|| Problem::new(ProblemCode::InvalidConfig, "credential_ref は secret:{name} の形です"))?;
        let core = ctx.control();
        if !core.state.secrets.values().any(|s| s.tenant == ctx.tenant() && s.name == name) {
            return Err(Problem::new(ProblemCode::InvalidConfig, format!("秘密値 {name} がありません")));
        }
    }
    Ok(())
}

/// stream の表現。`key` は作成・作り直しの応答でだけ渡す。
pub fn render(ctx: &Ctx, s: &Stream, key: Option<&str>) -> Value {
    let live = ctx.app.live_info(&s.id);
    let observed = if s.desired_state == "stopped" {
        "stopped"
    } else if live.is_some() {
        "live"
    } else {
        "waiting_input"
    };
    let urls = &ctx.app.cfg.public;
    let mut ingest = json!({
        "rtmp_url": format!("{}/live", urls.rtmp),
        "stream_key": key,
    });
    if let Some(srt) = &urls.srt {
        ingest["srt_url"] = json!(srt);
        ingest["srt_streamid_format"] = json!("publish:{stream_key}");
    }
    if let Some(http) = &urls.http {
        ingest["whip_url"] = json!(format!("{http}/whip/{}", s.id));
    }
    json!({
        "id": s.id,
        "name": s.name,
        "generation": s.generation,
        "created_at": rfc3339(s.created_at_ms),
        "updated_at": rfc3339(s.updated_at_ms),
        "desired_state": s.desired_state,
        "observed_state": observed,
        "latency_profile": s.latency_profile,
        "codec_profile": s.codec_profile,
        "recording": s.recording,
        "snapshots": s.snapshots,
        "hls": s.hls,
        "limits": s.limits,
        "metadata": s.metadata,
        "ingest": ingest,
        "live": live,
        "captions": s.captions,
    })
}

pub fn create(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let b: StreamBody = ctx.body()?;
    check_caption_refs(ctx, &b.captions)?;
    let key = secret::generate(KEY_PREFIX).map_err(|_| Problem::internal())?;
    let s = Stream {
        id: new_id()?,
        tenant: ctx.tenant().to_string(),
        name: b.name,
        generation: 1,
        created_at_ms: ctx.now_ms,
        updated_at_ms: ctx.now_ms,
        key_hash: secret::hash(key.expose()),
        latency_profile: b.latency_profile,
        codec_profile: b.codec_profile,
        recording: b.recording,
        snapshots: b.snapshots,
        hls: b.hls,
        limits: b.limits,
        metadata: b.metadata,
        desired_state: b.desired_state.unwrap_or_else(|| "running".into()),
        captions: b.captions,
    };
    validate(&s)?;
    ctx.app.propose(Command::CreateStream { stream: s.clone() })?;
    ctx.app.audit(ctx, "stream.create", &s.id);
    Ok(ApiResponse::json(201, &render(ctx, &s, Some(key.expose())))
        .with_header("Location", format!("/api/v1/streams/{}", s.id))
        .with_header("ETag", etag(s.generation)))
}

pub fn list(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let (limit, cursor) = ctx.page()?;
    let items: Vec<(String, Stream)> = {
        let core = ctx.control();
        core.state.streams.values().filter(|s| s.tenant == ctx.tenant()).map(|s| (s.id.clone(), s.clone())).collect()
    };
    let (page, next) = paginate(items, limit, cursor.as_deref());
    let items: Vec<Value> = page.iter().map(|s| render(ctx, s, None)).collect();
    Ok(ApiResponse::json(200, &json!({ "items": items, "next_cursor": next })))
}

pub fn get(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let s = ctx.control().state.tenant_stream(ctx.tenant(), id)?.clone();
    Ok(ApiResponse::json(200, &render(ctx, &s, None)).with_header("ETag", etag(s.generation)))
}

pub fn patch(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let expect = ctx.if_match(true)?.unwrap_or(0);
    let p: StreamPatch = ctx.body()?;
    let mut s = ctx.control().state.tenant_stream(ctx.tenant(), id)?.clone();
    if s.generation != expect {
        return Err(Problem::new(ProblemCode::PreconditionFailed, "資源が更新されています（If-Match が古い）"));
    }
    if let Some(v) = p.name {
        s.name = v;
    }
    if let Some(v) = p.latency_profile {
        s.latency_profile = v;
    }
    if let Some(v) = p.codec_profile {
        s.codec_profile = v;
    }
    if let Some(v) = p.recording {
        s.recording = v;
    }
    if let Some(v) = p.snapshots {
        s.snapshots = v;
    }
    if let Some(v) = p.hls {
        s.hls = v;
    }
    if let Some(v) = p.limits {
        s.limits = v;
    }
    if let Some(v) = p.metadata {
        s.metadata = v;
    }
    if let Some(v) = p.desired_state {
        s.desired_state = v;
    }
    if let Some(v) = p.captions {
        check_caption_refs(ctx, &v)?;
        s.captions = v;
    }
    s.updated_at_ms = ctx.now_ms;
    validate(&s)?;
    ctx.app.propose(Command::UpdateStream { stream: s, expect_generation: expect })?;
    let s = ctx.control().state.tenant_stream(ctx.tenant(), id)?.clone();
    ctx.app.on_stream_changed(&s);
    ctx.app.audit(ctx, "stream.update", id);
    Ok(ApiResponse::json(200, &render(ctx, &s, None)).with_header("ETag", etag(s.generation)))
}

pub fn delete(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let expect = ctx.if_match(false)?;
    ctx.app.propose(Command::DeleteStream { tenant: ctx.tenant().to_string(), id: id.to_string(), expect_generation: expect })?;
    ctx.app.on_stream_deleted(id);
    ctx.app.audit(ctx, "stream.delete", id);
    Ok(ApiResponse::empty(204))
}

pub fn rotate_key(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let key = secret::generate(KEY_PREFIX).map_err(|_| Problem::internal())?;
    let mut s = ctx.control().state.tenant_stream(ctx.tenant(), id)?.clone();
    let expect = s.generation;
    s.key_hash = secret::hash(key.expose());
    s.updated_at_ms = ctx.now_ms;
    ctx.app.propose(Command::UpdateStream { stream: s, expect_generation: expect })?;
    let s = ctx.control().state.tenant_stream(ctx.tenant(), id)?.clone();
    ctx.app.audit(ctx, "stream.rotate_key", id);
    Ok(ApiResponse::json(200, &render(ctx, &s, Some(key.expose()))).with_header("ETag", etag(s.generation)))
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PlaybackBody {
    #[serde(default)]
    ttl_secs: Option<u32>,
}

pub fn playback_url(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::Subscribe)?;
    let b: PlaybackBody = ctx.body_or_default()?;
    let ttl = b.ttl_secs.unwrap_or(300);
    if ttl == 0 || ttl > ctx.app.cfg.max_playback_ttl_secs {
        return Err(Problem::invalid(format!("ttl_secs は 1〜{}", ctx.app.cfg.max_playback_ttl_secs)));
    }
    let s = ctx.control().state.tenant_stream(ctx.tenant(), id)?.clone();
    let exp = ctx.now_ms / 1000 + ttl as i64;
    let urls = ctx.app.signed_urls("live", &s.id, exp)?;
    Ok(ApiResponse::json(200, &json!({ "expires_at": rfc3339(exp * 1000), "playback": urls })))
}

/// 変換計画（仕様 7 章）。副作用なし。
pub fn plan(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let s = ctx.control().state.tenant_stream(ctx.tenant(), id)?.clone();
    Ok(ApiResponse::json(200, &ctx.app.plan(&s)))
}

pub fn snapshot(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let s = ctx.control().state.tenant_stream(ctx.tenant(), id)?.clone();
    let fmt = image_format(ctx)?;
    let img = ctx.app.snapshot_now(&s, fmt)?;
    Ok(ApiResponse::bytes(200, fmt.mime(), img))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Jpeg,
    Png,
}

impl ImageFormat {
    pub fn mime(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Png => "image/png",
        }
    }
    #[allow(dead_code)]
    pub fn ext(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Png => "png",
        }
    }
}

fn image_format(ctx: &Ctx) -> Result<ImageFormat, Problem> {
    let mut f = ImageFormat::Jpeg;
    for (k, v) in ctx.req.query_pairs() {
        match (k.as_str(), v.as_str()) {
            ("format", "jpeg" | "jpg") => f = ImageFormat::Jpeg,
            ("format", "png") => f = ImageFormat::Png,
            _ => return Err(Problem::invalid(format!("知らないクエリ: {k}={v}"))),
        }
    }
    Ok(f)
}

pub fn snapshots_list(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    ctx.control().state.tenant_stream(ctx.tenant(), id)?;
    let items = ctx.app.snapshot_store_list(id);
    Ok(ApiResponse::json(200, &json!({ "items": items })))
}

pub fn snapshot_get(ctx: &Ctx, id: &str, sid: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    ctx.control().state.tenant_stream(ctx.tenant(), id)?;
    let (mime, b) = ctx.app.snapshot_store_get(id, sid).ok_or_else(Problem::not_found)?;
    Ok(ApiResponse::bytes(200, mime, b))
}

pub fn snapshot_delete(ctx: &Ctx, id: &str, sid: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    ctx.control().state.tenant_stream(ctx.tenant(), id)?;
    if !ctx.app.snapshot_store_delete(id, sid) {
        return Err(Problem::not_found());
    }
    Ok(ApiResponse::empty(204))
}
