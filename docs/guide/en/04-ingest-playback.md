# 4. Ingest and playback

[日本語](../ja/04-ingest-playback.md)

How to get video and audio from a sender (encoder, camera, browser) into a stream, and how to deliver it to viewers.

- Rationale: [ADR-0006 stream task](../../dev/adr/0006-stream-hub.md), [ADR-0012 WebRTC](../../dev/adr/0012-webrtc.md), [ADR-0014 relay](../../dev/adr/0014-relay.md), [ADR-0004 codecs and patents](../../dev/adr/0004-codecs-patents.md)
- Related pages: [3. Configuration](03-configuration.md), [5. Recording](05-recording.md), [6. Inputs and outputs](06-inputs-outputs.md), [8. Captions](08-captions.md), [9. Cluster and grid](09-cluster-grid.md), [12. Security](12-security.md), [14. Codecs and patents](14-codecs-and-patents.md), [15. API and SDKs](15-api-and-sdks.md)
- Implementation: `crates/senca-node/src/rtmp.rs`, `srt.rs`, `webrtc.rs`, `capture.rs`, `http.rs`, `live.rs`, `packager.rs`, `rtsp_serve.rs`, `api/streams.rs`

## 4.1 Overview

A stream is the unit of one broadcast and has **at most one sender at a time**. Access units (encoded frames) from the sender are
segmented for HLS exactly once by the stream task (the hub), and the same data feeds live HLS, recording, RTMP/SRT/RTSP/WHEP viewers,
outputs and captions.

| Ingest | Protocols | Authentication |
|---|---|---|
| Push (the sender connects) | RTMP, RTMPS, SRT (listener), WHIP | The stream's publish key (`sk_...`) |
| Pull (SENCA connects out) | RTMP, RTSP, SRT (caller), HLS | The input definition ([6. Inputs and outputs](06-inputs-outputs.md)) |
| Capture device | Input with `protocol: "device"` | The input definition (§4.7) |

| Playback | Source of the URL | Required settings |
|---|---|---|
| HLS (TS / fMP4, LL-HLS) | `hls` in the signed playback URLs | `--http` (`--public-http-url`) |
| RTMP play | `rtmp` in the signed playback URLs | `--rtmp` (`--public-rtmp-url`) |
| SRT play | `play:{stream_id}?exp=..&sig=..` (§4.12) | `--srt` |
| RTSP play | `rtsp` in the signed playback URLs | `--rtsp` and `--public-rtsp-url` |
| WHEP | `whep` in the signed playback URLs | `--webrtc-udp` |

The examples use these variables. `TOKEN` is a management API token (on the first start of a single node it is written to
`{data-dir}/control/bootstrap-admin-token`).

```sh
API=http://127.0.0.1:8080/api/v1
TOKEN=$(cat /var/lib/senca/control/bootstrap-admin-token)
H=(-H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json")
```

**Always set the public URLs.** URLs in API responses (`ingest.*`, playback URLs) are built from `--public-rtmp-url`,
`--public-http-url`, `--public-srt-url` and `--public-rtsp-url`, not from the listen addresses. `--public-http-url` defaults to
`http://127.0.0.1:8080` and `--public-rtmp-url` to `rtmp://127.0.0.1:1935`; they do not follow changes to the listen addresses.
`--public-srt-url` and `--public-rtsp-url` have no default; without them `ingest.srt_url` and the `rtsp` playback URL are omitted
(see [3. Configuration](03-configuration.md)).

## 4.2 Creating a stream

```sh
curl -s -X POST "$API/streams" "${H[@]}" -d '{"name":"cam1","recording":{"enabled":true}}'
```

Response (excerpt):

```json
{
  "id": "189f0fe84d8fc18358226d7380800f32",
  "name": "cam1",
  "observed_state": "waiting_input",
  "ingest": {
    "rtmp_url": "rtmp://127.0.0.1:24935/live",
    "stream_key": "sk_QE7_qJh12KNHNNfYUI19WlUT4VqV02XOwYDUcib3qL8",
    "srt_url": "srt://127.0.0.1:24900",
    "srt_streamid_format": "publish:{stream_key}",
    "whip_url": "http://127.0.0.1:24080/whip/189f0fe84d8fc18358226d7380800f32"
  },
  "live": null
}
```

