//! 本物の senca-node を起動して API を叩く試験。
//!
//! ノードの実行ファイルは `SENCA_NODE_BIN`、無ければこの試験と同じ target の `debug/senca-node`。
//! どちらも無ければ `cargo build -p senca-node` を試し、作れなければ理由を出して飛ばす。

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use senca_client::models::{SecretCreate, StreamCreate, StreamPatch, TokenCreate};
use senca_client::{EventsOptions, IfMatch, ListOptions, ProblemCode, RequestOptions, ServerEvent, SencaClient};

struct Node {
    child: Child,
    dir: PathBuf,
    url: String,
    token: String,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map(|a| a.port()).unwrap_or(0)
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").and_then(|s| s.local_addr()).map(|a| a.port()).unwrap_or(0)
}

/// この試験の target の `debug/`（`target/debug/deps/node-xxxx` の 2 つ上）。
fn target_debug() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent()?.parent().map(Path::to_path_buf)
}

fn node_bin() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("SENCA_NODE_BIN") {
        return Ok(p.into());
    }
    let dir = target_debug().ok_or("target が分からない")?;
    let bin = dir.join("senca-node");
    if bin.exists() {
        return Ok(bin);
    }
    // 作ってみる（同じ target）
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let target = dir.parent().ok_or("target が分からない")?;
    let st = Command::new(cargo)
        .args(["build", "--offline", "-q", "-p", "senca-node"])
        .env("CARGO_TARGET_DIR", target)
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .status()
        .map_err(|e| format!("cargo: {e}"))?;
    if st.success() && bin.exists() { Ok(bin) } else { Err("senca-node を作れない".into()) }
}

