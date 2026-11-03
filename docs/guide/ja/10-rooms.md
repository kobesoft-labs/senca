# 10. リアルタイムの部屋

[English](../en/10-rooms.md)

- 関係: [ADR-0015](../../adr/0015-realtime-rooms.md)（リアルタイムの部屋）、[ADR-0013](../../adr/0013-grid.md)（グリッド）、[ADR-0014](../../adr/0014-relay.md)（中継）、[ADR-0016](../../adr/0016-media-pipelines.md)（メディアパイプライン）
- プロトコル: [シグナリング v1](../../realtime/signaling.md)
- 実装: `crates/senca-rooms`（部屋の核）、`crates/senca-node/src/rooms.rs`（ノードへの組み込み）、`crates/senca-node/src/api/rooms.rs`（REST）

部屋（room）は、参加者どうしが WebRTC で映像・音声・データを送り合う場です。次の機能
（role、マルチストリーム、サイマルキャスト、スポットライト、DataChannel のメッセージ、転送フィルター、metadata・client_id・bundle_id、
E2EE の素通し、統計）を持ちます。サーバーは SFU で、受けたものを変換せずに配ります。

## 1. 考え方

- **部屋は制御状態の資源**です。`POST /api/v1/rooms` で作り、どのノードでも読み書きできます。部屋の ID は**テナントをまたいで一意**です。
- **部屋はグリッドの仕事**（`room:{id}`）として、ちょうど 1 つのノードに置かれます（[09. クラスタとグリッド](09-cluster-grid.md)）。
  置かれたノード（hosting node）が自分の部屋の核に部屋を開き、参加者のピア接続を受けます。結果は部屋の `placement_status` に出ます。
- **どのノードにつないでもよい**: シグナリング `GET /signaling` は、最初の `connect` の `room_id` を見て、部屋が別のノードにあれば
  `redirect {location}` を返して閉じます。REST の参加者・メッセージ・統計は、受けたノードが部屋を置いたノードへ中継します。
- **部屋の置き場所が変わったら**（排出・ノードの停止）、元のノードがまだ動いていれば今の参加者に `redirect` を送り、新しいノードへ入り直させます。
- **参加トークン**はどのノードでも発行・検査できます（署名鍵は制御状態の鍵から導くので、全ノードで同じ）。

## 2. 準備（ノードの起動）

部屋を使うノードは WebRTC を有効にします。

| フラグ | 要否 | 意味 |
|---|---|---|
| `--webrtc-udp ADDR` | 必須 | WebRTC の UDP ソケット（WHIP・WHEP と共用）。ICE の候補には**この待ち受けアドレスがそのまま載る**ので、`0.0.0.0` ではなく、クライアントから届く IP を書く |
| `--public-http-url URL` | 推奨 | 部屋・トークンの応答の `signaling_url` の元（`http://`→`ws://`、`https://`→`wss://` に `/signaling` を付ける）。無いと、自分に置いた部屋の `signaling_url` は `null` |
| `--https ADDR` + `--tls-cert`・`--tls-key` | 任意 | `wss://` で受ける（`/signaling` は HTTP と HTTPS の両方の待ち受けで動く） |

```sh
senca-node --data-dir /var/lib/senca --http 0.0.0.0:8080 \
  --webrtc-udp 203.0.113.10:20000 --public-http-url https://media.example.com
```

`--webrtc-udp` の無いノードでは:

- `GET /signaling` は HTTP 503（`text/plain`、`webrtc (--webrtc-udp) is off`）。
- 部屋を置いたノードがこのノードなら、参加者・メッセージ・統計の API は 503 `CAPACITY_UNAVAILABLE`。

**注意（クラスタ）:** グリッドは、ノードで WebRTC が動いているかを配置の条件にしていません。WebRTC の無いノードに部屋が置かれると、
その部屋には誰も入れません。クラスタでは全ノードで `--webrtc-udp` を有効にするか、WebRTC のあるノードに `--label webrtc=on` を付けて
部屋の `placement.labels` で絞ってください。また、redirect と API の中継には、クラスタ設定の各ピアの `api`（管理 API の URL）を使います。
`api` の無いノードに置かれた部屋は、他のノードからは「置き場所が決まっていない」扱いになります（§10）。

