//! メディアパイプラインの API（ADR-0016、AWS Chime SDK Media Pipelines 相当）。
//!
//! `POST /media-pipelines` `{name, room, participants?, export_participants?, composite?, recording?, egress?, captions?, desired_state?}`
//! - 書き出し（stream pipeline）: 参加者ごとの stream（`artifacts.participant_streams`）
//! - 録画（capture）: `recording`（参加者ごと・合成）。録画は普通の録画の API で読む・書き出す（まとめ = concatenation）
//! - 送り出し（live connector）: `egress`（合成の stream の出力。`composite` が要る）
//! - 文字起こし（insights）: `captions`（参加者ごとの stream の字幕・翻訳）

use super::{ApiResponse, ApiResult, Ctx, etag, new_id, paginate};
use crate::mediapipes::{composite_pipeline_id, composite_stream_id, egress_output_id};
use senca_control::auth::Scope;
use senca_control::model::{CaptionSettings, CompositeSpec, MediaPipeline, MediaPipelineEgress, MediaPipelineRecording};
use senca_control::state::Command;
use senca_control::{Problem, ProblemCode};
use serde::Deserialize;
use serde_json::{Value, json};

fn bad(t: impl Into<String>) -> Problem {
    Problem::new(ProblemCode::InvalidConfig, t)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    name: String,
    room: String,
    #[serde(default)]
    participants: Vec<String>,
    #[serde(default)]
    export_participants: Option<bool>,
    #[serde(default)]
    composite: Option<CompositeSpec>,
    #[serde(default)]
    recording: Option<MediaPipelineRecording>,
    #[serde(default)]
    egress: Vec<MediaPipelineEgress>,
    #[serde(default)]
    captions: Option<CaptionSettings>,
    #[serde(default)]
    desired_state: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Patch {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    participants: Option<Vec<String>>,
    #[serde(default)]
    export_participants: Option<bool>,
    #[serde(default, deserialize_with = "double_option")]
    composite: Option<Option<CompositeSpec>>,
    #[serde(default, deserialize_with = "double_option")]
    recording: Option<Option<MediaPipelineRecording>>,
    #[serde(default)]
    egress: Option<Vec<MediaPipelineEgress>>,
    #[serde(default, deserialize_with = "double_option")]
    captions: Option<Option<CaptionSettings>>,
    #[serde(default)]
    desired_state: Option<String>,
}

fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::deserialize(d)?))
}

fn check(ctx: &Ctx, m: &MediaPipeline) -> Result<(), Problem> {
    crate::api::streams::validate_name(&m.name)?;
    // E2EE の部屋: SFU は中身を見られないので、合成・文字起こしはできない（書き出し・録画は暗号のまま）
    let e2ee = ctx.control().state.rooms.get(&m.room).is_some_and(|r| r.settings.get("e2ee").and_then(|v| v.as_bool()).unwrap_or(false));
    if e2ee && (m.composite.is_some() || m.captions.is_some()) {
        return Err(crate::api::rooms::e2ee_refused("合成・文字起こし"));
    }
    if m.name.len() > 40 {
        return Err(bad("name は 40 文字まで（子の stream の名前に使う）"));
    }
    if m.desired_state != "running" && m.desired_state != "stopped" {
        return Err(bad("desired_state は running か stopped"));
    }
    if m.participants.len() > 100 || m.participants.iter().any(|p| p.is_empty() || p.len() > 255) {
        return Err(bad("participants は 100 人まで（client_id）"));
    }
    if let Some(c) = &m.composite {
        if c.layout != "grid" && c.layout != "speaker" {
            return Err(bad("composite.layout は grid か speaker"));
        }
        if !(64..=3840).contains(&c.width) || !(64..=2160).contains(&c.height) || c.width % 2 == 1 || c.height % 2 == 1 {
            return Err(bad("composite の大きさは 64〜3840 × 64〜2160 の偶数"));
        }
        if !(1..=60).contains(&c.fps) {
            return Err(bad("composite.fps は 1〜60"));
        }
        if !matches!(c.video_codec.as_str(), "av1" | "vp9" | "vp8" | "h264" | "h265") {
            return Err(bad("composite.video_codec は av1・vp9・vp8・h264・h265"));
        }
        if !(100..=50_000).contains(&c.bitrate_kbps) {
            return Err(bad("composite.bitrate_kbps は 100〜50000"));
        }
    }
    if let Some(r) = &m.recording {
        if r.composite && m.composite.is_none() {
            return Err(bad("recording.composite には composite が要ります"));
        }
        if r.participants && !m.export_participants {
            return Err(bad("recording.participants には export_participants が要ります"));
        }
        if r.retention_days.is_some_and(|d| !(1..=36_500).contains(&d)) {
            return Err(bad("recording.retention_days は 1〜36500"));
        }
    }
    if !m.egress.is_empty() && m.composite.is_none() {
        return Err(bad("egress（送り出し）には composite が要ります（合成した 1 本を送る）"));
    }
    if m.egress.len() > 8 {
        return Err(bad("egress は 8 個まで"));
    }
    for e in &m.egress {
        if !crate::api::io::OUTPUT_PROTOCOLS.contains(&e.protocol.as_str()) {
            return Err(bad(format!("egress.protocol は {}", crate::api::io::OUTPUT_PROTOCOLS.join("・"))));
        }
        if e.protocol == "paas" {
            let p = e.provider.as_deref().ok_or_else(|| bad("paas には provider が要ります"))?;
            if !crate::paas::PROVIDERS.contains(&p) {
                return Err(bad(format!("provider は {}", crate::paas::PROVIDERS.join("・"))));
            }
            if e.credential_ref.is_none() {
                return Err(bad("paas には credential_ref（API トークン）が要ります"));
            }
        } else {
            crate::api::io::check_url(&e.protocol, e.url.as_deref().ok_or_else(|| bad("egress に url が要ります"))?)?;
        }
        crate::api::io::check_ref(ctx, &e.credential_ref)?;
        crate::api::io::check_options(&e.options)?;
    }
    if let Some(c) = &m.captions {
        let c: CaptionSettings = serde_json::from_value(c.clone()).map_err(|e| bad(format!("captions: {e}")))?;
        crate::api::streams::validate_captions(&c)?;
        crate::api::streams::check_caption_refs(ctx, &Some(c))?;
        if !m.export_participants {
            return Err(bad("captions には export_participants が要ります（参加者ごとの stream に付ける）"));
        }
    }
    Ok(())
}

