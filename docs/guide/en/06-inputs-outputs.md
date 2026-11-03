# 6. Inputs and outputs

[日本語](../ja/06-inputs-outputs.md)

**Inputs** are connections SENCA makes to fetch media (pull); **outputs** send a stream elsewhere (push and PaaS integration).
Push ingest where the sender connects to SENCA (RTMP, SRT, WHIP) is authorized by the stream key and covered in [4. Ingest and playback](04-ingest-playback.md).

- Rationale: [ADR-0007 inputs, outputs and egress](../../dev/adr/0007-inputs-outputs-egress.md), [ADR-0010 PaaS integration](../../dev/adr/0010-paas.md), [ADR-0013 grid](../../dev/adr/0013-grid.md)
- Related pages: [4. Ingest and playback](04-ingest-playback.md), [9. Cluster and grid](09-cluster-grid.md), [12. Security](12-security.md), [15. API and SDKs](15-api-and-sdks.md)

`API`, `TOKEN` and `H` in the examples are the same as in [4. Ingest and playback](04-ingest-playback.md).

## 6.1 API

| Operation | Request | Scope |
|---|---|---|
| Add an input | `POST /streams/{id}/inputs` | `stream:write` |
| List inputs | `GET /streams/{id}/inputs?limit&cursor` | `stream:read` |
| Get, update, delete an input | `GET`, `PATCH`, `DELETE /inputs/{id}` | `stream:read` / `stream:write` |
| Add an output | `POST /streams/{id}/outputs` | `stream:write` |
| List outputs | `GET /streams/{id}/outputs?limit&cursor` | `stream:read` |
| Get, update, delete an output | `GET`, `PATCH`, `DELETE /outputs/{id}` | `stream:read` / `stream:write` |
| Secrets | `POST`, `GET /secrets`, `DELETE /secrets/{id}` | `tenant:admin` |

Body:

| Field | Input | Output | Meaning |
|---|---|---|---|
| `protocol` | required | required | Inputs: `rtmp_pull`, `rtsp_pull`, `srt_pull`, `hls_pull`, `device`. Outputs: `rtmp_push`, `srt_push`, `paas` |
| `url` | required | required except for `paas` | Remote endpoint. The scheme depends on the protocol (see below). At most 2048 characters, no spaces or control characters, and **`user:pass@` is rejected with 400** |
| `credential_ref` | optional | optional (required for `paas`) | `secret:{name}`. Credentials go here, not in the URL (§6.6) |
| `enabled` | default `true` | default `true` | When false, nothing runs |
| `options` | optional | optional | String map; up to 16 entries, keys up to 64 and values up to 256 characters |
| `provider` | | required for `paas` | `cloudflare_stream`, `aws_ivs`, `aws_medialive` |
| `placement` | optional | optional | Placement (§6.8) |

| protocol | `url` scheme |
|---|---|
| `rtmp_pull`, `rtmp_push` | `rtmp`, `rtmps` |
| `rtsp_pull` | `rtsp`, `rtsps` (over TLS) |
| `srt_pull`, `srt_push` | `srt` |
| `hls_pull` | `http`, `https` |
| `device` | `device:{device id}` ([4.7](04-ingest-playback.md)) |

- A stream can have up to 4 inputs and 16 outputs (beyond that, 403 `QUOTA_EXCEEDED`).
- `PATCH` can change `url`, `credential_ref` (`null` removes it), `enabled`, `options` and `placement`; `If-Match` is required.
  `protocol` and `provider` cannot be changed. A change bumps the generation and restarts the running connection.
- Responses include `status` (§6.5), `placement` and `placement_status` (§6.8); outputs also include `provider` and `provider_resource`.

## 6.2 Inputs (pull)

An input acts as the stream's sender. **A stream has one sender at a time**, so while a push sender is connected the input cannot
attach and keeps retrying (and vice versa). Input disconnects are subject to the reconnect grace like any sender ([4.16](04-ingest-playback.md)).

### rtmp_pull

```sh
curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" -d '{"protocol":"rtmp_pull","url":"rtmp://origin.example.com/live/program1"}'
```

