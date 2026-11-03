[日本語](signaling.md) | English

# Realtime rooms: signaling v1

This document specifies the signaling protocol for client implementers who join SENCA realtime rooms.
For server configuration and the REST API, see [10. Realtime rooms](../guide/en/10-rooms.md).

- Background: [ADR-0015](../dev/adr/0015-realtime-rooms.md) (realtime rooms), [ADR-0012](../dev/adr/0012-webrtc.md) (WebRTC), [ADR-0013](../dev/adr/0013-grid.md) (grid), [ADR-0014](../dev/adr/0014-relay.md) (relay)

Terms:

- **Sender**: a participant that sends tracks. **Receiver**: a participant that receives other participants' tracks.
- **Receive slot** (slot): one receiving m-line of the client (§3).
- **Layer**: one of the simulcast encodings at a different resolution or bitrate (identified by its rid).

## 1. Transport and version

| Item | Value |
|---|---|
| Transport | WebSocket (RFC 6455). `GET /signaling`, subprotocol **`senca.rooms.v1`** (required). Extensions (permessage-deflate) are not used |
| Framing | One JSON message (UTF-8) per text frame. Binary frames are not used |
| Switching | Once connected, the same JSON can be carried over the DataChannel **`signaling`** (§6) |
| Version | `connect.version` = **1**. The server answers with `connected.version`. On a mismatch it closes with `unsupported_version` |
| Types | Every message has a `type`. **An unknown `type` or an unknown field is `bad_message`** (never silently ignored) |
| Limits | Message 256 KiB, metadata 8 KiB, application message 64 KiB, 50 messages per second (above that `rate_limited`; at 4 times that, disconnect) |

The version is raised only for changes in meaning. Adding a field also raises the version (because unknown fields are
rejected, sending a new field to an older server results in `bad_message`; clients look at `connected.version` to decide what to send).

## 2. Flow

```
client                                   server
  |-- WebSocket (senca.rooms.v1) -------->|
  |-- connect {version, room_id, token} ->|  token check, (auth webhook), room limits, E2EE, codecs
  |<- connected {connection_id, participants, recv_slots} --|
  |-- offer {sdp} ----------------------->|  sending m-lines (sendonly) + receiving m-lines (recvonly), as many as recv_slots
  |<- answer {sdp, slots, published} -----|  whose track goes into each receive slot, IDs of the sent tracks
  |   ... ICE (the server is ICE-lite), DTLS, media ...
  |<- notify {connection.created / track.added ...}
  |<- renegotiate {audio, video} ---------|  not enough slots (a new sender)
  |-- offer {sdp} (with added slots) ---->|
  |<- answer {sdp, slots} ----------------|
  |<- slots {slots} (assignment changes) -|  a sender left, the spotlight changed
  |<- ping / -- pong -->                  |  every 10 seconds, pong within 10 seconds
  |-- disconnect ------------------------>|
```

**The client always makes the offer** (the server only answers), so glare cannot happen. The client does not send
another offer until it has received the answer to the previous one (doing so yields `invalid_state`). If a
`renegotiate` arrives while waiting for an answer, the client offers again after the answer.

## 3. Receive slots

**Each receiving m-line of the client is one slot** (`recvonly` or `sendrecv`). The server assigns the room's tracks to slots
and forwards them by switching the source of the SFU's outgoing track (the receiver sees a single SSRC; when the source changes, it starts from a keyframe).

- Assignment is stable: existing assignments are kept, and a new track goes into a free slot of the same kind. A slot whose sender left becomes free (`state: "empty"`)
  and is reused for the next sender (m-lines do not keep growing).
- If there are not enough slots, the server sends `renegotiate {audio, video}` (the **total** number of slots per kind). The client adds `recvonly` transceivers and makes an offer.
- The maximum is 32 slots per kind.
- Whose media a slot carries is given by `slots[].connection_id`, `client_id` and `track_id` (match them against the `mid` of the transceiver in `ontrack`).
- `state`: `active` (being forwarded), `paused` (assigned but not forwarded; outside the spotlight with `unfocused: "off"`), `empty`.