## 3. 部屋の REST

すべて `/api/v1` の下で、共通の約束（Bearer、problem+json、`Idempotency-Key`、`ETag`・`If-Match`、`limit`・`cursor`、未知のフィールドは 400）に従います
（[15. API と SDK](15-api-and-sdks.md)）。

| 操作 | メソッドとパス | 権限 | 結果 |
|---|---|---|---|
| 作る | `POST /rooms` `{id?, settings?, metadata?, placement?}` | `stream:write` | 201、`Location`・`ETag` |
| 一覧 | `GET /rooms?limit&cursor` | `stream:read` | 200 `{items, next_cursor}`（ID 順、`limit` 既定 100、1〜1000） |
| 取得 | `GET /rooms/{id}` | `stream:read` | 200、`ETag: "{generation}"` |
| 更新 | `PATCH /rooms/{id}` `{settings?, metadata?, placement?}` | `stream:write` | 200。`If-Match` 必須（無ければ 428、古ければ 412） |
| 削除 | `DELETE /rooms/{id}` | `stream:write` | 204。参加者は `room_deleted` で切れる |
| 参加トークン | `POST /rooms/{id}/tokens` | `stream:write` | 201（§4） |
| 参加者の一覧 | `GET /rooms/{id}/participants?limit&cursor` | `stream:read` | 200 `{items, next_cursor}` |
| 参加者 | `GET /rooms/{id}/participants/{connection_id}` | `stream:read` | 200 |
| 参加者の metadata | `PATCH /rooms/{id}/participants/{connection_id}` `{metadata}` | `stream:write` | 200。全員に `connection.updated` |
| 切断 | `DELETE /rooms/{id}/participants/{connection_id}` | `stream:write` | 204。理由 `kicked` |
| 送る | `POST /rooms/{id}/messages` `{to?, label?, data}` | `stream:write` | 200 `{delivered}`（§8） |
| 統計 | `GET /rooms/{id}/stats` | `stream:read` | 200（§9） |

### 作る

- `id` を省くと 24 文字の 16 進を作ります。`id` は 1〜128 文字の英数字と `-_.:@`。
- 同じ ID の部屋があれば、**どのテナントのものでも** 409 `CONFLICT`。テナントあたり 10000 部屋まで（超えると 403 `QUOTA_EXCEEDED`）。
- `metadata` は任意の JSON（8 KiB まで）。
- `placement` は入力・出力と同じ配置の指定（`node`・`device`・`labels`・`anti_affinity`・`pin_policy`・`priority`・`strategy`。
  `node` はノードの ID か名前で、保存するときに ID に直る）。`degrade` はパイプラインだけなので 400。詳しくは [09](09-cluster-grid.md)。

### 部屋の設定（`settings`）

省いたフィールドは既定値です。未知のフィールドは 400。作成・更新のときに下の範囲をすべて検査します（違反は 400 `INVALID_CONFIG`、`details.errors` にフィールド）。

| フィールド | 既定 | 範囲・意味 |
|---|---|---|
| `max_participants` | 50 | 1〜10000。部屋の参加者の上限。グリッドの見積もり（下）にも使う |
| `e2ee` | false | E2EE の部屋（§11） |
| `video_codec` | `vp8` | `vp8`・`vp9`・`h264`・`av1`。部屋で 1 つ（SFU は変換しないので、全員が同じコーデックで送る） |
| `audio_codec` | `opus` | `opus` だけ |
| `max_video_bitrate_kbps` | なし | 50〜50000。`connected` で送り手に勧める映像の上限 |
| `spotlight` | なし | `{number, unfocused?, focus_rid?}`。`number` は 1〜16、`unfocused` は `lowest`（既定）か `off`、`focus_rid` は英数字 1〜16 文字（§7） |
| `recording` | `none` | `none`・`per_participant`・`composite`。**方針の値だけ**で、ノードはこれを見て録画を始めない。録画はメディアパイプライン（[11](11-media-pipelines.md)）。`e2ee: true` と `composite` は併用できない（400） |
| `forwarding_filter` | なし | 部屋全体の転送フィルター（§6） |
| `data_labels` | `["#*"]` | 使ってよい DataChannel のラベルの型。`#` で始まる 64 byte まで、32 個まで。末尾の `*` は前方一致 |
| `empty_timeout_secs` | 60 | 自動で作った部屋（`auto: true`）が空になってから消すまでの秒数 |

