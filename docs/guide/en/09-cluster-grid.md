# 9. Cluster and grid

[日本語](../ja/09-cluster-grid.md)

Several senca-node processes can form one cluster. The control state is replicated, and inputs, outputs,
pipelines and rooms are placed on nodes by the grid. When a node or a device stops, its work moves to a
live node or keeps running in a degraded form.

- Rationale: [ADR-0009 Distributed control plane](../../adr/0009-cluster.md), [ADR-0013 Grid](../../adr/0013-grid.md), [ADR-0014 Relay](../../adr/0014-relay.md)
- Related pages: [3. Configuration](03-configuration.md), [6. Inputs and outputs](06-inputs-outputs.md), [7. Pipelines](07-pipelines.md), [10. Rooms](10-rooms.md), [12. Security](12-security.md), [13. Operations](13-operations.md), [15. API and SDKs](15-api-and-sdks.md)

The grid also runs on a single node, using the same machinery (the node is its own leader and the only
placement target). `placement`, degradation and `GET /api/v1/grid` all work on a single node.

## 9.1 Architecture

| Component | Role |
|---|---|
| Raft (`senca-raft`) | Replicates every change to the control state (commands). 1 to 7 voters |
| Ownership | The node that receives a publisher becomes the owner of the stream (with a fencing token) |
| Relay (ADR-0014) | Viewers on a non-owner node are served from a local "mirror" fed with AUs from the owner |
| Grid (`senca-grid`) | The leader detects failures from heartbeats and, every second, decides placement and commits it to the log. Each node runs only its own share |

**Only the control state is replicated.** Media data (HLS segments, recording files) stays on the disk of the
node that produced it ([5. Recording](05-recording.md)).

## 9.2 Building a cluster

### Configuration file

The cluster configuration is JSON, passed with `--cluster-config FILE` (or written as the `cluster` object of
the `--config` JSON). Unknown fields are a startup error.

```json
{
  "node_id": 1,
  "listen": "10.0.0.1:7000",
  "relay_listen": "10.0.0.1:7001",
  "secret_file": "/etc/senca/cluster-secret",
  "tls_ca": "/etc/senca/cluster-ca.pem",
  "tls_cert": "/etc/senca/n1.pem",
  "tls_key": "/etc/senca/n1.key",
  "peers": [
    {"id": 1, "addr": "10.0.0.1:7000", "relay": "10.0.0.1:7001", "api": "https://n1.example.com"},
    {"id": 2, "addr": "10.0.0.2:7000", "relay": "10.0.0.2:7001", "api": "https://n2.example.com"},
    {"id": 3, "addr": "10.0.0.3:7000", "relay": "10.0.0.3:7001", "api": "https://n3.example.com"}
  ]
}
```

| Field | Required | Meaning |
|---|---|---|
| `node_id` | yes | This node's Raft ID. At least 1, and `peers` must contain the same `id` |
| `listen` | yes | Listener for node-to-node traffic (Raft, forwarding, heartbeats) |
| `peers` | yes | The voting nodes, including this one. 1 to 7 |
| `peers[].id` | yes | Raft ID |
| `peers[].addr` | yes | That node's `listen` address |
| `peers[].api` | no | That node's management API URL. Used for 307 redirects, room `redirect` messages and forwarding of room REST calls (see below) |
| `peers[].relay` | no | That node's relay listener. Defaults to the port of `addr` + 1 |
| `secret_file` | yes | File holding the cluster secret. Same content on every node; at least 16 characters after trimming whitespace |
| `tls_ca`, `tls_cert`, `tls_key` | no | Node-to-node mTLS. Give all three or none (a partial set is an error) |
| `relay_listen` | no | This node's relay listener. Defaults to the port of `listen` + 1 |

The node refuses to start when:

- `node_id` is 0 or not present in `peers`;
- `peers` has 0 or more than 7 entries;
- the secret is shorter than 16 characters;
- only some of the `tls_*` fields are set.

