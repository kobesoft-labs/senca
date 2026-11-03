# 10. Realtime rooms

[日本語](../ja/10-rooms.md)

A room is where participants exchange video, audio and data over WebRTC. The server is an SFU: it forwards what it receives without transcoding.
Rooms provide the following features:
roles, multistream, simulcast, spotlight, DataChannel messaging, forwarding filters, metadata / client_id / bundle_id,
E2EE pass-through and statistics.

- Background: [ADR-0015](../../dev/adr/0015-realtime-rooms.md) (realtime rooms), [ADR-0013](../../dev/adr/0013-grid.md) (grid), [ADR-0014](../../dev/adr/0014-relay.md) (relay), [ADR-0016](../../dev/adr/0016-media-pipelines.md) (media pipelines)
- Protocol specification: [Signaling v1](../../realtime/signaling.en.md)

## 10.1 Concepts

- **A room is a control-state resource.** You create it with `POST /api/v1/rooms` and can read or change it through any node. Room IDs are
  **unique across tenants**.
- **A room is a grid workload** (`room:{id}`) and is placed on exactly one node ([09. Cluster and grid](09-cluster-grid.md)).
  The node it is placed on (the hosting node) opens the room and terminates the participants' peer connections. The result appears in the
  room's `placement_status`.
- **Clients may connect to any node.** Signaling (`GET /signaling`) looks at the `room_id` of the first `connect` message; if the room is hosted elsewhere,
  the node replies with `redirect {location}` and closes. For participants, messages and stats, the REST API forwards the request to the hosting node.
  Rooms with `cascade: true` are not redirected; the node the client connected to serves it (cascading, 10.10).
- **When a room moves** (drain, node stop), the old node — if it is still running — sends `redirect` to the current participants so they rejoin on the new node.
- **Join tokens** can be issued and verified by any node: the signing key is derived from the control-state key, so it is the same on every node.

## 10.2 Prerequisites (node startup)

Nodes that serve rooms must have WebRTC enabled.

| Flag | Required | Meaning |
|---|---|---|
| `--webrtc-udp ADDR` | yes | The WebRTC UDP socket (shared with WHIP and WHEP). When bound to `0.0.0.0:port`, the ICE candidates carry the node's network interface addresses |
| `--webrtc-public-ip IP` | yes behind NAT or in Docker | The node's IP address as clients see it; the ICE candidates carry this address (several with commas or by repeating the flag) |
| `--public-http-url URL` | recommended | Base of the `signaling_url` returned for rooms and tokens (`http://`→`ws://`, `https://`→`wss://`, plus `/signaling`). Without it, `signaling_url` is `null` for rooms hosted on this node |
| `--https ADDR` with `--tls-cert`, `--tls-key` | optional | Accept `wss://` (`/signaling` is served on both the HTTP and HTTPS listeners) |

```sh
senca-node --data-dir /var/lib/senca --http 0.0.0.0:8080 \
  --webrtc-udp 0.0.0.0:20000 --webrtc-public-ip 203.0.113.10 --public-http-url https://media.example.com
```

On a node without `--webrtc-udp`:

- `GET /signaling` returns HTTP 503 (`text/plain`, `webrtc (--webrtc-udp) is off`).
- If a room is hosted on this node, the participant, message and stats APIs return 503 `CAPACITY_UNAVAILABLE`.

**Caution (clusters):** the grid does not consider whether a node runs WebRTC when placing rooms. A room placed on a node without WebRTC cannot be joined.
In a cluster, either enable `--webrtc-udp` on every node, or label the WebRTC nodes (`--label webrtc=on`) and restrict rooms with `placement.labels`.
Redirects and API forwarding use each peer's `api` (management API URL) from the cluster configuration. A room placed on a node without `api` is treated by the other
nodes as "placement not decided yet" (see "Requests answered by the hosting node" in 10.3).

## 10.3 Room REST API

All paths are under `/api/v1` and follow the common conventions (Bearer, problem+json, `Idempotency-Key`, `ETag`/`If-Match`, `limit`/`cursor`, unknown fields are 400;
see [15. API and SDKs](15-api-and-sdks.md)).

