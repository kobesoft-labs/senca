# 10. Realtime rooms

[日本語](../ja/10-rooms.md)

- Related: [ADR-0015](../../adr/0015-realtime-rooms.md) (realtime rooms), [ADR-0013](../../adr/0013-grid.md) (grid), [ADR-0014](../../adr/0014-relay.md) (relay), [ADR-0016](../../adr/0016-media-pipelines.md) (media pipelines)
- Protocol: [Signaling v1](../../realtime/signaling.en.md)
- Implementation: `crates/senca-rooms` (room core), `crates/senca-node/src/rooms.rs` (node integration), `crates/senca-node/src/api/rooms.rs` (REST)

A room is where participants exchange video, audio and data over WebRTC. Rooms provide the following features:
roles, multistream, simulcast, spotlight, DataChannel messaging, forwarding filters, metadata / client_id / bundle_id,
E2EE pass-through and statistics. The server is an SFU: it forwards what it receives without transcoding.

## 1. Concepts

- **A room is a control-state resource.** You create it with `POST /api/v1/rooms` and can read or change it through any node. Room IDs are
  **unique across tenants**.
- **A room is a grid workload** (`room:{id}`) and is placed on exactly one node ([09. Cluster and grid](09-cluster-grid.md)).
  The node it is placed on (the hosting node) opens the room in its room core and terminates the participants' peer connections. The result appears in the
  room's `placement_status`.
- **Clients may connect to any node.** Signaling (`GET /signaling`) looks at the `room_id` of the first `connect` message; if the room is hosted elsewhere,
  the node replies with `redirect {location}` and closes. For participants, messages and stats, the REST API forwards the request to the hosting node.
- **When a room moves** (drain, node stop), the old node — if it is still running — sends `redirect` to the current participants so they rejoin on the new node.
- **Join tokens** can be issued and verified by any node: the signing key is derived from the control-state key, so it is the same on every node.

## 2. Prerequisites (node startup)

Nodes that serve rooms must have WebRTC enabled.

| Flag | Required | Meaning |
|---|---|---|
| `--webrtc-udp ADDR` | yes | The WebRTC UDP socket (shared with WHIP and WHEP). **The listen address is advertised as-is as the ICE candidate**, so use an IP the clients can reach, not `0.0.0.0` |
| `--public-http-url URL` | recommended | Base of the `signaling_url` returned for rooms and tokens (`http://`→`ws://`, `https://`→`wss://`, plus `/signaling`). Without it, `signaling_url` is `null` for rooms hosted on this node |
| `--https ADDR` with `--tls-cert`, `--tls-key` | optional | Accept `wss://` (`/signaling` is served on both the HTTP and HTTPS listeners) |

```sh
senca-node --data-dir /var/lib/senca --http 0.0.0.0:8080 \
  --webrtc-udp 203.0.113.10:20000 --public-http-url https://media.example.com
```

On a node without `--webrtc-udp`:

- `GET /signaling` returns HTTP 503 (`text/plain`, `webrtc (--webrtc-udp) is off`).
- If a room is hosted on this node, the participant, message and stats APIs return 503 `CAPACITY_UNAVAILABLE`.

**Caution (clusters):** the grid does not consider whether a node runs WebRTC when placing rooms. A room placed on a node without WebRTC cannot be joined.
In a cluster, either enable `--webrtc-udp` on every node, or label the WebRTC nodes (`--label webrtc=on`) and restrict rooms with `placement.labels`.
Redirects and API forwarding use each peer's `api` (management API URL) from the cluster configuration. A room placed on a node without `api` is treated by the other
nodes as "placement not decided yet" (§10).

## 3. Room REST API

All paths are under `/api/v1` and follow the common conventions (Bearer, problem+json, `Idempotency-Key`, `ETag`/`If-Match`, `limit`/`cursor`, unknown fields are 400;
see [15. API and SDKs](15-api-and-sdks.md)).

