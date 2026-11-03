# 7. Pipelines

[日本語](../ja/07-pipelines.md)

A pipeline is a processing graph (a directed acyclic graph) that takes streams as input, decodes, corrects whiteboards, scales, crops,
composes, mixes audio and encodes, and feeds the result into another stream. Use it for picture-in-picture, scene switching (a
production switcher), transcoding and similar tasks.

- Rationale: [ADR-0004 codecs and patents](../../dev/adr/0004-codecs-patents.md), [ADR-0013 grid](../../dev/adr/0013-grid.md) (placement and degradation)
- Related pages: [4. Ingest and playback](04-ingest-playback.md), [9. Cluster and grid](09-cluster-grid.md), [14. Codecs and patents](14-codecs-and-patents.md), [15. API and SDKs](15-api-and-sdks.md)

`API`, `TOKEN` and `H` in the examples are the same as in [4. Ingest and playback](04-ingest-playback.md).

In this chapter, "node" in a graph means a processing node (an element of the graph's `nodes` array). Servers are called SENCA nodes where the two could be confused.

## 7.1 How it works

- The graph runs inside a codec worker (a separate, sandboxed process). Raw frames never leave the worker. A node without a codec
  worker cannot run pipelines (`pipelines` is `false` in `GET /capabilities`).
- `source` reads access units from a SENCA stream; `sink` feeds the resulting access units into the destination stream **as its sender**.
  The destination stream behaves like any other stream (HLS, recording, outputs, captions). If another sender is connected to the
  destination stream, the pipeline cannot publish.
- A pipeline is a grid workload (`pipeline:{id}`) and prefers nodes that receive its input streams ([9. Cluster and grid](09-cluster-grid.md)).
- When the worker crashes or fails, `observed_state` becomes `error` and the pipeline is rebuilt after 1, 2, 4 … 32 seconds.

## 7.2 API

| Operation | Request | Scope |
|---|---|---|
| Create | `POST /pipelines` `{name, graph, desired_state?, placement?}` | `stream:write` |
| List | `GET /pipelines?limit&cursor` | `stream:read` |
| Get | `GET /pipelines/{id}` | `stream:read` |
| Update | `PATCH /pipelines/{id}` `{name?, graph?, desired_state?, placement?}` (`If-Match` required) | `stream:write` |
| Delete | `DELETE /pipelines/{id}` | `stream:write` |
| Validate | `POST /pipelines/{id}/validate` (optionally with another graph in the body; otherwise the saved graph) | `stream:read` |
| Operate | `POST /pipelines/{id}/control` | `stream:write` |

- `name` follows the stream name rules and is unique within the tenant. A tenant can have up to 100 pipelines.
- **`desired_state` defaults to `stopped`.** Pass `"running"` to run it.
- The graph is validated on create and update. The streams referenced by `source` and `sink` must exist and belong to the tenant.
- Changing `graph` stops the pipeline and restarts it with the new graph (the output is interrupted).
- Responses include `observed_state` (`starting`, `running`, `error`, `stopped`), `last_error`, `plan` (the validation estimate),
  `status` and `meters` (worker status and meters, updated at about 10 Hz by default), `placement` and `placement_status`.

Validation failures are problem+json with `details.issues` as `[{code, node, message}]`. If equipment is missing (any
`CAPACITY_UNAVAILABLE` issue) the answer is 503 `CAPACITY_UNAVAILABLE` (`retryable: false`); otherwise 400 `INVALID_CONFIG`.

| `issues[].code` | Meaning |
|---|---|
| `MALFORMED` | Not valid JSON, unknown fields, `version` other than 1, or larger than 16 MiB |
| `INVALID_PARAM` | Out-of-range or malformed value (including ID format) |
| `DUPLICATE_ID` | Two nodes share an `id` |
| `UNKNOWN_REFERENCE` | Reference to an unknown node, port, scene or image |
| `TYPE_MISMATCH` | Port types do not match (encoded media into a compositor, video encoded with Opus, raw frames into a sink, …) |
| `CYCLE` | The graph has a cycle |
| `UNCONSUMED_OUTPUT` | An output of a non-terminal node (anything but `sink` and `snapshot_on_change`) is not used (`source`: every declared track; `compose`: `program`) |
| `POLICY_REJECTED` | H.264, H.265 or AAC used under the royalty-free-only policy (passthrough included) |
| `UNSUPPORTED_CODEC` | AAC decode or encode, or an unknown codec |
| `CAPACITY_UNAVAILABLE` | No H.264/H.265 hardware, no provider for the resolution, or the hardware session limit would be exceeded |
| `LIMIT_EXCEEDED` | Limits on the number of nodes, resolution, images, scenes or inputs exceeded |

If any `source` or `sink` stream has `codec_profile: royalty_free`, the whole graph is validated under the royalty-free-only policy.

## 7.3 Graph structure

```json
{"version": 1, "options": {...}, "nodes": [ {"type": "...", "id": "...", ...}, ... ]}
```

- `version` is 1. Unknown fields are rejected.
- `id` is 1–64 characters of letters, digits, `_` and `-`. Up to 256 nodes.
- An input reference is `"node"` or `"node.port"`. For `source` the port is mandatory (`cam.video`, `cam.audio`). `compose` has the
  ports `program` (default) and `preview`. Other nodes have the single port `out`, which may be omitted.
- Resolutions are 2–7680 × 2–4320, frame rates up to 240 (up to 120 for compositor canvases).

Port types:

| Type | Produced by |
|---|---|
| Encoded video/audio (with codec) | `source` (`video`, `audio`), `encode` |
| Raw video | `decode` (video), `board`, `scale`, `crop`, `compose` |
| Raw audio (48 kHz) | `decode` (audio), `audio_mix` (48 kHz stereo) |

### `options`

| Item | Default | Range | Meaning |
|---|---|---|---|
| `clock` | `realtime` | `realtime`, `input` | `realtime`: output ticks follow a monotonic clock (live). `input`: advance by input timestamps (emit when all inputs are present; for files and tests) |
| `overflow` | `drop` | `drop`, `block` | When a queue is full. `drop`: discard the oldest (video until the next keyframe). `block`: make upstream wait (discard after `block_timeout_ms`) |
| `latency_ms` | 100 | 0–10000 | Delay that absorbs input jitter; compositor and mixer render this far behind |
| `missing_timeout_ms` | 2000 | 100–60000 | How long without data before an input counts as missing |
| `block_timeout_ms` | 2000 | 10–60000 | Maximum wait with `block` |
| `input_queue_aus` | 256 | 8–100000 | Video access-unit input queue length |
| `frame_queue` | 8 | 1–256 | Raw frame queue per node input |
| `status_interval_ms` | 100 | 0 or 20–60000 | Status and meter interval (0 disables) |
| `commit_timeout_ms` | 10000 | 100–600000 | Maximum wait when switching to a new generation (not used through the node API) |

## 7.4 Node types

### `source` (input)

| Item | Required | Meaning |
|---|---|---|
| `stream` | yes | SENCA stream ID |
| `video`, `audio` | at least one | Tracks to use: `{codec, track?, width?, height?, fps?, sample_rate?, channels?}` |

`codec` is `h264`, `h265`, `av1`, `vp8`, `vp9`, `opus`, `aac`, and so on (the codec the input actually sends). Without `track`, the first
track of that kind is used. `width`, `height`, `fps`, `sample_rate` (8000–192000) and `channels` (1–8) are used for validation and estimates.

### `decode`

`{input, max_width?, max_height?, threads?}`. Turns encoded input into raw frames or audio. `max_width`/`max_height` cap the accepted
resolution (default 3840×2160); `threads` is the software codec thread count (0 = automatic, up to 64). AAC is never decoded (`UNSUPPORTED_CODEC`).

### `board` (whiteboard correction)

`{input, params}`. Rectifies a whiteboard or blackboard to a frontal view and makes it easier to read.

| `params` item | Default | Meaning |
|---|---|---|
| `mode` | `whiteboard` | `whiteboard`, `blackboard` |
| `corners` | none | Four board corners (top-left, top-right, bottom-right, bottom-left, in input pixels) as `[[x,y],…]` |
| `auto_detect` | `false` | Detect the corners automatically (tried every frame until found) |
| `redetect_frames` | 0 | Re-detection interval in frames (0 = never) |
| `output_width`, `output_height` | 1280, 720 | Output size (at least 16). Without `corners` or `auto_detect`, the input size is kept |
| `enhance` | `true` | Correct uneven lighting and glare, and enhance |
| `stroke_gain`, `chroma_gain` | none | 0–10 |
| `glare_suppression` | none | |
| `invert_to_white` | `false` | Turn a blackboard into dark strokes on white |
| `clahe` | `false` | Luma CLAHE |

### `scale`, `crop`

- `scale`: `{input, width, height, filter?}`. `filter` is `nearest`, `bilinear` (default), `bicubic`, `lanczos3` or `area`.
- `crop`: `{input, x, y, width, height}` in input pixels. Sizes are rounded down to even numbers. If the input size is known, a rectangle that does not fit is `INVALID_PARAM`.

### `compose` (compositor)

| Item | Default | Meaning |
|---|---|---|
| `inputs` | (required) | Name used by scenes → upstream reference (raw video). Up to 32 |
| `canvas` | (required) | `{width, height, fps_num, fps_den?(1), background?("#000000")}`; width and height must be even |
| `images` | `{}` | Still images (logos, …): name → `{png_base64}` or `{jpeg_base64}`. Up to 32, each up to 8 MiB of base64. Validation actually decodes them |
| `scenes` | (required) | Scene name → `{layers: [...]}`. 1–64 scenes |
| `program` | (required) | Scene sent to the output |
| `preview` | none | Scene rendered on the `preview` port |
| `transition` | `{kind: cut, duration_ms: 0}` | Default transition for `take`. `kind` is `cut`, `fade`, `wipe_left` or `wipe_right`; `duration_ms` up to 60000 |
| `missing` | `{hold_ms: 2000, slate_color: "#000000"}` | When an input stops: hold the last frame for `hold_ms` (up to 600000), then show a slate (`slate_image`, an `images` name, takes precedence over the color) |
| `threads` | 0 | Rendering parallelism (0 = number of CPUs, up to 64) |

Names (inputs, images, scenes) are 1–64 characters of letters, digits, `_` and `-`.

Each layer (`layers[]`) has **exactly one** of `source` (an `inputs` name), `image` (an `images` name) or `color` (`#RRGGBB`):

| Item | Default | Meaning |
|---|---|---|
| `x`, `y`, `width`, `height` | (required) | Position and size on the canvas |
| `crop` | 0 | `{left, top, right, bottom}`: fractions trimmed from each edge |
| `fit` | `contain` | `stretch`, `contain`, `cover` |
| `z` | 0 | Stacking order (higher is on top) |
| `opacity` | 1.0 | 0–1 |
| `corner_radius` | 0 | Up to 4096 |
| `border` | none | `{width (up to 512), color}` |
| `visible` | `true` | |
| `filter` | `bicubic` | Scaling kernel |

### `audio_mix`

| Item | Default | Meaning |
|---|---|---|
| `inputs` | (required) | Name → `{from, gain_db?(0), mute?, solo?, pan?(0), delay_ms?(0)}`; `from` is raw audio. 1–32 inputs |
| `master_gain_db` | 0 | -120–24 |
| `limiter` | `true` | |

`gain_db` is -120–24, `pan` -1–1, `delay_ms` 0–5000. The output is 48 kHz stereo.

### `encode`

| Item | Default | Meaning |
|---|---|---|
| `input` | (required) | Raw video or raw audio |
| `codec` | (required) | Video: `av1`, `vp8`, `vp9`, `h264`, `h265`. Audio: `opus` |
| `bitrate_kbps` | (required) | 1–800000 |
| `gop` | 2 seconds' worth | Keyframe interval in frames (up to 100000) |
| `low_latency` | `true` | |
| `rate_control` | `cbr` | `cbr`, `vbr` |
| `width`, `height` | input size | Give both. Frames of other sizes are scaled |
| `frame_ms` | 20 | Opus packet duration: 2.5, 5, 10, 20, 40 or 60 |
| `threads` | 0 | Up to 64 |

`h264` and `h265` require a GPU (NVENC, oneVPL, AMF) and are otherwise `CAPACITY_UNAVAILABLE`. `av1` uses SVT-AV1 (or hardware),
`vp8`/`vp9` libvpx, `opus` libopus. There is no AAC encoder ([14. Codecs and patents](14-codecs-and-patents.md)).

### `snapshot_on_change` (still images when the board changes)

`{input, params}`. Analyzes raw video and saves a JPEG when the content has changed and settled. Images are stored as still images of
**the first `sink`'s stream** (`GET /streams/{id}/snapshots`, `reason: board_change`, with `changed_fraction`). Images saved this way
are not pruned by any count limit.

| `params` item | Default | Range |
|---|---|---|
| `mode` | `whiteboard` | `whiteboard`, `blackboard` |
| `enabled` | `true` | When false, start with the `snapshot` operation |
| `analysis_fps` | 2.0 | 0–120 (0 = every frame) |
| `stable_frames` | 3 | 1–10000 |
| `change_fraction` | 0.004 | Greater than 0 and less than 1 |
| `jpeg_quality` | 85 | 1–100 |

### `sink` (output)

`{inputs, stream}`. `inputs` holds at most one encoded video and one encoded audio input (1–2 entries); they become tracks 0 and 1 in
that order. Feeding a `source` track directly passes it through without re-encoding. `stream` is the destination stream ID.

Make sure the destination stream's HLS container can carry the codecs (AV1 and VP9 need `hls.container: fmp4`; see [4.9](04-ingest-playback.md)).

## 7.5 Example: picture-in-picture

The whiteboard camera (`jb`) fills the frame, the teacher (`jt`) is overlaid
small in the lower right, the teacher's audio is mixed to Opus, and the result is AV1. All three streams use
`codec_profile: royalty_free` and `hls.container: fmp4`.

```sh
for n in jt jb jout; do
  curl -s -X POST "$API/streams" "${H[@]}" -d '{"name":"'$n'","codec_profile":"royalty_free","hls":{"container":"fmp4"}}' > $n.json
done
id() { python3 -c "import json;print(json.load(open('$1.json'))['id'])"; }

cat > graph.json <<EOF
{"version":1,"options":{"clock":"realtime"},"nodes":[
 {"type":"source","id":"t","stream":"$(id jt)","video":{"codec":"vp9"},"audio":{"codec":"opus"}},
 {"type":"source","id":"b","stream":"$(id jb)","video":{"codec":"vp9"}},
 {"type":"decode","id":"dt","input":"t.video"},
 {"type":"decode","id":"db","input":"b.video"},
 {"type":"decode","id":"at","input":"t.audio"},
 {"type":"compose","id":"prod","inputs":{"board":"db","teacher":"dt"},
  "canvas":{"width":640,"height":360,"fps_num":30},
  "scenes":{
    "pinp":{"layers":[{"source":"board","x":0,"y":0,"width":640,"height":360},
                      {"source":"teacher","x":440,"y":250,"width":192,"height":108,"z":1}]},
    "teacher":{"layers":[{"source":"teacher","x":0,"y":0,"width":640,"height":360}]}},
  "program":"pinp","preview":"teacher","transition":{"kind":"fade","duration_ms":300}},
 {"type":"audio_mix","id":"mix","inputs":{"mic":{"from":"at","gain_db":0}}},
 {"type":"encode","id":"venc","input":"prod","codec":"av1","bitrate_kbps":800,"gop":30},
 {"type":"encode","id":"aenc","input":"mix","codec":"opus","bitrate_kbps":96},
 {"type":"sink","id":"out","inputs":["venc","aenc"],"stream":"$(id jout)"}]}
EOF

python3 -c "import json;print(json.dumps({'name':'pinp','graph':json.load(open('graph.json')),'desired_state':'running'}))" > body.json
curl -s -X POST "$API/pipelines" "${H[@]}" --data-binary @body.json
```

Once VP9 (and Opus) is sent to `jt` and `jb`, `jout` goes live; watch the composed result through `jout`'s playback URL.

This setup needs VP9 decoding (libvpx), AV1 encoding (SVT-AV1) and Opus encoding (libopus). See [14. Codecs and patents](14-codecs-and-patents.md) for installation.

## 7.6 Operator controls

Send an operation as JSON (`"op"` selects the kind; unknown fields are 400) to `POST /pipelines/{id}/control`. Operations on
compositors and mixers take effect at the next output frame boundary.

```sh
# Take the preview (teacher) to program with the default transition (fade, 300 ms); the old program becomes the preview
curl -s -X POST "$API/pipelines/$PID/control" "${H[@]}" -d '{"op":"take","node":"prod"}'
# Switch scenes with an explicit transition
curl -s -X POST "$API/pipelines/$PID/control" "${H[@]}" -d '{"op":"switch_program","node":"prod","scene":"pinp","transition":{"kind":"wipe_left","duration_ms":500}}'
# Microphone fader
curl -s -X POST "$API/pipelines/$PID/control" "${H[@]}" -d '{"op":"set_audio","node":"mix","input":"mic","gain_db":-6}'
```

`node` is the `id` of the processing node the operation targets.

| `op` | Fields | Meaning |
|---|---|---|
| `switch_program` | `node`, `scene`, `transition?` | Switch the program scene (cut if omitted) |
| `set_preview` | `node`, `scene` | Set the preview scene |
| `take` | `node`, `transition?` | Move preview to program (default: the compositor's `transition`); the old program becomes the preview |
| `update_layer` | `node`, `scene`, `layer` (index), `patch` | Partially update a layer; `patch` may contain `x`, `y`, `width`, `height`, `z`, `opacity`, `visible`, `source` |
| `set_scene` | `node`, `scene`, `layers` | Create or replace a scene |
| `set_audio` | `node` (`audio_mix`), `input`, `gain_db?`, `mute?`, `solo?`, `pan?`, `delay_ms?` | Change an audio input (only the given fields) |
| `set_master` | `node`, `gain_db` | Master level |
| `snapshot` | `node` (`snapshot_on_change`), `enabled` | Start or stop change detection |
| `request_keyframe` | `node?` | Request a keyframe (all video encoders if omitted) |

- The response is 200 `{"ok": true, "reply": {...}}`. If the pipeline is not running, 409; if the operation is invalid (unknown node,
  scene, …), 400 with `details.reason`.
- Operations such as `set_scene` and `update_layer` affect only the running pipeline and do not change the saved graph (a restart returns to the graph's contents).
- Generation operations (`prepare`, `commit`, `rollback`) are not available through this API (400). Change the graph with `PATCH`.

## 7.7 Degradation

When devices or node resources run short, the grid can run a pipeline at a predefined **degradation level** with some nodes removed
instead of stopping it.

```sh
curl -s -X PATCH "$API/pipelines/$PID" "${H[@]}" -H "If-Match: $ETAG" -d '{
  "placement": {"priority": "high", "degrade": [
    {"label": "no-board", "disable": ["b"]}
  ]}
}'
```

- `placement.degrade` lists levels from best to worst (up to 8). Each level is `{label, disable}`. Labels must be unique and cannot be
  `full` (the name of level 0). `disable` must not be empty and may contain only node IDs present in the graph.
- Levels **accumulate**: level 2 removes the `disable` lists of both level 1 and level 2.
- Pruning: the listed nodes are removed, and so is every downstream node whose inputs are all removed. Nodes that lose
  only some inputs (multi-input `sink`, `compose`, `audio_mix`) stay, minus the removed inputs; a `compose` also drops, from every
  scene, the layers that use a removed input.
- The pruned graph is validated too. Removing downstream nodes while leaving their upstream makes the upstream outputs
  `UNCONSUMED_OUTPUT`, and the pipeline will not run, so **remove from the upstream end (for example the `source`)**. The example removes
  the whiteboard `source` (`b`); its `decode` (`db`) follows, and the board layer disappears from the compositor (teacher only).
- When the level changes, the pipeline restarts with the pruned graph. The result appears in `placement_status` (`state: degraded`, `level`, `level_label`).
- Resource requirements per level (CPU, memory, GPU sessions) are estimated from the pruned graph.

For how levels are chosen, priorities, and disabling devices, see [9. Cluster and grid](09-cluster-grid.md).
