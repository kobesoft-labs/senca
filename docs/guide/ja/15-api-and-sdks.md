# 15. 管理 API と SDK

[English](../en/15-api-and-sdks.md)

SENCA のノードは HTTP の管理 API（`/api/v1`）で操作する。管理画面（`/console/`）も同じ API を使う。この章は API の共通の決まり、
OpenAPI の文書、イベント（SSE）、JavaScript の SDK（`@senca/sdk`）と Rust のクライアント（`senca-client`）をまとめる。

- 定義: [`docs/api/openapi.yaml`](../../api/openapi.yaml)（実装の挙動を書いたもの。`tools/interop/openapi.sh` が経路表・権限・応答と突き合わせる）
- 実装: `crates/senca-node/src/api/`（振り分けと共通の契約は `api/mod.rs`）、`crates/senca-node/src/events.rs`（SSE）、
  `crates/senca-control/src/auth.rs`・`jwt.rs`・`problem.rs`
- 個別の資源: 配信と再生は [04-ingest-playback.md](04-ingest-playback.md)、録画は [05-recording.md](05-recording.md)、入力・出力は
  [06-inputs-outputs.md](06-inputs-outputs.md)、パイプラインは [07-pipelines.md](07-pipelines.md)、ノードとグリッドは
  [09-cluster-grid.md](09-cluster-grid.md)、部屋は [10-rooms.md](10-rooms.md)、メディアパイプラインは [11-media-pipelines.md](11-media-pipelines.md)

## 1. 共通の決まり

### 1.1 基本

| 項目 | 値 |
|---|---|
| base path | `/api/v1`（`--http` で指定した待ち受け。`--https` を付ければ TLS でも同じ） |
| 本文 | JSON（UTF-8）。時刻は UTC の RFC 3339（`2026-10-25T00:00:00.000Z`） |
| 本文の上限 | 1 MiB（HTTP 層。超えると text/plain の 413 を返して接続を閉じる。problem+json ではない） |
| 応答の見出し | すべてに `X-Request-Id` と `Cache-Control: no-store` |
| `X-Request-Id` | 要求の `X-Request-Id` が 64 文字以内の英数字と `-` `_` `.` ならそれを使う。違えば `req_...` を作る |
| パス | 空の区切り（`//`）・`.`・`..` を含むパスは 404 |
| メソッド | 知られた資源に定義の無いメソッド（HEAD を含む）は 405（`METHOD_NOT_ALLOWED`）。知らないパスは 404 |
| CORS | `/api/v1` は CORS の見出しを返さない。ブラウザーからは同じ origin（管理画面など）か、アプリのサーバーを通して呼ぶ |

### 1.2 認証

すべての `/api/v1`（`/capabilities` を含む）に `Authorization: Bearer <値>` が要る。`/healthz`・`/readyz`・署名付きの再生パス
（`/s/...`）は要らない。

- 値が **`senca_` で始まれば API トークン**、それ以外は **JWT**。値は 8192 バイトまで。
- 失敗は 401（`AUTH_FAILED`、`WWW-Authenticate: Bearer`）。理由（期限切れ・知らないトークンなど）は応答に出さず、ノードのログに書く。

**API トークン**

- 最初のノードが起動したとき、トークンが 1 つも無ければ `tenant:admin` と `node:admin` を持つトークン（名前 `bootstrap-admin`、
  テナント `default`、期限なし）を作り、値を `{data-dir}/control/bootstrap-admin-token`（mode 0600）に書く。クラスターでは
  ID の最も小さいノードが作る。
- 以後は `POST /api/v1/tokens`（`tenant:admin`）で作る。値（`token`）は作成の応答でだけ返る。保存するのはハッシュだけ。

```
TOKEN=$(cat data/control/bootstrap-admin-token)
curl -s -X POST http://127.0.0.1:8080/api/v1/tokens \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"ci","scopes":["stream:read","stream:write"],"expires_at":"2027-01-01T00:00:00Z"}'
```

| フィールド | 決まり |
|---|---|
| `name` | 1〜64 文字（違えば 400 `INVALID_CONFIG`） |
| `scopes` | 1 つ以上。知らない権限は 400。**自分が持たない権限は渡せない**（403 `FORBIDDEN`） |
| `expires_at` | 任意。RFC 3339。過去なら 400。期限を過ぎたトークンは 401 |

