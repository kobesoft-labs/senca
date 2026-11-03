//! パイプラインの API（REQ-002・REQ-005・REQ-008）。
//! POST /pipelines・GET /pipelines・GET/PATCH/DELETE /pipelines/{id}・POST /pipelines/{id}/validate・
//! POST /pipelines/{id}/control（オペレーターの操作）・GET /pipelines/{id}/snapshots（板書の変化の静止画）・
//! GET/DELETE /pipelines/{id}/snapshots/{snapshot_id}

use super::{ApiResponse, ApiResult, Ctx, etag, new_id, paginate};
use crate::util::rfc3339;
use senca_control::auth::Scope;
use senca_control::model::Pipeline;
use senca_control::state::Command;
use senca_control::{Problem, ProblemCode};
use senca_pipeline::{Graph, ValidationContext};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    name: String,
    graph: Value,
    #[serde(default)]
    desired_state: Option<String>,
    #[serde(default)]
    placement: Option<senca_control::model::PlacementSpec>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Patch {
    name: Option<String>,
    graph: Option<Value>,
    desired_state: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    placement: Option<Option<senca_control::model::PlacementSpec>>,
}

fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::deserialize(d)?))
}

fn render(ctx: &Ctx, p: &Pipeline) -> Value {
    let rt = ctx.app.pipelines.runtime(&p.id);
    json!({
        "id": p.id, "name": p.name, "generation": p.generation, "graph": p.graph, "desired_state": p.desired_state,
        "created_at": rfc3339(p.created_at_ms), "updated_at": rfc3339(p.updated_at_ms),
        "observed_state": rt.as_ref().map(|r| r.state.clone()).unwrap_or_else(|| if p.desired_state == "running" { "starting".into() } else { "stopped".into() }),
        "last_error": rt.as_ref().and_then(|r| r.last_error.clone()),
        "plan": rt.as_ref().and_then(|r| r.plan.clone()),
        "status": rt.as_ref().and_then(|r| r.status.clone()),
        "meters": rt.as_ref().map(|r| json!(r.meters)),
        "placement": p.placement,
        "placement_status": super::placement::status(ctx, &format!("{}{}", senca_control::workloads::PIPELINE, p.id)),
    })
}

/// グラフを検証する（参照する stream がテナントのものか、方針・装置の検査）。計画を返す。
fn check(ctx: &Ctx, graph: &Value) -> Result<Value, Problem> {
    let g = Graph::from_json(&graph.to_string()).map_err(|e| Problem::new(ProblemCode::InvalidConfig, "グラフが不正です").with_details(json!({ "reason": format!("{e:?}") })))?;
    let mut rf = false;
    {
        let core = ctx.control();
        let sources = crate::pipelines::graph_sources(&g);
        for s in sources.iter().cloned().chain(crate::pipelines::sink_streams(&g)) {
            let st = core.state.tenant_stream(ctx.tenant(), &s).map_err(|_| Problem::new(ProblemCode::InvalidConfig, format!("stream {s} がありません")))?;
            rf |= st.codec_profile == senca_control::model::CodecProfile::RoyaltyFree;
            // E2EE の部屋から書き出した stream はパイプラインの入力にできない（復号・合成・変換はサーバーでできない）
            if sources.contains(&s) && crate::api::streams::stream_is_e2ee(&core.state, st) {
                return Err(crate::api::streams::e2ee_refused(&format!("パイプラインの入力（{s}）")));
            }
        }
    }
    let caps = ctx.app.codec.as_ref().map(|c| c.capability_list()).unwrap_or_default();
    let vc = ValidationContext { policy: senca_codec::Policy { royalty_free_only: rf, ..Default::default() }, capabilities: caps, limits: Default::default() };
    match senca_pipeline::validate(&g, &vc) {
        Ok(plan) => Ok(serde_json::to_value(plan).unwrap_or(Value::Null)),
        Err(senca_pipeline::PipelineError::Invalid(issues)) => {
            let unavailable = issues.iter().any(|i| format!("{:?}", i.code).contains("Capacity"));
            let code = if unavailable { ProblemCode::CapacityUnavailable } else { ProblemCode::InvalidConfig };
            Err(Problem::new(code, "パイプラインを実行できません").with_retryable(false).with_details(json!({ "issues": issues })))
        }
        Err(e) => Err(Problem::new(ProblemCode::InvalidConfig, format!("{e:?}"))),
    }
}

fn state_ok(s: &str) -> Result<(), Problem> {
    if s == "running" || s == "stopped" { Ok(()) } else { Err(Problem::new(ProblemCode::InvalidConfig, "desired_state は running か stopped")) }
}

pub fn create(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let b: Body = ctx.body()?;
    crate::api::streams::validate_name(&b.name)?;
    let desired = b.desired_state.unwrap_or_else(|| "stopped".into());
    state_ok(&desired)?;
    check(ctx, &b.graph)?;
    let placement = super::placement::check(ctx, b.placement, Some(&b.graph))?;
    let p = Pipeline {
        id: new_id()?,
        tenant: ctx.tenant().into(),
        name: b.name,
        generation: 1,
        graph: b.graph,
        desired_state: desired,
        created_at_ms: ctx.now_ms,
        updated_at_ms: ctx.now_ms,
        placement,
    };
    ctx.app.propose(Command::PutPipeline { pipeline: p.clone(), expect_generation: None })?;
    ctx.app.pipelines_reconcile();
    ctx.app.audit(ctx, "pipeline.create", &p.id);
    Ok(ApiResponse::json(201, &render(ctx, &p)).with_header("Location", format!("/api/v1/pipelines/{}", p.id)).with_header("ETag", etag(1)))
}

