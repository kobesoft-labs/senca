# 10. リアルタイムの部屋

[English](../en/10-rooms.md)

部屋（ルーム）は、参加者どうしが WebRTC で映像・音声・データを送受信するための場です。
SENCA は SFU として動作し、受信したメディアを変換せずにそのまま他の参加者へ転送します。
次の機能を備えています: role、マルチストリーム、サイマルキャスト、スポットライト、
DataChannel のメッセージ、転送フィルター、metadata・client_id・bundle_id、E2EE のパススルー、統計。

- 設計の背景: [ADR-0015](../../dev/adr/0015-realtime-rooms.md)（リアルタイムの部屋）、[ADR-0013](../../dev/adr/0013-grid.md)（グリッド）、[ADR-0014](../../dev/adr/0014-relay.md)（中継）、[ADR-0016](../../dev/adr/0016-media-pipelines.md)（メディアパイプライン）
- プロトコルの仕様: [シグナリング v1](../../realtime/signaling.md)

## 10.1 基本の考え方

- **部屋は制御状態のリソースです。** `POST /api/v1/rooms` で作成し、どのノードからでも読み書きできます。部屋の ID は**テナントをまたいで一意**です。
- **部屋はグリッドのワークロード**（`room:{id}`）として、ちょうど 1 つのノードに配置されます（[9. クラスターとグリッド](09-cluster-grid.md)）。
  配置されたノードが部屋を開き、参加者のピア接続を受け付けます。配置の結果は部屋の `placement_status` に表示されます。
- **どのノードに接続してもかまいません。** シグナリングの `GET /signaling` は、最初の `connect` の `room_id` を見て、部屋が別のノードにあれば
  `redirect {location}` を返して接続を閉じます。参加者・メッセージ・統計の REST API は、受け付けたノードが部屋を配置したノードへ転送します。
  `cascade: true` の部屋では redirect せず、接続したノードで参加者を受け付けます（10.10 のカスケード）。
- **部屋の配置先が変わったとき**（ドレインやノードの停止）は、元のノードがまだ動いていれば、現在の参加者に `redirect` を送って新しいノードへ入り直させます。
- **参加トークン**は、どのノードでも発行・検証できます（署名鍵は制御状態の鍵から導出するため、全ノードで同じです）。

## 10.2 準備（ノードの起動）

部屋を使うノードでは WebRTC を有効にします。

| フラグ | 要否 | 意味 |
|---|---|---|
| `--webrtc-udp ADDR` | 必須 | WebRTC の UDP ソケット（WHIP・WHEP と共用）。ICE の候補には**この待ち受けアドレスがそのまま載る**ので、`0.0.0.0` ではなく、クライアントから到達できる IP アドレスを書いてください |
| `--public-http-url URL` | 推奨 | 部屋・トークンの応答に含まれる `signaling_url` の元になります（`http://` は `ws://`、`https://` は `wss://` に変え、`/signaling` を付けます）。指定しないと、自ノードに配置された部屋の `signaling_url` は `null` になります |
| `--https ADDR` + `--tls-cert`・`--tls-key` | 任意 | `wss://` で受け付けます（`/signaling` は HTTP と HTTPS の両方の待ち受けで動作します） |

```sh
senca-node --data-dir /var/lib/senca --http 0.0.0.0:8080 \
  --webrtc-udp 203.0.113.10:20000 --public-http-url https://media.example.com
```

`--webrtc-udp` を指定していないノードでは、次のようになります。

- `GET /signaling` は HTTP 503（`text/plain`、`webrtc (--webrtc-udp) is off`）を返します。
- 部屋がこのノードに配置されている場合、参加者・メッセージ・統計の API は 503 `CAPACITY_UNAVAILABLE` を返します。

**クラスターでの注意:** グリッドは、ノードで WebRTC が有効かどうかを配置の条件にしていません。WebRTC が無効なノードに部屋が配置されると、
その部屋には誰も参加できません。クラスターでは全ノードで `--webrtc-udp` を有効にするか、WebRTC が有効なノードに `--label webrtc=on` を付けて、
部屋の `placement.labels` で配置先を絞ってください。また、redirect と API の転送には、クラスター設定の各ピアの `api`（管理 API の URL）を使います。
`api` が設定されていないノードに配置された部屋は、他のノードからは「配置先が決まっていない」ものとして扱われます（10.3 の「部屋を配置したノードへの問い合わせ」）。

## 10.3 部屋の REST API

すべて `/api/v1` の下にあり、共通の規約（Bearer、problem+json、`Idempotency-Key`、`ETag`・`If-Match`、`limit`・`cursor`、未知のフィールドは 400）に従います
（[15. API と SDK](15-api-and-sdks.md)）。

