# 12. Security

Authentication and scopes, credentials for publishers and viewers (stream keys, signed URLs, join tokens), secret
storage, egress restrictions, cluster mTLS and the codec worker sandbox.

- Related pages: [3. Configuration](03-configuration.md), [9. Cluster and grid](09-cluster-grid.md), [10. Realtime rooms](10-rooms.md), [13. Operations](13-operations.md), [15. Management API and SDKs](15-api-and-sdks.md)
- Rationale: [ADR-0002 Dependencies and cryptography](../../dev/adr/0002-dependencies-and-crypto.md), [ADR-0007 Inputs, outputs, secrets and egress](../../dev/adr/0007-inputs-outputs-egress.md), [ADR-0009 Distributed control plane](../../dev/adr/0009-cluster.md)

## 12.1 Authenticating to the management API

The management API (`/api/v1/...`) and SSE (`/api/v1/events`) use `Authorization: Bearer <value>`.

- A value starting with `senca_` is an **API token**; anything else is treated as a **JWT** (only when `--jwks` is set).
- Failures return 401 with `WWW-Authenticate: Bearer`. The reason (unknown token, expired, bad signature, ...) is
  never put in the response; the node logs it as `auth failed: <reason> request_id=...`.
- Served without authentication: `/healthz`, `/readyz`, the static files of the console and sender page, and signed
  playback URLs (`/s/...`). WHIP authenticates with the stream key, WHEP with the playback signature, and room
  signaling with a join token.

### API tokens

- Format: `senca_` followed by 32 random bytes in base64url (43 characters). The server stores only a SHA-256 hash
  and compares against every stored hash in constant time.
- The value is returned only in the create response (`POST /api/v1/tokens`, 201), never by list or get.
- Create body: `{"name": "...", "scopes": ["..."], "expires_at": "RFC 3339"}` (`expires_at` optional, must be in
  the future).
- Creating tokens requires `tenant:admin`, and **you can only grant scopes you hold yourself**. A new token belongs to
  the creator's tenant.
- At most 100 tokens per tenant. The last `tenant:admin` token of a tenant cannot be deleted (409).

### Scopes

| Scope | Allows |
|---|---|
| `stream:read` | Reading streams, schedules, inputs, outputs, pipelines, rooms and media pipelines; snapshots; subscribing to SSE |
| `stream:write` | Creating, changing and deleting streams and rotating their keys; starting, stopping and changing recordings (retention, legal hold); changing schedules, inputs, outputs, pipelines, rooms and media pipelines; issuing room join tokens |
| `publish` | Publishing with WHIP (usable instead of the stream key in `Authorization: Bearer` of `POST /whip/{stream_id}`; streams of the same tenant only) |
| `subscribe` | Issuing live playback URLs |
| `recording:read` | Reading recordings; recording playback URLs, thumbnails and MP4 export |
| `recording:delete` | Deleting recordings |
| `tenant:admin` | Everything except `node:admin`, plus managing tokens and secrets and reading the audit log (`GET /audit`) |
| `node:admin` | Operations outside any tenant: `/nodes`, `/grid`, `/devices`, draining, disabling and enabling devices, node and grid events, reading other tenants' audit log (`tenant` on `GET /audit`) |

Per-operation scopes are listed in [15. Management API and SDKs](15-api-and-sdks.md) and the OpenAPI document
([docs/api/openapi.yaml](../../api/openapi.yaml)).

### Tenants

Every resource belongs to a tenant, and resources of other tenants are invisible (404). The first admin token
belongs to tenant `default`. Since API tokens can only be created inside the creator's tenant,
**additional tenants come from JWTs** (the tenant is a JWT claim).

### The bootstrap admin token

On the very first start the node writes the first token (`tenant:admin` and `node:admin`, no expiry) to
`data_dir/control/bootstrap-admin-token` with mode 0600. In a cluster only the node with the lowest ID creates it.
Before going live:

1. Create tokens that carry only the scopes each client needs, with an expiry.
2. Create a new admin token, then delete the bootstrap one with `DELETE /api/v1/tokens/{id}` (the last
   `tenant:admin` token cannot be deleted, so create the replacement first).
