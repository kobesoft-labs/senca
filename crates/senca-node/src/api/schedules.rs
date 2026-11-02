//! 予約の API（REQ-002）。実行は `scheduler.rs`。

use super::{ApiResponse, ApiResult, Ctx, etag, new_id, paginate};
use crate::util::rfc3339;
use senca_control::auth::Scope;
use senca_control::model::*;
use senca_control::schedule::{self, MAX_CONCURRENT_PER_STREAM, MAX_WINDOW_MS};
use senca_control::state::Command;
use senca_control::{Problem, ProblemCode};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    stream: String,
    #[serde(default)]
    name: Option<String>,
    action: ScheduleAction,
    start_at: String,
    end_at: String,
    #[serde(default)]
    pre_roll_ms: Option<u32>,
    #[serde(default)]
    post_roll_ms: Option<u32>,
    #[serde(default)]
    grace_ms: Option<u32>,
    #[serde(default)]
    snapshot_interval_secs: Option<u32>,
    #[serde(default)]
    recurrence: Option<Recurrence>,
    #[serde(default)]
    retention_days: Option<u32>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Patch {
    name: Option<String>,
    start_at: Option<String>,
    end_at: Option<String>,
    pre_roll_ms: Option<u32>,
    post_roll_ms: Option<u32>,
    grace_ms: Option<u32>,
    snapshot_interval_secs: Option<u32>,
    retention_days: Option<u32>,
    /// "canceled" だけ受け付ける。
    state: Option<String>,
}

fn time(s: &str, field: &str) -> Result<i64, Problem> {
    senca_core::rfc3339::parse_millis(s).ok_or_else(|| Problem::new(ProblemCode::InvalidConfig, format!("{field} の形が違います（RFC 3339）")))
}

pub fn render(s: &Schedule) -> Value {
    json!({
        "id": s.id,
        "stream": s.stream,
        "name": s.name,
        "generation": s.generation,
        "action": s.action,
        "start_at": rfc3339(s.start_at_ms),
        "end_at": rfc3339(s.end_at_ms),
        "pre_roll_ms": s.pre_roll_ms,
        "post_roll_ms": s.post_roll_ms,
        "grace_ms": s.grace_ms,
        "snapshot_interval_secs": s.snapshot_interval_secs,
        "recurrence": s.recurrence,
        "retention_days": s.retention_days,
        "state": s.state,
        "created_at": rfc3339(s.created_at_ms),
        "last_run": s.runs.last().map(render_run),
    })
}

fn render_run(r: &ScheduleRun) -> Value {
    json!({
        "occurrence": r.occurrence,
        "start_at": rfc3339(r.start_at_ms),
        "end_at": rfc3339(r.end_at_ms),
        "state": r.state,
        "recording": r.recording,
        "reason": r.reason,
    })
}