Not received: your own tracks, tracks of connections with the same `bundle_id`, tracks excluded by forwarding filters (§8), and anything for connections whose role is `sendonly`
or whose permissions have `subscribe: false`. A connection with `multistream: false` receives one track per kind (the spotlight focus, or the first sender if there is none).

## 4. Messages (client to server)

### `connect` (the first message, within 10 seconds)

| Field | Type | Default | Meaning |
|---|---|---|---|
| `version` | 1 | required | |
| `room_id` | string | required | 1 to 128 characters: alphanumerics and `-_.:@` |
| `token` | string | | Join token (§9). May be omitted if authorization is done by the auth webhook alone |
| `role` | `sendrecv`, `sendonly`, `recvonly` | required | Within the token's `role` (a `sendrecv` token allows any) |
| `client_id` | string | | If the token has a `client_id`, it must match. In rooms admitted by token alone, a value not in the token cannot be claimed (because it is used as a forwarding filter condition; the auth webhook sees this value and decides) |
| `bundle_id` | string | `connection_id` | Connections with the same value do not receive each other's tracks (the same person's camera and screen share). Like `client_id`, in rooms admitted by token alone only the token's `bundle_id` is used |
| `metadata` | JSON | | Passed only to the auth webhook (not shown to other participants) |
| `multistream` | bool | true | false receives only one track per kind |
| `simulcast` | `{rids, default_rid?}` | | rids of the layers sent (**lowest first**, 1 to 4, up to 16 alphanumeric characters). `default_rid` is the initial layer when receiving |
| `spotlight` | `{number, focus_rid?, unfocused?}` | room settings | Chooses the number of people and layers within the room's spotlight |
| `audio` / `video` | `{enabled, codec?, bitrate_kbps?}` | | If `codec` differs from the room's codec, `unsupported_codec` |
| `data_channels` | `[{label, direction, ordered, max_retransmits?, max_packet_life_time?}]` | | DataChannels to use (§6). The client opens them with the same labels |
| `signaling` | `{data_channel, close_websocket}` | false | Switch to the DataChannel (§6) |
| `e2ee` | bool | false | If it differs from the room's `e2ee`, `e2ee_mismatch` |
| `forwarding_filter` | §8 | | Narrows this connection's subscriptions |
| `sdk` | `{name, version}` | | For logs |

### Others