/// 部屋の設定 `recording` から作ったものは、部屋の設定で変える（409）。
fn managed_refused(m: &MediaPipeline) -> Result<(), Problem> {
    if m.managed {
        return Err(Problem::new(ProblemCode::Conflict, "このメディアパイプラインは部屋の設定 recording から作ったものです（部屋の設定を変えてください）")
            .with_details(json!({ "room": m.room })));
    }
    Ok(())
}

fn tenant_mp(ctx: &Ctx, id: &str) -> Result<MediaPipeline, Problem> {
    ctx.control().state.media_pipelines.get(id).filter(|m| m.tenant == ctx.tenant()).cloned().ok_or_else(Problem::not_found)
}

fn render(ctx: &Ctx, m: &MediaPipeline) -> Value {
    let (parts, composite) = {
        let core = ctx.control();
        let parts: Vec<Value> = core
            .state
            .streams
            .values()
            .filter(|s| {
                s.tenant == m.tenant
                    && s.metadata.get("media_pipeline") == Some(&m.id)
                    && s.metadata.get("participant").is_some_and(|p| s.id == crate::mediapipes::participant_stream_id(&m.id, p))
            })
            .map(|s| json!({ "stream": s.id, "name": s.name, "participant": s.metadata.get("participant") }))
            .collect();
        let composite = m.composite.as_ref().map(|_| {
            json!({
                "stream": composite_stream_id(&m.id),
                "pipeline": composite_pipeline_id(&m.id),
                "outputs": (0..m.egress.len()).map(|i| egress_output_id(&m.id, i)).collect::<Vec<_>>(),
            })
        });
        (parts, composite)
    };
    // 送り出しは秘密を出さない（credential_ref の名前だけ）
    json!({
        "id": m.id, "name": m.name, "room": m.room, "generation": m.generation,
        "participants": m.participants, "export_participants": m.export_participants,
        "composite": m.composite, "recording": m.recording, "egress": m.egress, "captions": m.captions,
        "desired_state": m.desired_state,
        "managed": m.managed,
        "created_at": crate::util::rfc3339(m.created_at_ms), "updated_at": crate::util::rfc3339(m.updated_at_ms),
        "artifacts": { "participant_streams": parts, "composite": composite },
        "status": ctx.app.mediapipes.status(&m.id),
    })
}

fn remove_children(ctx: &Ctx, m: &MediaPipeline) {
    crate::mediapipes::remove_children(ctx.app, m);
}