### 更新

- `settings` を送ると**全体を置き換えます**（送らなかったフィールドは既定値に戻る）。変えたいフィールドだけでなく、今の設定を読んで全部送ってください。
- 参加者がいる間は `e2ee`・`video_codec`・`audio_codec` を変えられません（409）。
- `placement: null` で配置の指定を外せます。
- 部屋を置いたノードは、制御状態の世代が変わったのを見て部屋の核の設定を差し替えます。部屋の転送フィルターやスポットライトを変えると、その場で枠を割り当て直します。

### 部屋の表示

`POST`・`GET`・`PATCH` の応答（制御状態の部屋。参加者の数などは含まない）:

| フィールド | 意味 |
|---|---|
| `id`・`generation`・`settings`・`metadata` | |
| `auto` | 参加トークンの発行で自動に作った部屋 |
| `created_at`・`updated_at` | RFC 3339 |
| `signaling_url` | つなぐ先。部屋が別のノードにあればそのノードの URL、そうでなければこのノードの `--public-http-url` から（無ければ `null`） |
| `placement` | 配置の指定 |
| `placement_status` | `state`（`running`・`degraded`・`pending`・`unschedulable`・`not_scheduled`）、置かれていれば `node`・`node_name`・`device`・`level`・`level_label`・`since`・`reason`、置けなければ `reasons` |

### グリッドの見積もり

部屋は `max_participants`（N）から見積もります（`crates/senca-control/src/workloads.rs`）:

| 資源 | 見積もり |
|---|---|
| CPU | 200 + 20 × N ミリコア |
| メモリ | 64 + 2 × N MB |
| 上り | 2 × N Mbit/s |
| 下り | 2 × N × min(N, 8) Mbit/s |

部屋の動いているメディアパイプライン（`desired_state: running`）1 つにつき CPU 20 × N・メモリ 8 × N MB を足します。
既定の 50 人なら CPU 1200 ミリコア・メモリ 164 MB・上り 100・下り 800 Mbit/s です。小さな部屋は `max_participants` を実際の人数に合わせると、
ノードに多く詰められます（ノードの容量は `--net-mbps` など、[09](09-cluster-grid.md)）。

### 部屋の置かれたノードでの問い合わせ

参加者・メッセージ・統計は、部屋を置いたノードの部屋の核が答えます。

- 別のノードに置かれていれば、受けたノードが同じ認証の見出し（`Authorization`・`Content-Type`・`If-Match`・`Idempotency-Key`・`X-Request-Id`）のまま
  そのノードの `/api/v1` へ中継し、状態コード・本文・`ETag` をそのまま返します（10 秒で時間切れ）。**中継の中継はしません**（503、`retryable: true`）。
- 置き場所がまだ決まっていない（作った直後など）: 503 `CAPACITY_UNAVAILABLE`（`retryable: true`）。
- 部屋の核が 5 秒応えない: 503（`retryable: true`）。
- 部屋の核がまだ部屋を開いていない: 404。
- 時刻（`joined_at`・`at` など）は RFC 3339 で返します。

## 4. 参加トークン

`POST /rooms/{id}/tokens`:

