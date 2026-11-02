# リアルタイムの部屋: シグナリング v1

- 関係: ADR-0015（リアルタイムの部屋）、ADR-0012（WebRTC）、ADR-0013（グリッド）、ADR-0014（中継）
- 実装: `crates/senca-rooms`（`signaling.rs` が型、`service.rs` が状態機械）、`crates/senca-ws`（WebSocket）
- 相互接続の試験: `tools/interop/rooms.sh`（Chrome 3 ページ）、`crates/senca-rooms/tests/e2e.rs`（senca-webrtc 同士）

## 1. 輸送と版

| 項目 | 値 |
|---|---|
| 輸送 | WebSocket（RFC 6455）。`GET /signaling`、subprotocol **`senca.rooms.v1`**（必須）。拡張（permessage-deflate）は使わない |
| 枠 | text frame 1 つに JSON の message 1 つ（UTF-8）。binary frame は使わない |
| 切り替え | つながった後は DataChannel **`signaling`** に同じ JSON を載せられる（§6） |
| 版 | `connect.version` = **1**。サーバーは `connected.version` で答える。違えば `unsupported_version` で閉じる |
| 型 | すべての message は `type` を持つ。**未知の `type`・未知のフィールドは `bad_message`**（黙って無視しない） |
| 上限 | message 256 KiB、metadata 8 KiB、アプリの message 64 KiB、1 秒に 50 message（超えたら `rate_limited`、4 倍で切断） |

版を上げるのは意味の変わる変更のときだけ。フィールドを足すときも版を上げる（未知のフィールドを拒むため、
古いサーバーに新しいフィールドを送ると `bad_message` になる。クライアントは `connected.version` を見て送るものを決める）。

## 2. 流れ

```
client                                   server
  |-- WebSocket (senca.rooms.v1) -------->|
  |-- connect {version, room_id, token} ->|  トークン検証・（認証 Webhook）・部屋の上限・E2EE・コーデック
  |<- connected {connection_id, participants, recv_slots} --|
  |-- offer {sdp} ----------------------->|  送る m 行（sendonly）＋受ける m 行（recvonly）を recv_slots の数だけ
  |<- answer {sdp, slots, published} -----|  受ける枠に誰のトラックを流すか、送るトラックの ID
  |   ... ICE（サーバーは ICE-lite）・DTLS・メディア ...
  |<- notify {connection.created / track.added ...}
  |<- renegotiate {audio, video} ---------|  枠が足りない（新しい送り手）
  |-- offer {sdp}（枠を足した）---------->|
  |<- answer {sdp, slots} ----------------|
  |<- slots {slots}（割り当ての変化）-----|  送り手が抜けた・スポットライトが変わった
  |<- ping / -- pong -->                  |  10 秒ごと、10 秒以内に pong
  |-- disconnect ------------------------>|
```

**offer は常にクライアントが出す**（サーバーは answer だけ）。そのため glare が起きない。クライアントは
1 つの offer の answer を受けるまで次の offer を出さない（出せば `invalid_state`）。answer を待つ間に
`renegotiate` が来たら、answer の後で offer し直す。

## 3. 受信の枠（slot）

クライアントの**受ける m 行 1 つが枠 1 つ**（`recvonly` か `sendrecv`）。サーバーは部屋のトラックを枠に割り当て、
SFU の送信トラックの送り元を替えて流す（受け手には 1 本の SSRC に見え、送り元が替わるときはキーフレームから）。

- 割り当ては安定: 今の割り当ては保ち、新しいトラックは同じ種類の空いた枠に入る。送り手が抜けた枠は空き（`state: "empty"`）、
  次の送り手に使い回す（m 行は増え続けない）。
- 足りなければ `renegotiate {audio, video}`（種類ごとの**合計**の枠の数）。クライアントは `recvonly` の transceiver を足して offer する。
- 枠の上限は種類ごとに 32（`ServiceConfig::max_slots_per_kind`）。
- 誰の映像かは `slots[].connection_id`・`client_id`・`track_id` で分かる（`ontrack` の transceiver の `mid` と突き合わせる）。
- `state`: `active`（配っている）・`paused`（割り当てているが配っていない。スポットライトの外で `unfocused: "off"`）・`empty`。

