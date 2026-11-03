# 1. Overview

SENCA is a media server for live streaming, recording and real-time communication, written in Rust and licensed
under Apache-2.0. It runs as a single `senca-node` process or as a cluster of nodes (the grid).

This page describes the programs that make up SENCA, its internal architecture, how it handles media data, the
feature map and what has been verified.

- Related pages: [2. Quickstart](02-quickstart.md), [3. Configuration](03-configuration.md), [12. Security](12-security.md), [13. Operations](13-operations.md)
- Rationale: [ADR-0001 Architecture](../../dev/adr/0001-architecture.md)

## 1.1 Executables

SENCA consists of three executables.

| Executable | Role |
|---|---|
| `senca-node` | The server. Ingest (gateway), media processing, delivery (origin) and the control plane share one process |
| `senca-codec-worker` | Runs codecs in a separate, sandboxed process. `senca-node` starts a pool of them and talks to them over pipes (large frames go through shared memory) |
| `senca-capture-worker` | Separate process for capture devices (V4L2, ALSA, DeckLink). Without it, capture devices are simply unavailable |

`senca-node` looks for `senca-codec-worker` and `senca-capture-worker` in its own directory
(the codec worker location can be overridden with `--codec-worker-bin`).

## 1.2 Architecture

The overall architecture is shown below. Every ingested stream goes into a per-stream hub, and from there it branches
into delivery, recording and processing.

```
            RTMP/RTMPS  SRT  WHIP  RTSP/RTMP/SRT/HLS pull  capture
                 │       │     │            │                 │
                 ▼       ▼     ▼            ▼                 ▼
          ┌──────────────────────────────────────────────────────────┐
          │ stream hub (one task per stream, bounded queues)          │
          │   ├─ packager → LiveCache (memory) → HTTP: HLS (TS/fMP4/LL-HLS)
          │   ├─ subscribers: RTMP play, WHEP, RTSP play, push outputs, pipelines
          │   └─ recorder → segments/ (immutable files) + recordings/{id}/index.jsonl
          └──────────────────────────────────────────────────────────┘
                 │ encode / decode / compose               ▲
                 ▼                                         │
          senca-codec-worker (separate, sandboxed process)   │
                                                           │
  Management API (REST, SSE) → control state (deterministic state machine)
        single node: control/log (append + fsync) / cluster: Raft (control/raft/)
        grid: places work (inputs, outputs, pipelines, rooms) on nodes
        relay: carries encoded access units between nodes
```

### Control plane

- The control state (streams, tokens, recording metadata, schedules, inputs, outputs, secrets, pipelines, rooms
  and so on) is kept as the control state, a deterministic state machine updated by applying commands in order.
- On a single node, each applied command is appended to `control/log` and fsynced before the API answers. Every
  1000 commands the state is written to `control/snapshot.json` and the log is emptied.
- In a cluster, the same commands are replicated with SENCA's own Raft implementation, with 1 to 7
  voters. A write sent to any node is forwarded to the leader. The side that loses the majority cannot write (503).
  Reads are served from each node's local copy. See [ADR-0009](../../dev/adr/0009-cluster.md) and
  [9. Cluster and grid](09-cluster-grid.md).

### Stream hub, packager and recorder

- **Hub** ([ADR-0006](../../dev/adr/0006-stream-hub.md)): one task per stream. It outlives connections; if the publisher
  reconnects within the grace period (`--reconnect-grace`, 30 s by default) the stream simply continues.
  Its input queue is bounded, so a slow hub stops reading from the publisher (TCP backpressure).
- **Packager**: segmenting and muxing happen once, inside the hub, and the result is kept in the in-memory
  cache (`LiveCache`). Live HLS never touches the disk.
- **Recorder** ([ADR-0005](../../dev/adr/0005-recording.md)): only while something is being recorded, the same segments
  that serve live HLS are written to `segments/` as immutable files and appended to a per-recording index
  (`recordings/{id}/index.jsonl`). If its queue overflows the recorder drops segments and records a gap; live
  delivery is never stalled. On startup, unfinished recordings are closed as `interrupted`
  ([13. Operations](13-operations.md)).

### Codec worker sandbox

H.264/H.265 are encoded and decoded only through GPU vendor SDKs (NVENC/NVDEC, oneVPL, AMF, VA-API); AV1, VP8/VP9
and Opus use royalty-free libraries (dav1d, SVT-AV1, libvpx, libopus). All of them are loaded at run time with
`dlopen`, never into `senca-node` itself, but into `senca-codec-worker`, which runs as a separate process restricted
with seccomp, Landlock and resource limits ([12. Security](12-security.md),
[ADR-0004](../../dev/adr/0004-codecs-patents.md), [14. Codecs and patents](14-codecs-and-patents.md)).
Without the worker or the libraries, SENCA can still pass through, deliver and record what it receives;
transcoding, snapshots and thumbnails return `503 CAPACITY_UNAVAILABLE`.

### Grid

The grid accounts for node resources (CPU, memory, bandwidth, GPUs, capture devices) and places work (pull inputs,
push outputs, pipelines, rooms, media pipelines) on nodes. Placement is decided by the Raft leader and written to
the replicated log. When a node or a device fails, its work moves elsewhere or degrades, and it comes back when the
resource returns. A single node runs the same machinery ([ADR-0013](../../dev/adr/0013-grid.md),
[9. Cluster and grid](09-cluster-grid.md)).

### Relay

In a cluster, when a viewer reaches a node that is not receiving the stream, the node pulls encoded access units
from the owning node over one multiplexed inter-node connection and serves them from a local mirror
([ADR-0014](../../dev/adr/0014-relay.md)). There is a single upstream per stream, no matter how many viewers the node has.

