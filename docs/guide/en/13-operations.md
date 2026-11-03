# 13. Operations

Health checks, logging, monitoring, event-driven automation, crash recovery, backups, upgrades, capacity planning,
draining a node and shutting it down.

- Related pages: [3. Configuration](03-configuration.md), [5. Recording](05-recording.md), [9. Cluster and grid](09-cluster-grid.md), [12. Security](12-security.md), [15. Management API and SDKs](15-api-and-sdks.md)
- Rationale: [ADR-0005 Recording format and index](../../dev/adr/0005-recording.md), [ADR-0009 Distributed control plane](../../dev/adr/0009-cluster.md), [ADR-0013 Grid](../../dev/adr/0013-grid.md)

## 13.1 Startup and exit codes

| Exit code | Meaning |
|---|---|
| 0 | Clean shutdown (after SIGTERM or SIGINT) |
| 1 | Failure during startup (`fatal: ...`: unreadable file, listener cannot be bound, control log corrupted in the middle, cluster secret too short, ...) |
| 2 | Configuration error (unknown option, value fails validation) |
| 70 | Raft storage failed in a cluster (the node stops rather than run without persistence) |

## 13.2 Health checks

| Path | Auth | Response |
|---|---|---|
| `GET /healthz` | none | Always `200 ok` while HTTP is being served |
| `GET /readyz` | none | `200 ready` when ready, otherwise `503` |

- A single node becomes ready once startup (loading the control state, recording recovery, opening listeners) is done.
- In a cluster, a node becomes ready once a leader is known and its own registration (and, on the first node,
  cluster initialization) has been committed.
- After a shutdown signal, `/readyz` returns 503.
- **In a cluster, readiness drops when contact with a quorum is lost.** When more than **4 seconds** have passed since
  the leader last heard from a quorum of voters (or a follower last heard from the leader), the node stops its grid
  work (pull inputs, push outputs, pipelines; fencing, log `grid: fenced`) and `/readyz` returns 503 (write APIs return
  503 too). Once contact is back it returns 200 again and resumes the work assigned to it (log
  `quorum contact restored`). Right after startup it is also 503 until the leader has been reached.
