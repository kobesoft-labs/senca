//! `GET /api/v1/events`（SSE、仕様 10.5）。適用されたコマンドをイベントにして、権限で絞って流す。
//! event_id は制御ログの位置（applied_index）。`Last-Event-ID` より後のものだけを流す。
//! 保持は直近のメモリ分（broadcast の長さ）で、取りこぼしたら `resync` を送って閉じる。

use crate::app::App;
use senca_control::auth::{self, Scope};
use senca_control::state::Command;
use senca_control::{Problem, ProblemCode};
use senca_http::RequestHead;
use senca_http::sse::SseEvent;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt};

pub async fn serve_sse<S: AsyncWrite + Unpin>(app: &Arc<App>, sock: &mut S, h: &RequestHead) {
    let now = crate::util::now_ms();
    let principal = {
        let core = app.control.lock().unwrap();
        auth::authenticate(h.headers.get_str("authorization"), &core.state, core.jwt.as_ref(), now)
    };
    let rid = crate::util::request_id(h.headers.get_str("x-request-id"));
    let deny = |status: u16, code: ProblemCode, title: &str| {
        let mut p = Problem::new(code, title);
        p.request_id = Some(rid.clone());
        let body = serde_json::to_vec(&p).unwrap_or_default();
        let extra = if status == 401 { "WWW-Authenticate: Bearer\r\n" } else { "" };
        let mut v = format!(
            "HTTP/1.1 {status} {}\r\nContent-Type: application/problem+json\r\nX-Request-Id: {rid}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
            senca_http::response::reason_phrase(status),
            body.len()
        )
        .into_bytes();
        v.extend_from_slice(&body);
        v
    };
    let Ok(p) = principal else {
        let _ = sock.write_all(&deny(401, ProblemCode::AuthFailed, "認証に失敗しました")).await;
        return;
    };
    if !p.has(Scope::StreamRead) {
        let _ = sock.write_all(&deny(403, ProblemCode::Forbidden, "権限 stream:read が必要です")).await;
        return;
    }
    let last: u64 = h.headers.get_str("last-event-id").and_then(|v| v.parse().ok()).unwrap_or(0);
    let (mut rx, current) = {
        let core = app.control.lock().unwrap();
        (core.applied_tx.subscribe(), core.state.applied_index)
    };
    // 部屋の出来事（合意ログに載らない。event_id を持たないので Last-Event-ID の再開の対象外）
    let mut room_rx = app.room_events.subscribe();
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
    if sock.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    if last > 0 && last < current {
        // 古い位置からは再同期を求める（ログの中身をここでは保持しない）
        let ev = SseEvent { event: Some("resync"), id: Some(&current.to_string()), retry: None, data: "{}" };
        if let Ok(b) = ev.encode() {
            let _ = sock.write_all(&b).await;
        }
    }
    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(a) => {
                    let Some((ty, tenant, id, data)) = describe(&a.cmd) else { continue };
                    if tenant.as_deref().is_some_and(|t| t != p.tenant) || (tenant.is_none() && !p.has(Scope::NodeAdmin)) {
                        continue;
                    }
                    let data = json!({ "type": ty, "resource_id": id, "occurred_at": crate::util::rfc3339(crate::util::now_ms()), "data": data }).to_string();
                    let idx = a.index.to_string();
                    let ev = SseEvent { event: Some(ty), id: Some(&idx), retry: None, data: &data };
                    match ev.encode() {
                        Ok(b) => if sock.write_all(&b).await.is_err() { return },
                        Err(_) => continue,
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let ev = SseEvent { event: Some("resync"), id: None, retry: None, data: "{}" };
                    if let Ok(b) = ev.encode() { let _ = sock.write_all(&b).await; }
                    return;
                }
                Err(_) => return,
            },
            r = room_rx.recv() => match r {
                Ok(ev) => {
                    if ev.tenant != p.tenant {
                        continue;
                    }
                    let Some(ty) = room_event_name(&ev.envelope.event) else { continue };
                    // RoomEvent は {type, data} の形。SSE の type は上の名前に、中身は data だけにする
                    let body = serde_json::to_value(&ev.envelope.event).ok().and_then(|v| v.get("data").cloned()).unwrap_or_default();
                    let data = json!({
                        "type": ty, "resource_id": ev.envelope.room_id, "event_id": ev.envelope.id, "node": ev.envelope.node,
                        "occurred_at": crate::util::rfc3339(ev.envelope.at), "data": body,
                    })
                    .to_string();
                    let e = SseEvent { event: Some(ty), id: None, retry: None, data: &data };
                    match e.encode() {
                        Ok(b) => if sock.write_all(&b).await.is_err() { return },
                        Err(_) => continue,
                    }
                }
                // 部屋の出来事は保持しない: 取りこぼしは飛ばす
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            },
            _ = tokio::time::sleep(Duration::from_secs(15)) => {
                if sock.write_all(&senca_http::sse::keepalive()).await.is_err() { return }
            }
        }
    }
}