| 操作 | メソッドとパス | 権限 | 結果 |
|---|---|---|---|
| 作成 | `POST /rooms` `{id?, settings?, metadata?, placement?}` | `stream:write` | 201、`Location`・`ETag` |
| 一覧 | `GET /rooms?limit&cursor` | `stream:read` | 200 `{items, next_cursor}`（ID 順。`limit` の既定は 100、1〜1000） |
| 取得 | `GET /rooms/{id}` | `stream:read` | 200、`ETag: "{generation}"` |
| 更新 | `PATCH /rooms/{id}` `{settings?, metadata?, placement?}` | `stream:write` | 200。`If-Match` 必須（無ければ 428、古ければ 412） |
| 削除 | `DELETE /rooms/{id}` | `stream:write` | 204。参加者は `room_deleted` で切断されます |
| 参加トークン | `POST /rooms/{id}/tokens` | `stream:write` | 201（10.4） |
| 参加者の一覧 | `GET /rooms/{id}/participants?limit&cursor` | `stream:read` | 200 `{items, next_cursor}` |
| 参加者の取得 | `GET /rooms/{id}/participants/{connection_id}` | `stream:read` | 200 |
| 参加者の metadata の変更 | `PATCH /rooms/{id}/participants/{connection_id}` `{metadata}` | `stream:write` | 200。全員に `connection.updated` が通知されます |
| 参加者の切断 | `DELETE /rooms/{id}/participants/{connection_id}` | `stream:write` | 204。理由は `kicked` |
| メッセージの送信 | `POST /rooms/{id}/messages` `{to?, label?, data}` | `stream:write` | 200 `{delivered}`（10.8） |
| 統計 | `GET /rooms/{id}/stats` | `stream:read` | 200（10.9） |

### 作成

- `id` を省略すると、24 文字の 16 進数の ID を生成します。`id` に使えるのは、1〜128 文字の英数字と `-_.:@` です。
- 同じ ID の部屋があれば、**どのテナントの部屋であっても** 409 `CONFLICT` になります。1 つのテナントで作成できる部屋は 10000 個までです（超えると 403 `QUOTA_EXCEEDED`）。
- `metadata` には任意の JSON（8 KiB まで）を指定できます。
- `placement` は入力・出力と同じ配置の指定です（`node`・`device`・`labels`・`anti_affinity`・`pin_policy`・`priority`・`strategy`。
  `node` にはノードの ID か名前を指定でき、保存時に ID に変換されます）。`degrade` はパイプライン専用のため、指定すると 400 になります。
  詳しくは [9. クラスターとグリッド](09-cluster-grid.md) を参照してください。

### 部屋の設定（`settings`）

省略したフィールドには既定値が使われます。未知のフィールドは 400 です。作成時と更新時に、下の範囲をすべて検査します
（違反すると 400 `INVALID_CONFIG`。`details.errors` に該当するフィールドが入ります）。

| フィールド | 既定 | 範囲・意味 |
|---|---|---|
| `max_participants` | 50 | 1〜10000。部屋の参加者数の上限。グリッドでの資源の見積もり（下記）にも使います |
| `e2ee` | false | E2EE の部屋にします（10.11） |
| `video_codec` | `vp8` | `vp8`・`vp9`・`h264`・`av1`。部屋で 1 つです（SFU は変換しないため、全員が同じコーデックで送信します） |
| `audio_codec` | `opus` | `opus` のみ |
| `max_video_bitrate_kbps` | なし | 50〜50000。`connected` で配信者に推奨する映像のビットレートの上限 |
| `spotlight` | なし | `{number, unfocused?, focus_rid?}`。`number` は 1〜16、`unfocused` は `lowest`（既定）か `off`、`focus_rid` は 1〜16 文字の英数字（10.7） |
| `recording` | `none` | `none`・`per_participant`・`composite`。**方針を示す値で、この値によってノードが録画を始めることはありません**。録画にはメディアパイプライン（[11](11-media-pipelines.md)）を使います。`e2ee: true` と `composite` は併用できません（400） |
| `forwarding_filter` | なし | 部屋全体の転送フィルター（10.6） |
| `data_labels` | `["#*"]` | 使用を許可する DataChannel のラベルのパターン。`#` で始まる 64 バイトまでの文字列を 32 個まで。末尾の `*` は前方一致 |
| `empty_timeout_secs` | 60 | 自動で作成された部屋（`auto: true`）が空になってから削除されるまでの秒数 |
| `cascade` | false | true にすると、部屋の配置先でないノードに接続した参加者も、そのノードで受け付けます（10.10 のカスケード）。false なら配置先へ redirect します |
| `auth_webhook_url` | なし | この部屋の認証 Webhook の URL（`http://`・`https://`）。ノードの `--rooms-auth-webhook` より優先されます（10.4） |

