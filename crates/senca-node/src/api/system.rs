//! capabilities・ノード・デバイス。

use super::{ApiResponse, ApiResult, Ctx};
use senca_control::auth::Scope;
use senca_control::state::Command;
use senca_control::{Problem, ProblemCode};
use serde_json::json;

pub fn capabilities(ctx: &Ctx) -> ApiResult {
    Ok(ApiResponse::json(200, &ctx.app.capabilities()))
}

pub fn nodes(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::NodeAdmin)?;
    let items: Vec<_> = ctx.control().state.nodes.values().map(|n| ctx.app.render_node(n)).collect();
    Ok(ApiResponse::json(200, &json!({ "items": items })))
}

pub fn node(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::NodeAdmin)?;
    let n = ctx.control().state.nodes.get(id).cloned().ok_or_else(Problem::not_found)?;
    Ok(ApiResponse::json(200, &ctx.app.render_node(&n)))
}

pub fn drain(ctx: &Ctx, id: &str, on: bool) -> ApiResult {
    ctx.require(Scope::NodeAdmin)?;
    let mut core = ctx.control();
    let mut n = core.state.nodes.get(id).cloned().ok_or_else(Problem::not_found)?;
    n.draining = on;
    core.propose(Command::PutNode { node: n.clone() })?;
    drop(core);
    ctx.app.audit(ctx, if on { "node.drain" } else { "node.undrain" }, id);
    Ok(ApiResponse::json(202, &ctx.app.render_node(&n)))
}

pub fn devices(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::NodeAdmin)?;
    Ok(ApiResponse::json(200, &ctx.app.devices()))
}

/// SSE は HTTP 層で扱う（長く開いたままの応答なので）。ここに来たら設定の誤り。
pub fn events_unsupported() -> ApiResult {
    Err(Problem::new(ProblemCode::InternalError, "events は HTTP 層で扱います"))
}
