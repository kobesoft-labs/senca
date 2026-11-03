//! 録画の API（仕様 10.2、8.4）。録画の実体（セグメントと索引）は `recorder.rs`。

use super::{ApiResponse, ApiResult, Ctx, etag, paginate};
use crate::util::rfc3339;
use senca_control::auth::Scope;
use senca_control::model::*;
use senca_control::state::Command;
use senca_control::{Problem, ProblemCode};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn render(ctx: &Ctx, r: &Recording) -> Value {
    let info = ctx.app.recording_info(&r.id);
    json!({
        "id": r.id,
        "stream": r.stream,
        "name": r.name,
        "generation": r.generation,
        "state": r.state,
        "started_at": rfc3339(r.started_at_ms),
        "ended_at": r.ended_at_ms.map(rfc3339),
        "retention_until": r.retention_until_ms.map(rfc3339),
        "legal_hold": r.legal_hold,
        "schedule": r.schedule,
        "duration_ms": info.as_ref().map(|i| i.duration_ms),
        "gaps": info.as_ref().map(|i| i.gaps.clone()),
        "segments": info.as_ref().map(|i| i.segments),
        "bytes": info.as_ref().map(|i| i.bytes),
        "thumbnail": info.as_ref().is_some_and(|i| i.has_thumbnail),
    })
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct StartBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    retention_days: Option<u32>,
}

pub fn start(ctx: &Ctx, stream_id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let b: StartBody = ctx.body_or_default()?;
    if let Some(d) = b.retention_days
        && !(1..=36_500).contains(&d)
    {
        return Err(Problem::new(ProblemCode::InvalidConfig, "retention_days は 1〜36500"));
    }
    let s = ctx.control().state.tenant_stream(ctx.tenant(), stream_id)?.clone();
    // クラスタ: 配信を受けているノードでしか録れない（別のノードなら案内する）
    if let Some(p) = ctx.app.owned_elsewhere(&s.id) {
        return Err(p);
    }
    let rec = ctx.app.start_recording(&s, b.name, b.retention_days, None, None)?;
    ctx.app.audit(ctx, "recording.start", &rec.id);
    Ok(ApiResponse::json(202, &render(ctx, &rec))
        .with_header("Location", format!("/api/v1/recordings/{}", rec.id))
        .with_header("ETag", etag(rec.generation)))
}

pub fn stop(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let r = ctx.control().state.tenant_recording(ctx.tenant(), id)?.clone();
    if r.state != RecordingState::Recording {
        return Err(Problem::new(ProblemCode::Conflict, "録画中ではありません"));
    }
    ctx.app.stop_recording(id, None);
    ctx.app.audit(ctx, "recording.stop", id);
    let r = ctx.control().state.tenant_recording(ctx.tenant(), id)?.clone();
    Ok(ApiResponse::json(202, &render(ctx, &r)))
}

pub fn list(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::RecordingRead)?;
    let (limit, cursor) = ctx.page()?;
    let q = ctx.req.query_pairs();
    let stream = q.iter().find(|(k, _)| k == "stream").map(|(_, v)| v.clone());
    let state = match q.iter().find(|(k, _)| k == "state").map(|(_, v)| v.as_str()) {
        None => None,
        Some("recording") => Some(RecordingState::Recording),
        Some("finalized") => Some(RecordingState::Finalized),
        Some("interrupted") => Some(RecordingState::Interrupted),
        Some(_) => return Err(Problem::invalid("state は recording・finalized・interrupted")),
    };
    let items: Vec<(String, Recording)> = ctx
        .control()
        .state
        .recordings
        .values()
        .filter(|r| {
            r.tenant == ctx.tenant()
                && r.state != RecordingState::Deleting
                && stream.as_ref().is_none_or(|s| *s == r.stream)
                && state.is_none_or(|x| x == r.state)
        })
        .map(|r| (r.id.clone(), r.clone()))
        .collect();
    let (page, next) = paginate(items, limit, cursor.as_deref());
    let items: Vec<Value> = page.iter().map(|r| render(ctx, r)).collect();
    Ok(ApiResponse::json(200, &json!({ "items": items, "next_cursor": next })))
}

fn visible(ctx: &Ctx, id: &str) -> Result<Recording, Problem> {
    let r = ctx.control().state.tenant_recording(ctx.tenant(), id)?.clone();
    if r.state == RecordingState::Deleting {
        return Err(Problem::not_found());
    }
    Ok(r)
}

