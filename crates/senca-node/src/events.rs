//! `GET /api/v1/events`（SSE、仕様 10.5）。適用されたコマンドをイベントにして、権限で絞って流す。
//! event_id は制御ログの位置（applied_index。単一ノードでもクラスタでも同じ数え方で、クラスタのどのノードでも同じ）。
//! `Last-Event-ID` で再接続すると、ノードが残している直近のイベント（[`crate::control::EventLog`]、既定 4096 件、
//! 単一ノードでは再起動をまたいで直近の写しの後ろの分も）から続きを送る。残している範囲より古い位置なら
//! `resync`（id は今の位置）を送ってから今のイベントを流す。送るのが追いつかず通知の列からあふれたときも、
//! 環に残っていれば続きを送り、無ければ `resync` を送って閉じる。

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

pub async fn serve_sse<S: AsyncWrite + Unpin>(app: &Arc<App>, sock: &mut S, h: &RequestHead, peer: &str) {
    let now = crate::util::now_ms();
    let rid = crate::util::request_id(h.headers.get_str("x-request-id"));
    let principal = crate::api::authenticate(app, h.headers.get_str("authorization"), crate::ratelimit::peer_ip(peer), &rid, now);
    let deny_p = |p: Problem| {
        let mut p = p;
        p.request_id = Some(rid.clone());
        let status = p.status;
        let body = serde_json::to_vec(&p).unwrap_or_default();
        let extra = match status {
            401 => "WWW-Authenticate: Bearer\r\n".to_string(),
            429 => format!("Retry-After: {}\r\n", p.retry_after_secs()),
            _ => String::new(),
        };
        let mut v = format!(
            "HTTP/1.1 {status} {}\r\nContent-Type: application/problem+json\r\nX-Request-Id: {rid}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
            senca_http::response::reason_phrase(status),
            body.len()
        )
        .into_bytes();
        v.extend_from_slice(&body);
        v
    };
    let p = match principal {
        Ok(p) => p,
        Err(e) => {
            let _ = sock.write_all(&deny_p(e)).await;
            return;
        }
    };
    if !p.has(Scope::StreamRead) {
        let _ = sock.write_all(&deny_p(Problem::new(ProblemCode::Forbidden, "権限 stream:read が必要です"))).await;
        return;
    }
    if p.session.as_ref().is_some_and(|s| !s.pending.is_empty()) {
        let _ = sock.write_all(&deny_p(Problem::new(ProblemCode::Forbidden, "先にパスワードの変更・MFA の登録を済ませてください"))).await;
        return;
    }
    let last: u64 = h.headers.get_str("last-event-id").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    // クラスタ: 先に進んだノードで受けた位置なら、このノードの適用が追いつくまで少し待つ
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while last > app.control.lock().unwrap().state.applied_index && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // 購読と取りこぼしの読み出しを同じロックの中で（間に適用されたものを落とさない・二重に出さない）
    let (mut rx, current, backlog) = {
        let core = app.control.lock().unwrap();
        let backlog = if last > 0 && last <= core.state.applied_index { core.events.since(last) } else { Some(Vec::new()) };
        (core.applied_tx.subscribe(), core.state.applied_index, backlog)
    };
    // 部屋の出来事（合意ログに載らない。event_id を持たないので Last-Event-ID の再開の対象外）
    let mut room_rx = app.room_events.subscribe();
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
    if sock.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    // 最後に送った位置（これ以下は送らない）
    let mut sent = last.min(current);
    match backlog {
        // 残している範囲より古い（または知らない先の）位置: 再同期を求める（id は今の位置。続きはここから）
        None => {
            if write_resync(sock, current).await.is_err() {
                return;
            }
            sent = current;
        }
        Some(v) => {
            for a in v {
                if write_applied(sock, &p, &a).await.is_err() {
                    return;
                }
                sent = a.index;
            }
        }
    }
    if last > current {
        // 別のクラスタ・作り直したノードの位置: 再同期
        if write_resync(sock, current).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(a) => {
                    if a.index <= sent {
                        continue;
                    }
                    sent = a.index;
                    if write_applied(sock, &p, &a).await.is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // 通知の列からあふれた: 環に残っていればそこから続ける。無ければ再同期を求めて閉じる
                    let more = app.control.lock().unwrap().events.since(sent);
                    match more {
                        Some(v) => {
                            for a in v {
                                if write_applied(sock, &p, &a).await.is_err() {
                                    return;
                                }
                                sent = a.index;
                            }
                        }
                        None => {
                            let cur = app.control.lock().unwrap().state.applied_index;
                            let _ = write_resync(sock, cur).await;
                            return;
                        }
                    }
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

/// 制御のイベントを 1 つ書く（権限・テナントで見せないものは書かない）。
async fn write_applied<S: AsyncWrite + Unpin>(sock: &mut S, p: &auth::Principal, a: &crate::control::Applied) -> std::io::Result<()> {
    let Some((ty, tenant, id, data)) = describe(&a.cmd) else { return Ok(()) };
    if tenant.as_deref().is_some_and(|t| t != p.tenant) || (tenant.is_none() && !p.has(Scope::NodeAdmin)) {
        return Ok(());
    }
    // 利用者の出来事は管理者だけに見せる
    if ty.starts_with("user.") && !p.has(Scope::TenantAdmin) {
        return Ok(());
    }
    let data = json!({ "type": ty, "resource_id": id, "occurred_at": crate::util::rfc3339(a.at_ms), "data": data }).to_string();
    let idx = a.index.to_string();
    let ev = SseEvent { event: Some(ty), id: Some(&idx), retry: None, data: &data };
    match ev.encode() {
        Ok(b) => sock.write_all(&b).await,
        Err(_) => Ok(()),
    }
}

/// 再同期を求める（`id` は今の位置: 状態を読み直した後、この位置から続きを受けられる）。
async fn write_resync<S: AsyncWrite + Unpin>(sock: &mut S, current: u64) -> std::io::Result<()> {
    let id = current.to_string();
    let ev = SseEvent { event: Some("resync"), id: Some(&id), retry: None, data: "{}" };
    match ev.encode() {
        Ok(b) => sock.write_all(&b).await,
        Err(_) => Ok(()),
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
        // 利用者（ADR-0017）。セッションの出来事（ログイン・最後に使った時刻）は流さない
        Command::CreateUser { user } => ("user.created", Some(user.tenant.clone()), user.id.clone(), json!({ "username": user.username })),
        Command::UpdateUser { tenant, id, profile, .. } => {
            ("user.updated", Some(tenant.clone()), id.clone(), json!({ "change": "profile", "disabled": profile.disabled }))
        }
        Command::SetUserPassword { tenant, id, .. } => ("user.updated", Some(tenant.clone()), id.clone(), json!({ "change": "password" })),
        Command::SetUserMfa { tenant, id, mfa, .. } => {
            ("user.updated", Some(tenant.clone()), id.clone(), json!({ "change": "mfa", "mfa_enabled": mfa.is_some() }))
        }
        Command::LinkUserOidc { tenant, id, link, .. } => {
            ("user.updated", Some(tenant.clone()), id.clone(), json!({ "change": "oidc_link", "provider": link.provider }))
        }
        Command::DeleteUser { tenant, id } => ("user.deleted", Some(tenant.clone()), id.clone(), json!({})),
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
