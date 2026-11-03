# 11. Media pipelines

[日本語](../ja/11-media-pipelines.md)

A media pipeline takes the participants of a room ([10-rooms.md](10-rooms.md)), exports them as SENCA streams, and lets you
specify recording, compositing, live egress and transcription in a single resource. It is the SENCA equivalent of
AWS Chime SDK Media Pipelines.

- Design rationale: [ADR-0016](../../dev/adr/0016-media-pipelines.md)
- API: `/api/v1/media-pipelines`

## 1. Mapping

| AWS (Chime SDK Media Pipelines) | SENCA |
|---|---|
| Media stream pipeline (to Kinesis Video Streams) | One stream per participant (`artifacts.participant_streams`). From there on it is an ordinary stream: HLS, recording, outputs and pipelines all apply |
| Media capture pipeline (record a meeting) | `recording`: automatic recording of the per-participant streams and the composite stream |
| Media concatenation pipeline | The ordinary recordings API: `GET /recordings/{id}/export.mp4` produces a single MP4 |
| Media live connector pipeline (meeting to RTMP) | `egress`: outputs of the composite stream (RTMP, SRT, PaaS). Requires `composite` |
| Media insights pipeline (transcription) | `captions`: captions and translation on each per-participant stream ([08-captions.md](08-captions.md)) |

## 2. How it works

- A media pipeline definition is a control-state resource; the API on any node of the cluster accepts it.
- **It runs on the node that hosts the room** (`placement_status.node` in `GET /rooms/{id}`). The runtime on that node looks at
  the room's participants **every second** and reconciles the streams, the composite and the egress outputs. If the room moves
  to another node, the runtime on the new node takes over ([09-cluster-grid.md](09-cluster-grid.md)).
- Only media pipelines with `desired_state: running` are run.
- The IDs of the child resources (streams, the composite pipeline, the egress outputs) are derived from the media pipeline ID,
  so they are the same every time they are recreated. They appear in `artifacts`. The composite pipeline and the outputs can
  also be read with `GET /pipelines/{id}` and `GET /outputs/{id}`.

### 2.1 Per-participant streams (no re-encoding)

- One stream is created per participant. The participant key is the `client_id` (or the `connection_id` if there is none).
- If `participants` is empty, everyone is exported. Otherwise only participants whose `client_id` is listed are exported
  (participants without a `client_id` are excluded).
- The **best layer** of the participant's video (simulcast rids are listed lowest first, so this is the last rid) and the audio
  are forwarded into the stream **as is, without re-encoding**. Video starts at the first keyframe.
- While exporting, the server asks the sender for a keyframe **every 2 seconds**, so that HLS viewers and recordings that join
  midway can start quickly. If the export falls behind and drops data, it also requests a keyframe and resumes from the next one.
- A participant with neither video nor audio is not exported. Only the first video track and the first audio track are used.
- When a participant's tracks or layers change, its export is rebuilt. When the participant leaves, the export stops (the stream remains).
- In a cascaded room ([10-rooms.md](10-rooms.md)), only participants connected to the room's host node (home) are exported.
  Participants connected to another node are not exported.

Stream attributes:

| Item | Value |
|---|---|
| Name | `{name}-{participant}-{first 6 characters of the stream ID}` (characters other than alphanumerics and `-_.` become `-`; the first part is truncated to 56 characters) |
| `metadata` | `media_pipeline` (the media pipeline ID), `room`, `participant` |
| `codec_profile` | `any` if the room's `video_codec` is `h264` or `h265`; otherwise (default `vp8`) `royalty_free` |
| `hls.container` | `fmp4` |
| Publisher | Only the room export. Publish keys are not accepted (the stream holds the hash of an unusable key) |

With `export_participants: false` the per-participant streams are still created, because they are the inputs of the composite.
Setting it to false means those streams cannot be recorded (`recording.participants`) or transcribed (`captions`).

### 2.2 Composite

