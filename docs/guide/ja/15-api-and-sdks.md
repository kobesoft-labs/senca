# 15. 管理 API と SDK

[English](../en/15-api-and-sdks.md)

SENCA のノードは、HTTP の管理 API（`/api/v1`）で操作します。管理画面（`/console/`）も同じ API を使っています。
この章では、API の共通ルール、OpenAPI の定義、イベント（SSE）、JavaScript の SDK（`@senca/sdk`）、Rust のクライアント（`senca-client`）を説明します。

- API の定義: [`docs/api/openapi.yaml`](../../api/openapi.yaml)（ノードの実際の動作に合わせて記述しています）
- リソースごとの説明: 配信と再生は [4. 取り込みと再生](04-ingest-playback.md)、録画は [5. 録画](05-recording.md)、入力・出力は
  [6. 入力と出力](06-inputs-outputs.md)、パイプラインは [7. パイプライン](07-pipelines.md)、ノードとグリッドは
  [9. クラスターとグリッド](09-cluster-grid.md)、部屋は [10. リアルタイムの部屋](10-rooms.md)、メディアパイプラインは [11. メディアパイプライン](11-media-pipelines.md)

## 1. 共通のルール

### 1.1 基本

| 項目 | 値 |
|---|---|
| ベースパス | `/api/v1`（`--http` で指定したアドレスで待ち受けます。`--https` を指定すれば TLS でも同じパスです） |
| 本文 | JSON（UTF-8）。時刻は UTC の RFC 3339（`2026-10-25T00:00:00.000Z`） |
| 本文の上限 | 1 MiB（HTTP 層で検査します。超えると text/plain の 413 を返して接続を閉じます。problem+json ではありません） |
| 応答ヘッダー | すべての応答に `X-Request-Id` と `Cache-Control: no-store` が付きます |
| `X-Request-Id` | リクエストの `X-Request-Id` が 64 文字以内の英数字と `-` `_` `.` であればその値を使います。そうでなければ `req_...` を生成します |
| パス | 空の区切り（`//`）・`.`・`..` を含むパスは 404 です |
| メソッド | 既知のリソースに定義されていないメソッド（HEAD を含む）は 405（`METHOD_NOT_ALLOWED`）です。未知のパスは 404 です |
| CORS | `/api/v1` は CORS ヘッダーを返しません。ブラウザーからは、同じオリジン（管理画面など）か、アプリのサーバーを経由して呼び出してください |

### 1.2 認証

すべての `/api/v1`（`/capabilities` を含む）に `Authorization: Bearer <値>` ヘッダーが必要です。`/healthz`・`/readyz`・署名付きの再生パス
（`/s/...`）には不要です。

- 値が **`senca_` で始まれば API トークン**、それ以外は **JWT** として扱います。値は 8192 バイトまでです。
- 認証に失敗すると 401（`AUTH_FAILED`、`WWW-Authenticate: Bearer`）を返します。失敗の理由（期限切れ・未知のトークンなど）は応答に含めず、ノードのログに記録します。

**API トークン**

- 最初のノードの起動時にトークンが 1 つも無ければ、`tenant:admin` と `node:admin` を持つトークン（名前 `bootstrap-admin`、
  テナント `default`、期限なし）を作成し、値を `{data-dir}/control/bootstrap-admin-token`（パーミッション 0600）に書き出します。クラスターでは
  ID が最も小さいノードが作成します。
- それ以降のトークンは `POST /api/v1/tokens`（`tenant:admin`）で作成します。値（`token`）が返るのは作成時の応答だけです。サーバーにはハッシュだけを保存します。

```
TOKEN=$(cat data/control/bootstrap-admin-token)
curl -s -X POST http://127.0.0.1:8080/api/v1/tokens \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"ci","scopes":["stream:read","stream:write"],"expires_at":"2027-01-01T00:00:00Z"}'
```

| フィールド | ルール |
|---|---|
| `name` | 1〜64 文字（違反すると 400 `INVALID_CONFIG`） |
| `scopes` | 1 つ以上。未知の権限は 400 です。**自分が持っていない権限は付与できません**（403 `FORBIDDEN`） |
| `expires_at` | 任意。RFC 3339。過去の時刻は 400 です。期限を過ぎたトークンでの認証は 401 になります |

- 一覧と取得ではトークンの値を返しません。`DELETE /tokens/{id}` で失効させます。テナントで**最後の `tenant:admin` トークンは削除できません**（409 `CONFLICT`）。
- トークンの扱い（保管・入れ替え）は [12. セキュリティ](12-security.md) を見てください。

**JWT**（外部の認証基盤のトークンをそのまま使う場合）

```
senca-node --jwks /etc/senca/jwks.json --jwt-issuer https://idp.example/ --jwt-audience senca [--jwt-tenant-claim tenant]
```