| `type` | Fields | Meaning |
|---|---|---|
| `offer` | `sdp` | Offer with the sending and receiving m-lines. BUNDLE and rtcp-mux are required |
| `candidate` | `candidate`, `sdp_mid?`, `sdp_mline_index?`, `username_fragment?` | Trickle ICE (an empty string means end-of-candidates). The server is ICE-lite, so this may be omitted |
| `pong` | `stats?` | Reply to `ping`. `stats` may carry a getStats summary (up to 64 KiB) |
| `layer` | `track_id`, `rid` (`null` for automatic) | Selects the layer of a track being received |
| `message` | `label`, `to?`, `data` or `data_base64` | Application message (`to` is a list of connection_ids; empty means everyone). The sender must have declared `label` with a sending direction (`sendrecv`, `sendonly`). Receivers are participants that declared `label` with a receiving direction and are allowed by their permissions. §6 |
| `stats` | `reports` | getStats summary (shown in the API's stats) |
| `disconnect` | `reason?` | Disconnects |

## 5. Messages (server to client)

| `type` | Fields | Meaning |
|---|---|---|
| `connected` | `version`, `connection_id`, `client_id`, `bundle_id`, `room_id`, `role`, `multistream`, `e2ee`, `video_codec`, `audio_codec`, `max_video_bitrate_kbps?`, `spotlight?`, `participants`, `recv_slots {audio, video}`, `data_channels`, `signaling_data_channel`, `ping_interval_ms`, `metadata` | Joined. `role` and `metadata` are the values after any change by the auth webhook. The first offer must include `recv_slots` receiving m-lines |
| `answer` | `sdp`, `slots[]`, `published[] {mid, track_id, kind, rids}` | The answer to the offer, and the assignment of all slots |
| `slots` | `slots[]` | Only the slots that changed |
| `renegotiate` | `audio`, `video` | Add slots up to these counts and offer again |
| `notify` | `event` and payload (below) | Room events |
| `push` | `data` | Push from the API |
| `message` | `label`, `from?`, `data` or `data_base64` | Application message delivered over signaling |
| `ping` | | Every 10 seconds. Without a `pong` within 10 seconds, disconnect with `ping_timeout` |
| `switched` | | From now on, signaling uses the DataChannel `signaling` (§6) |
| `redirect` | `location` | Reconnect to another node (followed by `disconnect {reason: "redirected"}` and close; for the exception see §13) |
| `error` | `code`, `message`, `fatal`, `request?` | §7 |
| `disconnect` | `reason` | Disconnects (followed by close) |

`event` of `notify`:

| `event` | Payload |
|---|---|
| `connection.created` | `participant {connection_id, client_id, bundle_id, role, metadata, tracks[], node?}` (`node` marks a participant on another node) |
| `connection.updated` | `connection_id`, `metadata` |
| `connection.destroyed` | `connection_id`, `client_id?`, `reason` |
| `track.added` | `track {track_id, connection_id, kind, rids[]}` |
| `track.removed` | `track_id`, `connection_id` |
| `spotlight.changed` | `focused[]` (connection_ids, most recently focused first) |
| `layer.changed` | `track_id`, `mid`, `rid` (the layer now being received) |
| `network.status` | `level` (0 good to 3 bad), `bandwidth_bps`, `rtt_ms` (your own link; at most once every 2 seconds) |

## 6. DataChannel

- **Application messages**: DataChannels whose label starts with `#`. Declare them in `connect.data_channels`; **the client opens them**
  (`pc.createDataChannel('#chat', {ordered, maxRetransmits})`). The server closes labels that were not declared or that do not match the room's `data_labels`
  or the token's `permissions.data`.
- A received message is forwarded as is (text as text, binary as binary) to **everyone else** who has opened the same label with a receiving `direction`
  (`sendrecv`, `recvonly`). It is not echoed back to the sender. Messages from a connection with `direction: "recvonly"` are not forwarded.
- To send to specific peers, use the signaling `message {label, to: [connection_id], data}`. Receivers get `message {label, from, data}`.
- **Switching signaling**: with `signaling: {data_channel: true}` and `data_channels: [{label: "signaling"}]`,
  once the client opens `signaling`, the server sends `switched` over the WebSocket and from then on sends over the DataChannel.
  The client may also send over the DataChannel from then on (messages sent over the WebSocket are still accepted).
  With `close_websocket: true`, closing the WebSocket does not disconnect (with false, it does). If the DataChannel closes, signaling falls back to the WebSocket;
  if that is closed too, the connection is disconnected.

## 7. Errors

```json
{"type":"error","code":"room_full","message":"room is full","fatal":true,"request":"connect"}
```

After `fatal: true`, the server sends `disconnect {reason: "error"}` and closes the WebSocket with the code below. `fatal: false`
rejects only that message and the connection continues (`bad_message` or `invalid_state` after joining, errors in the second and later offers, etc.).

| `code` | close | When |
|---|---|---|
| `bad_message` | 4400 | JSON, types, unknown fields, value ranges (fatal before joining) |
| `unsupported_version` | 4422 | `version` is not 1 |
| `auth_failed` | 4401 | Token missing, invalid or expired; rejected by the auth webhook or it timed out (5 seconds) |
| `forbidden` | 4403 | Token for another room, role or client_id does not match the token, sending a kind not permitted |
| `room_not_found` | 4404 | The room does not exist (senca-node never creates rooms on join) |
| `room_full` | 4503 | Room or node limit (a server configured with `overflow_redirect` sends a `redirect` instead; senca-node cannot be configured this way) |
| `capacity_unavailable` | 4503 | No node can host the room right now (on senca-node, when the grid cannot place it) |
| `role_violation` | 4403 | A `recvonly` connection offered a sending m-line |
| `e2ee_mismatch` | 4422 | `connect.e2ee` differs from the room's `e2ee` |
| `unsupported_codec` | 4422 | Differs from the room's codec |
| `invalid_sdp` | 4400 | The SDP cannot be parsed, or an m-line has no mid |
| `negotiation_failed` | 4500 | The answer cannot be built (BUNDLE, rtcp-mux, codecs) |
| `invalid_state` | 4400 | The first message is not `connect`, an offer while an offer is being processed, a message during authentication |
| `rate_limited` | 4429 | More than 50 per second |
| `unknown_track` | 4404 | The track_id or rid in `layer` does not exist or is not being received |
| `unknown_label` | 4403 | `message` with a label that may not be used |
| `timeout` | 4408 | No `connect` within 10 seconds |
| `internal` | 4500 | Server error |

Disconnect reasons (`disconnect.reason`, `connection.destroyed.reason`, events): `normal`, `websocket_closed`, `ping_timeout`,
`peer_failed` (ICE or DTLS failure, or not connected within 30 seconds of joining), `kicked` (API), `room_deleted`, `error`, `redirected`, `node_shutdown`.

## 8. Forwarding filters

```json
{"action":"block","rules":[[{"field":"kind","operator":"is_in","values":["video"]},
                            {"field":"client_id","operator":"is_not_in","values":["teacher"]}]]}
```

`rules` is an OR of AND groups (1 to 10 groups, each with 1 to 10 rules). Rules look at the **sender's** `connection_id`, `client_id`, `bundle_id`, `kind` and
`metadata.<key>` (string, number, boolean). `block` does not forward what matches; `allow` forwards only what matches.
Only what satisfies both the room settings' filter and the connection's filter is forwarded. Changing the room's filter reassigns slots immediately.

## 9. Authentication

### Join tokens (JWT-compatible)

Signed with a SENCA signing key (Ed25519 = `EdDSA`, P-256 = `ES256`). Can be issued with `POST /rooms/{id}/tokens` (senca-node signs with Ed25519).
`none`, the HS family (shared keys), the RS family and `crit` are rejected; the key determines the algorithm (a different `alg` in the header is rejected).

```json
{"iss":"senca","aud":"senca-rooms","exp":1767225600,"iat":1767222000,"jti":"...",
 "room":"daily","role":"sendrecv","client_id":"alice","metadata":{"name":"Alice"},
 "permissions":{"publish":["audio","video"],"subscribe":true,"data":["#chat"]}}
```

`exp` is required (clock skew up to 30 seconds), and the lifetime (`exp - iat`) is at most 24 hours. `metadata` is visible to other participants.
The default `permissions` are "may send both, may receive, `#*`".

### Auth webhook

When an auth webhook is configured, the server queries its URL with a `POST` on join (the body is signed, see below).
Without an answer within 5 seconds, the join is rejected. There are two modes:

- `after_token` (default): query after the token has been verified.
- `only`: authorize by the webhook's answer alone, instead of a token.

```json
{"version":1,"type":"connection.auth","request_id":"…","node":"node-a","room_id":"daily",
 "connection_id":"…","client_id":"alice","role":"sendrecv","metadata":{…the client's connect.metadata…},
 "token_claims":{…if any…},"remote_addr":"203.0.113.5:53211"}
```

Response (2xx; unknown fields are rejected):

```json
{"allowed":true,"role":"recvonly","client_id":"alice","metadata":{"name":"Alice"},
 "permissions":{"publish":[],"subscribe":true,"data":["#chat"]}}
```

With `allowed: false`, `reason` is passed to the client (`auth_failed`). Non-2xx responses and unreadable bodies are also rejections.

On senca-node:

- The webhook URL is the room setting `auth_webhook_url` (http/https) if present, otherwise `--rooms-auth-webhook`.
- `--rooms-auth-mode` selects the mode (`after_token` or `only`).
- Rooms without either URL are authorized by the token alone (a `client_id`/`bundle_id` not in the token cannot be claimed).
- The signing secret comes from `--rooms-webhook-secret-file`; joins to a room with a URL are rejected when no secret is configured.
- Destinations are checked against the `--egress-allow` policy (private addresses need to be allowed).
- Requests also carry `Senca-Webhook-Id` (the request_id).

### Signatures (common to webhooks)

`Senca-Signature: t=<UNIX seconds>,v1=<hex(HMAC-SHA256(secret, "<t>.<body>"))>`. While rotating secrets, multiple `v1=` are attached.
The receiver checks `|now - t| <= 300` seconds and that one of the `v1` values matches.
Event webhooks also carry `Senca-Webhook-Id` (event_id) and `Senca-Webhook-Attempt`.

## 10. Events (SSE and webhooks)

The same envelope is emitted to SSE (`/api/v1/events`) and webhooks.

```json
{"id":"01j…","version":1,"at":1767222000123,"node":"node-a","room_id":"daily",
 "type":"connection.destroyed","data":{"connection_id":"…","client_id":"alice","reason":"normal","duration_ms":61234}}
```

`type`: `room.created`, `room.updated`, `room.destroyed`, `connection.created`, `connection.updated`, `connection.destroyed`,
`track.published`, `track.unpublished`, `spotlight.changed`.

On senca-node's SSE the names are `room.started` (= `room.created`), `room.stopped` (= `room.destroyed`), `room.connection.*`,
`room.track.*` and `room.spotlight.changed`, with the payload in `data` (`room.updated` comes from the control-plane change event).

With `--rooms-event-webhook URL`, senca-node signs and POSTs the envelope above (with the webhook `type` names listed above;
`--rooms-webhook-secret-file` is required).

- Each node sends the events of its own participants (tell them apart by `node`).
- Cascade edge nodes do not send `room.created`, `room.updated` or `room.destroyed` (only the home node does).
- The number and IDs of abandoned events are shown in `rooms.event_webhook` of `GET /api/v1/nodes`.

Webhooks are delivered **at least once** (receivers deduplicate by `id`). A 2xx completes delivery. 408, 429, 5xx, connection failures and a 10-second timeout
are retried after 1, 2, 4, … seconds (±20 %, up to 1 hour), giving up after 10 attempts (dead letter). Other 4xx give up immediately.

## 11. REST

Follows the common `/api/v1` contract (Bearer, problem+json, `Idempotency-Key`, `If-Match`/`ETag`, `limit`/`cursor`, timestamps in RFC 3339).
Participants, messages and stats are answered by the node hosting the room (whichever node you send to, it relays to that node).
Updating or disconnecting a participant is relayed further to the node the participant is connected to (including a cascade edge).
Room `settings` updates change only the fields sent (JSON Merge Patch). Signaling may also connect to any node (if the room is on another node, `redirect`).

| Operation | Path |
|---|---|
| Create a room | `POST /rooms` `{id?, settings?, metadata?, placement?}` → 201 (a room is placed on one node as a grid workload; `placement` sets manual assignment and priority. ADR-0013) |
| List, get | `GET /rooms?limit&cursor`, `GET /rooms/{id}` (`ETag: "{generation}"`) |
| Update | `PATCH /rooms/{id}` `{settings?, metadata?, placement?}` (`If-Match` required. `e2ee`, `video_codec` and `audio_codec` cannot be changed while there are participants: 409) |
| Delete | `DELETE /rooms/{id}` (participants get `room_deleted`) |
| Participants | `GET /rooms/{id}/participants`, `GET …/{connection_id}` (`subscriptions` are the slots) |
| Participant metadata | `PATCH /rooms/{id}/participants/{connection_id}` `{metadata}` → `connection.updated` |
| Disconnect | `DELETE /rooms/{id}/participants/{connection_id}` (`kicked`) |
| Send | `POST /rooms/{id}/messages` `{to?, label?, data}` (with `label`, over the DataChannel; without, as `push`) |
| Stats | `GET /rooms/{id}/stats` (per participant: RTT, estimated bandwidth, bytes sent and received, bitrate of sent layers, layers being received, client getStats) |
| Join token | `POST /rooms/{id}/tokens` `{role, client_id?, bundle_id?, metadata?, permissions?, ttl_secs?}` (`ttl_secs` defaults to 600, 10 to 86400) → `{token, room_id, expires_at (RFC 3339), signaling_url}`. If the room does not exist it is created automatically (`auto: true`; it disappears after being empty for a while). Signed with Ed25519 |

Room settings (`settings`): `max_participants` (50), `e2ee`, `video_codec` (`vp8`, `vp9`, `h264`, `av1`), `audio_codec` (`opus`),
`max_video_bitrate_kbps`, `spotlight {number, unfocused: "lowest"|"off", focus_rid?}`, `recording` (`none`, `per_participant`,
`composite`; cannot be combined with E2EE), `forwarding_filter`, `data_labels` (`["#*"]`), `empty_timeout_secs` (60; for rooms created automatically when a token is issued),
`cascade`, `auth_webhook_url`. See [10.3](../guide/en/10-rooms.md) for details.

In E2EE rooms (SFrame and the like), the SFU forwards without looking at the content. Server-side compositing, composite recording and transcription are not possible:
creating a media pipeline with a composite or transcription for an E2EE room is rejected with 422 (`details.reason: "e2ee"`).

## 12. Layer selection and spotlight

The layer is chosen per receiver and per track. Order of precedence: the client's `layer` → spotlight (focused: `focus_rid` or automatic;
not focused: the lowest layer or `off`) → `simulcast.default_rid` → automatic (estimated bandwidth × 0.9 divided by the number of video subscriptions, the highest layer that fits;
going down only after it has persisted for 1 second (immediately if far short), going up only after 2 seconds). The switch happens at a keyframe of the new layer (the current layer is forwarded until then).