The runtime creates a pipeline ([07-pipelines.md](07-pipelines.md)) that lays out the participants that have video, mixes all audio
(including audio-only participants) at 0 dB, and produces a single stream.

- A participant is added to the graph only after its stream is live, so the composite does not stall waiting for an input.
  When participants change, the graph is updated.
- If no participant has video, the composite pipeline is `stopped`.
- The composite pipeline is pinned to the node that hosts the room (`placement.node`) and has the same priority as the room.
  When the room moves, the runtime on the old node stops it and the runtime on the new node places it again.
- The composite stream is named `{name}-composite-{first 6 characters of its ID}` and the pipeline `{name}-compose-{first 6 characters of its ID}`.
- The composite stream's `codec_profile` is `royalty_free` if neither the composite `video_codec` nor the room's `video_codec`
  is H.264/H.265, and `any` otherwise.

Layouts:

| `layout` | Arrangement |
|---|---|
| `grid` | With n video participants: columns = ⌈√n⌉, rows = ⌈n / columns⌉. Ordered by participant key |
| `speaker` | The first participant (by participant key) is shown large on top (3/4 of the height); the rest share the bottom strip (1/4 of the height) equally. With one video participant it is the same as `grid`. Active speaker detection is not used by the composite yet |

Encoding:

| Item | Value |
|---|---|
| Video | `composite.video_codec`, `composite.bitrate_kbps`, GOP = `fps × 2` (a keyframe every 2 seconds) |
| Audio | Opus, 96 kbps |
| Clock | `realtime` |

Compositing decodes, composes and encodes in codec workers. Run the node with codec workers (`--codec-workers`, `--codec-lib-dir`;
see [07-pipelines.md](07-pipelines.md)). H.264/H.265 encoding needs a GPU. This is not checked at creation time; it fails when the
composite pipeline is placed or validated, and the error shows up in that pipeline's `placement_status`.

### 2.3 Recording (capture) and concatenation

- `recording.participants: true` applies an automatic recording policy (`recording.enabled: true`, `retention_days`) to the
  per-participant streams, and `recording.composite: true` to the composite stream. Recording starts when the stream goes live
  and is finalized when the publisher leaves ([05-recording.md](05-recording.md)).
- These are ordinary recordings. Find them with `GET /recordings?stream={stream ID}` and export one as a single MP4 with
  `GET /recordings/{id}/export.mp4`.
  - Segments are joined into an MP4 with `moov` at the front, without re-encoding. Discontinuities (gaps in the input) are
    joined by shifting timestamps.
  - While recording, the export returns 409 `CONFLICT`. Recordings larger than 4 GiB return 413 `PAYLOAD_TOO_LARGE`. Scope: `recording:read`.
- Recordings are stored in the node's data directory (`--data-dir`).

### 2.4 Egress (live connector)

- Each element of `egress` becomes an output of the composite stream (same shape and validation as output resources;
  see [06-inputs-outputs.md](06-inputs-outputs.md)).
- Allowed `protocol` values: `rtmp_push` (`rtmp://`, `rtmps://`), `srt_push` (`srt://`), and `paas` (`provider` is
  `cloudflare_stream`, `aws_ivs` or `aws_medialive`).
- Do not put credentials in the URL (`user@host` is rejected with 400). Reference a secret with `credential_ref: "secret:{name}"`.
- The destination IP is checked at connect time. Loopback, private and similar addresses are reachable only when the
  administrator allows them with `--egress-allow CIDR` ([12-security.md](12-security.md)).

### 2.5 Transcription (insights)

- `captions` has the same shape as a stream's `captions` (`CaptionSettings`; see [08-captions.md](08-captions.md)) and is applied
  to each per-participant stream.
- It is not applied to the composite stream. Requires `export_participants: true`.

## 3. Resource

### 3.1 Fields