- `stream_key` has a value **only in the responses to creation and key rotation (`POST /streams/{id}/keys`)**; it is `null` everywhere
  else. SENCA stores only the key's SHA-256, so if you lose the key, rotate it (the old key stops working immediately).
- When a POST with an `Idempotency-Key` is replayed, the key in the replayed response is `null` and `stream_key_redacted: true` is set.
- `srt_url` and `srt_streamid_format` appear only with `--public-srt-url`; `whip_url` appears when a public HTTP URL is set.
- `observed_state` is `waiting_input` (no sender), `live`, or `stopped` (`desired_state: stopped`). While live, `live` describes the sender:

```json
"live": {"transport": "rtmp", "epoch": 0, "started_at": "2026-11-03T01:15:53.255Z", "bitrate_bps": 956271, "viewers_hint": 0,
         "tracks": [{"track": 0, "codec": "h264", "width": 640, "height": 360},
                    {"track": 1, "codec": "aac", "sample_rate": 48000, "channels": 1}]}
```

`epoch` increases each time a sender reconnects to the same stream (§4.16).

## 4.3 Stream fields

Used by creation (`POST /streams`) and update (`PATCH /streams/{id}`, `If-Match` required). Unknown fields are rejected with 400.

| Field | Default | Validation | Meaning |
|---|---|---|---|
| `name` | (required) | 1–64 characters of letters, digits, `-`, `_`, `.`; must not start with `.`; unique within the tenant (409 on duplicates) | Display name |
| `latency_profile` | `standard` | `standard`, `low`, `interactive` | `low` emits LL-HLS parts (§4.9). `interactive` currently behaves like `standard` |
| `codec_profile` | `any` | `any`, `royalty_free` | `royalty_free` drops tracks in patent-pooled codecs (H.264, H.265, AAC) and requires `hls.container: fmp4` ([14. Codecs and patents](14-codecs-and-patents.md)) |
| `hls.segment_ms` | `2000` | 500–10000 | Target segment duration in ms. Actual cuts happen at keyframes |
| `hls.window` | `6` | 3–60 | Number of segments in the live playlist |
| `hls.container` | `ts` | `ts`, `fmp4` | Segment container. Recordings use the same container |
| `recording.enabled` | `false` | | Record automatically when publishing starts ([5. Recording](05-recording.md)) |
| `recording.retention_days` | `null` | 1–36500 | Retention for automatic recordings (`null` = forever) |
| `snapshots` | `null` | `interval_secs` 1–86400, `keep` 1–10000 | Save a still image at a fixed interval (§4.15) |
| `limits.max_viewers` | `null` | | Stored, but not enforced by the current node |
| `limits.max_ingress_bps` | `null` | | Same as above |
| `metadata` | `{}` | Up to 32 entries; keys 1–64 characters; values up to 1024 characters | Free-form string tags |
| `desired_state` | `running` | `running`, `stopped` | While `stopped`, publishing is refused. Switching a live stream to `stopped` stops accepting the sender and ends the broadcast immediately |
| `captions` | `null` | [8. Captions](08-captions.md) | Speech recognition and translation |

PATCH replaces only the fields you send. `hls`, `recording`, `limits` and `metadata` are **replaced as whole objects**
(`{"hls":{"segment_ms":1000}}` resets `window` and `container` to their defaults). `snapshots: null` and `captions: null` remove those settings.

```sh
ETAG=$(curl -s -D - -o /dev/null "$API/streams/$SID" -H "Authorization: Bearer $TOKEN" | awk -F': ' 'tolower($1)=="etag"{print $2}' | tr -d '\r')
curl -s -X PATCH "$API/streams/$SID" "${H[@]}" -H "If-Match: $ETAG" \
  -d '{"latency_profile":"low","hls":{"segment_ms":2000,"window":6,"container":"fmp4"}}'
```

