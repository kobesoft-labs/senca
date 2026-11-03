# リアルタイムの部屋: シグナリング v1

日本語 | [English](signaling.en.md)

この文書は、SENCA のリアルタイムの部屋（ルーム）に参加するクライアントを実装する人のための、シグナリングプロトコルの仕様です。
サーバーの設定や REST API の使い方は [10. リアルタイムの部屋](../guide/ja/10-rooms.md) を参照してください。

- 設計の背景: [ADR-0015](../dev/adr/0015-realtime-rooms.md)（リアルタイムの部屋）、[ADR-0012](../dev/adr/0012-webrtc.md)（WebRTC）、[ADR-0013](../dev/adr/0013-grid.md)（グリッド）、[ADR-0014](../dev/adr/0014-relay.md)（中継）

用語:

- **配信者**: トラックを送信する参加者。**受信者**: 他の参加者のトラックを受信する参加者。
- **受信スロット**: クライアントの受信用の m 行 1 つ（§3）。
- **レイヤー**: サイマルキャストで送る、解像度やビットレートの異なる各エンコード（rid で識別）。

## 1. トランスポートとバージョン

| 項目 | 値 |
|---|---|
| トランスポート | WebSocket（RFC 6455）。`GET /signaling`、subprotocol **`senca.rooms.v1`**（必須）。拡張（permessage-deflate）は使いません |
| フレーム | 1 つの text frame に 1 つの JSON メッセージ（UTF-8）を入れます。binary frame は使いません |
| 切り替え | 接続後は、同じ JSON を DataChannel **`signaling`** で送受信することもできます（§6） |
| バージョン | `connect.version` = **1**。サーバーは `connected.version` で応答します。一致しなければ `unsupported_version` で閉じます |
| 型 | すべてのメッセージは `type` を持ちます。**未知の `type` や未知のフィールドは `bad_message`** になります（黙って無視はしません） |
| 上限 | メッセージ 256 KiB、metadata 8 KiB、アプリケーションのメッセージ 64 KiB、毎秒 50 メッセージ（超えると `rate_limited`、4 倍を超えると切断） |

バージョンを上げるのは、意味が変わる変更のときだけです。ただし、フィールドを追加するときもバージョンを上げます
（未知のフィールドは拒否されるため、古いサーバーに新しいフィールドを送ると `bad_message` になります。クライアントは `connected.version` を見て、送る内容を決めてください）。

## 2. 全体の流れ

```
client                                   server
  |-- WebSocket (senca.rooms.v1) -------->|
  |-- connect {version, room_id, token} ->|  トークンの検証・（認証 Webhook）・部屋の上限・E2EE・コーデックを確認
  |<- connected {connection_id, participants, recv_slots} --|
  |-- offer {sdp} ----------------------->|  送信用の m 行（sendonly）＋受信用の m 行（recvonly）を recv_slots の数だけ
  |<- answer {sdp, slots, published} -----|  各受信スロットに流すトラックと、送信するトラックの ID
  |   ... ICE（サーバーは ICE-lite）・DTLS・メディア ...
  |<- notify {connection.created / track.added ...}
  |<- renegotiate {audio, video} ---------|  受信スロットが足りない（新しい配信者が来た）
  |-- offer {sdp}（受信スロットを追加）-->|
  |<- answer {sdp, slots} ----------------|
  |<- slots {slots}（割り当ての変化）-----|  配信者が退出した・スポットライトが変わった
  |<- ping / -- pong -->                  |  10 秒ごと。10 秒以内に pong を返す
  |-- disconnect ------------------------>|
```

**offer は常にクライアントが送ります**（サーバーは answer だけを返します）。そのため glare（offer の衝突）は起きません。
クライアントは、1 つの offer に対する answer を受け取るまで、次の offer を送ってはいけません（送ると `invalid_state`）。
answer を待っている間に `renegotiate` が届いたら、answer を受け取った後で offer を送り直してください。

## 3. 受信スロット

クライアントの**受信用の m 行 1 つが、受信スロット 1 つ**になります（`recvonly` か `sendrecv`）。サーバーは部屋のトラックを受信スロットに割り当て、
SFU の送信トラックの送信元を切り替えて流します（受信者からは 1 本の SSRC に見えます。送信元が切り替わるときは、キーフレームから始まります）。

