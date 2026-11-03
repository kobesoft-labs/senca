# 15. Management API and SDKs

[日本語](../ja/15-api-and-sdks.md)

A SENCA node is managed over its HTTP management API (`/api/v1`); the web console (`/console/`) uses the same API. This page covers the
conventions shared by every endpoint, the OpenAPI document, the event stream (SSE), the JavaScript SDK (`@senca/sdk`) and the Rust
client (`senca-client`).

- Definition: [`docs/api/openapi.yaml`](../../api/openapi.yaml). It describes what the node actually does.
- Individual resources: streams and playback in [04-ingest-playback.md](04-ingest-playback.md), recordings in
  [05-recording.md](05-recording.md), inputs and outputs in [06-inputs-outputs.md](06-inputs-outputs.md), pipelines in
  [07-pipelines.md](07-pipelines.md), nodes and the grid in [09-cluster-grid.md](09-cluster-grid.md), rooms in [10-rooms.md](10-rooms.md),
  media pipelines in [11-media-pipelines.md](11-media-pipelines.md).

## 1. Conventions

### 1.1 Basics

| Item | Value |
|---|---|
| Base path | `/api/v1` on the `--http` listener (and on `--https` when TLS is enabled) |
| Bodies | JSON (UTF-8). Timestamps are UTC RFC 3339 (`2026-10-25T00:00:00.000Z`) |
| Body size limit | 1 MiB, enforced by the HTTP layer. Larger bodies get a `text/plain` 413 and the connection is closed (this one is not problem+json) |
| Response headers | Every response carries `X-Request-Id` and `Cache-Control: no-store` |
| `X-Request-Id` | Echoed from the request if it is at most 64 characters of ASCII letters, digits, `-`, `_` and `.`; otherwise a `req_...` value is generated |
| Paths | Paths with empty segments (`//`), `.` or `..` return 404 |
| Methods | An undefined method (including HEAD) on a known resource returns 405 (`METHOD_NOT_ALLOWED`); unknown paths return 404 |
| CORS | `/api/v1` sends no CORS headers. Call it from the same origin (as the console does) or from your own backend |

### 1.2 Authentication

Every `/api/v1` request, including `/capabilities`, needs `Authorization: Bearer <value>`. `/healthz`, `/readyz` and the signed playback
paths (`/s/...`) do not.

- A value **starting with `senca_` is an API token**; anything else is treated as a **JWT**. Values are limited to 8192 bytes.
- Failures return 401 (`AUTH_FAILED`, `WWW-Authenticate: Bearer`). The reason (expired, unknown token, ...) is never put in the response;
  it goes to the node log.

**API tokens**

- When the first node starts and no token exists yet, it creates one with `tenant:admin` and `node:admin` (name `bootstrap-admin`,
  tenant `default`, no expiry) and writes its value to `{data-dir}/control/bootstrap-admin-token` (mode 0600). In a cluster, the node
  with the lowest ID creates it.
- Further tokens are created with `POST /api/v1/tokens` (requires `tenant:admin`). The value (`token`) is returned only in the creation
  response; the node stores only a hash.

```
TOKEN=$(cat data/control/bootstrap-admin-token)
curl -s -X POST http://127.0.0.1:8080/api/v1/tokens \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"ci","scopes":["stream:read","stream:write"],"expires_at":"2027-01-01T00:00:00Z"}'
```

| Field | Rules |
|---|---|
| `name` | 1 to 64 characters (otherwise 400 `INVALID_CONFIG`) |
| `scopes` | At least one. Unknown scopes are 400. **You cannot grant a scope you do not hold yourself** (403 `FORBIDDEN`) |
| `expires_at` | Optional, RFC 3339. A time in the past is 400. Expired tokens get 401 |

- List and get never return the value. `DELETE /tokens/{id}` revokes a token. The **last `tenant:admin` token of a tenant cannot be
  deleted** (409 `CONFLICT`).
- Token handling (storage, rotation) is covered in [12-security.md](12-security.md).

**JWT** (accept tokens issued by your own identity provider)

