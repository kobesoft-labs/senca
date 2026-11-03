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
    // 制御状態の鍵を放してから描く（自分のノードは部屋の係にも問い合わせる）
    let nodes: Vec<_> = ctx.control().state.nodes.values().cloned().collect();
    let items: Vec<_> = nodes.iter().map(|n| ctx.app.render_node(n)).collect();
    Ok(ApiResponse::json(200, &json!({ "items": items })))
}

pub fn node(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::NodeAdmin)?;
    let n = ctx.control().state.nodes.get(id).cloned().ok_or_else(Problem::not_found)?;
    Ok(ApiResponse::json(200, &ctx.app.render_node(&n)))
}

pub fn drain(ctx: &Ctx, id: &str, on: bool) -> ApiResult {
    ctx.require(Scope::NodeAdmin)?;
    let mut n = ctx.control().state.nodes.get(id).cloned().ok_or_else(Problem::not_found)?;
    n.draining = on;
    ctx.app.propose(Command::PutNode { node: n.clone() })?;
    // グリッドにも伝える: 新しい仕事を置かず、置いてある仕事は移せるものから移す（ADR-0013）
    if ctx.control().state.grid.nodes.contains_key(id) {
        ctx.app.propose(Command::Grid { cmd: senca_grid::GridCommand::SetDraining { node: id.to_string(), draining: on } })?;
    }
    ctx.app.audit(ctx, if on { "node.drain" } else { "node.undrain" }, id);
    Ok(ApiResponse::json(202, &ctx.app.render_node(&n)))
}

/// グリッドの様子（ノードの資源・予約・GPU、仕事の割り当て・縮退・置けない理由）。
pub fn grid(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::NodeAdmin)?;
    Ok(ApiResponse::json(200, &crate::grid::overview(ctx.app)))
}

/// 装置（GPU・キャプチャ機器）を手で止める・戻す。止めた装置の仕事は別の装置へ移る（または縮退する）。
/// 測った健康とは別の印なので、心拍でもノードの申告し直しでも戻らない（手で戻すまで）。ノードは ID か名前。
pub fn device_health(ctx: &Ctx, node: &str, device: &str, enable: bool) -> ApiResult {
    ctx.require(Scope::NodeAdmin)?;
    let node_id = {
        let core = ctx.control();
        let id = core.state.nodes.values().find(|n| n.id == node || n.name == node).map(|n| n.id.clone()).ok_or_else(Problem::not_found)?;
        let g = core.state.grid.nodes.get(&id).ok_or_else(Problem::not_found)?;
        if !g.spec.gpus.iter().any(|x| x.id == device) && !g.spec.capture.iter().any(|x| x.id == device) {
            return Err(Problem::not_found());
        }
        id
    };
    let cmd = if enable {
        senca_grid::GridCommand::EnableDevice { node: node_id.clone(), device: device.to_string() }
    } else {
        senca_grid::GridCommand::DisableDevice { node: node_id.clone(), device: device.to_string() }
    };
    ctx.app.propose(Command::Grid { cmd })?;
    ctx.app.audit(ctx, if enable { "device.enable" } else { "device.disable" }, &format!("{node_id}/{device}"));
    Ok(ApiResponse::json(202, &json!({ "node": node_id, "device": device, "enabled": enable })))
}

pub fn devices(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::NodeAdmin)?;
    Ok(ApiResponse::json(200, &ctx.app.devices()))
}

/// SSE は HTTP 層で扱う（長く開いたままの応答なので）。ここに来たら設定の誤り。
pub fn events_unsupported() -> ApiResult {
    Err(Problem::new(ProblemCode::InternalError, "events は HTTP 層で扱います"))
}