| Operation | Method and path | Scope | Result |
|---|---|---|---|
| Create | `POST /rooms` `{id?, settings?, metadata?, placement?}` | `stream:write` | 201 with `Location` and `ETag` |
| List | `GET /rooms?limit&cursor` | `stream:read` | 200 `{items, next_cursor}` (ID order, `limit` default 100, 1–1000) |
| Get | `GET /rooms/{id}` | `stream:read` | 200, `ETag: "{generation}"` |
| Update | `PATCH /rooms/{id}` `{settings?, metadata?, placement?}` | `stream:write` | 200. `If-Match` is required (428 if missing, 412 if stale) |
| Delete | `DELETE /rooms/{id}` | `stream:write` | 204. Participants are disconnected with `room_deleted` |
| Join token | `POST /rooms/{id}/tokens` | `stream:write` | 201 (§4) |
| List participants | `GET /rooms/{id}/participants?limit&cursor` | `stream:read` | 200 `{items, next_cursor}` |
| Participant | `GET /rooms/{id}/participants/{connection_id}` | `stream:read` | 200 |
| Participant metadata | `PATCH /rooms/{id}/participants/{connection_id}` `{metadata}` | `stream:write` | 200. Everyone receives `connection.updated` |
| Disconnect | `DELETE /rooms/{id}/participants/{connection_id}` | `stream:write` | 204. Reason `kicked` |
| Send | `POST /rooms/{id}/messages` `{to?, label?, data}` | `stream:write` | 200 `{delivered}` (§8) |
| Stats | `GET /rooms/{id}/stats` | `stream:read` | 200 (§9) |

### Create

- If `id` is omitted, a 24-character hex ID is generated. `id` is 1–128 characters of ASCII letters, digits and `-_.:@`.
- If a room with the same ID exists **in any tenant**, the result is 409 `CONFLICT`. Each tenant may have up to 10,000 rooms (beyond that, 403 `QUOTA_EXCEEDED`).
- `metadata` is arbitrary JSON (up to 8 KiB).
- `placement` is the same placement spec used by inputs and outputs (`node`, `device`, `labels`, `anti_affinity`, `pin_policy`, `priority`, `strategy`;
  `node` may be a node ID or name and is stored as the ID). `degrade` is for pipelines only and is rejected with 400. See [09](09-cluster-grid.md).

### Room settings (`settings`)

Omitted fields take their defaults; unknown fields are rejected with 400. All ranges below are checked on create and update (violations are 400 `INVALID_CONFIG`
with the offending fields in `details.errors`).

| Field | Default | Range / meaning |
|---|---|---|
| `max_participants` | 50 | 1–10000. Participant limit for the room; also drives the grid estimate (below) |
| `e2ee` | false | End-to-end encrypted room (§11) |
| `video_codec` | `vp8` | `vp8`, `vp9`, `h264`, `av1`. One per room (the SFU does not transcode, so everyone sends the same codec) |
| `audio_codec` | `opus` | `opus` only |
| `max_video_bitrate_kbps` | none | 50–50000. Video ceiling recommended to senders in `connected` |
| `spotlight` | none | `{number, unfocused?, focus_rid?}`. `number` 1–16, `unfocused` `lowest` (default) or `off`, `focus_rid` 1–16 alphanumerics (§7) |
| `recording` | `none` | `none`, `per_participant`, `composite`. **A policy value only**: the node does not start recording because of it. Recording is done with media pipelines ([11](11-media-pipelines.md)). `composite` cannot be combined with `e2ee: true` (400) |
| `forwarding_filter` | none | Room-wide forwarding filter (§6) |
| `data_labels` | `["#*"]` | DataChannel label patterns allowed in the room. Each starts with `#`, up to 64 bytes, at most 32 patterns. A trailing `*` is a prefix match |
| `empty_timeout_secs` | 60 | For auto-created rooms (`auto: true`): seconds a room may stay empty before it is deleted |

### Update

- Sending `settings` **replaces the whole object** (fields you leave out revert to their defaults). Read the current settings and send all of them, not just the changed field.
- `e2ee`, `video_codec` and `audio_codec` cannot change while participants are present (409).
- `placement: null` removes the placement spec.
- The hosting node notices the new generation in the control state and swaps the settings in its room core. Changing the room's forwarding filter or spotlight
  reassigns receive slots immediately.

