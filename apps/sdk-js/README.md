# @senca/sdk（apps/sdk-js）

[English](README.en.md) | 日本語

SENCA のクライアント SDK（JavaScript・TypeScript）。

- **リアルタイムの部屋**（ブラウザー）: `Senca.connect()` で部屋に入り、公開・受信・メッセージ・層の選択・統計を扱う。
  シグナリング v1（[docs/realtime/signaling.md](../../docs/realtime/signaling.md)）の SDP・受信の枠・再交渉・ping・再接続は SDK の中に隠す。
- **管理 API**（ブラウザー・Node.js 20 以上）: `new SencaClient()` で `/api/v1`（[docs/api/openapi.yaml](../../docs/api/openapi.yaml)）を型付きで呼ぶ。
  Idempotency-Key・If-Match・cursor の頁・problem+json・再試行・SSE を SDK が持つ。

実行時の依存は無い。ソースは JSDoc の型を付けた素の ES modules（`src/`、ビルドせずにそのまま動く）で、型定義（`types/`）は
`tsc` が JSDoc から作る。API の一覧は [API.md](API.md)（型定義から生成）。

```
apps/sdk-js/
  src/index.js            入口（Senca・SencaClient・誤りの型）
  src/realtime/           部屋: session.js（状態機械）・objects.js（Participant・RemoteTrack・Publication）・media.js・protocol.js
  src/rest/               管理 API: client.js（資源）・http.js（芯）・sse.js・openapi.gen.d.ts / operations.gen.js（生成物）
  types/                  型定義（scripts/build.mjs が作る）
  test/                   単体試験（node --test）・test/live（本物のノード）・test/interop（Chrome のページ）
  scripts/gen_types.py    openapi.yaml → src/rest/*.gen.*
  scripts/build.mjs       型の検査・types/・API.md
```

## 部屋（ブラウザー）

```js
import { Senca } from '@senca/sdk';

const camera = await navigator.mediaDevices.getUserMedia({ video: { width: 1280, height: 720 }, audio: true });
const session = await Senca.connect({
  url: 'wss://node.example/signaling',      // REST の Room.signaling_url / RoomToken.signaling_url
  room: 'daily',
  token: () => fetch('/my-backend/room-token').then((r) => r.text()),   // 再接続のたびに取り直す
  publish: { stream: camera, simulcast: true },                       // 参加と同時に公開（最初の offer に入る）
  dataChannels: ['#chat'],
  signal: abortController.signal,                                       // 取り消すと閉じる
});

// 受けたトラック（今あるもの → これから来るもの）
for await (const track of session.remoteTracks()) {
  const video = track.attach(document.createElement('video'));
  document.body.append(video);
  track.ended.then(() => video.remove());
}
```

別々の非同期の流れとして書ける（コールバックを登録しない）:

```js
// 出来事
for await (const ev of session.events({ types: ['participant.joined', 'participant.left', 'reconnecting'] })) {
  if (ev.type === 'participant.joined') console.log('joined', ev.participant.clientId, ev.participant.metadata);
}

// メッセージ（オブジェクトは JSON で送られる）
await session.send('#chat', { text: 'hello' });                    // 部屋の全員（DataChannel）
await session.send('#chat', { text: 'psst' }, { to: participant }); // 1 人へ（シグナリング経由、受け手に from が付く）
for await (const m of session.messages('#chat')) console.log(m.sender?.clientId, m.json());

// サイマルキャストの層（サーバーが切り替えたら解決）
const t = [...session.tracks.values()].find((t) => t.kind === 'video');
await t.setLayer('r0');          // 'r0' | 'r1' | 'r2' | 'auto'
for await (const e of t.events()) if (e.type === 'layer') console.log('now', e.rid);

// 1 つの出来事を待つ
const { participant } = await session.waitFor('participant.joined', { timeoutMs: 30_000 });

// 統計（型付きの要約と getStats そのもの）
const st = await session.stats();
st.inbound.forEach((i) => console.log(i.clientId, i.kind, i.layer, i.framesDecoded, i.bitrateBps));

// 後から公開・やめる・差し替える
const screen = await session.publish(await navigator.mediaDevices.getDisplayMedia(), { maxBitrateKbps: 2500 });
await screen.video.replace(otherTrack);
await screen.setMaxBitrate(1200);
await screen.unpublish();

await session.close();   // または `await using session = await Senca.connect(...)`
```

`session.participants`（connection_id → Participant）と `session.tracks`（トラック ID → RemoteTrack）はその場で変わる Map。
`session.spotlight.focused`・`participant.focused` でスポットライトの注目、`session.network` で回線の状態（`network.status`）が分かる。

### 生のプロトコルとの比べ

シグナリング v1 を直に書くと、利用者が次をすべて持つ（`crates/senca-rooms/examples/rooms_server.rs` の試験ページがその例で、約 150 行）:

| やること | 生のプロトコル | SDK |
|---|---|---|
| 参加 | WebSocket（subprotocol `senca.rooms.v1`）を開き `connect` を送り、`connected.recv_slots` の数だけ recvonly の transceiver を足して offer、answer を入れる | `await Senca.connect({...})` |
| コーデック | 部屋の `video_codec` に合わせて `setCodecPreferences`（違えば `unsupported_codec`） | 自動 |
| 新しい送り手 | `renegotiate {audio, video}` で transceiver を足して offer し直す。answer の前に次の offer を出すと `invalid_state` なので待つ | 自動（offer は 1 つずつ、待つ間の要求はまとめる） |
| 誰の映像か | `answer.slots`・`slots` の `mid` を `ontrack` の transceiver と突き合わせ、送り手の入れ替わり・`paused`・`empty` を追う | `RemoteTrack`（`participant`・`state`・`ended`） |
| 層の選択 | `{type: 'layer', track_id, rid}` を送り、`layer.changed` を待つ。誤りは `request: 'layer'` の `error` | `await track.setLayer('r2')` |
| 1 人へのメッセージ | `{type: 'message', label, to, data \| data_base64}` | `session.send(label, data, {to})` |
| 生存確認 | `ping` に 10 秒以内に `pong`（遅れると `ping_timeout`） | 自動（ping が来なければ切れたとみなす見張りも） |
| 切断・誤り | `error.fatal`・close code・`disconnect.reason` を読み分ける | `SencaError`（`code`・`fatal`）、`session.closed` |
| 再接続 | 自分で | 指数バックオフで入り直す（下） |
| redirect | `redirect.location` へつなぎ直す | 自動（3 回まで） |
| DataChannel への切り替え | `signaling` を開き、`switched` の後は DataChannel で送る | `signaling: {dataChannel: true, closeWebSocket: true}` |

### 再交渉と再接続の扱い

- **offer は常に SDK が出す**（サーバーは answer だけ）。参加時は `recv_slots`（受ける人がいれば最低 1 本ずつ）の recvonly の
  transceiver を用意して 1 回で交渉する。`renegotiate` は種類ごとの合計の数まで transceiver を足して offer し直す。
  交渉は直列（answer を待つ間の `renegotiate`・`publish` は、answer の後に 1 回の offer にまとめる）。answer の誤り（`fatal: false`）は
  ロールバックしてその操作（`publish()` など）を失敗させる。answer が 15 秒来なければ接続が失われたとみなす。
- **ICE**: ピア接続が `disconnected` のまま 3 秒、または `failed` になったら、シグナリングが生きていれば **ICE restart**
  （`createOffer({iceRestart: true})` を同じ手順で交渉）。10 秒で戻らなければ参加し直す。出来事 `ice.restarting`・`ice.restarted`。
- **参加し直し**: プロトコルに再開（resume）は無いので、シグナリングが切れた（`websocket_closed`・`ping_timeout`・`peer_failed`・
  `node_shutdown`・ネットワークの断）ときは**新しい接続として入り直す**。待ちは 0.5 秒から倍々で最大 10 秒（±20 %）、10 回まで
  （`reconnect: {maxAttempts, initialDelayMs, maxDelayMs}`、`false` で入り直さない）。`token` が関数なら取り直す。
  `url` を複数書けば順に替える。入り直すと connection_id が変わり、公開は最初の offer に張り直し（`LocalTrack.id` が新しくなる）、
  受けていたトラックは `ended`（理由 `reconnecting`）になって新しい RemoteTrack が届く。出来事は `reconnecting`（回数・待ち・理由）と `reconnected`。
- **入り直さないもの**: `fatal` な誤りのうち直らないもの（`auth_failed`・`forbidden`・`room_not_found`・`role_violation`・`e2ee_mismatch`・
  `unsupported_codec`・`bad_message` など）、`disconnect` の `kicked`・`room_deleted`、`close()`。`session.closed` が理由と誤りで解決する。
- **redirect**: 参加の前なら案内された先へそのまま入る。参加の後（部屋の置き場所が変わった）なら待たずに入り直す。

### 誤り

すべて `SencaError`（`code`・`fatal`・`closeCode`・`request`）。サーバーの `error.code`（`room_full` など）はそのまま `code` に入る。
SDK の code は `connection_failed`・`closed`・`not_connected`・`kicked`・`room_deleted`・`message_too_large`・`invalid_argument`・
`unsupported`・`too_many_redirects`・`timeout`。取り消し（`signal`）は標準どおり `AbortError`（DOMException）。

## 管理 API（ブラウザー・Node.js）