- 検証鍵は**ローカルの JWKS ファイルからだけ**読み込みます（トークンの内容をもとに URL へ鍵を取りに行くことはありません）。`--jwks` には `--jwt-issuer` と `--jwt-audience` が必須です。
- アルゴリズム: EdDSA（Ed25519）・ES256・RS256（RSA は 2048 ビット以上）。アルゴリズムは鍵で決まり、ヘッダーの `alg` が鍵と合わなければ拒否します。
  JWKS の鍵には `kid` が必須で、`use` があれば `sig` である必要があります。`alg=none`・HS 系・`crit` 付きのトークンは拒否します。
- 検査する内容: `kid` が JWKS にあること、`exp`（必須）、`nbf`、`iss` の一致、`aud` の一致（文字列または配列）。時計のずれは 60 秒まで許容します。
- テナントはクレーム `tenant`（`--jwt-tenant-claim` で名前を変更できます）で決まります。無い、または空なら拒否します。主体（subject）は `sub` です。
- 権限は `scope`（空白区切り）または `scp`（文字列または配列）から読みます。未知の権限は無視します。

### 1.3 権限とテナント

| 権限 | 主な操作 |
|---|---|
| `stream:read` | ストリーム・予約・入力・出力・パイプライン・部屋・メディアパイプラインの読み取り、`POST /streams/{id}/plan`、`POST /schedules/check`、`POST /pipelines/{id}/validate`、**SSE** |
| `stream:write` | 上のリソースの作成・更新・削除、鍵の再発行、録画の開始・停止・`PATCH /recordings/{id}`、部屋の参加トークンの発行・参加者の操作・メッセージ送信 |
| `subscribe` | `POST /streams/{id}/playback-url`（ライブの署名付き URL） |
| `recording:read` | 録画の一覧・取得・VOD の URL・サムネイル・MP4 の書き出し |
| `recording:delete` | `DELETE /recordings/{id}` |
| `tenant:admin` | トークン・秘密値（`/secrets`）の管理。**`node:admin` 以外のすべての権限を含みます** |
| `node:admin` | テナントをまたぐ運用操作: `/nodes`・`/devices`・`/grid`・ドレイン・デバイスの停止と再開、SSE のテナントに属さないイベント |
| `publish` | WHIP での配信（`POST /whip/{stream_id}` で stream key の代わりに使えます。同じテナントの stream だけ） |

- 操作ごとに必要な権限は、OpenAPI の拡張フィールド `x-senca-scope` に記載しています（`null` は認証だけが必要という意味で、`GET /capabilities` が該当します）。
  権限が足りなければ 403（`FORBIDDEN`。title に必要な権限の名前が入ります）です。
- **テナントは認証情報から決まります**（API トークンは作成者のテナント、JWT はテナントのクレーム）。本文でテナントを指定するフィールドはありません。
  他のテナントのリソースは 404 に見えます。新しいテナントは JWT のテナントのクレームで作ります（API トークンは自分のテナントにしか作れません）。

### 1.4 本文の検査

- 本文のあるリクエストは、`Content-Type: application/json`（`; charset=...` は可）でなければ 415（`UNSUPPORTED_MEDIA_TYPE`）です。
- **未知のフィールドは 400** です（`INVALID_REQUEST`。`details.reason` に解析エラーの内容が入ります）。型の誤りも同じです。黙って無視することはしません。
- 値の検査（範囲・組み合わせ・参照先）のエラーは、多くが 400 `INVALID_CONFIG` です。

### 1.5 Idempotency-Key

POST に `Idempotency-Key` ヘッダーを付けると、24 時間は同じリクエストに同じ応答を返します。作成のリクエストを再送したときに、リソースが二重に作られるのを防ぐために使います。

- キーは 1〜255 文字の表示可能な ASCII（空白なし）です。違反すると 400 です。キーの有効範囲はテナントごとです。
- 同じリクエストかどうかは、メソッド・パス・クエリ・本文の SHA-256 で判定します（ヘッダーは見ません）。**同じキーで別のリクエスト**を送るか、別の主体が
  同じキーを使うと 409（`IDEMPOTENCY_CONFLICT`）です。
- 再送への応答は、保存した状態コード・本文・`Location`・`ETag` に **`Idempotency-Replayed: true`** を付けたものです。
- **4xx の応答も保存します**（同じキーで再送すると同じエラーが返ります）。**5xx は保存しません**（再送すると処理をやり直します）。256 KiB を超える応答は保存しません。
- 保存する本文からは秘密情報を消します。`stream_key`・`token`・`secret` を `null` にし、代わりに `stream_key_redacted: true` などを入れます。
  つまり、**秘密情報は最初の応答でしか見られません**。再送で得た応答から値を取り出すことはできません。