| フィールド | 既定 | 意味 |
|---|---|---|
| `role` | 必須 | `sendrecv`・`sendonly`・`recvonly`。参加できる役割の上限（`sendrecv` ならどれでも） |
| `client_id` | なし | 1〜255 文字、制御文字なし |
| `bundle_id` | なし | 1〜255 文字、制御文字なし |
| `metadata` | なし | 他の参加者に見せる metadata（8 KiB まで） |
| `permissions` | 下の既定 | `{publish, subscribe, data}` |
| `ttl_secs` | 600 | 10〜86400 |

応答（201）:

```json
{"token":"eyJ…","room_id":"daily","expires_at":"2026-11-03T10:10:00Z","signaling_url":"wss://media.example.com/signaling"}
```

- **無い部屋のトークンを頼むと、部屋を自動で作ります**（既定の設定、`auto: true`）。その部屋は、空のまま `empty_timeout_secs`（既定 60 秒）経つと消えます
  （部屋を置いたノードが 5 秒ごとに見る。参加者がいないまま作っただけの部屋も消える）。他のテナントの部屋の ID なら 404。
- トークンは JWT 互換で Ed25519（`EdDSA`、`kid: senca-rooms-1`）で署名し、`iss: "senca"`、`aud: "senca-rooms"`、`sub` にテナントが入ります。
- 署名鍵がまだ無い（クラスタの初期化の前）ときは 503（`retryable: true`）。

### 許可（`permissions`）

| フィールド | 既定 | 意味 |
|---|---|---|
| `publish` | `["audio","video"]` | 送ってよいトラックの種類。無い種類を送ると `forbidden` |
| `subscribe` | true | 他の参加者のトラックを受けてよいか |
| `data` | `["#*"]` | 使ってよい DataChannel のラベルの型 |

### 検査の規則

参加のとき（`connect`）に部屋の核が検査します。

- **トークンは必須**です。ノードは認証 Webhook も匿名の参加も使わない設定なので、トークンの無い `connect` は `auth_failed`（§12）。
- `exp` は必須で、時計のずれは 30 秒まで。今から `exp` までが 24 時間を超えるものは拒否。`iat` が未来すぎるものも拒否。
- トークンの `room` と `connect.room_id` が違えば `forbidden`。`connect.role` がトークンの `role` の範囲を超えれば `forbidden`。
- **`client_id`・`bundle_id` は名乗った値を信じません**（転送フィルターの条件になるため）:
  - トークンに `client_id` があれば、それを使います。`connect.client_id` を付けるなら同じ値でなければ `forbidden`。
  - トークンに `client_id` が無いのに `connect.client_id` を付けると `forbidden`（`client_id must be issued in the token`）。
  - `bundle_id` も同じ（トークンに無いのに `connect.bundle_id` を付けると `forbidden`）。トークンにも無ければ `bundle_id` は `connection_id` と同じ値。
- 参加者の metadata はトークンの `metadata`。`connect.metadata` は他の参加者には見せません（ノードでは使い道がない。認証 Webhook へ渡すための欄）。

## 5. シグナリング

WebSocket の `GET /signaling`（subprotocol **`senca.rooms.v1`** が必須）に JSON の message を載せます。詳しい形は
[シグナリング v1](../../realtime/signaling.md) にあります。ここではノードでの流れだけを書きます。

1. アプリのサーバーが `POST /rooms/{id}/tokens` でトークンを作り、`token` と `signaling_url` をブラウザーへ渡す。
2. クライアントが `signaling_url` へつなぎ、10 秒以内に `connect {version: 1, room_id, token, role, ...}` を送る（HTTP の `Authorization` は使わない）。
3. 受けたノードは部屋の置き場所を見る（作ったばかりの部屋は 3 秒まで置き場所が決まるのを待つ）:
   - 別のノード: `{"type":"redirect","location":"ws://…/signaling"}` を送り、close code 4300 で閉じる。クライアントはその URL へつなぎ直す。
   - 置き場所が決まらない: `{"type":"error","code":"capacity_unavailable","fatal":true,…}` を送り、close code 4503 で閉じる。
   - このノード: 部屋の核が `connect` を処理し、`connected` を返す。