- `url` is `rtmp[s]://host[:port]/{app}/{stream name}`. Default ports: 1935 for `rtmp`, 443 for `rtmps`.
- The stream name is the `credential_ref` secret if present, otherwise the last path component of the URL (an error if neither exists).
- Plays as live (play start = -2). If nothing is received for 20 seconds, the connection is closed and retried.

### rtsp_pull

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"cam-login","value":"viewer:s3cret"}'
curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" -d '{"protocol":"rtsp_pull","url":"rtsp://192.0.2.10:554/stream1","credential_ref":"secret:cam-login"}'
```

- Transport is **TCP interleaved only** (works through NAT and firewalls). Default port 554.
- Store credentials in the secret as `user:password`. Basic authentication is used only with `options.allow_basic: "true"` (it sends the password in clear text).
- `rtsps://` connects over TLS (default port 322). The server certificate is verified against the certificate authorities the OS
  trusts (the node's `SSL_CERT_FILE` environment variable can point to a CA file). For cameras with a self-signed certificate, pin the
  certificate's public key: put the SHA-256 of its SPKI, in base64 or hex, in `options.tls_spki_sha256` (several values can be separated
  with `,`). With a pin, the CA, validity period and host name are not checked. If the pin does not match, the TLS handshake fails;
  there is no fallback to plain RTSP. A malformed value is rejected with 400.

  ```sh
  # Compute the pin from the camera's certificate
  openssl x509 -in cam.crt -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64
  curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" \
    -d '{"protocol":"rtsp_pull","url":"rtsps://192.0.2.10:322/stream1","credential_ref":"secret:cam-login","options":{"tls_spki_sha256":"<value above>"}}'
  ```
- Connecting to private addresses such as on-premises cameras requires `--egress-allow` (§6.7).

### srt_pull

```sh
curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" -d '{"protocol":"srt_pull","url":"srt://contrib.example.com:9000?streamid=feed1&latency=200000","credential_ref":"secret:srt-feed-pass"}'
```

- SENCA connects as the caller. The payload is MPEG-TS.
- Only `streamid` and `latency` are read from the URL query. **`latency` is in microseconds** (as in ffmpeg URLs; `200000` = 200 ms). Default 200 ms.
- Do not put the passphrase in the URL; pass it with `credential_ref` (10–79 characters).

### hls_pull

```sh
curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" -d '{"protocol":"hls_pull","url":"https://cdn.example.com/live/master.m3u8"}'
```

- For a multivariant playlist, the variant with the highest `BANDWIDTH` is chosen.
- TS and fMP4 segments are supported and re-sent at real-time pace.
- A live playlist is first read from its last 3 segments, then reloaded every half `TARGETDURATION` (0.5–5 seconds).
- A playlist with `#EXT-X-ENDLIST` (VOD) is read to the end once; the state becomes `ended` (no looping).
- The latency added by buffering the source's segments cannot be removed.

### device

Capture devices; see [4.7 Capture devices](04-ingest-playback.md).

## 6.3 Outputs (push)

An output waits in `waiting_input` until the stream goes live, then connects to the destination. When the broadcast ends it closes
the connection and waits for the next one.

### rtmp_push

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"yt-key","value":"xxxx-xxxx-xxxx-xxxx"}'
curl -s -X POST "$API/streams/$SID/outputs" "${H[@]}" -d '{"protocol":"rtmp_push","url":"rtmps://live.example.com:443/app","credential_ref":"secret:yt-key"}'
```

- `url` is `rtmp[s]://host[:port]/{app}[/{stream key}]`. The stream key is the `credential_ref` secret if present, otherwise the last
  path component of the URL. A key written in the URL is visible in API responses (`url`), so `credential_ref` is recommended.
- Sends FLV (including Enhanced RTMP). Whether the destination accepts the stream's codecs depends on the destination.
- Timeouts: 15 seconds to connect and start publishing, 10 seconds per write.

### srt_push

```sh
curl -s -X POST "$API/streams/$SID/outputs" "${H[@]}" -d '{"protocol":"srt_push","url":"srt://receiver.example.com:9000?streamid=senca&latency=300000","credential_ref":"secret:srt-out-pass"}'
```