- 同じキーのリクエストは、ノード内で順番に処理します（同時に再送しても、処理は 1 回だけ実行されます）。

### 1.6 ETag と If-Match（楽観的排他制御）

- リソースの応答（作成・取得・更新）の `ETag` は `"{generation}"`（例 `"3"`）です。`W/"3"` の形式も受け付けます。
- **PATCH には `If-Match` が必須です**。無いと 428（`PRECONDITION_REQUIRED`）、形式が違うか古いと 412（`PRECONDITION_FAILED`）です。
- DELETE の `If-Match` は任意で、検査するのは `/streams`・`/recordings`・`/schedules` だけです（付けた場合、古ければ 412）。入力・出力・秘密値・
  パイプライン・部屋・メディアパイプライン・トークンの DELETE では `If-Match` を検査しません。

```
# 取得してから If-Match を付けて更新する
ETAG=$(curl -s -D - -o /dev/null -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/streams/$ID | awk -F': ' 'tolower($1)=="etag"{print $2}' | tr -d '\r')
curl -s -X PATCH http://127.0.0.1:8080/api/v1/streams/$ID \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -H "If-Match: $ETAG" \
  -d '{"metadata":{"owner":"studio-a"}}'
```

412 が返ったら、リソースを取得し直してから変更をやり直してください。

### 1.7 一覧とページング

- 一覧の応答は `{"items": [...], "next_cursor": "..." | null}` です。並び順は ID の昇順です。
- クエリは `limit`（1〜1000、既定 100）と `cursor`（前の応答の `next_cursor`）です。範囲外の `limit` は 400 です。
- 一覧で**未知のクエリパラメーターは 400** です。使える絞り込みは次のものだけです: `GET /recordings` の `stream`・`state`、`GET /schedules` の
  `stream`・`state`、`GET /media-pipelines` の `room`。
- `next_cursor` が `null` なら最後のページです。ページングの無い一覧もあります（`GET /secrets` は全件を返し `next_cursor: null`、`GET /nodes` は `items` だけ）。

```
curl -s -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:8080/api/v1/streams?limit=50'
curl -s -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:8080/api/v1/streams?limit=50&cursor=<next_cursor>'
```

### 1.8 エラー（problem+json）

エラーは `Content-Type: application/problem+json`（RFC 9457 の形式）で返します。

```json
{
  "type": "urn:senca:error:precondition-failed",
  "title": "資源が更新されています（If-Match が古い）",
  "status": 412,
  "code": "PRECONDITION_FAILED",
  "request_id": "req_...",
  "retryable": false
}
```

- `title` は日本語の説明です。プログラムでは `code` で判別してください。`details` はコードごとの補足情報です（無ければ省略します）。
- `retryable` が true なら、同じリクエストを再送してかまいません。503・429・504 で `retryable` が true のときは `Retry-After` が付きます
  （429 では次に通るまでの秒数、それ以外は 1）。
- 量の上限（429）は、どの `/api/v1` の操作でも返ることがあります（[12. セキュリティ](12-security.md)）。
- `/api/v1` 以外の版（`/api/v2/...` など）は 422 `UNSUPPORTED_PROTOCOL_VERSION`（`details.supported: ["v1"]`）です。
- 問い合わせの際は `request_id`（= `X-Request-Id`）を添えてください。