| Field | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | (required) | 1–40 characters of alphanumerics and `-_.`, not starting with `.`. Unique within the tenant (shorter than the 64 allowed for streams because it is used in child stream names) |
| `room` | string | (required) | Room ID. 400 if it is not a room of the tenant. Cannot be changed after creation |
| `participants` | string[] | `[]` (everyone) | `client_id`s to export. Up to 100, each 1–255 bytes |
| `export_participants` | bool | `true` | Treat per-participant streams as deliverables. Required by `recording.participants` and `captions` |
| `composite` | object | none | Compositing; see below |
| `recording` | object | none | Recording; see below |
| `egress` | object[] | `[]` | Egress. Up to 8. Requires `composite` |
| `captions` | object | none | Transcription and translation (`CaptionSettings`) |
| `desired_state` | string | `running` | `running` or `stopped` |

`composite`:

| Field | Default | Range |
|---|---|---|
| `layout` | `grid` | `grid`, `speaker` |
| `width` | 1280 | even, 64–3840 |
| `height` | 720 | even, 64–2160 |
| `fps` | 30 | 1–60 |
| `video_codec` | `av1` | `av1`, `vp9`, `vp8`, `h264`, `h265` (H.264/H.265 need a GPU) |
| `bitrate_kbps` | 2500 | 100–50000 |

`recording`:

| Field | Default | Meaning |
|---|---|---|
| `participants` | `true` | Record the per-participant streams. Requires `export_participants: true` |
| `composite` | `true` | Record the composite stream. Requires `composite` |
| `retention_days` | none | 1–36500 |

The `participants` and `composite` flags of `recording` **default to true when omitted**. If you specify `recording` on a media
pipeline without `composite`, set `"composite": false` explicitly (otherwise you get 400).

`egress[]`:

| Field | Meaning |
|---|---|
| `protocol` | `rtmp_push`, `srt_push`, `paas` |
| `url` | Required except for `paas`. Up to 2048 characters, no spaces, control characters or credentials |
| `credential_ref` | `secret:{name}`. 400 if the tenant has no such secret. Required for `paas` (the API token) |
| `provider` | Required for `paas` |
| `options` | Up to 16 entries; keys 1–64 characters, values up to 256 characters |

Unknown fields are rejected with 400 (`INVALID_REQUEST`). A `Content-Type` other than `application/json` gets 415.

### 3.2 Response

```json
{
  "id": "…", "name": "mtg", "room": "meet", "generation": 1,
  "participants": [], "export_participants": true,
  "composite": {"layout":"grid","width":640,"height":360,"fps":30,"video_codec":"av1","bitrate_kbps":800},
  "recording": {"participants":true,"composite":true},
  "egress": [{"protocol":"rtmp_push","url":"rtmp://…"}],
  "desired_state": "running",
  "created_at": "…", "updated_at": "…",
  "artifacts": {
    "participant_streams": [{"stream":"…","name":"mtg-alice-1a2b3c","participant":"alice"}],
    "composite": {"stream":"…","pipeline":"…","outputs":["…"]}
  },
  "status": {
    "participants": [{"participant":"alice","stream":"…","video":true}],
    "composite_members": 2
  }
}
```

- `participants` and `egress` are omitted when empty; `composite`, `recording` and `captions` are omitted when absent.
- `artifacts.participant_streams`: the per-participant streams this media pipeline created (from control state; identical on every node).
- `artifacts.composite`: present only when `composite` is set. The IDs of the composite stream and pipeline, and the output IDs in `egress` order.
- `status`: **what is running on the node that answered**. It is `null` on nodes other than the room's host, and while stopped.
  `composite_members` is the number of inputs in the current composite graph (audio-only participants included), or `null` if there is no graph.
- Egress secrets are never returned (only the `credential_ref` name).

### 3.3 API