/// 検査（作成・変更・check で共通）。問題があれば 409 / 400。
fn check_schedule(ctx: &Ctx, s: &Schedule) -> Result<Value, Problem> {
    let bad = |t: &str| Problem::new(ProblemCode::InvalidConfig, t.to_string());
    if s.end_at_ms <= s.start_at_ms {
        return Err(bad("end_at は start_at より後にしてください"));
    }
    if s.end_at_ms - s.start_at_ms > MAX_WINDOW_MS {
        return Err(bad("予約の長さは 24 時間まで"));
    }
    if s.pre_roll_ms > 300_000 || s.post_roll_ms > 300_000 || s.grace_ms > 3_600_000 {
        return Err(bad("pre_roll_ms・post_roll_ms は 300000 まで、grace_ms は 3600000 まで"));
    }
    if s.end_at_ms <= ctx.now_ms {
        return Err(bad("終了時刻が過去です"));
    }
    match (s.action, s.snapshot_interval_secs) {
        (ScheduleAction::Snapshot, None) => return Err(bad("snapshot には snapshot_interval_secs が要ります")),
        (ScheduleAction::Snapshot, Some(i)) if !(1..=3600).contains(&i) => return Err(bad("snapshot_interval_secs は 1〜3600")),
        (ScheduleAction::Record | ScheduleAction::Live, Some(_)) => return Err(bad("snapshot_interval_secs は snapshot のときだけ")),
        _ => {}
    }
    if let Some(r) = &s.recurrence {
        if r.freq != "daily" && r.freq != "weekly" {
            return Err(bad("recurrence.freq は daily か weekly"));
        }
        if r.weekdays.iter().any(|d| *d > 6) {
            return Err(bad("recurrence.weekdays は 0〜6"));
        }
        if ctx.app.timezone(&r.timezone).is_none() {
            return Err(bad("recurrence.timezone が見つかりません"));
        }
    }
    let core = ctx.control();
    core.state.tenant_stream(&s.tenant, &s.stream)?;
    // 同じ stream の録画予約の同時数
    let mut conflicts = Vec::new();
    if s.action == ScheduleAction::Record {
        let others: Vec<&Schedule> = core
            .state
            .schedules
            .values()
            .filter(|o| o.stream == s.stream && o.id != s.id && o.action == ScheduleAction::Record)
            .collect();
        let a = s.start_at_ms - s.pre_roll_ms as i64;
        let b = s.end_at_ms + s.post_roll_ms as i64;
        let n = schedule::max_concurrent(&others, a, b);
        for o in &others {
            if o.start_at_ms - (o.pre_roll_ms as i64) < b && a < o.end_at_ms + o.post_roll_ms as i64 {
                conflicts.push(json!({ "id": o.id, "start_at": rfc3339(o.start_at_ms), "end_at": rfc3339(o.end_at_ms) }));
            }
        }
        if n + 1 > MAX_CONCURRENT_PER_STREAM {
            return Err(Problem::new(ProblemCode::Conflict, "同じ stream の同時の録画予約が上限を超えます")
                .with_details(json!({ "limit": MAX_CONCURRENT_PER_STREAM, "overlaps": conflicts })));
        }
    }
    Ok(json!({ "ok": true, "overlaps": conflicts, "shared_input": !conflicts.is_empty() }))
}

fn from_body(ctx: &Ctx, b: Body) -> Result<Schedule, Problem> {
    Ok(Schedule {
        id: new_id()?,
        tenant: ctx.tenant().to_string(),
        stream: b.stream,
        generation: 1,
        name: b.name,
        action: b.action,
        start_at_ms: time(&b.start_at, "start_at")?,
        end_at_ms: time(&b.end_at, "end_at")?,
        pre_roll_ms: b.pre_roll_ms.unwrap_or(0),
        post_roll_ms: b.post_roll_ms.unwrap_or(0),
        grace_ms: b.grace_ms.unwrap_or(60_000),
        snapshot_interval_secs: b.snapshot_interval_secs,
        recurrence: b.recurrence,
        retention_days: b.retention_days,
        state: ScheduleState::Pending,
        runs: vec![],
        created_at_ms: ctx.now_ms,
    })
}

pub fn create(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let s = from_body(ctx, ctx.body()?)?;
    check_schedule(ctx, &s)?;
    ctx.app.propose(Command::CreateSchedule { schedule: s.clone() })?;
    ctx.app.audit(ctx, "schedule.create", &s.id);
    ctx.app.wake_scheduler();
    Ok(ApiResponse::json(201, &render(&s))
        .with_header("Location", format!("/api/v1/schedules/{}", s.id))
        .with_header("ETag", etag(s.generation)))
}

pub fn check(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let s = from_body(ctx, ctx.body()?)?;
    let v = check_schedule(ctx, &s)?;
    Ok(ApiResponse::json(200, &v))
}

