# @senca/sdk（apps/sdk-js）

[English](README.en.md) | 日本語

SENCA のクライアント SDK（JavaScript・TypeScript）です。次の 2 つの機能を提供します。

- **リアルタイムの部屋**（ブラウザー）: `Senca.connect()` で部屋に参加し、メディアの公開・受信、メッセージ、レイヤーの選択、統計を扱います。
  シグナリング v1（[docs/realtime/signaling.md](../../docs/realtime/signaling.md)）の SDP・受信スロット・再ネゴシエーション・ping・再接続は、
  SDK が内部で処理します。
- **管理 API**（ブラウザー・Node.js 20 以上）: `new SencaClient()` で `/api/v1`（[docs/api/openapi.yaml](../../docs/api/openapi.yaml)）を
  型付きで呼び出します。Idempotency-Key・If-Match・カーソルによるページ送り・problem+json・再試行・SSE は SDK が処理します。

実行時の依存パッケージはありません。ソースは JSDoc で型を付けた素の ES modules で（`src/`、ビルドせずにそのまま動きます）、
型定義（`types/`）は `tsc` が JSDoc から生成します。API リファレンスは [API.md](API.md) です（型定義から生成）。

```
apps/sdk-js/
  src/index.js            エントリーポイント（Senca・SencaClient・エラーの型）
  src/realtime/           部屋: session.js（状態機械）・objects.js（Participant・RemoteTrack・Publication）・media.js・protocol.js
  src/rest/               管理 API: client.js（リソース）・http.js（中核）・sse.js・openapi.gen.d.ts / operations.gen.js（生成物）
  types/                  型定義（scripts/build.mjs が生成）
  test/                   単体テスト（node --test）・test/live（実際のノード）・test/interop（Chrome のページ）
  scripts/gen_types.py    openapi.yaml → src/rest/*.gen.*
  scripts/build.mjs       型の検査・types/・API.md
```

## 部屋（ブラウザー）

```js
import { Senca } from '@senca/sdk';

const camera = await navigator.mediaDevices.getUserMedia({ video: { width: 1280, height: 720 }, audio: true });
const session = await Senca.connect({
  url: 'wss://node.example/signaling',      // REST API の Room.signaling_url / RoomToken.signaling_url
  room: 'daily',
  token: () => fetch('/my-backend/room-token').then((r) => r.text()),   // 再接続のたびに取得し直す
  publish: { stream: camera, simulcast: true },                       // 参加と同時に公開（最初の offer に含まれる）
  dataChannels: ['#chat'],
  signal: abortController.signal,                                       // abort するとセッションを閉じる
});

// 受信したトラック（すでにあるもの → 後から届くもの）
for await (const track of session.remoteTracks()) {
  const video = track.attach(document.createElement('video'));
  document.body.append(video);
  track.ended.then(() => video.remove());
}
```

それぞれの処理を、独立した非同期の流れとして書けます（コールバックを登録する必要はありません）。

```js
// イベント
for await (const ev of session.events({ types: ['participant.joined', 'participant.left', 'reconnecting'] })) {
  if (ev.type === 'participant.joined') console.log('joined', ev.participant.clientId, ev.participant.metadata);
}

// メッセージ（オブジェクトは JSON として送信される）
await session.send('#chat', { text: 'hello' });                    // 部屋の全員へ（DataChannel）
await session.send('#chat', { text: 'psst' }, { to: participant }); // 1 人へ（シグナリング経由。受信側には from が付く）
for await (const m of session.messages('#chat')) console.log(m.sender?.clientId, m.json());

// サイマルキャストのレイヤー（サーバーが切り替えると解決）
const t = [...session.tracks.values()].find((t) => t.kind === 'video');
await t.setLayer('r0');          // 'r0' | 'r1' | 'r2' | 'auto'
for await (const e of t.events()) if (e.type === 'layer') console.log('now', e.rid);

// 1 つのイベントを待つ
const { participant } = await session.waitFor('participant.joined', { timeoutMs: 30_000 });

// 統計（型付きの要約と getStats の結果そのもの）
const st = await session.stats();
st.inbound.forEach((i) => console.log(i.clientId, i.kind, i.layer, i.framesDecoded, i.bitrateBps));

// 後から公開する・差し替える・公開をやめる
const screen = await session.publish(await navigator.mediaDevices.getDisplayMedia(), { maxBitrateKbps: 2500 });
await screen.video.replace(otherTrack);
await screen.setMaxBitrate(1200);
await screen.unpublish();

await session.close();   // または `await using session = await Senca.connect(...)`
```