- 一覧・取得は値を返さない。`DELETE /tokens/{id}` で失効する。テナントの**最後の `tenant:admin` トークンは消せない**（409 `CONFLICT`）。
- トークンの扱い（保管・入れ替え）は [12-security.md](12-security.md)。

**JWT**（外部の認証基盤のトークンをそのまま使う）

```
senca-node --jwks /etc/senca/jwks.json --jwt-issuer https://idp.example/ --jwt-audience senca [--jwt-tenant-claim tenant]
```

- 鍵は**ローカルの JWKS ファイルだけ**（トークンの中身で URL を取りに行かない）。`--jwks` には `--jwt-issuer` と `--jwt-audience` が必須。
- 算法: EdDSA（Ed25519）・ES256・RS256（RSA は 2048 ビット以上）。算法は鍵が決め、ヘッダーの `alg` が鍵と違えば拒否。
  JWKS の鍵は `kid` が必須、`use` があれば `sig`。`alg=none`・HS 系・`crit` 付きは拒否。
- 検査: `kid` が JWKS にあること、`exp` が必須、`nbf`、`iss` の一致、`aud` の一致（文字列か配列）。時計のずれは 60 秒まで許す。
- テナントはクレーム `tenant`（`--jwt-tenant-claim` で名前を変える）。無い・空なら拒否。主体は `sub`。
- 権限は `scope`（空白区切り）か `scp`（文字列か配列）。知らない権限は黙って捨てる。

### 1.3 権限とテナント

| 権限 | 主な操作 |
|---|---|
| `stream:read` | 配信・予約・入力・出力・パイプライン・部屋・メディアパイプラインの読み取り、`POST /streams/{id}/plan`、`POST /schedules/check`、`POST /pipelines/{id}/validate`、**SSE** |
| `stream:write` | 上の資源の作成・更新・削除、鍵の作り直し、録画の開始・停止・`PATCH /recordings/{id}`、部屋の参加トークン・参加者の操作・message |
| `subscribe` | `POST /streams/{id}/playback-url`（ライブの署名付き URL） |
| `recording:read` | 録画の一覧・取得・VOD の URL・サムネイル・MP4 の書き出し |
| `recording:delete` | `DELETE /recordings/{id}` |
| `tenant:admin` | トークン・秘密値（`/secrets`）。**`node:admin` 以外のすべてを含む** |
| `node:admin` | テナントの外の運用: `/nodes`・`/devices`・`/grid`・drain・装置の停止と再開、SSE のテナントを持たないイベント |
| `publish` | WHIP の送出（`POST /whip/{stream_id}` に stream key の代わりに使える。同じテナントの stream だけ） |

- 操作ごとの権限は OpenAPI の拡張 `x-senca-scope` にある（`null` は認証だけ。`GET /capabilities` がそれ）。足りなければ 403
  （`FORBIDDEN`、title に要る権限の名前）。
- **テナントは認証から決まる**（API トークンはそれを作った主体のテナント、JWT はテナントのクレーム）。本文でテナントを指定する欄は無い。
  他テナントの資源は 404 に見える。新しいテナントは JWT のテナントのクレームで作る（API トークンは自分のテナントにしか作れない）。

### 1.4 本文の検査

- 本文のある要求は `Content-Type: application/json`（`; charset=...` は可）でなければ 415（`UNSUPPORTED_MEDIA_TYPE`）。
- **未知のフィールドは 400**（`INVALID_REQUEST`、`details.reason` に解析の誤り）。型の誤りも同じ。黙って無視しない。
- 値の検査（範囲・組み合わせ・参照先）の誤りは多くが 400 `INVALID_CONFIG`。

### 1.5 Idempotency-Key

POST に `Idempotency-Key` を付けると、24 時間は同じ要求に同じ答えを返す。作成の再送で二重に作らないために使う。

- 鍵は 1〜255 文字の表示可能な ASCII（空白なし）。違えば 400。鍵の範囲はテナントごと。
- 同じ要求かどうかは、メソッド・パス・クエリ・本文の SHA-256 で見る（見出しは見ない）。**同じ鍵で別の要求**、または別の主体が
  同じ鍵を使うと 409（`IDEMPOTENCY_CONFLICT`）。