Changes to `hls` and `latency_profile` take effect from the next publish (the running packager is not rebuilt).

## 4.4 Sending with RTMP / RTMPS

- URL: `ingest.rtmp_url` (`{public-rtmp-url}/live`), with `stream_key` as the stream name (stream key).
- Accepted application names: `live`, any name starting with `live`, and `play`. Other names are refused at connect.
- Anything after `?` in the stream key is ignored.
- If a second sender publishes while one is active, it is refused with `NetStream.Publish.BadName` (already publishing).
- A wrong key or a `stopped` stream is refused with `NetStream.Publish.BadName`; publishing outside a `live` schedule window
  ([5. Recording](05-recording.md)) is refused with `NetStream.Publish.Rejected`.
- A publishing connection that receives nothing for 30 seconds is closed.

```sh
ffmpeg -re -i input.mp4 -c:v libx264 -g 60 -c:a aac -f flv "rtmp://media.example.com:1935/live/$KEY"
```

In OBS, set "Server" to `rtmp://media.example.com:1935/live` and "Stream Key" to the `sk_...` key.

**RTMPS**: `--rtmps ADDR` together with `--tls-cert` and `--tls-key` opens a TLS listener. The URL is `rtmps://{host}:{port}/live` with
the same key. `ingest.rtmp_url` is built from the public RTMP URL, so build the RTMPS URL on the sender side.

**Codecs** (FLV and Enhanced RTMP v2):

| Kind | Legacy FLV | Enhanced RTMP (FourCC) |
|---|---|---|
| Video | H.264 (CodecID 7), H.265 (unofficial CodecID 12) | `avc1` (H.264), `hvc1` (H.265), `av01` (AV1), `vp09` (VP9), `vp08` (VP8) |
| Audio | AAC (10), G.711 A-law (7) and μ-law (8) | `mp4a` (AAC), `Opus` |

Enhanced RTMP Multitrack is not supported yet (the connection is closed with an error), and neither are other audio formats such as MP3.

## 4.5 Sending with SRT (listener)

`--srt ADDR` (UDP) opens an SRT listener. SENCA is the listener; the sender is the caller.

- Stream ID: `publish:{stream_key}`, or the SRT Access Control form `#!::r={stream_key},m=publish`.
- Payload is MPEG-TS. Send codecs that fit in TS (H.264, H.265, AAC, Opus).
- The listener's receive latency is 200 ms.
- A wrong key is rejected during the handshake (`X_FORBIDDEN`).

```sh
ffmpeg -re -i input.mp4 -c:v libx264 -g 60 -c:a aac -f mpegts \
  "srt://media.example.com:9000?streamid=publish:$KEY"
```

**Encryption (passphrase)**: if the tenant has a secret named `srt-passphrase`, every SRT connection to that tenant's streams
(both publish and play) must be encrypted with that passphrase. Connections without the passphrase, or with a different one, are
rejected (there is no silent fallback to plaintext). Tenants without that secret do not use encryption. SRT requires passphrases of 10–79 characters.

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"srt-passphrase","value":"correct-horse-battery"}'
ffmpeg -re -i input.mp4 -c copy -f mpegts \
  "srt://media.example.com:9000?streamid=publish:$KEY&passphrase=correct-horse-battery"
