# senca-client（Rust の管理 API クライアント）

[English](README.en.md) | 日本語

SENCA の管理 API（`/api/v1`、[`docs/api/openapi.yaml`](../../docs/api/openapi.yaml)）を tokio から使うための非同期クライアントです。
HTTP/1.1 には `senca-http`、TLS には `senca-tls` を使い、外部の HTTP クライアントには依存しません（外部の依存は tokio・serde・
serde_json・bytes・getrandom・`futures-core` です）。

```rust
use senca_client::{EventsOptions, ListOptions, ProblemCode, RequestOptions, SencaClient, ServerEvent};
use senca_client::models::{StreamCreate, StreamPatch};

let client = SencaClient::new("https://senca.example", std::env::var("SENCA_TOKEN")?)?;

// 作成（POST には Idempotency-Key が自動で付く。自分で指定することもできる）
let s = client.streams().create(&StreamCreate { name: "studio-a".into(), ..Default::default() }).await?;
let key = s.ingest.stream_key.clone(); // ストリームキーはこの応答でのみ返される

// 更新には If-Match が必須。直前の応答（ETag）をそのまま渡せる
let patch = StreamPatch { name: Some("studio-b".into()), ..Default::default() };
let s = match client.streams().update(&s.id, &patch, &s).await {
    Ok(v) => v,
    Err(e) if e.code() == Some(ProblemCode::PreconditionFailed) => { /* 読み直して再試行する */ todo!() }
    Err(e) => return Err(e.into()),
};

// 一覧はカーソルを自動でたどって順に返す（futures::Stream としても使える）
let mut it = client.streams().list_with(ListOptions::default().limit(100));
while let Some(x) = it.next().await {
    println!("{}", x?.name);
}

// SSE（切断されたら Last-Event-ID を付けて再接続する）
let mut events = client.events(EventsOptions::default().types(["stream.*", "recording.*"]));
while let Some(ev) = events.next().await {
    match ev? {
        ServerEvent::Event(e) => println!("{} {} {}", e.event, e.data.resource_id, e.data.occurred_at),
        ServerEvent::Resync { .. } => { /* イベントを取りこぼした可能性がある: 一覧を読み直す */ }
    }
}

client.streams().delete(&s.id, s.if_match()).await?;
```

## 操作

各リソースには `client.<リソース>()` からアクセスします。各メソッドと openapi.yaml の operationId の対応は、`src/ops.rs` の `OPERATIONS` に
定義されています。この対応表はテストで API 仕様と照合しており、仕様に追加された操作が SDK にない場合はテストが失敗します。

| リソース | メソッド |
|---|---|
| `streams()` | `create`・`create_with`・`list`・`list_with`・`list_page`・`get`・`update`・`delete`・`rotate_key`・`playback_url`・`start_recording`・`plan`・`snapshot`・`snapshots`・`snapshot_by_id`・`delete_snapshot` |
| `recordings()` | `list`・`list_with`（`ListOptions::stream` で絞り込み）・`list_page`・`get`・`update`・`delete`・`stop`・`playback_url`・`thumbnail`・`export_mp4` |
| `schedules()` | `create`・`list`・`check`・`get`・`update`・`delete`・`runs` |
| `tokens()` | `create`・`list`・`get`・`delete` |
| `inputs()`・`outputs()` | `create(stream_id, ..)`・`list(stream_id)`・`get`・`update`・`delete` |
| `secrets()` | `create`・`list`・`delete` |
| `pipelines()` | `create`・`list`・`get`・`update`・`delete`・`validate`・`control` |
| `nodes()` | `list`・`get`・`drain`・`undrain`・`disable_device`・`enable_device` |
| クライアント | `capabilities`・`grid`・`devices`・`events`・`healthz`・`readyz`・`fetch_signed`（署名付きの再生パスを取得。307 のリダイレクトをたどる）・`request_json`（型のないリクエスト） |
| `rooms()` | `create`・`list`・`list_with`・`list_page`・`get`・`update`・`delete`・`issue_token`・`send_message`・`stats`・`participants`・`participants_with`・`participant`・`update_participant`・`disconnect` |
| `media_pipelines()` | `create`・`list`・`list_with`（`ListOptions::room` で絞り込み）・`list_page`・`get`・`update`・`set_state`・`delete` |
| `audit()` | `list(&AuditQuery)`・`list_with`・`list_page`（監査ログ。新しい順、`AuditQuery` で時刻・操作・主体・資源・ノードを絞り込み） |

## 規約

- **バージョン**: `get`・`create`・`update` は `Versioned<T>`（`value`・`etag`・`replayed`・`request_id`。`Deref` で中身を参照）を返します。
  `update(id, patch, if_match)` の `if_match` には、`&Versioned<T>`・`IfMatch::Etag`・`IfMatch::Generation(n)`・`u64` のいずれも指定できます。
- **Idempotency**: POST には毎回新しいキー（`sdk-<128 bit>`）を付け、再送時も同じキーを使います。`RequestOptions::idempotency_key("..")` で
  キーを指定すると、別のプロセスから再送しても同じ応答が返ります（`Versioned::replayed` が true になります）。
- **エラー**: `Error::Api(ApiError)` は、problem+json の `code`（`ProblemCode`。未知の値は `Unknown` になり、元の値は `raw_code` に入ります）・
  `title`・`status`・`retryable`・`request_id`・`details`・`retry_after` を持ちます。それ以外のエラーは `Connect`・`Transport`・`Timeout`・
  `Decode`・`InvalidUrl`・`InvalidArgument` です。
- **再送**（`RetryPolicy`。既定は最大 3 回の再送で、間隔は 200 ms から倍々に増えて上限 10 秒、±20 % のジッターあり。`Retry-After` より短くはしません）:
  再送の対象は、接続できなかったリクエスト、冪等なリクエスト（GET・DELETE・Idempotency-Key 付きの POST）のタイムアウトと送受信エラー、
  `retryable: true` または 429・502・503・504 の応答です。PATCH は、サーバーが「処理されていない」と示した応答（`retryable`・429・503）の
  ときだけ再送します。
- **モデル**: `src/models.rs` は openapi.yaml から生成したものです（`python3 crates/senca-client/gen/gen_models.py`）。応答に含まれる未知の
  フィールドは無視し、未知の enum の値は `Unknown` になります（サーバーが先に更新されても動作します）。`*Patch` の null を取りうる項目は
  `Option<Option<T>>` です（`None` = 変更しない、`Some(None)` = null にして消去する）。
- **TLS**: 既定では OS の認証局を使います。`SencaClient::builder(url).root_certificates_pem(pem)` で差し替えられます。

## テスト

```
CARGO_TARGET_DIR=... cargo test -p senca-client
```

単体テスト・モックサーバーでのテスト（`tests/mock.rs`）・実際の senca-node に対するテスト（`tests/node.rs`）があります。
それぞれが確認する内容は [docs/dev/notes/senca-client-tests.md](../../docs/dev/notes/senca-client-tests.md) にあります。
