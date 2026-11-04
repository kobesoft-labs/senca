# 12. Security

Authentication and scopes, user accounts and login (password, MFA, OIDC), credentials for publishers and viewers
(stream keys, signed URLs, join tokens), secret storage, egress restrictions, cluster mTLS and the codec worker sandbox.

- Related pages: [3. Configuration](03-configuration.md), [9. Cluster and grid](09-cluster-grid.md), [10. Realtime rooms](10-rooms.md), [13. Operations](13-operations.md), [15. Management API and SDKs](15-api-and-sdks.md)
- Rationale: [ADR-0002 Dependencies and cryptography](../../dev/adr/0002-dependencies-and-crypto.md), [ADR-0017 User accounts, login sessions and OIDC](../../dev/adr/0017-users-sessions-oidc.md) (Japanese), [ADR-0007 Inputs, outputs, secrets and egress](../../dev/adr/0007-inputs-outputs-egress.md), [ADR-0009 Distributed control plane](../../dev/adr/0009-cluster.md)

## 12.1 Authenticating to the management API

The management API (`/api/v1/...`) and SSE (`/api/v1/events`) use `Authorization: Bearer <value>`.

- A value starting with `senca_` is an **API token**, one starting with `sencas_` is a user's **login session**
  ([12.13](#1213-user-accounts-and-login)), and anything else is treated as a **JWT** (only when `--jwks` is set).
- Failures return 401 with `WWW-Authenticate: Bearer`. The reason (unknown token, expired, bad signature, ...) is
  never put in the response; the node logs it as `auth failed: <reason> request_id=...`.
- Served without authentication: `/healthz`, `/readyz`, the static files of the console and sender page, signed
  playback URLs (`/s/...`), and the login operations (`GET /api/v1/auth/providers`, `POST /api/v1/auth/login`,
  `POST /api/v1/auth/login/mfa`, `POST /api/v1/auth/exchange`, `GET /api/v1/auth/oidc/{id}/start`,
  `GET /api/v1/auth/oidc/{id}/callback`). WHIP authenticates with the stream key, WHEP with the playback signature, and room
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
**additional tenants come from JWTs or OIDC** (the tenant is a JWT claim, or comes from the OIDC IdP configuration,
[12.15](#1215-oidc-single-sign-on)).

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
| `login` | client IP address | failed logins (password, codes, OIDC) | 10 per minute (`--rate-limit-login`) |
| `login_account` | account (tenant and username) | failed passwords and codes | 5, then waits doubling from 1 second (up to 15 minutes) |

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
  forge playback URLs and room join tokens. They also contain users' password hashes (Argon2id), hashes of login
  sessions and recovery codes, and encrypted authenticator secrets.
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

`subject` is `token:{ID}` for API tokens, `user:{user ID}` for login sessions and the `sub` claim for JWTs. Secrets
and request bodies are never written. Responses remembered for Idempotency-Key replay have `stream_key`, `token`,
`secret`, `temporary_password`, `recovery_codes`, `otpauth_uri`, `enrollment_token` and `mfa_token` removed before
storage (a replayed response shows `null` plus `*_redacted: true`).

Logins and user operations are recorded with the following `action` values (passwords, codes, recovery codes and
session values are never written).

| `action` | Meaning |
|---|---|
| `auth.login` | A login, with `outcome` (`success`, `failure`, `mfa_required`, `callback`), `method` (`password`, `password+totp`, `password+recovery_code`, `oidc:{id}`), `reason` and `ip`. A failure with an unknown username is recorded with `subject: anonymous`, and the attempted username is not recorded |
| `auth.logout`, `auth.session.revoke` | Logout, revoking one of your own sessions |
| `auth.password.change` | Changing your own password |
| `auth.mfa.enable`, `auth.mfa.disable`, `auth.mfa.recovery_codes`, `auth.mfa.recovery_code_used` | Registering and disabling MFA, regenerating recovery codes, using a recovery code |
| `user.create`, `user.update`, `user.delete` | Creating, changing (whether roles or scopes changed, disabling) and deleting users. `by: "oidc:{id}"` when OIDC created the user or recomputed its roles |
| `user.password.reset`, `user.mfa.reset`, `user.sessions.revoke`, `user.oidc.link` | An administrator resetting a password, removing MFA, revoking sessions; linking to an IdP user |

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

## 12.13 User accounts and login

When people use the console or the management API, create **user accounts** for them instead of handing out API
tokens. There are two ways to log in, and both can be enabled at the same time:

- **Password** (optionally with a code from an authenticator app, [12.14](#1214-multi-factor-authentication-mfa))
- **OIDC single sign-on** (Google Workspace, Microsoft Entra ID, Keycloak, Auth0, ...; [12.15](#1215-oidc-single-sign-on))

Logging in returns a **session** (a value starting with `sencas_`). Use it in `Authorization: Bearer` just like an
API token.

### Users

- Users belong to a tenant. Users of other tenants are invisible (404). A tenant can have up to 1000 users.
- A username is 1–64 characters of letters, digits and `. _ @ + -`. Usernames are case-insensitive (`Alice` and
  `alice` are the same username). Email addresses must also be unique within a tenant.
- The password is optional. A user without a password logs in with OIDC only.
- A user's permissions are the union of their **roles** and individually granted scopes.

| Role | Scopes |
|---|---|
| `admin` | `tenant:admin` (everything except `node:admin`) |
| `operator` | `stream:read`, `stream:write`, `publish`, `subscribe`, `recording:read` |
| `viewer` | `stream:read`, `recording:read`, `subscribe` |

  Scopes that no role includes (`recording:delete`, `node:admin`, ...) are granted individually with `scopes`.
  Changes to roles and scopes take effect immediately.

### Creating the first administrator

If `setup_required` in `GET /api/v1/auth/providers` (no authentication needed) is `true`, there are no users yet.
The console then shows a "create the first administrator" screen. With the API, create the user with the bootstrap
admin token ([12.1](#121-authenticating-to-the-management-api)):

```sh
TOKEN=$(cat /var/lib/senca/control/bootstrap-admin-token)
curl -s -X POST http://127.0.0.1:8080/api/v1/users \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"username": "alice", "email": "alice@example.com", "password": "a long password that is hard to guess", "roles": ["admin"]}'
```

The bootstrap admin token keeps working after users exist. See [12.1](#121-authenticating-to-the-management-api)
for how to replace it.

### Passwords

- 12–256 characters (counted as characters, so any language works). The username and email address cannot be used as
  the password. Passwords are used exactly as given, without Unicode normalization.
- The server stores only an Argon2id hash (19 MiB, 2 iterations, parallelism 1, a per-user salt). The parameters are
  recorded inside the hash, so when they are raised in a future version, the hash is replaced with one using the new
  parameters at the next login.
- Users change their own password with `POST /api/v1/auth/password` (the current password is required). All their
  other sessions are then revoked; the session that made the change stays.
- Administrators reset a password with `POST /api/v1/users/{id}/password`. Without a password in the body, the node
  generates a 20-character temporary password and returns it in that response only. A user who logs in with a
  temporary password cannot do anything else until they change it (`pending: ["password_change"]`).

### Logging in with a password

```sh
curl -s -X POST http://127.0.0.1:8080/api/v1/auth/login -H 'Content-Type: application/json' \
  -d '{"username": "alice", "password": "a long password that is hard to guess"}'
# → {"status": "ok", "token": "sencas_...", "session": {...}, "user": {...}, "pending": [], ...}
```

- For users of a tenant other than `default`, add `"tenant": "acme"` to the body.
- Users who registered an authenticator app get `{"status": "mfa_required", "mfa_token": "..."}` instead
  ([12.14](#1214-multi-factor-authentication-mfa)).
- An unknown user, a wrong password and a disabled user all get the same 401. The node does the same amount of hashing
  for unknown users, so neither the response nor its timing reveals whether a user exists.
- **Failure limits**: 10 failures per minute per client IP address (`--rate-limit-login`) and 5 failures per account.
  From the 6th failure, the account waits 1 s, 2 s, 4 s, ... (up to 15 minutes; the count starts over after an hour
  without failures). While it waits, even the correct password gets 429 (`details.limit` is `login_account`). The
  IP address limit shows up as `details.limit: login`. Both are counted per node.
- `--auth-password-login off` disables password login (OIDC only).

### Sessions

- A session expires after 8 hours without use (`--auth-session-idle`), and 7 days after login even while in use
  (`--auth-session-lifetime`).
- Sessions are also revoked on logout (`POST /api/v1/auth/logout`), password change or reset, when the user is
  disabled or deleted, and when the user's roles or scopes change.
- Users list their own sessions with `GET /api/v1/auth/sessions` (with the IP address and browser they logged in
  from) and revoke one with `DELETE /api/v1/auth/sessions/{id}`. Administrators use `GET` and
  `DELETE /api/v1/users/{id}/sessions`. Each user can have up to 20 sessions; beyond that the oldest is removed.
- The server stores only the SHA-256 of the session value. In a cluster, sessions are replicated to every node, so a
  session from a login on any node works on every node.
- The last-used time is recorded every few minutes (every 5 minutes by default), not on every request, to keep writes
  to the cluster's consensus log low. The idle expiry can therefore come up to that much earlier.
- The `subject` in the audit log is `user:{user ID}`. `GET /api/v1/auth/me` shows your user, tenant, scopes and the
  session's expiry.

### Managing users

Managing users requires `tenant:admin` (`/api/v1/users`).

| Operation | API |
|---|---|
| Create, list, get | `POST /users`, `GET /users`, `GET /users/{id}` |
| Change roles, scopes, disabled, display name, email | `PATCH /users/{id}` (`If-Match` required) |
| Delete | `DELETE /users/{id}` |
| Reset the password | `POST /users/{id}/password` |
| Remove MFA (lost authenticator) | `DELETE /users/{id}/mfa` |
| List sessions, revoke all sessions | `GET /users/{id}/sessions`, `DELETE /users/{id}/sessions` |

- **You can only grant scopes you hold yourself** (403). Only a principal with `node:admin` can grant `node:admin`.
  You also cannot change, delete or reset the password of a user who holds a scope you do not have (for example a
  user with `node:admin`) (403).
- **The last administrator of a tenant** (an enabled user with `tenant:admin`) cannot be deleted, disabled or demoted
  (409). Create another administrator first.
- Creating, changing and deleting users is published on SSE (`GET /api/v1/events`) as `user.created`, `user.updated`
  and `user.deleted` (only to principals with `tenant:admin`).

## 12.14 Multi-factor authentication (MFA)

Users who log in with a password can register an authenticator app (TOTP: Google Authenticator, Microsoft
Authenticator, 1Password, ...) as a second factor.

1. `POST /api/v1/auth/mfa/totp/setup` (body: the current password, `{"password": "..."}`) returns `otpauth_uri` (for
   a QR code), `secret` (to type in by hand) and `enrollment_token`. The secret is shown only this once.
2. After adding it to the app, send the 6-digit code to `POST /api/v1/auth/mfa/totp/confirm`
   (`{"enrollment_token": "...", "code": "123456"}`) to enable MFA.
3. Have the user store the **10 recovery codes** (`xxxx-xxxx-xxxx-xxxx`) from the response in a safe place. They are
   also shown only this once.

Logging in once MFA is enabled:

```sh
curl -s -X POST .../api/v1/auth/login -d '{"username": "alice", "password": "..."}'      # → {"status": "mfa_required", "mfa_token": "..."}
curl -s -X POST .../api/v1/auth/login/mfa -d '{"mfa_token": "...", "code": "123456"}'   # → {"status": "ok", "token": "sencas_..."}
```

- A recovery code can be used instead of the app (`{"mfa_token": "...", "recovery_code": "abcd-efgh-jkmn-pqrs"}`).
  Each recovery code works once.
- The same code cannot be used twice (of the codes that change every 30 seconds, only ones later than the last used
  code are accepted). Clock drift of up to 30 seconds either way is tolerated.
- Regenerating recovery codes (`POST /api/v1/auth/mfa/recovery-codes`) and disabling MFA
  (`POST /api/v1/auth/mfa/disable`) need the current password or a code (`{"password": "..."}` or `{"code": "..."}`).
- On a node started with `--auth-require-mfa`, MFA is mandatory for users who log in with a password. Users who have
  not registered get a session with `pending: ["mfa_enrollment"]` and cannot do anything else until they register.
  MFA cannot be disabled on such a node.
- The authenticator secret is encrypted with the same key as secrets ([12.5](#125-secrets)). Only hashes of recovery
  codes are stored.
- SENCA's MFA does not apply to users who log in with OIDC. Configure MFA at the IdP.

## 12.15 OIDC single sign-on

The node acts as an OpenID Connect relying party and users authenticate on the IdP's login page (authorization code
flow, always with PKCE (S256), `state` and `nonce`).

### Flow

1. The login button opens `oidc[].start_url` from `GET /api/v1/auth/providers`
   (`/api/v1/auth/oidc/{id}/start?return_to=/console/...`) in the browser.
2. The node redirects to the IdP. The in-flight values are kept in an encrypted cookie (`senca_oidc`, HttpOnly,
   10 minutes).
3. When the IdP redirects back to `/api/v1/auth/oidc/{id}/callback`, the node fetches and validates the ID token,
   determines the user and redirects to the console at `/console/#/auth/complete?code=...` (or `?error=...` on
   failure).
4. The console sends `POST /api/v1/auth/exchange` (`{"code": "..."}`) from the same browser and receives a session.
   The code is valid once, for 60 seconds.

In a cluster, steps 1, 3 and 4 may each reach a different node (the cookie is encrypted with the cluster-wide key).

### On the IdP side

- Register `{public URL}/api/v1/auth/oidc/{id}/callback` as the **redirect URI** (`{public URL}` is
  `--public-http-url`). To use a different URL, for example behind a reverse proxy, set `redirect_url`. When
  `--public-http-url` is `https://`, the cookie gets `Secure`.
- A client with a client secret (confidential client) is recommended. Put the secret in a file and point
  `client_secret_file` at it. Clients without a secret (public clients) also work.
- The default scopes are `openid email profile`.

### On the SENCA side

For one IdP use startup options; for several, use a JSON file (`--oidc-config`).

```sh
senca-node ... --public-http-url https://senca.example.com \
  --oidc-id keycloak --oidc-display-name "Company account" \
  --oidc-issuer https://sso.example.com/realms/media --oidc-client-id senca \
  --oidc-client-secret-file /etc/senca/keycloak-client-secret \
  --oidc-role-map groups=senca-admins:admin --oidc-default-role viewer --oidc-auto-provision
```

The `--oidc-config` file has the form `{"providers": [ ... ]}`, and each entry has these fields:

| Field | Default | Meaning |
|---|---|---|
| `id` | (required) | Name used in URLs (lowercase letters, digits, `-`, `_`; 1–32 characters) |
| `display_name` | `id` | Name shown on the login button |
| `issuer` | (required) | Issuer URL. The node fetches `{issuer}/.well-known/openid-configuration` |
| `client_id` | (required) | Client ID |
| `client_secret_file` | none | File with the client secret (surrounding whitespace is removed). Without it, a public client |
| `scopes` | `["openid", "email", "profile"]` | Scopes to request (`openid` is required) |
| `redirect_url` | `{--public-http-url}/api/v1/auth/oidc/{id}/callback` | Redirect URI registered at the IdP |
| `allowed_email_domains` | `[]` (no restriction) | Email domains allowed to log in |
| `require_email_verified` | `true` | When the email address is used (domain restriction, linking to existing users), require `email_verified: true` |
| `tenant` | `default` | Tenant the users belong to |
| `tenant_claim` | none | Name of the claim that decides the tenant (cannot be combined with `tenant`) |
| `role_mappings` | `[]` | List of `{"claim", "value", "role"}`. If the claim contains the value, the user gets the role (`admin`, `operator`, `viewer`) or scope |
| `default_role` | none | Role when no mapping matches. Without it, the login is refused |
| `sync_roles` | `true` | Recompute roles from `role_mappings` at every login (only when `role_mappings` is set) |
| `auto_provision` | `false` | Create a user automatically on first login |
| `username_claim` | `preferred_username` | Claim used for the username of automatically created users (falls back to `email`, then `sub`) |
| `prompt`, `max_age` | none | Passed through in the authorization request (with `max_age`, `auth_time` is checked too) |

The startup options are `--oidc-id`, `--oidc-display-name`, `--oidc-issuer`, `--oidc-client-id`,
`--oidc-client-secret-file`, `--oidc-scopes`, `--oidc-redirect-url`, `--oidc-allowed-domain` (repeatable),
`--oidc-tenant`, `--oidc-tenant-claim`, `--oidc-role-map CLAIM=VALUE:ROLE` (repeatable), `--oidc-default-role`,
`--oidc-no-sync-roles`, `--oidc-auto-provision`, `--oidc-allow-unverified-email`, `--oidc-username-claim`,
`--oidc-prompt` and `--oidc-max-age` ([3. Configuration](03-configuration.md)).

### How the user is determined

1. If a SENCA user is linked to the IdP user (the ID token's `sub`), that user.
2. Otherwise, a user with the same email address is looked up and linked (only when the email address is verified).
   If an administrator creates a user with an email address in advance, that user is linked the first time the
   person logs in with OIDC.
3. Otherwise, with `auto_provision: true` a new user (without a password) is created. With `false`, the login is
   refused (`error=no_account`).

- With `role_mappings`, roles are recomputed at every login. Join nested claims with `.` (for example Keycloak's
  `realm_access.roles`). The IdP configuration may also grant `node:admin`.
- Without `role_mappings`, `default_role` is used when a user is created automatically, and later role changes are
  made in SENCA (console or API).
- The list of `error` values on failure (`email_domain_not_allowed`, `no_role`, `account_disabled`, ...) is in the
  OpenAPI description of `GET /auth/oidc/{provider}/callback`.

### ID token validation

- The node fetches only the configured issuer's discovery document and the JWKS and token endpoint listed in it
  (it never connects to URLs found in tokens). A discovery document whose `issuer` differs from the configuration,
  or one that lists an `http://` endpoint when the issuer is `https://`, is not used.
- Discovery and JWKS are cached for an hour. A token with an unknown `kid` triggers a JWKS refetch, so key rotation
  is picked up.
- Checked: the signature (EdDSA, ES256, RS256; the algorithm is decided by the key type, and `none` and HS algorithms
  are rejected), `iss`, `aud` (must contain `client_id`), `exp`, `nbf`, `iat` (within 10 minutes), `nonce`, `azp`
  (when present, and required when `aud` has several values) and `sub`. Clock drift of up to 60 seconds is tolerated.
- The issuer is a destination configured by the administrator, so the egress checks of
  [12.6](#126-egress-restrictions-ssrf-protection) (`--egress-allow`) are not applied to it. A Keycloak on an internal
  network works without adding an allow rule.

### Google (Google Workspace)

1. In the Google Cloud console under "APIs & Services" → "Credentials", create an OAuth client ID of type "Web
   application". Add `https://senca.example.com/api/v1/auth/oidc/google/callback` to "Authorized redirect URIs".
2. Save the client secret in a file.
3. Google ID tokens carry no group information, so restrict users by domain and assign roles in SENCA.

```json
{"providers": [{
  "id": "google", "display_name": "Sign in with Google",
  "issuer": "https://accounts.google.com",
  "client_id": "1234567890-abcdefg.apps.googleusercontent.com",
  "client_secret_file": "/etc/senca/google-client-secret",
  "allowed_email_domains": ["example.com"],
  "auto_provision": true, "default_role": "viewer"
}]}
```

Create the people who should be administrators in advance with `POST /api/v1/users`
(`{"username": "alice", "email": "alice@example.com", "roles": ["admin"]}`); they are linked on their first login.

### Microsoft Entra ID

1. In the Entra admin center under "App registrations", register a new application and add
   `https://senca.example.com/api/v1/auth/oidc/entra/callback` as a redirect URI of the "Web" platform. Create a
   client secret under "Certificates & secrets".
2. To decide roles by group, add a "groups claim" under "Token configuration" (security groups, group ID in the ID
   token). The `groups` claim then contains the groups' object IDs.
3. Use the tenant-specific issuer `https://login.microsoftonline.com/{tenant ID}/v2.0` (`common` and
   `organizations` do not work).

```json
{"providers": [{
  "id": "entra", "display_name": "Sign in with Microsoft",
  "issuer": "https://login.microsoftonline.com/00000000-0000-0000-0000-000000000000/v2.0",
  "client_id": "11111111-2222-3333-4444-555555555555",
  "client_secret_file": "/etc/senca/entra-client-secret",
  "auto_provision": true,
  "role_mappings": [
    {"claim": "groups", "value": "aaaaaaaa-0000-0000-0000-000000000001", "role": "admin"},
    {"claim": "groups", "value": "aaaaaaaa-0000-0000-0000-000000000002", "role": "operator"}
  ],
  "default_role": "viewer"
}]}
```

- Entra ID tokens do not include `email_verified`, and users can sometimes change `email`. By default, linking by
  email address and `allowed_email_domains` therefore do not apply (restricting domains gives
  `error=email_not_verified`). As in the example, use the tenant-specific issuer to limit logins to your organization
  and decide roles by group. Set `"require_email_verified": false` only when your organization manages the email
  addresses and you want to link existing users by email.
- When a user belongs to many groups (more than 200 in an ID token), the `groups` claim is omitted. In that case,
  configure the app to emit only the groups assigned to the application.

### Keycloak

1. Create a client in the realm (Client authentication ON, Standard flow ON). Add
   `https://senca.example.com/api/v1/auth/oidc/keycloak/callback` to "Valid redirect URIs" and save the secret from
   "Credentials" in a file.
2. To decide roles by group, add a "Group Membership" mapper to a client scope (Token Claim Name `groups`, Full group
   path OFF, Add to ID token ON). To use realm roles, turn on Add to ID token for the "realm roles" mapper of the
   "roles" scope (the claim is `realm_access.roles`).

```json
{"providers": [{
  "id": "keycloak", "display_name": "Company account",
  "issuer": "https://sso.example.com/realms/media",
  "client_id": "senca",
  "client_secret_file": "/etc/senca/keycloak-client-secret",
  "auto_provision": true,
  "role_mappings": [
    {"claim": "groups", "value": "senca-admins", "role": "admin"},
    {"claim": "realm_access.roles", "value": "senca-operator", "role": "operator"}
  ],
  "default_role": "viewer"
}]}
```

In Keycloak, a user's email address counts as verified only when "Email verified" is ON for that user.

### Auth0 and others

- The Auth0 issuer is `https://{domain}/` (including the trailing `/`). Use the application type "Regular Web
  Application" and add the redirect URI to "Allowed Callback URLs".
- Other IdPs work if they support discovery (`/.well-known/openid-configuration`), the authorization code flow and
  PKCE with S256. ID tokens must be signed with EdDSA, ES256 or RS256.

### Tenants from claims

To serve several organizations from one SENCA, the tenant can come from an IdP claim (`"tenant_claim": "org"`). The
claim value must be 1–64 characters of letters, digits and `. _ -`. Since API tokens can only be created inside the
creator's tenant, the first administrator of a new tenant comes from this or from a JWT.

## 12.16 When nobody can log in

- **Another administrator exists**: they use `POST /api/v1/users/{id}/password` (reset the password) or
  `DELETE /api/v1/users/{id}/mfa` (remove MFA).
- **You have an admin token**: on the node's server, run the following. It re-enables the user, removes MFA and
  prints a temporary password; the user must change it at the next login.

  ```sh
  senca-node reset-password --data-dir /var/lib/senca --username alice
  # temporary password for alice: Xk3... (20 characters)
  ```

  It calls the running node's management API (default `http://127.0.0.1:8080`, change with `--api`) with the token in
  `{data-dir}/control/bootstrap-admin-token` (or `recovery-admin-token` if that does not exist). Pass a token in
  another file with `--token-file`. If the user does not exist, `--create-admin` creates an administrator. Because it
  goes through the API, it works in a cluster too.
- **No admin token either** (the bootstrap token was deleted or its value lost): restart one node with
  `--recovery-admin-token`. The node writes an admin token for tenant `default` (`tenant:admin`, `node:admin`, expires
  in 24 hours) to `control/recovery-admin-token` (mode 0600). Run `reset-password` above with it, then delete the file
  and restart without the option (no new token is created while the file exists). Anyone who can change the data
  directory and the startup arguments can already read the whole control state, so this does not grant additional
  privileges.