```

Creating secrets requires `tenant:admin`. Passphrases for SRT connections that SENCA makes itself (`srt_pull` inputs, `srt_push` outputs)
are separate and are passed with the input's or output's `credential_ref` ([6. Inputs and outputs](06-inputs-outputs.md)).

## 4.6 Sending with WHIP (WebRTC)

`--webrtc-udp ADDR` enables WebRTC (ICE-lite, one UDP socket shared by all sessions). The listen address is advertised as the ICE
candidate as-is, so specify an IP the sender can reach instead of `0.0.0.0`.

| Operation | Request | Response |
|---|---|---|
| Start | `POST /whip/{stream_id}`, `Authorization: Bearer {stream_key}`, `Content-Type: application/sdp`, body = offer | 201 with the answer (`application/sdp`) and `Location: /whip/{stream_id}/{session}` |
| Trickle ICE / ICE restart | `PATCH {Location}`, `Content-Type: application/trickle-ice-sdpfrag` | 204, or 200 with an sdpfrag (ICE restart) |
| Stop | `DELETE {Location}` | 200 |

- The path uses the stream **ID** (not the name). `ingest.whip_url` can be used as-is.
- Wrong key: 401. Wrong Content-Type: 415. Unacceptable offer: 400. Node without `--webrtc-udp`: 404.
- `PATCH` and `DELETE` are authorized only by the unguessable 64-bit session ID, so do not leak the `Location`.
- CORS: `OPTIONS` is answered and `Location` and `ETag` are exposed, so browsers can publish.
- Accepted codecs: H.264 (Constrained Baseline and High, packetization-mode 1), VP8, VP9 profile 0, AV1, Opus, PCMU, PCMA.
  With simulcast, only the first layer is used.
- To cut HLS segments, SENCA asks the sender for a keyframe (PLI) every 2 seconds.
- The browser sender page (`apps/sender-web`) is served at `/sender/` when `--sender-dir` is set.

```sh
curl -s -X POST "http://media.example.com:8080/whip/$SID" -H "Authorization: Bearer $KEY" \
  -H "Content-Type: application/sdp" --data-binary @offer.sdp -D headers.txt -o answer.sdp
```

## 4.7 Capture devices

Cameras and capture cards attached to a node are brought into a stream as an input with `protocol: "device"`. Device SDKs are not
loaded into the node: a separate `senca-capture-worker` process (placed in the same directory as `senca-node`) captures and encodes,
and passes only encoded access units to the node.

Available devices are listed under `nodes[].capture` in `GET /api/v1/devices` (requires `node:admin`).

```sh
curl -s -X POST "$API/streams/$SID/inputs" "${H[@]}" -d '{
  "protocol": "device",
  "url": "device:v4l2:/dev/video0",
  "options": {"codec": "vp9", "width": "1280", "height": "720", "fps": "30", "bitrate_bps": "2500000",
              "audio_device": "alsa:hw:1,0", "audio_codec": "opus"}
}'
```

| Item | Default | Meaning |
|---|---|---|
| `url` | (required) | `device:{device id}` (e.g. `device:v4l2:/dev/video0`, `device:decklink:0`); at most 300 characters |
| `options.codec` | `vp9` | Video encoder. H.264/H.265 require a GPU encoder (the grid reserves a GPU session) |
| `options.width`, `height`, `fps` | 1280, 720, 30 | |
| `options.bitrate_bps` | 2500000 | |
| `options.audio_device` | none | Audio is captured only when set |
| `options.audio_codec` | `opus` | |

All option values are strings. If the stream is `royalty_free`, the capture worker chooses encoders under that policy. Capture inputs
are placed on the node that owns the device (a grid workload; see [9. Cluster and grid](09-cluster-grid.md)). Status and reconnection
work as for other inputs ([6. Inputs and outputs](06-inputs-outputs.md)). `--capture-synthetic` provides a synthetic test device
(`device:synthetic:0`).

## 4.8 Signed playback URLs

Playback URLs live outside `/api/v1` and do not use an Authorization header; they carry an expiry and an HMAC signature.

```sh
curl -s -X POST "$API/streams/$SID/playback-url" "${H[@]}" -d '{"ttl_secs":600}'
```

```json
{
  "expires_at": "2026-11-03T01:26:00Z",
  "playback": {
    "hls":  "http://127.0.0.1:24080/s/1793669160/MGO1aub_Vkp8D2OwIHZjGw/live/189f0fe8.../index.m3u8",
    "rtmp": "rtmp://127.0.0.1:24935/play/189f0fe8...?exp=1793669160&sig=MGO1aub_Vkp8D2OwIHZjGw",
    "whep": "http://127.0.0.1:24080/whep/189f0fe8...?exp=1793669160&sig=MGO1aub_Vkp8D2OwIHZjGw",
    "rtsp": "rtsp://127.0.0.1:24554/live/189f0fe8...?exp=1793669160&sig=MGO1aub_Vkp8D2OwIHZjGw"
  }
}
```

- The body is optional. `ttl_secs` defaults to 300 and must be 1–`--max-playback-ttl` (default 86400). Requires the `subscribe` scope.
- `rtsp` appears only with `--public-rtsp-url`. `whep` is `null` on a node without `--webrtc-udp`.
  `moq` (the MoQ WebTransport URL, [4.15](#415-moq-and-http3)) appears only on nodes with `--quic`.
- The signature is HMAC-SHA256 over `live/{stream_id}/{exp}` (first 16 bytes, base64url). The file name is not part of the signature,
  so it works for any file under the same `/s/{exp}/{sig}/live/{id}/` prefix (segments, `master.m3u8`, subtitles).
- Expired or invalid signatures get 403. The signing key is part of the control state and is identical on every cluster node, so any node can verify.
- Issued URLs cannot be revoked before they expire; prefer short `ttl_secs` values ([12. Security](12-security.md)).

## 4.9 HLS

| Path | Content |
|---|---|
| `/s/{exp}/{sig}/live/{id}/index.m3u8` | Live media playlist |
| `…/{seq}.ts`, `…/{seq}.m4s` | Segments (depending on `hls.container`) |
| `…/{seq}.{part}.m4s` (`.ts`) | LL-HLS parts |
| `…/init{n}.mp4` | fMP4 initialization segment (the number advances whenever the codec configuration changes) |
| `…/master.m3u8`, `sub_{lang}.m3u8`, `sub_{lang}_{n}.vtt` | When captions are enabled ([8. Captions](08-captions.md)) |

- Segments are served from memory and never touch the disk. Segments that slid out of the window remain available for a little while (4 more segments).
- Playlists are sent with `Cache-Control: no-cache`, segments with `max-age=60`. Responses carry `Access-Control-Allow-Origin: *`.
- Segment numbers continue across sender reconnects (the playlist's `EXT-X-MEDIA-SEQUENCE` never goes backwards).
- When the broadcast ends (the reconnect grace expires), `#EXT-X-ENDLIST` is appended to the playlist.
- Codecs that do not fit the container are not included in HLS. TS: H.264, H.265, AAC, Opus. fMP4: H.264, H.265, AV1, VP9, AAC, Opus.
  `POST /streams/{id}/plan` shows how the current input is handled for each output (remux, unsupported, with reasons).