| Method and path | Scope | Description |
|---|---|---|
| `POST /api/v1/media-pipelines` | `stream:write` | Create. 201 with `Location` and `ETag` |
| `GET /api/v1/media-pipelines` | `stream:read` | List (ascending ID). `room`, `limit` (1–1000, default 100), `cursor`. Returns `{items, next_cursor}` |
| `GET /api/v1/media-pipelines/{id}` | `stream:read` | Definition, artifacts and status. `ETag` |
| `PATCH /api/v1/media-pipelines/{id}` | `stream:write` | Replace only the fields given. `If-Match` is required |
| `DELETE /api/v1/media-pipelines/{id}` | `stream:write` | Delete. 204 |

Order of checks on create, and errors:

1. Body shape (400 `INVALID_REQUEST`, 415)
2. `room` is a room of the tenant (400 `INVALID_CONFIG`)
3. Content validation (400 `INVALID_CONFIG`; the ranges and dependencies in 3.1)
4. Duplicate name (409 `CONFLICT`)
5. At most 8 per room (403 `QUOTA_EXCEEDED`)

Order on `PATCH`: missing `If-Match` (428) → body shape (400, 415) → not found (404) → stale generation (412) → the same content
validation as create (400) → renamed to a duplicate name (409). `room` cannot be changed (it is rejected as an unknown field, 400).
Writing `null` to `composite`, `recording` or `captions` removes it.

`POST` accepts `Idempotency-Key` ([15-api-and-sdks.md](15-api-and-sdks.md)).

## 4. Cleanup

| Operation | Composite pipeline and egress outputs | Per-participant streams, composite stream, recordings |
|---|---|---|
| `DELETE /media-pipelines/{id}` | Deleted (for PaaS outputs the provider-side resource is cleaned up too) | Kept |
| `PATCH` with `desired_state: stopped` | Deleted | Kept |
| `PATCH` removing `composite` or changing `egress` | Those of the previous definition are deleted (if still running, the runtime recreates them within a second) | Kept |
| `PATCH` shrinking `participants` | — | Streams of removed participants are kept too |
| `DELETE /rooms/{id}` | The room's media pipelines and their children are deleted | Kept |

