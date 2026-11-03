# 14. Codecs and patents

[日本語](../ja/14-codecs-and-patents.md)

Which codecs SENCA handles and how, what it deliberately does not ship, and what it loads at run time. This page covers what operators
provide (GPU drivers, royalty-free codec libraries) and how SENCA behaves when they are missing.

- Rationale: [ADR-0004 codec selection and patent policy](../../dev/adr/0004-codecs-patents.md), [ADR-0003 internal media format](../../dev/adr/0003-internal-media-format.md)
- Ledger: [license and patent ledger](../../licenses/README.md)
- Related pages: [4. Ingest and playback](04-ingest-playback.md), [5. Recording](05-recording.md), [7. Pipelines](07-pipelines.md), [11. Media pipelines](11-media-pipelines.md), [13. Operations](13-operations.md)
- Implementation: `crates/senca-codec` (providers, dlopen), `crates/senca-codec-worker` (worker, sandbox), `crates/senca-node/src/codec.rs`

> **This is not legal advice.** This page and the ledger describe SENCA's implementation policy. Whether your content, service or region
> needs a license (including patent pool royalties) is for you to determine.

## 14.1 Policy

1. **H.264 and H.265 are encoded and decoded only with GPU vendor hardware codecs.** Software H.264/H.265 codecs (libavcodec, OpenH264,
   x264, x265, …) are neither shipped nor loaded.
2. **AAC is only passed through.** There is no AAC encoder or decoder (fdk-aac, …). When audio must be converted, it becomes Opus.
3. **Codecs offered royalty-free are used in software**: dav1d (AV1 decode), SVT-AV1 (AV1 encode), libvpx (VP8/VP9), libopus (Opus).
4. External codec libraries are not bundled; **the separate `senca-codec-worker` process loads them at run time (dlopen)**. The node
   process itself never loads codec libraries.
5. When the required device or library is missing, SENCA refuses with a reason (503 `CAPACITY_UNAVAILABLE` or a validation error)
   instead of silently degrading.

**Bitstream parsing and conversion** for H.264/H.265 (Annex B ⇔ length-prefixed, SPS/PPS/VPS parsing, RTP packetization, muxing into
TS, fMP4 and FLV) is not encoding or decoding, and SENCA implements it itself.

## 14.2 Passthrough always works

Paths that neither encode nor decode need no GPU and no codec library.

| Path | Codecs passed through |
|---|---|
| Ingest (RTMP, SRT, WHIP, pull) | H.264, H.265, AV1, VP8, VP9, AAC, Opus, G.711 (per-protocol support in [4. Ingest and playback](04-ingest-playback.md)) |
| HLS (TS) | H.264, H.265, AAC, Opus |
| HLS (fMP4) | H.264, H.265, AV1, VP9, AAC, Opus |
| Recording, VOD, MP4 export | Same as HLS (same segments) |
| RTMP play, `rtmp_push` | Whatever FLV / Enhanced RTMP can carry |
| SRT play, `srt_push` | Whatever TS can carry |
| RTSP play | Whatever can be packetized into RTP |
| WHEP | H.264, VP8, VP9, AV1, Opus, PCMA, PCMU (**AAC cannot be carried**, so no audio) |
| Captions | AAC or Opus audio, repackaged and sent ([8. Captions](08-captions.md)) |

`POST /streams/{id}/plan` reports how each track of the current input is handled by each output (`remux`, `repacketize`,
`passthrough`, `unsupported`, with reasons).

## 14.3 Operations that need encoding or decoding

| Operation | Requires | When unavailable |
|---|---|---|
| Current frame `GET /streams/{id}/snapshot` | Video decode | 503 `CAPACITY_UNAVAILABLE` (`retryable: false`) |
| Saved still images (`snapshots` policy, `snapshot` schedules) | Video decode | Not saved (reason in the log) |
| Recording thumbnails | Video decode | Not created (`GET /recordings/{id}/thumbnail` returns 404) |
| Pipelines (`decode`, `encode`) | Decode/encode per node | Create/update fails with 503 `CAPACITY_UNAVAILABLE` or 400 `POLICY_REJECTED` / `UNSUPPORTED_CODEC` ([7. Pipelines](07-pipelines.md)) |
| Composite of a room media pipeline | Decoding participant video and encoding the composite (AV1 by default) | See [11. Media pipelines](11-media-pipelines.md) |
| Capture inputs | Encoding in the capture worker (VP9 by default) | The input goes to `error` / `backoff` |