```
senca-node --jwks /etc/senca/jwks.json --jwt-issuer https://idp.example/ --jwt-audience senca [--jwt-tenant-claim tenant]
```

- Keys come **only from a local JWKS file**; the node never fetches URLs named inside a token. `--jwks` requires both `--jwt-issuer` and
  `--jwt-audience`.
- Algorithms: EdDSA (Ed25519), ES256 and RS256 (RSA keys of at least 2048 bits). The key determines the algorithm; a header `alg` that
  does not match the key is rejected. Every JWKS key needs a `kid`, and `use`, if present, must be `sig`. `alg=none`, HS* and tokens with
  `crit` are rejected.
- Checks: `kid` present in the JWKS, `exp` required, `nbf`, `iss` must match, `aud` must match (string or array). Up to 60 seconds of
  clock skew is tolerated.
- The tenant comes from the `tenant` claim (rename with `--jwt-tenant-claim`); a missing or empty tenant is rejected. The subject is `sub`.
- Scopes come from `scope` (space-separated) or `scp` (string or array). Unknown scope names are silently ignored.

### 1.3 Scopes and tenants

| Scope | Main operations |
|---|---|
| `stream:read` | Read streams, schedules, inputs, outputs, pipelines, rooms and media pipelines; `POST /streams/{id}/plan`, `POST /schedules/check`, `POST /pipelines/{id}/validate`; **SSE** |
| `stream:write` | Create, update and delete the resources above; rotate stream keys; start and stop recordings and `PATCH /recordings/{id}`; issue room join tokens, manage participants, post room messages |
| `subscribe` | `POST /streams/{id}/playback-url` (signed live playback URL) |
| `recording:read` | List and get recordings, VOD URLs, thumbnails, MP4 export |
| `recording:delete` | `DELETE /recordings/{id}` |
| `tenant:admin` | Tokens and secrets (`/secrets`). **Implies every scope except `node:admin`** |
| `node:admin` | Operations outside any tenant: `/nodes`, `/devices`, `/grid`, drain, device disable/enable, and tenant-less SSE events |
| `publish` | Publishing with WHIP (usable instead of the stream key for `POST /whip/{stream_id}`; same-tenant streams only) |

- The scope required by each operation is in the OpenAPI extension `x-senca-scope` (`null` means "authenticated only", as for
  `GET /capabilities`). Missing scopes return 403 (`FORBIDDEN`, with the scope name in the title).
- **The tenant is derived from the credentials**: an API token belongs to the tenant of whoever created it, a JWT carries its tenant claim.
  No request body has a tenant field. Resources of other tenants look like 404. New tenants appear through JWT tenant claims (an API
  token can only create tokens in its own tenant).

### 1.4 Body validation

- Requests with a body must use `Content-Type: application/json` (parameters such as `; charset=...` are fine), otherwise 415
  (`UNSUPPORTED_MEDIA_TYPE`).
- **Unknown fields are rejected with 400** (`INVALID_REQUEST`, parser message in `details.reason`), as are type errors. Nothing is
  silently ignored.
- Semantic errors (ranges, combinations, references) are mostly 400 `INVALID_CONFIG`.

### 1.5 Idempotency-Key

With an `Idempotency-Key` header, a POST returns the same answer for the same request for 24 hours. Use it so that retried creates do not
create duplicates.

- Keys are 1 to 255 printable ASCII characters (no spaces); anything else is 400. Keys are scoped per tenant.
- "Same request" means the same SHA-256 over method, path, query and body (headers are not included). **A different request with the
  same key**, or the same key from a different principal, is 409 (`IDEMPOTENCY_CONFLICT`).
- A replay returns the stored status, body, `Location` and `ETag`, plus **`Idempotency-Replayed: true`**.
- **4xx responses are stored too** (replaying returns the same error). **5xx responses are not stored** (a retry executes again).
  Responses larger than 256 KiB are not stored.
- Secrets are removed from the stored body: `stream_key`, `token` and `secret` become `null`, with `stream_key_redacted: true` (and so on)
  added. In other words, **secrets are visible only in the first response**; a replay will not give you the value.
- Requests with the same key are serialized inside the node, so concurrent retries execute only once.