- 再送の応答は、覚えた状態コード・本文・`Location`・`ETag` に **`Idempotency-Replayed: true`** を付けたもの。
- **4xx も覚える**（同じ鍵の再送は同じ誤りを返す）。**5xx は覚えない**（再送すれば実行し直す）。256 KiB を超える応答は覚えない。
- 覚える本文から秘密を消す: `stream_key`・`token`・`secret` は `null` に、代わりに `stream_key_redacted: true` などを入れる。
  つまり**秘密は最初の応答でだけ見える**。再送で得た応答から値は取れない。
- 同じ鍵の処理はノードの中で直列にする（同時の再送でも 1 回だけ実行する）。

### 1.6 ETag・If-Match（楽観的排他）

- 資源の応答（作成・取得・更新）の `ETag` は `"{generation}"`（例 `"3"`）。`W/"3"` も受け付ける。
- **PATCH は `If-Match` が必須**。無いと 428（`PRECONDITION_REQUIRED`）、形が違う・古いと 412（`PRECONDITION_FAILED`）。
- DELETE の `If-Match` は任意で、`/streams`・`/recordings`・`/schedules` だけが検査する（付ければ古いと 412）。入力・出力・秘密値・
  パイプライン・部屋・メディアパイプライン・トークンの DELETE は見ない。

```
# 取得 → If-Match で更新
ETAG=$(curl -s -D - -o /dev/null -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/streams/$ID | awk -F': ' 'tolower($1)=="etag"{print $2}' | tr -d '\r')
curl -s -X PATCH http://127.0.0.1:8080/api/v1/streams/$ID \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -H "If-Match: $ETAG" \
  -d '{"metadata":{"owner":"studio-a"}}'
```

412 が返ったら、読み直してから変更を当て直す。

### 1.7 一覧と頁

- 一覧は `{"items": [...], "next_cursor": "..." | null}`。順序は ID の昇順。
- クエリは `limit`（1〜1000、既定 100）と `cursor`（前の応答の `next_cursor`）。範囲外の `limit` は 400。
- 一覧の**知らないクエリは 400**。絞り込みは決まったものだけ: `GET /recordings` の `stream`・`state`、`GET /schedules` の
  `stream`・`state`、`GET /media-pipelines` の `room`。
- `next_cursor` が `null` なら最後の頁。頁の無い一覧もある（`GET /secrets` は全件で `next_cursor: null`、`GET /nodes` は `items` だけ）。

```
curl -s -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:8080/api/v1/streams?limit=50'
curl -s -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:8080/api/v1/streams?limit=50&cursor=<next_cursor>'
```

### 1.8 エラー（problem+json）

誤りは `Content-Type: application/problem+json`（RFC 9457 の形）で返す。

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

- `title` は日本語の説明。プログラムでは `code` で分ける。`details` は code ごとの補足（無ければ省く）。
- `retryable` が真なら、同じ要求を送り直してよい。503・429・504 で `retryable` のときは `Retry-After` が付く
  （429 は次に通るまでの秒数、それ以外は 1）。
- 量の上限（429）は、どの `/api/v1` の操作でも返ることがある（[12. セキュリティ](12-security.md)）。
- `/api/v1` 以外の版（`/api/v2/...` など）は 422 `UNSUPPORTED_PROTOCOL_VERSION`（`details.supported: ["v1"]`）。
- 問い合わせには `request_id`（= `X-Request-Id`）を添える。