**LL-HLS** (`latency_profile: "low"`): the part target is `min(500 ms, segment_ms / 4)` and at least 200 ms. The playlist carries
`EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,CAN-SKIP-UNTIL=…,PART-HOLD-BACK=…`, `EXT-X-PART-INF`, `EXT-X-PART` and `EXT-X-PRELOAD-HINT`.

| Query | Behavior |
|---|---|
| `_HLS_msn=N` | Wait until segment N is complete, then respond (blocking playlist reload) |
| `_HLS_msn=N&_HLS_part=P` | Wait until part P of segment N exists |
| `_HLS_skip=YES` (or `v2`) | Delta playlist (older segments replaced by `EXT-X-SKIP`) |

The server waits at most 6 seconds and then answers 503 with `Retry-After: 1`. A sequence number two or more beyond the segment
currently being built, `_HLS_part` of 100 or more, or a non-numeric value gets 400. If the broadcast has ended, the request returns immediately.

Example (fMP4, LL-HLS, `segment_ms` 2000):

```
#EXTM3U
#EXT-X-VERSION:6
#EXT-X-TARGETDURATION:2
#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,CAN-SKIP-UNTIL=12.000,PART-HOLD-BACK=1.500
#EXT-X-PART-INF:PART-TARGET=0.500
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-MAP:URI="init0.mp4"
…
#EXT-X-PART:DURATION=0.467,URI="6.0.m4s",INDEPENDENT=YES
#EXT-X-PART:DURATION=0.500,URI="6.1.m4s"
#EXT-X-PRELOAD-HINT:TYPE=PART,URI="6.2.m4s"
```