### Room representation

Response of `POST`, `GET` and `PATCH` (the control-state room; it does not include live counts such as participants):

| Field | Meaning |
|---|---|
| `id`, `generation`, `settings`, `metadata` | |
| `auto` | The room was created automatically by issuing a join token |
| `created_at`, `updated_at` | RFC 3339 |
| `signaling_url` | Where to connect. The hosting node's URL if the room is elsewhere; otherwise derived from this node's `--public-http-url` (`null` if unset) |
| `placement` | Placement spec |
| `placement_status` | `state` (`running`, `degraded`, `pending`, `unschedulable`, `not_scheduled`); when placed, `node`, `node_name`, `device`, `level`, `level_label`, `since`, `reason`; when it cannot be placed, `reasons` |

### Grid estimate

A room's resource needs are estimated from `max_participants` (N) (`crates/senca-control/src/workloads.rs`):

| Resource | Estimate |
|---|---|
| CPU | 200 + 20 × N millicores |
| Memory | 64 + 2 × N MB |
| Ingress | 2 × N Mbit/s |
| Egress | 2 × N × min(N, 8) Mbit/s |

Each running media pipeline of the room (`desired_state: running`) adds 20 × N millicores and 8 × N MB.
With the default of 50 participants this is 1200 millicores, 164 MB, 100 Mbit/s in and 800 Mbit/s out. For small rooms, set `max_participants` to the real
size so more rooms fit on a node (node capacity comes from `--net-mbps` etc.; see [09](09-cluster-grid.md)).

### Requests answered by the hosting node

Participants, messages and stats are answered by the room core on the hosting node.

- If the room is hosted on another node, the receiving node forwards the request to that node's `/api/v1` with the same headers (`Authorization`, `Content-Type`,
  `If-Match`, `Idempotency-Key`, `X-Request-Id`) and returns the status, body and `ETag` unchanged (10-second timeout). **Forwarded requests are never forwarded again**
  (503, `retryable: true`).
- Placement not decided yet (e.g. right after creation): 503 `CAPACITY_UNAVAILABLE` (`retryable: true`).
- The room core does not answer within 5 seconds: 503 (`retryable: true`).
- The room core has not opened the room yet: 404.
- Timestamps (`joined_at`, `at`, …) are returned as RFC 3339.

## 4. Join tokens

`POST /rooms/{id}/tokens`:

| Field | Default | Meaning |
|---|---|---|
| `role` | required | `sendrecv`, `sendonly`, `recvonly`. Upper bound of the role the participant may join with (`sendrecv` allows any) |
| `client_id` | none | 1–255 characters, no control characters |
| `bundle_id` | none | 1–255 characters, no control characters |
| `metadata` | none | Metadata shown to other participants (up to 8 KiB) |
| `permissions` | defaults below | `{publish, subscribe, data}` |
| `ttl_secs` | 600 | 10–86400 |

Response (201):

```json
{"token":"eyJ…","room_id":"daily","expires_at":"2026-11-03T10:10:00Z","signaling_url":"wss://media.example.com/signaling"}
```

- **Requesting a token for a room that does not exist creates the room** (default settings, `auto: true`). Such a room is deleted once it has been empty for
  `empty_timeout_secs` (default 60 s); the hosting node checks every 5 seconds, so a room that was created but never joined is deleted as well.
  If the ID belongs to another tenant's room, the result is 404.
- Tokens are JWT-compatible, signed with Ed25519 (`EdDSA`, `kid: senca-rooms-1`), with `iss: "senca"`, `aud: "senca-rooms"`, and the tenant in `sub`.
- If the signing key does not exist yet (before cluster initialization), the result is 503 (`retryable: true`).

### Permissions (`permissions`)

| Field | Default | Meaning |
|---|---|---|
| `publish` | `["audio","video"]` | Track kinds the participant may send. Sending another kind is `forbidden` |
| `subscribe` | true | Whether the participant may receive other participants' tracks |
| `data` | `["#*"]` | DataChannel label patterns the participant may use |