| Operation | Method and path | Scope | Result |
|---|---|---|---|
| Create | `POST /rooms` `{id?, settings?, metadata?, placement?}` | `stream:write` | 201 with `Location` and `ETag` |
| List | `GET /rooms?limit&cursor` | `stream:read` | 200 `{items, next_cursor}` (ID order, `limit` default 100, 1–1000) |
| Get | `GET /rooms/{id}` | `stream:read` | 200, `ETag: "{generation}"` |
| Update | `PATCH /rooms/{id}` `{settings?, metadata?, placement?}` | `stream:write` | 200. `If-Match` is required (428 if missing, 412 if stale) |
| Delete | `DELETE /rooms/{id}` | `stream:write` | 204. Participants are disconnected with `room_deleted` |
| Join token | `POST /rooms/{id}/tokens` | `stream:write` | 201 (10.4) |
| List participants | `GET /rooms/{id}/participants?limit&cursor` | `stream:read` | 200 `{items, next_cursor}` |
| Participant | `GET /rooms/{id}/participants/{connection_id}` | `stream:read` | 200 |
| Participant metadata | `PATCH /rooms/{id}/participants/{connection_id}` `{metadata}` | `stream:write` | 200. Everyone receives `connection.updated` |
| Disconnect | `DELETE /rooms/{id}/participants/{connection_id}` | `stream:write` | 204. Reason `kicked` |

Updating or disconnecting a participant works on any node: the request is forwarded to the node the participant is connected to (including a cascade edge).
| Send | `POST /rooms/{id}/messages` `{to?, label?, data}` | `stream:write` | 200 `{delivered}` (10.8) |
| Stats | `GET /rooms/{id}/stats` | `stream:read` | 200 (10.9) |

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
| `e2ee` | false | End-to-end encrypted room (10.11) |
| `video_codec` | `vp8` | `vp8`, `vp9`, `h264`, `av1`. One per room (the SFU does not transcode, so everyone sends the same codec) |
| `audio_codec` | `opus` | `opus` only |
| `max_video_bitrate_kbps` | none | 50–50000. Video ceiling recommended to senders in `connected` |
| `spotlight` | none | `{number, unfocused?, focus_rid?}`. `number` 1–16, `unfocused` `lowest` (default) or `off`, `focus_rid` 1–16 alphanumerics (10.7) |
| `recording` | `none` | `none`, `per_participant`, `composite`. With `per_participant` (one recording per participant) or `composite` (everyone side by side in one recording), a recording media pipeline is created automatically ("Room recording" in 10.3). `composite` cannot be combined with `e2ee: true` (422) |
| `forwarding_filter` | none | Room-wide forwarding filter (10.6) |
| `data_labels` | `["#*"]` | DataChannel label patterns allowed in the room. Each starts with `#`, up to 64 bytes, at most 32 patterns. A trailing `*` is a prefix match |
| `empty_timeout_secs` | 60 | For auto-created rooms (`auto: true`): seconds a room may stay empty before it is deleted |
| `cascade` | false | If true, participants who connect to a node other than the room's node are served on that node (cascading, 10.10). If false they are redirected to the room's node |
| `auth_webhook_url` | none | Auth webhook URL for this room (`http://` or `https://`). Takes precedence over the node's `--rooms-auth-webhook` (10.4) |

### Update

- Send **only the fields you want to change** in `settings` (JSON Merge Patch, RFC 7396). Fields you leave out keep their values, and a
  field set to `null` reverts to its default (nested objects such as `spotlight` follow the same rule). The merged result is validated like a new room.
- `e2ee`, `video_codec` and `audio_codec` cannot change while participants are present (409).
- `placement: null` removes the placement spec.

### Room recording

When `settings.recording` is `per_participant` or `composite`, a recording media pipeline for the room (ID `room-rec.{room ID}`,
`managed: true`) is created automatically and kept in line with the setting ([11. Media pipelines](11-media-pipelines.md)).

- `per_participant`: each participant is exported to its own stream, and each stream is recorded (in E2EE rooms the media stays encrypted).
- `composite`: video participants are laid out in a grid in one stream (AV1, 1280x720), which is recorded.
- Setting it back to `none` deletes the media pipeline. Exported streams and recordings are kept.
- This media pipeline cannot be changed or deleted through its own API (409); change the room setting instead.
- The hosting node notices the new generation in the control state and applies the new settings. Changing the room's forwarding filter or spotlight
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

A room's resource needs are estimated from `max_participants` (N):

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

Participants, messages and stats are answered by the hosting node.