The spotlight detects speakers by audio level (RFC 6464 -dBov; if absent, an estimate from Opus packet sizes):
speaking after 200 ms, not speaking after 1.5 seconds of silence, and a focus is not removed for 2 seconds after it is set (to prevent flapping). There is one focus set per room
(`spotlight.changed`); each receiver gets `number` people, excluding itself.

## 13. Cascade

A room can span multiple nodes (ADR-0013, 0014). Participants on other nodes appear as participants with `node`,
and their tracks are assigned to slots like any other participant's.
When a room cannot be served by this node (grid placement, limits), the client is sent to another node with `redirect {location}`.

On senca-node:

- A redirect before the first `connect` (the room lives on another node) is a `redirect` message followed by
  close 4300 (no `disconnect`). If no `connect` arrives within 10 s, the socket is closed with 4408.
- The node the room is placed on (the grid assignment) is the **home**. If the room setting `cascade: true` (or the node flag
  `--rooms-cascade`) is set, a participant who connects to another node is not redirected but served by an **edge** instance
  of the room on that node. Rooms with `cascade` false are redirected to the home as before. Draining nodes do not create new edges.
- Participants of a node that has not synchronized with this node for 7 seconds are removed from the participant list.
- `max_participants` counts participants across nodes (simultaneous joins on different nodes may briefly exceed it).
- When the home dies, the grid moves the room to another node. If that node has an edge, it becomes the home with its participants.
  Participants of the dead node rejoin by connecting to another node. Edges without participants close after 20 seconds.

## 14. Example

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
