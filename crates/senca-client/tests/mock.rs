//! 台本どおりに答える小さな HTTP サーバーで、送り直し・頁・SSE のつなぎ直しを確かめる（ノード不要）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use senca_client::{EventsOptions, ListOptions, ProblemCode, RetryPolicy, SencaClient, ServerEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
struct Seen {
    head: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }
    fn line(&self) -> &str {
        self.head.lines().next().unwrap_or("")
    }
}

/// 要求ごとに次の応答（生のバイト）を返す。要求は記録する。
async fn serve(responses: Vec<Vec<u8>>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    tokio::spawn(async move {
        for r in responses {
            let Ok((mut sock, _)) = l.accept().await else { return };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = sock.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..p]).to_string();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                        .unwrap_or(0);
                    while buf.len() < p + 4 + len {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    s2.lock().unwrap().push(Seen { head });
                    break;
                }
            }
            let _ = sock.write_all(&r).await;
            let _ = sock.shutdown().await;
        }
    });
    (url, seen)
}

fn resp(status: &str, extra: &str, ctype: &str, body: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}", body.len()).into_bytes()
}

fn problem(status: u16, code: &str, retryable: bool) -> String {
    format!(r#"{{"type":"urn:senca:error:x","title":"t","status":{status},"code":"{code}","retryable":{retryable}}}"#)
}

fn fast() -> RetryPolicy {
    RetryPolicy { max_retries: 3, initial_delay: Duration::from_millis(10), max_delay: Duration::from_millis(50) }
}

fn client(url: &str) -> SencaClient {
    SencaClient::builder(url).token("senca_t").retry(fast()).build().unwrap()
}

#[tokio::test]
async fn retries_get_on_503_with_retry_after() {
    let (url, seen) = serve(vec![
        resp("503 Service Unavailable", "Retry-After: 0\r\n", "application/problem+json", &problem(503, "CAPACITY_UNAVAILABLE", true)),
        resp("200 OK", "ETag: \"3\"\r\n", "application/json", r#"{"id":"n1","name":"a","draining":false,"registered_at":"2026-01-01T00:00:00Z","labels":{},"live_streams":0,"self":true,"relay":null}"#),
    ])
    .await;
    let n = client(&url).nodes().get("n1").await.unwrap();
    assert!(n.is_self);
    let s = seen.lock().unwrap();
    assert_eq!(s.len(), 2);
    assert_eq!(s[0].header("authorization").as_deref(), Some("Bearer senca_t"));
    assert!(s[0].line().starts_with("GET /api/v1/nodes/n1 "));
}

#[tokio::test]
async fn post_is_retried_with_the_same_idempotency_key() {
    let (url, seen) = serve(vec![
        resp("500 Internal Server Error", "", "application/problem+json", &problem(500, "INTERNAL_ERROR", true)),
        resp("500 Internal Server Error", "", "application/problem+json", &problem(500, "INTERNAL_ERROR", true)),
        resp("202 Accepted", "", "application/json", r#"{"id":"n1","name":"a","draining":true,"registered_at":"2026-01-01T00:00:00Z","labels":{},"live_streams":0,"self":true,"relay":null}"#),
    ])
    .await;
    let n = client(&url).nodes().drain("n1").await.unwrap();
    assert!(n.draining);
    let s = seen.lock().unwrap();
    assert_eq!(s.len(), 3);
    let k: Vec<_> = s.iter().map(|x| x.header("idempotency-key").unwrap()).collect();
    assert!(k[0].starts_with("sdk-") && k.iter().all(|x| *x == k[0]), "{k:?}");
}

#[tokio::test]
async fn non_retryable_errors_are_returned_at_once() {
    let (url, seen) = serve(vec![
        resp("409 Conflict", "X-Request-Id: req_9\r\n", "application/problem+json", &problem(409, "CONFLICT", false)),
        resp("200 OK", "", "application/json", "{}"),
    ])
    .await;
    let e = client(&url).nodes().drain("n1").await.unwrap_err();
    assert_eq!(e.code(), Some(ProblemCode::Conflict));
    assert_eq!(e.api().unwrap().request_id.as_deref(), Some("req_9"));
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn gives_up_after_max_retries() {
    let r = resp("503 Service Unavailable", "", "text/plain", "busy");
    let (url, seen) = serve(vec![r.clone(), r.clone(), r.clone(), r.clone(), r]).await;
    let e = client(&url).grid().await.unwrap_err();
    assert_eq!(e.status(), Some(503));
    assert_eq!(seen.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn paginates_with_cursor() {
    let tok = |id: &str| format!(r#"{{"id":"{id}","name":"t","scopes":["stream:read"],"created_at":"2026-01-01T00:00:00Z","expires_at":null,"last_used_at":null,"token":null}}"#);
    let (url, seen) = serve(vec![
        resp("200 OK", "", "application/json", &format!(r#"{{"items":[{},{}],"next_cursor":"t2"}}"#, tok("t1"), tok("t2"))),
        resp("200 OK", "", "application/json", &format!(r#"{{"items":[{}],"next_cursor":null}}"#, tok("t3"))),
    ])
    .await;
    let all = client(&url).tokens().list_with(ListOptions::default().limit(2)).collect_all().await.unwrap();
    assert_eq!(all.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["t1", "t2", "t3"]);
    let s = seen.lock().unwrap();
    assert!(s[0].line().starts_with("GET /api/v1/tokens?limit=2 "), "{}", s[0].line());
    assert!(s[1].line().starts_with("GET /api/v1/tokens?limit=2&cursor=t2 "), "{}", s[1].line());
}

#[tokio::test]
async fn sse_reconnects_with_last_event_id() {
    let ev = r#"{"type":"stream.created","resource_id":"s1","occurred_at":"2026-01-01T00:00:00Z","data":{}}"#;
    let first = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n: hi\n\nretry: 20\nevent: recording.updated\nid: 4\ndata: {{}}\n\nevent: stream.created\nid: 5\ndata: {ev}\n\n"
    );
    let second = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\nevent: resync\nid: 9\ndata: {}\n\n".to_string();
    let (url, seen) = serve(vec![first.into_bytes(), second.into_bytes()]).await;
    let mut es = client(&url).events(EventsOptions::default().types(["stream.*"]));
    let a = tokio::time::timeout(Duration::from_secs(5), es.next()).await.unwrap().unwrap().unwrap();
    match a {
        ServerEvent::Event(e) => {
            assert_eq!((e.id.as_deref(), e.event.as_str(), e.data.resource_id.as_str()), (Some("5"), "stream.created", "s1"));
        }
        other => panic!("{other:?}"),
    }
    let b = tokio::time::timeout(Duration::from_secs(5), es.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(b, ServerEvent::Resync { id: Some("9".into()) });
    let s = seen.lock().unwrap();
    assert_eq!(s[0].header("last-event-id"), None);
    assert_eq!(s[1].header("last-event-id").as_deref(), Some("5"));
    assert_eq!(s[1].header("accept").as_deref(), Some("text/event-stream"));
}

/// ノードが実際に返したメディアパイプライン（`GET /media-pipelines/{id}`）。
const MEDIA_PIPELINE: &str = r#"{"artifacts":{"composite":{"outputs":["40ff38686cbc592caa9f58b8c18b571f"],"pipeline":"eea4ac0b434713af6c81c8b7ca8fa544","stream":"14228b3d447ba3c24e9d2fdf7db5d142"},"participant_streams":[{"stream":"s1","name":"mp1-alice-abcdef"}]},"captions":null,"composite":{"bitrate_kbps":2500,"fps":30,"height":720,"layout":"grid","video_codec":"av1","width":1280},"created_at":"2026-11-03T00:53:36.808Z","desired_state":"running","egress":[{"protocol":"rtmp_push","url":"rtmp://example.com/live/x"}],"export_participants":true,"generation":3,"id":"m1","name":"mp1","participants":[],"recording":{"composite":true,"participants":true},"room":"r1","status":{"composite_members":null,"participants":[{"participant":"alice","stream":"s1","video":true}]},"updated_at":"2026-11-03T00:53:36.808Z"}"#;

#[tokio::test]
async fn media_pipelines_list_by_room_and_set_state() {
    use senca_client::models::{MediaPipelineCompositeLayout, MediaPipelineState};
    let (url, seen) = serve(vec![
        resp("200 OK", "", "application/json", &format!(r#"{{"items":[{MEDIA_PIPELINE}],"next_cursor":null}}"#)),
        resp("200 OK", "ETag: \"4\"\r\n", "application/json", &MEDIA_PIPELINE.replace(r#""desired_state":"running""#, r#""desired_state":"stopped""#)),
        resp("204 No Content", "", "application/json", ""),
    ])
    .await;
    let c = client(&url);
    let all = c.media_pipelines().list_with(ListOptions::default().room("r1")).collect_all().await.unwrap();
    assert_eq!(all.len(), 1);
    let m = &all[0];
    let comp = m.composite.as_ref().unwrap();
    assert_eq!((comp.layout, comp.width, comp.height), (Some(MediaPipelineCompositeLayout::Grid), Some(1280), Some(720)));
    assert_eq!(m.artifacts.composite.as_ref().unwrap().outputs.len(), 1);
    assert_eq!(m.artifacts.participant_streams[0].name, "mp1-alice-abcdef");
    assert!(m.status.as_ref().unwrap().participants[0].video);
    let m = c.media_pipelines().set_state("m1", MediaPipelineState::Stopped, 3u64).await.unwrap();
    assert_eq!(m.desired_state, MediaPipelineState::Stopped);
    c.media_pipelines().delete("m1").await.unwrap();
    let s = seen.lock().unwrap();
    assert!(s[0].line().starts_with("GET /api/v1/media-pipelines?room=r1 "), "{}", s[0].line());
    assert!(s[1].line().starts_with("PATCH /api/v1/media-pipelines/m1 "), "{}", s[1].line());
    assert_eq!(s[1].header("if-match").as_deref(), Some("\"3\""));
    assert!(s[2].line().starts_with("DELETE /api/v1/media-pipelines/m1 "), "{}", s[2].line());
}

#[tokio::test]
async fn login_without_a_token_and_exchange_with_the_flow_cookie() {
    let (url, seen) = serve(vec![
        resp("200 OK", "", "application/json", r#"{"status":"mfa_required","mfa_token":"m1","expires_at":"2026-01-01T00:05:00Z","methods":["totp","recovery_code"]}"#),
        resp("401 Unauthorized", "WWW-Authenticate: Bearer\r\n", "application/problem+json", &problem(401, "AUTH_FAILED", false)),
    ])
    .await;
    let c = SencaClient::builder(&url).retry(fast()).build().unwrap();
    let body = senca_client::models::LoginRequest { username: "alice".into(), password: "pw".into(), ..Default::default() };
    match c.auth().login(&body).await.unwrap() {
        senca_client::LoginOutcome::MfaRequired(m) => assert_eq!(m.mfa_token, "m1"),
        other => panic!("{other:?}"),
    }
    let e = c.auth().exchange("code1", Some("senca_oidc=abc")).await.unwrap_err();
    assert_eq!(e.api().map(|a| a.status), Some(401));
    let s = seen.lock().unwrap();
    assert!(s[0].line().starts_with("POST /api/v1/auth/login "));
    assert_eq!(s[0].header("authorization"), None, "トークンなしのクライアントは Authorization を送らない");
    assert!(s[1].line().starts_with("POST /api/v1/auth/exchange "));
    assert_eq!(s[1].header("cookie").as_deref(), Some("senca_oidc=abc"));
    assert_eq!(c.auth().oidc_start_url("kc", Some("/console/#/x")), format!("{url}/api/v1/auth/oidc/kc/start?return_to=%2Fconsole%2F%23%2Fx"));
}
