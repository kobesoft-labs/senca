# SENCA

English | [日本語](README.ja.md)

A media server for live streaming, recording and real-time communication, written in Rust.
It runs on a single node or as a grid of many nodes that places work by resources and keeps running when nodes fail.

> **Status: v0.0.0 (experimental).** The feature set is broad, but the release gate has not been passed yet
> (24-hour fuzzing, 72-hour soak test, independent review of the cryptographic and authorization code, signed builds).
> Do not use it in production. See [docs/RELEASE.md](docs/RELEASE.md) (Japanese).

## Features

- **Ingest**: RTMP/RTMPS (Enhanced RTMP: AV1, VP9, H.265), SRT (listener and caller, AES), WHIP (WebRTC), RTSP pull,
  RTMP/SRT/HLS pull, capture devices
- **Delivery**: HLS (TS, fMP4, LL-HLS), RTMP/SRT/RTSP play, WHEP, signed playback URLs, relay mirrors between nodes
- **Recording**: recording branched from the live path, crash recovery, VOD, MP4 export, schedules, retention
- **Processing**: pipelines (decode, compose, whiteboard correction, audio mixing, encode), production controls
  (scenes, transitions), snapshots and thumbnails, captions and translation (WebVTT)
- **Egress**: RTMP/SRT push, integration with cloud streaming services (PaaS)
- **Realtime rooms** (ADR-0015): SFU, simulcast, spotlight, forwarding filters, DataChannels, join tokens
  (Ed25519), events on SSE, E2EE pass-through, and a browser SDK (`apps/sdk-js`)
- **Media pipelines** (ADR-0016): export room participants as streams, composite, record, push over RTMP, transcribe
- **Grid** (ADR-0013): places work by node resources (CPU, memory, bandwidth, GPUs, capture devices); manual pinning
  to nodes and GPUs, priorities, moving work off failed nodes and devices with step-wise degradation, and restoring it
  when they come back
- **Clustering**: control plane replicated with an in-house Raft, mTLS, stream ownership with fencing,
  inter-node relay (ADR-0014)
- **Management**: REST API (OpenAPI: [docs/api/openapi.yaml](docs/api/openapi.yaml)), SSE, web console (`apps/console`),
  TypeScript and Rust API clients (`apps/sdk-js`, `crates/senca-client`)
- **Building blocks**: QUIC v1, HTTP/3, WebTransport, MoQ (draft-22); not yet wired into the node

## Codecs and patents

H.264/H.265 encoding and decoding is done only through GPU vendor SDKs (NVIDIA NVENC/NVDEC, Intel oneVPL, AMD AMF,
VA-API), loaded at run time inside a sandboxed codec worker. No software H.264/H.265/AAC codec is shipped.
Without a GPU, those transcodes return `503 CAPACITY_UNAVAILABLE`; passing received H.264/AAC through
(delivery and recording) always works. Royalty-free codecs (AV1: dav1d and SVT-AV1, VP8/VP9: libvpx, Opus: libopus)
are handled in software. Licensing obligations for distributing content are yours to check.
See [ADR-0004](docs/adr/0004-codecs-patents.md) (Japanese) and the guide page on codecs.

## Getting started

```sh
cargo build --release -p senca-node -p senca-codec-worker
./target/release/senca-node --data-dir ./data --http 127.0.0.1:8080 --rtmp 127.0.0.1:1935 \
  --webrtc-udp 127.0.0.1:8443
cat ./data/control/bootstrap-admin-token   # token for the management API
```

- Containers: `Dockerfile`, `compose.yaml`
- Clustering: `--cluster-config`

## Documentation

**User guide: [docs/guide/en](docs/guide/en/README.md)** (installation, configuration, streaming, recording, grid,
rooms, media pipelines, API and SDKs). The same guide in Japanese is in [docs/guide/ja](docs/guide/ja/README.md).

| | |
|---|---|
| API | [docs/api/openapi.yaml](docs/api/openapi.yaml), room signaling [docs/realtime/signaling.en.md](docs/realtime/signaling.en.md) |
| SDKs | [apps/sdk-js](apps/sdk-js/README.md), [crates/senca-client](crates/senca-client/README.md) |
| Design records, specification, requirements (Japanese) | [docs/adr](docs/adr), [docs/spec](docs/spec), [docs/requirements](docs/requirements) |
| Progress (Japanese) | [docs/ROADMAP.md](docs/ROADMAP.md), conformance ledger [docs/conformance](docs/conformance) |
| Development (Japanese) | [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md), [docs/RELEASE.md](docs/RELEASE.md), dependency licenses [docs/licenses](docs/licenses) |

## License

[Apache License 2.0](LICENSE). See also [NOTICE](NOTICE).