### Realtime rooms

A WebRTC SFU with simulcast, spotlight, forwarding filters, DataChannels and E2EE pass-through. Signaling is a
WebSocket (`GET /signaling`), and participants authenticate with join tokens. Each room is a grid workload placed on
one node ([ADR-0015](../../dev/adr/0015-realtime-rooms.md), [10. Realtime rooms](10-rooms.md)).

### Media pipelines

Export room participants as SENCA streams and feed them into compositing, recording, RTMP and other outputs, and
transcription ([ADR-0016](../../dev/adr/0016-media-pipelines.md), [11. Media pipelines](11-media-pipelines.md)).

## 1.3 How media data is handled

SENCA handles media data according to these principles:

- Payloads are assembled once on receipt and then shared by reference without copying. New buffers are created only where
  the format changes (muxing to TS or fMP4, encryption).
- Components inside a node are connected with in-memory channels, never through loopback TCP or an external broker.
- A slow subscriber notices that it fell behind and resumes from the next keyframe without holding anyone else up.
  New subscribers get the most recent GOP first.

## 1.4 Feature map

| Area | What it does | Page |
|---|---|---|
| Ingest | RTMP/RTMPS (Enhanced RTMP: AV1, VP9, H.265), SRT (listener and caller, AES), WHIP | [4. Ingest and playback](04-ingest-playback.md) |
| Playback | HLS (TS, fMP4, LL-HLS), RTMP play, WHEP, RTSP play, signed playback URLs | [4. Ingest and playback](04-ingest-playback.md) |
| Recording | Recording branched from the live path, VOD, MP4 export, schedules, retention, legal hold, crash recovery | [5. Recording](05-recording.md) |
| Inputs and outputs | RTMP/RTSP/SRT/HLS pull, capture devices, RTMP/SRT push, cloud streaming services (PaaS), secrets | [6. Inputs and outputs](06-inputs-outputs.md) |
| Processing | Pipelines (decode, compose, whiteboard correction, audio mixer, encode), production controls, snapshots and thumbnails | [7. Pipelines](07-pipelines.md) |
| Captions | WebVTT HLS subtitles from external speech-recognition and translation services | [8. Captions and translation](08-captions.md) |
| Clustering | Raft control plane, mTLS, ownership with fencing, grid placement and degradation, inter-node relay | [9. Cluster and grid](09-cluster-grid.md) |
| Rooms | SFU, join tokens, browser SDK | [10. Realtime rooms](10-rooms.md) |
| Media pipelines | Export, compose, record, push and transcribe rooms | [11. Media pipelines](11-media-pipelines.md) |
| Security | API tokens and scopes, JWT, signed URLs, sealed secrets, egress restrictions | [12. Security](12-security.md) |
| Operations | Health checks, logs and audit log, SSE, recovery, backups, draining, shutdown | [13. Operations](13-operations.md) |
| Codecs | Hardware and royalty-free codecs, patent policy | [14. Codecs and patents](14-codecs-and-patents.md) |
| API | REST (OpenAPI), SSE, TypeScript and Rust clients | [15. Management API and SDKs](15-api-and-sdks.md) |

The web console (`apps/console`) is served at `/console/` when `--console-dir` is set, and the browser publishing
page (`apps/sender-web`, WHIP) at `/sender/` when `--sender-dir` is set.

## 1.5 Verification status

This guide covers v0.0.1. Progress is tracked in [docs/dev/ROADMAP.md](../../dev/ROADMAP.md) (Japanese), per-standard test
status in the [conformance ledger](../../dev/conformance/ledger.md) (Japanese), and the checks done before a release in
[docs/dev/RELEASE.en.md](../../dev/RELEASE.en.md).

**Interoperability tested against independent implementations**:

- RTMP (FFmpeg 7.0.2); MPEG-TS, fMP4 and HLS (FFmpeg); HTTP/1.1 (curl)
- SRT (libsrt 1.4.4 / 1.5.4); RTP, SDP and RTSP (FFmpeg, mediamtx); STUN and TURN (coturn)
- DTLS (OpenSSL 3.5); SRTP (FFmpeg; AEAD-GCM against test vectors only)
- Royalty-free codecs (bit-exact against FFmpeg's decoders); JPEG and PNG (FFmpeg)
- WebRTC (WHIP, WHEP, rooms, media pipelines, browser SDK) with Chrome
- QUIC v1, HTTP/3, WebTransport and MoQ: aioquic, OpenSSL, Chrome and moq-dev. For the node's `--quic`, HTTP/3 is tested with
  aioquic and Chrome, and MoQ subscriptions with moq-dev's `moq export` and Chrome WebTransport. MoQ publishing is tested with
  SENCA's own client

**Tested only with SENCA's own tests**: Enhanced RTMP, LL-HLS (not yet checked with Apple's validator or Safari),
the management API, JWT, ICE, Raft (deterministic simulation), codec worker fault isolation, egress restrictions,
SBOM and license checks.

**Not verified at present**:

- GPU hardware codecs (no hardware available; only detection without devices is tested) and real capture devices
- PaaS integration (tested only against mock APIs)
- Firefox and Safari; media paths through TURN
- Builds of the Android and Apple sender apps

**Not supported at present**:

- Shared storage for recordings (recordings are stored on the local disk of each node)
- Linearizable reads (ReadIndex); reads come from each node's local state
- AAC encoding and decoding (pass-through only), so AAC audio cannot be sent to WebRTC