`session.participants`（connection_id → Participant）と `session.tracks`（トラック ID → RemoteTrack）は、状態に合わせて随時更新される Map です。
`session.spotlight.focused`・`participant.focused` でスポットライトの注目対象が、`session.network` で回線の状態（`network.status`）がわかります。

### プロトコルを直接使う場合との比較

シグナリング v1 を直接使う場合、アプリケーションは次の処理をすべて自分で実装する必要があります
（`crates/senca-rooms/examples/rooms_server.rs` のテストページがその例で、約 150 行です）。

| 処理 | プロトコルを直接使う場合 | SDK |
|---|---|---|
| 参加 | WebSocket（subprotocol `senca.rooms.v1`）を開いて `connect` を送り、`connected.recv_slots` の数だけ recvonly の transceiver を追加して offer を送り、answer を適用する | `await Senca.connect({...})` |
| コーデック | 部屋の `video_codec` に合わせて `setCodecPreferences` を呼ぶ（合わないと `unsupported_codec`） | 自動 |
| 新しい配信者 | `renegotiate {audio, video}` を受けたら transceiver を追加して offer を送り直す。answer の前に次の offer を送ると `invalid_state` になるので待つ必要がある | 自動（offer は 1 つずつ送り、待っている間の要求はまとめる） |
| 誰の映像かの判別 | `answer.slots`・`slots` の `mid` を `ontrack` の transceiver と照合し、配信者の入れ替わり・`paused`・`empty` を追跡する | `RemoteTrack`（`participant`・`state`・`ended`） |
| レイヤーの選択 | `{type: 'layer', track_id, rid}` を送り、`layer.changed` を待つ。失敗は `request: 'layer'` の `error` で届く | `await track.setLayer('r2')` |
| 1 人へのメッセージ | `{type: 'message', label, to, data \| data_base64}` | `session.send(label, data, {to})` |
| 死活監視 | `ping` に 10 秒以内に `pong` を返す（遅れると `ping_timeout`） | 自動（ping が届かなくなったら切断とみなす監視も含む） |
| 切断・エラー | `error.fatal`・close code・`disconnect.reason` を区別して扱う | `SencaError`（`code`・`fatal`）、`session.closed` |
| 再接続 | 自分で実装する | 指数バックオフで参加し直す（後述） |
| リダイレクト | `redirect.location` へ接続し直す | 自動（3 回まで） |
| DataChannel への切り替え | `signaling` を開き、`switched` の後は DataChannel で送る | `signaling: {dataChannel: true, closeWebSocket: true}` |

### 再ネゴシエーションと再接続の動作

- **offer は常に SDK が送ります**（サーバーは answer だけを返します）。参加時には `recv_slots` の数（受信する参加者がいれば、種類ごとに
  最低 1 本）の recvonly の transceiver を用意し、1 回のネゴシエーションで済ませます。`renegotiate` を受けると、種類ごとの合計数まで
  transceiver を追加して offer を送り直します。ネゴシエーションは 1 つずつ順に行います（answer を待つ間に呼ばれた `renegotiate`・`publish` は、
  answer の後に 1 回の offer にまとめます）。answer がエラー（`fatal: false`）の場合はロールバックし、その操作（`publish()` など）を失敗させます。
  15 秒以内に answer が届かない場合は、接続が失われたとみなします。
- **ICE**: ピア接続が 3 秒間 `disconnected` のまま、または `failed` になったとき、シグナリングが生きていれば **ICE restart** を行います
  （`createOffer({iceRestart: true})` を同じ手順でネゴシエーション）。10 秒以内に回復しなければ、参加し直します。
  イベントは `ice.restarting`・`ice.restarted` です。
