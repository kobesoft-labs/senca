# 2. Quickstart

Build from source, run a single node, publish over RTMP and watch over HLS. Docker comes at the end.

- Related pages: [1. Overview](01-overview.md), [3. Configuration](03-configuration.md), [4. Ingest and playback](04-ingest-playback.md), [12. Security](12-security.md), [14. Codecs and patents](14-codecs-and-patents.md)
- Rationale: [ADR-0008 Container deployment](../../dev/adr/0008-container-deployment.md)

Every command on this page was run on Linux x86_64 (Ubuntu 26.04).

## 2.1 Requirements

- Linux (the codec worker sandbox relies on Linux seccomp and Landlock)
- Rust 1.90 or later (`rust-version` in `Cargo.toml`) and a C linker
- FFmpeg for test publishing and playback (optional; OBS or any other player works too)
- Royalty-free codec libraries (optional; see [2.3](#23-codec-libraries))

## 2.2 Build

```sh
cargo build --release -p senca-node -p senca-codec-worker
```

This produces `senca-node` and `senca-codec-worker` in `target/release/`. `senca-node` starts the
`senca-codec-worker` that sits next to it, so keep the two in the same directory (or point at the worker with
`--codec-worker-bin`). Add `-p senca-capture` if you need capture devices (it builds `senca-capture-worker`).

## 2.3 Codec libraries

AV1, VP8/VP9 and Opus are handled by the following shared libraries, which the codec worker loads with `dlopen`
at run time. They are optional: without them the node still delivers and records what it receives as is; only
transcoding, snapshots and thumbnails return 503.

| Library (soname) | Used for | Ubuntu 26.04 package |
|---|---|---|
| `libdav1d.so.7` | AV1 decoding | `libdav1d7` |
| `libSvtAv1Enc.so.2` | AV1 encoding | `libsvtav1enc2` |
| `libvpx.so.12` | VP8/VP9 encoding and decoding (only the libvpx 1.16 ABI) | `libvpx12` |
| `libopus.so.0` | Opus encoding and decoding | `libopus0` |

Lookup order:

1. If a file with that exact name exists in a directory given by `--codec-lib-dir DIR` (or, without the flag, the
   `SENCA_CODEC_LIB_DIR` environment variable), that file is loaded. `--codec-lib-dir` replaces
   `SENCA_CODEC_LIB_DIR` in the worker's environment; the value may list several directories separated by `:`.
2. Otherwise the library is opened by name only, using the normal OS search (`ld.so.cache`, `LD_LIBRARY_PATH`).

If you installed the distribution packages you do not need `--codec-lib-dir`; use it for libraries kept somewhere
else. Whatever was loaded is listed under `codecs.transcode` in `GET /api/v1/capabilities`.

No software H.264/H.265 codec is shipped or ever loaded. Transcoding H.264/H.265 needs a GPU and its vendor driver
([14. Codecs and patents](14-codecs-and-patents.md)).

## 2.4 Start the node

```sh
./target/release/senca-node --data-dir ./data \
  --http 127.0.0.1:8080 --rtmp 127.0.0.1:1935 \
  --console-dir ./apps/console
```

- By default SENCA listens on `0.0.0.0:8080` (HTTP) and `0.0.0.0:1935` (RTMP); here both are restricted to localhost.
- **Always pass `--data-dir`.** The default is `out`, relative to the current directory.
- The public URLs that end up in API responses and playback URLs default to `http://127.0.0.1:8080` and
  `rtmp://127.0.0.1:1935`. If your ports or externally visible host names differ, set `--public-http-url` and
  `--public-rtmp-url` to match ([3. Configuration](03-configuration.md)).

On startup the node logs to stderr (codec worker sandbox lines omitted):

```
created the first admin token in ./data/control/bootstrap-admin-token (mode 0600)
2026-11-03T01:10:04.936Z senca-node 0.0.0 started (node f1be5f27675d35f8dd72b88676be8754)
2026-11-03T01:10:04.936Z http listening on 127.0.0.1:8080
2026-11-03T01:10:04.936Z rtmp listening on 127.0.0.1:1935
2026-11-03T01:10:04.942Z grid: registered (cpu 7200 m, mem 20422 MB, gpus 0, capture 0)
2026-11-03T01:10:04.942Z grid: became the scheduler (leader)
```

Check it:

```sh
curl -s http://127.0.0.1:8080/healthz   # ok
curl -s http://127.0.0.1:8080/readyz    # ready
```

## 2.5 The admin token

On first start the node creates the first admin token (scopes `tenant:admin` and `node:admin`, tenant `default`,
no expiry) and writes it to `data_dir/control/bootstrap-admin-token` with mode 0600. There is no default password.

```sh
TOKEN=$(cat ./data/control/bootstrap-admin-token)
```

This token is powerful. Create narrower tokens for day-to-day use and move the file somewhere safe or delete it
([12. Security](12-security.md)).

## 2.6 Create a stream

```sh
curl -s -X POST http://127.0.0.1:8080/api/v1/streams \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"demo"}'
```

Response (201, abridged):

```json
{
  "id": "60dd5cca4fa2785a5d8eefc3b1f5d1bc",
  "name": "demo",
  "generation": 1,
  "desired_state": "running",
  "observed_state": "waiting_input",
  "hls": {"container": "ts", "segment_ms": 2000, "window": 6},
  "ingest": {
    "rtmp_url": "rtmp://127.0.0.1:1935/live",
    "stream_key": "sk_zgo54TfcU8PEPmrJDH34OHnjYR5Ot6nv__hlyLSf4pk",
    "whip_url": "http://127.0.0.1:8080/whip/60dd5cca4fa2785a5d8eefc3b1f5d1bc"
  }
}
```

**`stream_key` is returned only here and when the key is rotated (`POST /api/v1/streams/{id}/keys`).** Save it;
the server keeps only a hash.

```sh
SID=60dd5cca4fa2785a5d8eefc3b1f5d1bc
KEY=sk_zgo54TfcU8PEPmrJDH34OHnjYR5Ot6nv__hlyLSf4pk
```

## 2.7 Publish over RTMP

### FFmpeg (test pattern and tone)

```sh
ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -c:v libx264 -preset veryfast -tune zerolatency -g 60 -pix_fmt yuv420p \
  -c:a aac -b:a 128k \
  -f flv "rtmp://127.0.0.1:1935/live/$KEY"
```

The URL is `rtmp_url` followed by the `stream_key`. The RTMP application name is `live` (names starting with
`live` are accepted too). Keep the keyframe interval at or below the HLS segment length (2 s by default); the
example uses `-g 60` at 30 fps, i.e. 2 s.

### OBS

Settings → Stream:

- Service: Custom
- Server: `rtmp://127.0.0.1:1935/live`
- Stream Key: `sk_...` (the `stream_key` from the create response)

In the output settings, set the keyframe interval to 2 s or less.

Once publishing starts, `observed_state` in `GET /api/v1/streams/{id}` becomes `live` and `live` lists the
received tracks.

```sh
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/streams/$SID
```

## 2.8 Get a playback URL and watch

Live playback requires a signed URL (scope `subscribe`).

```sh
curl -s -X POST -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:8080/api/v1/streams/$SID/playback-url
```

```json
{
  "expires_at": "2026-11-03T01:16:01Z",
  "playback": {
    "hls": "http://127.0.0.1:8080/s/1793668561/nWqDQOWkMTMY3Z2GoC94Rw/live/60dd5cca4fa2785a5d8eefc3b1f5d1bc/index.m3u8",
    "rtmp": "rtmp://127.0.0.1:1935/play/60dd5cca4fa2785a5d8eefc3b1f5d1bc?exp=1793668561&sig=nWqDQOWkMTMY3Z2GoC94Rw",
    "whep": "http://127.0.0.1:8080/whep/60dd5cca4fa2785a5d8eefc3b1f5d1bc?exp=1793668561&sig=nWqDQOWkMTMY3Z2GoC94Rw"
  }
}
```

- Without a body the URL is valid for 300 s. Send `{"ttl_secs": 3600}` to change that (1 to `--max-playback-ttl`,
  86400 by default).
- Expired URLs and URLs with a wrong signature get 403.
- `whep` only works on a node started with `--webrtc-udp` ([4. Ingest and playback](04-ingest-playback.md)).

Watch it:

```sh
ffplay "http://127.0.0.1:8080/s/.../index.m3u8"
# or
ffprobe -v error -show_entries stream=codec_name,width,height -of compact "http://127.0.0.1:8080/s/.../index.m3u8"
```

In a browser, use an HLS player such as hls.js (responses on signed URLs carry `Access-Control-Allow-Origin: *`).
Playback in Safari and on iOS is not verified at present ([1.5 Verification status](01-overview.md#15-verification-status)).
The console is at `http://127.0.0.1:8080/console/`. The first time you open it, it asks you to create the first administrator: paste the admin token above, then choose a username and password (you can also keep signing in with the API token; see [12.13](12-security.md#1213-user-accounts-and-login)). Its player handles fMP4 HLS and WHEP
only, so it cannot play a stream that uses the default TS container; see `hls.container` in
[4. Ingest and playback](04-ingest-playback.md).

## 2.9 Try recording (optional)

```sh
curl -s -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d '{}' \
  http://127.0.0.1:8080/api/v1/streams/$SID/recordings          # 202, state "recording"
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/v1/recordings
```

Stopping, VOD playback and MP4 export are covered in [5. Recording](05-recording.md).

## 2.10 Stop

Press `Ctrl-C` (SIGINT) or send SIGTERM. Active recordings are closed as `finalized` before the process exits.
After a hard stop (SIGKILL, power loss), the next start closes unfinished recordings as `interrupted`
([13. Operations](13-operations.md)).

## 2.11 Docker

The published container image (`ghcr.io/kobesoft-labs/senca`) runs a single node. With the repository's `compose.yaml`:

```sh
docker compose up -d
docker compose exec senca cat /var/lib/senca/control/bootstrap-admin-token
```

- Management API, HLS and console: `http://localhost:8080` (console at `/console/`)
- RTMP: `rtmp://localhost:1935/live`; SRT: `localhost:9000/udp`; WebRTC: `8443/udp`
- Compose variables:

  | Variable | Default | Meaning |
  |---|---|---|
  | `SENCA_IMAGE` | `ghcr.io/kobesoft-labs/senca:latest` | Image to run (`:slim`, `:gpu` and so on) |
  | `SENCA_PUBLIC_IP` | `127.0.0.1` | IP address announced to WebRTC peers (ICE candidate). The default only works for a browser on the same machine |
  | `SENCA_PUBLIC_HTTP` | `http://localhost:8080` | Public HTTP URL used in playback URLs and similar |
  | `SENCA_PUBLIC_RTMP` | `rtmp://localhost:1935` | Public RTMP URL |

  For example: `SENCA_PUBLIC_IP=203.0.113.10 SENCA_PUBLIC_HTTP=http://203.0.113.10:8080 docker compose up -d`
- To build the image from your checkout, use `docker compose up -d --build` (it builds the standard variant of the `Dockerfile`).
- Compose runs the node with a read-only root, tmpfs `/tmp` and `no-new-privileges`. `compose.yaml` has commented
  examples for passing GPUs.

For the image variants (standard, slim, gpu), Raspberry Pi, GPUs and adding libraries, see [16. Container images](16-containers.md).

## 2.12 Next steps

- Ingest and playback in depth (SRT, WHIP, LL-HLS, fMP4): [4. Ingest and playback](04-ingest-playback.md)
- Every option: [3. Configuration](03-configuration.md)
- Before exposing a node: [12. Security](12-security.md) and [13. Operations](13-operations.md)