### 1.6 ETag and If-Match (optimistic concurrency)

- Resource responses (create, get, update) carry `ETag: "{generation}"` (for example `"3"`). `W/"3"` is accepted as well.
- **PATCH requires `If-Match`**. Without it you get 428 (`PRECONDITION_REQUIRED`); a malformed or stale value gives 412
  (`PRECONDITION_FAILED`).
- `If-Match` on DELETE is optional and only checked by `/streams`, `/recordings` and `/schedules` (stale means 412). DELETE on inputs,
  outputs, secrets, pipelines, rooms, media pipelines and tokens ignores it.

```
# read, then update with If-Match
ETAG=$(curl -s -D - -o /dev/null -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/streams/$ID | awk -F': ' 'tolower($1)=="etag"{print $2}' | tr -d '\r')
curl -s -X PATCH http://127.0.0.1:8080/api/v1/streams/$ID \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -H "If-Match: $ETAG" \
  -d '{"metadata":{"owner":"studio-a"}}'
```

On 412, read the resource again and reapply your change.

### 1.7 Lists and pagination

- Lists return `{"items": [...], "next_cursor": "..." | null}`, ordered by ascending ID.
- Query parameters are `limit` (1 to 1000, default 100) and `cursor` (the previous `next_cursor`). An out-of-range `limit` is 400.
- **Unknown query parameters on list endpoints are 400.** The only filters are `stream` and `state` on `GET /recordings` and
  `GET /schedules`, and `room` on `GET /media-pipelines`.
- `next_cursor: null` marks the last page. Some lists are not paginated: `GET /secrets` returns everything with `next_cursor: null`, and
  `GET /nodes` returns only `items`.

```
curl -s -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:8080/api/v1/streams?limit=50'
curl -s -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:8080/api/v1/streams?limit=50&cursor=<next_cursor>'
```

### 1.8 Errors (problem+json)

Errors are returned as `Content-Type: application/problem+json` in the RFC 9457 shape.

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

- `title` is a human-readable message in Japanese; branch on `code` in programs. `details` holds code-specific extra information and is
  omitted when empty.
- `retryable: true` means the same request may be sent again. A retryable 503, 429 or 504 also carries `Retry-After`
  (for 429, the seconds until a request will pass; otherwise 1).
- Any `/api/v1` operation may answer 429 when a rate limit is hit ([12. Security](12-security.md)).
- API versions other than `/api/v1` (such as `/api/v2/...`) get 422 `UNSUPPORTED_PROTOCOL_VERSION` (`details.supported: ["v1"]`).
- Quote `request_id` (the same value as `X-Request-Id`) when reporting problems.

| `code` | Status | `retryable` | Typical cause |
|---|---|---|---|
| `INVALID_CONFIG` | 400 | no | Semantic validation (ranges, combinations, references) |
| `INVALID_REQUEST` | 400 | no | Unparseable body, unknown fields, unknown query parameters, `limit` out of range, malformed Idempotency-Key |
| `UNSUPPORTED_PROTOCOL_VERSION` | 422 | no | A path under an API version other than `/api/v1` |
| `UNSUPPORTED_CODEC` | 422 | no | Snapshot decoding (`GET /streams/{id}/snapshot`), 422 from room processing, server-side processing for E2EE rooms and the streams exported from them (`details.reason: "e2ee"`) |
| `UNSUPPORTED_MEDIA_TYPE` | 415 | no | `Content-Type` is not `application/json` |
| `AUTH_FAILED` | 401 | no | Authentication failed |
| `FORBIDDEN` | 403 | no | Missing scope, or trying to grant a scope you do not hold |
| `NOT_FOUND` | 404 | no | Missing resource, another tenant's resource, unknown path |
| `METHOD_NOT_ALLOWED` | 405 | no | Method not defined on the resource |
| `CONFLICT` | 409 | no | Conflicts with current state (deleting a secret in use, deleting the last admin token, ...) |
| `IDEMPOTENCY_CONFLICT` | 409 | no | Different request with the same Idempotency-Key |
| `PRECONDITION_FAILED` | 412 | no | Stale or malformed `If-Match` |
| `PRECONDITION_REQUIRED` | 428 | no | PATCH without `If-Match` |
| `QUOTA_EXCEEDED` | 403 | no | A limit was hit (for example, at most 8 concurrent recordings per stream) |
| `RATE_LIMITED` | 429 | yes | A rate limit was hit (`details.limit` is `ip`, `token`, `auth_failures` or `connect`) |
| `CAPACITY_UNAVAILABLE` | 503 | usually yes | The cluster cannot accept the proposal (leader change, congestion), the room lives on another node, `--webrtc-udp` is not enabled, no codec worker (this one is not retryable) |
| `INPUT_TIMEOUT` | 504 | yes | A current frame was requested but no keyframe of the stream arrived within 5 seconds |
| `OWNERSHIP_LOST` | 409 | no | In a cluster, starting a recording or taking a current frame of a stream that another node is receiving (ask the node in `details.owner_api`) |
| `PAYLOAD_TOO_LARGE` | 413 | no | Recording too large to export as MP4, consensus command too large |
| `INTERNAL_ERROR` | 500 | yes | Failure to persist control state, and similar |