- If the room is hosted on another node, the receiving node forwards the request to that node's `/api/v1` with the same headers (`Authorization`, `Content-Type`,
  `If-Match`, `Idempotency-Key`, `X-Request-Id`) and returns the status, body and `ETag` unchanged (10-second timeout). **Forwarded requests are never forwarded again**
  (503, `retryable: true`).
- Placement not decided yet (e.g. right after creation): 503 `CAPACITY_UNAVAILABLE` (`retryable: true`).
- The room does not answer within 5 seconds: 503 (`retryable: true`).
- The hosting node has not opened the room yet: 404.
- Timestamps (`joined_at`, `at`, …) are returned as RFC 3339.

## 10.4 Join tokens

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

The hosting node checks the token at `connect`.

- **A token is required** (by default). A `connect` without a token fails with `auth_failed` ([Signaling v1 §7](../../realtime/signaling.en.md)). Only with `--rooms-auth-mode only`
  can the auth webhook admit participants instead of a token.
- `exp` is required; clock skew of up to 30 seconds is tolerated. Tokens whose remaining lifetime (now to `exp`) exceeds 24 hours are rejected, as are tokens whose `iat` is too far in the future.
- If the token's `room` differs from `connect.room_id`, the result is `forbidden`. If `connect.role` exceeds the token's `role`, the result is `forbidden`.
- **Self-asserted `client_id` and `bundle_id` are not trusted** (they are used as forwarding-filter conditions):
  - If the token has a `client_id`, it is used. A `connect.client_id`, if given, must be identical or the result is `forbidden`.
  - If the token has no `client_id` but `connect.client_id` is given, the result is `forbidden` (`client_id must be issued in the token`).
  - The same applies to `bundle_id`. If neither provides one, `bundle_id` equals the `connection_id`.
- The participant's visible metadata is the token's `metadata`. `connect.metadata` is never shown to other participants (it is passed to the auth webhook).

### Auth webhook

If the room setting `auth_webhook_url`, or otherwise the node's `--rooms-auth-webhook`, is set, the node verifies the token and then
asks that URL with a signed `POST` (request and response formats: [Signaling v1](../../realtime/signaling.en.md) §9).

- A response with `allowed: false` fails the join with `auth_failed` (the `reason` reaches the client). The response may override
  `role`, `client_id`, `metadata` and `permissions`. No answer within 5 seconds, or a non-2xx status, is also a rejection.
- The signing secret is passed with `--rooms-webhook-secret-file` (at least 16 bytes; used for the `Senca-Signature` header). On a node without a secret, joins to
  rooms that have a webhook URL are rejected.
- Destinations are checked against the `--egress-allow` policy; allow internal addresses explicitly.
- Rooms without either URL are admitted by token alone, as before.

## 10.5 Signaling

Signaling is JSON messages over a WebSocket at `GET /signaling` (subprotocol **`senca.rooms.v1`** is required). The message formats are defined in
[Signaling v1](../../realtime/signaling.en.md); this section only describes the flow on a node.

1. Your application server creates a token with `POST /rooms/{id}/tokens` and hands `token` and `signaling_url` to the browser.
2. The client connects to `signaling_url` and sends `connect {version: 1, room_id, token, role, ...}` within 10 seconds (HTTP `Authorization` is not used).
3. The receiving node looks up where the room is hosted (for a just-created room it waits up to 3 seconds for placement):
   - Another node: it sends `{"type":"redirect","location":"ws://…/signaling"}` and closes with close code 4300. The client reconnects to that URL.
   - Placement not decided: it sends `{"type":"error","code":"capacity_unavailable","fatal":true,…}` and closes with close code 4503.
   - This node: it processes `connect` and replies with `connected`.
4. The client sends the offer (**the client always offers**; the server is ICE-lite and only answers), and media flows after ICE and DTLS.
5. The server sends `ping` every 10 seconds; if no `pong` arrives within 10 seconds, the connection is closed with `ping_timeout`.

Other close codes:

| Close code | When |
|---|---|
| 4408 | No first message within 10 seconds of opening the WebSocket |
| 1013 | The node cannot serve rooms yet (no signing key; before cluster initialization) |
| 4300 | The redirect above |
| 4503 | The `capacity_unavailable` above, or `room_full` |

All other error codes, close codes and disconnect reasons are listed in [Signaling v1 §7](../../realtime/signaling.en.md).
If the room does not exist (not in the control state, or not yet opened by the hosting node), the result is `room_not_found` (the node never creates rooms on join).