### Verification rules

The room core checks the token at `connect`.

- **A token is required.** The node runs without an auth webhook and without anonymous access, so a `connect` without a token fails with `auth_failed` (§12).
- `exp` is required; clock skew of up to 30 seconds is tolerated. Tokens whose remaining lifetime (now to `exp`) exceeds 24 hours are rejected, as are tokens whose `iat` is too far in the future.
- If the token's `room` differs from `connect.room_id`, the result is `forbidden`. If `connect.role` exceeds the token's `role`, the result is `forbidden`.
- **Self-asserted `client_id` and `bundle_id` are not trusted** (they are used as forwarding-filter conditions):
  - If the token has a `client_id`, it is used. A `connect.client_id`, if given, must be identical or the result is `forbidden`.
  - If the token has no `client_id` but `connect.client_id` is given, the result is `forbidden` (`client_id must be issued in the token`).
  - The same applies to `bundle_id`. If neither provides one, `bundle_id` equals the `connection_id`.
- The participant's visible metadata is the token's `metadata`. `connect.metadata` is never shown to other participants (it exists for the auth webhook, which the node does not use).

## 5. Signaling

Signaling is JSON messages over a WebSocket at `GET /signaling` (subprotocol **`senca.rooms.v1`** is required). The message formats are defined in
[Signaling v1](../../realtime/signaling.en.md); this section only describes the flow on a node.

1. Your application server creates a token with `POST /rooms/{id}/tokens` and hands `token` and `signaling_url` to the browser.
2. The client connects to `signaling_url` and sends `connect {version: 1, room_id, token, role, ...}` within 10 seconds (HTTP `Authorization` is not used).
3. The receiving node looks up where the room is hosted (for a just-created room it waits up to 3 seconds for placement):
   - Another node: it sends `{"type":"redirect","location":"ws://…/signaling"}` and closes with close code 4300. The client reconnects to that URL.
   - Placement not decided: it sends `{"type":"error","code":"capacity_unavailable","fatal":true,…}` and closes with close code 4503.
   - This node: the room core processes `connect` and replies with `connected`.
4. The client sends the offer (**the client always offers**; the server is ICE-lite and only answers), and media flows after ICE and DTLS.
5. The server sends `ping` every 10 seconds; if no `pong` arrives within 10 seconds, the connection is closed with `ping_timeout`.

Other close codes:

| Close code | When |
|---|---|
| 4408 | No first message within 10 seconds of opening the WebSocket |
| 1013 | The room core cannot run yet (no signing key; before cluster initialization) |
| 4300 | The redirect above |
| 4503 | The `capacity_unavailable` above, or `room_full` |

All other error codes, close codes and disconnect reasons are listed in [Signaling v1 §7](../../realtime/signaling.en.md).
If the room does not exist (not in the control state, or not yet opened by the room core), the result is `room_not_found` (the node never creates rooms on join).

## 6. Multistream, simulcast, layer selection and forwarding filters

- **Receive slots**: each receiving m-line of the client is one slot. The server assigns room tracks to slots and sends `renegotiate` when more are needed.
  At most 32 slots per kind. A client never receives its own tracks or tracks from connections with the same `bundle_id`. With `multistream: false`, one track per kind.
- **Simulcast**: `connect.simulcast {rids, default_rid?}` lists the rids of the sent layers **lowest first**, 1–4 of them (1–16 alphanumerics each, no duplicates).
- **Layer selection** (per receiver, per track), in priority order:
  1. The client's `layer {track_id, rid}` (`rid: null` returns to automatic)
  2. Spotlight (§7)
  3. `simulcast.default_rid`
  4. Automatic: subtract 64 kbit/s for audio from the estimated bandwidth, multiply by 0.9, divide by the number of video subscriptions, and pick the highest layer that fits.
     Downgrades happen after the shortfall has lasted 1 second (immediately if the estimate is below 0.6 × what is needed); upgrades after the layer has fit for 2 seconds.
  Switching happens at a keyframe of the new layer (until then the current layer is forwarded). The layer currently received is reported by `notify {event: "layer.changed"}`.