| `code` | ステータス | `retryable` | 主な場面 |
|---|---|---|---|
| `INVALID_CONFIG` | 400 | false | 値の検査エラー（範囲・組み合わせ・参照先） |
| `INVALID_REQUEST` | 400 | false | 本文の解析エラー・未知のフィールド・未知のクエリ・`limit` の範囲外・Idempotency-Key の形式 |
| `UNSUPPORTED_PROTOCOL_VERSION` | 422 | false | `/api/v1` 以外の版のパス |
| `UNSUPPORTED_CODEC` | 422 | false | 静止画のデコード（`GET /streams/{id}/snapshot`）、部屋の処理からの 422、E2EE の部屋と、そこから書き出した stream でのサーバーの処理（`details.reason: "e2ee"`） |
| `UNSUPPORTED_MEDIA_TYPE` | 415 | false | `Content-Type` が `application/json` でない |
| `AUTH_FAILED` | 401 | false | 認証の失敗 |
| `FORBIDDEN` | 403 | false | 権限が足りない、持っていない権限をトークンに付与しようとした |
| `NOT_FOUND` | 404 | false | 存在しない、他のテナントのリソース、未知のパス |
| `METHOD_NOT_ALLOWED` | 405 | false | リソースに無いメソッド |
| `CONFLICT` | 409 | false | 現在の状態と矛盾する（使用中の秘密値の削除、最後の管理者トークンの削除など） |
| `IDEMPOTENCY_CONFLICT` | 409 | false | 同じ Idempotency-Key で別のリクエスト |
| `PRECONDITION_FAILED` | 412 | false | `If-Match` が古い、または形式が違う |
| `PRECONDITION_REQUIRED` | 428 | false | PATCH に `If-Match` が無い |
| `QUOTA_EXCEEDED` | 403 | false | 上限に達した（同じストリームで同時に録画できるのは 8 本まで、など） |
| `RATE_LIMITED` | 429 | true | 要求の量の上限を超えた（`details.limit` は `ip`・`token`・`auth_failures`・`connect`） |
| `CAPACITY_UNAVAILABLE` | 503 | 多くは true | クラスターが変更の提案を受け付けられない（リーダーの交代・混雑）、部屋が別のノードにある、`--webrtc-udp` が無い、codec worker が無い（このときは false） |
| `INPUT_TIMEOUT` | 504 | true | 静止画を求めたが、配信のキーフレームが 5 秒以内に届かない |
| `OWNERSHIP_LOST` | 409 | false | クラスターで、配信を別のノードが受信している stream の録画の開始・静止画（`details.owner_api` のノードに頼み直します） |
| `PAYLOAD_TOO_LARGE` | 413 | false | 録画が大きすぎて MP4 に書き出せない、合意ログのコマンドが大きすぎる |
| `INTERNAL_ERROR` | 500 | true | 制御状態の保存の失敗など |

`retryable` の既定値は表のとおりです（`RATE_LIMITED`・`CAPACITY_UNAVAILABLE`・`INPUT_TIMEOUT`・`INTERNAL_ERROR` が true）。個々のエラーで
上書きされることがあります。`type` は `urn:senca:error:{code を小文字にし、_ をハイフンにしたもの}` です。

## 2. OpenAPI の定義

[`docs/api/openapi.yaml`](../../api/openapi.yaml)（OpenAPI 3.1）に、すべての操作・スキーマ・ステータスコードを記述しています。説明は日本語です。

- ノードは OpenAPI の定義ファイルを配信しません。リポジトリのファイルを、OpenAPI 3.1 に対応したビューアーで開いてください。例（Docker の Swagger UI）:

  ```
  docker run --rm -p 127.0.0.1:8081:8080 -e SWAGGER_JSON=/spec/openapi.yaml \
    -v "$PWD/docs/api:/spec:ro" swaggerapi/swagger-ui
  # http://127.0.0.1:8081/ を開く
  ```

  `/api/v1` は CORS ヘッダーを返さないため、ビューアーの「Try it out」から別オリジンのノードへリクエストを送ることはできません。試すときは curl か SDK を使ってください。
- 拡張フィールド:
  - `x-senca-scope`: その操作に必要な権限（`null` は認証だけが必要）
  - `x-senca-undefined-method-status: 405`: 定義されていないメソッドへの応答
  - `x-senca-event-data`: SSE の `data` のスキーマ（`SseEventData`）
- 現在対応していない機能（非同期処理の operation リソースなど）は、定義ファイルの冒頭にまとめています。
- JavaScript の SDK と Rust のクライアントの型は、この定義ファイルから生成しています。

## 3. イベント（SSE）

`GET /api/v1/events` は、変更を Server-Sent Events（`text/event-stream`）で配信します。必要な権限は `stream:read` です。

```
curl -sN -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/events
```

```
event: stream.created
id: 42
data: {"type":"stream.created","resource_id":"...","occurred_at":"2026-11-03T01:02:03.000Z","data":{"name":"studio-a"}}
```

- **制御のイベント**: 合意された（単一ノードでは保存された）制御のコマンドから作られます。SSE の `id` は制御ログ上の位置（適用済みのインデックス、整数）です。
  `data` は `{type, resource_id, occurred_at, data}` です。`occurred_at` は送信した時刻です。
- **テナントによる絞り込み**: 自分のテナントのイベントだけが届きます。テナントに属さないイベント（ノード・グリッド・`stream.owner`）は、
  `node:admin` を持つ主体にだけ届きます。
- **イベントの種類で絞り込む機能はサーバーにはありません**。SDK はクライアント側で不要なイベントを捨てます（§4・§5）。
- 15 秒ごとにコメント行（keep-alive）を送ります。
- 401・403 は、他の API と同じく problem+json で返します（403 の title は「権限 stream:read が必要です」）。

### 3.1 Last-Event-ID と resync

ノードは直近のイベントを残しています（既定 4096 件、`--events-retain`。単一ノードでは、再起動の後も制御ログのスナップショットより後の分を読み直します）。