### 更新

- `settings` を送ると、**設定全体を置き換えます**（送らなかったフィールドは既定値に戻ります）。変えたいフィールドだけでなく、現在の設定を読み込んで全体を送ってください。
- 参加者がいる間は、`e2ee`・`video_codec`・`audio_codec` を変更できません（409）。
- `placement: null` で配置の指定を解除できます。
- 部屋を配置したノードは、制御状態の世代が変わったことを検知して設定を反映します。部屋の転送フィルターやスポットライトを変更すると、その場で受信スロットを割り当て直します。

### 部屋の表示

`POST`・`GET`・`PATCH` の応答には、制御状態に保存された部屋の情報が含まれます（参加者数などは含みません）。

| フィールド | 意味 |
|---|---|
| `id`・`generation`・`settings`・`metadata` | |
| `auto` | 参加トークンの発行時に自動で作成された部屋かどうか |
| `created_at`・`updated_at` | RFC 3339 |
| `signaling_url` | 接続先。部屋が別のノードにあればそのノードの URL、そうでなければこのノードの `--public-http-url` から作った URL（無ければ `null`） |
| `placement` | 配置の指定 |
| `placement_status` | `state`（`running`・`degraded`・`pending`・`unschedulable`・`not_scheduled`）。配置済みなら `node`・`node_name`・`device`・`level`・`level_label`・`since`・`reason`、配置できなければ `reasons` |

### グリッドでの資源の見積もり

部屋に必要な資源は、`max_participants`（N）から見積もります。

| 資源 | 見積もり |
|---|---|
| CPU | 200 + 20 × N ミリコア |
| メモリ | 64 + 2 × N MB |
| 受信 | 2 × N Mbit/s |
| 送信 | 2 × N × min(N, 8) Mbit/s |

部屋で動作中のメディアパイプライン（`desired_state: running`）1 つにつき、CPU 20 × N ミリコア・メモリ 8 × N MB を加算します。
既定の 50 人なら、CPU 1200 ミリコア・メモリ 164 MB・受信 100 Mbit/s・送信 800 Mbit/s です。小さな部屋では `max_participants` を実際の人数に合わせると、
1 つのノードにより多くの部屋を配置できます（ノードの容量は `--net-mbps` などで設定します。[9. クラスターとグリッド](09-cluster-grid.md)）。

### 部屋を配置したノードへの問い合わせ

参加者・メッセージ・統計の API には、部屋を配置したノードが応答します。

- 部屋が別のノードに配置されている場合、リクエストを受けたノードは、同じヘッダー（`Authorization`・`Content-Type`・`If-Match`・`Idempotency-Key`・`X-Request-Id`）のまま
  配置先のノードの `/api/v1` へ転送し、ステータスコード・本文・`ETag` をそのまま返します（タイムアウトは 10 秒）。**転送されたリクエストをさらに転送することはありません**（503、`retryable: true`）。
- 配置先がまだ決まっていない場合（作成直後など）: 503 `CAPACITY_UNAVAILABLE`（`retryable: true`）。
- 部屋の処理が 5 秒以内に応答しない場合: 503（`retryable: true`）。
- 配置先のノードがまだ部屋を開いていない場合: 404。
- 時刻（`joined_at`・`at` など）は RFC 3339 で返します。

## 10.4 参加トークン

`POST /rooms/{id}/tokens`:

| フィールド | 既定 | 意味 |
|---|---|---|
| `role` | 必須 | `sendrecv`・`sendonly`・`recvonly`。参加時に選べる役割の上限（`sendrecv` ならどれでも選べます） |
| `client_id` | なし | 1〜255 文字、制御文字は不可 |
| `bundle_id` | なし | 1〜255 文字、制御文字は不可 |
| `metadata` | なし | 他の参加者に公開する metadata（8 KiB まで） |
| `permissions` | 下の既定値 | `{publish, subscribe, data}` |
| `ttl_secs` | 600 | 10〜86400 |

応答（201）:

```json
{"token":"eyJ…","room_id":"daily","expires_at":"2026-11-03T10:10:00Z","signaling_url":"wss://media.example.com/signaling"}
```