| `code` | 状態 | `retryable` | 主な場面 |
|---|---|---|---|
| `INVALID_CONFIG` | 400 | 否 | 値の検査の誤り（範囲・組み合わせ・参照先） |
| `INVALID_REQUEST` | 400 | 否 | 本文の解析の誤り・未知のフィールド・知らないクエリ・`limit` の範囲・Idempotency-Key の形 |
| `UNSUPPORTED_PROTOCOL_VERSION` | 422 | 否 | `/api/v1` 以外の版のパス |
| `UNSUPPORTED_CODEC` | 422 | 否 | 静止画の復号（`GET /streams/{id}/snapshot`）、部屋の核からの 422、E2EE の部屋・書き出した stream でのサーバーの処理（`details.reason: "e2ee"`） |
| `UNSUPPORTED_MEDIA_TYPE` | 415 | 否 | `Content-Type` が `application/json` でない |
| `AUTH_FAILED` | 401 | 否 | 認証の失敗 |
| `FORBIDDEN` | 403 | 否 | 権限が足りない、持たない権限をトークンに渡そうとした |
| `NOT_FOUND` | 404 | 否 | 無い、他テナントの資源、知らないパス |
| `METHOD_NOT_ALLOWED` | 405 | 否 | 資源に無いメソッド |
| `CONFLICT` | 409 | 否 | 状態と合わない（使用中の秘密値の削除、最後の管理者トークンの削除など） |
| `IDEMPOTENCY_CONFLICT` | 409 | 否 | 同じ Idempotency-Key で別の要求 |
| `PRECONDITION_FAILED` | 412 | 否 | `If-Match` が古い・形が違う |
| `PRECONDITION_REQUIRED` | 428 | 否 | PATCH に `If-Match` が無い |
| `QUOTA_EXCEEDED` | 403 | 否 | 上限（同じ stream の同時の録画は 8 本など） |
| `RATE_LIMITED` | 429 | 是 | 要求の量の上限を超えた（`details.limit` は `ip`・`token`・`auth_failures`・`connect`） |
| `CAPACITY_UNAVAILABLE` | 503 | 多くは是 | クラスターの提案を受け付けられない（リーダーの交代・混雑）、部屋が別のノードにある、`--webrtc-udp` が無い、codec worker が無い（このときは否） |
| `INPUT_TIMEOUT` | 504 | 是 | 今の画を頼んだが、配信のキーフレームが 5 秒以内に届かない |
| `OWNERSHIP_LOST` | 409 | 否 | クラスターで、配信を別のノードが受けている stream の録画の開始・今の画（`details.owner_api` のノードに頼み直す） |
| `PAYLOAD_TOO_LARGE` | 413 | 否 | 録画が大きすぎて MP4 に書き出せない、合意ログのコマンドが大きすぎる |
| `INTERNAL_ERROR` | 500 | 是 | 制御状態の永続化の失敗など |

`retryable` の既定は表のとおり（`RATE_LIMITED`・`CAPACITY_UNAVAILABLE`・`INPUT_TIMEOUT`・`INTERNAL_ERROR` が真）で、個々の誤りが
上書きすることがある。`type` は `urn:senca:error:{code を小文字・ハイフンにしたもの}`。

## 2. OpenAPI の文書

[`docs/api/openapi.yaml`](../../api/openapi.yaml)（OpenAPI 3.1）がすべての操作・スキーマ・状態コードを持つ。説明は日本語。

- ノードは OpenAPI の文書を配らない。リポジトリのファイルを OpenAPI 3.1 に対応した閲覧器で開く。例（Docker の Swagger UI）:

  ```
  docker run --rm -p 127.0.0.1:8081:8080 -e SWAGGER_JSON=/spec/openapi.yaml \
    -v "$PWD/docs/api:/spec:ro" swaggerapi/swagger-ui
  # http://127.0.0.1:8081/ を開く
  ```

  `/api/v1` は CORS の見出しを返さないので、閲覧器の「試す」から別 origin のノードへは送れない。試すのは curl か SDK で。
- 拡張:
  - `x-senca-scope`: その操作に要る権限（`null` は認証だけ）
  - `x-senca-undefined-method-status: 405`: 書いていないメソッドの扱い
  - `x-senca-event-data`: SSE の `data` のスキーマ（`SseEventData`）
- 各操作の説明に「仕様との差」がある。まとめは文書の冒頭（operation 資源が無い、`PATCH /streams/{id}` は 200 など）。
- JS の SDK（`apps/sdk-js/scripts/gen_types.py`）と Rust のクライアント（`crates/senca-client/gen/gen_models.py`）の型はこの文書から生成する。
- 確かめ方: `tools/interop/openapi.sh` が (a) `api/mod.rs` の経路表・handler の権限と文書の過不足、(b) 実際にノードを起動して全操作を送り、
  状態コードが文書にあり本文がスキーマに合うことを見る。

## 3. イベント（SSE）

`GET /api/v1/events` は Server-Sent Events（`text/event-stream`）で変更を流す。権限は `stream:read`。

```
curl -sN -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/events
```

```
event: stream.created
id: 42
data: {"type":"stream.created","resource_id":"...","occurred_at":"2026-11-03T01:02:03.000Z","data":{"name":"studio-a"}}
```