The `retryable` column shows the default per code (`RATE_LIMITED`, `CAPACITY_UNAVAILABLE`, `INPUT_TIMEOUT` and `INTERNAL_ERROR` default to
true); individual errors can override it. `type` is `urn:senca:error:{code in lower case with hyphens}`.

## 2. The OpenAPI document

[`docs/api/openapi.yaml`](../../api/openapi.yaml) (OpenAPI 3.1) lists every operation, schema and status code. Its descriptions are in
Japanese.

- The node does not serve the document. Open the file from the repository in any OpenAPI 3.1 viewer, for example Swagger UI in Docker:

  ```
  docker run --rm -p 127.0.0.1:8081:8080 -e SWAGGER_JSON=/spec/openapi.yaml \
    -v "$PWD/docs/api:/spec:ro" swaggerapi/swagger-ui
  # then open http://127.0.0.1:8081/
  ```

  Because `/api/v1` sends no CORS headers, a viewer's "try it out" cannot reach a node on another origin. Use curl or an SDK to try requests.
- Extensions:
  - `x-senca-scope`: the scope an operation requires (`null` = authenticated only)
  - `x-senca-undefined-method-status: 405`: how undocumented methods are answered
  - `x-senca-event-data`: the schema of the SSE `data` payload (`SseEventData`)
- Features that are not supported yet (such as asynchronous operation resources) are summarized at the top of the document.
- The types of the JS SDK and of the Rust client are generated from this document.

## 3. Events (SSE)

`GET /api/v1/events` streams changes as Server-Sent Events (`text/event-stream`). It requires `stream:read`.

```
curl -sN -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/events
```

```
event: stream.created
id: 42
data: {"type":"stream.created","resource_id":"...","occurred_at":"2026-11-03T01:02:03.000Z","data":{"name":"studio-a"}}
```

- **Control events** are generated from control commands once they are committed (in a cluster) or persisted (single node). The SSE `id`
  is the position in the control log (applied index, an integer). `data` is `{type, resource_id, occurred_at, data}`; `occurred_at` is the
  time the event was sent.
- **Tenant filtering**: you only receive your own tenant's events. Tenant-less events (nodes, grid, `stream.owner`) are delivered only to
  principals holding `node:admin`.
- **The server has no type filter.** The SDKs filter on the client side (sections 4 and 5).
- A comment line (keepalive) is sent every 15 seconds.
- 401 and 403 are problem+json like the rest of the API (the 403 title names `stream:read`).

### 3.1 Last-Event-ID and resync

The node keeps the most recent events (4096 by default, `--events-retain`; a single node also reloads, after a restart, the events
written since the last control-log snapshot).

- `id` is the number of applied commands. In a cluster the same event has the same `id` on every node, so you can reconnect to
  another node with the same `Last-Event-ID` and continue.
- When you reconnect with `Last-Event-ID`, the node first sends the retained events after that position, then streams new events.
- If the position is older than the retained window, the node first sends `event: resync` (`id` = current position, `data: {}`) and
  then keeps streaming new events. Without the header you get only events that happen after you connect.