- 割り当ては安定しています。現在の割り当ては維持し、新しいトラックは同じ種類の空いている受信スロットに入ります。配信者が退出した受信スロットは空き（`state: "empty"`）になり、
  次の配信者に再利用されます（m 行が増え続けることはありません）。
- 受信スロットが足りなければ、サーバーは `renegotiate {audio, video}`（種類ごとの受信スロットの**合計数**）を送ります。クライアントは `recvonly` の transceiver を追加して offer を送ります。
- 受信スロットは種類ごとに 32 個までです。
- 誰の映像かは、`slots[].connection_id`・`client_id`・`track_id` で分かります（`ontrack` の transceiver の `mid` と照合します）。
- `state`: `active`（転送中）・`paused`（割り当て済みだが転送していない。スポットライトの対象外で `unfocused: "off"` の場合）・`empty`。

次のトラックは受信しません: 自分のトラック、同じ `bundle_id` の接続のトラック、転送フィルター（§8）で除外されたもの。
また、役割が `sendonly` の接続と、権限が `subscribe: false` の接続は何も受信しません。
`multistream: false` の接続は、種類ごとに 1 本だけ受信します（スポットライトの注目、無ければ最初の配信者）。

## 4. メッセージ（クライアント → サーバー）

### `connect`（最初のメッセージ。10 秒以内に送ります）

| フィールド | 型 | 既定 | 意味 |
|---|---|---|---|
| `version` | 1 | 必須 | |
| `room_id` | string | 必須 | 1〜128 文字の英数字と `-_.:@` |
| `token` | string | | 参加トークン（§9）。認証 Webhook だけで認可する場合は省略できます |
| `role` | `sendrecv`・`sendonly`・`recvonly` | 必須 | トークンの `role` の範囲内（`sendrecv` のトークンならどれでも可） |
| `client_id` | string | | トークンに `client_id` があれば、同じ値でなければなりません。トークンだけで認可する部屋では、トークンに無い値は名乗れません（転送フィルターの条件に使われるためです。認証 Webhook はこの値を見て認可できます） |
| `bundle_id` | string | `connection_id` | 同じ値を持つ接続どうしは、互いのトラックを受信しません（同じ人のカメラと画面共有など）。`client_id` と同じく、トークンだけで認可する部屋ではトークンの `bundle_id` だけを使います |
| `metadata` | JSON | | 認証 Webhook にだけ渡します（他の参加者には公開しません） |
| `multistream` | bool | true | false にすると、種類ごとに 1 本だけ受信します |
| `simulcast` | `{rids, default_rid?}` | | 送信するレイヤーの rid（**低い順**、1〜4 個、英数字 16 文字まで）。`default_rid` は受信時の最初のレイヤー |
| `spotlight` | `{number, focus_rid?, unfocused?}` | 部屋の設定 | 部屋のスポットライトの範囲内で、人数とレイヤーを選びます |
| `audio` / `video` | `{enabled, codec?, bitrate_kbps?}` | | `codec` が部屋のコーデックと違うと `unsupported_codec` |
| `data_channels` | `[{label, direction, ordered, max_retransmits?, max_packet_life_time?}]` | | 使用する DataChannel（§6）。クライアントが同じラベルで開きます |
| `signaling` | `{data_channel, close_websocket}` | false | DataChannel への切り替え（§6） |
| `e2ee` | bool | false | 部屋の `e2ee` と違うと `e2ee_mismatch` |
| `forwarding_filter` | §8 | | この接続で受信するトラックを絞り込みます |
| `sdk` | `{name, version}` | | ログ用 |

### その他のメッセージ

| `type` | フィールド | 意味 |
|---|---|---|
| `offer` | `sdp` | 送信用と受信用の m 行を含む offer。BUNDLE と rtcp-mux が必須 |
| `candidate` | `candidate`, `sdp_mid?`, `sdp_mline_index?`, `username_fragment?` | trickle ICE（空文字列は end-of-candidates）。サーバーは ICE-lite なので、送らなくてもかまいません |
| `pong` | `stats?` | `ping` への応答。`stats` に getStats の要約（64 KiB まで）を含められます |
| `layer` | `track_id`, `rid`（`null` で自動） | 受信しているトラックのレイヤーを選びます |
| `message` | `label`, `to?`, `data` または `data_base64` | アプリケーションのメッセージ（`to` は connection_id の配列。空なら全員）。送信者は `label` を送信方向（`sendrecv`・`sendonly`）で宣言している必要があります。受信するのは、`label` を受信方向で宣言し、権限で許可されている参加者だけです。§6 |
| `stats` | `reports` | getStats の要約（REST API の統計に表示されます） |
| `disconnect` | `reason?` | 切断します |