Example: requesting the current frame of an H.264 stream on a node without a GPU (verified by running it):

```json
{"type":"urn:senca:error:capacity-unavailable","title":"h264 を復号できる装置がありません（H.264/H.265 はハードウェアのデコーダーだけを使います）",
 "status":503,"code":"CAPACITY_UNAVAILABLE","retryable":false}
```

(The title says that no device can decode H.264 and that only hardware decoders are used for H.264/H.265.) On the same node, a VP9
stream is decoded with libvpx and a JPEG or PNG is returned.

## 14.4 Hardware providers

| Provider (`provider`) | API | Libraries loaded | Encode | Decode |
|---|---|---|---|---|
| `nvidia` | NVENC / NVDEC (Video Codec SDK 13.1, Linux driver 610 or later) | `libcuda.so.1`, `libnvidia-encode.so.1`, `libnvcuvid.so.1` (`libnvidia-ml.so.1` only for version display, optional) | H.264, H.265, AV1 | H.264, H.265, AV1, VP9 |
| `intel-vpl` | oneVPL (Quick Sync) | `libvpl.so.2` (the dispatcher finds the driver implementation) | H.264, H.265, AV1 | H.264, H.265, AV1, VP9 |
| `amd-amf` | AMF (Vulkan on Linux) | `libamfrt64.so.1` | H.264, H.265, AV1 | H.264, H.265 |
| `vaapi` | VA-API | `libva.so.2`, `libva-drm.so.2`, `/dev/dri/renderD*` | none | H.264 |