- **存在しない部屋のトークンを発行すると、部屋を自動で作成します**（既定の設定で、`auto: true`）。この部屋は、空のまま `empty_timeout_secs`（既定 60 秒）が過ぎると削除されます
  （部屋を配置したノードが 5 秒ごとに確認します。作成しただけで誰も参加していない部屋も削除されます）。他のテナントの部屋の ID を指定すると 404 です。
- トークンは JWT 互換の形式で、Ed25519（`EdDSA`、`kid: senca-rooms-1`）で署名されます。`iss: "senca"`、`aud: "senca-rooms"` で、`sub` にはテナントが入ります。
- 署名鍵がまだ無い場合（クラスターの初期化前）は 503（`retryable: true`）です。

### 権限（`permissions`）

| フィールド | 既定 | 意味 |
|---|---|---|
| `publish` | `["audio","video"]` | 送信を許可するトラックの種類。許可されていない種類を送ると `forbidden` |
| `subscribe` | true | 他の参加者のトラックを受信できるかどうか |
| `data` | `["#*"]` | 使用を許可する DataChannel のラベルのパターン |

### 検証の規則

参加時（`connect`）に、部屋を配置したノードが次の規則で検証します。

- **トークンは必須です**（既定）。トークンの無い `connect` は `auth_failed` になります（[シグナリング v1 §7](../../realtime/signaling.md)）。
  `--rooms-auth-mode only` のときだけ、認証 Webhook がトークンの代わりに参加を認めます。
- `exp` は必須で、許容する時計のずれは 30 秒までです。現在から `exp` までが 24 時間を超えるトークンは拒否します。`iat` が未来すぎるトークンも拒否します。
- トークンの `room` と `connect.room_id` が違えば `forbidden` です。`connect.role` がトークンの `role` の範囲を超えても `forbidden` です。
- **`client_id`・`bundle_id` は、クライアントが自分で名乗った値を信用しません**（転送フィルターの条件に使われるためです）。
  - トークンに `client_id` があれば、それを使います。`connect.client_id` を付ける場合は、同じ値でなければ `forbidden` です。
  - トークンに `client_id` が無いのに `connect.client_id` を付けると `forbidden`（`client_id must be issued in the token`）です。
  - `bundle_id` も同じです（トークンに無いのに `connect.bundle_id` を付けると `forbidden`）。トークンにも無ければ、`bundle_id` は `connection_id` と同じ値になります。
- 参加者の metadata には、トークンの `metadata` を使います。`connect.metadata` は他の参加者には公開されません（認証 Webhook に渡すためのフィールドです）。

### 認証 Webhook

部屋の設定 `auth_webhook_url`、それが無ければノードの `--rooms-auth-webhook` に URL が設定されていると、ノードはトークンを検証した後に、
その URL へ署名付きの `POST` で問い合わせます（本文と応答の形式は [シグナリング v1](../../realtime/signaling.md) §9）。

- 応答が `allowed: false` なら `auth_failed` になります（`reason` はクライアントに届きます）。応答で `role`・`client_id`・`metadata`・
  `permissions` を上書きできます。5 秒以内に応答が無い場合や、2xx 以外の応答も拒否として扱います。
- 署名用のシークレットは `--rooms-webhook-secret-file` で渡します（16 バイト以上。`Senca-Signature` ヘッダーに使います）。シークレットが設定されていないノードでは、
  Webhook の URL を持つ部屋への参加を拒否します。
- 宛先は `--egress-allow` の方針で検査します。社内のアドレスへ送る場合は許可してください。
- どちらの URL も設定されていない部屋では、従来どおりトークンだけで参加を認めます。

## 10.5 シグナリング

シグナリングは、WebSocket の `GET /signaling`（subprotocol **`senca.rooms.v1`** が必須）で JSON のメッセージをやり取りします。
メッセージの詳しい形式は [シグナリング v1](../../realtime/signaling.md) にあります。ここではノードでの処理の流れだけを説明します。

1. アプリケーションのサーバーが `POST /rooms/{id}/tokens` でトークンを発行し、`token` と `signaling_url` をブラウザーへ渡します。
2. クライアントが `signaling_url` へ接続し、10 秒以内に `connect {version: 1, room_id, token, role, ...}` を送ります（HTTP の `Authorization` ヘッダーは使いません）。
3. 接続を受けたノードは、部屋の配置先を確認します（作成直後の部屋では、配置先が決まるまで最大 3 秒待ちます）。
   - 別のノードの場合: `{"type":"redirect","location":"ws://…/signaling"}` を送り、close code 4300 で閉じます。クライアントはその URL へ接続し直します。
   - 配置先が決まらない場合: `{"type":"error","code":"capacity_unavailable","fatal":true,…}` を送り、close code 4503 で閉じます。
   - このノードの場合: `connect` を処理し、`connected` を返します。