4. クライアントが offer を出し（**offer は常にクライアント**、サーバーは ICE-lite で answer だけ）、ICE・DTLS の後にメディアが流れる。
5. サーバーは 10 秒ごとに `ping` を送り、10 秒以内に `pong` が無ければ `ping_timeout` で切ります。

その他の close code:

| close code | いつ |
|---|---|
| 4408 | WebSocket を開いてから 10 秒以内に最初の message が無い |
| 1013 | 部屋の核がまだ動けない（署名鍵が無い、クラスタの初期化の前） |
| 4300 | 上の redirect |
| 4503 | 上の `capacity_unavailable`、または `room_full` |

それ以外の誤りの code・close code・切断の理由は [シグナリング v1 §7](../../realtime/signaling.md) の表のとおりです。
部屋が無い（制御状態に無い、または部屋の核がまだ開いていない）ときは `room_not_found`（ノードは参加で部屋を作らない）。

## 6. マルチストリーム・サイマルキャスト・層の選択・転送フィルター

- **受信の枠**: クライアントの受ける m 行 1 つが枠 1 つ。サーバーは部屋のトラックを枠に割り当て、足りなければ `renegotiate` を送ります。
  枠は種類ごとに 32 まで。自分のトラック・同じ `bundle_id` の接続のトラックは受けません。`multistream: false` なら種類ごとに 1 本。
- **サイマルキャスト**: `connect.simulcast {rids, default_rid?}` で送る層の rid を**低い順**に 1〜4 個（英数字 1〜16 文字、重複不可）。
- **層の選択**（受け手ごと・トラックごと）。優先の順:
  1. クライアントの `layer {track_id, rid}`（`rid: null` で自動に戻す）
  2. スポットライト（§7）
  3. `simulcast.default_rid`
  4. 自動: 推定帯域から音声の分 64 kbit/s を引き、0.9 を掛けて映像の購読の数で分け、収まる最も高い層。
     下げるのは足りない状態が 1 秒続いてから（推定が必要量の 0.6 倍を下回ればすぐ）、上げるのは収まる状態が 2 秒続いてから。
  切り替えは新しい層のキーフレームから（それまで今の層を配る）。今受けている層は `notify {event: "layer.changed"}` で分かります。
- **転送フィルター**: 部屋の `settings.forwarding_filter` と接続ごとの `connect.forwarding_filter` の**両方を満たすもの**だけ配ります。

```json
{"action":"block","rules":[[{"field":"kind","operator":"is_in","values":["video"]},
                            {"field":"client_id","operator":"is_not_in","values":["teacher"]}]]}
```

（先生以外の映像を受けない）。`rules` は AND の組の OR（1〜10 組、各 1〜10 規則）。`field` は**送り手**の `connection_id`・`client_id`・`bundle_id`・`kind`・
`metadata.<key>`（キーは 64 byte まで、値が文字列・数・真偽のとき）。`operator` は `is_in`・`is_not_in`、`values` は 1〜100 個（各 255 byte まで、`kind` なら `audio`・`video`）。
`action` は `block`（一致したものを配らない）か `allow`（一致したものだけ配る）。

## 7. スポットライト

部屋の `settings.spotlight` で有効にします。話者を音量で検出し、注目した `number` 人の映像を高い層で、それ以外を一番低い層
（`unfocused: "lowest"`）で配るか、配りません（`"off"`。枠は残し `paused`）。

- 話者の検出: RFC 6464 の音量（無ければ Opus の packet の大きさからの見積もり）。200 ms 続けば話している、1.5 秒黙れば話していない、
  注目してから 2 秒は外さない。注目の組は部屋で 1 つ（`spotlight.changed`）、受け手からは自分を除いた `number` 人。
- 注目した送り手の層は `focus_rid`（無ければ帯域で自動）。
- クライアントは `connect.spotlight {number, focus_rid?, unfocused?}` で部屋の範囲の中で選べます（`number` は部屋の値以下に切り詰め）。

