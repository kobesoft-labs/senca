# @senca/sdk (apps/sdk-js)

English | [日本語](README.md)

The SENCA client SDK for JavaScript and TypeScript.

- **Realtime rooms** (browsers): `Senca.connect()` joins a room and handles publishing, receiving, messages, layer selection and statistics.
  The SDP, receive slots, renegotiation, ping and reconnection of signaling v1 ([docs/realtime/signaling.md](../../docs/realtime/signaling.md), Japanese) are hidden inside the SDK.
- **Management API** (browsers and Node.js 20+): `new SencaClient()` calls `/api/v1` ([docs/api/openapi.yaml](../../docs/api/openapi.yaml)) with types.
  The SDK takes care of Idempotency-Key, If-Match, cursor pagination, problem+json, retries and SSE.

There are no runtime dependencies. The source is plain ES modules with JSDoc types (`src/`, runs as is without a build step), and the
type definitions (`types/`) are generated from the JSDoc by `tsc`. The API reference is [API.md](API.md) (generated from the type definitions).

```
apps/sdk-js/
  src/index.js            entry point (Senca, SencaClient, error types)
  src/realtime/           rooms: session.js (state machine), objects.js (Participant, RemoteTrack, Publication), media.js, protocol.js
  src/rest/               management API: client.js (resources), http.js (core), sse.js, openapi.gen.d.ts / operations.gen.js (generated)
  types/                  type definitions (produced by scripts/build.mjs)
  test/                   unit tests (node --test), test/live (a real node), test/interop (Chrome pages)
  scripts/gen_types.py    openapi.yaml -> src/rest/*.gen.*
  scripts/build.mjs       type check, types/, API.md
```

## Rooms (browsers)

```js
import { Senca } from '@senca/sdk';

const camera = await navigator.mediaDevices.getUserMedia({ video: { width: 1280, height: 720 }, audio: true });
const session = await Senca.connect({
  url: 'wss://node.example/signaling',      // Room.signaling_url / RoomToken.signaling_url from the REST API
  room: 'daily',
  token: () => fetch('/my-backend/room-token').then((r) => r.text()),   // fetched again on every reconnect
  publish: { stream: camera, simulcast: true },                       // publish while joining (goes into the first offer)
  dataChannels: ['#chat'],
  signal: abortController.signal,                                       // aborting closes the session
});

// Received tracks (the ones already there, then the ones that arrive later)
for await (const track of session.remoteTracks()) {
  const video = track.attach(document.createElement('video'));
  document.body.append(video);
  track.ended.then(() => video.remove());
}
```

Everything can be written as separate async flows (no callbacks to register):

```js
// Events
for await (const ev of session.events({ types: ['participant.joined', 'participant.left', 'reconnecting'] })) {
  if (ev.type === 'participant.joined') console.log('joined', ev.participant.clientId, ev.participant.metadata);
}

// Messages (objects are sent as JSON)
await session.send('#chat', { text: 'hello' });                    // to everyone in the room (DataChannel)
await session.send('#chat', { text: 'psst' }, { to: participant }); // to one participant (via signaling; the receiver gets `from`)
for await (const m of session.messages('#chat')) console.log(m.sender?.clientId, m.json());

// Simulcast layers (resolves once the server has switched)
const t = [...session.tracks.values()].find((t) => t.kind === 'video');
await t.setLayer('r0');          // 'r0' | 'r1' | 'r2' | 'auto'
for await (const e of t.events()) if (e.type === 'layer') console.log('now', e.rid);

// Wait for a single event
const { participant } = await session.waitFor('participant.joined', { timeoutMs: 30_000 });

// Statistics (a typed summary plus the raw getStats report)
const st = await session.stats();
st.inbound.forEach((i) => console.log(i.clientId, i.kind, i.layer, i.framesDecoded, i.bitrateBps));

// Publish later, stop, replace
const screen = await session.publish(await navigator.mediaDevices.getDisplayMedia(), { maxBitrateKbps: 2500 });
await screen.video.replace(otherTrack);
await screen.setMaxBitrate(1200);
await screen.unpublish();

await session.close();   // or `await using session = await Senca.connect(...)`
```

`session.participants` (connection_id -> Participant) and `session.tracks` (track ID -> RemoteTrack) are live Maps that change in place.
`session.spotlight.focused` and `participant.focused` tell you the spotlight focus, and `session.network` the link quality (`network.status`).

### Compared with the raw protocol

Writing signaling v1 directly means the application owns all of the following (the test page of `crates/senca-rooms/examples/rooms_server.rs` is such an example, about 150 lines):