- **制御のイベント**: 合意した（単一ノードでは保存した）制御のコマンドから作る。SSE の `id` は制御ログの位置（applied index、整数）。
  `data` は `{type, resource_id, occurred_at, data}`。`occurred_at` は送った時刻。
- **テナントで絞る**: 自分のテナントのイベントだけが流れる。テナントを持たないイベント（ノード・グリッド・`stream.owner`）は
  `node:admin` を持つ主体にだけ流れる。
- **種類で絞る機能はサーバーに無い**。SDK はクライアントの側で捨てる（§4・§5）。
- 15 秒ごとにコメント行（keepalive）を送る。
- 401・403 は他の API と同じ problem+json（403 の title は「権限 stream:read が必要です」）。

### 3.1 Last-Event-ID と resync

ノードは直近のイベントを残しています（既定 4096 件、`--events-retain`。単一ノードでは再起動の後も、制御ログの写しより後の分を読み直します）。

- `id` は、適用したコマンドの数です。クラスターでは、どのノードでも同じイベントは同じ `id` になります。
  そのため、別のノードへつなぎ直しても、同じ `Last-Event-ID` で続きを受け取れます。
- `Last-Event-ID` を付けてつなぎ直すと、その位置より後のイベントのうち、ノードが残している分を先に送り、その後に新しいイベントを流します。
- 位置が残している範囲より古いときは、最初に `event: resync`（`id` は今の位置、`data: {}`）を送り、そのまま新しいイベントを流し続けます。
  `Last-Event-ID` が無ければ、つないだ後のイベントだけが流れます。
- クラスターで、つないだノードの適用が `Last-Event-ID` に追いついていなければ、3 秒まで待ちます。
- 読むのが遅れて通知があふれたときは、残している分から続きを送ります。それも無ければ `event: resync` を送って**接続を閉じます**。
- `resync` を受けたら、一覧の API で状態を読み直してから、受けた `id` を `Last-Event-ID` にしてつなぎ直します。

### 3.2 イベントの一覧

制御のイベント（`id` あり）:

| 種類 | テナント | `data` |
|---|---|---|
| `stream.created` | あり | `{name}` |
| `stream.updated` | あり | `{generation}` |
| `stream.deleted` | あり | `{}` |
| `stream.owner` | なし（`node:admin`） | `{node}`（配信の持ち主のノードが替わった。[09-cluster-grid.md](09-cluster-grid.md)） |
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
| `room.updated` | あり | `{auto}`（部屋の作成・自動の作成・更新） |
| `room.deleted` | あり | `{}` |
| `media_pipeline.updated` | あり | `{room, desired_state}`（作成・更新） |
| `media_pipeline.deleted` | あり | `{}` |
| `node.down`・`node.up` | なし（`node:admin`） | `{}`（`resource_id` はノードの ID） |
| `device.down`・`device.up` | なし（`node:admin`） | `{device}`（測った健康の変化） |
| `device.disabled`・`device.enabled` | なし（`node:admin`） | `{device}`（手での停止・再開） |
| `grid.decisions` | なし（`node:admin`） | `{decisions}`（配置の決定。`resource_id` は空） |

部屋の出来事（[10-rooms.md](10-rooms.md)、ADR-0015）:

| 種類 | 部屋の核での名前 | `data.data` |
|---|---|---|
| `room.started` | `room.created` | `{settings, auto_created}` |
| `room.stopped` | `room.destroyed` | `{reason}` |
| `room.connection.created` | `connection.created` | `{participant}` |
| `room.connection.updated` | `connection.updated` | `{connection_id, metadata}` |
| `room.connection.destroyed` | `connection.destroyed` | `{connection_id, client_id?, reason, duration_ms}` |
| `room.track.published` | `track.published` | `{track}` |
| `room.track.unpublished` | `track.unpublished` | `{track_id, connection_id}` |
| `room.spotlight.changed` | `spotlight.changed` | `{focused}` |

- 部屋の出来事は合意ログに載らない。**SSE の `id` を持たない**ので `Last-Event-ID` の再開の対象外で、取りこぼしは黙って飛ばす。
- **部屋を置いたノードにつないだ購読者にだけ流れる**。クラスターで全部の部屋の出来事を見るには、各ノードの `/api/v1/events` を購読する。
- `data` は `{type, resource_id（部屋の ID）, event_id（出来事の ID。重複を除くのに使う）, node（出したノードの ID）, occurred_at, data}`。
  内側の `data` は上の表の中身だけ（部屋の核の名前の `type` は入らない）。
