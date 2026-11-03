# 3. Configuration

`senca-node` is configured with command-line options and, optionally, a JSON file passed with `--config FILE`.
This page lists every option, the JSON format, the cluster configuration, environment variables and the layout of
the data directory.

- Related pages: [2. Quickstart](02-quickstart.md), [9. Cluster and grid](09-cluster-grid.md), [12. Security](12-security.md), [13. Operations](13-operations.md)
- `senca-node --help` also prints the list of options.

## 3.1 Rules

- Options are written as `--name value`; the `--name=value` form is not accepted. The options that take no value
  are `--capture-synthetic`, `--verify-recordings`, `--log-json` and `--rooms-cascade`.
- An unknown option is an error (exit code 2, usage printed). An invalid configuration never starts the node;
  nothing is applied partially.
- If `--config FILE` is present, that JSON is loaded first and the other command-line options are then applied on
  top of it, regardless of their position.
- Listen addresses are `IP:port` (for example `0.0.0.0:1935` or `[::]:8080`); host names are not accepted.
  Every listener except `--http` can be disabled with `off`.
- `--help` (`-h`) prints usage and exits. `--healthcheck URL` is handled
  [separately](13-operations.md#132-health-checks) and must be the first argument.

## 3.2 Options

### Listeners

| Option | Value | Default | Meaning |
|---|---|---|---|
| `--http` | ADDR | `0.0.0.0:8080` | HTTP: management API (`/api/v1`), SSE, signed HLS (`/s/...`), WHIP/WHEP, room signaling (`/signaling`), console, `/healthz`, `/readyz`. Cannot be disabled |
| `--https` | ADDR\|`off` | off | HTTPS serving the same content as HTTP. Requires `--tls-cert` and `--tls-key` |
| `--rtmp` | ADDR\|`off` | `0.0.0.0:1935` | RTMP (publish and play) |
| `--rtmps` | ADDR\|`off` | off | RTMPS. Requires `--tls-cert` and `--tls-key` |
| `--srt` | ADDR\|`off` | off | SRT (UDP) |
| `--rtsp` | ADDR\|`off` | off | RTSP play (TCP) |
| `--webrtc-udp` | ADDR\|`off` | off | The WebRTC UDP socket shared by WHIP, WHEP and rooms. When bound to `0.0.0.0:port`, the ICE candidates carry the node's network interface addresses (loopback excluded); when bound to a specific address, that address |
| `--webrtc-public-ip` | IP[,IP…] | none | IP addresses to announce as ICE candidates (with the `--webrtc-udp` port; up to 8, repeatable). Use it when peers see the node at a different address than it listens on, as behind NAT or in Docker |
| `--quic` | ADDR\|`off` | off | QUIC v1 over UDP: HTTP/3 (ALPN `h3`, the same content as HTTP except SSE and `/signaling`), WebTransport and MoQ draft-22 (ALPN `moqt-22`; `/moq` over WebTransport) on one port. Requires `--tls-cert` and `--tls-key`. When enabled, HTTP and HTTPS responses carry `Alt-Svc: h3=":PORT"` (see [4. Ingest and playback](04-ingest-playback.md#415-moq-and-http3)) |
| `--tls-cert` | FILE | none | Certificate chain for `--https`, `--rtmps` and `--quic` (PEM, leaf first) |
| `--tls-key` | FILE | none | Its private key (PEM). Startup fails if it does not match the leaf's public key |

### Public URLs

The externally visible URLs that SENCA puts into API responses (`ingest`) and playback URLs. They are configured
independently of the listen addresses.

| Option | Default | Used for |
|---|---|---|
| `--public-http-url` | `http://127.0.0.1:8080` | `hls` and `whep` in playback URLs; `ingest.whip_url` |
| `--public-rtmp-url` | `rtmp://127.0.0.1:1935` | `ingest.rtmp_url` (`{URL}/live`); `rtmp` in playback URLs (`{URL}/play/{id}?...`) |
| `--public-srt-url` | none | `ingest.srt_url` (used verbatim). Omitted when unset |
| `--public-rtsp-url` | none | `rtsp` in playback URLs (`{URL}/live/{id}?...`). Omitted when unset |

Set these whenever you change ports from the defaults, or when clients reach the node through a reverse proxy or
from outside a container.

### Playback and recording

| Option | Value | Default | Meaning |
|---|---|---|---|
| `--max-playback-ttl` | seconds | `86400` | Upper bound for `ttl_secs` on playback URLs (live and recordings). Must be > 0 |
| `--reconnect-grace` | ms | `30000` | How long a stream waits for its publisher to come back and continue as the same broadcast |
| `--record-queue` | N | `4096` | Base size of the recorder queue (16 to 1048576). The per-stream queue is N/64 entries, clamped to 16–4096 (64 segments by default). Overflow becomes a gap in the recording |
| `--retention-sweep` | seconds | `60` | Interval of the sweep that deletes recordings past their retention (and expires Idempotency-Key records). Minimum 1 |
| `--verify-recordings` | — | off | During startup recovery, check the SHA-256 of every indexed segment and log mismatches (startup time grows with the amount of data) |
| `--zoneinfo` | DIR | `/usr/share/zoneinfo` | Where TZif files are read for schedule time zones (IANA names) |

### Authentication (JWT)

| Option | Value | Default | Meaning |
|---|---|---|---|
| `--jwks` | FILE | none | Local JWKS file with the keys used to verify JWTs. Read once at startup |
| `--jwt-issuer` | string | none | Required value of `iss`. Mandatory with `--jwks` |
| `--jwt-audience` | string | none | Value that `aud` must contain. Mandatory with `--jwks` |
| `--jwt-tenant-claim` | name | `tenant` | Name of the claim that carries the tenant |

See [12. Security](12-security.md).

### Codecs and capture

| Option | Value | Default | Meaning |
|---|---|---|---|
| `--codec-workers` | N | `0` (= number of CPUs) | Number of codec worker processes for one-shot jobs (snapshots, thumbnails and the like) |
| `--codec-worker-bin` | FILE | `senca-codec-worker` next to `senca-node` | The codec worker executable. If it cannot be found, transcoding returns 503 |
| `--codec-lib-dir` | DIR | none | Directory searched first for codec shared libraries. Passed to the codec and capture workers as `SENCA_CODEC_LIB_DIR` (several directories may be joined with `:`). See [2.3](02-quickstart.md#23-codec-libraries) |
| `--capture-synthetic` | — | off | Expose a synthetic test capture device (passes `--synthetic` to the capture worker) |

### Web pages

| Option | Value | Default | Meaning |
|---|---|---|---|
| `--console-dir` | DIR | none | Serve the console (`apps/console`) at `/console/`. 404 when unset |
| `--sender-dir` | DIR | none | Serve the browser publishing page (`apps/sender-web`) at `/sender/`. 404 when unset |

`/` redirects (302) to `/console/`.

### Node and grid

| Option | Value | Default | Meaning |
|---|---|---|---|
| `--node-name` | name | `node-1` | Display name of the node. Becomes the grid label `senca.io/name`; device endpoints accept it in place of the node ID |
| `--label` | KEY=VALUE | none | Label used by placement constraints (`placement.labels`). Repeatable. Key 1–128 characters, value up to 128, no control characters; keys starting with `senca.io/` and `raft_id` are reserved; at most 64 labels |
| `--net-mbps` | Mbit/s | `10000` | Bandwidth of this node, applied to both directions. Counted by grid placement |
| `--capacity-cpu-millis` | millicores | CPUs × 1000 × 90% | CPU offered to workloads |
| `--capacity-memory-mb` | MB | 85% of `MemTotal` | Memory offered to workloads |
| `--cluster-config` | FILE | none | Cluster configuration (JSON, see [3.4](#34-cluster-configuration)). Without it the node runs alone |
| `--egress-allow` | CIDR | none | Private address range that outbound connections (pull inputs, push outputs, captions, PaaS, HLS pull) may reach. Repeatable. See [12. Security](12-security.md) |

### Rooms (realtime)

| Option | Value | Default | Meaning |
|---|---|---|---|
| `--rooms-cascade` | — | off | Use cascading for every room (serve participants of rooms placed on other nodes here even if the room setting `cascade` is false). [10. Realtime rooms](10-rooms.md) |
| `--rooms-auth-webhook` | URL | none | Auth webhook for rooms (the room setting `auth_webhook_url` takes precedence). Requires `--rooms-webhook-secret-file` |
| `--rooms-auth-mode` | `after_token`, `only` | `after_token` | `after_token` asks the webhook after the token is verified; `only` admits by webhook instead of a token |
| `--rooms-event-webhook` | URL | none | Webhook that receives room events (signed, retried). Requires `--rooms-webhook-secret-file` |
| `--rooms-webhook-secret-file` | FILE | none | File with the webhook signing secret (HMAC-SHA256), at least 16 bytes after trimming whitespace |

Webhook destinations are checked against the `--egress-allow` policy (private addresses must be allowed).

### Request rate limits

Requests over a limit get 429 (`RATE_LIMITED` with `Retry-After`; see [12. Security](12-security.md)). `0` disables each limit.

| Option | Value | Default | Meaning |
|---|---|---|---|
| `--rate-limit-ip` | N | `300` | Management API (`/api/v1`) requests per second per client IP address (bursts up to twice that). Counted before authentication |
| `--rate-limit-token` | N | `200` | Management API requests per second per API token (per subject for JWTs), with bursts up to twice that |
| `--rate-limit-auth-failures` | N | `30` | Failed authentications allowed per client IP address per minute. Once used up, even a valid token gets 429 |
| `--rate-limit-connect` | N | `30` | New connections (`/signaling`, WHIP, WHEP, SSE) per second per client IP address (bursts up to twice that) |

### Other

| Option | Value | Default | Meaning |
|---|---|---|---|
| `--config` | FILE | none | JSON configuration ([3.3](#33-configuration-file-json)) |
| `--events-retain` | N | `4096` | Number of recent events the node keeps so that reconnecting SSE clients (`GET /api/v1/events`) get what they missed (0–1000000). See [15. API and SDKs](15-api-and-sdks.md) |
| `--data-dir` | DIR | `out` (relative to the current directory) | Data directory ([3.6](#36-data-directory)). Always set it |
| `--log-json` | — | off | Write node logs as one JSON object per line to stdout ([13. Operations](13-operations.md)) |

### Startup checks

The node refuses to start (exit code 2) when:

- `--jwks` is given without `--jwt-issuer` or `--jwt-audience`;
- `--https`, `--rtmps` or `--quic` is given without `--tls-cert` or `--tls-key`;
- `--record-queue` is outside 16–1048576, or `--max-playback-ttl` is 0;
- an `--egress-allow` value is not a valid CIDR;
- a `--webrtc-public-ip` value is not an IP address, is `0.0.0.0`, `::` or a multicast address, or more than 8 are given; `--events-retain` is over 1000000;
- `--rooms-auth-webhook` or `--rooms-event-webhook` is not an http/https URL, or is given without `--rooms-webhook-secret-file`;
  the secret file cannot be read or is shorter than 16 bytes;
- the cluster configuration has `node_id` 0, does not list itself in `peers`, or has fewer than 1 or more than 7 peers.

Problems found later during startup (unreadable files, a listener that cannot be bound, a cluster secret shorter
than 16 characters, and so on) end the process with `fatal: ...` and exit code 1.

## 3.3 Configuration file (JSON)

The `--config FILE` JSON is an object with the keys below. Every key is optional; missing keys take their default.
**Unknown keys are an error.** Mapping to command-line options:

| JSON key | Option | Type |
|---|---|---|
| `data_dir` | `--data-dir` | string |
| `node_name` | `--node-name` | string |
| `http_listen` | `--http` | string (ADDR) |
| `https_listen`, `rtmp_listen`, `rtmps_listen`, `srt_listen`, `rtsp_listen`, `webrtc_udp`, `quic_listen` | `--https` etc. (`quic_listen` is `--quic`) | string (ADDR) or `null` (disabled) |
| `tls_cert`, `tls_key` | `--tls-cert`, `--tls-key` | string |
| `public` | `--public-*-url` | `{"rtmp": string, "http": string or null, "srt": string or null, "rtsp": string or null}` |
| `max_playback_ttl_secs` | `--max-playback-ttl` | number |
| `reconnect_grace_ms` | `--reconnect-grace` | number |
| `record_queue` | `--record-queue` | number |
| `retention_sweep_secs` | `--retention-sweep` | number |
| `jwks`, `jwt_issuer`, `jwt_audience`, `jwt_tenant_claim` | `--jwks` etc. | string |
| `codec_workers` | `--codec-workers` | number |
| `codec_worker_bin`, `codec_lib_dir` | same names | string |
| `zoneinfo_dir` | `--zoneinfo` | string |
| `console_dir`, `sender_dir` | same names | string |
| `capture_synthetic`, `verify_recordings`, `log_json` | same names | boolean |
| `cluster` | `--cluster-config` | object ([3.4](#34-cluster-configuration)) |
| `egress_allow` | `--egress-allow` | array of strings |
| `net_mbps` | `--net-mbps` | number |
| `labels` | `--label` | object mapping string to string |
| `capacity_cpu_millis`, `capacity_memory_mb` | same names | number or `null` |
| `rooms_cascade` | `--rooms-cascade` | boolean |
| `rooms_auth_webhook`, `rooms_event_webhook` | same names | string or `null` |
| `rooms_auth_mode` | `--rooms-auth-mode` | `"after_token"` or `"only"` |
| `rooms_webhook_secret_file` | same names | string or `null` |

Notes:

- If `rtmp_listen` is omitted from the JSON, the default (`0.0.0.0:1935`) applies. Write `null` to disable RTMP.
- Keys omitted inside `public` take their defaults (`rtmp` and `http` as in [Public URLs](#public-urls); `srt` and
  `rtsp` unset). An explicit `"http": null` makes the `hls` playback URL relative (starting with `/s/...`) and drops
  `ingest.whip_url`.
- `labels` are checked with the same rules as `--label` (the node does not start otherwise; exit code 2).

Example:

```json
{
  "data_dir": "/var/lib/senca",
  "node_name": "edge-1",
  "http_listen": "0.0.0.0:8080",
  "rtmp_listen": "0.0.0.0:1935",
  "srt_listen": "0.0.0.0:9000",
  "public": {
    "rtmp": "rtmp://media.example.com:1935",
    "http": "https://media.example.com",
    "srt": "srt://media.example.com:9000"
  },
  "codec_lib_dir": "/usr/lib/x86_64-linux-gnu",
  "labels": { "zone": "a" },
  "log_json": true
}
```

## 3.4 Cluster configuration

Passed with `--cluster-config FILE` (or as `cluster` inside `--config`). Unknown keys are an error. For how to run
a cluster, see [9. Cluster and grid](09-cluster-grid.md).

### ClusterConfig

| Key | Required | Type | Meaning |
|---|---|---|---|
| `node_id` | yes | number (≥ 1) | Raft ID of this node. `peers` must contain an entry with the same `id` |
| `listen` | yes | ADDR | Listen address for inter-node (Raft) traffic |
| `peers` | yes | array (1–7) | Voting nodes, including this one |
| `secret_file` | yes | path | File with the cluster secret. Same content on every node; at least 16 characters after trimming whitespace |
| `tls_ca` | no | path | CA certificate for inter-node mTLS (PEM) |
| `tls_cert` | no | path | This node's certificate (PEM, SAN `DNS:senca-cluster`) |
| `tls_key` | no | path | Its private key (PEM) |
| `relay_listen` | no | ADDR | Listen address for the inter-node relay. Defaults to the `listen` port + 1 |

Give all three of `tls_ca`, `tls_cert` and `tls_key`, or none of them. Without them inter-node traffic is
plaintext ([12. Security](12-security.md#127-cluster-mtls)).

### ClusterPeer (`peers[]`)

| Key | Required | Type | Meaning |
|---|---|---|---|
| `id` | yes | number | The peer's Raft ID |
| `addr` | yes | ADDR | The peer's Raft address |
| `api` | no | URL | The peer's public HTTP URL (e.g. `http://10.0.0.2:8080`). Used for 307 redirects of viewers and for redirecting room participants to another node |
| `relay` | no | ADDR | The peer's relay address. Defaults to the `addr` port + 1 |

Because the relay ports default to "Raft port + 1", make sure that port is free. When several nodes share one host
(tests), set `relay_listen` and `peers[].relay` explicitly. A configuration where some node's relay address
collides with another node's Raft or relay address (for example Raft on 7000 and 7001 on one host with default
relay ports) is refused at startup (exit code 2, naming the colliding addresses).

```json
{
  "node_id": 1,
  "listen": "10.0.0.1:7000",
  "secret_file": "/etc/senca/cluster-secret",
  "tls_ca": "/etc/senca/cluster-ca.pem",
  "tls_cert": "/etc/senca/n1.pem",
  "tls_key": "/etc/senca/n1.key",
  "peers": [
    {"id": 1, "addr": "10.0.0.1:7000", "api": "http://10.0.0.1:8080"},
    {"id": 2, "addr": "10.0.0.2:7000", "api": "http://10.0.0.2:8080"},
    {"id": 3, "addr": "10.0.0.3:7000", "api": "http://10.0.0.3:8080"}
  ]
}
```

## 3.5 Environment variables

`senca-node` itself reads no environment variables; everything is configured through options and JSON.
The workers it starts read the following.

| Variable | Read by | Meaning |
|---|---|---|
| `SENCA_CODEC_LIB_DIR` | codec worker | Directories searched first for codec libraries (`:`-separated). Replaced by `--codec-lib-dir` when that is set |
| `SENCA_CODEC_DISABLE` | codec worker | Providers to skip (`,`-separated): `hardware`, `software`, or any of `nvidia`, `intel-vpl`, `amd-amf`, `vaapi`, `dav1d`, `libvpx`, `svt-av1`, `opus` |
| `LD_LIBRARY_PATH` | codec worker | Used by the OS loader when a library is opened by name |
| `SENCA_CAPTURE_SYNTHETIC` | capture worker | `1` or `true` exposes a synthetic test device |
| `SENCA_CAPTURE_DISABLE` | capture worker | Backends not to probe (`,`-separated: `v4l2`, `alsa`, `decklink`) |
| `SENCA_CAPTURE_LIB_DIR` | capture worker | Directories searched for DeckLink and ALSA libraries (`:`-separated) |

- The codec worker starts with an empty environment; only `SENCA_CODEC_LIB_DIR`, `SENCA_CODEC_DISABLE` and
  `LD_LIBRARY_PATH` are copied from the node.
- The capture worker inherits the node's environment.
- `SENCA_PUBLIC_HTTP`, `SENCA_PUBLIC_RTMP` and `SENCA_PUBLIC_IP` in `compose.yaml` are compose variable substitutions that feed
  `--public-*-url` and `--webrtc-public-ip`; the node does not read them.

## 3.6 Data directory

Contents of `--data-dir`. The node creates what it needs.

| Path | Contents |
|---|---|
| `node-id` | This node's ID (128-bit, hex). Created once and never changed |
| `audit.log` | Audit log (one JSON object per line, append-only) |
| `control/log` | Single-node control-state log (append-only, each record fsynced) |
| `control/snapshot.json` | Single-node control-state snapshot. Rewritten when the log exceeds 1000 records, after which the log is emptied |
| `control/raft/` | Raft storage in a cluster (`hardstate`, `snap-*.snap`, `log/*.seg`). `control/log` is not used in a cluster |
| `control/secret-key` | Key that seals stored secrets (32 bytes, mode 0600). Created on first start if missing. In a cluster, one node's key becomes the cluster-wide key, wrapped with the cluster secret and replicated |
| `control/bootstrap-admin-token` | Value of the first admin token (mode 0600). Written only on the very first start |
| `recordings/{recording ID}/index.jsonl` | Recording index (append-only JSON Lines: `begin`, `init`, `segment`, `gap`, `end`) |
| `recordings/{recording ID}/thumbnail.jpg`, `thumbnail.json` | Recording thumbnail and its metadata |
| `recordings/.trash-{recording ID}` | Deletion in progress (temporary) |
| `segments/{stream ID}/{run}/{sequence}.ts` or `.m4s` (plus the fMP4 init segment) | Recorded segments (immutable). `run` is an ID per stream task |
| `segments/{stream ID}/orphans/{run}/` | Segments found by startup recovery to be referenced by no index (quarantined, not deleted) |
| `snapshots/{stream ID}/{ID}.jpg`, `{ID}.json` | Periodic snapshots and their metadata |

- The single-node control state (`control/log`, `snapshot.json`) contains the playback signing key in plain text.
  Restrict the data directory's permissions as described in [12. Security](12-security.md).
- What to back up is covered in [13. Operations](13-operations.md).