- Exported streams and recordings **remain** as artifacts. Delete them with `DELETE /streams/{id}` and `DELETE /recordings/{id}` if you do not need them.
- When stopped, the runtime stops the participant exports (the streams' publisher goes away and recordings are finalized).
- Media pipelines removed by `DELETE /rooms/{id}` do not emit `media_pipeline.deleted` (only `room.deleted`).
- A room created automatically by issuing a join token (`auto: true`) is deleted after it has been empty for `empty_timeout_secs`
  (default 60 seconds), and its media pipelines go with it. In that case the composite pipeline is stopped, but the composite
  pipeline and egress output resources are left behind (see Caveats). Create rooms that use media pipelines explicitly with `POST /rooms`.

## 5. Events

On `GET /api/v1/events` (SSE; see [15-api-and-sdks.md](15-api-and-sdks.md)):

| type | `data` | When |
|---|---|---|
| `media_pipeline.updated` | `{room, desired_state}` | Create and update |
| `media_pipeline.deleted` | `{}` | Delete |

Child resources emit their own events (`stream.*`, `pipeline.updated`, `output.*`, recording events).

## 6. Examples

Assumptions: `$API=http://127.0.0.1:8080/api/v1`; `$TOKEN` has `stream:write`, `stream:read`, `subscribe` and `recording:read`
(the examples use the bootstrap admin token). Compositing needs a node with codec workers, and pushing egress to the same machine
needs `--egress-allow 127.0.0.0/8`.

```sh
api() { m=$1; p=$2; shift 2; curl -s -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "$API$p" "$@"; }

# Create the room explicitly (auto rooms disappear when empty)
api POST /rooms -d '{"id":"meet","settings":{"max_participants":8,"video_codec":"vp8"}}'

# Media pipeline: per-participant streams, an AV1 grid composite, recording of both, composite pushed over RTMP
api POST /media-pipelines -d '{
  "name": "mtg", "room": "meet",
  "composite": {"layout":"grid","width":640,"height":360,"fps":30,"video_codec":"av1","bitrate_kbps":800},
  "recording": {"participants":true,"composite":true},
  "egress": [{"protocol":"rtmp_push","url":"rtmp://live.example.com/app/STREAMKEY"}]
}' | tee mp.json
MP=$(jq -r .id mp.json); CSID=$(jq -r .artifacts.composite.stream mp.json)

# Participant token (client_id becomes the participant key)
api POST /rooms/meet/tokens -d '{"role":"sendrecv","client_id":"alice"}'

# While participants are in the room: artifacts and status
api GET /media-pipelines/$MP | jq '.artifacts, .status'

# Watch the composite stream over HLS
api POST /streams/$CSID/playback-url | jq -r .playback.hls

# After they leave: export the composite recording as one MP4
RID=$(api GET "/recordings?stream=$CSID" | jq -r '.items[0].id')
curl -s -H "Authorization: Bearer $TOKEN" -o composite.mp4 "$API/recordings/$RID/export.mp4"

# Stop (If-Match with the current generation)
G=$(api GET /media-pipelines/$MP -D - -o /dev/null | sed -n 's/^[Ee][Tt]ag: *//p' | tr -d '\r')
api PATCH /media-pipelines/$MP -H "If-Match: $G" -d '{"desired_state":"stopped"}'

# Delete (composite pipeline and egress go away; streams and recordings remain)
api DELETE /media-pipelines/$MP
```

Other shapes:

```jsonc
// Recording only (no composite). Set composite: false explicitly
{"name":"rec","room":"meet","recording":{"participants":true,"composite":false,"retention_days":30}}

// Export and transcribe a single participant
{"name":"cap","room":"meet","participants":["teacher"],
 "captions":{"asr_provider":"whisper_cpp","asr_endpoint":"http://asr.internal:8080","language":"ja","targets":["en"],
             "translate_provider":"deepl","translate_endpoint":"https://api.deepl.com","translate_credential_ref":"secret:deepl"}}

// Speaker layout, H.264 composite to a PaaS (needs a GPU)
{"name":"show","room":"meet",
 "composite":{"layout":"speaker","video_codec":"h264","bitrate_kbps":4000},
 "egress":[{"protocol":"paas","provider":"cloudflare_stream","credential_ref":"secret:cf-token"}]}
```

## 7. Grid resource estimates

A media pipeline is not itself a grid workload. The grid accounts for it as follows:

- **Participant export** runs on the same node as the room, so it is added to the room workload's estimate. For each running
  media pipeline, with m = the room's `max_participants` (default 50): CPU `20 × m` millicores and memory `8 × m` MB.
- **Composite** is a separate workload (a pipeline), estimated from its graph:

| Graph node | CPU (millicores) | Memory (MB) | GPU |
|---|---|---|---|
| Base | 200 | 128 | — |
| Decode (two per participant: video and audio) | 400 (100 for H.264/H.265) | 128 | 1 decode for H.264/H.265 |
| `compose` | 300 | 64 | — |
| `audio_mix` | 50 | — | — |
| Video encode (AV1, VP8, VP9) | 1500 | 128 | — |
| Video encode (H.264, H.265) | 100 | 128 | 1 encode |
| Audio encode (Opus) | 50 | 128 | — |

Network is 10 Mbit/s in and out. Example: compositing two participants of a VP8 room into AV1 costs
CPU 200 + 400 × 4 + 300 + 50 + 1500 + 50 = 3700 millicores and memory 128 + 128 × 4 + 64 + 128 × 2 = 960 MB.

## 8. Caveats

- A media pipeline only exports participants connected to the node that hosts the room. In a cascaded room, participants
  connected to another node are not exported.
- `status` has a value only on the room's host node. Behind a load balancer another node may answer and return `null`.
- When an automatically created room is deleted for being empty, its media pipelines are deleted, but the composite pipeline
  (stopped) and the egress output resources remain. Find them with `GET /pipelines` and `GET /outputs` and delete them.
- Recordings are stored in the node's data directory. Storing to S3-compatible storage and WHIP egress are not supported.