- In a cluster, if the node you connect to has not yet applied up to `Last-Event-ID`, it waits up to 3 seconds.
- If you read too slowly and the notifications overflow, the node continues from the retained events; if they are gone too, it sends
  `event: resync` and **closes the connection**.
- On `resync`, reload the state you care about through the list endpoints, then reconnect with the last received `id` as `Last-Event-ID`.

### 3.2 Event catalog

Control events (with an `id`):

| Type | Tenant | `data` |
|---|---|---|
| `stream.created` | yes | `{name}` |
| `stream.updated` | yes | `{generation}` |
| `stream.deleted` | yes | `{}` |
| `stream.owner` | none (`node:admin`) | `{node}`: the node owning the stream changed ([09-cluster-grid.md](09-cluster-grid.md)) |
| `recording.updated` | yes | `{stream, state, generation}` |
| `recording.deleted` | yes | `{}` |
| `schedule.created`, `schedule.updated`, `schedule.deleted` | yes | `{}` |
| `schedule.run` | yes | `{state, occurrence, run_state, recording}` |
| `node.updated` | none (`node:admin`) | `{draining}` |
| `input.updated`, `output.updated` | yes | `{stream, generation, enabled}` |
| `input.deleted`, `output.deleted` | yes | `{}` |
| `secret.created` | yes | `{name}` |
| `secret.deleted` | yes | `{}` |
| `pipeline.updated` | yes | `{name, desired_state}` |
| `pipeline.deleted` | yes | `{}` |
| `room.updated` | yes | `{auto}` (room created, auto-created or updated) |
| `room.deleted` | yes | `{}` |
| `media_pipeline.updated` | yes | `{room, desired_state}` (created or updated) |
| `media_pipeline.deleted` | yes | `{}` |
| `node.down`, `node.up` | none (`node:admin`) | `{}` (`resource_id` is the node ID) |
| `device.down`, `device.up` | none (`node:admin`) | `{device}` (measured health changed) |
| `device.disabled`, `device.enabled` | none (`node:admin`) | `{device}` (manual disable/enable) |
| `grid.decisions` | none (`node:admin`) | `{decisions}` (placement decisions; `resource_id` is empty) |

Room events ([10-rooms.md](10-rooms.md), ADR-0015):

| Type | Internal room event name | `data.data` |
|---|---|---|
| `room.started` | `room.created` | `{settings, auto_created}` |
| `room.stopped` | `room.destroyed` | `{reason}` |
| `room.connection.created` | `connection.created` | `{participant}` |
| `room.connection.updated` | `connection.updated` | `{connection_id, metadata}` |
| `room.connection.destroyed` | `connection.destroyed` | `{connection_id, client_id?, reason, duration_ms}` |
| `room.track.published` | `track.published` | `{track}` |
| `room.track.unpublished` | `track.unpublished` | `{track_id, connection_id}` |
| `room.spotlight.changed` | `spotlight.changed` | `{focused}` |

- Room events are not part of the consensus log. **They have no SSE `id`**, so they are not covered by `Last-Event-ID` resumption, and
  dropped room events are skipped silently.
- **They are delivered only to subscribers connected to the node where the event happened** (for cascaded rooms see [10-rooms.md](10-rooms.md)).
  To see room events for every room in a cluster,
  subscribe to `/api/v1/events` on every node.
- `data` is `{type, resource_id (room ID), event_id (unique per event; use it to de-duplicate), node (emitting node ID), occurred_at, data}`.
  The inner `data` holds only the payload in the table above (the internal room event name is not included).
- Filtered by tenant; even `node:admin` does not receive other tenants' room events. Room setting changes are reported by the control event
  `room.updated`.

```
event: room.connection.created
data: {"type":"room.connection.created","resource_id":"daily","event_id":"...","node":"...","occurred_at":"...","data":{"participant":{"connection_id":"...","client_id":"alice",...}}}
```

## 4. JavaScript SDK (`@senca/sdk`)