- **Forwarding filters**: only tracks that satisfy **both** the room's `settings.forwarding_filter` and the connection's `connect.forwarding_filter` are forwarded.

```json
{"action":"block","rules":[[{"field":"kind","operator":"is_in","values":["video"]},
                            {"field":"client_id","operator":"is_not_in","values":["teacher"]}]]}
```

(do not receive video from anyone but the teacher). `rules` is an OR of AND-groups (1–10 groups of 1–10 rules each). `field` refers to the **sender's**
`connection_id`, `client_id`, `bundle_id`, `kind` or `metadata.<key>` (key up to 64 bytes; matches string, number or boolean values). `operator` is `is_in` or `is_not_in`;
`values` has 1–100 entries of up to 255 bytes (`audio`/`video` for `kind`). `action` is `block` (drop matches) or `allow` (forward only matches).

## 7. Spotlight

Enabled with the room's `settings.spotlight`. Speakers are detected by audio level; the `number` focused senders are forwarded at a high layer and everyone else
either at the lowest layer (`unfocused: "lowest"`) or not at all (`"off"`; the slot is kept and marked `paused`).

- Speaker detection uses the RFC 6464 audio level (or, if absent, an estimate from Opus packet sizes). 200 ms of voice means speaking, 1.5 s of silence means not speaking,
  and a focused speaker is kept for at least 2 seconds. There is one focus set per room (`spotlight.changed`); each receiver sees `number` senders excluding itself.
- The layer for focused senders is `focus_rid` (automatic by bandwidth if unset).
- A client can choose within the room's limits with `connect.spotlight {number, focus_rid?, unfocused?}` (`number` is capped at the room's value).

## 8. Messaging (DataChannel and push)

- **DataChannel**: DataChannels whose label starts with `#`. Declare them in `connect.data_channels`; the client opens them. Only labels that match both the room's
  `data_labels` and the token's `permissions.data` are accepted. A received message is forwarded unchanged to **everyone else** who has the same label open in a receiving direction.
- **To one participant**: the signaling message `message {label, to: [connection_id], data}`. The receiver gets it with `from`.
- **From the API**: `POST /rooms/{id}/messages`:

| Field | Meaning |
|---|---|
| `to` | List of connection_ids (omitted or empty means everyone). 404 if any ID is not in the room |
| `label` | If present, send on that DataChannel label to participants who have it open (must start with `#`; `data` must be a string) |
| `data` | JSON (up to 64 KiB). Without `label`, it arrives as the signaling message `push {data}` |

The response is `{"delivered": N}` (the number of participants it was delivered to).

- Limits: 64 KiB per application message, 256 KiB per signaling message, 50 messages per second per connection (beyond that `rate_limited`; above 4× the connection is closed).

## 9. Participants and statistics

Participant (`GET …/participants`): `connection_id`, `client_id`, `bundle_id`, `role`, `metadata`, `tracks[] {track_id, connection_id, kind, rids?}`,
`joined_at`, `peer_connected`, `signaling` (`websocket` or `datachannel`), `subscriptions[]` (slots: `mid`, `kind`, `state` (`active`, `paused`, `empty`), `track_id`, `connection_id`, `client_id`).

Stats (`GET …/stats`): `{room_id, at, participants[]}`. Per participant: `connection_id`, `rtt_ms`, `bandwidth_bps` (estimated bandwidth), `send_bitrate_bps`,
`retransmitted_packets`, `nack_requested`, `received_bytes`, `forwarded_bytes`, `forwarded_frames`, `published[] {track_id, kind, layers[] {rid, bitrate_bps}}`,
`subscriptions[] {mid, track_id, state, rid}`, and `client` (the getStats summary the client sent in `pong.stats` or `stats`).

## 10. Room moves and failures

- When the grid moves a room to another node because of a **drain** (`POST /nodes/{id}/drain`) or a **node stop**, the old node sends `redirect` to the current participants
  (followed by `disconnect {reason: "redirected"}` and close) and releases the room. The new node opens the room and participants rejoin (with new connection_ids and track IDs).