- `id` は、適用したコマンドの数です。クラスターでは、同じイベントはどのノードでも同じ `id` になるため、別のノードへ再接続しても、
  同じ `Last-Event-ID` で続きを受け取れます。
- `Last-Event-ID` を付けて再接続すると、その位置より後のイベントのうちノードが残している分を先に送り、その後に新しいイベントを送ります。
- 位置が残している範囲より古いときは、最初に `event: resync`（`id` は現在の位置、`data: {}`）を送り、そのまま新しいイベントを送り続けます。
  `Last-Event-ID` が無ければ、接続した後のイベントだけを送ります。
- クラスターで、接続したノードの適用が `Last-Event-ID` に追いついていなければ、3 秒まで待ちます。
- 読み取りが遅れて通知があふれたときは、残している分から続きを送ります。それも無ければ `event: resync` を送って**接続を閉じます**。
- `resync` を受け取ったら、一覧の API で状態を取得し直してから、受け取った `id` を `Last-Event-ID` にして再接続してください。

### 3.2 イベントの一覧

制御のイベント（`id` あり）:

| 種類 | テナント | `data` |
|---|---|---|
| `stream.created` | あり | `{name}` |
| `stream.updated` | あり | `{generation}` |
| `stream.deleted` | あり | `{}` |
| `stream.owner` | なし（`node:admin`） | `{node}`（ストリームを担当するノードが替わった。[9. クラスターとグリッド](09-cluster-grid.md)） |
| `recording.updated` | あり | `{stream, state, generation}` |
| `recording.deleted` | あり | `{}` |
| `schedule.created`・`schedule.updated`・`schedule.deleted` | あり | `{}` |
| `schedule.run` | あり | `{state, occurrence, run_state, recording}` |
| `node.updated` | なし（`node:admin`） | `{draining}` |
| `input.updated`・`output.updated` | あり | `{stream, generation, enabled}` |
| `input.deleted`・`output.deleted` | あり | `{}` |
| `secret.created` | あり | `{name}` |
| `secret.deleted` | あり | `{}` |
| `pipeline.updated` | あり | `{name, desired_state}` |
| `pipeline.deleted` | あり | `{}` |
| `room.updated` | あり | `{auto}`（部屋の作成・自動作成・更新） |
| `room.deleted` | あり | `{}` |
| `media_pipeline.updated` | あり | `{room, desired_state}`（作成・更新） |
| `media_pipeline.deleted` | あり | `{}` |
| `node.down`・`node.up` | なし（`node:admin`） | `{}`（`resource_id` はノードの ID） |
| `device.down`・`device.up` | なし（`node:admin`） | `{device}`（ヘルスチェックで検出した状態の変化） |
| `device.disabled`・`device.enabled` | なし（`node:admin`） | `{device}`（手動での停止・再開） |
| `grid.decisions` | なし（`node:admin`） | `{decisions}`（配置の決定。`resource_id` は空） |

部屋のイベント（[10. リアルタイムの部屋](10-rooms.md)、[ADR-0015](../../dev/adr/0015-realtime-rooms.md)）:

| 種類 | 部屋の内部での名前 | `data.data` |
|---|---|---|
| `room.started` | `room.created` | `{settings, auto_created}` |
| `room.stopped` | `room.destroyed` | `{reason}` |
| `room.connection.created` | `connection.created` | `{participant}` |
| `room.connection.updated` | `connection.updated` | `{connection_id, metadata}` |
| `room.connection.destroyed` | `connection.destroyed` | `{connection_id, client_id?, reason, duration_ms}` |
| `room.track.published` | `track.published` | `{track}` |
| `room.track.unpublished` | `track.unpublished` | `{track_id, connection_id}` |
| `room.spotlight.changed` | `spotlight.changed` | `{focused}` |

- 部屋のイベントは合意ログに記録されません。**SSE の `id` を持たない**ため `Last-Event-ID` による再開の対象外で、取りこぼしたイベントは再送されません。
- **部屋のイベントは、そのイベントが起きたノードに接続している購読者にだけ届きます**（カスケードの扱いは [10. リアルタイムの部屋](10-rooms.md) を参照）。
  クラスターで全部の部屋のイベントを受け取るには、各ノードの `/api/v1/events` を購読してください。
- `data` は `{type, resource_id（部屋の ID）, event_id（イベントの ID。重複の除去に使います）, node（イベントを出したノードの ID）, occurred_at, data}` です。
  内側の `data` には上の表の内容だけが入ります（部屋の内部での名前の `type` は入りません）。
- テナントで絞り込みます。`node:admin` でも、他のテナントの部屋のイベントは届きません。部屋の設定の変更は、制御のイベント `room.updated` で通知されます。

```
event: room.connection.created
data: {"type":"room.connection.created","resource_id":"daily","event_id":"...","node":"...","occurred_at":"...","data":{"participant":{"connection_id":"...","client_id":"alice",...}}}
```