## 5. メッセージ（サーバー → クライアント）

| `type` | フィールド | 意味 |
|---|---|---|
| `connected` | `version`, `connection_id`, `client_id`, `bundle_id`, `room_id`, `role`, `multistream`, `e2ee`, `video_codec`, `audio_codec`, `max_video_bitrate_kbps?`, `spotlight?`, `participants`, `recv_slots {audio, video}`, `data_channels`, `signaling_data_channel`, `ping_interval_ms`, `metadata` | 参加に成功しました。`role`・`metadata` は、認証 Webhook が変更した後の値です。最初の offer には、`recv_slots` の数だけ受信用の m 行を入れてください |
| `answer` | `sdp`, `slots[]`, `published[] {mid, track_id, kind, rids}` | offer への answer と、すべての受信スロットの割り当て |
| `slots` | `slots[]` | 変化した受信スロットだけ |
| `renegotiate` | `audio`, `video` | 受信スロットをこの数まで増やして、offer を送り直してください |
| `notify` | `event` と内容（下表） | 部屋のイベント |
| `push` | `data` | REST API からのプッシュ |
| `message` | `label`, `from?`, `data` または `data_base64` | シグナリングで届いたアプリケーションのメッセージ |
| `ping` | | 10 秒ごとに送ります。10 秒以内に `pong` が無ければ `ping_timeout` で切断します |
| `switched` | | 以後のシグナリングは DataChannel `signaling` で行います（§6） |
| `redirect` | `location` | 別のノードへ接続し直してください（この後 `disconnect {reason: "redirected"}` を送って閉じます。例外は §13） |
| `error` | `code`, `message`, `fatal`, `request?` | §7 |
| `disconnect` | `reason` | 切断します（この後に閉じます） |

`notify` の `event`:

| `event` | 内容 |
|---|---|
| `connection.created` | `participant {connection_id, client_id, bundle_id, role, metadata, tracks[], node?}`（`node` は別のノードにいる参加者に付きます） |
| `connection.updated` | `connection_id`, `metadata` |
| `connection.destroyed` | `connection_id`, `client_id?`, `reason` |
| `track.added` | `track {track_id, connection_id, kind, rids[]}` |
| `track.removed` | `track_id`, `connection_id` |
| `spotlight.changed` | `focused[]`（connection_id の配列。新しく注目した順） |
| `layer.changed` | `track_id`, `mid`, `rid`（現在受信しているレイヤー） |
| `network.status` | `level`（0 = 良好〜3 = 不良）, `bandwidth_bps`, `rtt_ms`（自分の回線の状態。2 秒に 1 回まで） |

## 6. DataChannel

- **アプリケーションのメッセージ**: ラベルが `#` で始まる DataChannel を使います。`connect.data_channels` で宣言し、**クライアントが開きます**
  （`pc.createDataChannel('#chat', {ordered, maxRetransmits})`）。宣言していないラベルや、部屋の `data_labels`・トークンの
  `permissions.data` に一致しないラベルは、サーバーが閉じます。
- 届いたメッセージは、同じラベルを開いていて、`direction` が受信方向（`sendrecv`・`recvonly`）の**他の参加者全員**へ、そのまま
  （text は text、binary は binary のまま）転送します。送信者自身には返しません。`direction: "recvonly"` の接続から送られたメッセージは転送しません。
- 特定の相手へ送るには、シグナリングの `message {label, to: [connection_id], data}` を使います。受信者には `message {label, from, data}` が届きます。
- **シグナリングの切り替え**: `connect` に `signaling: {data_channel: true}` と `data_channels: [{label: "signaling"}]` を付け、
  クライアントが `signaling` を開くと、サーバーは WebSocket で `switched` を送り、以後は DataChannel でメッセージを送ります。
  クライアントも以後は DataChannel で送ってかまいません（WebSocket で送ったメッセージも受け付けます）。
  `close_websocket: true` なら、WebSocket を閉じても切断しません（false なら切断します）。DataChannel が閉じると WebSocket に戻り、
  WebSocket も閉じていれば切断します。