受けないもの: 自分のトラック、同じ `bundle_id` の接続のトラック、転送フィルター（§8）で外れたもの、役割が `sendonly`・
許可の `subscribe: false` の接続。`multistream: false` の接続は種類ごとに 1 本（スポットライトの注目、無ければ最初の送り手）。

## 4. message（クライアント → サーバー）

### `connect`（最初の message、10 秒以内）

| フィールド | 型 | 既定 | 意味 |
|---|---|---|---|
| `version` | 1 | 必須 | |
| `room_id` | string | 必須 | 1〜128 文字の英数字と `-_.:@` |
| `token` | string | | 参加トークン（§9）。認証 Webhook だけで認可するなら省ける |
| `role` | `sendrecv`・`sendonly`・`recvonly` | 必須 | トークンの `role` の範囲（`sendrecv` のトークンはどれでも） |
| `client_id` | string | | トークンに `client_id` があれば同じでなければならない |
| `bundle_id` | string | `connection_id` | 同じ値の接続どうしは互いのトラックを受けない（同じ人のカメラと画面共有） |
| `metadata` | JSON | | 認証 Webhook へだけ渡す（他の参加者には見せない） |
| `multistream` | bool | true | false で種類ごとに 1 本だけ受ける |
| `simulcast` | `{rids, default_rid?}` | | 送る層の rid（**低い順**、1〜4 個、英数字 16 文字まで）。`default_rid` は受けるときの最初の層 |
| `spotlight` | `{number, focus_rid?, unfocused?}` | 部屋の設定 | 部屋のスポットライトの範囲で人数・層を選ぶ |
| `audio` / `video` | `{enabled, codec?, bitrate_kbps?}` | | `codec` は部屋のコーデックと同じでなければ `unsupported_codec` |
| `data_channels` | `[{label, direction, ordered, max_retransmits?, max_packet_life_time?}]` | | 使う DataChannel（§6）。クライアントが同じラベルで開く |
| `signaling` | `{data_channel, close_websocket}` | false | DataChannel への切り替え（§6） |
| `e2ee` | bool | false | 部屋の `e2ee` と同じでなければ `e2ee_mismatch` |
| `forwarding_filter` | §8 | | この接続の購読を絞る |
| `sdk` | `{name, version}` | | ログ用 |

### その他

| `type` | フィールド | 意味 |
|---|---|---|
| `offer` | `sdp` | 送る m 行と受ける m 行の offer。BUNDLE・rtcp-mux 必須 |
| `candidate` | `candidate`, `sdp_mid?`, `sdp_mline_index?`, `username_fragment?` | trickle ICE（空文字列は end-of-candidates）。サーバーは ICE-lite なので省いてよい |
| `pong` | `stats?` | `ping` への答え。`stats` に getStats の要約（64 KiB まで）を載せてよい |
| `layer` | `track_id`, `rid`（`null` で自動） | 受けているトラックの層を選ぶ |
| `message` | `label`, `to?`, `data` か `data_base64` | アプリの message（`to` は connection_id の並び、空なら全員）。§6 |
| `stats` | `reports` | getStats の要約（API の統計に出る） |
| `disconnect` | `reason?` | 切断する |

## 5. message（サーバー → クライアント）

| `type` | フィールド | 意味 |
|---|---|---|
| `connected` | `version`, `connection_id`, `client_id`, `bundle_id`, `room_id`, `role`, `multistream`, `e2ee`, `video_codec`, `audio_codec`, `max_video_bitrate_kbps?`, `spotlight?`, `participants`, `recv_slots {audio, video}`, `data_channels`, `signaling_data_channel`, `ping_interval_ms`, `metadata` | 参加した。`role`・`metadata` は認証 Webhook が変えた後の値。最初の offer には `recv_slots` の数の受ける m 行を入れる |
| `answer` | `sdp`, `slots[]`, `published[] {mid, track_id, kind, rids}` | offer への答えと、全部の枠の割り当て |
| `slots` | `slots[]` | 変わった枠だけ |
| `renegotiate` | `audio`, `video` | 枠をこの数まで足して offer し直す |
| `notify` | `event` と中身（下） | 部屋の出来事 |
| `push` | `data` | API からのプッシュ |
| `message` | `label`, `from?`, `data` か `data_base64` | シグナリングで届いたアプリの message |
| `ping` | | 10 秒ごと。10 秒以内に `pong` が無ければ `ping_timeout` で切断 |
| `switched` | | 以後のシグナリングは DataChannel `signaling`（§6） |
| `redirect` | `location` | 別のノードへつなぎ直す（この後 `disconnect {reason: "redirected"}` と close） |
| `error` | `code`, `message`, `fatal`, `request?` | §7 |
| `disconnect` | `reason` | 切断する（この後 close） |