## 4. JavaScript の SDK（`@senca/sdk`）

`apps/sdk-js` にあります。ブラウザーで使うリアルタイムの部屋（`Senca.connect()`）と、ブラウザー・Node.js 20 以上で使う管理 API のクライアント（`SencaClient`）を含みます。
実行時の依存パッケージは無く、ソースは JSDoc で型を付けた ES modules です（ビルドせずに動きます）。型定義は `types/`、API の一覧は
[`apps/sdk-js/API.md`](../../../apps/sdk-js/API.md)、詳しい説明は [`apps/sdk-js/README.md`](../../../apps/sdk-js/README.md) にあります。

- npm には公開していません。リポジトリからインストールしてください（例 `npm install /path/to/senca/apps/sdk-js`）。
- エントリーポイント: `@senca/sdk`（すべて）、`@senca/sdk/rest`（管理 API だけ）、`@senca/sdk/protocol`（シグナリングの型）。

### 4.1 管理 API（`SencaClient`）

```js
import { SencaClient, ApiError } from '@senca/sdk';

const senca = new SencaClient({ baseUrl: 'https://node.example', token: process.env.SENCA_TOKEN });

const s = await senca.streams.create({ name: 'studio-a' });     // Idempotency-Key を自動で付ける
for await (const x of senca.streams.list({ limit: 100 })) console.log(x.name);   // cursor を自動でたどる

const cur = await senca.streams.get(s.id);
await senca.streams.update(s.id, { metadata: { a: 'b' } }, { ifMatch: cur });     // ETag・generation・リソースのどれでも指定できる
await senca.streams.modify(s.id, (c) => ({ metadata: { ...c.metadata, n: '1' } })); // 412 なら取得し直してやり直す（既定 3 回）

try {
  await senca.streams.update(s.id, { name: 'x' }, { ifMatch: 1 });
} catch (e) {
  if (e instanceof ApiError && e.code === 'PRECONDITION_FAILED') { /* 他のクライアントが先に更新した */ }
}
```

`ClientOptions`:

| 項目 | 既定 | 意味 |
|---|---|---|
| `baseUrl` | （必須） | `https://node.example`（`/api/v1` を補います）または `https://node.example/api/v1` |
| `token` | なし | API トークンまたは JWT。関数を渡すと、リクエストのたびに呼び出します |
| `retry` | `true` | 再試行の設定（`maxAttempts` 3（初回を含む）・`initialDelayMs` 200・`maxDelayMs` 5000・`factor` 2・`jitter` 0.2）。`false` で再試行しません |
| `timeoutMs` | 30000 | 1 回のリクエストのタイムアウト（SSE には適用しません） |
| `headers` | `{}` | すべてのリクエストに付けるヘッダー |
| `idempotency` | `true` | POST に Idempotency-Key（UUID v4）を自動で付けます |
| `fetch` | `globalThis.fetch` | fetch の実装を差し替えるときに使います |

- リソース: `streams`（`rotateKey`・`playbackUrl`・`startRecording`・`plan`・`snapshot`・`snapshots.list/get/delete`）・
  `recordings`（`stop`・`playbackUrl`・`thumbnail`・`exportMp4`。作成は `streams.startRecording`）・`schedules`（`check`・`runs`）・
  `tokens`・`inputs`・`outputs`（`create(streamId, body)`・`list(streamId)`）・`secrets`・`pipelines`（`validate`・`control`）・
  `nodes`（`drain`・`undrain`・`devices.disable/enable`）・`rooms`（`createToken`・`participants(id).list/get/updateMetadata/disconnect`・
  `sendMessage`・`stats`）・`mediaPipelines`（`list({room})`・`start`・`stop`）。クライアント自体のメソッドとして `capabilities()`・`devices()`・
  `grid()`・`events()` があります。型の無いリクエストは `senca.request(method, path, opts)` で送れます。
- 共通のメソッド: `create`・`list`（`Paginator`。`for await` で全件を取得）・`listPage`（1 ページ）・`get`・`update`（`ifMatch` が必須。無いと
  `TypeError`）・`modify`・`delete`（`ifMatch` は任意）。
- **再試行**: 通信の失敗・408・429・502・503・504・`retryable: true` の problem を、`Retry-After` と指数バックオフに従って再試行します。
  応答が無かった失敗を再試行するのは、GET・DELETE・PUT・Idempotency-Key 付きの POST だけです。PATCH は 429・503 のときだけ再試行します。
- **エラー**: problem+json は `ApiError`（`status`・`code`・`title`・`type`・`requestId`・`retryable`・`retryAfterMs`・`details`・`problem`）になります。
  通信の失敗は `status: 0`、`code: 'NETWORK_ERROR' | 'TIMEOUT'` です。