## 7. エラー

```json
{"type":"error","code":"room_full","message":"room is full","fatal":true,"request":"connect"}
```

`fatal: true` の場合、サーバーは続けて `disconnect {reason: "error"}` を送り、WebSocket を下表の close code で閉じます。`fatal: false` の場合は
そのメッセージだけを拒否し、接続は続きます（参加後の `bad_message`・`invalid_state`、2 回目以降の offer の誤りなど）。

| `code` | close code | 発生する場面 |
|---|---|---|
| `bad_message` | 4400 | JSON・型・未知のフィールド・値の範囲の誤り（参加前なら fatal） |
| `unsupported_version` | 4422 | `version` が 1 でない |
| `auth_failed` | 4401 | トークンが無い・不正・期限切れ、認証 Webhook が拒否した・タイムアウトした（5 秒） |
| `forbidden` | 4403 | 別の部屋のトークン、役割や client_id がトークンと合わない、許可されていない種類のトラックを送った |
| `room_not_found` | 4404 | 部屋が存在しない（senca-node は参加時に部屋を作成しません） |
| `room_full` | 4503 | 部屋またはノードの上限に達した（`overflow_redirect` を設定したサーバーでは `redirect`。senca-node では設定できません） |
| `capacity_unavailable` | 4503 | 部屋を配置できるノードが今は無い（senca-node で、グリッドが配置先を決められない場合） |
| `role_violation` | 4403 | `recvonly` の接続が送信用の m 行を含めた |
| `e2ee_mismatch` | 4422 | `connect.e2ee` と部屋の `e2ee` が違う |
| `unsupported_codec` | 4422 | 部屋のコーデックと違う |
| `invalid_sdp` | 4400 | SDP を解釈できない、mid の無い m 行がある |
| `negotiation_failed` | 4500 | answer を作成できない（BUNDLE・rtcp-mux・コーデック） |
| `invalid_state` | 4400 | 最初のメッセージが `connect` でない、offer の処理中に次の offer を送った、認証中にメッセージを送った |
| `rate_limited` | 4429 | 毎秒 50 メッセージを超えた |
| `unknown_track` | 4404 | `layer` の track_id・rid が存在しない、または受信していない |
| `unknown_label` | 4403 | 使えないラベルで `message` を送った |
| `timeout` | 4408 | 10 秒以内に `connect` が届かない |
| `internal` | 4500 | サーバー内部のエラー |

切断の理由（`disconnect.reason`・`connection.destroyed.reason`・イベントで使います）: `normal`・`websocket_closed`・`ping_timeout`・
`peer_failed`（ICE・DTLS の失敗、または参加から 30 秒以内に接続できない）・`kicked`（REST API で切断）・`room_deleted`・`error`・`redirected`・`node_shutdown`。

## 8. 転送フィルター

```json
{"action":"block","rules":[[{"field":"kind","operator":"is_in","values":["video"]},
                            {"field":"client_id","operator":"is_not_in","values":["teacher"]}]]}
```

`rules` は、AND で結んだ規則の組を OR でつないだものです（1〜10 組、各組 1〜10 規則）。各規則は**配信者**の `connection_id`・`client_id`・`bundle_id`・`kind`・
`metadata.<key>`（値が文字列・数値・真偽値の場合）を条件にします。`block` は一致したトラックを転送せず、`allow` は一致したトラックだけを転送します。
部屋の設定のフィルターと接続のフィルターの両方を満たすトラックだけを転送します。部屋のフィルターを変更すると、その場で受信スロットを割り当て直します。

## 9. 認証

### 参加トークン（JWT 互換）

SENCA の署名鍵（Ed25519 = `EdDSA`、P-256 = `ES256`）で署名します。`POST /rooms/{id}/tokens` で発行できます（senca-node は Ed25519 で署名します）。
`none`、HS 系（共有鍵）、RS 系、`crit` は拒否します。アルゴリズムは鍵によって決まります（ヘッダーの `alg` が鍵と合わなければ拒否します）。

```json
{"iss":"senca","aud":"senca-rooms","exp":1767225600,"iat":1767222000,"jti":"...",
 "room":"daily","role":"sendrecv","client_id":"alice","metadata":{"name":"Alice"},
 "permissions":{"publish":["audio","video"],"subscribe":true,"data":["#chat"]}}
```