| Task | Raw protocol | SDK |
|---|---|---|
| Joining | Open a WebSocket (subprotocol `senca.rooms.v1`), send `connect`, add as many recvonly transceivers as `connected.recv_slots`, send an offer, apply the answer | `await Senca.connect({...})` |
| Codec | `setCodecPreferences` to match the room's `video_codec` (otherwise `unsupported_codec`) | Automatic |
| New senders | On `renegotiate {audio, video}`, add transceivers and offer again. Sending the next offer before the answer gives `invalid_state`, so you have to wait | Automatic (one offer at a time; requests that arrive while waiting are merged) |
| Whose video is it | Match the `mid` in `answer.slots` / `slots` against the transceiver from `ontrack`, and follow sender changes, `paused` and `empty` | `RemoteTrack` (`participant`, `state`, `ended`) |
| Layer selection | Send `{type: 'layer', track_id, rid}` and wait for `layer.changed`. Failures arrive as an `error` with `request: 'layer'` | `await track.setLayer('r2')` |
| Message to one participant | `{type: 'message', label, to, data \| data_base64}` | `session.send(label, data, {to})` |
| Keep-alive | Answer `ping` with `pong` within 10 s (otherwise `ping_timeout`) | Automatic (including a watchdog that treats missing pings as a lost connection) |
| Disconnects and errors | Tell apart `error.fatal`, the close code and `disconnect.reason` | `SencaError` (`code`, `fatal`), `session.closed` |
| Reconnecting | Up to you | Rejoins with exponential backoff (below) |
| Redirect | Reconnect to `redirect.location` | Automatic (up to 3 times) |
| Switching to a DataChannel | Open `signaling` and, after `switched`, send over the DataChannel | `signaling: {dataChannel: true, closeWebSocket: true}` |

### How renegotiation and reconnection work

- **The SDK always makes the offer** (the server only answers). On join it prepares `recv_slots` recvonly transceivers (at least one
  per kind if anyone will receive) and negotiates once. On `renegotiate` it adds transceivers up to the total per kind and offers again.
  Negotiation is serialized (`renegotiate` and `publish` calls made while waiting for an answer are merged into one offer after the answer).
  An error in the answer (`fatal: false`) is rolled back and fails that operation (`publish()` and so on). If no answer arrives within
  15 seconds the connection is treated as lost.
- **ICE**: if the peer connection stays `disconnected` for 3 seconds, or becomes `failed`, and signaling is still alive, the SDK performs an
  **ICE restart** (`createOffer({iceRestart: true})`, negotiated the same way). If it does not recover within 10 seconds, the SDK rejoins.
  Events: `ice.restarting`, `ice.restarted`.
- **Rejoining**: the protocol has no resume, so when signaling is lost (`websocket_closed`, `ping_timeout`, `peer_failed`,
  `node_shutdown`, network loss) the SDK **joins again as a new connection**. The wait starts at 0.5 seconds and doubles up to 10 seconds
  (±20 %), for up to 10 attempts (`reconnect: {maxAttempts, initialDelayMs, maxDelayMs}`; `false` disables rejoining). If `token` is a
  function, it is called again. If `url` lists several URLs, they are tried in turn. Rejoining changes the connection_id, publications
  are re-established in the first offer (`LocalTrack.id` changes), and tracks that were being received end (`ended`, reason
  `reconnecting`) and arrive again as new RemoteTracks. Events: `reconnecting` (attempt, delay, reason) and `reconnected`.
- **When the SDK does not rejoin**: `fatal` errors that a retry cannot fix (`auth_failed`, `forbidden`, `room_not_found`,
  `role_violation`, `e2ee_mismatch`, `unsupported_codec`, `bad_message` and so on), the `disconnect` reasons `kicked` and `room_deleted`,
  and `close()`. `session.closed` resolves with the reason and the error.
- **Redirect**: before joining, the SDK goes straight to the indicated location. After joining (the room has moved), it rejoins without waiting.

### Errors

Every error is a `SencaError` (`code`, `fatal`, `closeCode`, `request`). The server's `error.code` (`room_full` and so on) goes into `code` unchanged.
The SDK's own codes are `connection_failed`, `closed`, `not_connected`, `kicked`, `room_deleted`, `message_too_large`, `invalid_argument`,
`unsupported`, `too_many_redirects` and `timeout`. Cancellation (`signal`) is reported as the standard `AbortError` (DOMException).

## Management API (browsers and Node.js)