`notify` の `event`:

| `event` | 中身 |
|---|---|
| `connection.created` | `participant {connection_id, client_id, bundle_id, role, metadata, tracks[], node?}`（`node` は別のノードの参加者） |
| `connection.updated` | `connection_id`, `metadata` |
| `connection.destroyed` | `connection_id`, `client_id?`, `reason` |
| `track.added` | `track {track_id, connection_id, kind, rids[]}` |
| `track.removed` | `track_id`, `connection_id` |
| `spotlight.changed` | `focused[]`（connection_id、新しく注目した順） |
| `layer.changed` | `track_id`, `mid`, `rid`（今受けている層） |
| `network.status` | `level`（0 よい〜3 悪い）, `bandwidth_bps`, `rtt_ms`（自分の回線、2 秒に 1 回まで） |

## 6. DataChannel

- **アプリの message**: ラベルが `#` で始まる DataChannel。`connect.data_channels` で宣言し、**クライアントが開く**
  （`pc.createDataChannel('#chat', {ordered, maxRetransmits})`）。宣言していない・部屋の `data_labels`・トークンの
  `permissions.data` に合わないラベルはサーバーが閉じる。
- 届いた message は、同じラベルを開いていて `direction` が受ける（`sendrecv`・`recvonly`）**他の全員**へそのまま
  （text は text、binary は binary）配る。送り手には返さない。`direction: "recvonly"` の接続からの message は配らない。
- 特定の相手へは シグナリングの `message {label, to: [connection_id], data}`。受け手には `message {label, from, data}` が届く。
- **シグナリングの切り替え**: `signaling: {data_channel: true}` と `data_channels: [{label: "signaling"}]` を付け、
  クライアントが `signaling` を開くと、サーバーは WebSocket で `switched` を送り、以後は DataChannel で送る。
  クライアントも以後は DataChannel で送ってよい（WebSocket で送ったものも受ける）。
  `close_websocket: true` なら WebSocket を閉じても切断しない（false なら切断）。DataChannel が閉じたら WebSocket に戻り、
  それも閉じていれば切断。

## 7. 誤り

```json
{"type":"error","code":"room_full","message":"room is full","fatal":true,"request":"connect"}
```

`fatal: true` の後は `disconnect {reason: "error"}` を送り、WebSocket を下の code で閉じる。`fatal: false` は
その message だけを拒み、接続は続く（参加の後の `bad_message`・`invalid_state`・2 回目以降の offer の誤りなど）。

| `code` | close | いつ |
|---|---|---|
| `bad_message` | 4400 | JSON・型・未知のフィールド・値の範囲（参加の前なら fatal） |
| `unsupported_version` | 4422 | `version` が 1 でない |
| `auth_failed` | 4401 | トークンが無い・不正・期限切れ、認証 Webhook の拒否・時間切れ（5 秒） |
| `forbidden` | 4403 | 別の部屋のトークン、役割・client_id がトークンと合わない、許可の無い種類を送る |
| `room_not_found` | 4404 | 部屋が無く、自動で作らない設定 |
| `room_full` | 4503 | 部屋・ノードの上限（ノードに `overflow_redirect` があれば `redirect`） |
| `role_violation` | 4403 | `recvonly` が送る m 行を出した |
| `e2ee_mismatch` | 4422 | `connect.e2ee` と部屋の `e2ee` が違う |
| `unsupported_codec` | 4422 | 部屋のコーデックと違う |
| `invalid_sdp` | 4400 | SDP を読めない・mid の無い m 行 |
| `negotiation_failed` | 4500 | answer を作れない（BUNDLE・rtcp-mux・コーデック） |
| `invalid_state` | 4400 | 最初が `connect` でない、offer の処理中の offer、認証中の message |
| `rate_limited` | 4429 | 1 秒に 50 を超えた |
| `unknown_track` | 4404 | `layer` の track_id・rid が無い・受けていない |
| `unknown_label` | 4403 | 使えないラベルの `message` |
| `timeout` | 4408 | 10 秒以内に `connect` が無い |
| `internal` | 4500 | サーバーの誤り |