- If the old node **crashes**, no redirect can be sent. The participants' peer connections and signaling drop, and clients rejoin. They may rejoin through any node
  (they are redirected to the new host), so listing several nodes in the SDK's `url` lets the client skip the failed one. For how long the grid takes to move workloads,
  see [09](09-cluster-grid.md).
- Room state from before the move (participants, tracks, spotlight) is not carried over; the room starts empty on the new node.

## 11. E2EE (pass-through)

In a room with `settings.e2ee: true`, the SFU forwards media that clients have encrypted (for example with SFrame) without looking at its content.

- Participants must set `connect.e2ee` to match the room's `e2ee` (otherwise `e2ee_mismatch`). `e2ee` cannot change while participants are present.
- Encryption, decryption and key distribution are up to the application. The browser SDK only checks the flag against the room; it does not implement encryption (Encoded Transform).
- Because the server cannot see the content, compositing, composite recording and transcription are meaningless; `recording: "composite"` is rejected with 400.
  **The node's media pipeline API does not yet check whether the room is E2EE** (it will let you create a composite or captions). For E2EE rooms, use only
  per-participant export and recording (which store the media still encrypted).

## 12. Events (SSE)

Room events are delivered on `GET /api/v1/events` ([15](15-api-and-sdks.md)).

Control events (in the consensus log; they carry an SSE `id` and can be resumed with `Last-Event-ID`):

| `event` | When | `data` |
|---|---|---|
| `room.updated` | Room created, auto-created or updated | `{auto}` |
| `room.deleted` | Room deleted (including auto-room cleanup) | `{}` |

Room-core events (**delivered only to subscribers on the hosting node**; they have no SSE `id`, cannot be resumed with `Last-Event-ID`, and are skipped if the subscriber lags):

| `event` | When |
|---|---|
| `room.started` | The room core opened the room |
| `room.stopped` | The room core closed the room (deletion, move to another node) |
| `room.connection.created` | A participant joined |
| `room.connection.updated` | A participant's metadata changed |
| `room.connection.destroyed` | A participant left (`reason`, `duration_ms`) |
| `room.track.published` | A track was published |
| `room.track.unpublished` | A track was unpublished |
| `room.spotlight.changed` | The focus set changed |

Shape of a room-core event's `data`:

```json
{"type":"room.connection.destroyed","resource_id":"daily","event_id":"…","node":"…","occurred_at":"2026-11-03T10:00:00Z",
 "data":{"connection_id":"…","client_id":"alice","reason":"normal","duration_ms":61234}}
```

To receive all room-core events, subscribe to the SSE stream of the hosting node (`placement_status.node`). Events are filtered by tenant, so other tenants' room events are never delivered.

## 13. Limits

| Item | Value |
|---|---|
| Participants per room | `max_participants` (default 50, 1–10000); beyond that `room_full` |
| Participants and rooms per node | Room-core defaults of 10,000 each (beyond that `room_full`) |
| Rooms per tenant | 10,000 |
| Receive slots | 32 per kind |
| Simulcast layers | 1–4 |
| Metadata | 8 KiB (room, participant, token) |
| Application message | 64 KiB |
| Signaling message | 256 KiB, 50 per second |
| Token lifetime | 10–86400 seconds (default 600) |
| Deadline for `connect` | 10 seconds |
| Ping | every 10 seconds, `pong` deadline 10 seconds |
| Peer connection deadline | `peer_failed` if not connected 30 seconds after joining |

## 14. Not yet available in the node

Some features exist in the room core (`senca-rooms`) and the Signaling v1 document but are not yet usable in `senca-node`:

- **Auth webhook**: the node runs the room core without `auth_webhook` (admission is by token only). Admission cannot be delegated to an external service.
  (Even if the room core issued a webhook request, the node would treat it as unanswered and reject: see `drive()` in `crates/senca-node/src/rooms.rs`.)
  Consequently `connect.metadata` is not used anywhere and joining without a token is impossible.