- 補助機能: `SencaClient.etagOf(resource)`、`SencaClient.newIdempotencyKey()`（プロセスをまたいで同じキーで再送するとき）、
  `SencaClient.operations`（operationId → メソッド・パス・権限の表）。

**SSE**

```js
for await (const ev of senca.events({ types: ['stream.*', 'room.connection.*'], signal })) {
  if (ev.type === 'resync') await reloadAll();
  else console.log(ev.type, ev.data.resource_id);
}
```

- `EventSource` は `Authorization` ヘッダーを送れないため、`fetch` で読み取ります。
- `types` は完全一致か末尾 `*` の前方一致で、クライアント側で絞り込みます。`resync` は絞り込みに関係なく必ず届きます。
- 接続が切れたら、最後の `id` を `Last-Event-ID` にして再接続します（1 秒から倍々で、最大 30 秒。`reconnect: false` で再接続しません）。
  401・403・404 では再接続せず、`ApiError` で終了します。

### 4.2 リアルタイムの部屋（`Senca.connect()`、ブラウザー）

```js
import { Senca } from '@senca/sdk';

const camera = await navigator.mediaDevices.getUserMedia({ video: { width: 1280, height: 720 }, audio: true });
const session = await Senca.connect({
  url: signalingUrl,                 // REST の RoomToken.signaling_url
  room: 'daily',
  token: () => fetch('/my-backend/room-token').then((r) => r.text()),   // 再接続のたびに取得し直す
  publish: { stream: camera, simulcast: true },
  dataChannels: ['#chat'],
});

for await (const track of session.remoteTracks()) {
  document.body.append(track.attach(document.createElement('video')));
}
```

- 参加トークンは、アプリのサーバーが `POST /api/v1/rooms/{id}/tokens`（`senca.rooms.createToken(room, {role, client_id, metadata})`）で発行し、
  `token` と `signaling_url` をブラウザーへ渡します。管理 API のトークンをブラウザーに渡さないでください。
- `Session` は、`participants`・`tracks`（随時更新される Map）、`events()`・`remoteTracks()`・`messages()`（非同期イテレーター）、`waitFor()`、
  `publish()`・`send()`・`stats()`・`close()` を持ちます。シグナリング v1 の SDP、受信の枠、再ネゴシエーション、ping、ICE restart、再接続
  （0.5 秒から倍々で最大 10 秒、10 回まで）、redirect は SDK の内部で処理します。
- エラーは `SencaError`（`code`・`fatal`・`closeCode`・`request`）です。
- 部屋の機能と制約は [10. リアルタイムの部屋](10-rooms.md)、プロトコルは [docs/realtime/signaling.md](../../realtime/signaling.md) を見てください。

## 5. Rust のクライアント（`senca-client`）

`crates/senca-client` にある、tokio ベースの非同期クライアントです。HTTP/1.1 と TLS には SENCA 自身の `senca-http`・`senca-tls` を使い、外部の HTTP クライアントには依存しません。
詳しくは [`crates/senca-client/README.md`](../../../crates/senca-client/README.md) を見てください。crates.io には公開していないため、path か git で依存関係に追加します。

```rust
use senca_client::{EventsOptions, ListOptions, ProblemCode, SencaClient, ServerEvent};
use senca_client::models::{StreamCreate, StreamPatch};

let client = SencaClient::new("https://senca.example", std::env::var("SENCA_TOKEN")?)?;

let s = client.streams().create(&StreamCreate { name: "studio-a".into(), ..Default::default() }).await?;
let key = s.ingest.stream_key.clone();   // stream key はこの応答でしか受け取れない

let patch = StreamPatch { name: Some("studio-b".into()), ..Default::default() };
let s = match client.streams().update(&s.id, &patch, &s).await {   // 前の応答（ETag）をそのまま If-Match に使う
    Ok(v) => v,
    Err(e) if e.code() == Some(ProblemCode::PreconditionFailed) => { /* 取得し直して再試行 */ todo!() }
    Err(e) => return Err(e.into()),
};

let mut it = client.streams().list_with(ListOptions::default().limit(100));
while let Some(x) = it.next().await {
    println!("{}", x?.name);
}

let mut events = client.events(EventsOptions::default().types(["stream.*", "recording.*"]));
while let Some(ev) = events.next().await {
    match ev? {
        ServerEvent::Event(e) => println!("{} {}", e.event, e.data.resource_id),
        ServerEvent::Resync { .. } => { /* 一覧を取得し直す */ }
    }
}

client.streams().delete(&s.id, s.if_match()).await?;
```