`apps/sdk-js` contains both the browser realtime-room client (`Senca.connect()`) and the management API client (`SencaClient`) for browsers
and Node.js 20+. It has no runtime dependencies; the source is plain ES modules with JSDoc types and runs without a build step. Type
definitions live in `types/`, the API reference is [`apps/sdk-js/API.md`](../../../apps/sdk-js/API.md) and the detailed guide is
[`apps/sdk-js/README.en.md`](../../../apps/sdk-js/README.en.md) (the API reference is in Japanese).

- Not published to npm. Install it from the repository, e.g. `npm install /path/to/senca/apps/sdk-js`.
- Entry points: `@senca/sdk` (everything), `@senca/sdk/rest` (management API only), `@senca/sdk/protocol` (signaling types).

### 4.1 Management API (`SencaClient`)

```js
import { SencaClient, ApiError } from '@senca/sdk';

const senca = new SencaClient({ baseUrl: 'https://node.example', token: process.env.SENCA_TOKEN });

const s = await senca.streams.create({ name: 'studio-a' });     // Idempotency-Key added automatically
for await (const x of senca.streams.list({ limit: 100 })) console.log(x.name);   // follows cursors

const cur = await senca.streams.get(s.id);
await senca.streams.update(s.id, { metadata: { a: 'b' } }, { ifMatch: cur });     // an ETag, a generation or the resource itself
await senca.streams.modify(s.id, (c) => ({ metadata: { ...c.metadata, n: '1' } })); // re-reads and retries on 412 (3 attempts by default)

try {
  await senca.streams.update(s.id, { name: 'x' }, { ifMatch: 1 });
} catch (e) {
  if (e instanceof ApiError && e.code === 'PRECONDITION_FAILED') { /* someone else wrote first */ }
}
```

`ClientOptions`:

| Option | Default | Meaning |
|---|---|---|
| `baseUrl` | (required) | `https://node.example` (`/api/v1` is appended) or `https://node.example/api/v1` |
| `token` | none | API token or JWT. If a function, it is called for every request |
| `retry` | `true` | Retry policy (`maxAttempts` 3 including the first, `initialDelayMs` 200, `maxDelayMs` 5000, `factor` 2, `jitter` 0.2). `false` disables retries |
| `timeoutMs` | 30000 | Per-request timeout (not applied to SSE) |
| `headers` | `{}` | Headers added to every request |
| `idempotency` | `true` | Add an Idempotency-Key (UUID v4) to every POST automatically |
| `fetch` | `globalThis.fetch` | Override for testing or custom transports |

- Resources: `streams` (`rotateKey`, `playbackUrl`, `startRecording`, `plan`, `snapshot`, `snapshots.list/get/delete`),
  `recordings` (`stop`, `playbackUrl`, `thumbnail`, `exportMp4`; recordings are started with `streams.startRecording`), `schedules`
  (`check`, `runs`), `tokens`, `inputs` and `outputs` (`create(streamId, body)`, `list(streamId)`), `secrets`, `pipelines` (`validate`,
  `control`), `nodes` (`drain`, `undrain`, `devices.disable/enable`), `rooms` (`createToken`,
  `participants(id).list/get/updateMetadata/disconnect`, `sendMessage`, `stats`), `mediaPipelines` (`list({room})`, `start`, `stop`). On the
  client itself: `capabilities()`, `devices()`, `grid()`, `events()`. Untyped calls: `senca.request(method, path, opts)`.
- Common methods: `create`, `list` (a `Paginator`; `for await` walks every page), `listPage` (one page), `get`, `update` (`ifMatch` is
  required; omitting it throws `TypeError`), `modify`, `delete` (`ifMatch` optional).
- **Retries**: network failures, 408, 429, 502, 503, 504 and problems with `retryable: true` are retried with exponential backoff, waiting at
  least `Retry-After`. Failures with no response are retried only for GET, DELETE, PUT and POST with an Idempotency-Key. PATCH is retried
  only on 429 and 503.
- **Errors**: problem+json becomes `ApiError` (`status`, `code`, `title`, `type`, `requestId`, `retryable`, `retryAfterMs`, `details`,
  `problem`). Network failures have `status: 0` and `code: 'NETWORK_ERROR' | 'TIMEOUT'`.