- A single node stays ready until shutdown once ready. Draining ([13.9](#139-draining-a-node)) does not affect
  readiness.

For containers without curl there is a built-in probe:

```sh
senca-node --healthcheck http://127.0.0.1:8080/readyz
```

- It must be the **first argument** (do not mix it with other options). Without a URL it checks
  `http://127.0.0.1:8080/readyz`.
- Plain `http://` only. Exit code 0 if the response starts with `HTTP/1.1 200`, 1 otherwise, 2 for a malformed URL.
- The Docker image's `HEALTHCHECK` runs it every 10 s.

## 13.3 Logging

### Node log

- Default: stderr, `RFC 3339 timestamp + space + message`.
- `--log-json`: stdout, one JSON object per line (`{"ts": "...", "node": "<node ID>", "msg": "..."}`).

```
{"msg":"senca-node 0.0.0 started (node f1be5f27675d35f8dd72b88676be8754)","node":"f1be5f27675d35f8dd72b88676be8754","ts":"2026-11-03T01:11:38.915Z"}
```

With `--log-json`, every line of the node goes to stdout as JSON: the "created the first admin token" line,
`codec worker unavailable: ...`, `fatal: ...` (startup failures; `node` is null before the node ID is known),
configuration errors, and the stderr lines of the codec and capture workers (the node reads them line by line and
puts each into `msg`; at most 4096 bytes per line). Without `--log-json`, worker lines still go to stderr as before.

Things that are logged: listeners starting, publishers attaching (`publisher attached`) and leaving, recording
failures, recovery (`recovery: ...`), retention deletes (`retention: deleting recording ...`), authentication failures
with their reason and request ID, input/output reconnect errors, and cluster and grid events
(`grid: node ... is down`, ...). Stream keys and token values are never logged.

### Audit log

State-changing API calls are appended to `data_dir/audit.log`, one JSON object per line (format in
[12. Security](12-security.md#1212-audit-log)). The node reopens the file for every write, so rotating it with `mv`
(logrotate's `create` mode) is safe. The node never rotates or truncates it itself. If it cannot write, the node logs
`audit log write failed: ...`; the API call still succeeds.

## 13.4 Monitoring APIs

All require `node:admin` (`capabilities` only requires authentication).

| Path | Contents |
|---|---|
| `GET /api/v1/nodes` | Registered nodes: `id`, `name`, `draining`, `registered_at`, `labels`, `live_streams` (counted only for the node answering), `self`, `relay` (that node's relay status) |
| `GET /api/v1/nodes/{id}` | A single node |
| `GET /api/v1/grid` | Grid overview: `scheduler` (the node doing placement; filled in only when asking the leader); per node `up`, `draining`, `roles`, `labels`, `capacity`, `reserved`, `gpus` (sessions and reservations), `capture`, `usage` (measured usage from heartbeats), `last_heartbeat`, `workloads`; per workload `state` (`running`, `degraded`, `pending`, `unschedulable`), `node`, `device`, `level`, `reason` |
| `GET /api/v1/devices` | This node's codec providers (available or not, and why), sandbox state and capture devices |
| `GET /api/v1/capabilities` | This node's ingest, output and codec features (`codecs.transcode` lists what was actually loaded) |

Per stream, watch `observed_state` (`live`, `waiting_input`, `stopped`) and `live` (start time, bitrate, tracks) in
`GET /api/v1/streams/{id}`; for inputs and outputs, their `status` (`state`, `last_error`, connect count).

There is no Prometheus-style metrics endpoint yet; poll the APIs above.

## 13.5 Automation with events (SSE)

`GET /api/v1/events` (`stream:read`) streams committed changes as Server-Sent Events.

```sh
curl -sN -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/events
```

```
event: node.updated
id: 10
data: {"data":{"draining":true},"occurred_at":"2026-11-03T01:12:07.603Z","resource_id":"f1be5f27675d35f8dd72b88676be8754","type":"node.updated"}
```

- `id` is the position in the control log. You only receive events for your own tenant. Node, device and grid
  events (`node.*`, `device.*`, `grid.decisions`, `stream.owner`) require `node:admin`.
- A keep-alive is sent every 15 s.
- When you reconnect with `Last-Event-ID` and that position is older than the node's current one, you immediately
  get a `resync` event; past events are not replayed. A client that reads too slowly and falls behind also gets
  `resync`, after which the connection is closed.
- Room events (`room.started`, `room.connection.created`, ...) have no `id` and are not replayed if missed.

A robust pattern: on connect and on every `resync`, re-list the resources you care about via REST, then follow
the events for changes. The event catalog is in [15. Management API and SDKs](15-api-and-sdks.md).

## 13.6 Recovery after a crash

Recordings are recovered on every start (`recovery.rs`):

1. Deletions that were in progress are completed.
2. Each `recordings/*/index.jsonl` is read. An index without an `end` line (an unfinished recording) has any
   half-written trailing line truncated and gets an `end` line with `state: "interrupted"` (end time = end of the
   last segment). The recording in the control state becomes `interrupted` too. Log:
   `recovery: recording <ID> closed as interrupted (<N> segments)`.
3. Recordings of this node that are still `recording` in the control state after that (for example because they have no index) become `interrupted`.
4. Segments referenced by no index are moved to `segments/{stream}/orphans/{run}/` (never deleted). If any index
   cannot be parsed, this quarantine step is skipped (to avoid losing references); unreadable indexes themselves are
   left untouched.
5. With `--verify-recordings`, the SHA-256 of every indexed segment is checked and mismatches are logged.

Segments recorded before the crash remain playable as VOD and exportable from an `interrupted` recording.

The single-node control log (`control/log`) is read with a half-written trailing record truncated. A corrupted
record in the middle prevents startup; nothing is discarded silently.

Verified: killing the node with SIGKILL during a recording and restarting it left the recording `interrupted` with
its three segments intact; stopping with SIGTERM left it `finalized`.

## 13.7 Backups

| What | Importance | Contents |
|---|---|---|
| `control/` (single node: `log`, `snapshot.json`) | required | The whole control state (streams, tokens, recording metadata, schedules, inputs, outputs, secrets, playback key) |
| `control/secret-key` | required (single node) | Key that opens stored secrets. Without it, stored secrets cannot be recovered. In a cluster, secrets are sealed with a cluster-wide key replicated in the control state, wrapped with the cluster secret; keep the cluster secret (`secret_file`) safe separately ([12. Security](12-security.md)) |
| `recordings/` and `segments/` | required if recordings matter | Back up the index and the segments **together** (indexes reference segments) |
| `snapshots/` | optional | Periodic snapshots |
| `node-id` | recommended | The node's ID; a new ID registers as a different node |
| `audit.log` | per your policy | Audit log |

- For a consistent copy, stop the node first or use a filesystem snapshot (LVM, ZFS, btrfs, ...). Copying `control/`
  from a running node can race with snapshot rotation (writing `snapshot.json`, then emptying the log) and miss changes.
- To restore, stop the node, put the data directory back as a whole and start it. Unfinished recordings are closed
  by startup recovery.
- Backups contain the playback key, sealed secrets and token hashes. Protect them like the data directory
  ([12. Security](12-security.md#128-protecting-the-data-directory)).
- Clusters: the control state is replicated to every node, but each recording exists only on the disk of the node
  that recorded it, so back up recordings on every node. Never put an old copy of Raft storage (`control/raft/`)
  back on one node of a running cluster: Raft assumes a voter never loses state it has persisted. Restoring a cluster
  as a whole is covered in [9. Cluster and grid](09-cluster-grid.md).

## 13.8 Upgrades

SENCA is v0.0.0 and makes no promise of data compatibility between versions. What is known about the current formats:

- The control log stores commands as JSON tagged with an `op` name. Many control-state fields fall back to defaults
  when missing, but an older binary cannot read an `op` it does not know and will not start
  (`control log record N: ...`). **Assume downgrades are impossible.**
- A recording index carries `schema` (currently 1) on its `begin` line. Recovery leaves indexes with an unknown
  schema untouched (`left untouched`) and then also skips orphan quarantine.
- `senca-node` and `senca-codec-worker` talk a versioned protocol. **Always upgrade both together.**
- Running nodes of different versions in one cluster has not been tested.

Procedure for a single node:

1. Take a backup ([13.7](#137-backups)).
2. Stop the node with SIGTERM (recordings are closed as `finalized`).
3. Replace `senca-node`, `senca-codec-worker` (and `senca-capture-worker` if you use it).
4. Start it and check `/readyz` and the log (`recovery: ...`, `fatal: ...`).

## 13.9 Draining a node

```sh
curl -s -X POST -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/nodes/$NODE/drain     # 202
curl -s -X POST -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/nodes/$NODE/undrain   # 202
```

- Requires `node:admin`. Sets the node's `draining` flag and emits `node.updated`.
- The grid stops placing new work (pull inputs, push outputs, pipelines, rooms, media pipelines) on the node and moves
  the work already there to other nodes where it can ([9. Cluster and grid](09-cluster-grid.md)).
- **A draining node refuses new publishers.** An RTMP publish gets `NetStream.Publish.Rejected` (description
  `this node is draining; publish to another node`), an SRT publish is rejected in the handshake (reason 1503), and
  a WHIP `POST /whip/{stream}` gets 307 to the same path on another node that is not draining (its `peers[].api`;
  503 with `Retry-After: 10` if there is none). WHIP clients follow the redirect. Take the node out of your load
  balancer or DNS so that RTMP and SRT publishers reconnect elsewhere.
- **Connected publishers and viewers are kept**, and `/readyz` keeps returning 200 (remove the node from your load
  balancer separately).
- On a single node there is nowhere to move grid work, so it stays (after draining, a pull input stays `running`
  and `/readyz` stays 200). New publishers are refused (WHIP gets 503).

To take a single device (GPU or capture device) out of service, use
`POST /api/v1/nodes/{node}/devices/{device}/disable` (and `enable` to bring it back).

## 13.10 Graceful shutdown

On SIGTERM or SIGINT (`Ctrl-C`) the node shuts down in this order:

1. Logs `shutting down`; `/readyz` starts returning 503.
2. In a cluster, sends GoAway on its inter-node relay connections so peers know the node is going away.
3. For every stream, closes the segment in progress and closes all active recordings as `finalized` (writes the
   `end` line to the index and fsyncs).
4. Stops the codec workers.
5. Logs `bye` and exits with code 0.

- Publisher and viewer connections end when the process exits.
- There is no shutdown timeout of its own (in tests it completed within a few to a few tens of milliseconds).
  `docker stop` sends SIGKILL after 10 s by default. After SIGKILL or a power loss, the next start recovers
  ([13.6](#136-recovery-after-a-crash)).

## 13.11 Capacity planning

### What the node declares

| Option | Default | Meaning |
|---|---|---|
| `--capacity-cpu-millis` | CPUs × 1000 × 90% | CPU the grid may hand out to workloads (leaves room for the OS and control plane) |
| `--capacity-memory-mb` | 85% of `MemTotal` | Memory for workloads |
| `--net-mbps` | 10000 | Bandwidth, applied to each direction |
| `--label KEY=VALUE` | none | Placement constraints (`zone=a` and so on) |

GPUs are declared per device found by the codec worker, as encode and decode session counts (encode defaults to 8
when the limit is unknown; decode is at least 16). See `capacity` and `gpus` in `GET /api/v1/grid`.

### What the grid counts

The grid places work by adding up fixed **estimates (reservations)** per workload
(`crates/senca-control/src/workloads.rs`), not observed usage.

| Workload | CPU | Memory | Bandwidth |
|---|---|---|---|
| Pull input | 150 m | 64 MB | 10 Mbit/s in |
| Capture input | 500 m | 64 MB | — (one GPU encode session when encoding H.264/H.265) |
| Push output | 100 m | 64 MB | 10 Mbit/s out |
| Pipeline | from its nodes (decode 400 m, or 100 m with GPU decode; compose, whiteboard, scale and crop 300 m; ...) | from its nodes | 10 Mbit/s each way |
| Room (participant limit N, default 50) | 200 + 20N m | 64 + 2N MB | 2N Mbit/s in, 2N × min(N, 8) Mbit/s out |

- **Pushed ingest (RTMP, SRT, WHIP publishers) and HLS and other viewers are not part of grid reservations.**
  Leave headroom for them by declaring less than the hardware has via `--capacity-*` and `--net-mbps`.
- With the default limit of 50 participants a room reserves 800 Mbit/s outbound. If you lower `--net-mbps`, rooms
  may become `unschedulable`; set each room's `max_participants` realistically.
- The number of codec worker processes for one-shot jobs is `--codec-workers` (default: number of CPUs); each running
  pipeline gets its own worker process on top of that.
- The recorder queue size comes from `--record-queue` ([3. Configuration](03-configuration.md)). A slow disk can
  overflow it, which records gaps in recordings (live delivery is not affected).

### Measured numbers

On a single development machine (8 threads, loopback), 4 publishers × 200 HLS viewers (about 4 Mbit/s each) for 60 s
delivered 877 Mbit/s with a segment fetch p99 of 11.9 ms and no errors
([performance.md](../../dev/conformance/performance.md), Japanese). This checks the tooling and shows trends; it is not a
guarantee. Load tests in the reference environment and the 72-hour soak are still to be done.