## 4.10 Try it: send with RTMP, watch with HLS

These steps were run against a local node while writing this page.

```sh
# 1. Create a stream
curl -s -X POST "$API/streams" "${H[@]}" -d '{"name":"cam1"}' > s.json
SID=$(python3 -c "import json;print(json.load(open('s.json'))['id'])")
KEY=$(python3 -c "import json;print(json.load(open('s.json'))['ingest']['stream_key'])")

# 2. Send a test signal over RTMP (in another terminal)
ffmpeg -re -f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -c:v libx264 -g 60 -pix_fmt yuv420p -c:a aac -b:a 128k -f flv "rtmp://127.0.0.1:1935/live/$KEY"

# 3. Get a playback URL and watch
HLS=$(curl -s -X POST "$API/streams/$SID/playback-url" "${H[@]}" | python3 -c "import json,sys;print(json.load(sys.stdin)['playback']['hls'])")
curl -s "$HLS"            # #EXTM3U … 0.ts 1.ts …
ffplay "$HLS"             # or ffprobe / hls.js in a browser
```

Match the keyframe interval to the segment duration (for example `-g 60` at 30 fps for 2 s). Segments can only be cut at keyframes.

## 4.11 RTMP play

Use the signed `rtmp` URL as-is (`rtmp://{host}/play/{stream_id}?exp=..&sig=..`): application `play`, stream name
`{stream_id}?exp=..&sig=..`. Invalid or expired signatures get `NetStream.Play.StreamNotFound`.

```sh
ffplay "rtmp://127.0.0.1:1935/play/$SID?exp=$EXP&sig=$SIG"
```

The output is FLV (including Enhanced RTMP). A slow viewer whose send queue exceeds 8 MiB is skipped forward to the next keyframe.

## 4.12 SRT play

The Stream ID is `play:{stream_id}?exp={exp}&sig={sig}` (the same signature as the playback URLs), or
`#!::r={stream_id}?exp=..&sig=..,m=request`. The payload is MPEG-TS in 1316-byte chunks. If the tenant has `srt-passphrase`, the passphrase is required.

When putting the value into a URL's `streamid=` parameter, percent-encode the `?`, `&` and `=` inside it. Otherwise the URL is split
at `&`, the signature never arrives, and the connection is rejected.

```sh
ffplay "srt://127.0.0.1:9000?streamid=play:$SID%3Fexp%3D$EXP%26sig%3D$SIG"
```

Take `EXP` and `SIG` from the query of, for example, the `rtmp` playback URL. Codecs that do not fit TS (AV1, VP8, VP9) are not delivered on this path.

## 4.13 RTSP play

`--rtsp ADDR` opens the RTSP listener; with `--public-rtsp-url` set, playback URLs include `rtsp`.

- URL: `rtsp://{host}:{port}/live/{stream_id}?exp={exp}&sig={sig}`
- Transport is **TCP interleaved only**; UDP SETUP is refused with 461. Tell your player to use TCP (`-rtsp_transport tcp` in ffmpeg).
- An invalid signature gets 404 at DESCRIBE; no sender or no track that can be packetized gets 415.

```sh
ffplay -rtsp_transport tcp "rtsp://127.0.0.1:8554/live/$SID?exp=$EXP&sig=$SIG"
```

## 4.14 WHEP

`POST {whep URL}` with `Content-Type: application/sdp` and the offer as the body returns 201 with the answer and
`Location: /whep/{stream_id}/{session}`. `PATCH` and `DELETE` work as for WHIP.

- Only codecs the stream currently carries are offered: H.264, VP8, VP9, AV1, Opus, PCMA, PCMU.
- **AAC audio cannot be carried over WebRTC and is not sent** (SENCA does not decode AAC, by patent policy). If WHEP viewers need
  audio, have the sender use Opus. If no track can be carried at all, the answer is 422.
- Invalid or expired signatures get 403.
- Until the connection is established, the latest GOP is buffered and then sent, so playback starts at a keyframe.