- テナントで絞る。`node:admin` でも他テナントの部屋の出来事は流れない。部屋の設定の変更は制御のイベント `room.updated` が出す。

```
event: room.connection.created
data: {"type":"room.connection.created","resource_id":"daily","event_id":"...","node":"...","occurred_at":"...","data":{"participant":{"connection_id":"...","client_id":"alice",...}}}
```

## 4. JavaScript の SDK（`@senca/sdk`）

`apps/sdk-js`。ブラウザーのリアルタイムの部屋（`Senca.connect()`）と、ブラウザー・Node.js 20 以上の管理 API（`SencaClient`）を持つ。
実行時の依存は無く、ソースは JSDoc の型を付けた ES modules（ビルドせずに動く）。型定義は `types/`、API の一覧は
[`apps/sdk-js/API.md`](../../../apps/sdk-js/API.md)、詳しい説明は [`apps/sdk-js/README.md`](../../../apps/sdk-js/README.md)。

- npm の公開はまだ無い（版は 0.0.0）。リポジトリから入れる（例 `npm install /path/to/senca/apps/sdk-js`）。
- 入口: `@senca/sdk`（全部）、`@senca/sdk/rest`（管理 API だけ）、`@senca/sdk/protocol`（シグナリングの型）。

### 4.1 管理 API（`SencaClient`）

```js
import { SencaClient, ApiError } from '@senca/sdk';

const senca = new SencaClient({ baseUrl: 'https://node.example', token: process.env.SENCA_TOKEN });

const s = await senca.streams.create({ name: 'studio-a' });     // Idempotency-Key を自動で付ける
for await (const x of senca.streams.list({ limit: 100 })) console.log(x.name);   // cursor を自動でたどる

const cur = await senca.streams.get(s.id);
await senca.streams.update(s.id, { metadata: { a: 'b' } }, { ifMatch: cur });     // ETag・generation・資源のどれでも
await senca.streams.modify(s.id, (c) => ({ metadata: { ...c.metadata, n: '1' } })); // 412 なら読み直してやり直す（既定 3 回）

try {
  await senca.streams.update(s.id, { name: 'x' }, { ifMatch: 1 });
} catch (e) {
  if (e instanceof ApiError && e.code === 'PRECONDITION_FAILED') { /* 他の人が先に書いた */ }
}
```

`ClientOptions`:

| 項目 | 既定 | 意味 |
|---|---|---|
| `baseUrl` | （必須） | `https://node.example`（`/api/v1` を足す）か `https://node.example/api/v1` |
| `token` | なし | API トークンか JWT。関数なら要求のたびに呼ぶ |
| `retry` | `true` | 送り直しの方針（`maxAttempts` 3（最初を含む）・`initialDelayMs` 200・`maxDelayMs` 5000・`factor` 2・`jitter` 0.2）。`false` で送り直さない |
| `timeoutMs` | 30000 | 1 回の要求の時間切れ（SSE には使わない） |
| `headers` | `{}` | すべての要求に付ける見出し |
| `idempotency` | `true` | POST に自動で Idempotency-Key（UUID v4）を付ける |
| `fetch` | `globalThis.fetch` | 差し替え用 |

- 資源: `streams`（`rotateKey`・`playbackUrl`・`startRecording`・`plan`・`snapshot`・`snapshots.list/get/delete`）・
  `recordings`（`stop`・`playbackUrl`・`thumbnail`・`exportMp4`。作成は `streams.startRecording`）・`schedules`（`check`・`runs`）・
  `tokens`・`inputs`・`outputs`（`create(streamId, body)`・`list(streamId)`）・`secrets`・`pipelines`（`validate`・`control`）・
  `nodes`（`drain`・`undrain`・`devices.disable/enable`）・`rooms`（`createToken`・`participants(id).list/get/updateMetadata/disconnect`・
  `sendMessage`・`stats`）・`mediaPipelines`（`list({room})`・`start`・`stop`）。クライアントの `capabilities()`・`devices()`・
  `grid()`・`events()`。型の無い要求は `senca.request(method, path, opts)`。