`exp` は必須です（許容する時計のずれは 30 秒まで）。有効期間（`exp - iat`）は 24 時間までです。`metadata` は他の参加者に公開されます。
`permissions` の既定は「音声と映像を送信でき、受信でき、DataChannel のラベルは `#*`」です。

### 認証 Webhook

認証 Webhook を設定すると、サーバーは参加時に Webhook の URL へ `POST` で問い合わせます（本文には署名を付けます。下記）。5 秒以内に応答が無ければ拒否します。
問い合わせ方は 2 通りあります。

- `after_token`（既定）: トークンを検証した後に、さらに問い合わせます。
- `only`: トークンの代わりに、Webhook の応答だけで認可します。

```json
{"version":1,"type":"connection.auth","request_id":"…","node":"node-a","room_id":"daily",
 "connection_id":"…","client_id":"alice","role":"sendrecv","metadata":{…クライアントの connect.metadata…},
 "token_claims":{…トークンがあれば…},"remote_addr":"203.0.113.5:53211"}
```

応答（2xx。未知のフィールドがあると拒否します）:

```json
{"allowed":true,"role":"recvonly","client_id":"alice","metadata":{"name":"Alice"},
 "permissions":{"publish":[],"subscribe":true,"data":["#chat"]}}
```

`allowed: false` の場合は、`reason` をクライアントへ伝えます（`auth_failed`）。2xx 以外の応答や、解釈できない本文も拒否として扱います。

senca-node での設定:

- 問い合わせ先は、部屋の設定 `auth_webhook_url`（http/https）があればその URL、無ければノードの `--rooms-auth-webhook` の URL です。
- 問い合わせ方は `--rooms-auth-mode`（`after_token` または `only`）で選びます。
- どちらの URL も無い部屋は、トークンだけで認可します（トークンに無い `client_id`・`bundle_id` は名乗れません）。
- 署名用のシークレットは `--rooms-webhook-secret-file` で渡します。シークレットが無いのに URL を持つ部屋への参加は拒否します。
- 宛先は `--egress-allow` の方針で検査します（プライベートアドレスへ送るには許可が必要です）。
- リクエストには `Senca-Webhook-Id`（`request_id` と同じ値）ヘッダーも付きます。

### 署名（Webhook 共通）

`Senca-Signature: t=<UNIX 秒>,v1=<hex(HMAC-SHA256(secret, "<t>.<本文>"))>` の形式です。シークレットの入れ替え中は、`v1=` を複数付けます。
受信側は、`|now - t| <= 300` 秒であることと、いずれかの `v1` が一致することを確認してください。
イベントの Webhook には、`Senca-Webhook-Id`（event_id）と `Senca-Webhook-Attempt` も付きます。

## 10. イベント（SSE と Webhook）

SSE（`/api/v1/events`）と Webhook には、同じ形式のエンベロープでイベントを出力します。

```json
{"id":"01j…","version":1,"at":1767222000123,"node":"node-a","room_id":"daily",
 "type":"connection.destroyed","data":{"connection_id":"…","client_id":"alice","reason":"normal","duration_ms":61234}}
```

`type`: `room.created`・`room.updated`・`room.destroyed`・`connection.created`・`connection.updated`・`connection.destroyed`・
`track.published`・`track.unpublished`・`spotlight.changed`。

senca-node の SSE では、名前を `room.started`（= `room.created`）・`room.stopped`（= `room.destroyed`）・`room.connection.*`・
`room.track.*`・`room.spotlight.changed` に変え、`data` に内容を入れます（`room.updated` は、制御状態の変更のイベントとして出力されます）。

senca-node に `--rooms-event-webhook URL` を指定すると、上の形式のエンベロープ（`type` は上記の Webhook 用の名前）に署名して POST します
（`--rooms-webhook-secret-file` が必要です）。

- 各ノードは、自ノードにいる参加者のイベントを送ります（`node` で区別できます）。
- カスケードの edge のノードは `room.created`・`room.updated`・`room.destroyed` を送りません（home のノードだけが送ります）。
- 諦めたイベントの数と ID は、`GET /api/v1/nodes` の `rooms.event_webhook` で確認できます。