- **参加し直し**: プロトコルには再開（resume）の仕組みがないため、シグナリングが切れたとき（`websocket_closed`・`ping_timeout`・`peer_failed`・
  `node_shutdown`・ネットワークの切断）は**新しい接続として参加し直します**。待ち時間は 0.5 秒から倍々に増えて最大 10 秒（±20 %）で、
  最大 10 回まで試みます（`reconnect: {maxAttempts, initialDelayMs, maxDelayMs}`。`false` で参加し直しを無効にします）。`token` が関数なら
  呼び出し直します。`url` に複数の URL を指定すると、順に切り替えて試します。参加し直すと connection_id が変わります。公開中のメディアは
  最初の offer で張り直され（`LocalTrack.id` が新しくなります）、受信中だったトラックは `ended`（理由 `reconnecting`）になり、新しい
  RemoteTrack として届き直します。イベントは `reconnecting`（試行回数・待ち時間・理由）と `reconnected` です。
- **参加し直さない場合**: 再試行しても解決しない `fatal` なエラー（`auth_failed`・`forbidden`・`room_not_found`・`role_violation`・
  `e2ee_mismatch`・`unsupported_codec`・`bad_message` など）、`disconnect` の理由が `kicked`・`room_deleted` のとき、および `close()` を
  呼んだとき。`session.closed` は理由とエラーで解決します。
- **リダイレクト**: 参加前であれば、指定された接続先へそのまま参加します。参加後（部屋の配置先が変わった場合）は、待たずに参加し直します。

### エラー

エラーはすべて `SencaError`（`code`・`fatal`・`closeCode`・`request`）です。サーバーの `error.code`（`room_full` など）は、そのまま `code` に入ります。
SDK 独自の code は `connection_failed`・`closed`・`not_connected`・`kicked`・`room_deleted`・`message_too_large`・`invalid_argument`・
`unsupported`・`too_many_redirects`・`timeout` です。中止（`signal`）は、標準どおり `AbortError`（DOMException）になります。

## 管理 API（ブラウザー・Node.js）

```js
import { SencaClient, ApiError } from '@senca/sdk';

const senca = new SencaClient({ baseUrl: 'https://node.example', token: process.env.SENCA_TOKEN });

const s = await senca.streams.create({ name: 'studio-a' });            // Idempotency-Key は自動（再試行しても 1 回だけ適用）
for await (const x of senca.streams.list({ limit: 100 })) console.log(x.name);   // カーソルを自動でたどる
const cur = await senca.streams.get(s.id);
await senca.streams.update(s.id, { metadata: { a: 'b' } }, { ifMatch: cur });     // If-Match には ETag・generation・リソースのいずれも指定できる
await senca.streams.modify(s.id, (c) => ({ metadata: { ...c.metadata, n: '1' } })); // 412 なら読み直して再実行する

try {
  await senca.streams.update(s.id, { name: 'x' }, { ifMatch: 1 });
} catch (e) {
  if (e instanceof ApiError && e.code === 'PRECONDITION_FAILED') { /* 他のクライアントが先に更新した */ }
}

for await (const ev of senca.events({ types: ['stream.*'], signal })) {   // SSE（切れたら Last-Event-ID で再接続する）
  if (ev.type === 'resync') await reloadAll(); else console.log(ev.type, ev.data.resource_id);
}

// 部屋: トークンを発行してブラウザーへ渡す（signaling_url を Senca.connect の url に使う）
const room = await senca.rooms.create({ id: 'daily', settings: { video_codec: 'vp8', max_participants: 8 } });
const { token, signaling_url } = await senca.rooms.createToken('daily', { role: 'sendrecv', client_id: 'alice', metadata: { name: 'Alice' } });
await senca.rooms.participants('daily').updateMetadata(connectionId, { name: 'Alice (host)' });  // 参加者に connection.updated が届く
```