4. クライアントが offer を送ります（**offer は常にクライアントが送ります**。サーバーは ICE-lite で、answer だけを返します）。ICE と DTLS の確立後に、メディアが流れ始めます。
5. サーバーは 10 秒ごとに `ping` を送ります。10 秒以内に `pong` が返らなければ、`ping_timeout` で切断します。

その他の close code:

| close code | 発生する場面 |
|---|---|
| 4408 | WebSocket を開いてから 10 秒以内に最初のメッセージが届かない |
| 1013 | 部屋の処理をまだ開始できない（署名鍵が無い、クラスターの初期化前） |
| 4300 | 上記の redirect |
| 4503 | 上記の `capacity_unavailable`、または `room_full` |

その他のエラーコード・close code・切断の理由は、[シグナリング v1 §7](../../realtime/signaling.md) の表のとおりです。
部屋が存在しない場合（制御状態に無い、または配置先のノードがまだ部屋を開いていない場合）は `room_not_found` になります（ノードは参加時に部屋を作成しません）。

## 10.6 マルチストリーム・サイマルキャスト・レイヤーの選択・転送フィルター

- **受信スロット**: クライアントの受信用の m 行 1 つが、受信スロット 1 つになります。サーバーは部屋のトラックを受信スロットに割り当て、足りなければ `renegotiate` を送ります。
  受信スロットは種類（音声・映像）ごとに 32 個までです。自分のトラックと、同じ `bundle_id` の接続のトラックは受信しません。`multistream: false` の場合は種類ごとに 1 本だけ受信します。
- **サイマルキャスト**: `connect.simulcast {rids, default_rid?}` で、送信するレイヤーの rid を**低い順**に 1〜4 個指定します（1〜16 文字の英数字、重複不可）。
- **レイヤーの選択**（受信者ごと・トラックごと）。優先順位は次のとおりです。
  1. クライアントが送った `layer {track_id, rid}`（`rid: null` で自動に戻ります）
  2. スポットライト（10.7）
  3. `simulcast.default_rid`
  4. 自動: 推定帯域から音声の分として 64 kbit/s を引き、0.9 を掛けた値を、受信している映像の数で割ります。その帯域に収まる最も高いレイヤーを選びます。
     レイヤーを下げるのは帯域の不足が 1 秒続いてから（推定値が必要量の 0.6 倍を下回った場合はすぐ）、上げるのは帯域に収まる状態が 2 秒続いてからです。

  切り替えは新しいレイヤーのキーフレームから行います（それまでは現在のレイヤーを転送します）。現在受信しているレイヤーは `notify {event: "layer.changed"}` で分かります。
- **転送フィルター**: 部屋の `settings.forwarding_filter` と、接続ごとの `connect.forwarding_filter` の**両方の条件を満たすトラック**だけを転送します。

```json
{"action":"block","rules":[[{"field":"kind","operator":"is_in","values":["video"]},
                            {"field":"client_id","operator":"is_not_in","values":["teacher"]}]]}
```

上の例は「先生以外の映像を受信しない」という意味です。`rules` は、AND で結んだ規則の組を OR でつないだものです（1〜10 組、各組 1〜10 規則）。
`field` には**配信者**の `connection_id`・`client_id`・`bundle_id`・`kind`・`metadata.<key>`（キーは 64 バイトまで、値が文字列・数値・真偽値の場合）を指定します。
`operator` は `is_in`・`is_not_in`、`values` は 1〜100 個（各 255 バイトまで。`kind` の場合は `audio`・`video`）です。
`action` は `block`（一致したものを転送しない）か `allow`（一致したものだけを転送する）です。

## 10.7 スポットライト

部屋の `settings.spotlight` で有効にします。音量から話者を検出し、注目した `number` 人の映像を高いレイヤーで転送します。
それ以外の参加者の映像は、最も低いレイヤーで転送するか（`unfocused: "lowest"`）、転送しません（`"off"`。受信スロットは残り、`paused` になります）。

- 話者の検出: RFC 6464 の音量（無ければ Opus のパケットサイズから推定）を使います。200 ms 続けば話している、1.5 秒無音なら話していないと判定します。
  注目してから 2 秒間は注目から外しません。注目の組は部屋で 1 つ（`spotlight.changed`）で、各受信者には自分を除いた `number` 人が注目として扱われます。
- 注目した配信者のレイヤーは `focus_rid` です（指定が無ければ帯域に応じて自動で選びます）。
- クライアントは `connect.spotlight {number, focus_rid?, unfocused?}` で、部屋の設定の範囲内で値を選べます（`number` は部屋の値以下に切り詰めます）。