## 8. メッセージ（DataChannel・プッシュ）

- **DataChannel**: ラベルが `#` で始まる DataChannel。`connect.data_channels` で宣言し、クライアントが開きます。部屋の `data_labels` と
  トークンの `permissions.data` の両方に合うラベルだけ受け付けます。届いた message は、同じラベルを受ける向きで開いている**他の全員**へそのまま配ります。
- **1 人へ**: シグナリングの `message {label, to: [connection_id], data}`。受け手には `from` 付きで届きます。
- **API から**: `POST /rooms/{id}/messages`:

| フィールド | 意味 |
|---|---|
| `to` | connection_id の並び（省くか空なら全員）。部屋にいない ID があれば 404 |
| `label` | あれば、そのラベルの DataChannel を開いている参加者へ送る（`#` で始まること、`data` は文字列であること） |
| `data` | JSON（64 KiB まで）。`label` が無ければシグナリングの `push {data}` として届く |

応答は `{"delivered": N}`（届けた参加者の数）。

- 上限: アプリの message は 64 KiB、シグナリングの message は 256 KiB、1 接続あたり 1 秒に 50 message（超えると `rate_limited`、4 倍を超えると切断）。

## 9. 参加者と統計

参加者（`GET …/participants`）: `connection_id`・`client_id`・`bundle_id`・`role`・`metadata`・`tracks[] {track_id, connection_id, kind, rids?}`・
`joined_at`・`peer_connected`・`signaling`（`websocket`・`datachannel`）・`subscriptions[]`（枠: `mid`・`kind`・`state`（`active`・`paused`・`empty`）・`track_id`・`connection_id`・`client_id`）。

統計（`GET …/stats`）: `{room_id, at, participants[]}`。参加者ごとに `connection_id`・`rtt_ms`・`bandwidth_bps`（推定帯域）・`send_bitrate_bps`・
`retransmitted_packets`・`nack_requested`・`received_bytes`・`forwarded_bytes`・`forwarded_frames`・`published[] {track_id, kind, layers[] {rid, bitrate_bps}}`・
`subscriptions[] {mid, track_id, state, rid}`・`client`（クライアントが `pong.stats`・`stats` で送った getStats の要約）。

## 10. 部屋の移動と障害

- **排出**（`POST /nodes/{id}/drain`）や**ノードの停止**でグリッドが部屋を別のノードへ移すと、元のノードは今の参加者に `redirect` を送り
  （`disconnect {reason: "redirected"}` の後に閉じる）、部屋を手放します。新しいノードが部屋を開き、参加者は入り直します（connection_id・トラック ID は新しくなる）。
- **元のノードが落ちた**ときは redirect を送れません。参加者のピア接続とシグナリングが切れ、クライアントが入り直します。入り直す先は
  どのノードでもよい（新しい置き場所へ redirect される）ので、SDK の `url` に複数のノードを書いておくと、落ちたノードを飛ばせます。
  グリッドが移すまでの時間は [09](09-cluster-grid.md) を見てください。
- 移る前の部屋の状態（参加者・トラック・スポットライト）は引き継ぎません。新しいノードでは空の部屋から始まります。

## 11. E2EE（素通し）

`settings.e2ee: true` の部屋では、クライアントが SFrame などで暗号化したメディアを、SFU は中身を見ずに配ります。

- 参加者は `connect.e2ee` を部屋の `e2ee` と同じにします（違えば `e2ee_mismatch`）。参加者がいる間は `e2ee` を変えられません。
- 暗号化・復号と鍵の配り方はアプリの側です。ブラウザー SDK は部屋の設定との照合だけをし、暗号化（Encoded Transform）は持ちません。
- サーバーは中身を見られないので、合成・合成の録画・文字起こしは意味を持ちません。`recording: "composite"` との併用は 400 です。
  **ノードのメディアパイプラインの API は、E2EE の部屋かどうかをまだ検査しません**（合成・字幕を作れてしまう）。E2EE の部屋には、参加者ごとの書き出し・録画（暗号のまま）だけを使ってください。