## 10.6 Multistream, simulcast, layer selection and forwarding filters

- **Receive slots**: each receiving m-line of the client is one slot. The server assigns room tracks to slots and sends `renegotiate` when more are needed.
  At most 32 slots per kind. A client never receives its own tracks or tracks from connections with the same `bundle_id`. With `multistream: false`, one track per kind.
- **Simulcast**: `connect.simulcast {rids, default_rid?}` lists the rids of the sent layers **lowest first**, 1–4 of them (1–16 alphanumerics each, no duplicates).
- **Layer selection** (per receiver, per track), in priority order:
  1. The client's `layer {track_id, rid}` (`rid: null` returns to automatic)
  2. Spotlight (10.7)
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

## 10.7 Spotlight

Enabled with the room's `settings.spotlight`. Speakers are detected by audio level; the `number` focused senders are forwarded at a high layer and everyone else
either at the lowest layer (`unfocused: "lowest"`) or not at all (`"off"`; the slot is kept and marked `paused`).

- Speaker detection uses the RFC 6464 audio level (or, if absent, an estimate from Opus packet sizes). 200 ms of voice means speaking, 1.5 s of silence means not speaking,
  and a focused speaker is kept for at least 2 seconds. There is one focus set per room (`spotlight.changed`); each receiver sees `number` senders excluding itself.
- The layer for focused senders is `focus_rid` (automatic by bandwidth if unset).
- A client can choose within the room's limits with `connect.spotlight {number, focus_rid?, unfocused?}` (`number` is capped at the room's value).

## 10.8 Messaging (DataChannel and push)

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

## 10.9 Participants and statistics

Participant (`GET …/participants`): `connection_id`, `client_id`, `bundle_id`, `role`, `metadata`, `tracks[] {track_id, connection_id, kind, rids?}`,
`joined_at`, `peer_connected`, `signaling` (`websocket` or `datachannel`), `subscriptions[]` (slots: `mid`, `kind`, `state` (`active`, `paused`, `empty`), `track_id`, `connection_id`, `client_id`).

Stats (`GET …/stats`): `{room_id, at, participants[]}`. Per participant: `connection_id`, `rtt_ms`, `bandwidth_bps` (estimated bandwidth), `send_bitrate_bps`,
`retransmitted_packets`, `nack_requested`, `received_bytes`, `forwarded_bytes`, `forwarded_frames`, `published[] {track_id, kind, layers[] {rid, bitrate_bps}}`,
`subscriptions[] {mid, track_id, state, rid}`, and `client` (the getStats summary the client sent in `pong.stats` or `stats`).

## 10.10 Room moves and failures

- When the grid moves a room to another node because of a **drain** (`POST /nodes/{id}/drain`) or a **node stop**, the old node sends `redirect` to the current participants
  (followed by `disconnect {reason: "redirected"}` and close) and releases the room. The new node opens the room and participants rejoin (with new connection_ids and track IDs).
- If the old node **crashes**, no redirect can be sent. The participants' peer connections and signaling drop, and clients rejoin. They may rejoin through any node
  (they are redirected to the new host), so listing several nodes in the SDK's `url` lets the client skip the failed one. For how long the grid takes to move workloads,
  see [09](09-cluster-grid.md).
- Room state from before the move (participants, tracks, spotlight) is not carried over; the room starts empty on the new node.

### Cascading (spanning a room across nodes)

With the room setting `cascade: true` (or the node flag `--rooms-cascade`), participants stay on the node they connected to
(for example the nearest one) and still join the same room. This needs a cluster (the inter-node relay).

- The node the room is placed on is the **home**; other nodes that serve participants are **edges**. An edge is created when a
  participant arrives and closes 20 seconds after its last participant leaves. Draining nodes do not create edges and redirect to the home.
- Participants on other nodes appear with the `node` field; their video, audio and DataChannel messages arrive over the relay.
  In the REST participant list (forwarded to the home) they are shown with `signaling: "remote"`.