3. Delete the `bootstrap-admin-token` file or move it somewhere safe; the node never reads it again.

### Request rate limits

To stop brute force and overuse, the node counts requests and answers 429 (`RATE_LIMITED`) above a limit. The response carries
the seconds to wait in `Retry-After` and the limit that was hit in `details.limit`. Limits are counted per node (not summed over a cluster).

| `details.limit` | Counted per | Applies to | Default (option) |
|---|---|---|---|
| `ip` | client IP address | management API (`/api/v1`), counted before authentication | 300 per second (`--rate-limit-ip`) |
| `token` | API token (subject for JWTs) | management API | 200 per second (`--rate-limit-token`) |
| `auth_failures` | client IP address | failed authentication (management API, SSE, wrong WHIP tokens and stream keys) | 30 per minute (`--rate-limit-auth-failures`) |
| `connect` | client IP address | new connections (`/signaling`, WHIP, WHEP, SSE) | 30 per second (`--rate-limit-connect`) |

- Per-second limits allow short bursts of up to twice the rate.
- An IP address that has used up its authentication failures gets 429 even with a valid token until it recovers.
- The client IP address is the peer of the TCP connection the node accepted. Behind a reverse proxy, also limit requests at the proxy.
- Each limit can be disabled with `0` ([3. Configuration](03-configuration.md)).

### Request rate limits

To stop brute force and overuse, the node counts requests and answers 429 (`RATE_LIMITED`) above a limit. The response carries
the seconds to wait in `Retry-After` and the limit that was hit in `details.limit`. Limits are counted per node (not summed over a cluster).

| `details.limit` | Counted per | Applies to | Default (option) |
|---|---|---|---|
| `ip` | client IP address | management API (`/api/v1`), counted before authentication | 300 per second (`--rate-limit-ip`) |
| `token` | API token (subject for JWTs) | management API | 200 per second (`--rate-limit-token`) |
| `auth_failures` | client IP address | failed authentication (management API, SSE, wrong WHIP tokens and stream keys) | 30 per minute (`--rate-limit-auth-failures`) |
| `connect` | client IP address | new connections (`/signaling`, WHIP, WHEP, SSE) | 30 per second (`--rate-limit-connect`) |

- Per-second limits allow short bursts of up to twice the rate.
- An IP address that has used up its authentication failures gets 429 even with a valid token until it recovers.
- The client IP address is the peer of the TCP connection the node accepted. Behind a reverse proxy, also limit requests at the proxy.
- Each limit can be disabled with `0` ([3. Configuration](03-configuration.md)).

## 12.2 JWT

To accept tokens from an external identity provider, give the node its verification keys as a local JWKS file.

```sh
senca-node ... --jwks /etc/senca/jwks.json \
  --jwt-issuer https://idp.example.com/ --jwt-audience senca [--jwt-tenant-claim tenant]
```

- **Algorithms**: EdDSA (Ed25519), ES256 (P-256) and RS256 (2048 bits or more). The key type determines the
  algorithm; a header `alg` that does not match the key is rejected. `none` and the HS family are not supported.
- **JWKS**: every key needs a `kid`; `use`, if present, must be `sig`; `alg`, if present, must match the key type.
  A single unsupported key prevents startup (keys are never skipped silently). The file is read once at startup, so
  restart the node after rotating keys. SENCA never fetches keys from URLs found in a token.
- **Header**: `kid` is required; a `crit` header is rejected.
- **Claims**:
  - `exp` is required; `nbf` is optional; up to 60 s of clock skew is tolerated.
  - `iss` must equal `--jwt-issuer`; `aud` (string or array) must contain `--jwt-audience`.
  - Tenant: the claim named by `--jwt-tenant-claim` (default `tenant`). Missing or empty values are rejected.
  - Scopes: `scope` (space-separated string or array), falling back to `scp`. Unknown names are ignored.
  - `sub` becomes the `subject` in the audit log.

Example payload:

```json
{"iss": "https://idp.example.com/", "aud": "senca", "sub": "alice", "tenant": "acme",
 "scope": "stream:read stream:write", "exp": 1793669243}
```