## 12. イベント（SSE）

`GET /api/v1/events`（[15](15-api-and-sdks.md)）に部屋の出来事が流れます。

制御の出来事（合意ログに載る。SSE の `id` あり、`Last-Event-ID` で再開できる）:

| `event` | いつ | `data` |
|---|---|---|
| `room.updated` | 部屋の作成・自動の作成・更新 | `{auto}` |
| `room.deleted` | 部屋の削除（自動の部屋の掃除も） | `{}` |

部屋の核の出来事（**部屋を置いたノードの購読者にだけ**流れる。SSE の `id` を持たず、`Last-Event-ID` の再開の対象外。取りこぼしは飛ばす）:

| `event` | いつ |
|---|---|
| `room.started` | 部屋の核が部屋を開いた |
| `room.stopped` | 部屋の核が部屋を閉じた（削除・別のノードへの移動） |
| `room.connection.created` | 参加した |
| `room.connection.updated` | 参加者の metadata が変わった |
| `room.connection.destroyed` | 退出した（`reason`・`duration_ms`） |
| `room.track.published` | トラックを公開した |
| `room.track.unpublished` | トラックをやめた |
| `room.spotlight.changed` | 注目の組が変わった |

部屋の核の出来事の `data` の形:

```json
{"type":"room.connection.destroyed","resource_id":"daily","event_id":"…","node":"…","occurred_at":"2026-11-03T10:00:00Z",
 "data":{"connection_id":"…","client_id":"alice","reason":"normal","duration_ms":61234}}
```

部屋の核の出来事を漏れなく受けるには、部屋を置いたノード（`placement_status.node`）の SSE を聞いてください。テナントで絞るので、他のテナントの部屋の出来事は流れません。

## 13. 上限

| 項目 | 値 |
|---|---|
| 部屋の参加者 | `max_participants`（既定 50、1〜10000）。超えると `room_full` |
| ノードの参加者・部屋 | 部屋の核の既定で、それぞれ 10000（超えると `room_full`） |
| テナントの部屋 | 10000 |
| 受信の枠 | 種類ごとに 32 |
| サイマルキャストの層 | 1〜4 |
| metadata | 8 KiB（部屋・参加者・トークン） |
| アプリの message | 64 KiB |
| シグナリングの message | 256 KiB、1 秒に 50 |
| トークンの寿命 | 10〜86400 秒（既定 600） |
| `connect` までの期限 | 10 秒 |
| ping | 10 秒ごと、`pong` の期限 10 秒 |
| ピア接続の期限 | 参加から 30 秒つながらなければ `peer_failed` |

## 14. ノードでまだ使えないもの

部屋の核（`senca-rooms`）とシグナリング v1 の文書にはあっても、`senca-node` ではまだ使えないものがあります。

- **認証 Webhook**: ノードは部屋の核を `auth_webhook` なしで動かします（トークンで認める）。参加の可否を外部に問い合わせることはできません。
  （仮に部屋の核が問い合わせを出しても、ノードは応答なしとして拒否します: `crates/senca-node/src/rooms.rs` の `drive()`。）
  したがって `connect.metadata` はどこにも使われず、トークンなしの参加もできません。
- **イベントの Webhook**: 部屋の出来事を外部へ POST する仕組み（`senca_rooms::webhook::Outbox`）はノードに組み込まれていません。出来事は SSE だけです。
- **カスケード（部屋を複数のノードにまたがせる）**: 有効ではありません。部屋は 1 つのノードに留まり、他のノードにつないだ参加者は redirect されます。
  したがって 1 つの部屋の大きさは 1 つのノードの容量まで、参加者の `node` 欄（別のノードの参加者）は現れません。