pub fn list(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let (limit, cursor) = ctx.page()?;
    let items: Vec<(String, Pipeline)> = ctx.control().state.pipelines.values().filter(|p| p.tenant == ctx.tenant()).map(|p| (p.id.clone(), p.clone())).collect();
    let (page, next) = paginate(items, limit, cursor.as_deref());
    Ok(ApiResponse::json(200, &json!({ "items": page.iter().map(|p| render(ctx, p)).collect::<Vec<_>>(), "next_cursor": next })))
}

fn get_p(ctx: &Ctx, id: &str) -> Result<Pipeline, Problem> {
    ctx.control().state.pipelines.get(id).filter(|p| p.tenant == ctx.tenant()).cloned().ok_or_else(Problem::not_found)
}

pub fn get(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let p = get_p(ctx, id)?;
    Ok(ApiResponse::json(200, &render(ctx, &p)).with_header("ETag", etag(p.generation)))
}

pub fn validate(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let p = get_p(ctx, id)?;
    let graph = if ctx.req.body.is_empty() { p.graph } else { ctx.body::<Value>()? };
    let plan = check(ctx, &graph)?;
    Ok(ApiResponse::json(200, &json!({ "ok": true, "plan": plan })))
}

pub fn patch(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let expect = ctx.if_match(true)?.unwrap_or(0);
    let b: Patch = ctx.body()?;
    let mut p = get_p(ctx, id)?;
    if let Some(n) = b.name {
        crate::api::streams::validate_name(&n)?;
        p.name = n;
    }
    if let Some(g) = b.graph {
        check(ctx, &g)?;
        p.graph = g;
    }
    if let Some(s) = b.desired_state {
        state_ok(&s)?;
        p.desired_state = s;
    }
    match b.placement {
        Some(pl) => p.placement = super::placement::check(ctx, pl, Some(&p.graph))?,
        // 図だけ変えたとき: 今の縮退の段が新しい図に合うか確かめる
        None => p.placement = super::placement::check(ctx, p.placement.take(), Some(&p.graph))?,
    }
    p.updated_at_ms = ctx.now_ms;
    ctx.app.propose(Command::PutPipeline { pipeline: p, expect_generation: Some(expect) })?;
    ctx.app.pipelines_reconcile();
    ctx.app.audit(ctx, "pipeline.update", id);
    let p = get_p(ctx, id)?;
    Ok(ApiResponse::json(200, &render(ctx, &p)).with_header("ETag", etag(p.generation)))
}

pub fn delete(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    ctx.app.propose(Command::DeletePipeline { tenant: ctx.tenant().into(), id: id.into() })?;
    ctx.app.pipelines_reconcile();
    // 板書の変化の静止画も消す（このノードに置いた分）
    let _ = std::fs::remove_dir_all(crate::snapshot::pipeline_dir(ctx.app, id));
    ctx.app.audit(ctx, "pipeline.delete", id);
    Ok(ApiResponse::empty(204))
}

/// `snapshot_on_change` が保存した静止画の一覧（古い順。`node` に部品の id）。
pub fn snapshots_list(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    get_p(ctx, id)?;
    let items = crate::snapshot::list_in(&crate::snapshot::pipeline_dir(ctx.app, id));
    Ok(ApiResponse::json(200, &json!({ "items": items })))
}

pub fn snapshot_get(ctx: &Ctx, id: &str, sid: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    get_p(ctx, id)?;
    let (mime, b) = crate::snapshot::get_in(&crate::snapshot::pipeline_dir(ctx.app, id), sid).ok_or_else(Problem::not_found)?;
    Ok(ApiResponse::bytes(200, mime, b))
}

pub fn snapshot_delete(ctx: &Ctx, id: &str, sid: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    get_p(ctx, id)?;
    if !crate::snapshot::delete_in(&crate::snapshot::pipeline_dir(ctx.app, id), sid) {
        return Err(Problem::not_found());
    }
    Ok(ApiResponse::empty(204))
}

/// オペレーターの操作（シーンの切り替え、トランジション、フェーダーなど。senca-pipeline の Control の JSON）。
pub fn control(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    get_p(ctx, id)?;
    let c: senca_pipeline::Control = ctx.body()?;
    if matches!(c, senca_pipeline::Control::Prepare { .. } | senca_pipeline::Control::Commit { .. } | senca_pipeline::Control::Rollback { .. }) {
        return Err(Problem::invalid("世代の操作は PATCH の graph で行います"));
    }
    let app = ctx.app;
    let r = app.rt.get().ok_or_else(Problem::internal)?.block_on(app.pipelines.control(id, c));
    match r {
        Ok(v) => Ok(ApiResponse::json(200, &json!({ "ok": true, "reply": v }))),
        // 動いていない → 409、操作の中身が誤り（知らない部品・シーンなど）→ 400
        Err(e) if e.contains("動いていません") || e.contains("止まりました") => Err(Problem::new(ProblemCode::Conflict, e)),
        Err(e) => Err(Problem::invalid("操作を受け付けられません").with_details(serde_json::json!({ "reason": e }))),
    }
}