Notes:

- **Every node is a voter.** Non-voting, media-only nodes cannot be configured yet, so a cluster has at most 7 nodes.
- **Membership is fixed by the configuration file.** There is no API to add or remove voters at run time. A new
  cluster is created when all voters start for the first time with the same `peers` (an empty `control/raft/`
  in the data directory).
- Set `peers[].api` to a URL **reachable both from clients and from the other nodes**. It is recorded when the
  node first registers; changing it later does not update the registered value. Without it, 307 redirects and
  room redirects to that node are not possible.
- Turning an existing single-node data directory into a cluster member does not carry its control state over:
  in a cluster only the Raft log is used, and the single-node log `control/log` is not read.

### Ports

| Port | Peer | Purpose |
|---|---|---|
| `listen` | other nodes only | Raft, proposal forwarding, grid heartbeats |
| `relay_listen` (default `listen` + 1) | other nodes only | Relay (AU transport) |
| `--http`, `--https` | clients and other nodes | Management API, HLS, signaling. Target of `peers[].api` |
| `--rtmp`, `--srt`, `--rtsp`, `--webrtc-udp`, ... | clients | Publishing and playback ([4. Ingest and playback](04-ingest-playback.md)) |

### Cluster secret and mTLS

Every node-to-node connection proves knowledge of the cluster secret in its first frame (its hash, compared in
constant time). With `tls_*` set, the connection runs over mTLS: both sides verify certificates issued by the
cluster CA (`tls_ca`), and each certificate must carry **`senca-cluster`** as a DNS SAN. Without `tls_*` the
traffic is plain TCP; use that only for tests on a closed network ([12. Security](12-security.md)). Relay
connections use the same certificates (without certificates they are plain TCP authenticated with a value
derived from the secret).

Example, following `tools/interop/cluster.sh` (P-256; node certificates have both serverAuth and clientAuth):

```sh
# Cluster CA
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout ca.key -out ca.pem -days 365 -subj "/CN=senca-cluster-ca"
# One per node (n1, n2, n3)
for n in n1 n2 n3; do
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout $n.key -out $n.csr -subj "/CN=$n"
  printf "subjectAltName=DNS:senca-cluster\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth,clientAuth\n" > $n.ext
  openssl x509 -req -in $n.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
    -out $n.pem -days 365 -extfile $n.ext
done
# Cluster secret (same file on every node)
openssl rand -hex 32 > cluster-secret
```

A node presenting a certificate from another CA is refused even if it knows the secret (the log shows
`mTLS from ... refused`).

### Starting

```sh
senca-node --data-dir /var/lib/senca --node-name n1 \
  --http 0.0.0.0:8080 --public-http-url https://n1.example.com \
  --cluster-config /etc/senca/cluster-n1.json \
  --label zone=a --net-mbps 10000
```

- `--node-name` is the node's name. Placement and the API accept it to refer to the node. It is separate from
  the node ID (a 128-bit ID stored in `data_dir/node-id`).
- After a leader is elected, each node registers itself. Only the node with the smallest ID (`id: 1` in the
  example) creates the playback URL key and the first admin token. The token is written to that node's
  `data_dir/control/bootstrap-admin-token` (mode 0600) and works on every node (it is replicated).
- `/readyz` does not return 200 until this initialization has completed.

### Writes and reads

- **Send API calls to any node.** Writes are forwarded to the leader, and after consensus they are applied in the
  same order on every node.
- A write is first tried against the local state, so obvious errors (400, 404, 409 and so on) come back
  immediately. The final result is decided when the committed command is applied.
- When there is no known leader, no majority is reachable, or the command is not applied within 5 seconds, the
  response is **503 `CAPACITY_UNAVAILABLE` (`retryable: true`)**. The minority side of a split cannot write.
- **Reads are served from the node's local state** (the leader is not consulted). Right after a write, a read
  may be a few milliseconds stale. A node that has lost the majority can still read.