- Only hardware implementations are selected (the software implementations of oneVPL and AMF are not used).
- A missing library or device is normal; the provider is reported as unavailable with a reason in `GET /devices`.
- Some hardware paths have not been verified on real hardware because the development machine has no GPU (see "verification status"
  in each provider's source, and REQ-004). Validate with your own GPU and driver before relying on them.
- The Apple VideoToolbox provider listed in ADR-0004 is not implemented at present.

To use a GPU in a container, pass the device and the driver libraries into the container ([13. Operations](13-operations.md), `compose.yaml`).

## 14.5 Royalty-free software codecs

| Provider | Library (soname) | Use | License |
|---|---|---|---|
| `dav1d` | `libdav1d.so.7` | AV1 decode | BSD-2-Clause |
| `svt-av1` | `libSvtAv1Enc.so.2` | AV1 encode | BSD-3-Clause + AOM patent license |
| `libvpx` | `libvpx.so.12` (libvpx 1.16 ABI) | VP8, VP9 encode and decode | BSD-3-Clause + WebM patent license |
| `opus` | `libopus.so.0` | Opus encode and decode | BSD-3-Clause + patent licenses per IETF declarations |

### Installing

Install your distribution's packages. The official container image (`Dockerfile`) uses Ubuntu 26.04 and installs:

```sh
apt-get install -y --no-install-recommends libdav1d7 libvpx12 libopus0 libsvtav1enc2 libva2 libva-drm2 libvpl2
```

Versions with a different soname (for example only `libvpx.so.9`) are not loaded. The libvpx provider targets the libvpx 1.16 ABI (`libvpx.so.12`).

Search order:

1. `--codec-lib-dir DIR` (the node passes it to the worker as `SENCA_CODEC_LIB_DIR`; several directories may be separated with `:`).
   The official image starts with `--codec-lib-dir /usr/lib/x86_64-linux-gnu`.
2. Otherwise the system's default dlopen search order (including `LD_LIBRARY_PATH`).

Only the fixed sonames of each provider are loaded; the API cannot make SENCA load an arbitrary path.
To keep specific providers from being used, set the node's environment variable `SENCA_CODEC_DISABLE` to a comma-separated list of
provider names (`nvidia`, `intel-vpl`, `amd-amf`, `vaapi`, `dav1d`, `libvpx`, `svt-av1`, `opus`) or `hardware` / `software`.

Check the result with `codecs.transcode` in `GET /api/v1/capabilities` and with `GET /api/v1/devices` (§14.8).

## 14.6 The codec worker

- Place the `senca-codec-worker` executable in the same directory as `senca-node` (or point to it with `--codec-worker-bin FILE`).
- The pool for short jobs (still images, thumbnails) has `--codec-workers N` processes (0 or unset = number of CPU cores). Each pipeline gets its own process.
- Each job times out after 30 seconds. Crashed workers are replaced.
- If the worker cannot be found or started, the node logs `codec worker unavailable: ...` and keeps running. Still images, thumbnails and
  pipelines are then unavailable (503 or not created), `pipelines` in `GET /capabilities` is `false`, and `codecs.transcode` is empty.

### Sandbox (Linux)

The worker logs the restrictions it applied at startup (e.g. `sandbox: core=0; nofile=256; no_new_privs; landlock(abi 8): writes only under /dev; cpu=300s/job; seccomp deny-list`).

- Dies with its parent, cannot be ptraced (`PR_SET_DUMPABLE=0`), no core dumps, `PR_SET_NO_NEW_PRIVS`.
- Landlock (on kernels that support it): creating, writing and deleting files is forbidden except under `/dev` (GPU device files). Reading is allowed (for dlopen).
- seccomp deny list: networking (socket, connect, bind, listen, accept), executing programs, ptrace, mount, kernel modules, bpf, namespaces, and more.
- A CPU-time limit per job (when exceeded, the worker exits and is replaced).
- Because networking is denied, some driver helpers (such as connecting to NVIDIA's persistenced) are unavailable.

## 14.7 `codec_profile: royalty_free`

A stream with `codec_profile: royalty_free` does not handle patent-pooled codecs (H.264, H.265, AAC).

- If an H.264, H.265 or AAC track arrives at ingest, **that track is dropped** (log: `rejected by royalty_free profile; dropping track`).
  The sender is not disconnected; if the video is H.264, the broadcast simply has no video.
- `hls.container` must be `fmp4` (`ts` is rejected with 400 on create or update, since AV1 and VP9 do not fit in TS).
- For pipelines, if any `source` or `sink` stream is `royalty_free`, any use of H.264, H.265 or AAC (passthrough included) is `POLICY_REJECTED`.
- Still images, thumbnails and capture encoding also choose providers under this policy.
- Have senders use AV1, VP9 or VP8 with Opus (WHIP, Enhanced RTMP; see [4. Ingest and playback](04-ingest-playback.md)).

```sh
curl -s -X POST "$API/streams" "${H[@]}" -d '{"name":"rf1","codec_profile":"royalty_free","hls":{"container":"fmp4"}}'
```

## 14.8 `GET /capabilities` and `GET /devices`

`GET /api/v1/capabilities` (any authenticated caller, no particular scope) returns what this node can do.

```json
{
  "api_version": "v1",
  "codecs": {
    "passthrough": ["h264","h265","av1","vp8","vp9","aac","opus","pcma","pcmu"],
    "transcode": [
      {"provider":"dav1d","codec":"av1","direction":"decode","hardware":false,"max_width":16384,"max_height":16384,"max_sessions":null,"device":"dav1d"},
      {"provider":"libvpx","codec":"vp8","direction":"encode","hardware":false,"max_width":16383,"max_height":16383,"max_sessions":null,"device":"libvpx"}
    ]
  },
  "codec_profiles": ["any","royalty_free"],
  "pipelines": true
}
```

(Excerpt from a development machine without a GPU, with the royalty-free libraries installed.)

- `codecs.transcode` lists the providers the codec worker can actually use (provider, codec, direction, hardware or not, maximum resolution, concurrent sessions, device name).
- Other fields: `ingest`, `output`, `recordings`, `schedules`, `inputs`, `outputs`, `paas_providers`, `captions`, `rtsp_play`, `cluster` ([15. API and SDKs](15-api-and-sdks.md)).

`GET /api/v1/devices` (`node:admin`) returns per-provider status with reasons, the sandbox, and capture devices.

```json
{"nodes": [{"node": "…",
  "devices": {"providers": [{"provider": "nvidia", "available": false, "detail": "…libcuda.so.1…"},
                            {"provider": "dav1d", "available": true, "detail": "…"}],
              "sandbox": "…"},
  "capture": {"available": false, "reason": "senca-capture-worker がありません"}}]}
```

For `available: false`, `detail` explains which library or device is missing. (The capture `reason` above says that `senca-capture-worker` is not present.)

## 14.9 A note on licensing

- Hardware codecs are offered as features of products whose vendors paid for them, but **royalties related to distribution services and
  content** (for example Via LA's AVC/HEVC programs, Access Advance) may not be covered by buying the hardware.
- Points to check, such as how AAC passthrough is treated and third-party pools asserting rights over AV1, VP8, VP9 and Opus, are
  collected in the [license and patent ledger](../../licenses/README.md). SENCA does not describe AV1 and the like as "patent-free".
- Determine what applies to your own service, region and content.