- Helpers: `SencaClient.etagOf(resource)`, `SencaClient.newIdempotencyKey()` (for retrying with the same key across processes),
  `SencaClient.operations` (operationId to method, path and scope).

**SSE**

```js
for await (const ev of senca.events({ types: ['stream.*', 'room.connection.*'], signal })) {
  if (ev.type === 'resync') await reloadAll();
  else console.log(ev.type, ev.data.resource_id);
}
```

- `EventSource` cannot send `Authorization`, so the SDK reads the stream with `fetch`.
- `types` entries are exact names or prefixes ending in `*`, applied on the client. `resync` is always delivered, whatever the filter.
- On disconnect it reconnects with the last `id` as `Last-Event-ID` (backoff from 1 s doubling up to 30 s; `reconnect: false` disables
  this). 401, 403 and 404 end the iteration with an `ApiError` instead of reconnecting.

### 4.2 Realtime rooms (`Senca.connect()`, browser)

```js
import { Senca } from '@senca/sdk';

const camera = await navigator.mediaDevices.getUserMedia({ video: { width: 1280, height: 720 }, audio: true });
const session = await Senca.connect({
  url: signalingUrl,                 // RoomToken.signaling_url from the REST API
  room: 'daily',
  token: () => fetch('/my-backend/room-token').then((r) => r.text()),   // fetched again on every reconnect
  publish: { stream: camera, simulcast: true },
  dataChannels: ['#chat'],
});

for await (const track of session.remoteTracks()) {
  document.body.append(track.attach(document.createElement('video')));
}
```

- Your backend issues the join token with `POST /api/v1/rooms/{id}/tokens` (`senca.rooms.createToken(room, {role, client_id, metadata})`)
  and hands `token` and `signaling_url` to the browser. Never give a management API token to a browser.
- `Session` exposes `participants` and `tracks` (live Maps), `events()`, `remoteTracks()` and `messages()` (async iterators), `waitFor()`,
  `publish()`, `send()`, `stats()` and `close()`. The SDK handles signaling v1 internally: SDP, receive slots, renegotiation, ping, ICE
  restart, rejoining (backoff from 0.5 s doubling up to 10 s, at most 10 attempts) and redirects.
- Errors are `SencaError` (`code`, `fatal`, `closeCode`, `request`).
- Room features and limits: [10-rooms.md](10-rooms.md). Protocol: [docs/realtime/signaling.en.md](../../realtime/signaling.en.md).

## 5. Rust client (`senca-client`)

`crates/senca-client` is an async client for tokio. It uses `senca-http` for HTTP/1.1 and `senca-tls` for TLS, with no external HTTP
client. See [`crates/senca-client/README.en.md`](../../../crates/senca-client/README.en.md). It is not published on crates.io; depend on it
by path or git.

```rust
use senca_client::{EventsOptions, ListOptions, ProblemCode, SencaClient, ServerEvent};
use senca_client::models::{StreamCreate, StreamPatch};

let client = SencaClient::new("https://senca.example", std::env::var("SENCA_TOKEN")?)?;

let s = client.streams().create(&StreamCreate { name: "studio-a".into(), ..Default::default() }).await?;
let key = s.ingest.stream_key.clone();   // the stream key is only returned here

let patch = StreamPatch { name: Some("studio-b".into()), ..Default::default() };
let s = match client.streams().update(&s.id, &patch, &s).await {   // pass the previous response (its ETag) as If-Match
    Ok(v) => v,
    Err(e) if e.code() == Some(ProblemCode::PreconditionFailed) => { /* re-read and retry */ todo!() }
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
        ServerEvent::Resync { .. } => { /* reload lists */ }
    }
}

client.streams().delete(&s.id, s.if_match()).await?;
```