With this token you can create streams in tenant `acme`. APIs that need `tenant:admin` return 403, and the
streams it creates are not visible from the `default` tenant.

## 12.3 Stream keys (publisher credentials)

- RTMP, RTMPS, SRT and WHIP publishers authenticate with the stream key: `sk_` followed by 32 random bytes in base64url.
- Only a SHA-256 hash is stored. The value appears only in the responses to creating a stream and to
  `POST /api/v1/streams/{id}/keys` (rotation); rotating invalidates the old key.
- RTMP takes the key as the last path segment (`rtmp://host:1935/live/<key>`), SRT as streamid `publish:<key>`, and
  WHIP as `Authorization: Bearer <key>`.
- WHIP also accepts an API token (or JWT) with the `publish` scope instead of the stream key (for streams of the same tenant).
  You can hand the sending equipment a token with an expiry instead of the stream key, and delete it afterwards.
- WHIP also accepts an API token (or JWT) with the `publish` scope instead of the stream key (for streams of the same tenant).
  You can hand the sending equipment a token with an expiry instead of the stream key, and delete it afterwards.
- Stream keys and tokens are kept out of logs, errors and debug output.
- If a stream has a `live` schedule, publishing is accepted only while that schedule is running
  ([5. Recording](05-recording.md)).

## 12.4 Signed playback URLs

- `POST /api/v1/streams/{id}/playback-url` (`subscribe`) issues live URLs; `POST /api/v1/recordings/{id}/playback-url`
  (`recording:read`) issues VOD URLs. Body: `{"ttl_secs": N}` (default 300, 1 to `--max-playback-ttl`).
- The signature is the first 16 bytes of HMAC-SHA256 over `{kind}/{id}/{exp}` (kind `live` or `vod`), base64url
  encoded. The key is the playback key in the control state (32 random bytes created on first start; identical on
  every node of a cluster).
- For live streams the same signature works for HLS (`/s/{exp}/{sig}/live/{id}/...`), RTMP play
  (`/play/{id}?exp=&sig=`), WHEP (`/whep/{id}?exp=&sig=`) and RTSP play. Recordings use HLS
  (`/s/{exp}/{sig}/vod/{id}/...`).
- Past `exp` (UNIX seconds) or with a wrong signature, requests get 403.
- **Anyone holding the URL can use it until it expires**; URLs are not bound to a viewer and cannot be revoked.
  Keep TTLs short and issue URLs on demand.