```js
import { SencaClient, ApiError } from '@senca/sdk';

const senca = new SencaClient({ baseUrl: 'https://node.example', token: process.env.SENCA_TOKEN });

const s = await senca.streams.create({ name: 'studio-a' });            // Idempotency-Key は自動（再試行しても 1 回）
for await (const x of senca.streams.list({ limit: 100 })) console.log(x.name);   // cursor を自動でたどる
const cur = await senca.streams.get(s.id);
await senca.streams.update(s.id, { metadata: { a: 'b' } }, { ifMatch: cur });     // If-Match は ETag・generation・資源のどれでも
await senca.streams.modify(s.id, (c) => ({ metadata: { ...c.metadata, n: '1' } })); // 412 なら読み直してやり直す

try {
  await senca.streams.update(s.id, { name: 'x' }, { ifMatch: 1 });
} catch (e) {
  if (e instanceof ApiError && e.code === 'PRECONDITION_FAILED') { /* 他の人が先に書いた */ }
}

for await (const ev of senca.events({ types: ['stream.*'], signal })) {   // SSE（切れたら Last-Event-ID で付け直す）
  if (ev.type === 'resync') await reloadAll(); else console.log(ev.type, ev.data.resource_id);
}

// 部屋: トークンを作ってブラウザーへ（signaling_url が Senca.connect の url）
const room = await senca.rooms.create({ id: 'daily', settings: { video_codec: 'vp8', max_participants: 8 } });
const { token, signaling_url } = await senca.rooms.createToken('daily', { role: 'sendrecv', client_id: 'alice', metadata: { name: 'Alice' } });
await senca.rooms.participants('daily').updateMetadata(connectionId, { name: 'Alice (host)' });  // 参加者に connection.updated
```

資源: `streams`（`rotateKey`・`playbackUrl`・`startRecording`・`plan`・`snapshot`・`snapshots`）・`recordings`（`stop`・`playbackUrl`・`thumbnail`・
`exportMp4`）・`schedules`（`check`・`runs`）・`tokens`・`inputs`・`outputs`（stream ごとに作る）・`secrets`・`pipelines`（`validate`・`control`）・
`nodes`（`drain`・`undrain`・`devices.disable/enable`）・`rooms`（`participants(id)`・`sendMessage`・`stats`・`createToken`）・
`mediaPipelines`（部屋の書き出し・録画・合成・送り出し・文字起こし。`list({room})`・`start`・`stop`）と、
`capabilities()`・`devices()`・`grid()`・`events()`。仕様のすべての操作に SDK のメソッドがある（`test/rest.test.mjs` が
`operations.gen.js` と突き合わせる）。まだ SDK に無い操作は `senca.request(method, path, opts)` で呼べる。

- **再試行**: 通信の失敗・408・429・502・503・504・problem の `retryable: true` を、`Retry-After` と指数バックオフで送り直す（既定は最初の要求を含めて 3 回まで）。
  本文を作ったかもしれない失敗は GET・DELETE と Idempotency-Key 付きの POST だけ送り直す（PATCH は 429・503 だけ）。
- **誤り**: problem+json は `ApiError`（`status`・`code`・`title`・`requestId`・`retryable`・`retryAfterMs`・`details`・`problem`）。
  通信の失敗は `status: 0`・`code: 'NETWORK_ERROR' | 'TIMEOUT'`。
- **型**: 資源の型は openapi.yaml から生成（`types/rest/openapi.gen.d.ts`）。仕様が変わったら `npm run gen`（`python3 scripts/gen_types.py`）。

## 試験

```
npm test                                 # = node --test "test/*.test.mjs"（偽の WebSocket・RTCPeerConnection・fetch、オフライン）
SENCA_URL=... SENCA_TOKEN=... node --test test/live/node.live.mjs   # 本物のノードに対して
npm run build                            # tsc で型の検査、types/・API.md を作る（TypeScript 5.x は devDependencies にある: npm install。別の tsc は TSC=...）
tools/interop/sdk_js.sh                  # 上の全部 + 本物の senca-node + Docker の Chrome 3 ページ（rooms_server と senca-node の両方）
```

`tools/interop/sdk_js.sh` の部屋の試験: 3 つのページ（alice はサイマルキャスト 3 層）が SDK だけで入り、互いの映像を復号し
（`session.stats()` の framesDecoded）、`#chat` を DataChannel と宛先つき（シグナリング）で往復させ、carol が alice の層を
`r2` → `r0` と選んで復号の幅が 1280 → 320 になることを確かめ、順に `close()` して残った人に `participant.left` が届き部屋が空になることを見る。

## 限界・まだ無いもの

- **自分の metadata の変更**: シグナリング v1 にクライアントから metadata を変える message が無い。アプリのサーバーから
  `senca.rooms.participants(room).updateMetadata(connectionId, metadata)` で変える（参加者に `connection.updated` が届く）。
- **再開（resume）**: プロトコルに無いので、切れたら入り直す（connection_id・トラック ID が変わる）。
- **DataChannel で部屋全体に送ったメッセージには送り手が付かない**（サーバーが中身をそのまま配るため `from: null`）。送り手が要るなら
  `{to}` で送るか本文に入れる。
- **E2EE**: `e2ee: true` は部屋の設定との照合だけ。SFrame の暗号化・復号（Encoded Transform）は SDK に無い。
- **trickle ICE は送らない**（サーバーは ICE-lite で、クライアントからの接続確認でアドレスを知る）。
- 1 つの `publish()` に映像・音声は 1 本ずつ（画面共有は別の `publish()` か、`bundleId` を同じにした別のセッション）。
- 確かめたブラウザーは Chrome だけ（Docker の ov-chrome）。Firefox・Safari は未確認。
