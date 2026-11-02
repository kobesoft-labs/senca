//! API トークン（仕様 10.2 の表にない追加。ADR-0005）。

use super::{ApiResponse, ApiResult, Ctx, new_id, paginate};
use crate::util::rfc3339;
use senca_control::auth::{Scope, TOKEN_PREFIX};
use senca_control::model::ApiToken;
use senca_control::secret;
use senca_control::state::Command;
use senca_control::{Problem, ProblemCode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenBody {
    name: String,
    scopes: Vec<String>,
    #[serde(default)]
    expires_at: Option<String>,
}

fn render(t: &ApiToken, value: Option<&str>) -> Value {
    json!({
        "id": t.id,
        "name": t.name,
        "scopes": t.scopes,
        "created_at": rfc3339(t.created_at_ms),
        "expires_at": t.expires_at_ms.map(rfc3339),
        "token": value,
    })
}

pub fn create(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    let b: TokenBody = ctx.body()?;
    if b.name.is_empty() || b.name.len() > 64 {
        return Err(Problem::new(ProblemCode::InvalidConfig, "name は 1〜64 文字"));
    }
    let mut scopes = BTreeSet::new();
    for s in &b.scopes {
        let sc = Scope::parse(s).ok_or_else(|| Problem::new(ProblemCode::InvalidConfig, format!("知らない権限: {s}")))?;
        // 自分が持つ権限しか渡せない
        if !ctx.principal.has(sc) {
            return Err(Problem::new(ProblemCode::Forbidden, format!("持っていない権限は渡せません: {s}")));
        }
        scopes.insert(sc);
    }
    if scopes.is_empty() {
        return Err(Problem::new(ProblemCode::InvalidConfig, "scopes が空です"));
    }
    let expires_at_ms = match &b.expires_at {
        None => None,
        Some(s) => {
            let t = senca_core::rfc3339::parse_millis(s).ok_or_else(|| Problem::new(ProblemCode::InvalidConfig, "expires_at の形が違います"))?;
            if t <= ctx.now_ms {
                return Err(Problem::new(ProblemCode::InvalidConfig, "expires_at が過去です"));
            }
            Some(t)
        }
    };
    let value = secret::generate(TOKEN_PREFIX).map_err(|_| Problem::internal())?;
    let t = ApiToken {
        id: new_id()?,
        tenant: ctx.tenant().to_string(),
        name: b.name,
        hash: secret::hash(value.expose()),
        scopes,
        created_at_ms: ctx.now_ms,
        expires_at_ms,
    };
    ctx.control().propose(Command::CreateToken { token: t.clone() })?;
    ctx.app.audit(ctx, "token.create", &t.id);
    Ok(ApiResponse::json(201, &render(&t, Some(value.expose()))).with_header("Location", format!("/api/v1/tokens/{}", t.id)))
}

pub fn list(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    let (limit, cursor) = ctx.page()?;
    let items: Vec<(String, ApiToken)> = ctx
        .control()
        .state
        .tokens
        .values()
        .filter(|t| t.tenant == ctx.tenant())
        .map(|t| (t.id.clone(), t.clone()))
        .collect();
    let (page, next) = paginate(items, limit, cursor.as_deref());
    let items: Vec<Value> = page.iter().map(|t| render(t, None)).collect();
    Ok(ApiResponse::json(200, &json!({ "items": items, "next_cursor": next })))
}

pub fn delete(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    ctx.control().propose(Command::DeleteToken { tenant: ctx.tenant().to_string(), id: id.to_string() })?;
    ctx.app.audit(ctx, "token.delete", id);
    Ok(ApiResponse::empty(204))
}
