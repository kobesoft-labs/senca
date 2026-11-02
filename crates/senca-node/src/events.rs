//! `GET /api/v1/events`（SSE、仕様 10.5）。適用されたコマンドをイベントにして、権限で絞って流す。
//! event_id は制御ログの位置（applied_index）。`Last-Event-ID` より後のものだけを流す。
//! 保持は直近のメモリ分（broadcast の長さ）で、取りこぼしたら `resync` を送って閉じる。

use crate::app::App;
use senca_control::auth::{self, Scope};
use senca_control::state::Command;
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
    let Ok(p) = principal else {
        let _ = sock
            .write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return;
    };
    if !p.has(Scope::StreamRead) {
        let _ = sock.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        return;
    }
    let last: u64 = h.headers.get_str("last-event-id").and_then(|v| v.parse().ok()).unwrap_or(0);
    let (mut rx, current) = {
        let core = app.control.lock().unwrap();
        (core.applied_tx.subscribe(), core.state.applied_index)
    };
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
            _ = tokio::time::sleep(Duration::from_secs(15)) => {
                if sock.write_all(&senca_http::sse::keepalive()).await.is_err() { return }
            }
        }
    }
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
        _ => return None,
    })
}