Webhook は**少なくとも 1 回**届けます（受信側は `id` で重複を取り除いてください）。2xx の応答で配信完了です。408・429・5xx・接続の失敗・10 秒のタイムアウトの場合は、
1, 2, 4, … 秒（±20 %、最大 1 時間）の間隔で再送し、10 回で諦めます（dead letter）。その他の 4xx ではすぐに諦めます。

## 11. REST API

`/api/v1` の共通の規約（Bearer、problem+json、`Idempotency-Key`、`If-Match`・`ETag`、`limit`・`cursor`、時刻は RFC 3339）に従います。
参加者・メッセージ・統計には、部屋を配置したノードが応答します（どのノードに送っても、配置先のノードへ転送されます）。
シグナリングも、どのノードに接続してもかまいません（部屋が別のノードにあれば `redirect` が返ります）。

| 操作 | パス |
|---|---|
| 部屋の作成 | `POST /rooms` `{id?, settings?, metadata?, placement?}` → 201（部屋はグリッドのワークロードとして 1 つのノードに配置されます。`placement` で手動の割り当てや優先度を指定できます。ADR-0013） |
| 一覧・取得 | `GET /rooms?limit&cursor`・`GET /rooms/{id}`（`ETag: "{generation}"`） |
| 更新 | `PATCH /rooms/{id}` `{settings?, metadata?, placement?}`（`If-Match` 必須。参加者がいる間は `e2ee`・`video_codec`・`audio_codec` を変更できません: 409） |
| 削除 | `DELETE /rooms/{id}`（参加者は `room_deleted` で切断されます） |
| 参加者 | `GET /rooms/{id}/participants`・`GET …/{connection_id}`（`subscriptions` は受信スロット） |
| 参加者の metadata | `PATCH /rooms/{id}/participants/{connection_id}` `{metadata}` → `connection.updated` |
| 切断 | `DELETE /rooms/{id}/participants/{connection_id}`（`kicked`） |
| メッセージの送信 | `POST /rooms/{id}/messages` `{to?, label?, data}`（`label` があれば DataChannel、無ければ `push`） |
| 統計 | `GET /rooms/{id}/stats`（参加者ごとの RTT・推定帯域・送受信量・送信しているレイヤーのビットレート・受信しているレイヤー・クライアントの getStats） |
| 参加トークン | `POST /rooms/{id}/tokens` `{role, client_id?, bundle_id?, metadata?, permissions?, ttl_secs?}`（`ttl_secs` の既定は 600、10〜86400）→ `{token, room_id, expires_at（RFC 3339）, signaling_url}`。部屋が無ければ自動で作成します（`auto: true`。空のまましばらく経つと削除されます）。署名は Ed25519 |

部屋の設定（`settings`）: `max_participants`（50）・`e2ee`・`video_codec`（`vp8`・`vp9`・`h264`・`av1`）・`audio_codec`（`opus`）・
`max_video_bitrate_kbps`・`spotlight {number, unfocused: "lowest"|"off", focus_rid?}`・`recording`（`none`・`per_participant`・
`composite`。E2EE とは併用できません）・`forwarding_filter`・`data_labels`（`["#*"]`）・`empty_timeout_secs`（60。トークンの発行で自動作成された部屋に適用）・
`cascade`・`auth_webhook_url`。詳細は [10.3](../guide/ja/10-rooms.md) を参照してください。

E2EE の部屋（SFrame など）では、SFU は中身を見ずに転送します。サーバーでの合成・合成した録画・文字起こしはできません。
E2EE の部屋に合成か文字起こしを含むメディアパイプラインを作成しようとすると、422（`details.reason: "e2ee"`）で拒否されます。

## 12. レイヤーの選択とスポットライト

レイヤーは、受信者ごと・トラックごとに選びます。優先順位は次のとおりです。

1. クライアントの `layer`
2. スポットライト（注目している配信者は `focus_rid` か自動、注目していない配信者は最も低いレイヤーか `off`）
3. `simulcast.default_rid`
4. 自動: 推定帯域 × 0.9 を受信している映像の数で割り、その帯域に収まる最も高いレイヤーを選びます。
   下げるのは帯域の不足が 1 秒続いてから（大きく不足している場合はすぐ）、上げるのは収まる状態が 2 秒続いてからです。

切り替えは、新しいレイヤーのキーフレームから行います（それまでは現在のレイヤーを転送します）。