- Raft timing: 100 ms tick, election timeout 1 to 2 seconds (random, 10 to 20 ticks), snapshot every 1000
  entries. The log in `data_dir/control/raft/` is fsynced before messages are sent and entries applied. If
  storage fails, the node exits (exit code 70).

### What is replicated

All of `ControlState`: streams, API tokens (hashes), recording records (metadata), schedules, Idempotency-Key
records, node registrations (`GET /nodes`), inputs and outputs, secrets (encrypted values), stream owners, the
playback URL key, the grid state (node specs, workloads, assignments, unschedulable reasons), rooms and media
pipelines.

Not replicated: media data (live segments, recording files), grid heartbeats, room participants and room
events (they live on the node hosting the room).

## 9.3 Stream ownership, and serving viewers on any node

- The node where a publisher (RTMP, SRT, WHIP, ..., or a pull input) connects proposes `ClaimStream`. Applying
  it issues a new fencing token and makes that node the owner (SSE `stream.owner`).
- Every other node watches for this and disconnects any local publisher of the same stream (log:
  `publisher evicted`). A stream is never published on two nodes at once; the most recent connection wins.
- Publishers may connect to any node. Publishing again elsewhere moves ownership to that node.

### Playback: relay mirrors

When playback reaches a non-owner node (signed HLS; RTMP, SRT or RTSP play; WHEP; a pipeline input), that node
receives AUs from the owner over the relay and serves them from a local **mirror** of the stream.

- However many viewers watch a stream on a node, only one copy crosses between the nodes. The mirror's packager
  and GOP cache behave exactly as on the owner.