## 10.8 メッセージ（DataChannel・プッシュ）

- **DataChannel**: ラベルが `#` で始まる DataChannel を使います。`connect.data_channels` で宣言し、クライアントが開きます。部屋の `data_labels` と
  トークンの `permissions.data` の両方に一致するラベルだけを受け付けます。届いたメッセージは、同じラベルを受信方向で開いている**他の参加者全員**へそのまま転送します。
- **特定の参加者へ**: シグナリングの `message {label, to: [connection_id], data}` を使います。受信者には `from` 付きで届きます。
- **API から**: `POST /rooms/{id}/messages` を使います。

| フィールド | 意味 |
|---|---|
| `to` | connection_id の配列（省略するか空なら全員）。部屋にいない ID が含まれていると 404 |
| `label` | 指定すると、そのラベルの DataChannel を開いている参加者へ送ります（`#` で始まること、`data` が文字列であること） |
| `data` | JSON（64 KiB まで）。`label` が無ければ、シグナリングの `push {data}` として届きます |

応答は `{"delivered": N}`（メッセージを届けた参加者の数）です。

- 上限: アプリケーションのメッセージは 64 KiB、シグナリングのメッセージは 256 KiB、1 接続あたり毎秒 50 メッセージまで（超えると `rate_limited`、4 倍を超えると切断）。

## 10.9 参加者と統計

参加者（`GET …/participants`）: `connection_id`・`client_id`・`bundle_id`・`role`・`metadata`・`tracks[] {track_id, connection_id, kind, rids?}`・
`joined_at`・`peer_connected`・`signaling`（`websocket`・`datachannel`）・`subscriptions[]`（受信スロット: `mid`・`kind`・`state`（`active`・`paused`・`empty`）・`track_id`・`connection_id`・`client_id`）。

統計（`GET …/stats`）: `{room_id, at, participants[]}`。参加者ごとに `connection_id`・`rtt_ms`・`bandwidth_bps`（推定帯域）・`send_bitrate_bps`・
`retransmitted_packets`・`nack_requested`・`received_bytes`・`forwarded_bytes`・`forwarded_frames`・`published[] {track_id, kind, layers[] {rid, bitrate_bps}}`・
`subscriptions[] {mid, track_id, state, rid}`・`client`（クライアントが `pong.stats`・`stats` で送った getStats の要約）が含まれます。

## 10.10 部屋の移動と障害

- **ドレイン**（`POST /nodes/{id}/drain`）や**ノードの停止**によってグリッドが部屋を別のノードへ移すと、元のノードは現在の参加者に `redirect` を送り
  （`disconnect {reason: "redirected"}` を送ってから閉じます）、部屋を手放します。新しいノードが部屋を開き、参加者は入り直します（connection_id とトラック ID は新しくなります）。
- **元のノードが異常停止した**場合は、redirect を送れません。参加者のピア接続とシグナリングが切れ、クライアントが入り直します。入り直す先は
  どのノードでもかまいません（新しい配置先へ redirect されます）。SDK の `url` に複数のノードを書いておくと、停止したノードを飛ばして接続できます。
  グリッドが部屋を移すまでの時間は [9. クラスターとグリッド](09-cluster-grid.md) を参照してください。
- 移動前の部屋の状態（参加者・トラック・スポットライト）は引き継がれません。新しいノードでは空の部屋から始まります。

### カスケード（部屋を複数のノードにまたがらせる）

部屋の設定を `cascade: true` にする（またはノードで `--rooms-cascade` を指定する）と、参加者は近くのノードに接続したまま同じ部屋に参加できます。
クラスター（ノード間の中継）が必要です。

- 部屋の配置先のノードを **home**、それ以外で参加者を受け付けるノードを **edge** と呼びます。edge は参加者が来たときに作られ、
  参加者がいなくなってから 20 秒で閉じます。ドレイン中のノードは edge を作らず、home へ redirect します。
- 別のノードの参加者は `node` フィールド付きの参加者として見えます。映像・音声・DataChannel のメッセージは、ノード間の中継で届きます。
  REST の参加者一覧（home へ転送されます）では、`signaling: "remote"` と表示されます。
- `max_participants` は、ノードをまたいで合計した人数で判定します。別々のノードで同時に参加すると、短い間だけ上限を超えることがあります。
- **home が停止したとき**: グリッドが部屋を別のノードへ移します。移動先が edge だった場合、そこにいた参加者は切断されずにそのまま続けられます。
  停止したノードにいた参加者は、別のノードへ接続し直せば入り直せます（SDK の `url` に複数のノードを書いておきます）。