- 共通のメソッド: `create`・`list`（`Paginator`、`for await` で全件）・`listPage`（1 頁）・`get`・`update`（`ifMatch` 必須。無いと
  `TypeError`）・`modify`・`delete`（`ifMatch` は任意）。
- **送り直し**: 通信の失敗・408・429・502・503・504・`retryable: true` の problem を、`Retry-After` と指数バックオフで送り直す。
  応答が無かった失敗は GET・DELETE・PUT・Idempotency-Key 付きの POST だけ送り直す。PATCH は 429・503 だけ。
- **誤り**: problem+json は `ApiError`（`status`・`code`・`title`・`type`・`requestId`・`retryable`・`retryAfterMs`・`details`・`problem`）。
  通信の失敗は `status: 0`、`code: 'NETWORK_ERROR' | 'TIMEOUT'`。
- 補助: `SencaClient.etagOf(resource)`、`SencaClient.newIdempotencyKey()`（プロセスをまたいで同じ鍵で送り直すとき）、
  `SencaClient.operations`（operationId → method・path・権限の表）。

**SSE**

```js
for await (const ev of senca.events({ types: ['stream.*', 'room.connection.*'], signal })) {
  if (ev.type === 'resync') await reloadAll();
  else console.log(ev.type, ev.data.resource_id);
}
```

- `EventSource` は `Authorization` を送れないので `fetch` で読む。
- `types` は完全一致か末尾 `*` の前方一致で、クライアントの側で絞る。`resync` は絞っても必ず流す。
- 切れたら最後の `id` を `Last-Event-ID` にしてつなぎ直す（1 秒から倍々、最大 30 秒。`reconnect: false` で付け直さない）。
  401・403・404 はつなぎ直さずに `ApiError` で終わる。

### 4.2 リアルタイムの部屋（`Senca.connect()`、ブラウザー）

```js
import { Senca } from '@senca/sdk';

const camera = await navigator.mediaDevices.getUserMedia({ video: { width: 1280, height: 720 }, audio: true });
const session = await Senca.connect({
  url: signalingUrl,                 // REST の RoomToken.signaling_url
  room: 'daily',
  token: () => fetch('/my-backend/room-token').then((r) => r.text()),   // 再接続のたびに取り直す
  publish: { stream: camera, simulcast: true },
  dataChannels: ['#chat'],
});

for await (const track of session.remoteTracks()) {
  document.body.append(track.attach(document.createElement('video')));
}
```

- 参加トークンはアプリのサーバーが `POST /api/v1/rooms/{id}/tokens`（`senca.rooms.createToken(room, {role, client_id, metadata})`）で作り、
  `token` と `signaling_url` をブラウザーへ渡す。管理 API のトークンをブラウザーに渡さない。
- `Session` は `participants`・`tracks`（その場で変わる Map）、`events()`・`remoteTracks()`・`messages()`（非同期の列）、`waitFor()`、
  `publish()`・`send()`・`stats()`・`close()` を持つ。シグナリング v1 の SDP・受信の枠・再交渉・ping・ICE restart・再接続
  （0.5 秒から倍々で最大 10 秒、10 回まで）・redirect は SDK の中で扱う。
- 誤りは `SencaError`（`code`・`fatal`・`closeCode`・`request`）。
- 部屋の機能と限界は [10-rooms.md](10-rooms.md)、プロトコルは [docs/realtime/signaling.md](../../realtime/signaling.md)。

### 4.3 試験

```
cd apps/sdk-js && npm test        # 単体（偽の WebSocket・RTCPeerConnection・fetch、オフライン）
npm run build                     # tsc で型を検査し、types/・API.md を作る（TypeScript 5.x）
npm run gen                       # openapi.yaml が変わったら型を作り直す
tools/interop/sdk_js.sh           # 本物の senca-node と Docker の Chrome 3 ページまで
```

## 5. Rust のクライアント（`senca-client`）

`crates/senca-client`。tokio の非同期クライアント。HTTP/1.1 は `senca-http`、TLS は `senca-tls` で、外部の HTTP クライアントに依らない。
詳しくは [`crates/senca-client/README.md`](../../../crates/senca-client/README.md)。crates.io の公開はまだ無いので、path か git で依存に入れる。