- Only encoded AUs are carried (no transcoding).
- Mirrors never record, caption or claim ownership (that is the owner's job).
- A freshly created mirror waits up to 6 seconds for its first playlist.
- A mirror unused for 30 seconds is torn down. When ownership moves to this node, the mirror stops and yields
  to the real publisher.
- When ownership moves to another node, the mirror re-subscribes to the new owner; the HLS playlist gets a
  discontinuity at that point.

### 307 redirect (fallback)

- Live: if no mirror can be created, or it produces no playlist within 6 seconds, and there is no local
  playlist, the node answers **307** to the same path (`/s/...`) on the owner's `peers[].api`.
- Recordings (VOD): recording files live on the node that recorded them. If the file is not local, the node
  answers 307 to that node.
- The redirect target is the registered `peers[].api`. Without it, there is no redirect (the request fails, e.g.
  404).

Relay status appears in `GET /api/v1/nodes` under `relay` of the node itself (`"self": true`): `peers[]`
(`node`, `rtt_ms`, `tx_bps`, `rx_bps`, `tx_bytes`, `rx_bytes`, `tx_aus`, `rx_aus`, `overhead_per_au`,
`upstreams`, `downstreams`, `going_away`) and `mirrors` (the stream IDs being mirrored).

## 9.4 The grid

### Node registration and capacity

Each node reports the resources it lends to workloads when it starts (and re-checks every 30 heartbeats,
re-registering if anything changed).

| Resource | Default | Override |
|---|---|---|
| CPU (millicores) | logical CPUs × 1000 × 90% | `--capacity-cpu-millis N` |
| Memory (MB) | 85% of MemTotal in `/proc/meminfo` (4096 MB is assumed if unreadable) | `--capacity-memory-mb N` |
| Bandwidth (Mbit/s, each direction) | 10000 | `--net-mbps N` |
| GPUs | Hardware encoders/decoders found by the codec worker, one entry per device (IDs like `nvidia:0`, `intel-vpl:0`, `amd-amf:0`, `vaapi:0`). Session limits are taken from the device when known; otherwise 8 encode and 16 decode sessions | — |
| Capture devices | Devices found by the capture worker | — |
| Labels | `--label KEY=VALUE` (repeatable). `senca.io/name` is set automatically to `--node-name` | `--label` |

- `--label` keys and values are 1 to 128 characters. Keys starting with `senca.io/` are reserved (rejected on
  the command line).
- In the `--config` file, the same settings are `net_mbps`, `labels`, `capacity_cpu_millis` and
  `capacity_memory_mb`.
- Node roles are `media` (runs workloads) and `control` (Raft voter). Currently every node has both.
- **Placement counts reservations.** A reservation is the sum of per-workload estimates (9.5), not measured
  usage. Heartbeats have a usage field, but the node currently sends zeros.

### Heartbeats and failure detection

- Every node sends a heartbeat to the leader **every second** (over the cluster connection; heartbeats are not
  written to the log). It lists the streams the node is receiving, the workloads it runs, and device health.
  The leader accepts a heartbeat only when the sender's Raft ID matches the `raft_id` of the node's registration.
- Every second, the leader marks a node whose last heartbeat is **more than 5 seconds** old as down by
  committing `NodeDown`, and commits `NodeUp` when it comes back (SSE `node.down`, `node.up`).
- When leadership changes, the detector is rebuilt. For 5 seconds after that, nodes that have not yet sent a
  heartbeat are assumed to be alive.
- Device health: the node currently always reports its registered devices as healthy. There is no automatic
  path that detects a device failure and emits `device.down` yet, so disable a broken GPU or capture device
  manually (9.7). If a device disappears from the node's report, re-registration (every 30 heartbeats)
  causes its work to be re-placed.
- Placement is planned in the round after a down notice is applied. When a workload is added, removed or
  changed, planning runs immediately instead of waiting for the next round.

### Workloads

Grid workloads are derived from resources automatically; users do not create them. A workload ID is the
resource kind plus its ID: `input:<id>`, `output:<id>`, `pipeline:<id>`, `room:<id>`.

| Resource | Becomes a workload when | Kind |
|---|---|---|
| Input (pull) | `enabled: true` | `ingest` (`capture` for `protocol: "device"`) |
| Output (push) | `enabled: true` | `egress` |
| Pipeline | `desired_state: "running"` | `pipeline` |
| Room | always | `room` |

Not workloads: push ingest from publishers (received wherever the publisher connects) and recordings (made on
the owner node). A room's media pipeline export is added to the room's estimate, and compositing becomes a
pipeline pinned to the room's node ([11. Media pipelines](11-media-pipelines.md)).

Each node runs only the assignments addressed to it. When an assignment moves to another node its token
changes, and the old node stops the workload as soon as it applies that change.

### Placement spec (`placement`)

`placement` can be set when creating or PATCHing an input, output, pipeline or room. The result appears in
the resource's `placement_status`.

```json
{
  "node": "n2",
  "device": "nvidia:0",
  "labels": [
    {"op": "eq", "key": "zone", "value": "a"},
    {"op": "in", "key": "rack", "values": ["r1", "r2"]},
    {"op": "not_in", "key": "tier", "values": ["spot"]}
  ],
  "anti_affinity": "camera-1",
  "pin_policy": "prefer",
  "priority": "high",
  "strategy": "spread",
  "degrade": [
    {"label": "no_av1", "disable": ["e3"]},
    {"label": "single", "disable": ["sc"]}
  ]
}
```

| Field | Default | Meaning |
|---|---|---|
| `node` | none | Manual assignment. Node ID or name (`--node-name`); stored as the ID. An unknown node is 400 |
| `device` | none | Manual assignment to a GPU ID. Without `node`, any node that has that GPU |
| `labels` | none | Label selectors; all must match. Up to 16 |
| `anti_affinity` | none | Group name. Workloads in the same group are never placed on the same node |
| `pin_policy` | `prefer` | What happens when the pinned target is down. `prefer` moves the work elsewhere and moves it back after the target returns and the cooldown has passed; `strict` leaves it unscheduled |
| `priority` | `normal` | `low` < `normal` < `high` < `critical` |
| `strategy` | `spread` | `spread` picks the node with the most room left after placing; `binpack` packs |
| `degrade` | none | Degradation levels (**pipelines only**; 400 on other resources). Up to 8 |

Label selectors:

| `op` | Shape | Matching nodes |
|---|---|---|
| `eq` | `{"op":"eq","key":K,"value":V}` | label K equals V |
| `in` | `{"op":"in","key":K,"values":[...]}` | label K is one of the values (nodes without K do not match) |
| `not_in` | `{"op":"not_in","key":K,"values":[...]}` | label K is none of the values (nodes without K match) |

Validation (violations are 400 `INVALID_CONFIG`): `node`, `device`, `anti_affinity`, and label keys and values
are 1 to 128 characters with no control characters. `values` of `in` / `not_in` has 1 to 32 entries. Unknown
fields are 400.

`degrade` levels:

- `label` is 1 to 128 characters. `full` (the name of level 0) is not allowed, and labels must be unique.
- `disable` must not be empty and may only name node IDs that exist in the graph.
- Levels are **cumulative** (level 2 also disables what level 1 disabled). Downstream nodes stop when all of
  their inputs are stopped. Nodes that lose only some inputs (multi-input sinks, compositors, audio mixers)
  keep running with those inputs removed.

Manual assignment (pin) behavior:

- While the target (node or GPU) is alive, both `strict` and `prefer` place the work only there (not newly,
  while the target is draining).
- When the target goes down, `strict` leaves the work unscheduled (reason `pin_target_down`) and `prefer`
  moves it to another node.
- Work moved elsewhere under `prefer` returns once the target is back and 30 seconds (the cooldown) have passed
  since it moved (reason `pin_returned`).

### Scoring

When placing into free capacity, the candidate with the highest score wins (ties go to the smaller node ID).

- `spread`: the fraction of capacity left after placing (per mille; the minimum over CPU, memory, bandwidth and
  GPU sessions). `binpack` uses 1000 minus that value.
- Data locality: +10000 for a node that is receiving the workload's input stream (for an output, its stream;
  for a pipeline, the streams of its `source` nodes).