| Resource | Methods |
|---|---|
| `streams()` | `create`, `create_with`, `list`, `list_with`, `list_page`, `get`, `update`, `delete`, `rotate_key`, `playback_url`, `start_recording`, `plan`, `snapshot`, `snapshots`, `snapshot_by_id`, `delete_snapshot` |
| `recordings()` | `list`, `list_with` (`ListOptions::stream`), `list_page`, `get`, `update`, `delete`, `stop`, `playback_url`, `thumbnail`, `export_mp4` |
| `schedules()` | `create`, `create_with`, `list`, `list_with`, `list_page`, `check`, `get`, `update`, `delete`, `runs` |
| `tokens()` | `create`, `create_with`, `list`, `list_with`, `list_page`, `get`, `delete` |
| `inputs()`, `outputs()` | `create(stream_id, ..)`, `create_with`, `list(stream_id)`, `list_with`, `get`, `update`, `delete` |
| `secrets()` | `create`, `create_with`, `list`, `delete` |
| `pipelines()` | `create`, `create_with`, `list`, `list_with`, `list_page`, `get`, `update`, `delete`, `validate`, `control` |
| `nodes()` | `list`, `get`, `drain`, `undrain`, `disable_device`, `enable_device` |
| `rooms()` | `create`, `create_with`, `list`, `list_with`, `list_page`, `get`, `update`, `delete`, `issue_token`, `send_message`, `stats`, `participants`, `participants_with`, `participant`, `update_participant`, `disconnect` |
| `media_pipelines()` | `create`, `create_with`, `list`, `list_with` (`ListOptions::room`), `list_page`, `get`, `update`, `set_state`, `delete` |
| Client | `capabilities`, `grid`, `devices`, `events`, `healthz`, `readyz`, `fetch_signed` (signed playback paths; follows 307), `request_json` (untyped) |

- **Versions**: `get`, `create` and `update` return `Versioned<T>` (`value`, `etag`, `replayed`, `request_id`; derefs to the value). The
  `if_match` argument of `update` accepts `&Versioned<T>`, `IfMatch::Etag`, `IfMatch::Generation(n)` or a `u64`.
- **Idempotency**: every POST gets a fresh key, reused across that call's retries. Fix the key with
  `create_with(.., RequestOptions::idempotency_key("..."))` to get the same answer even when retrying from another process
  (`Versioned::replayed` is then true).
- **Errors**: `Error::Api(ApiError)` with `code` (`ProblemCode`; unknown values become `Unknown` plus `raw_code`), `title`, `status`,
  `retryable`, `request_id`, `details`, `retry_after`. Other variants: `Connect`, `Transport`, `Timeout`, `Decode`, `InvalidUrl`,
  `InvalidArgument`.
- **Retries** (`RetryPolicy`): by default up to 3 retries after the first attempt, starting at 200 ms and doubling up to 10 s with ±20 %
  jitter, waiting at least `Retry-After`. `ClientBuilder::retry(RetryPolicy::NONE)` disables retries. PATCH is retried only for
  `retryable`, 429 and 503.
- **SSE**: `EventsOptions` (`types`, `last_event_id`, `reconnect` (default true), `initial_backoff` 1 s, `max_backoff` 30 s). Reconnects with
  `Last-Event-ID`; does not reconnect on 401 or 403.
- **Builder**: `SencaClient::builder(url).token(..).timeout(..)` (default 30 s) `.user_agent(..).root_certificates_pem(pem).build()`. TLS
  trusts the OS certificate store by default.
- **Models**: the types in `senca_client::models` are generated from openapi.yaml. Unknown response fields
  are skipped and unknown enum values become `Unknown`. Nullable fields in `*Patch` types are `Option<Option<T>>` (`None` = leave as is,
  `Some(None)` = clear with null).

## 6. Related

- Writes in a cluster (any node accepts them; proposals are forwarded to the leader): [09-cluster-grid.md](09-cluster-grid.md),
  [ADR-0009](../../dev/adr/0009-cluster.md)
- Rooms and room events on SSE: [10-rooms.md](10-rooms.md), [ADR-0015](../../dev/adr/0015-realtime-rooms.md)
- Media pipelines: [11-media-pipelines.md](11-media-pipelines.md), [ADR-0016](../../dev/adr/0016-media-pipelines.md)
- Token and secret handling: [12-security.md](12-security.md); operations (logs, monitoring): [13-operations.md](13-operations.md)