/// 部屋の出来事の SSE の名前（設定の変更は制御のイベント `room.updated` が出すので、ここでは出さない）。
fn room_event_name(e: &senca_rooms::RoomEvent) -> Option<&'static str> {
    use senca_rooms::RoomEvent::*;
    Some(match e {
        RoomCreated { .. } => "room.started",
        RoomUpdated { .. } => return None,
        RoomDestroyed { .. } => "room.stopped",
        ConnectionCreated { .. } => "room.connection.created",
        ConnectionUpdated { .. } => "room.connection.updated",
        ConnectionDestroyed { .. } => "room.connection.destroyed",
        TrackPublished { .. } => "room.track.published",
        TrackUnpublished { .. } => "room.track.unpublished",
        SpotlightChanged { .. } => "room.spotlight.changed",
    })
}

/// コマンド → (種類, テナント, 資源 ID, 中身)。秘密（鍵のハッシュなど）は出さない。
fn describe(c: &Command) -> Option<(&'static str, Option<String>, String, serde_json::Value)> {
    Some(match c {
        Command::CreateStream { stream } => ("stream.created", Some(stream.tenant.clone()), stream.id.clone(), json!({ "name": stream.name })),
        Command::UpdateStream { stream, .. } => {
            ("stream.updated", Some(stream.tenant.clone()), stream.id.clone(), json!({ "generation": stream.generation + 1 }))
        }
        Command::DeleteStream { tenant, id, .. } => ("stream.deleted", Some(tenant.clone()), id.clone(), json!({})),
        Command::PutRecording { recording: r } => (
            "recording.updated",
            Some(r.tenant.clone()),
            r.id.clone(),
            json!({ "stream": r.stream, "state": r.state, "generation": r.generation }),
        ),
        Command::DeleteRecording { tenant, id } => ("recording.deleted", Some(tenant.clone()), id.clone(), json!({})),
        Command::CreateSchedule { schedule: s } => ("schedule.created", Some(s.tenant.clone()), s.id.clone(), json!({})),
        Command::UpdateSchedule { schedule: s, .. } => ("schedule.updated", Some(s.tenant.clone()), s.id.clone(), json!({})),
        Command::DeleteSchedule { tenant, id, .. } => ("schedule.deleted", Some(tenant.clone()), id.clone(), json!({})),
        Command::RecordScheduleRun { tenant, id, state, run } => (
            "schedule.run",
            Some(tenant.clone()),
            id.clone(),
            json!({ "state": state, "occurrence": run.occurrence, "run_state": run.state, "recording": run.recording }),
        ),
        Command::PutNode { node } => ("node.updated", None, node.id.clone(), json!({ "draining": node.draining })),
        Command::PutInput { input: i } => ("input.updated", Some(i.tenant.clone()), i.id.clone(), json!({ "stream": i.stream, "generation": i.generation, "enabled": i.enabled })),
        Command::DeleteInput { tenant, id } => ("input.deleted", Some(tenant.clone()), id.clone(), json!({})),
        Command::PutOutput { output: o } => ("output.updated", Some(o.tenant.clone()), o.id.clone(), json!({ "stream": o.stream, "generation": o.generation, "enabled": o.enabled })),
        Command::DeleteOutput { tenant, id } => ("output.deleted", Some(tenant.clone()), id.clone(), json!({})),
        Command::PutSecret { secret: s } => ("secret.created", Some(s.tenant.clone()), s.id.clone(), json!({ "name": s.name })),
        Command::DeleteSecret { tenant, id } => ("secret.deleted", Some(tenant.clone()), id.clone(), json!({})),
        Command::PutPipeline { pipeline: p, .. } => ("pipeline.updated", Some(p.tenant.clone()), p.id.clone(), json!({ "name": p.name, "desired_state": p.desired_state })),
        Command::DeletePipeline { tenant, id } => ("pipeline.deleted", Some(tenant.clone()), id.clone(), json!({})),
        Command::ClaimStream { stream, node, .. } => ("stream.owner", None, stream.clone(), json!({ "node": node })),
        Command::PutRoom { room: r, .. } => ("room.updated", Some(r.tenant.clone()), r.id.clone(), json!({ "auto": r.auto })),
        Command::DeleteRoom { tenant, id } => ("room.deleted", Some(tenant.clone()), id.clone(), json!({})),
        Command::PutMediaPipeline { pipeline: m, .. } => {
            ("media_pipeline.updated", Some(m.tenant.clone()), m.id.clone(), json!({ "room": m.room, "desired_state": m.desired_state }))
        }
        Command::DeleteMediaPipeline { tenant, id } => ("media_pipeline.deleted", Some(tenant.clone()), id.clone(), json!({})),
        // グリッド: 運用の権限だけに見せる（テナントの外）
        Command::Grid { cmd } => match cmd {
            senca_grid::GridCommand::NodeDown { node, .. } => ("node.down", None, node.clone(), json!({})),
            senca_grid::GridCommand::NodeUp { node, .. } => ("node.up", None, node.clone(), json!({})),
            senca_grid::GridCommand::DeviceDown { node, device } => ("device.down", None, node.clone(), json!({ "device": device })),
            senca_grid::GridCommand::DeviceUp { node, device } => ("device.up", None, node.clone(), json!({ "device": device })),
            senca_grid::GridCommand::DisableDevice { node, device } => ("device.disabled", None, node.clone(), json!({ "device": device })),
            senca_grid::GridCommand::EnableDevice { node, device } => ("device.enabled", None, node.clone(), json!({ "device": device })),
            senca_grid::GridCommand::ApplyDecisions { decisions, .. } => ("grid.decisions", None, String::new(), json!({ "decisions": decisions })),
            _ => return None,
        },
        _ => return None,
    })
}