スポットライトは、音量（RFC 6464 の -dBov。無ければ Opus のパケットサイズから推定）で話者を検出します。
200 ms 続けば話している、1.5 秒無音なら話していないと判定し、注目してから 2 秒間は注目から外しません（注目が頻繁に切り替わるのを防ぐため）。
注目の組は部屋で 1 つ（`spotlight.changed`）で、各受信者には自分を除いた `number` 人が注目として扱われます。

## 13. カスケード

部屋は複数のノードにまたがることができます（ADR-0013・0014）。別のノードにいる参加者は `node` 付きの参加者として見え、
そのトラックも通常の参加者と同じように受信スロットに割り当てられます。
部屋をこのノードで受け付けられない場合（グリッドの配置や上限による）は、`redirect {location}` で別のノードへ案内します。

senca-node での動作:

- 最初の `connect` を受けた時点で部屋が別のノードにある場合は、`redirect` を送ってから close code 4300 で閉じます（この場合は `disconnect` を送りません）。
  `connect` が 10 秒以内に届かなければ、close code 4408 で閉じます。
- 部屋を配置したノード（グリッドの配置先）を **home** と呼びます。部屋の設定が `cascade: true`（またはノードで `--rooms-cascade` を指定）の場合、
  別のノードに接続した参加者は redirect されず、そのノードの **edge** で受け付けられます。
  `cascade` が false の部屋では、従来どおり home へ案内します。ドレイン中のノードは、新しい edge を作りません。
- ノード間で 7 秒間同期できなかったノードの参加者は、参加者の一覧から外れます。
- `max_participants` は、ノードをまたいで合計した人数で判定します（別々のノードで同時に参加すると、短い間だけ上限を超えることがあります）。
- home が停止すると、グリッドが部屋を別のノードへ移します。移動先に edge があれば、そこにいた参加者はそのまま続けられ、そのノードが新しい home になります。
  停止したノードにいた参加者は、別のノードへ接続し直せば入り直せます。参加者がいなくなった edge は 20 秒で閉じます。

## 14. 例

```json
→ {"type":"connect","version":1,"room_id":"daily","role":"sendrecv","token":"eyJ…","client_id":"alice",
   "simulcast":{"rids":["r0","r1","r2"]},"data_channels":[{"label":"#chat"}],"video":{"codec":"vp8"}}
← {"type":"connected","version":1,"connection_id":"6x0…","client_id":"alice","bundle_id":"6x0…","room_id":"daily",
   "role":"sendrecv","multistream":true,"e2ee":false,"video_codec":"vp8","audio_codec":"opus",
   "participants":[{"connection_id":"9k2…","client_id":"bob","bundle_id":"9k2…","role":"sendrecv","metadata":null,
   "tracks":[{"track_id":"9k2….0","connection_id":"9k2…","kind":"video"},{"track_id":"9k2….1","connection_id":"9k2…","kind":"audio"}]}],
   "recv_slots":{"audio":1,"video":1},"data_channels":[{"label":"#chat","direction":"sendrecv","ordered":true}],
   "signaling_data_channel":false,"ping_interval_ms":10000,"metadata":{"name":"Alice"}}
→ {"type":"offer","sdp":"v=0…m=video … a=sendonly … a=simulcast:send r0;r1;r2 … m=audio … a=sendonly … m=application … m=audio … a=recvonly … m=video … a=recvonly …"}
← {"type":"answer","sdp":"v=0…","published":[{"mid":"0","track_id":"6x0….0","kind":"video","rids":["r0","r1","r2"]},
   {"mid":"1","track_id":"6x0….1","kind":"audio"}],
   "slots":[{"mid":"3","kind":"audio","state":"active","track_id":"9k2….1","connection_id":"9k2…","client_id":"bob"},
            {"mid":"4","kind":"video","state":"active","track_id":"9k2….0","connection_id":"9k2…","client_id":"bob"}]}
← {"type":"notify","event":"connection.created","participant":{"connection_id":"p7q…","client_id":"carol",…}}
← {"type":"renegotiate","audio":2,"video":2}
→ {"type":"layer","track_id":"9k2….0","rid":null}
← {"type":"notify","event":"layer.changed","track_id":"9k2….0","mid":"4","rid":null}
← {"type":"ping"}
→ {"type":"pong"}
```