/// 部屋を消すときに、その部屋のメディアパイプラインの子を消す（rooms の API から）。
pub fn remove_for_room(ctx: &Ctx, room: &str) {
    let mps: Vec<MediaPipeline> = ctx.control().state.media_pipelines.values().filter(|m| m.room == room && m.tenant == ctx.tenant()).cloned().collect();
    for m in mps {
        remove_children(ctx, &m);
    }
}

pub fn create(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let b: Body = ctx.body()?;
    ctx.control().state.rooms.get(&b.room).filter(|r| r.tenant == ctx.tenant()).ok_or_else(|| bad(format!("部屋 {} がありません", b.room)))?;
    let m = MediaPipeline {
        id: new_id()?,
        tenant: ctx.tenant().into(),
        generation: 1,
        name: b.name,
        room: b.room,
        participants: b.participants,
        export_participants: b.export_participants.unwrap_or(true),
        composite: b.composite,
        recording: b.recording,
        egress: b.egress,
        captions: b.captions.map(|c| serde_json::to_value(c).unwrap_or_default()),
        desired_state: b.desired_state.unwrap_or_else(|| "running".into()),
        created_at_ms: ctx.now_ms,
        updated_at_ms: ctx.now_ms,
        managed: false,
    };
    check(ctx, &m)?;
    ctx.app.propose(Command::PutMediaPipeline { pipeline: m.clone(), expect_generation: None })?;
    ctx.app.audit(ctx, "media_pipeline.create", &m.id);
    let m = tenant_mp(ctx, &m.id)?;
    Ok(ApiResponse::json(201, &render(ctx, &m)).with_header("Location", format!("/api/v1/media-pipelines/{}", m.id)).with_header("ETag", etag(m.generation)))
}

pub fn list(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let (limit, cursor) = ctx.page()?;
    let room = ctx.req.query_pairs().into_iter().find(|(k, _)| k == "room").map(|(_, v)| v);
    let items: Vec<(String, MediaPipeline)> = ctx
        .control()
        .state
        .media_pipelines
        .values()
        .filter(|m| m.tenant == ctx.tenant() && room.as_ref().is_none_or(|r| &m.room == r))
        .map(|m| (m.id.clone(), m.clone()))
        .collect();
    let (page, next) = paginate(items, limit, cursor.as_deref());
    Ok(ApiResponse::json(200, &json!({ "items": page.iter().map(|m| render(ctx, m)).collect::<Vec<_>>(), "next_cursor": next })))
}

pub fn get(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let m = tenant_mp(ctx, id)?;
    Ok(ApiResponse::json(200, &render(ctx, &m)).with_header("ETag", etag(m.generation)))
}

pub fn patch(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let expect = ctx.if_match(true)?.unwrap_or(0);
    let p: Patch = ctx.body()?;
    let mut m = tenant_mp(ctx, id)?;
    managed_refused(&m)?;
    if m.generation != expect {
        return Err(Problem::new(ProblemCode::PreconditionFailed, "資源が更新されています（If-Match が古い）"));
    }
    let before = m.clone();
    if let Some(v) = p.name {
        m.name = v;
    }
    if let Some(v) = p.participants {
        m.participants = v;
    }
    if let Some(v) = p.export_participants {
        m.export_participants = v;
    }
    if let Some(v) = p.composite {
        m.composite = v;
    }
    if let Some(v) = p.recording {
        m.recording = v;
    }
    if let Some(v) = p.egress {
        m.egress = v;
    }
    if let Some(v) = p.captions {
        m.captions = v.map(|c| serde_json::to_value(c).unwrap_or_default());
    }
    if let Some(v) = p.desired_state {
        m.desired_state = v;
    }
    m.updated_at_ms = ctx.now_ms;
    check(ctx, &m)?;
    // 止める・合成をやめる・送り出しを減らす: 子を片付ける（動かすなら係が作り直す）
    if m.desired_state == "stopped" || m.composite.is_none() || m.egress.len() < before.egress.len() || m.egress != before.egress {
        remove_children(ctx, &before);
    }
    ctx.app.propose(Command::PutMediaPipeline { pipeline: m, expect_generation: Some(expect) })?;
    ctx.app.audit(ctx, "media_pipeline.update", id);
    let m = tenant_mp(ctx, id)?;
    Ok(ApiResponse::json(200, &render(ctx, &m)).with_header("ETag", etag(m.generation)))
}

pub fn delete(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let m = tenant_mp(ctx, id)?;
    managed_refused(&m)?;
    ctx.app.propose(Command::DeleteMediaPipeline { tenant: ctx.tenant().into(), id: id.into() })?;
    remove_children(ctx, &m);
    ctx.app.audit(ctx, "media_pipeline.delete", id);
    Ok(ApiResponse::empty(204))
}