## 4.15 MoQ and HTTP/3

`--quic ADDR` (with `--tls-cert` and `--tls-key`) opens a QUIC v1 listener on one UDP port. It is off by default.
The ALPN selects what runs on a connection.

| ALPN | Content |
|---|---|
| `h3` | HTTP/3, routed exactly like TCP HTTP (`/api/v1`, signed HLS including LL-HLS blocking reloads, WHIP/WHEP, `/healthz`, ...). SSE (`/api/v1/events`) and `/signaling` (WebSocket) answer 501; use HTTP/1.1 for them. An extended CONNECT to `/moq` on the same connection is MoQ over WebTransport |
| `moqt-22` | MoQ over native QUIC (draft-ietf-moq-transport-22) |

- With `--quic`, HTTP and HTTPS responses carry `Alt-Svc: h3=":PORT"; ma=86400`, so browsers can switch to HTTP/3.
- The certificate is the node's `--tls-cert`/`--tls-key` (the same as TCP HTTPS).
- Experimental: MoQ is an Internet-Draft and the version is pinned to draft-22 (`crates/senca-moq/DRAFT.md`). Clients offering
  other versions are refused during ALPN / `wt-available-protocols` negotiation.

### Subscribing (watching over MoQ)

Every live stream appears as the namespace `senca/live/{stream_id}` (streams owned by another node are served from a relay mirror).

| Track | Content |
|---|---|
| `catalog.json` | A hang catalog (the moq-dev/moq format). One group with one object per update. `video`/`audio` `renditions` carry `codec` (RFC 6381), `description` (hex avcC etc.) and `container: {"kind":"cmaf","init": base64(ftyp+moov)}` |
| `video`, `audio` (then `video2`, ...) | CMAF. One object is one fragment (moof+mdat, one sample). Video starts a group per GOP, audio roughly every second |

- Authorization uses the playback signature (`exp`, `sig`), the same as signed URLs. Pass it as the value of a MoQ
  AUTHORIZATION TOKEN (SETUP or SUBSCRIBE) `exp=..&sig=..`, in the native QUIC PATH (`moqt://host:port/?exp=..&sig=..`), or in the
  WebTransport URL query. SUBSCRIBE, FETCH and TRACK_STATUS without a valid, unexpired signature get `UNAUTHORIZED`.
  SUBSCRIBE_NAMESPACE announces only streams the signature is valid for.
- The playback URL response (4.8) includes `moq`, a WebTransport URL (`https://{host}:{port}/moq?exp=..&sig=..`), on nodes with `--quic`.
  The host is taken from `--public-http-url`.
- A subscriber joining mid-stream gets the current group from its start (a keyframe). With draft-22 FILL_PARAMETERS (current-group
  join) the head of the group goes on the fill stream and the rest on the subscription. The last two groups are also available via FETCH.

Writing fMP4 with the moq-dev/moq CLI:

```sh
Q="exp=1793669160&sig=MGO1aub_Vkp8D2OwIHZjGw"   # the query of the playback URL's moq field
moq --connect "moqt://media.example.com:4433/?$Q" --connect-version moq-transport-22 \
  --broadcast "senca/live/$SID" export fmp4 > live.mp4
```

### Publishing (sending over MoQ)

Send `PUBLISH_NAMESPACE senca/live/{stream_id}` with the stream key (`sk_...`) as an AUTHORIZATION TOKEN (in SETUP or in the
PUBLISH_NAMESPACE). The node subscribes to `catalog.json`, then to the catalog's tracks, and becomes the stream's publisher
(`live.transport` is `moq`).