- There is no API to rotate the playback key. Because the key lives in the control state, anyone who can read the
  control-state files can forge URLs ([12.8](#128-protecting-the-data-directory)).

## 12.5 Secrets

Credentials for inputs and outputs (RTMP keys, SRT passphrases, PaaS credentials) and caption API keys are stored
as secrets and referenced by name. Putting credentials into a URL (`rtmp://user:pass@...`) is rejected.

```sh
curl -s -X POST http://127.0.0.1:8080/api/v1/secrets \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"youtube-key","value":"xxxx-xxxx"}'
# → {"id": "...", "name": "youtube-key", "ref": "secret:youtube-key", "created_at": "..."}
```

- Managed with `tenant:admin` (create, list, delete). The value is never returned. A secret that an input, output or
  caption setting still uses cannot be deleted (409).
- Inputs and outputs reference it with `credential_ref: "secret:{name}"`; captions with `asr_credential_ref` and
  `translate_credential_ref`.
- Values are sealed with AES-256-GCM, and only the sealed form is kept in the control state. The tenant and secret
  ID are bound as associated data, so a sealed value copied to another resource cannot be opened.
- **Single node**: the key is the node key `data_dir/control/secret-key` (32 bytes, mode 0600, created on first start).
- **Cluster**: the key is a cluster-wide **data key**. The data key is wrapped with AES-256-GCM under a key derived
  from the cluster secret (`secret_file`) with HKDF-SHA256, and replicated to every node in the control state
  (`secret_data_key`). If there is none yet, every node proposes its own `secret-key`, wrapped, and the first one
  applied is used (it is never replaced). Each node unwraps it from the state, so a secret stored through any node
  can be opened on every node. No key files need to be copied.
  - The cluster secret must be the same on every node. A node with a different secret cannot unwrap the key, logs
    `cannot open the cluster-wide secret key`, and cannot use secrets (storing one returns 503).
  - Storing a secret on a node that has not received the data key yet returns 503 (`retryable: true`).
  - A copy of the control state (a backup, the Raft log) alone does not reveal secrets without the cluster secret.
    Use an unguessable cluster secret such as 32 random bytes (`openssl rand -hex 32`).
  - **Migration from earlier versions**: earlier cluster versions sealed a secret with the `secret-key` of the node
    that received it. The new version also tries the local `secret-key` after the data key when opening, and after
    startup initialization it re-seals every secret that only the local key can open with the data key (log:
    `re-sealed N secret(s)`). Once the node that sealed an old value has started the new version, every node can
    open it.
  - Changing the cluster secret makes the wrapped key unreadable (rotating the cluster secret is not supported yet).
- If `secret-key` is lost on a single node, stored secrets cannot be recovered; store them again. In a cluster,
  the cluster secret and the control state are enough to recover them.

## 12.6 Egress restrictions (SSRF protection)

Pull inputs, push outputs, HLS pull, caption endpoints, PaaS APIs and room webhooks make the node connect to user-supplied
destinations. SENCA checks the **resolved IP address** and by default refuses:

- loopback (`127.0.0.0/8`, `::1`), unspecified (`0.0.0.0`, `::`), `0.0.0.0/8`
- link-local (`169.254.0.0/16`, which includes the cloud metadata address `169.254.169.254`; `fe80::/10`)
- private ranges (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`), CGNAT (`100.64.0.0/10`), ULA (`fc00::/7`)
- multicast and broadcast. IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) are checked as IPv4

To reach cameras or other hosts on an internal network, an administrator allows the range explicitly:

```sh
senca-node ... --egress-allow 10.1.0.0/16 --egress-allow 192.168.10.0/24
```

A refused destination shows up in the input's or output's `status.last_error` and in the log as
`<IP> は許されていない宛先です（--egress-allow で許可が必要）` ("destination not allowed; --egress-allow required"),
and the node keeps retrying with backoff.

## 12.7 Cluster mTLS

Inter-node traffic (Raft and relay) uses mTLS when `tls_ca`, `tls_cert` and `tls_key` are set in the cluster
configuration. Both sides verify the peer certificate against the cluster CA and check the name `senca-cluster`.

Creating the certificates (P-256):

```sh
# Cluster CA (once; do not keep its key on the nodes)
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout cluster-ca.key -out cluster-ca.pem -days 3650 -subj "/CN=senca-cluster-ca"

# One certificate per node (n1, n2, n3, ...)
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout n1.key -out n1.csr -subj "/CN=n1"
printf "subjectAltName=DNS:senca-cluster\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth,clientAuth\n" > n1.ext
openssl x509 -req -in n1.csr -CA cluster-ca.pem -CAkey cluster-ca.key -CAcreateserial \
  -out n1.pem -days 825 -extfile n1.ext
```

- Node certificates need SAN `DNS:senca-cluster` and both `serverAuth` and `clientAuth` extended key usages.
- Install `cluster-ca.pem` plus the node's own `nN.pem` and `nN.key` on each node and reference them in its
  cluster configuration.
- A node presenting a certificate from a different CA is rejected.
- On Raft and relay connections, a mutual challenge-response with the cluster secret (`secret_file`) also runs
  inside TLS (see below).

### Node authentication with the cluster secret

Every Raft and relay connection starts with a mutual challenge-response, with or without mTLS:

1. The dialing node sends its node ID and a fresh 32-byte nonce.
2. The accepting node answers with its node ID, its own fresh nonce, and `HMAC-SHA256(K, "server" ‖ purpose ‖ IDs ‖ both nonces)`.
3. The dialing node checks that the peer is the node it dialed and that the MAC is correct, and only then sends
   `HMAC-SHA256(K, "client" ‖ ...)`, which the accepting node checks.

`K` is derived from the cluster secret with HKDF-SHA256, separately for Raft and for the relay. Neither the secret
nor a fixed value crosses the network, every MAC covers nonces chosen by the other side (a recorded handshake
cannot be replayed), and the client proves itself only after verifying the server. Messages have fixed lengths
and the whole exchange must finish within 5 seconds.

### Plaintext (shared secret only)

Without `tls_*`, nodes talk plain TCP and authenticate each other only with the challenge-response above.

- Control-state replication (token and stream-key hashes, **the playback key**, sealed secrets, the wrapped
  secret key, ...) and relayed media cross the network unencrypted, and nothing protects a connection from being
  modified after the handshake.
- Use plaintext mode only on an isolated, trusted network.

## 12.8 Protecting the data directory

- The single-node `control/log` and `control/snapshot.json`, and `control/raft/` in a cluster, contain the playback
  key (hex, unencrypted), token and stream-key hashes, and sealed secrets. With the playback key, an attacker can
  forge playback URLs and room join tokens.
- The node creates `secret-key` and `bootstrap-admin-token` with mode 0600 and sets `control/` to mode 0700 at every
  start, so other local users cannot read the control files. Recordings and segments follow the umask. Run the node as
  a dedicated user and consider `chmod 700` on the whole data directory (the Docker image runs as the dedicated user `senca` with data in a volume).
- Backups contain the same secrets; protect them the same way ([13. Operations](13-operations.md)).

## 12.9 Codec worker sandbox

External codec libraries (GPU SDKs, dav1d, libvpx, ...) are never loaded into `senca-node`; they run in
`senca-codec-worker`, a separate process that restricts itself on Linux as follows.

| Stage | Restrictions |
|---|---|
| Before loading libraries | stdout isolation (the protocol uses its own fd), `PR_SET_PDEATHSIG=SIGKILL`, `PR_SET_DUMPABLE=0`, `RLIMIT_CORE=0`, `RLIMIT_NOFILE=256`, `PR_SET_NO_NEW_PRIVS`, Landlock where the kernel supports it (creating, writing and deleting files is denied everywhere except under `/dev`; reading is not restricted) |
| After loading libraries | A seccomp-BPF deny list on all threads: networking (socket, connect, bind, listen, accept), executing programs, ptrace and access to other processes' memory, mount, kernel modules, kexec, bpf, keyrings, namespaces, reboot, swap, setting the clock, and more. `clone3` returns `ENOSYS` |
| Per job | `RLIMIT_CPU` (300 s per job). Wall-clock timeouts are enforced by the node (30 s for one-shot jobs) |

- The worker starts with an empty environment plus `SENCA_CODEC_LIB_DIR`, `SENCA_CODEC_DISABLE` and `LD_LIBRARY_PATH`.
- What was applied is logged at startup (for example
  `sandbox: core=0; nofile=256; no_new_privs; landlock(abi 8): writes only under /dev; cpu=300s/job; seccomp deny-list`)
  and reported as `sandbox` in `GET /api/v1/devices` (`node:admin`).
- If a worker crashes, hangs or returns malformed output, the job fails and the worker is replaced; the node keeps
  running.
- No address-space limit (`RLIMIT_AS`) is set by default. Because sockets are denied, some GPU driver helpers (such
  as connecting to NVIDIA's persistenced) are unavailable.

## 12.10 Room join tokens

- Issued by `POST /api/v1/rooms/{id}/tokens` (`stream:write`): JWT-compatible tokens signed with EdDSA, with
  `iss` `senca`, `aud` `senca-rooms` and `kid` `senca-rooms-1`. Lifetime `ttl_secs` (default 600, 10–86400).
  They carry the room, role, `client_id`, `bundle_id`, metadata and permissions. Requesting a token for a room that
  does not exist creates the room automatically.
- The signing key is an Ed25519 key derived from the playback key
  (`SHA-256("senca-rooms-join-token/1\0" || playback key)`), so every node can issue and verify tokens. A leaked
  playback key therefore also allows forging join tokens.
- By default a join token is required to join a room. With an auth webhook (`auth_webhook_url` or `--rooms-auth-webhook`),
  the node also asks the webhook after verifying the token. With `--rooms-auth-mode only`, the webhook admits participants
  instead of a token. See [10. Realtime rooms](10-rooms.md).

## 12.11 HTTPS and RTMPS

`--https` and `--rtmps` with `--tls-cert` and `--tls-key` (PEM) accept TLS 1.3. Supported key types are ECDSA P-256,
RSA and Ed25519. The TLS protocol handling is SENCA's own implementation; the cryptographic primitives come from vetted libraries. If you want TLS on the high-volume HLS and WHEP paths, you can also
terminate it at a reverse proxy: signatures live in the path and query, so they still verify behind a proxy. Set
`--public-http-url` to the proxy's URL.

## 12.12 Audit log

Every state-changing management API call is appended to `data_dir/audit.log` as one JSON object per line.

```json
{"action":"stream.create","request_id":"req_...","resource":"<resource ID>","subject":"token:<token ID>","tenant":"default","ts":"2026-11-03T01:10:15.531Z"}
```

`subject` is `token:{ID}` for API tokens and the `sub` claim for JWTs. Secrets and request bodies are never written.
Responses remembered for Idempotency-Key replay have `stream_key`, `token` and `secret` removed before storage
(a replayed response shows `null` plus `*_redacted: true`).

### Rotation

When `audit.log` reaches `--audit-log-max-bytes` (default 64 MiB), it is renamed to `audit.log.{N}` and a new `audit.log` is started.
`N` counts up from 1, so a larger number is a newer file. The newest `--audit-log-keep` (default 8) rotated files are kept and older
ones are deleted (by default, roughly 576 MiB per node at most). `--audit-log-max-bytes 0` turns rotation off.
Each line is appended with a single write, so a node that stops mid-way never damages earlier lines (a half-written line is skipped
when reading). To archive the log elsewhere, collect the rotated files (`audit.log.{N}`).

### Reading it through the API

`GET /api/v1/audit` returns the entries newest first (requires `tenant:admin`).

```sh
curl -s -H "Authorization: Bearer $TOKEN" \
  "http://127.0.0.1:8080/api/v1/audit?since=2026-11-03T00:00:00Z&action=stream.&limit=50"
```

```json
{"items":[{"action":"stream.create","node":"3d6d4c49...","node_name":"node-1","request_id":"req_...","resource":"<resource ID>",
  "subject":"token:<token ID>","tenant":"default","ts":"2026-11-03T01:10:15.531Z"}],"next_cursor":"eyJ0Ijox...","unavailable_nodes":[]}
```

| Query | Meaning |
|---|---|
| `since`, `until` | Time range (RFC 3339; `since` is inclusive, `until` exclusive) |
| `action` | Kind of operation: an exact match (`stream.create`), or a prefix match when it ends with `.` or `*` (`stream.`) |
| `subject`, `resource` | Exact match on who acted and on the target resource |
| `q` | Text contained in `action`, `subject`, `resource` or `request_id` (case-insensitive) |
| `node` | Only the entries of that node (ID or name) |
| `tenant` | Tenant to read. Defaults to your own; another tenant or `*` (all) also needs `node:admin` |
| `limit`, `cursor` | Paging as in every other list (follow `next_cursor` until it is `null`) |

- You only see your own tenant's entries.
- Files are read backwards in small chunks, so a large log is never loaded into memory as a whole. Each request reads at most 16 MiB
  per node; when that limit is reached, `next_cursor` is returned even if there are fewer items than `limit`. Reading continues across
  rotated files (entries in deleted files are gone).
- Lines that are not valid JSON are skipped. Entries may gain new fields in the future.

**Clusters**: an entry lives in the `audit.log` of the node that served the request. The node that receives `GET /audit` asks every node
over the inter-node connection (mutual authentication with the cluster secret, plus mTLS when configured; see
[12.7](#127-cluster-mtls)) and merges the answers newest first. `node` and `node_name` on each item name the node that wrote it. The
caller's token is never sent to other nodes: the receiving node derives the tenant filter from the caller's permissions and puts it in
the query. Nodes that do not answer within 5 seconds are listed in `unavailable_nodes` and their entries are missing (if you asked for
that node with `node`, the answer is 503). If node clocks drift, the order across nodes drifts too (keep clocks in sync with NTP or similar).