- **Event webhooks**: the mechanism that POSTs room events to an external endpoint (`senca_rooms::webhook::Outbox`) is not wired into the node. Events are available via SSE only.
- **Cascading (spanning a room across nodes)**: not enabled. A room stays on one node and participants who connect to other nodes are redirected.
  The size of a room is therefore bounded by one node's capacity, and the participant `node` field (participants on other nodes) never appears.
- **Node `overflow_redirect`**: cannot be configured (a full node answers `room_full`).
- **`settings.recording`**: a policy value only; it does not start recording (use media pipelines, [11](11-media-pipelines.md)).
- **A message for participants to change their own metadata**: Signaling v1 has none; change it from your application server with `PATCH /rooms/{id}/participants/{connection_id}`.
- **Resume**: after a disconnect, the client rejoins as a new connection.

## 15. Examples

### curl

```sh
API=https://media.example.com/api/v1
H="Authorization: Bearer $SENCA_TOKEN"

# Create a room
curl -s -X POST -H "$H" -H 'Content-Type: application/json' "$API/rooms" \
  -d '{"id":"demo","settings":{"max_participants":10,"video_codec":"vp8","spotlight":{"number":2}},"metadata":{"title":"standup"}}'

# Check placement (placement_status.state becomes running)
curl -s -H "$H" "$API/rooms/demo"

# Join token (put client_id in the token)
curl -s -X POST -H "$H" -H 'Content-Type: application/json' "$API/rooms/demo/tokens" \
  -d '{"role":"sendrecv","client_id":"alice","metadata":{"name":"Alice"},"ttl_secs":600}'

# Participants and stats
curl -s -H "$H" "$API/rooms/demo/participants"
curl -s -H "$H" "$API/rooms/demo/stats"

# Push to everyone / send on the #chat DataChannel
curl -s -X POST -H "$H" -H 'Content-Type: application/json' "$API/rooms/demo/messages" -d '{"data":{"notice":"ending in 5 minutes"}}'
curl -s -X POST -H "$H" -H 'Content-Type: application/json' "$API/rooms/demo/messages" -d '{"label":"#chat","data":"hello"}'

# Change settings (replaces the whole object; If-Match carries the current ETag)
curl -s -X PATCH -H "$H" -H 'Content-Type: application/json' -H 'If-Match: "1"' "$API/rooms/demo" \
  -d '{"settings":{"max_participants":20,"video_codec":"vp8","spotlight":{"number":2}}}'

# Kick one participant / delete the room
curl -s -X DELETE -H "$H" "$API/rooms/demo/participants/$CONNECTION_ID"
curl -s -X DELETE -H "$H" "$API/rooms/demo"
```

`tools/interop/node_rooms.sh` exercises this flow with three Chrome pages: it creates a room over REST, joins with tokens, and checks mutual video decoding,
DataChannel round trips, simulcast layer switching, the REST participants, stats and push, and the SSE events.

### Browser SDK (`@senca/sdk`)

```js
import { Senca } from '@senca/sdk';

const camera = await navigator.mediaDevices.getUserMedia({ video: { width: 1280, height: 720 }, audio: true });
const session = await Senca.connect({
  url: 'wss://media.example.com/signaling',   // signaling_url from the token response (an array rotates on rejoin)
  room: 'demo',
  token: () => fetch('/my-backend/room-token').then((r) => r.text()),   // fetched again on every rejoin
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
await session.send('#chat', { text: 'hello' });                  // everyone (DataChannel)
for await (const m of session.messages('#chat')) console.log(m.sender?.clientId, m.json());
const t = [...session.tracks.values()].find((t) => t.kind === 'video');
await t.setLayer('r0');                                          // 'r0' | 'r1' | 'r2' | 'auto'
await session.close();
```

The SDK hides SDP, receive slots, renegotiation, ping, redirects (up to 3) and reconnection (0.5 s doubling up to 10 s, up to 10 attempts).
Options and events are listed in `apps/sdk-js/README.md` and [15. API and SDKs](15-api-and-sdks.md).