- Connects as the caller and sends MPEG-TS in 1316-byte chunks. `streamid` and `latency` (microseconds) work as for srt_pull; the passphrase is `credential_ref`.
- Codecs that do not fit TS (AV1, VP8, VP9) cannot be sent.

Sending to an external WHIP server (`whip_push`) is not supported yet; `protocol: "whip_push"` is rejected with 400.

## 6.4 PaaS integration (`protocol: "paas"`)

SENCA **creates an ingest endpoint** in a cloud media service and sends to it with its own RTMP(S) or SRT implementation.

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"cf","value":"0123456789abcdef0123456789abcdef:CF_API_TOKEN"}'
curl -s -X POST "$API/streams/$SID/outputs" "${H[@]}" -d '{"protocol":"paas","provider":"cloudflare_stream","credential_ref":"secret:cf","options":{"recording":"automatic"}}'
```

How it works:

1. When the output is enabled its state is `provisioning`, and the external resource (Live Input, Channel or Input) is created. Every
   resource is tagged `senca:owner={output id}` (Cloudflare: `meta`), and progress checkpoints and the resource ID are saved to the
   output's `provider_resource` before the next request is sent (a crash never creates duplicates; an existing resource with the same owner is adopted).
2. Ingest secrets (stream key, passphrase) are not stored; they are fetched from the provider's API each time before sending.
3. Media is sent the same way as `rtmp_push` / `srt_push`. Choose the ingest type with `options.ingest` (`rtmps` or `srt`; by default the provider's first ingest).
4. **Deleting the output deletes the external resource.** Resources without SENCA's tag are never deleted. If cleanup fails, the output
   definition is still removed and the failure is logged.

Connections to the provider APIs are subject to the destination checks in §6.7.

| `provider` | What is created | `credential_ref` value | `options` |
|---|---|---|---|
| `cloudflare_stream` | Cloudflare Stream Live Input | `account_id:api_token` | `recording`: `off` (default) or `automatic`. `customer_subdomain`: `customer-xxxx.cloudflarestream.com`. `endpoint`: API endpoint override (for testing) |
| `aws_ivs` | Amazon IVS Channel | `access_key_id:secret_access_key[:session_token]` | `region` (required). `channel_type`: `STANDARD` (default), `BASIC`, `ADVANCED_SD`, `ADVANCED_HD`. `latency_mode`: `LOW` (default), `NORMAL`. `recording_configuration_arn`. `endpoint` |
| `aws_medialive` | AWS Elemental MediaLive Input and Input Security Group | `access_key_id:secret_access_key[:session_token]` | `region` (required). `input_type`: `RTMP_PUSH` (default) or `SRT_LISTENER`. Either `allow_cidr` (the node's egress CIDRs, comma-separated, up to 10) or `input_security_group` (an existing ID that SENCA neither creates nor deletes) is required. `pipelines`: `1` (default) or `2`. `rtmp_app` (default `senca`). `srt_passphrase_secret_arn`. `srt_min_latency_ms` (120–15000, default 2000). `endpoint` |

- Cloudflare: sends over RTMPS (default) or SRT.
- IVS: sends to `rtmps://{ingestEndpoint}:443/app/`; SRT can be chosen if the channel has SRT. The provider refuses to delete a channel while it is live (`ConflictException`); if cleanup fails this way, the failure is logged and the channel must be removed by hand.
- MediaLive: only the Input SENCA sends to and the Security Group protecting it are created. **You create the Channel (encoding
  settings and destinations) yourself** and attach this Input. An Input still attached to a Channel cannot be deleted.
  `provider_resource` is `input=<id>;sg=<id>`.

## 6.5 Status and reconnection

`status` of inputs and outputs:

| Field | Meaning |
|---|---|
| `state` | `starting`, `provisioning` (PaaS), `waiting_input` (output waiting for the stream), `connecting`, `active`, `backoff`, `error`, `ended` (a VOD `hls_pull` finished) |
| `last_error` | Reason for the last failure (never contains secrets) |
| `connects` | Number of successful connections |
| `since` | When the current `state` began |
| `bytes` | Bytes sent or received |