async fn start_node() -> Option<Node> {
    let bin = match node_bin() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("SKIP: {e}");
            return None;
        }
    };
    let dir = std::env::temp_dir().join(format!("senca-client-test-{}-{}", std::process::id(), free_port()));
    std::fs::create_dir_all(&dir).ok()?;
    let (http, rtmp, udp) = (free_port(), free_port(), free_udp_port());
    let log = std::fs::File::create(dir.join("node.log")).ok()?;
    let child = Command::new(&bin)
        .arg("--data-dir")
        .arg(dir.join("data"))
        .args(["--http", &format!("127.0.0.1:{http}"), "--rtmp", &format!("127.0.0.1:{rtmp}")])
        .args(["--webrtc-udp", &format!("127.0.0.1:{udp}")])
        .args(["--public-rtmp-url", &format!("rtmp://127.0.0.1:{rtmp}"), "--public-http-url", &format!("http://127.0.0.1:{http}")])
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: {}: {e}", bin.display());
            return None;
        }
    };
    let url = format!("http://127.0.0.1:{http}");
    let mut node = Node { child, dir: dir.clone(), url: url.clone(), token: String::new() };
    let probe = SencaClient::new(&url, "x").ok()?;
    for _ in 0..200 {
        if probe.readyz().await.is_ok()
            && let Ok(t) = std::fs::read_to_string(dir.join("data/control/bootstrap-admin-token"))
        {
            node.token = t.trim().to_string();
            return Some(node);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eprintln!("SKIP: node did not start: {}", std::fs::read_to_string(dir.join("node.log")).unwrap_or_default());
    None
}

fn token_req(role: senca_client::models::RoomTokenRequestRole, client_id: Option<&str>) -> senca_client::models::RoomTokenRequest {
    senca_client::models::RoomTokenRequest {
        role,
        client_id: client_id.map(str::to_string),
        bundle_id: None,
        metadata: None,
        permissions: None,
        ttl_secs: None,
    }
}

fn stream(name: &str) -> StreamCreate {
    StreamCreate { name: name.into(), ..Default::default() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn against_real_node() {
    let Some(node) = start_node().await else { return };
    let client = SencaClient::new(&node.url, node.token.clone()).unwrap();
    client.healthz().await.unwrap();
    let caps = client.capabilities().await.unwrap();
    assert!(!caps.node.is_empty());

    // 401 は型付きの誤り
    let bad = SencaClient::new(&node.url, "senca_wrong").unwrap();
    let e = bad.streams().list().next().await.unwrap().unwrap_err();
    assert_eq!((e.status(), e.code()), (Some(401), Some(ProblemCode::AuthFailed)), "{e}");

    // SSE を先に開く
    let mut events = client.events(EventsOptions::default().types(["stream.*"]));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 作成（Idempotency-Key の再送は同じ答え）
    let a = client.streams().create_with(&stream("sdk-a"), RequestOptions::idempotency_key("sdk-test-key-1")).await.unwrap();
    assert!(a.etag.is_some() && !a.replayed);
    assert_eq!(a.generation(), Some(a.generation as u64));
    let again = client.streams().create_with(&stream("sdk-a"), RequestOptions::idempotency_key("sdk-test-key-1")).await.unwrap();
    assert_eq!(again.id, a.id);
    assert!(again.replayed, "Idempotency-Replayed");
    // 同じ鍵で違う本文は 409
    let e = client.streams().create_with(&stream("sdk-other"), RequestOptions::idempotency_key("sdk-test-key-1")).await.unwrap_err();
    assert_eq!(e.code(), Some(ProblemCode::IdempotencyConflict), "{e}");

    // SSE に stream.created が来る
    let got = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(ev) = events.next().await {
            if let Ok(ServerEvent::Event(ev)) = ev
                && ev.event == "stream.created"
                && ev.data.resource_id == a.id
            {
                return Some(ev);
            }
        }
        None
    })
    .await
    .expect("SSE timeout")
    .expect("SSE ended");
    assert!(got.id.is_some());

    // 一覧（1 件ずつの頁を辿る）
    let b = client.streams().create(&stream("sdk-b")).await.unwrap();
    let c = client.streams().create(&stream("sdk-c")).await.unwrap();
    let page = client.streams().list_page(&ListOptions::default().limit(1)).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(page.next_cursor.is_some());
    let all = client.streams().list_with(ListOptions::default().limit(1)).collect_all().await.unwrap();
    let ids: Vec<&str> = all.iter().map(|s| s.id.as_str()).collect();
    for x in [&a.id, &b.id, &c.id] {
        assert!(ids.contains(&x.as_str()), "{ids:?}");
    }
    assert_eq!(all.len(), 3);

    // 取得・更新（If-Match）
    let got = client.streams().get(&a.id).await.unwrap();
    let patch = StreamPatch { name: Some("sdk-a2".into()), ..Default::default() };
    let upd = client.streams().update(&a.id, &patch, &got).await.unwrap();
    assert_eq!(upd.name, "sdk-a2");
    assert!(upd.generation() > got.generation());
    // 古い ETag は 412
    let e = client.streams().update(&a.id, &patch, &got).await.unwrap_err();
    assert_eq!((e.status(), e.code()), (Some(412), Some(ProblemCode::PreconditionFailed)), "{e}");
    // If-Match 無しは 428（型の無い要求で）
    let e = client
        .request_json("PATCH", &format!("/streams/{}", a.id), Some(&serde_json::json!({"name": "x"})), None)
        .await
        .unwrap_err();
    assert_eq!((e.status(), e.code()), (Some(428), Some(ProblemCode::PreconditionRequired)), "{e}");
    // 未知のフィールドは 400
    let e = client
        .request_json("PATCH", &format!("/streams/{}", a.id), Some(&serde_json::json!({"bogus": 1})), Some(upd.if_match().unwrap()))
        .await
        .unwrap_err();
    assert_eq!(e.status(), Some(400), "{e}");
    assert!(e.api().unwrap().request_id.is_some());

    // 削除（古い版は 412、今の版で消える、その後 404）
    let e = client.streams().delete(&a.id, Some(IfMatch::Generation(1))).await.unwrap_err();
    assert_eq!(e.code(), Some(ProblemCode::PreconditionFailed));
    client.streams().delete(&a.id, upd.if_match()).await.unwrap();
    let e = client.streams().get(&a.id).await.unwrap_err();
    assert_eq!((e.status(), e.code()), (Some(404), Some(ProblemCode::NotFound)));
    client.streams().delete(&b.id, None).await.unwrap();
    client.streams().delete(&c.id, None).await.unwrap();
    let left = client.streams().list().collect_all().await.unwrap();
    assert!(left.is_empty(), "{left:?}");

    // 他の資源もひととおり
    let t = client
        .tokens()
        .create(&TokenCreate { name: "sdk".into(), scopes: vec![senca_client::models::Scope::StreamRead], expires_at: None })
        .await
        .unwrap();
    assert!(t.token.as_deref().is_some_and(|x| x.starts_with("senca_")));
    let reader = SencaClient::new(&node.url, t.token.clone().unwrap()).unwrap();
    assert!(reader.streams().list().collect_all().await.unwrap().is_empty());
    let e = reader.streams().create(&stream("nope")).await.unwrap_err();
    assert_eq!(e.code(), Some(ProblemCode::Forbidden));
    assert!(client.tokens().list().collect_all().await.unwrap().iter().any(|x| x.id == t.id));
    client.tokens().delete(&t.id).await.unwrap();
    let s = client.secrets().create(&SecretCreate { name: "sdk-secret".into(), value: "v".into() }).await.unwrap();
    assert_eq!(client.secrets().list().collect_all().await.unwrap().len(), 1);
    client.secrets().delete(&s.id).await.unwrap();
    let nodes = client.nodes().list().await.unwrap();
    assert!(nodes.iter().any(|n| n.is_self));
    let grid = client.grid().await.unwrap();
    assert!(!grid.nodes.is_empty());
    // 監査ログ: いま消したトークンの記録が新しい順に読める（limit=1 で cursor を辿る）
    let q = senca_client::AuditQuery { action: Some("token.".into()), resource: Some(t.id.clone()), ..Default::default() };
    let entries = client.audit().list_with(&q, ListOptions::default().limit(1)).collect_all().await.unwrap();
    assert_eq!(entries.iter().map(|e| e.action.as_str()).collect::<Vec<_>>(), ["token.delete", "token.create"]);
    assert!(entries.iter().all(|e| !e.node.is_empty() && e.tenant == "default"));
    let page = client.audit().list_page(&senca_client::AuditQuery::default(), &ListOptions::default().limit(2)).await.unwrap();
    assert_eq!(page.items.len(), 2);
    assert!(page.next_cursor.is_some() && page.unavailable_nodes.is_empty());
    // 消したトークンでは読めない
    let e = reader.audit().list(&Default::default()).collect_all().await.unwrap_err();
    assert_eq!(e.code(), Some(ProblemCode::AuthFailed));

    // 部屋
    use senca_client::models::{RoomCreate, RoomPatch, RoomSettings, RoomTokenRequestRole};
    let rooms = client.rooms();
    let r1 = rooms.create(&RoomCreate { id: Some("sdk-room-1".into()), ..Default::default() }).await.unwrap();
    assert_eq!(r1.id, "sdk-room-1");
    assert!(r1.etag.is_some());
    let _r2 = rooms.create(&RoomCreate::default()).await.unwrap();
    let page = rooms.list_page(&ListOptions::default().limit(1)).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(page.next_cursor.is_some());
    assert_eq!(rooms.list_with(ListOptions::default().limit(1)).collect_all().await.unwrap().len(), 2);
    let got = rooms.get("sdk-room-1").await.unwrap();
    let patch = RoomPatch {
        settings: Some(Some(RoomSettings { max_participants: Some(5), ..Default::default() })),
        ..Default::default()
    };
    let upd = rooms.update("sdk-room-1", &patch, &got).await.unwrap();
    assert_eq!(upd.settings.max_participants, Some(5));
    assert!(upd.generation() > got.generation());
    let e = rooms.update("sdk-room-1", &patch, &got).await.unwrap_err();
    assert_eq!((e.status(), e.code()), (Some(412), Some(ProblemCode::PreconditionFailed)), "{e}");
    let tok = rooms
        .issue_token("sdk-room-1", &token_req(RoomTokenRequestRole::Sendrecv, Some("alice")))
        .await
        .unwrap();
    assert!(tok.token.as_deref().is_some_and(|t| t.split('.').count() == 3), "{:?}", tok.token);
    assert_eq!(tok.room_id, "sdk-room-1");
    assert!(rooms.participants("sdk-room-1").collect_all().await.unwrap().is_empty());
    let st = rooms.stats("sdk-room-1").await.unwrap();
    assert!(st.participants.is_empty());
    rooms.delete("sdk-room-1").await.unwrap();
    let e = rooms.get("sdk-room-1").await.unwrap_err();
    assert_eq!((e.status(), e.code()), (Some(404), Some(ProblemCode::NotFound)), "{e}");
    drop(events);
}