## 9.5 Estimates per workload

Reservations use the fixed estimates in `crates/senca-control/src/workloads.rs`. CPU in millicores, bandwidth
in Mbit/s.

| Workload | CPU | Memory | Inbound | Outbound | Other |
|---|---|---|---|---|---|
| Input (pull) | 150 | 64 MB | 10 | 0 | — |
| Input (`protocol: "device"`) | 500 | 64 MB | 0 | 0 | Takes the capture device exclusively. One GPU encode session if `options.codec` is H.264/H.265 |
| Output | 100 | 64 MB | 0 | 10 | Prefers the node receiving its stream |
| Room (participant limit N) | 200 + 20N | 64 + 2N MB | 2N | 2N × min(N, 8) | Each running media pipeline adds 20N CPU and 8N MB |
| Pipeline | sum below | sum below | 10 | 10 | All GPU sessions go on the same device |

N for a room is `settings.max_participants` (50 if unset, clamped to 1..10000).

Pipeline estimate (a base of 200 CPU and 128 MB, plus per node):

| Node | CPU | Memory | GPU |
|---|---|---|---|
| `encode` (H.264, H.265) | +100 | +128 MB | +1 encode |
| `encode` (Opus) | +50 | +128 MB | — |
| `encode` (AV1, VP8, VP9 and other software video) | +1500 | +128 MB | — |
| `decode` (input `source` codec is H.264/H.265) | +100 | +128 MB | +1 decode |
| `decode` (other) | +400 | +128 MB | — |
| `compose`, `board`, `scale`, `crop` | +300 | +64 MB | — |
| `audio_mix` | +50 | — | — |