```rust
use senca_client::{EventsOptions, ListOptions, ProblemCode, SencaClient, ServerEvent};
use senca_client::models::{StreamCreate, StreamPatch};

let client = SencaClient::new("https://senca.example", std::env::var("SENCA_TOKEN")?)?;

let s = client.streams().create(&StreamCreate { name: "studio-a".into(), ..Default::default() }).await?;
let key = s.ingest.stream_key.clone();   // stream key はこの応答でだけ渡る

let patch = StreamPatch { name: Some("studio-b".into()), ..Default::default() };
let s = match client.streams().update(&s.id, &patch, &s).await {   // 前の応答（ETag）をそのまま If-Match に
    Ok(v) => v,
    Err(e) if e.code() == Some(ProblemCode::PreconditionFailed) => { /* 読み直して再試行 */ todo!() }
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
        ServerEvent::Resync { .. } => { /* 一覧を読み直す */ }
    }
}

client.streams().delete(&s.id, s.if_match()).await?;
```

| 資源 | メソッド |
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
| クライアント | `capabilities`・`grid`・`devices`・`events`・`healthz`・`readyz`・`fetch_signed`（署名付きの再生パス。307 をたどる）・`request_json`（型の無い要求） |

- **版**: `get`・`create`・`update` は `Versioned<T>`（`value`・`etag`・`replayed`・`request_id`。`Deref` で中身）。`update` の
  `if_match` は `&Versioned<T>`・`IfMatch::Etag`・`IfMatch::Generation(n)`・`u64` のどれでも。
- **Idempotency**: POST は毎回新しい鍵を付け、送り直しでも同じ鍵を使う。`create_with(.., RequestOptions::idempotency_key("..."))`
  で鍵を決めれば、プロセスをまたいだ再送でも同じ答えになる（`Versioned::replayed` が真）。
- **誤り**: `Error::Api(ApiError)`（`code`（`ProblemCode`。知らない値は `Unknown` と `raw_code`）・`title`・`status`・`retryable`・
  `request_id`・`details`・`retry_after`）。他に `Connect`・`Transport`・`Timeout`・`Decode`・`InvalidUrl`・`InvalidArgument`。
- **送り直し**（`RetryPolicy`）: 既定は最初の要求の後に最大 3 回、200 ms から倍々・上限 10 秒・±20 % のゆらぎ、`Retry-After` 以上待つ。
  `ClientBuilder::retry(RetryPolicy::NONE)` で送り直さない。PATCH は `retryable`・429・503 のときだけ。
- **SSE**: `EventsOptions`（`types`・`last_event_id`・`reconnect`（既定 true）・`initial_backoff` 1 秒・`max_backoff` 30 秒）。
  切れたら `Last-Event-ID` を付けてつなぎ直す。401・403 ではつなぎ直さない。
- **組み立て**: `SencaClient::builder(url).token(..).timeout(..)`（既定 30 秒）`.user_agent(..).root_certificates_pem(pem).build()`。
  TLS の既定は OS の認証局。
- **モデル**: `src/models.rs` は openapi.yaml からの生成物（`python3 crates/senca-client/gen/gen_models.py`）。応答の未知のフィールドは
  読み飛ばし、知らない enum の値は `Unknown`。`*Patch` の null になりうる項目は `Option<Option<T>>`（`None` = 変えない、
  `Some(None)` = null で消す）。
- 試験: `cargo test -p senca-client`（`tests/node.rs` は本物の senca-node を起動して確かめる）。

## 6. 関係

- 管理 API の契約の根拠: 仕様 SENCA-SRS-001 10 章、[REQ-002](../../dev/requirements/REQ-002-control-api-and-console.md)
- クラスターでの書き込み（どのノードに送ってもよい。提案はリーダーへ回る）: [09-cluster-grid.md](09-cluster-grid.md)、[ADR-0009](../../dev/adr/0009-cluster.md)
- 部屋と SSE の部屋の出来事: [10-rooms.md](10-rooms.md)、[ADR-0015](../../dev/adr/0015-realtime-rooms.md)
- メディアパイプライン: [11-media-pipelines.md](11-media-pipelines.md)、[ADR-0016](../../dev/adr/0016-media-pipelines.md)
- トークン・秘密の扱い: [12-security.md](12-security.md)、運用（ログ・監視）: [13-operations.md](13-operations.md)