利用できるリソースは次のとおりです。`streams`（`rotateKey`・`playbackUrl`・`startRecording`・`plan`・`snapshot`・`snapshots`）・
`recordings`（`stop`・`playbackUrl`・`thumbnail`・`exportMp4`）・`schedules`（`check`・`runs`）・`tokens`・`inputs`・`outputs`（ストリームごとに作成）・
`secrets`・`pipelines`（`validate`・`control`）・`nodes`（`drain`・`undrain`・`devices.disable/enable`）・`rooms`（`participants(id)`・`sendMessage`・
`stats`・`createToken`）・`mediaPipelines`（部屋の書き出し・録画・合成・外部への送信・文字起こし。`list({room})`・`start`・`stop`）。
このほかに `capabilities()`・`devices()`・`grid()`・`events()` があります。API 仕様のすべての操作に対応する SDK のメソッドがあります
（`test/rest.test.mjs` が `operations.gen.js` と照合しています）。SDK にメソッドがない操作は、`senca.request(method, path, opts)` で呼び出せます。

- **再試行**: 通信の失敗・408・429・502・503・504・problem の `retryable: true` の場合は、`Retry-After` と指数バックオフに従って再送します
  （既定は最初のリクエストを含めて 3 回まで）。サーバー側で処理が行われた可能性のある失敗は、GET・DELETE と Idempotency-Key 付きの POST だけを
  再送します（PATCH は 429・503 のときだけ再送します）。
- **エラー**: problem+json は `ApiError`（`status`・`code`・`title`・`requestId`・`retryable`・`retryAfterMs`・`details`・`problem`）になります。
  通信の失敗は `status: 0`、`code: 'NETWORK_ERROR' | 'TIMEOUT'` です。
- **型**: リソースの型は openapi.yaml から生成します（`types/rest/openapi.gen.d.ts`）。API 仕様が変わったら `npm run gen`
  （`python3 scripts/gen_types.py`）を実行してください。

## テスト

```
npm test                                 # = node --test "test/*.test.mjs"（WebSocket・RTCPeerConnection・fetch はモック、オフラインで動作）
SENCA_URL=... SENCA_TOKEN=... node --test test/live/node.live.mjs   # 実際のノードに対して実行
npm run build                            # tsc で型を検査し、types/・API.md を生成（TypeScript 5.x は devDependencies にある: npm install。別の tsc を使うなら TSC=...）
tools/interop/sdk_js.sh                  # 上記すべて + 実際の senca-node + Docker 上の Chrome の 3 ページ（rooms_server と senca-node の両方）
```

`tools/interop/sdk_js.sh` が確認する内容は [docs/dev/notes/sdk-js-interop.md](../../docs/dev/notes/sdk-js-interop.md) にあります。

## 制限事項

- **自分の metadata の変更**: シグナリング v1 には、クライアントが自分の metadata を変更するメッセージがありません。アプリケーションのサーバーから
  `senca.rooms.participants(room).updateMetadata(connectionId, metadata)` で変更してください（参加者に `connection.updated` が届きます）。
- **再開（resume）**: プロトコルに再開の仕組みがないため、切断されたら参加し直します（connection_id とトラック ID は変わります）。
- **DataChannel で部屋全体に送ったメッセージには送信者の情報が付きません**（サーバーが内容をそのまま転送するため `from: null` になります）。
  送信者が必要な場合は、`{to}` を指定して送るか、メッセージ本文に含めてください。
- **E2EE**: `e2ee: true` は部屋の設定との照合だけを行います。SDK は SFrame の暗号化・復号（Encoded Transform）には対応していません。
- **trickle ICE は送信しません**（サーバーは ICE-lite で、クライアントからの接続確認によってアドレスを知ります）。
- 1 回の `publish()` で公開できるのは、映像・音声それぞれ 1 本までです（画面共有は別の `publish()` で行うか、`bundleId` を同じにした別のセッションを使います）。
- 動作を確認しているブラウザーは Chrome です（Docker の ov-chrome）。Firefox・Safari での動作は現在確認していません。