- **ノードの `overflow_redirect`**: 設定できません（ノードが満員なら `room_full`）。
- **`settings.recording`**: 方針の値だけで、録画は始まりません（[11](11-media-pipelines.md) のメディアパイプラインを使う）。
- **参加者が自分の metadata を変える message**: シグナリング v1 に無いので、アプリのサーバーから `PATCH /rooms/{id}/participants/{connection_id}` で変えます。
- **再開（resume）**: 切れたら新しい接続として入り直します。

## 15. 例

### curl

```sh
API=https://media.example.com/api/v1
H="Authorization: Bearer $SENCA_TOKEN"

# 部屋を作る
curl -s -X POST -H "$H" -H 'Content-Type: application/json' "$API/rooms" \
  -d '{"id":"demo","settings":{"max_participants":10,"video_codec":"vp8","spotlight":{"number":2}},"metadata":{"title":"standup"}}'

# 置かれたか（placement_status.state が running）
curl -s -H "$H" "$API/rooms/demo"

# 参加トークン（client_id はトークンに入れる）
curl -s -X POST -H "$H" -H 'Content-Type: application/json' "$API/rooms/demo/tokens" \
  -d '{"role":"sendrecv","client_id":"alice","metadata":{"name":"Alice"},"ttl_secs":600}'

# 参加者・統計
curl -s -H "$H" "$API/rooms/demo/participants"
curl -s -H "$H" "$API/rooms/demo/stats"

# 全員へプッシュ / #chat の DataChannel へ
curl -s -X POST -H "$H" -H 'Content-Type: application/json' "$API/rooms/demo/messages" -d '{"data":{"notice":"5 分後に終わります"}}'
curl -s -X POST -H "$H" -H 'Content-Type: application/json' "$API/rooms/demo/messages" -d '{"label":"#chat","data":"hello"}'

# 設定を変える（全体を置き換える。If-Match に今の ETag）
curl -s -X PATCH -H "$H" -H 'Content-Type: application/json' -H 'If-Match: "1"' "$API/rooms/demo" \
  -d '{"settings":{"max_participants":20,"video_codec":"vp8","spotlight":{"number":2}}}'

# 1 人を切る / 部屋を消す
curl -s -X DELETE -H "$H" "$API/rooms/demo/participants/$CONNECTION_ID"
curl -s -X DELETE -H "$H" "$API/rooms/demo"
```

`tools/interop/node_rooms.sh` が、この流れを Chrome 3 ページで確かめる試験です（REST で部屋を作り、トークンで入り、互いの映像の復号・
DataChannel の往復・サイマルキャストの層の切り替え・REST の参加者・統計・プッシュ・SSE の出来事を見る）。

### ブラウザー SDK（`@senca/sdk`）

```js
import { Senca } from '@senca/sdk';

const camera = await navigator.mediaDevices.getUserMedia({ video: { width: 1280, height: 720 }, audio: true });
const session = await Senca.connect({
  url: 'wss://media.example.com/signaling',   // トークンの応答の signaling_url（複数書くと入り直すときに順に替える）
  room: 'demo',
  token: () => fetch('/my-backend/room-token').then((r) => r.text()),   // 入り直すたびに取り直す
  publish: { stream: camera, simulcast: true },
  dataChannels: ['#chat'],
});

for await (const track of session.remoteTracks()) {
  const video = track.attach(document.createElement('video'));
  document.body.append(video);
  track.ended.then(() => video.remove());
}
```

```js
await session.send('#chat', { text: 'hello' });                  // 全員（DataChannel）
for await (const m of session.messages('#chat')) console.log(m.sender?.clientId, m.json());
const t = [...session.tracks.values()].find((t) => t.kind === 'video');
await t.setLayer('r0');                                          // 'r0' | 'r1' | 'r2' | 'auto'
await session.close();
```

SDK は SDP・受信の枠・再交渉・ping・redirect（3 回まで）・再接続（0.5 秒から倍々で最大 10 秒、10 回まで）を中に隠します。
オプションと出来事の一覧は `apps/sdk-js/README.md` と [15. API と SDK](15-api-and-sdks.md) にあります。