- 別のノードにいる参加者を切断・更新する REST API は、その参加者がいるノードで呼んでください。
  メディアパイプラインで書き出せるのは、home のノードにいる参加者のトラックです。
- カスケードの状態は、`GET /api/v1/nodes` の自ノードの `rooms` で確認できます。

## 10.11 E2EE（パススルー）

`settings.e2ee: true` の部屋では、クライアントが SFrame などで暗号化したメディアを、SFU は中身を見ずにそのまま転送します。

- 参加者は `connect.e2ee` を部屋の `e2ee` と同じ値にします（違うと `e2ee_mismatch`）。参加者がいる間は `e2ee` を変更できません。
- 暗号化・復号と鍵の配布は、アプリケーション側で行います。ブラウザー SDK は部屋の設定との照合だけを行い、暗号化の機能（Encoded Transform）は持ちません。
- サーバーは中身を見られないため、合成・合成した録画・文字起こしはできません。`recording: "composite"` との併用は 400 です。
  E2EE の部屋に、合成（`composite`）か文字起こし（`captions`）を含むメディアパイプラインを作成・変更しようとすると、422（`details.reason: "e2ee"`）になります。
  E2EE の部屋で使えるのは、参加者ごとの書き出しと録画（暗号化されたまま）だけです。

## 10.12 イベント（SSE）

`GET /api/v1/events`（[15. API と SDK](15-api-and-sdks.md)）に、部屋のイベントが流れます。イベントには 2 種類あります。

**制御のイベント**（合意ログに記録されます。SSE の `id` を持ち、`Last-Event-ID` で再開できます）:

| `event` | 発生する場面 | `data` |
|---|---|---|
| `room.updated` | 部屋の作成・自動作成・更新 | `{auto}` |
| `room.deleted` | 部屋の削除（自動作成された部屋の自動削除を含む） | `{}` |

**部屋の中のイベント**（参加者・トラックなど）は、**参加者がいるノードの購読者にだけ**流れます。
カスケードでは、edge のノードにいる参加者のイベントは edge のノードに流れ、`room.started`・`room.stopped` は home のノードにだけ流れます。
SSE の `id` を持たず、`Last-Event-ID` による再開の対象外です。取りこぼしたイベントは再送されません。

| `event` | 発生する場面 |
|---|---|
| `room.started` | ノードが部屋を開いた |
| `room.stopped` | ノードが部屋を閉じた（削除・別のノードへの移動） |
| `room.connection.created` | 参加者が参加した |
| `room.connection.updated` | 参加者の metadata が変わった |
| `room.connection.destroyed` | 参加者が退出した（`reason`・`duration_ms`） |
| `room.track.published` | トラックが公開された |
| `room.track.unpublished` | トラックの公開が終わった |
| `room.spotlight.changed` | 注目の組が変わった |

部屋の中のイベントの形式:

```json
{"type":"room.connection.destroyed","resource_id":"daily","event_id":"…","node":"…","occurred_at":"2026-11-03T10:00:00Z",
 "data":{"connection_id":"…","client_id":"alice","reason":"normal","duration_ms":61234}}
```

部屋の中のイベントを漏れなく受け取るには、部屋を配置したノード（`placement_status.node`）の SSE を購読してください。
イベントはテナントで絞り込まれるため、他のテナントの部屋のイベントは流れません。

### イベント Webhook

`--rooms-event-webhook URL` と `--rooms-webhook-secret-file FILE` を指定すると、各ノードは自ノードの部屋の中のイベントを、
その URL へ署名付きで `POST` します（本文の形式は [シグナリング v1](../../realtime/signaling.md) §10。`type` には
`connection.created` などの Webhook 用の名前が入ります）。

- イベントは少なくとも 1 回届けます。受信側は、本文の `id`（`Senca-Webhook-Id` ヘッダーと同じ値）で重複を取り除いてください。
- 2xx の応答で配信完了です。408・429・5xx・接続の失敗・10 秒のタイムアウトの場合は、1、2、4…秒の間隔で再送し、10 回で諦めます。その他の 4xx ではすぐに諦めます。
- 諦めた件数と最近のイベントの ID は、`GET /api/v1/nodes` の自ノードの `rooms.event_webhook` に表示されます。
- 宛先は `--egress-allow` の方針で検査します。Webhook を設定しても、SSE にはこれまでどおりイベントが流れます。

## 10.13 上限