- Only the `cmaf` container (init plus fragments) is accepted. A hang `legacy` or `loc` track is refused and the session closed
  (moq-dev's `moq import` emits `legacy`, so it cannot publish to SENCA as is).
- Publisher-initiated PUBLISH is not used (`NOT_SUPPORTED`). One session publishes one stream.
- Outside a "live" schedule window the publication is refused, as for every other ingest.

## 4.16 Still images

| Operation | Request | Scope | Result |
|---|---|---|---|
| Current frame | `GET /streams/{id}/snapshot[?format=jpeg\|png]` | `stream:read` | 200 `image/jpeg` (default) or `image/png` |
| List saved images | `GET /streams/{id}/snapshots` | `stream:read` | `{"items":[{id, taken_at, reason, width, height, bytes}]}` (oldest first) |
| One image | `GET /streams/{id}/snapshots/{snapshot_id}` | `stream:read` | JPEG |
| Delete | `DELETE /streams/{id}/snapshots/{snapshot_id}` | `stream:write` | 204 |

- Still images are made by **decoding** the latest GOP in the codec worker. H.264 and H.265 need a GPU decoder; without one the answer
  is 503 `CAPACITY_UNAVAILABLE` (`retryable: false`). AV1 (dav1d) and VP8/VP9 (libvpx) can be decoded in software
  ([14. Codecs and patents](14-codecs-and-patents.md)). If the stream is not live, or no keyframe has arrived yet, the answer is 409.
- Images are saved for three reasons (`reason`): `interval` (the stream's `snapshots` policy; the oldest are deleted beyond `keep`),
  `schedule` (a `snapshot` schedule, see [5. Recording](05-recording.md)), and `board_change` (a pipeline's `snapshot_on_change`,
  see [7. Pipelines](07-pipelines.md)).
- They are stored in `{data-dir}/snapshots/{stream_id}/` and deleted together with the stream.
- Recording thumbnails are covered in [5. Recording](05-recording.md).

```sh
curl -s -o now.jpg "$API/streams/$SID/snapshot" -H "Authorization: Bearer $TOKEN"
curl -s -X PATCH "$API/streams/$SID" "${H[@]}" -H "If-Match: $ETAG" -d '{"snapshots":{"interval_secs":60,"keep":1440}}'
```

## 4.17 When the sender drops or is replaced

- When the sender disconnects, the broadcast does not end for `--reconnect-grace` (default 30000 ms). If a sender reconnects with the
  same key within that time, `epoch` increases by 1 and the broadcast continues. HLS is split with `EXT-X-DISCONTINUITY` and segment
  numbers continue; the recording stays a single recording ([5. Recording](05-recording.md)). The protocol may change (for example,
  drop on RTMP and come back on SRT).
- When the grace expires the broadcast ends: HLS gets `ENDLIST`, RTMP/SRT/RTSP/WHEP viewers are closed, and recordings are finalized.
- Setting `desired_state: stopped` stops accepting the sender at once and ends the broadcast without waiting for the grace.

## 4.18 Playback from any node (cluster)

In a cluster, the node that receives the sender becomes the stream's owner. When playback (HLS, RTMP/SRT/RTSP play, WHEP) reaches a
node that is not the owner, that node receives the access units from the owner over the relay, builds a local "mirror", and serves
viewers from it. There is a single upstream per stream regardless of the number of viewers. On the first HLS request the node waits up
to 6 seconds for the mirror's first playlist; unused mirrors are removed after 30 seconds. Mirrors do not record or caption (that is
the owner's job). If the relay is unavailable, the request is redirected with 307 to the owner node (`media_addr`).
See [9. Cluster and grid](09-cluster-grid.md).

## 4.19 Troubleshooting

| Symptom | Cause |
|---|---|
| RTMP publish is refused | `BadName`: wrong key, another sender is already active, `stopped`. `Rejected`: outside a `live` schedule window |
| SRT connection is rejected immediately | Wrong Stream ID format, wrong key, or the tenant has `srt-passphrase` and no passphrase was given (`unsecure` in the log) |
| SRT play is rejected | The `streamid` value was not percent-encoded |
| HLS returns 404 | No segment yet (long keyframe interval), or the URL points to the wrong host (`--public-http-url`) |
| HLS lacks video or audio | The codec does not fit the container (`POST /streams/{id}/plan`), or `royalty_free` dropped H.264/AAC |
| Still image returns 503 | H.264/H.265 without a GPU, or the codec worker is not running |
| WHEP has no audio | The audio is AAC |
