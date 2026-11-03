# SENCA User Guide

English | [日本語](../ja/README.md)

How to run, use and operate SENCA, a media server for live streaming, recording and real-time communication.
This guide covers version v0.0.1.

If you are new to SENCA, start with [1. Overview](01-overview.md) for the overall architecture, then try it out with
[2. Quickstart](02-quickstart.md).

## Contents

| # | Page | What it covers |
|---|---|---|
| 1 | [Overview](01-overview.md) | What SENCA is; architecture (control plane, hubs, packager, recorder, codec workers, grid, relay, rooms, media pipelines); feature map; what has been verified |
| 2 | [Quickstart](02-quickstart.md) | Build from source, run a single node, create a stream, publish over RTMP, play over HLS, Docker |
| 3 | [Configuration](03-configuration.md) | Every option, the JSON configuration file, cluster configuration, environment variables, data directory |
| 4 | [Ingest and playback](04-ingest-playback.md) | Ingest over RTMP, SRT and WHIP; playback over HLS, LL-HLS, RTMP play, WHEP and RTSP |
| 5 | [Recording](05-recording.md) | Recordings, VOD, MP4 export, schedules, retention |
| 6 | [Inputs and outputs](06-inputs-outputs.md) | Pull inputs, push outputs, capture devices, cloud streaming services, secrets |
| 7 | [Pipelines](07-pipelines.md) | Decode, compose, whiteboard correction, audio mixing, encode; production controls |
| 8 | [Captions and translation](08-captions.md) | Speech recognition, translation and WebVTT HLS subtitles |
| 9 | [Cluster and grid](09-cluster-grid.md) | Raft control plane, mTLS, stream ownership, grid placement and degradation, relay |
| 10 | [Realtime rooms](10-rooms.md) | Realtime rooms (SFU), join tokens, signaling |
| 11 | [Media pipelines](11-media-pipelines.md) | Exporting, compositing, recording, pushing and transcribing rooms |
| 12 | [Security](12-security.md) | Tokens and scopes, JWT, stream keys, signed URLs, secrets, egress restrictions, mTLS, sandboxing |
| 13 | [Operations](13-operations.md) | Health checks, logging, monitoring, events, recovery, backups, upgrades, draining, shutdown, capacity |
| 14 | [Codecs and patents](14-codecs-and-patents.md) | Hardware and royalty-free codecs, patent policy |
| 15 | [Management API and SDKs](15-api-and-sdks.md) | REST conventions, OpenAPI, SSE, TypeScript and Rust clients |
| 16 | [Container images](16-containers.md) | Published image variants (standard, slim, gpu), amd64 and arm64, Raspberry Pi, GPUs, adding drivers and libraries |

## Related documents

Most of these are written in Japanese.

| | |
|---|---|
| Design decisions | [docs/dev/adr](../../dev/adr) |
| API definition | [docs/api/openapi.yaml](../../api/openapi.yaml); room signaling [docs/realtime/signaling.en.md](../../realtime/signaling.en.md) |
| Progress | [docs/dev/ROADMAP.md](../../dev/ROADMAP.md); conformance ledger [docs/dev/conformance/ledger.md](../../dev/conformance/ledger.md) |
| Development and releases | [docs/dev/DEVELOPMENT.en.md](../../dev/DEVELOPMENT.en.md), [docs/dev/RELEASE.en.md](../../dev/RELEASE.en.md) |
