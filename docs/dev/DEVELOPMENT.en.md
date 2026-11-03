# Development rules

English | [日本語](DEVELOPMENT.md)

## General

- A Rust monorepo (`crates/*`), edition 2024. `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` must pass.
- Documentation and comments are written in Japanese. Identifiers are in English.
- Do not leave traces such as the names of tools used for the work or co-author lines in commits or tracked files.

## Layers

- **sans-io**: protocol, container and bitstream crates (`senca-h264`, `senca-rtmp`, `senca-mpegts` and so on) own no I/O, threads or clocks. They are state machines: bytes go in, events and output bytes come out. Time is passed in as an argument.
- **I/O** lives only in executables such as `senca-node` (tokio).
- Shared types are in `senca-core` (`AccessUnit`, `CodecConfig`, `Timebase`, `Id128`, `BitReader`).

## Rules

- **Reject strictly.** Put limits on lengths, integer arithmetic, nesting and totals, and return `Err` for broken input. No panics, no infinite loops, no unbounded allocation (spec 9.4, AC-01). `unsafe` is forbidden (the only exception is FFI crates, which must state the reason and the boundary in comments).
- **Do not copy.** Take payloads as `bytes::Bytes` and pass them on as slices (`slice`) (REQ-010 R10-7).
- **Verify against independent implementations.** In addition to unit tests, put tests that compare against other implementations such as FFmpeg in `tools/interop/` (FFmpeg is used only as a test peer; it is never part of the product).
- **Do not leak secrets.** Stream keys and tokens never appear in logs, errors or Debug output.
- **Do not write cryptographic primitives yourself.** AES, SHA, HMAC, HKDF, signatures, key exchange and random numbers come from vetted libraries (ADR-0002). The protocols (the handshakes and record layers of TLS/DTLS/SRTP/QUIC) are implemented in-house.
- **When adding a dependency, update the list in ADR-0002.**

## Test tools (development machine)

- `~/tools/ffmpeg` and `~/tools/ffprobe` (static build 7.0.2). Override with the `FFMPEG` and `FFPROBE` environment variables.
- Docker (to run independent implementations such as Chromium).