pub fn list(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let (limit, cursor) = ctx.page()?;
    let q = ctx.req.query_pairs();
    let stream = q.iter().find(|(k, _)| k == "stream").map(|(_, v)| v.clone());
    let state: Option<ScheduleState> = match q.iter().find(|(k, _)| k == "state") {
        None => None,
        Some((_, v)) => Some(
            serde_json::from_value(Value::String(v.clone()))
                .map_err(|_| Problem::invalid("state は pending・running・completed・missed・canceled・failed"))?,
        ),
    };
    let items: Vec<(String, Schedule)> = ctx
        .control()
        .state
        .schedules
        .values()
        .filter(|s| s.tenant == ctx.tenant() && stream.as_ref().is_none_or(|x| *x == s.stream) && state.is_none_or(|x| x == s.state))
        .map(|s| (s.id.clone(), s.clone()))
        .collect();
    let (page, next) = paginate(items, limit, cursor.as_deref());
    let items: Vec<Value> = page.iter().map(render).collect();
    Ok(ApiResponse::json(200, &json!({ "items": items, "next_cursor": next })))
}

pub fn get(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let s = ctx.control().state.tenant_schedule(ctx.tenant(), id)?.clone();
    Ok(ApiResponse::json(200, &render(&s)).with_header("ETag", etag(s.generation)))
}

pub fn runs(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamRead)?;
    let s = ctx.control().state.tenant_schedule(ctx.tenant(), id)?.clone();
    let items: Vec<Value> = s.runs.iter().map(render_run).collect();
    Ok(ApiResponse::json(200, &json!({ "items": items })))
}

pub fn patch(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let expect = ctx.if_match(true)?.unwrap_or(0);
    let p: Patch = ctx.body()?;
    let mut s = ctx.control().state.tenant_schedule(ctx.tenant(), id)?.clone();
    if s.generation != expect {
        return Err(Problem::new(ProblemCode::PreconditionFailed, "資源が更新されています（If-Match が古い）"));
    }
    let running = s.state == ScheduleState::Running;
    if let Some(v) = p.start_at {
        if running {
            return Err(Problem::new(ProblemCode::Conflict, "実行中の予約の開始時刻は変えられません"));
        }
        s.start_at_ms = time(&v, "start_at")?;
    }
    if let Some(v) = p.end_at {
        s.end_at_ms = time(&v, "end_at")?;
    }
    if let Some(v) = p.name {
        s.name = Some(v);
    }
    if let Some(v) = p.pre_roll_ms {
        s.pre_roll_ms = v;
    }
    if let Some(v) = p.post_roll_ms {
        s.post_roll_ms = v;
    }
    if let Some(v) = p.grace_ms {
        s.grace_ms = v;
    }
    if let Some(v) = p.snapshot_interval_secs {
        s.snapshot_interval_secs = Some(v);
    }
    if let Some(v) = p.retention_days {
        s.retention_days = Some(v);
    }
    match p.state.as_deref() {
        None => {}
        Some("canceled") => s.state = ScheduleState::Canceled,
        Some(_) => return Err(Problem::new(ProblemCode::InvalidConfig, "state は canceled だけ指定できます")),
    }
    if s.state != ScheduleState::Canceled {
        check_schedule(ctx, &s)?;
    }
    ctx.app.propose(Command::UpdateSchedule { schedule: s, expect_generation: expect })?;
    let s = ctx.control().state.tenant_schedule(ctx.tenant(), id)?.clone();
    ctx.app.audit(ctx, "schedule.update", id);
    ctx.app.wake_scheduler();
    Ok(ApiResponse::json(200, &render(&s)).with_header("ETag", etag(s.generation)))
}

pub fn delete(ctx: &Ctx, id: &str) -> ApiResult {
    ctx.require(Scope::StreamWrite)?;
    let expect = ctx.if_match(false)?;
    ctx.app.propose(Command::DeleteSchedule { tenant: ctx.tenant().to_string(), id: id.to_string(), expect_generation: expect })?;
    ctx.app.audit(ctx, "schedule.delete", id);
    ctx.app.wake_scheduler();
    Ok(ApiResponse::empty(204))
}