Example: a graph that decodes VP9 and encodes two AV1 outputs, one through `scale`, is
200 + 400 + 1500 × 2 + 300 = 3900 millicores. A level that disables `scale` (and therefore the encoder after
it) needs 2100 millicores (`tools/interop/grid_single.sh`).

H.264 and H.265 have no software implementation ([14. Codecs and patents](14-codecs-and-patents.md)), so a
pipeline using them cannot be placed on a node without a GPU (reason `no_device`).

## 9.6 Degradation, eviction and restore

The scheduler runs on the leader every second and always produces the same decisions from the same input
(`crates/senca-grid/src/plan.rs`).

1. **Check existing assignments**: anything no longer valid because of a node failure, device failure,
   constraint change or anti-affinity is queued for re-placement. Valid assignments are left alone.
2. **Repair overcommit**: if a node's capacity shrank below its reservations, lower-priority work is degraded
   first, then evicted (reason `overcommitted`).
3. **By priority, highest first** (critical → high → normal → low), place queued and new work. At each level
   (starting from full) try, in order:
   1. free capacity;
   2. degrading **lower**-priority work;
   3. evicting **lower**-priority work;
   4. otherwise try the workload's own next (degraded) level.
   Among degrade/evict options, the one with the fewest evictions, then the lowest sum of evicted priorities,
   then the fewest level drops wins. **Work of equal or higher priority is never displaced** (the workload
   becomes `unschedulable` with reason `contended`).
4. Degraded work goes back to a better level once **30 seconds (the cooldown)** have passed since its level last
   changed (reason `capacity_returned`): free capacity on the same node, then moving to free capacity on another
   node, then degrading or evicting lower-priority work on the same node.
5. **Voluntary moves**: work on draining nodes, and `prefer`-pinned work whose target is back, is moved (at most
   64 moves per plan).

`PlanConfig` defaults: `restore_cooldown_ms` 30000, `restore_headroom_permille` 0 (no extra headroom required
when restoring), `max_voluntary_moves` 64, `locality_bonus` 10000. These are not configurable.

Work is only degraded or evicted for higher-priority work, or because of node/device failure or reduced
capacity. It only moves to a better level after the cooldown (to avoid flapping).

A degraded pipeline is restarted with the nodes disabled at that level removed from its graph (log:
`pipeline <id>: running degraded (level N, without [...])`). Workloads without degradation levels (inputs,
outputs, rooms) are simply left unscheduled or evicted when capacity runs out.

### `placement_status`

```json
{"state": "degraded", "node": "<node-id>", "node_name": "n2", "device": null,
 "level": 1, "level_label": "single", "since": "2026-11-03T10:00:00Z",
 "reason": {"code": "preempted", "by": "pipeline:..."}}
```

| `state` | Meaning | Other fields |
|---|---|---|
| `running` | Running at full level | `node`, `node_name`, `device`, `level`, `level_label`, `since`, `reason` |
| `degraded` | Running at a degraded level | same |
| `pending` | Waiting for placement | `reasons` (null) |
| `unschedulable` | Cannot be placed on any node | `reasons` |
| `not_scheduled` | No workload (disabled input/output, stopped pipeline) | — |

`reason.code`: `placed`, `no_capacity`, `node_down` (`node`), `device_down` (`device`), `draining`,
`constraint_changed`, `preempted` (`by`), `overcommitted`, `capacity_returned`, `pin_returned`,
`pin_target_down`.

`reasons` counts nodes per rejection reason, with up to 8 example node IDs:
`[{"reason": "cpu", "nodes": 2, "examples": ["<node-id>", ...]}]`. Reasons: `node_down`, `not_media`,
`draining`, `pinned_elsewhere`, `pin_target_down`, `labels`, `anti_affinity`, `no_device`, `capture`, `cpu`,
`memory`, `net_in`, `net_out`, `gpu_encode`, `gpu_decode`, `gpu_memory`, `contended` (it would fit alone, but
the node is full of equal- or higher-priority work).