```js
import { SencaClient, ApiError } from '@senca/sdk';

const senca = new SencaClient({ baseUrl: 'https://node.example', token: process.env.SENCA_TOKEN });

const s = await senca.streams.create({ name: 'studio-a' });            // Idempotency-Key is automatic (applied once even when retried)
for await (const x of senca.streams.list({ limit: 100 })) console.log(x.name);   // follows cursors automatically
const cur = await senca.streams.get(s.id);
await senca.streams.update(s.id, { metadata: { a: 'b' } }, { ifMatch: cur });     // If-Match takes an ETag, a generation or the resource
await senca.streams.modify(s.id, (c) => ({ metadata: { ...c.metadata, n: '1' } })); // on 412, reads again and retries

try {
  await senca.streams.update(s.id, { name: 'x' }, { ifMatch: 1 });
} catch (e) {
  if (e instanceof ApiError && e.code === 'PRECONDITION_FAILED') { /* someone else wrote first */ }
}

for await (const ev of senca.events({ types: ['stream.*'], signal })) {   // SSE (reconnects with Last-Event-ID when cut)
  if (ev.type === 'resync') await reloadAll(); else console.log(ev.type, ev.data.resource_id);
}

// Rooms: create a token and hand it to the browser (signaling_url is the url for Senca.connect)
const room = await senca.rooms.create({ id: 'daily', settings: { video_codec: 'vp8', max_participants: 8 } });
const { token, signaling_url } = await senca.rooms.createToken('daily', { role: 'sendrecv', client_id: 'alice', metadata: { name: 'Alice' } });
await senca.rooms.participants('daily').updateMetadata(connectionId, { name: 'Alice (host)' });  // participants get connection.updated
```

Resources: `streams` (`rotateKey`, `playbackUrl`, `startRecording`, `plan`, `snapshot`, `snapshots`), `recordings` (`stop`, `playbackUrl`, `thumbnail`,
`exportMp4`), `schedules` (`check`, `runs`), `tokens`, `inputs`, `outputs` (created per stream), `secrets`, `pipelines` (`validate`, `control`),
`nodes` (`drain`, `undrain`, `devices.disable/enable`), `rooms` (`participants(id)`, `sendMessage`, `stats`, `createToken`),
`mediaPipelines` (exporting, recording, compositing, pushing out and transcribing a room; `list({room})`, `start`, `stop`), plus
`capabilities()`, `devices()`, `grid()` and `events()`. Every operation in the specification has an SDK method (`test/rest.test.mjs` checks
this against `operations.gen.js`). Operations not yet in the SDK can be called with `senca.request(method, path, opts)`.

- **Retries**: network failures, 408, 429, 502, 503, 504 and problems with `retryable: true` are sent again, honouring `Retry-After`, with
  exponential backoff (3 attempts by default, including the first). Failures that may have created something are retried only for GET,
  DELETE and POST with an Idempotency-Key (PATCH only on 429 and 503).
- **Errors**: problem+json becomes `ApiError` (`status`, `code`, `title`, `requestId`, `retryable`, `retryAfterMs`, `details`, `problem`).
  Network failures have `status: 0` and `code: 'NETWORK_ERROR' | 'TIMEOUT'`.
- **Types**: resource types are generated from openapi.yaml (`types/rest/openapi.gen.d.ts`). When the specification changes, run
  `npm run gen` (`python3 scripts/gen_types.py`).

## Tests

```
npm test                                 # = node --test "test/*.test.mjs" (fake WebSocket, RTCPeerConnection and fetch; offline)
SENCA_URL=... SENCA_TOKEN=... node --test test/live/node.live.mjs   # against a real node
npm run build                            # type check with tsc, writes types/ and API.md (needs TypeScript 5.x: npm install or TSC=...)
tools/interop/sdk_js.sh                  # all of the above + a real senca-node + three pages in Chrome on Docker (against both rooms_server and senca-node)
```

The room test in `tools/interop/sdk_js.sh`: three pages (alice publishes three simulcast layers) join using only the SDK, decode each
other's video (framesDecoded from `session.stats()`), round-trip `#chat` both over the DataChannel and addressed (via signaling), carol selects
alice's layer `r2` -> `r0` and checks that the decoded width goes from 1280 to 320, and then each page calls `close()` in turn, checking that
the remaining participants receive `participant.left` and that the room ends up empty.

## Limitations and missing features

- **Changing your own metadata**: signaling v1 has no message for a client to change its metadata. Change it from the application server
  with `senca.rooms.participants(room).updateMetadata(connectionId, metadata)` (participants receive `connection.updated`).
- **Resume**: the protocol has none, so a lost connection rejoins (the connection_id and track IDs change).
- **Messages sent to the whole room over a DataChannel carry no sender** (the server forwards the payload as is, so `from: null`). If you
  need the sender, send with `{to}` or put it in the message body.
- **E2EE**: `e2ee: true` only checks against the room setting. SFrame encryption and decryption (Encoded Transform) are not in the SDK.
- **No trickle ICE is sent** (the server is ICE-lite and learns the address from the client's connectivity checks).
- One `publish()` carries at most one video and one audio track (screen sharing needs a separate `publish()`, or a separate session with the same `bundleId`).
- Chrome is the only browser tested (ov-chrome on Docker). Firefox and Safari are untested.
