# senca-client（Rust の管理 API クライアント）

[English](README.en.md) | 日本語

SENCA の管理 API（`/api/v1`、[`docs/api/openapi.yaml`](../../docs/api/openapi.yaml)）を tokio から使う非同期のクライアント。
HTTP/1.1 は `senca-http`、TLS は `senca-tls` を使い、外部の HTTP クライアントに依らない（依存は tokio・serde・serde_json・bytes・getrandom・`futures-core`）。

```rust
use senca_client::{EventsOptions, ListOptions, ProblemCode, RequestOptions, SencaClient, ServerEvent};
use senca_client::models::{StreamCreate, StreamPatch};

let client = SencaClient::new("https://senca.example", std::env::var("SENCA_TOKEN")?)?;

// 作成（POST には Idempotency-Key を自動で付ける。指定もできる）
let s = client.streams().create(&StreamCreate { name: "studio-a".into(), ..Default::default() }).await?;
let key = s.ingest.stream_key.clone(); // stream key はこの応答でだけ渡る

// 更新は If-Match が必須。前の応答（ETag）をそのまま渡せる
let patch = StreamPatch { name: Some("studio-b".into()), ..Default::default() };
let s = match client.streams().update(&s.id, &patch, &s).await {
    Ok(v) => v,
    Err(e) if e.code() == Some(ProblemCode::PreconditionFailed) => { /* 読み直して再試行 */ todo!() }
    Err(e) => return Err(e.into()),
};

// 一覧は cursor を辿る列（futures::Stream でもある）
let mut it = client.streams().list_with(ListOptions::default().limit(100));
while let Some(x) = it.next().await {
    println!("{}", x?.name);
}

// SSE（切れたら Last-Event-ID を付けてつなぎ直す）
let mut events = client.events(EventsOptions::default().types(["stream.*", "recording.*"]));
while let Some(ev) = events.next().await {
    match ev? {
        ServerEvent::Event(e) => println!("{} {} {}", e.event, e.data.resource_id, e.data.occurred_at),
        ServerEvent::Resync { .. } => { /* 取りこぼしの可能性: 一覧を読み直す */ }
    }
}

client.streams().delete(&s.id, s.if_match()).await?;
```

## 操作

資源ごとに `client.<資源>()` から（各メソッドと openapi.yaml の operationId の対応は `src/ops.rs` の `OPERATIONS`。
試験が仕様と突き合わせ、仕様に操作が増えたのに SDK に無いと失敗する）。

| 資源 | メソッド |
|---|---|
| `streams()` | `create`・`create_with`・`list`・`list_with`・`list_page`・`get`・`update`・`delete`・`rotate_key`・`playback_url`・`start_recording`・`plan`・`snapshot`・`snapshots`・`snapshot_by_id`・`delete_snapshot` |
| `recordings()` | `list`・`list_with`（`ListOptions::stream` で絞る）・`list_page`・`get`・`update`・`delete`・`stop`・`playback_url`・`thumbnail`・`export_mp4` |
| `schedules()` | `create`・`list`・`check`・`get`・`update`・`delete`・`runs` |
| `tokens()` | `create`・`list`・`get`・`delete` |
| `inputs()`・`outputs()` | `create(stream_id, ..)`・`list(stream_id)`・`get`・`update`・`delete` |
| `secrets()` | `create`・`list`・`delete` |
| `pipelines()` | `create`・`list`・`get`・`update`・`delete`・`validate`・`control` |
| `nodes()` | `list`・`get`・`drain`・`undrain`・`disable_device`・`enable_device` |
| クライアント | `capabilities`・`grid`・`devices`・`events`・`healthz`・`readyz`・`fetch_signed`（署名付きの再生パス、307 を辿る）・`request_json`（型の無い要求） |
| `rooms()` | `create`・`list`・`list_with`・`list_page`・`get`・`update`・`delete`・`issue_token`・`send_message`・`stats`・`participants`・`participants_with`・`participant`・`update_participant`・`disconnect` |
| `media_pipelines()` | `create`・`list`・`list_with`（`ListOptions::room` で絞る）・`list_page`・`get`・`update`・`set_state`・`delete` |


## 決まり

- **版**: `get`・`create`・`update` は `Versioned<T>`（`value`・`etag`・`replayed`・`request_id`。`Deref` で中身）。
  `update(id, patch, if_match)` の `if_match` は `&Versioned<T>`・`IfMatch::Etag`・`IfMatch::Generation(n)`・`u64` のどれでも。
- **Idempotency**: POST は毎回新しい鍵（`sdk-<128 bit>`）を付け、送り直しでも同じ鍵を使う。`RequestOptions::idempotency_key("..")` で
  鍵を決めれば、プロセスをまたいだ再送でも同じ答えになる（`Versioned::replayed` が真）。
- **誤り**: `Error::Api(ApiError)` は problem+json の `code`（`ProblemCode`、知らない値は `Unknown` と `raw_code`）・`title`・
  `status`・`retryable`・`request_id`・`details`・`retry_after`。他は `Connect`・`Transport`・`Timeout`・`Decode`・`InvalidUrl`・`InvalidArgument`。
- **送り直し**（`RetryPolicy`、既定 3 回、200 ms から倍々・上限 10 秒・±20 % のゆらぎ、`Retry-After` 以上待つ）:
  接続できなかった要求、冪等な要求（GET・DELETE・Idempotency-Key 付きの POST）の時間切れ・送受信の誤り、
  `retryable: true` か 429・502・503・504 の応答。PATCH は「効いていない」とサーバーが言ったもの（`retryable`・429・503）だけ。
- **モデル**: `src/models.rs` は openapi.yaml からの生成物（`python3 crates/senca-client/gen/gen_models.py`）。応答の未知のフィールドは
  読み飛ばし、知らない enum の値は `Unknown` になる（サーバーが先に新しくなっても壊れない）。`*Patch` の null になりうる項目は
  `Option<Option<T>>`（`None` = 変えない、`Some(None)` = null で消す）。
- **TLS**: 既定は OS の認証局。`SencaClient::builder(url).root_certificates_pem(pem)` で差し替えられる。

## 試験

```
CARGO_TARGET_DIR=... cargo test -p senca-client
```

- 単体（URL・problem・送り直しの間隔・SSE の解析・Patch の 3 状態・operationId の対応表）
- `tests/mock.rs`: 台本どおりに答えるサーバーで、503 の送り直し（Retry-After）、POST の同じ鍵での送り直し、送り直さない誤り、
  上限での諦め、cursor の頁、SSE のつなぎ直し（Last-Event-ID）と resync
- `tests/node.rs`: 本物の senca-node（`SENCA_NODE_BIN`、無ければ同じ target の `debug/senca-node`、無ければ作る）を起動して、
  作成・Idempotency の再送（同じ ID と `Idempotency-Replayed`、別の本文は 409）・SSE の `stream.created`・`limit=1` の頁の辿り・
  取得・If-Match の更新・古い ETag の 412・If-Match 無しの 428・未知のフィールドの 400・削除（古い版の 412、その後 404）・
  401・トークンの権限（403）・秘密値・ノード・グリッド、部屋（作成・`limit=1` の一覧・取得・If-Match の更新と 412・参加トークン・
  参加者の一覧（空）・統計・削除の後の 404。ノードは `--webrtc-udp` 付きで起動する）