## 9.7 Operations

All of these require the `node:admin` scope (not included in `tenant:admin`).

| Operation | API | Response |
|---|---|---|
| List / get nodes | `GET /api/v1/nodes`, `GET /api/v1/nodes/{id}` | 200 |
| Drain | `POST /api/v1/nodes/{id}/drain` | 202 |
| Undrain | `POST /api/v1/nodes/{id}/undrain` | 202 |
| Disable a device | `POST /api/v1/nodes/{node}/devices/{device}/disable` | 202 |
| Enable a device | `POST /api/v1/nodes/{node}/devices/{device}/enable` | 202 |
| Grid overview | `GET /api/v1/grid` | 200 |

- **Drain**: no new work is placed on the node, and its current work is moved to other nodes with free capacity
  (reason `draining`; up to 64 moves per plan). Work pinned to the node stays. `{id}` must be the node ID (names
  are not accepted here). Undraining does not move work back automatically (except `prefer` pins).
- **Device disable**: takes a GPU or capture device out of service independently of measured health. Its work
  moves to another device or degrades. Neither heartbeats nor re-registration undo it; only enable does.
  `{node}` may be the ID or the name. An unknown device is 404. Response:
  `{"node": "<id>", "device": "nvidia:0", "enabled": false}`.
- There is no API to remove a node from the grid. A node that stays down remains listed with `up: false`.

### `GET /api/v1/grid`

```json
{
  "scheduler": "<node-id> or null",
  "nodes": [{
    "id": "<node-id>", "name": "n1", "up": true, "draining": false,
    "roles": ["control", "media"], "labels": {"senca.io/name": "n1", "zone": "a"},
    "capacity": {"cpu_millis": 7200, "memory_mb": 13000, "net_in_mbps": 10000, "net_out_mbps": 10000},
    "reserved": {"cpu_millis": 350, "memory_mb": 192, "net_in_mbps": 20, "net_out_mbps": 10},
    "gpus": [{"id": "nvidia:0", "vendor": "nvidia", "healthy": true, "enabled": true,
              "encode_sessions": 8, "decode_sessions": 16, "encode_reserved": 1, "decode_reserved": 0}],
    "capture": [{"id": "...", "kind": "...", "healthy": true}],
    "last_heartbeat": "2026-11-03T10:00:00Z",
    "workloads": 3
  }],
  "workloads": [{
    "id": "input:...", "kind": "ingest", "priority": "high", "pinned": true,
    "state": "running", "node": "<node-id>", "device": null, "level": 0, "level_label": "full",
    "reason": {"code": "placed"}, "unschedulable": null
  }]
}
```

- `scheduler` is the node's own ID if the queried node is the leader (scheduler), otherwise null.
- `last_heartbeat` is known only to the leader; other nodes return null.
- `reserved` is the sum of the estimates of the assigned work, not measured usage.
- `workloads[].state` is `running`, `degraded`, `unschedulable` or `pending`.
- `disabled` appears in `capture[]` only for manually disabled devices.

### SSE events

These are delivered on `GET /api/v1/events` ([15. API and SDKs](15-api-and-sdks.md)). Grid and node events are
outside any tenant, so only tokens with `node:admin` see them.

| Event | `resource_id` | `data` |
|---|---|---|
| `node.updated` | node ID | `{"draining": bool}` (registration, drain) |
| `node.down`, `node.up` | node ID | `{}` |
| `device.down`, `device.up` | node ID | `{"device": ...}` (measured health change) |
| `device.disabled`, `device.enabled` | node ID | `{"device": ...}` (manual action) |
| `grid.decisions` | empty | `{"decisions": [...]}` |
| `stream.owner` | stream ID | `{"node": "<node-id>"}` |