| リソース | メソッド |
|---|---|
| `streams()` | `create`・`create_with`・`list`・`list_with`・`list_page`・`get`・`update`・`delete`・`rotate_key`・`playback_url`・`start_recording`・`plan`・`snapshot`・`snapshots`・`snapshot_by_id`・`delete_snapshot` |
| `recordings()` | `list`・`list_with`（`ListOptions::stream`）・`list_page`・`get`・`update`・`delete`・`stop`・`playback_url`・`thumbnail`・`export_mp4` |
| `schedules()` | `create`・`create_with`・`list`・`list_with`・`list_page`・`check`・`get`・`update`・`delete`・`runs` |
| `tokens()` | `create`・`create_with`・`list`・`list_with`・`list_page`・`get`・`delete` |
| `inputs()`・`outputs()` | `create(stream_id, ..)`・`create_with`・`list(stream_id)`・`list_with`・`get`・`update`・`delete` |
| `secrets()` | `create`・`create_with`・`list`・`delete` |
| `pipelines()` | `create`・`create_with`・`list`・`list_with`・`list_page`・`get`・`update`・`delete`・`validate`・`control` |
| `nodes()` | `list`・`get`・`drain`・`undrain`・`disable_device`・`enable_device` |
| `rooms()` | `create`・`create_with`・`list`・`list_with`・`list_page`・`get`・`update`・`delete`・`issue_token`・`send_message`・`stats`・`participants`・`participants_with`・`participant`・`update_participant`・`disconnect` |
| `media_pipelines()` | `create`・`create_with`・`list`・`list_with`（`ListOptions::room`）・`list_page`・`get`・`update`・`set_state`・`delete` |
| クライアント | `capabilities`・`grid`・`devices`・`events`・`healthz`・`readyz`・`fetch_signed`（署名付きの再生パス。307 をたどります）・`request_json`（型の無いリクエスト） |

- **バージョン情報付きの応答**: `get`・`create`・`update` は `Versioned<T>`（`value`・`etag`・`replayed`・`request_id`。`Deref` で中身を参照できます）を返します。`update` の
  `if_match` には、`&Versioned<T>`・`IfMatch::Etag`・`IfMatch::Generation(n)`・`u64` のどれでも渡せます。
- **Idempotency**: POST には毎回新しいキーを付け、再試行でも同じキーを使います。`create_with(.., RequestOptions::idempotency_key("..."))`
  でキーを指定すれば、プロセスをまたいだ再送でも同じ応答が返ります（`Versioned::replayed` が true になります）。
- **エラー**: `Error::Api(ApiError)`（`code`（`ProblemCode`。未知の値は `Unknown` と `raw_code`）・`title`・`status`・`retryable`・
  `request_id`・`details`・`retry_after`）。ほかに `Connect`・`Transport`・`Timeout`・`Decode`・`InvalidUrl`・`InvalidArgument` があります。
- **再試行**（`RetryPolicy`）: 既定では、最初のリクエストの後に最大 3 回、200 ms から倍々・上限 10 秒・±20 % のゆらぎで再試行し、`Retry-After` 以上の時間を待ちます。
  `ClientBuilder::retry(RetryPolicy::NONE)` で再試行しなくなります。PATCH は `retryable`・429・503 のときだけ再試行します。
- **SSE**: `EventsOptions`（`types`・`last_event_id`・`reconnect`（既定 true）・`initial_backoff` 1 秒・`max_backoff` 30 秒）。
  接続が切れたら `Last-Event-ID` を付けて再接続します。401・403 では再接続しません。
- **クライアントの構築**: `SencaClient::builder(url).token(..).timeout(..)`（既定 30 秒）`.user_agent(..).root_certificates_pem(pem).build()`。
  TLS の既定では OS の認証局を使います。
- **モデル**: `senca_client::models` の型は openapi.yaml から生成しています。応答に含まれる未知のフィールドは
  読み飛ばし、未知の enum の値は `Unknown` になります。`*Patch` の null を取りうる項目は `Option<Option<T>>` です（`None` = 変更しない、
  `Some(None)` = null を送って値を消す）。

## 6. 関連資料

- クラスターでの書き込み（どのノードに送ってもかまいません。変更の提案はリーダーへ転送されます）: [9. クラスターとグリッド](09-cluster-grid.md)、[ADR-0009](../../dev/adr/0009-cluster.md)
- 部屋と、SSE で届く部屋のイベント: [10. リアルタイムの部屋](10-rooms.md)、[ADR-0015](../../dev/adr/0015-realtime-rooms.md)
- メディアパイプライン: [11. メディアパイプライン](11-media-pipelines.md)、[ADR-0016](../../dev/adr/0016-media-pipelines.md)
- トークンと秘密情報の扱い: [12. セキュリティ](12-security.md)、運用（ログ・監視）: [13. 運用](13-operations.md)