`status` is filled in when queried on the node that runs the input or output (on other nodes, or when not running, it is `null`).

On failure the state becomes `error` and immediately `backoff`, and the connection is retried after an exponential backoff: 1 second
doubling up to 60 seconds, with each actual wait randomized between half and the full value. After a normal end (the remote finished
the broadcast) the next attempt starts after a short wait (0.5–1 second).

Example (an RTSP camera on 127.0.0.1 without `--egress-allow`):

```json
{"state": "backoff", "last_error": "127.0.0.1 は許されていない宛先です（--egress-allow で許可が必要）", "connects": 0, "bytes": 0,
 "since": "2026-11-03T01:19:43.679Z"}
```

(The message says that 127.0.0.1 is not an allowed destination and `--egress-allow` is required. Node messages are currently in Japanese.)

## 6.6 Secrets (`credential_ref`)

Credentials are stored as secrets and referenced as `secret:{name}`.

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"cam-login","value":"viewer:s3cret"}'
```

```json
{"id":"33bb760ab08f2f11af9fcd12af258649","name":"cam-login","ref":"secret:cam-login","created_at":"2026-11-03T01:19:07.291Z"}
```

- `name` follows the stream name rules (1–64 letters, digits and `-_.`) and is unique within the tenant (409 on duplicates). `value` is 1–8192 bytes.
- Values are encrypted with AES-256-GCM (with the node key `{data-dir}/control/secret-key` on a single node, with a cluster-wide key in a cluster; see [12. Security](12-security.md)). **A value can never be read back through
  the API** (lists show only `id`, `name`, `ref`, `created_at`). To change a value, delete and recreate the secret.
- A tenant can have up to 500 secrets.
- Secrets referenced by inputs, outputs or stream captions cannot be deleted (409).
- Referencing a secret that does not exist makes input/output creation or update fail with 400.

| Used by | Value format |
|---|---|
| `rtmp_pull` | Stream name |
| `rtmp_push` | Stream key |
| `rtsp_pull` | `user:password` |
| `srt_pull`, `srt_push` | Passphrase (10–79 characters) |
| `paas` | See the table in §6.4 |
| Captions ([8. Captions](08-captions.md)) | API key |
| Name `srt-passphrase` | Passphrase for the SRT listener ([4.5](04-ingest-playback.md)) |

## 6.7 Outbound connection restrictions (egress)

Destinations of pull inputs, push outputs, PaaS APIs and caption APIs are checked by **resolved IP address** (pointing a DNS name at
a private address does not get around the check). Rejected by default:

- Loopback (127.0.0.0/8, ::1), unspecified, multicast, broadcast, 0.0.0.0/8
- Link-local (169.254.0.0/16, including the cloud metadata address 169.254.169.254; fe80::/10)
- Private ranges (10/8, 172.16/12, 192.168/16, 100.64/10, fc00::/7), including IPv4-mapped IPv6

To reach on-premises cameras or destinations, an administrator allows ranges with `--egress-allow CIDR` (repeatable).

```sh
senca-node ... --egress-allow 192.0.2.0/24 --egress-allow 10.20.0.0/16
```

Disallowed destinations fail **at connect time**, not when the input or output is created, and the reason appears in `last_error` (see the example in §6.5).

## 6.8 Placement (grid)

Inputs and outputs with `enabled: true` become grid workloads (`input:{id}`, `output:{id}`) and run only on the node the scheduler assigns.

- Outputs prefer the node that receives the stream (saving one relay hop).
- Capture inputs are placed on the node that owns the device. Captures that encode H.264/H.265 reserve a GPU encode session.
- `placement` supports manual assignment (`node`, `device`), labels, anti-affinity, priority, strategy (`spread`, `binpack`) and the
  behavior when the assigned target stops (`pin_policy`). `degrade` is for pipelines only; using it on inputs or outputs is 400.
- The result is shown in `placement_status` (`state`: `running`, `pending`, `unschedulable`, `not_scheduled`; plus `node`, `node_name`, `reason`, and so on).

See [9. Cluster and grid](09-cluster-grid.md). A single node uses the same mechanism (it is the only placement target).