Each element of `grid.decisions` is distinguished by `type`: `assign` (`workload`, `node`, `device`, `level`,
`token`, `reason`), `move` (plus `from`), `degrade` and `restore` (`workload`, `level`, `device`, `reason`),
`evict` (`workload`, `reason`), `unschedulable` (`workload`, `reasons`).

## 9.8 Examples

Place a pull input on n3, moving it elsewhere if n3 goes down:

```sh
curl -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  https://n1.example.com/api/v1/streams/$SID/inputs \
  -d '{"protocol":"rtmp_pull","url":"rtmp://src.example.com/live/key",
       "placement":{"node":"n3","pin_policy":"prefer","priority":"high"}}'
curl -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/inputs/$IID | jq .placement_status
```

Two camera inputs on nodes in zone a or b, never on the same node:

```json
{"protocol":"srt_pull","url":"srt://cam1.example.com:9000",
 "placement":{"labels":[{"op":"in","key":"zone","values":["a","b"]}],"anti_affinity":"cameras"}}
```

A pipeline that degrades when capacity is short (the `placement` part):

```json
{"priority":"normal","degrade":[{"label":"single","disable":["sc"]}]}
```

Put a node into maintenance:

```sh
NID=$(curl -s -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/grid \
  | jq -r '.nodes[] | select(.name=="n2") | .id')
curl -X POST -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/nodes/$NID/drain
# after maintenance
curl -X POST -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/nodes/$NID/undrain
```

Disable and re-enable a GPU:

```sh
curl -X POST -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/nodes/n2/devices/nvidia:0/disable
curl -X POST -H "Authorization: Bearer $TOKEN" https://n1.example.com/api/v1/nodes/n2/devices/nvidia:0/enable
```

## 9.9 Failure scenarios and timings

Timings follow from the constants above (1 s heartbeat, 5 s down threshold, 1 to 2 s election, 30 s cooldown).

| Scenario | What happens | Expected time |
|---|---|---|
| A non-leader node stops | After more than 5 s without heartbeats, `node.down`. The next round moves its work (`move`, reason `node_down`) and the target node starts it. `strict` pins are not moved | About 6 to 9 s from the stop to the move (measured in `tools/interop/cluster.sh`) |
| The leader stops | Writes return 503 (retryable) during the election. The new leader rebuilds the detector and marks the stopped node down after the 5 s grace period | About 1 to 2 s election + 5 s grace + 1 s planning (estimated from the constants) |
| The majority is lost | Writes return 503 (after waiting up to 5 s). Reads are served from local state. Running work keeps running, but nothing is re-placed | Writes resume as soon as the majority is back |
| A stopped node returns | `node.up` in the next round. The node catches up on the log and reconciles its work. `prefer` pins return once 30 s have passed since they moved; degraded work is restored after the cooldown | About 1 s to `node.up`; restores after the 30 s cooldown |
| The owner node of a stream stops | The stream is interrupted. When the publisher reconnects to any node, that node becomes the new owner and mirrors re-subscribe to it | Depends on the publisher's reconnect |
| The node hosting a room stops | The room moves to another node. Participants rejoin (any node they connect to redirects them to the new host; see [10. Rooms](10-rooms.md)) | Same as a node stop |
| Network partition | A node on the minority side does not learn about new assignments, so it may keep running work that the majority side has moved until the partition heals. It then sees the new token and stops | For the duration of the partition |

`tools/interop/cluster.sh` checks, on three nodes: writes through any node, 409 on duplicate names, writes with
one node stopped, catch-up of a returning node, 503 without a majority while reads still work, convergence of
all nodes, HLS and RTMP playback through relay mirrors, eviction of the old publisher when ownership moves,
moving and returning (after the cooldown) a `prefer` pin, not moving a `strict` pin, and redirecting to and
moving a room. With `CLUSTER_TLS=1` it uses mTLS and checks that a node with a certificate from another CA is
refused. `tools/interop/grid_single.sh` checks, on one node, priority-based degradation, unschedulable reasons
and restoring after the cooldown.