| 項目 | 値 |
|---|---|
| 部屋の参加者数 | `max_participants`（既定 50、1〜10000）。超えると `room_full` |
| ノードあたりの参加者数・部屋数 | それぞれ 10000（超えると `room_full`） |
| テナントあたりの部屋数 | 10000 |
| 受信スロット | 種類ごとに 32 |
| サイマルキャストのレイヤー | 1〜4 |
| metadata | 8 KiB（部屋・参加者・トークン） |
| アプリケーションのメッセージ | 64 KiB |
| シグナリングのメッセージ | 256 KiB、毎秒 50 個 |
| トークンの有効期間 | 10〜86400 秒（既定 600） |
| `connect` までの期限 | 10 秒 |
| ping | 10 秒ごと、`pong` の期限は 10 秒 |
| ピア接続の期限 | 参加から 30 秒以内に接続できなければ `peer_failed` |

## 10.14 現在のノードで使えないもの

[シグナリング v1](../../realtime/signaling.md) の仕様にあっても、現在の senca-node では使えないものがあります。

- **ノードの `overflow_redirect`**: 設定できません（ノードが満員なら `room_full` になります）。
- **`settings.recording`**: 方針を示す値で、録画は始まりません（録画には [11](11-media-pipelines.md) のメディアパイプラインを使います）。
- **参加者が自分の metadata を変更するメッセージ**: シグナリング v1 にはありません。アプリケーションのサーバーから `PATCH /rooms/{id}/participants/{connection_id}` で変更してください。
- **再開（resume）**: 切断されたら、新しい接続として入り直します。

## 10.15 例

### curl

```sh
API=https://media.example.com/api/v1
H="Authorization: Bearer $SENCA_TOKEN"

# 部屋を作成する
curl -s -X POST -H "$H" -H 'Content-Type: application/json' "$API/rooms" \
  -d '{"id":"demo","settings":{"max_participants":10,"video_codec":"vp8","spotlight":{"number":2}},"metadata":{"title":"standup"}}'

# 配置されたか確認する（placement_status.state が running）
curl -s -H "$H" "$API/rooms/demo"

# 参加トークンを発行する（client_id はトークンに入れる）
curl -s -X POST -H "$H" -H 'Content-Type: application/json' "$API/rooms/demo/tokens" \
  -d '{"role":"sendrecv","client_id":"alice","metadata":{"name":"Alice"},"ttl_secs":600}'

# 参加者と統計
curl -s -H "$H" "$API/rooms/demo/participants"
curl -s -H "$H" "$API/rooms/demo/stats"

# 全員へプッシュ / #chat の DataChannel へ送信
curl -s -X POST -H "$H" -H 'Content-Type: application/json' "$API/rooms/demo/messages" -d '{"data":{"notice":"5 分後に終わります"}}'
curl -s -X POST -H "$H" -H 'Content-Type: application/json' "$API/rooms/demo/messages" -d '{"label":"#chat","data":"hello"}'

# 設定を変更する（全体を置き換える。If-Match に現在の ETag）
curl -s -X PATCH -H "$H" -H 'Content-Type: application/json' -H 'If-Match: "1"' "$API/rooms/demo" \
  -d '{"settings":{"max_participants":20,"video_codec":"vp8","spotlight":{"number":2}}}'

# 1 人を切断する / 部屋を削除する
curl -s -X DELETE -H "$H" "$API/rooms/demo/participants/$CONNECTION_ID"
curl -s -X DELETE -H "$H" "$API/rooms/demo"
```

### ブラウザー SDK（`@senca/sdk`）

```js
import { Senca } from '@senca/sdk';

const camera = await navigator.mediaDevices.getUserMedia({ video: { width: 1280, height: 720 }, audio: true });
const session = await Senca.connect({
  url: 'wss://media.example.com/signaling',   // トークンの応答の signaling_url（複数書くと、入り直すときに順に切り替える）
  room: 'demo',
  token: () => fetch('/my-backend/room-token').then((r) => r.text()),   // 入り直すたびに取得し直す
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
await session.send('#chat', { text: 'hello' });                  // 全員へ（DataChannel）
for await (const m of session.messages('#chat')) console.log(m.sender?.clientId, m.json());
const t = [...session.tracks.values()].find((t) => t.kind === 'video');
await t.setLayer('r0');                                          // 'r0' | 'r1' | 'r2' | 'auto'
await session.close();
```

SDK は、SDP、受信スロット、再ネゴシエーション、ping、redirect（3 回まで）、再接続（0.5 秒から倍々に延ばして最大 10 秒、10 回まで）を内部で処理します。
オプションとイベントの一覧は `apps/sdk-js/README.md` と [15. API と SDK](15-api-and-sdks.md) にあります。
