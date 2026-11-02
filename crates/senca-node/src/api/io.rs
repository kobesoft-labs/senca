//! 入力・出力・秘密値の API（仕様 10.2）。実行は `ingress.rs`・`egress_out.rs`（ノードの中の係）。

use super::{ApiResponse, ApiResult, Ctx, etag, new_id, paginate};
use crate::util::rfc3339;
use senca_control::auth::Scope;
use senca_control::model::{Input, Output, SecretEntry};
use senca_control::state::Command;
use senca_control::{Problem, ProblemCode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

const INPUT_PROTOCOLS: &[&str] = &["rtmp_pull", "rtsp_pull", "srt_pull", "hls_pull"];
const OUTPUT_PROTOCOLS: &[&str] = &["rtmp_push", "srt_push", "whip_push", "paas"];

fn bad(t: impl Into<String>) -> Problem {
    Problem::new(ProblemCode::InvalidConfig, t)
}

/// URL の形を検査する（宛先の IP は接続時に egress の方針で検査する）。資格情報を URL に入れさせない。
fn check_url(protocol: &str, url: &str) -> Result<(), Problem> {
    let scheme = url.split_once("://").map(|(s, _)| s).ok_or_else(|| bad("url の形が違います"))?;
    let ok = match protocol {
        "rtmp_pull" | "rtmp_push" => matches!(scheme, "rtmp" | "rtmps"),
        "rtsp_pull" => matches!(scheme, "rtsp" | "rtsps"),
        "srt_pull" | "srt_push" => scheme == "srt",
        "hls_pull" => matches!(scheme, "http" | "https"),
        "whip_push" => matches!(scheme, "http" | "https"),
        _ => false,
    };
    if !ok {
        return Err(bad(format!("{protocol} では {scheme}:// は使えません")));
    }
    if url.len() > 2048 || url.chars().any(|c| c.is_control() || c == ' ') {
        return Err(bad("url が長すぎるか、使えない文字があります"));
    }
    let rest = &url[scheme.len() + 3..];
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    if authority.contains('@') {
        return Err(bad("url に資格情報を入れないでください（credential_ref を使います）"));
    }
    if authority.is_empty() {
        return Err(bad("url にホストがありません"));
    }
    Ok(())
}

fn check_ref(ctx: &Ctx, r: &Option<String>) -> Result<(), Problem> {
    if let Some(r) = r {
        let name = r.strip_prefix("secret:").ok_or_else(|| bad("credential_ref は secret:{name} の形です"))?;
        let core = ctx.control();
        if !core.state.secrets.values().any(|s| s.tenant == ctx.tenant() && s.name == name) {
            return Err(bad(format!("秘密値 {name} がありません")));
        }
    }
    Ok(())
}

fn check_options(o: &BTreeMap<String, String>) -> Result<(), Problem> {
    if o.len() > 16 || o.iter().any(|(k, v)| k.is_empty() || k.len() > 64 || v.len() > 256) {
        return Err(bad("options は 16 個まで、キー 64 文字・値 256 文字まで"));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputBody {
    protocol: String,
    url: String,
    #[serde(default)]
    credential_ref: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    options: BTreeMap<String, String>,
}

pub fn render_input(ctx: &Ctx, i: &Input) -> Value {
    json!({
        "id": i.id, "stream": i.stream, "generation": i.generation, "protocol": i.protocol, "url": i.url,
        "credential_ref": i.credential_ref, "enabled": i.enabled, "options": i.options,
        "created_at": rfc3339(i.created_at_ms), "status": ctx.app.io_status(&i.id),
    })
}

pub fn render_output(ctx: &Ctx, o: &Output) -> Value {
    json!({
        "id": o.id, "stream": o.stream, "generation": o.generation, "protocol": o.protocol, "url": o.url,
        "credential_ref": o.credential_ref, "enabled": o.enabled, "provider": o.provider,
        "provider_resource": o.provider_resource, "options": o.options,
        "created_at": rfc3339(o.created_at_ms), "status": ctx.app.io_status(&o.id),
    })
}

pub fn create_input(ctx: &Ctx, stream: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let b: InputBody = ctx.body()?;
    ctx.control().state.tenant_stream(ctx.tenant(), stream)?;
    if !INPUT_PROTOCOLS.contains(&b.protocol.as_str()) {
        return Err(bad(format!("protocol は {}", INPUT_PROTOCOLS.join("・"))));
    }
    check_url(&b.protocol, &b.url)?;
    check_ref(ctx, &b.credential_ref)?;
    check_options(&b.options)?;
    let i = Input {
        id: new_id()?,
        tenant: ctx.tenant().into(),
        stream: stream.into(),
        generation: 1,
        protocol: b.protocol,
        url: b.url,
        credential_ref: b.credential_ref,
        enabled: b.enabled.unwrap_or(true),
        options: b.options,
        created_at_ms: ctx.now_ms,
    };
    ctx.app.propose(Command::PutInput { input: i.clone() })?;
    ctx.app.io_reconcile();
    ctx.app.audit(ctx, "input.create", &i.id);
    Ok(ApiResponse::json(201, &render_input(ctx, &i)).with_header("Location", format!("/api/v1/inputs/{}", i.id)).with_header("ETag", etag(1)))
}

pub fn list_inputs(ctx: &Ctx, stream: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    ctx.control().state.tenant_stream(ctx.tenant(), stream)?;
    let (limit, cursor) = ctx.page()?;
    let items: Vec<(String, Input)> =
        ctx.control().state.inputs.values().filter(|i| i.stream == stream && i.tenant == ctx.tenant()).map(|i| (i.id.clone(), i.clone())).collect();
    let (page, next) = paginate(items, limit, cursor.as_deref());
    Ok(ApiResponse::json(200, &json!({ "items": page.iter().map(|i| render_input(ctx, i)).collect::<Vec<_>>(), "next_cursor": next })))
}

fn tenant_input(ctx: &Ctx, id: &str) -> Result<Input, Problem> {
    ctx.control().state.inputs.get(id).filter(|i| i.tenant == ctx.tenant()).cloned().ok_or_else(Problem::not_found)
}

fn tenant_output(ctx: &Ctx, id: &str) -> Result<Output, Problem> {
    ctx.control().state.outputs.get(id).filter(|i| i.tenant == ctx.tenant()).cloned().ok_or_else(Problem::not_found)
}

pub fn get_input(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let i = tenant_input(ctx, id)?;
    Ok(ApiResponse::json(200, &render_input(ctx, &i)).with_header("ETag", etag(i.generation)))
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct IoPatch {
    url: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    credential_ref: Option<Option<String>>,
    enabled: Option<bool>,
    options: Option<BTreeMap<String, String>>,
}

fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::deserialize(d)?))
}

pub fn patch_input(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let expect = ctx.if_match(true)?.unwrap_or(0);
    let p: IoPatch = ctx.body()?;
    let mut i = tenant_input(ctx, id)?;
    if i.generation != expect {
        return Err(Problem::new(ProblemCode::PreconditionFailed, "資源が更新されています（If-Match が古い）"));
    }
    if let Some(u) = p.url {
        check_url(&i.protocol, &u)?;
        i.url = u;
    }
    if let Some(c) = p.credential_ref {
        check_ref(ctx, &c)?;
        i.credential_ref = c;
    }
    if let Some(e) = p.enabled {
        i.enabled = e;
    }
    if let Some(o) = p.options {
        check_options(&o)?;
        i.options = o;
    }
    i.generation += 1;
    ctx.app.propose(Command::PutInput { input: i.clone() })?;
    ctx.app.io_reconcile();
    ctx.app.audit(ctx, "input.update", id);
    Ok(ApiResponse::json(200, &render_input(ctx, &i)).with_header("ETag", etag(i.generation)))
}

pub fn delete_input(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    ctx.app.propose(Command::DeleteInput { tenant: ctx.tenant().into(), id: id.into() })?;
    ctx.app.io_reconcile();
    ctx.app.audit(ctx, "input.delete", id);
    Ok(ApiResponse::empty(204))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputBody {
    protocol: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    credential_ref: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    options: BTreeMap<String, String>,
}

pub fn create_output(ctx: &Ctx, stream: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let b: OutputBody = ctx.body()?;
    ctx.control().state.tenant_stream(ctx.tenant(), stream)?;
    if !OUTPUT_PROTOCOLS.contains(&b.protocol.as_str()) {
        return Err(bad(format!("protocol は {}", OUTPUT_PROTOCOLS.join("・"))));
    }
    if b.protocol == "paas" {
        let p = b.provider.as_deref().ok_or_else(|| bad("paas には provider が要ります"))?;
        if !crate::paas::PROVIDERS.contains(&p) {
            return Err(bad(format!("provider は {}", crate::paas::PROVIDERS.join("・"))));
        }
        if b.credential_ref.is_none() {
            return Err(bad("paas には credential_ref（API トークン）が要ります"));
        }
    } else {
        let u = b.url.as_deref().ok_or_else(|| bad("url が要ります"))?;
        check_url(&b.protocol, u)?;
    }
    check_ref(ctx, &b.credential_ref)?;
    check_options(&b.options)?;
    let o = Output {
        id: new_id()?,
        tenant: ctx.tenant().into(),
        stream: stream.into(),
        generation: 1,
        protocol: b.protocol,
        url: b.url,
        credential_ref: b.credential_ref,
        enabled: b.enabled.unwrap_or(true),
        provider: b.provider,
        provider_resource: None,
        options: b.options,
        created_at_ms: ctx.now_ms,
    };
    ctx.app.propose(Command::PutOutput { output: o.clone() })?;
    ctx.app.io_reconcile();
    ctx.app.audit(ctx, "output.create", &o.id);
    Ok(ApiResponse::json(201, &render_output(ctx, &o)).with_header("Location", format!("/api/v1/outputs/{}", o.id)).with_header("ETag", etag(1)))
}

pub fn list_outputs(ctx: &Ctx, stream: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    ctx.control().state.tenant_stream(ctx.tenant(), stream)?;
    let (limit, cursor) = ctx.page()?;
    let items: Vec<(String, Output)> =
        ctx.control().state.outputs.values().filter(|o| o.stream == stream && o.tenant == ctx.tenant()).map(|o| (o.id.clone(), o.clone())).collect();
    let (page, next) = paginate(items, limit, cursor.as_deref());
    Ok(ApiResponse::json(200, &json!({ "items": page.iter().map(|o| render_output(ctx, o)).collect::<Vec<_>>(), "next_cursor": next })))
}

pub fn get_output(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let o = tenant_output(ctx, id)?;
    Ok(ApiResponse::json(200, &render_output(ctx, &o)).with_header("ETag", etag(o.generation)))
}

pub fn patch_output(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let expect = ctx.if_match(true)?.unwrap_or(0);
    let p: IoPatch = ctx.body()?;
    let mut o = tenant_output(ctx, id)?;
    if o.generation != expect {
        return Err(Problem::new(ProblemCode::PreconditionFailed, "資源が更新されています（If-Match が古い）"));
    }
    if let Some(u) = p.url {
        check_url(&o.protocol, &u)?;
        o.url = Some(u);
    }
    if let Some(c) = p.credential_ref {
        check_ref(ctx, &c)?;
        o.credential_ref = c;
    }
    if let Some(e) = p.enabled {
        o.enabled = e;
    }
    if let Some(op) = p.options {
        check_options(&op)?;
        o.options = op;
    }
    o.generation += 1;
    ctx.app.propose(Command::PutOutput { output: o.clone() })?;
    ctx.app.io_reconcile();
    ctx.app.audit(ctx, "output.update", id);
    Ok(ApiResponse::json(200, &render_output(ctx, &o)).with_header("ETag", etag(o.generation)))
}

pub fn delete_output(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let o = tenant_output(ctx, id)?;
    // PaaS で SENCA が作った資源は後片付けする（失敗しても出力の定義は消す。後片付けの失敗はログに残す）
    ctx.app.paas_cleanup(&o);
    ctx.app.propose(Command::DeleteOutput { tenant: ctx.tenant().into(), id: id.into() })?;
    ctx.app.io_reconcile();
    ctx.app.audit(ctx, "output.delete", id);
    Ok(ApiResponse::empty(204))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretBody {
    name: String,
    value: String,
}

fn render_secret(s: &SecretEntry) -> Value {
    json!({ "id": s.id, "name": s.name, "ref": format!("secret:{}", s.name), "created_at": rfc3339(s.created_at_ms) })
}

pub fn create_secret(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    let b: SecretBody = ctx.body()?;
    crate::api::streams::validate_name(&b.name).map_err(|_| bad("name は 1〜64 文字の英数字と -_. です"))?;
    if b.value.is_empty() || b.value.len() > 8192 {
        return Err(bad("value は 1〜8192 バイト"));
    }
    let id = new_id()?;
    let sealed = ctx.app.secret_box.seal(b.value.as_bytes(), &format!("{}/{id}", ctx.tenant())).map_err(|_| Problem::internal())?;
    let s = SecretEntry { id: id.clone(), tenant: ctx.tenant().into(), name: b.name, sealed, created_at_ms: ctx.now_ms };
    ctx.app.propose(Command::PutSecret { secret: s.clone() })?;
    ctx.app.audit(ctx, "secret.create", &id);
    Ok(ApiResponse::json(201, &render_secret(&s)).with_header("Location", format!("/api/v1/secrets/{id}")))
}

pub fn list_secrets(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    let items: Vec<Value> = ctx.control().state.secrets.values().filter(|s| s.tenant == ctx.tenant()).map(render_secret).collect();
    Ok(ApiResponse::json(200, &json!({ "items": items, "next_cursor": null })))
}

pub fn delete_secret(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    let name = ctx.control().state.secrets.get(id).filter(|s| s.tenant == ctx.tenant()).map(|s| s.name.clone()).ok_or_else(Problem::not_found)?;
    let r = format!("secret:{name}");
    let used = {
        let core = ctx.control();
        core.state.inputs.values().any(|i| i.tenant == ctx.tenant() && i.credential_ref.as_deref() == Some(&r))
            || core.state.outputs.values().any(|o| o.tenant == ctx.tenant() && o.credential_ref.as_deref() == Some(&r))
    };
    if used {
        return Err(Problem::new(ProblemCode::Conflict, "入力・出力が使っている秘密値は消せません"));
    }
    ctx.app.propose(Command::DeleteSecret { tenant: ctx.tenant().into(), id: id.into() })?;
    ctx.app.audit(ctx, "secret.delete", id);
    Ok(ApiResponse::empty(204))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert!(check_url("rtmp_push", "rtmp://a.example/live/x").is_ok());
        assert!(check_url("rtmp_push", "rtmp://u:p@a.example/live").is_err());
        assert!(check_url("rtmp_push", "http://a/live").is_err());
        assert!(check_url("hls_pull", "https://cdn.example/x.m3u8").is_ok());
        assert!(check_url("rtsp_pull", "rtsp:///x").is_err());
    }
}