- `max_participants` counts participants on all nodes. Simultaneous joins on different nodes may exceed it for a short time.
- **When the home dies**, the grid moves the room to another node. If that node is an edge, its participants continue without
  interruption. Participants of the dead node rejoin by connecting to another node (list several nodes in the SDK's `url`).
- The REST endpoints that disconnect or update a participant work on any node; the request is forwarded to the participant's node.
- Media pipelines also export participants on edge nodes, receiving their tracks from that node over the relay.
- The cascade state is shown in `rooms` of this node in `GET /api/v1/nodes`.

## 10.11 E2EE (pass-through)

In a room with `settings.e2ee: true`, the SFU forwards media that clients have encrypted (for example with SFrame) without looking at its content.

- Participants must set `connect.e2ee` to match the room's `e2ee` (otherwise `e2ee_mismatch`). `e2ee` cannot change while participants are present.
- Encryption, decryption and key distribution are up to the application. The browser SDK only checks the flag against the room; it does not implement encryption (Encoded Transform).
- Because the server cannot see the content, every operation that would process it on the server fails with 422 (`UNSUPPORTED_CODEC`,
  `details.reason: "e2ee"`):
  - Rooms: `recording: "composite"`; creating or updating a media pipeline with a composite (`composite`) or transcription (`captions`);
    turning on `e2ee` for a room that has such a media pipeline.
  - Streams exported from an E2EE room (they carry `metadata.e2ee: "true"`): still images (`GET /streams/{id}/snapshot`), captions
    (`captions`), saved still images (`snapshots`), snapshot schedules, and pipeline sources (`source`). Recording thumbnails are not made either.
- E2EE rooms support only per-participant export and recording (which store the media still encrypted).

## 10.12 Events (SSE)

Room events are delivered on `GET /api/v1/events` ([15](15-api-and-sdks.md)).

Control events (in the consensus log; they carry an SSE `id` and can be resumed with `Last-Event-ID`):

| `event` | When | `data` |
|---|---|---|
| `room.updated` | Room created, auto-created or updated | `{auto}` |
| `room.deleted` | Room deleted (including auto-room cleanup) | `{}` |

In-room events (participants, tracks and so on; **delivered only to subscribers on the node where the participant is**; with cascading, events of an edge's participants
go to the edge node and `room.started`/`room.stopped` come only from the home; they have no SSE `id`, cannot be resumed with `Last-Event-ID`, and are skipped if the subscriber lags):

| `event` | When |
|---|---|
| `room.started` | The node opened the room |
| `room.stopped` | The node closed the room (deletion, move to another node) |
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

### Event webhook

With `--rooms-event-webhook URL` and `--rooms-webhook-secret-file FILE`, each node signs and `POST`s its in-room events to the URL
(body format: [Signaling v1](../../realtime/signaling.en.md) §10; `type` uses the webhook names such as `connection.created`).

- Delivery is at least once. Receivers deduplicate by the body's `id` (same as the `Senca-Webhook-Id` header).
- A 2xx completes delivery. 408, 429, 5xx, connection failures and a 10-second timeout are retried after 1, 2, 4, … seconds,
  giving up after 10 attempts. Other 4xx give up immediately.
- The number of abandoned events and recent IDs are shown in `rooms.event_webhook` of this node in `GET /api/v1/nodes`.
- Destinations are checked against the `--egress-allow` policy. Events are still streamed to SSE.

## 10.13 Limits

| Item | Value |
|---|---|
| Participants per room | `max_participants` (default 50, 1–10000); beyond that `room_full` |
| Participants and rooms per node | 10,000 each (beyond that `room_full`) |
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

## 10.14 Not yet available in the node

Some features are described in the [Signaling v1](../../realtime/signaling.en.md) specification but are not yet available in `senca-node`:

- **Node `overflow_redirect`**: cannot be configured (a full node answers `room_full`).
- **`settings.recording`**: a policy value only; it does not start recording (use media pipelines, [11](11-media-pipelines.md)).
- **A message for participants to change their own metadata**: Signaling v1 has none; change it from your application server with `PATCH /rooms/{id}/participants/{connection_id}`.
- **Resume**: after a disconnect, the client rejoins as a new connection.

## 10.15 Examples

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

# Change settings (only the fields you send change; If-Match carries the current ETag)
curl -s -X PATCH -H "$H" -H 'Content-Type: application/json' -H 'If-Match: "1"' "$API/rooms/demo" \
  -d '{"settings":{"max_participants":20,"spotlight":{"number":2}}}'

# Kick one participant / delete the room
curl -s -X DELETE -H "$H" "$API/rooms/demo/participants/$CONNECTION_ID"
curl -s -X DELETE -H "$H" "$API/rooms/demo"
```

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