切断の理由（`disconnect.reason`・`connection.destroyed.reason`・イベント）: `normal`・`websocket_closed`・`ping_timeout`・
`peer_failed`（ICE・DTLS の失敗、参加から 30 秒つながらない）・`kicked`（API）・`room_deleted`・`error`・`redirected`・`node_shutdown`。

## 8. 転送フィルター

```json
{"action":"block","rules":[[{"field":"kind","operator":"is_in","values":["video"]},
                            {"field":"client_id","operator":"is_not_in","values":["teacher"]}]]}
```

`rules` は AND の組の OR（1〜10 組、各 1〜10 規則）。規則は**送り手**の `connection_id`・`client_id`・`bundle_id`・`kind`・
`metadata.<key>`（文字列・数・真偽）を見る。`block` は一致したものを配らず、`allow` は一致したものだけ配る。
部屋の設定のフィルターと接続のフィルターの両方を満たすものだけ配る。部屋のフィルターを変えると、その場で枠を割り当て直す。

## 9. 認証

### 参加トークン（JWT 互換）

SENCA の署名鍵（Ed25519 = `EdDSA`、P-256 = `ES256`）で署名する。`POST /rooms/{id}/tokens` で発行できる。
`none`・HS 系（共有鍵）・RS 系・`crit` は拒否、鍵が算法を決める（ヘッダーの `alg` が違えば拒否）。

```json
{"iss":"senca","aud":"senca-rooms","exp":1767225600,"iat":1767222000,"jti":"...",
 "room":"daily","role":"sendrecv","client_id":"alice","metadata":{"name":"Alice"},
 "permissions":{"publish":["audio","video"],"subscribe":true,"data":["#chat"]}}
```

`exp` 必須（時計のずれ 30 秒まで）、寿命（`exp - iat`）は 24 時間まで。`metadata` は他の参加者に見える。
`permissions` の既定は「両方送れる・受けられる・`#*`」。

### 認証 Webhook

`ServiceConfig::auth_webhook` が `Only`（トークンの代わり）か `AfterToken`（トークンの後にも）のとき、ノードは
`POST` で問い合わせる（本文は署名する、下）。5 秒で答えが無ければ拒否。

```json
{"version":1,"type":"connection.auth","request_id":"…","node":"node-a","room_id":"daily",
 "connection_id":"…","client_id":"alice","role":"sendrecv","metadata":{…クライアントの connect.metadata…},
 "token_claims":{…あれば…},"remote_addr":"203.0.113.5:53211"}
```

応答（2xx、未知のフィールドは拒否）:

```json
{"allowed":true,"role":"recvonly","client_id":"alice","metadata":{"name":"Alice"},
 "permissions":{"publish":[],"subscribe":true,"data":["#chat"]}}
```

`allowed: false` なら `reason` をクライアントへ（`auth_failed`）。2xx 以外・読めない本文も拒否。

### 署名（Webhook 共通）

`Senca-Signature: t=<UNIX 秒>,v1=<hex(HMAC-SHA256(secret, "<t>.<本文>"))>`。秘密を入れ替える間は `v1=` を複数付ける。
受け手は `|now - t| <= 300` 秒と、どれかの `v1` の一致を確かめる（`senca_rooms::webhook::verify_signature`）。
イベントの Webhook には `Senca-Webhook-Id`（event_id）と `Senca-Webhook-Attempt` も付ける。

## 10. イベント（SSE と Webhook）

同じ封筒を SSE（`/api/v1/events`）と Webhook に出す。

```json
{"id":"01j…","version":1,"at":1767222000123,"node":"node-a","room_id":"daily",
 "type":"connection.destroyed","data":{"connection_id":"…","client_id":"alice","reason":"normal","duration_ms":61234}}
```