pub fn get(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::RecordingRead)?;
    let r = visible(ctx, id)?;
    Ok(ApiResponse::json(200, &render(ctx, &r)).with_header("ETag", etag(r.generation)))
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Patch {
    #[serde(default, deserialize_with = "double_option")]
    retention_until: Option<Option<String>>,
    legal_hold: Option<bool>,
    name: Option<String>,
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
    let p: Patch = ctx.body()?;
    let mut r = ctx.control().state.tenant_recording(ctx.tenant(), id)?.clone();
    if r.state == RecordingState::Deleting {
        return Err(Problem::not_found());
    }
    if r.generation != expect {
        return Err(Problem::new(ProblemCode::PreconditionFailed, "資源が更新されています（If-Match が古い）"));
    }
    if let Some(v) = p.retention_until {
        r.retention_until_ms = match v {
            None => None,
            Some(s) => Some(senca_core::rfc3339::parse_millis(&s).ok_or_else(|| Problem::new(ProblemCode::InvalidConfig, "retention_until の形が違います"))?),
        };
    }
    if let Some(v) = p.legal_hold {
        // 法的保全の解除は recording:delete を要求する
        if !v && r.legal_hold && !ctx.principal.has(Scope::RecordingDelete) {
            return Err(Problem::new(ProblemCode::Forbidden, "法的保全の解除には recording:delete が必要です"));
        }
        r.legal_hold = v;
    }
    if let Some(v) = p.name {
        if v.len() > 128 {
            return Err(Problem::new(ProblemCode::InvalidConfig, "name は 128 文字まで"));
        }
        r.name = Some(v);
    }
    r.generation += 1;
    ctx.app.propose(Command::PutRecording { recording: r.clone() })?;
    ctx.app.audit(ctx, "recording.update", id);
    Ok(ApiResponse::json(200, &render(ctx, &r)).with_header("ETag", etag(r.generation)))
}

pub fn delete(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::RecordingDelete)?;
    let expect = ctx.if_match(false)?;
    let r = visible(ctx, id)?;
    if let Some(g) = expect
        && g != r.generation
    {
        return Err(Problem::new(ProblemCode::PreconditionFailed, "資源が更新されています（If-Match が古い）"));
    }
    if r.legal_hold {
        return Err(Problem::new(ProblemCode::Conflict, "法的保全中の録画は消せません"));
    }
    ctx.app.delete_recording(&r)?;
    ctx.app.audit(ctx, "recording.delete", id);
    Ok(ApiResponse::json(202, &json!({ "id": id, "state": "deleting" })))
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PlaybackBody {
    #[serde(default)]
    ttl_secs: Option<u32>,
}

pub fn playback_url(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::RecordingRead)?;
    let b: PlaybackBody = ctx.body_or_default()?;
    let ttl = b.ttl_secs.unwrap_or(300);
    if ttl == 0 || ttl > ctx.app.cfg.max_playback_ttl_secs {
        return Err(Problem::invalid(format!("ttl_secs は 1〜{}", ctx.app.cfg.max_playback_ttl_secs)));
    }
    let r = visible(ctx, id)?;
    let exp = ctx.now_ms / 1000 + ttl as i64;
    let urls = ctx.app.signed_urls("vod", &r.id, exp)?;
    Ok(ApiResponse::json(200, &json!({ "expires_at": rfc3339(exp * 1000), "playback": urls })))
}

pub fn thumbnail(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::RecordingRead)?;
    let r = visible(ctx, id)?;
    let b = ctx.app.recording_thumbnail(&r.id).ok_or_else(Problem::not_found)?;
    Ok(ApiResponse::bytes(200, "image/jpeg", b))
}

/// `?start=&end=`（RFC 3339）で範囲を切り出せる。始まりは直前のキーフレームまで広がる。
pub fn export_mp4(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::RecordingRead)?;
    let q = ctx.req.query_pairs();
    if let Some((k, _)) = q.iter().find(|(k, _)| k != "start" && k != "end") {
        return Err(Problem::invalid(format!("知らないクエリ: {k}")));
    }
    let when = |k: &str| -> Result<Option<i64>, Problem> {
        match q.iter().find(|(a, _)| a == k) {
            None => Ok(None),
            Some((_, v)) => senca_core::rfc3339::parse_millis(v)
                .map(Some)
                .ok_or_else(|| Problem::new(ProblemCode::InvalidConfig, format!("{k} は RFC 3339 の時刻で書いてください"))),
        }
    };
    let (start, end) = (when("start")?, when("end")?);
    if let (Some(s), Some(e)) = (start, end)
        && e <= s
    {
        return Err(Problem::new(ProblemCode::InvalidConfig, "end は start より後にしてください"));
    }
    let r = visible(ctx, id)?;
    if r.state == RecordingState::Recording {
        return Err(Problem::new(ProblemCode::Conflict, "録画中は書き出せません"));
    }
    let b = ctx.app.export_mp4(&r.id, start, end)?;
    Ok(ApiResponse::bytes(200, "video/mp4", b).with_header("Content-Disposition", format!("attachment; filename=\"{}.mp4\"", r.id)))
}