`type`: `room.created`・`room.updated`・`room.destroyed`・`connection.created`・`connection.updated`・`connection.destroyed`・
`track.published`・`track.unpublished`・`spotlight.changed`。

Webhook は**少なくとも 1 回**（受け手は `id` で重複を除く）。2xx で完了。408・429・5xx・接続の失敗・10 秒の時間切れは
1, 2, 4, … 秒（±20 %、最大 1 時間）で送り直し、10 回で諦める（dead letter）。その他の 4xx はすぐ諦める。
（`senca_rooms::webhook::{Outbox, retry_delay}`）

## 11. REST

`/api/v1` の共通の契約（Bearer、problem+json、`Idempotency-Key`、`If-Match`・`ETag`、`limit`・`cursor`）に従う。

| 操作 | パス |
|---|---|
| 部屋を作る | `POST /rooms` `{id?, settings?, metadata?}` → 201 |
| 一覧・取得 | `GET /rooms?limit&cursor`・`GET /rooms/{id}`（`ETag: "{generation}"`） |
| 更新 | `PATCH /rooms/{id}` `{settings?, metadata?}`（`If-Match` 必須。参加者がいる間は `e2ee`、トラックがある間はコーデックを変えられない: 409） |
| 削除 | `DELETE /rooms/{id}`（参加者は `room_deleted`） |
| 参加者 | `GET /rooms/{id}/participants`・`GET …/{connection_id}`（`subscriptions` は枠） |
| 参加者の metadata | `PATCH /rooms/{id}/participants/{connection_id}` `{metadata}` → `connection.updated` |
| 切断 | `DELETE /rooms/{id}/participants/{connection_id}`（`kicked`） |
| 送る | `POST /rooms/{id}/messages` `{to?, label?, data}`（`label` があれば DataChannel、無ければ `push`） |
| 統計 | `GET /rooms/{id}/stats`（参加者ごとの RTT・推定帯域・送受の量・送る層の bitrate・受けている層・クライアントの getStats） |
| 参加トークン | `POST /rooms/{id}/tokens` `{role, client_id?, metadata?, permissions?, ttl_secs?}` |

部屋の設定（`settings`）: `max_participants`（50）・`e2ee`・`video_codec`（`vp8`・`vp9`・`h264`・`av1`）・`audio_codec`（`opus`）・
`max_video_bitrate_kbps`・`spotlight {number, unfocused: "lowest"|"off", focus_rid?}`・`recording`（`none`・`per_participant`・
`composite`。E2EE とは併用できない）・`forwarding_filter`・`data_labels`（`["#*"]`）・`empty_timeout_secs`（60、参加で自動に作った部屋）。

E2EE の部屋（SFrame）では SFU は中身を見ずに配る。サーバーでの合成・合成の録画・文字起こしは計画の時点で 422 と理由を返す
（`RoomView.compositing`、`RoomService::api_plan_composite`）。

## 12. 層の選択とスポットライト

受け手ごと・トラックごとに層を選ぶ。優先の順: クライアントの `layer` → スポットライト（注目は `focus_rid` か自動、
注目の外は一番低い層か `off`）→ `simulcast.default_rid` → 自動（推定帯域 × 0.9 を映像の購読の数で分け、収まる最も高い層。
下げるのは 1 秒続いてから（大きく足りなければすぐ）・上げるのは 2 秒続いてから）。切り替えは新しい層のキーフレームで（それまで今の層を配る）。

スポットライトは音量（RFC 6464 の -dBov。無ければ Opus の packet の大きさからの見積もり）で話者を検出する:
200 ms 続けば話している、1.5 秒黙れば話していない、注目してから 2 秒は外さない（揺れを防ぐ）。注目の組は部屋で 1 つ
（`spotlight.changed`）、受け手は自分を除いて `number` 人。

## 13. カスケード（グリッド）

部屋は複数のノードにまたがれる（ADR-0013・0014）。他のノードの参加者は `node` 付きの参加者として見え、
そのトラックは同じ枠に割り当てる。ノード間で運ぶのは、そのノードの購読者が要る層だけ（自動で選ぶ購読者がいれば全部の層、
固定の層だけならその層）。キーフレームの要求・アプリの message も中継する。
部屋がこのノードで受けられないとき（グリッドの配置・上限）は `redirect {location}` で別のノードへ。

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
